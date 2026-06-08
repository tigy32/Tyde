use leptos::prelude::*;

use crate::components::ui::{Button, ButtonVariant};
use crate::state::{AppMode, AppState, PairingScreen};

/// Shown when the user has no paired hosts yet. Walks them through opening
/// the desktop app, enabling mobile connections, starting a pairing offer,
/// and tapping the "Scan QR" button on the phone.
#[component]
pub fn OnboardingView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let state_for_scan = state.clone();
    let on_scan = Callback::new(move |_: ()| {
        state_for_scan
            .app_mode
            .set(AppMode::Pairing(PairingScreen::Scanner));
    });

    let state_for_paste = state.clone();
    let on_paste = Callback::new(move |_: ()| {
        state_for_paste
            .app_mode
            .set(AppMode::Pairing(PairingScreen::ManualPaste));
    });

    view! {
        <div class="view onboarding-view">
            <div class="view-header">
                <h1 class="view-title">"Welcome to Tyde"</h1>
            </div>
            <div class="view-body">
                <div class="onboarding-instructions">
                    <h2 class="onboarding-step">"1. Open Tyde on your computer"</h2>
                    <h2 class="onboarding-step">"2. Settings → Hosts → enable mobile connections"</h2>
                    <h2 class="onboarding-step">"3. Tap Start pairing on the desktop"</h2>
                    <h2 class="onboarding-step">"4. Scan the QR code below"</h2>
                </div>
                <div class="onboarding-actions">
                    <Button
                        label="Scan QR"
                        variant=ButtonVariant::Primary
                        full_width=true
                        on_click=on_scan
                    />
                    <Button
                        label="Paste pairing URI"
                        variant=ButtonVariant::Secondary
                        full_width=true
                        on_click=on_paste
                    />
                </div>
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppMode;
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
    async fn renders_walkthrough_steps_and_actions() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Onboarding mode is the default; assert the user-visible content.
            state.app_mode.set(AppMode::Onboarding);
            provide_context(state);
            view! { <OnboardingView /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Welcome to Tyde"),
            "expected welcome heading, got: {text}"
        );
        assert!(
            text.contains("enable mobile connections"),
            "expected setup instructions, got: {text}"
        );
        assert!(text.contains("Scan QR"));
        assert!(text.contains("Paste pairing URI"));
    }
}
