use leptos::prelude::*;

use crate::components::ui::{Button, ButtonVariant};
use crate::state::{AppMode, AppState, PairingScreen};

/// Shown when the user has no paired hosts yet. Walks them through opening
/// the desktop app, enabling mobile connections under Settings → Mobile,
/// starting a pairing offer, and tapping the "Scan QR" button on the phone.
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
                    // Mobile pairing lives under Settings → **Mobile** (the tab
                    // with the enable toggle, the pairing offer, and the QR).
                    // It is not under Hosts, which is where this used to send
                    // people — a dead end they could not recover from without
                    // guessing.
                    //
                    // The arrow is a visual breadcrumb, and screen readers
                    // announce it inconsistently ("right arrow", or nothing at
                    // all), so the step carries a spoken equivalent that names
                    // the tab in words.
                    <h2
                        class="onboarding-step"
                        aria-label="Step 2. In Settings, open the Mobile tab, then enable mobile connections."
                        data-mobile-test="onboarding-step-enable"
                    >
                        "2. Settings → Mobile → enable mobile connections"
                    </h2>
                    <h2 class="onboarding-step">"3. Tap Start pairing on the desktop"</h2>
                    // There is no QR *below*. Below are the two buttons; the QR is on
                    // the computer, where step 3 just put it. The old wording sent a
                    // first-time user hunting down this screen for a code that was
                    // never going to be there — and the one thing they had to do
                    // instead, tap Scan QR, went unnamed.
                    <h2 class="onboarding-step" data-mobile-test="onboarding-step-scan">
                        "4. Tap Scan QR and point your phone at the code on your computer"
                    </h2>
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

    /// **Step 4 pointed at a QR that is not there.**
    ///
    /// It read "Scan the QR code below". Below are two *buttons*; the QR is on the
    /// computer, where step 3 has just told the user to put it. A first-time user
    /// following the steps in order got sent hunting down this screen for a code that
    /// was never going to appear — and the one thing they actually had to do, tap
    /// **Scan QR**, went unnamed.
    #[wasm_bindgen_test]
    async fn step_four_names_the_action_and_does_not_promise_a_qr_on_this_screen() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.app_mode.set(AppMode::Onboarding);
            provide_context(state);
            view! { <OnboardingView /> }
        });
        next_tick().await;

        let step = container
            .query_selector("[data-mobile-test='onboarding-step-scan']")
            .unwrap()
            .expect("the scan step must render")
            .text_content()
            .unwrap_or_default();

        assert!(
            !step.to_lowercase().contains("below"),
            "the step must not claim a QR is on this screen — there is none: {step}"
        );
        assert!(
            step.contains("Scan QR"),
            "the step must name the control the user is meant to tap: {step}"
        );
        assert!(
            step.contains("on your computer"),
            "the step must say where the code actually is: {step}"
        );

        // The button it names is genuinely there, so the instruction is followable.
        let body = container.text_content().unwrap_or_default();
        assert!(
            body.contains("Scan QR") && body.contains("Paste pairing URI"),
            "both actions must still be on screen: {body}"
        );
    }

    /// The onboarding is the only thing telling a first-time user where mobile
    /// pairing lives. It sent them to Settings → Hosts, which has no pairing
    /// controls at all — a dead end with nothing on screen to correct it.
    ///
    /// The assertion above ("enable mobile connections") could not catch that:
    /// the phrase is the tail of both the right instruction and the wrong one,
    /// so the *destination* went unguarded. This pins the destination, which is
    /// the part that was actually broken.
    ///
    /// Evidence for "Mobile" being correct: `SettingsTab::Mobile` is labelled
    /// "Mobile" (`frontend/src/components/settings_panel.rs:642`), and `MobileTab`
    /// is what renders the `enable_mobile_connections` toggle, the "Start pairing"
    /// button, and the QR. `HostsTab` renders none of them.
    #[wasm_bindgen_test]
    async fn pairing_instructions_point_at_the_settings_tab_that_has_the_qr() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.app_mode.set(AppMode::Onboarding);
            provide_context(state);
            view! { <OnboardingView /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Settings → Mobile"),
            "onboarding must send the user to the Settings tab that actually has the \
             pairing QR, got: {text}"
        );
        assert!(
            !text.contains("Hosts"),
            "onboarding must not send the user to Hosts — it has no pairing controls, \
             so they arrive at a dead end: {text}"
        );

        // The arrow is decoration. A screen-reader user must still be told which
        // tab to open, in words.
        let step = container
            .query_selector("[data-mobile-test='onboarding-step-enable']")
            .unwrap()
            .expect("the enable-connections step must render");
        let spoken = step
            .get_attribute("aria-label")
            .expect("the step must have a spoken form, since '→' is announced inconsistently");
        assert!(
            spoken.contains("Settings") && spoken.contains("Mobile"),
            "the spoken form must name both Settings and the Mobile tab, got: {spoken}"
        );
        assert!(
            !spoken.contains('→'),
            "the spoken form must not rely on a glyph screen readers mangle, got: {spoken}"
        );
    }
}
