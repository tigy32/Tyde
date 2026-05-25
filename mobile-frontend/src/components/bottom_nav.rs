use leptos::prelude::*;

use crate::state::{AppState, MobileTab};

/// Mobile bottom navigation. Each tab is a button with explicit
/// `aria-current` so VoiceOver/TalkBack announces the active tab,
/// and a `data-mobile-test` selector so wasm tests can drive it.
///
/// The button reads its `active`-ness via a closure (not a stale
/// snapshot), so swapping tabs in any handler — including from inside
/// a deep child surface — updates the highlight without re-mounting.
#[component]
pub fn BottomNav() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let make_tab =
        move |tab: MobileTab, label: &'static str, icon: &'static str, test_id: &'static str| {
            let s_active = state.clone();
            let s_click = state.clone();
            let is_active = move || s_active.active_tab.get() == tab;
            let aria_current = move || if is_active() { "page" } else { "false" };

            view! {
                <button
                    type="button"
                    class="nav-tab"
                    class:active=is_active
                    role="tab"
                    data-mobile-test=test_id
                    aria-label=label
                    aria-current=aria_current
                    on:click=move |_| s_click.active_tab.set(tab)
                >
                    <span class="nav-icon" aria-hidden="true">{icon}</span>
                    <span class="nav-label">{label}</span>
                </button>
            }
        };

    view! {
        <nav
            class="bottom-nav"
            role="tablist"
            aria-label="Primary navigation"
            data-mobile-test="bottom-nav"
        >
            {make_tab(MobileTab::Home, "Home", "\u{2302}", "nav-tab-home")}
            {make_tab(MobileTab::Agents, "Agents", "\u{25B6}", "nav-tab-agents")}
            {make_tab(MobileTab::Sessions, "Sessions", "\u{2630}", "nav-tab-sessions")}
            {make_tab(MobileTab::Projects, "Projects", "\u{2261}", "nav-tab-projects")}
            {make_tab(MobileTab::Settings, "Settings", "\u{2699}", "nav-tab-settings")}
        </nav>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppState;
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

    /// Tapping a tab swaps `active_tab` AND `aria-current` on the
    /// new tab — both surfaces must move in lockstep so screen-reader
    /// users get the same signal as sighted users.
    #[wasm_bindgen_test]
    async fn bottom_nav_tap_updates_active_tab_and_aria() {
        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <BottomNav /> }
        });
        next_tick().await;

        // Home is the default active tab.
        let home = container
            .query_selector("[data-mobile-test='nav-tab-home']")
            .unwrap()
            .unwrap();
        assert_eq!(
            home.get_attribute("aria-current").as_deref(),
            Some("page"),
            "Home should be the active tab on first render"
        );

        // Tap Sessions.
        let sessions: HtmlElement = container
            .query_selector("[data-mobile-test='nav-tab-sessions']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        sessions.click();
        next_tick().await;

        let sessions_now = container
            .query_selector("[data-mobile-test='nav-tab-sessions']")
            .unwrap()
            .unwrap();
        assert_eq!(
            sessions_now.get_attribute("aria-current").as_deref(),
            Some("page"),
            "Sessions must be the active tab after tap"
        );
        let home_now = container
            .query_selector("[data-mobile-test='nav-tab-home']")
            .unwrap()
            .unwrap();
        assert_eq!(
            home_now.get_attribute("aria-current").as_deref(),
            Some("false"),
            "Home must lose active state after Sessions is tapped"
        );

        // And the state signal mirrors it.
        let state = state_handle.borrow().as_ref().unwrap().clone();
        assert_eq!(state.active_tab.get(), MobileTab::Sessions);
    }
}
