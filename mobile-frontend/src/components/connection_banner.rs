use leptos::prelude::*;

use crate::components::ui::{Spinner, StatusDot, StatusTone};
use crate::state::{AppState, ConnectionStatus};

/// Floating connection indicator. The healthy connected state stays
/// dot-only so it does not take vertical space from the workspace; states
/// needing attention expand to a compact pill.
#[component]
pub fn ConnectionBanner() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    view! {
        <div class="connection-banner" data-mobile-test="connection-banner">
            {let state = state.clone(); move || {
                let status = state.active_host_connection_status();
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
                        view! {
                            <div class="connection-banner-inner error">
                                <StatusDot
                                    label="Error".to_string()
                                    tone=StatusTone::Error
                                    data_mobile_test="connection-banner-dot-error"
                                />
                                <span class="status-text">{msg}</span>
                            </div>
                        }.into_any()
                    }
                }
            }}
            {move || {
                let error = state.active_host_command_error();
                error.map(|msg| view! {
                    <div class="command-error-banner" role="alert">
                        <span class="error-text">{msg}</span>
                    </div>
                })
            }}
        </div>
    }
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
}
