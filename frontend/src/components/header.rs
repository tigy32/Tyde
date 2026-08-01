use std::cell::Cell;

use leptos::prelude::*;

use crate::state::{AppState, ConnectionStatus, DockVisibility};

#[derive(Clone, PartialEq, Eq)]
struct UserFacingError {
    id: u64,
    message: String,
}

thread_local! {
    static USER_FACING_ERROR: ArcRwSignal<Option<UserFacingError>> =
        ArcRwSignal::new(None);
    static NEXT_USER_FACING_ERROR_ID: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn report_user_error(message: impl Into<String>) {
    let message = message.into();
    log::error!("user-visible error: {message}");
    let id = NEXT_USER_FACING_ERROR_ID.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1));
        id
    });
    USER_FACING_ERROR.with(|error| {
        error.set(Some(UserFacingError { id, message }));
    });
}

fn user_facing_error_signal() -> ArcRwSignal<Option<UserFacingError>> {
    USER_FACING_ERROR.with(Clone::clone)
}

#[cfg(test)]
pub(crate) fn current_user_error() -> Option<String> {
    USER_FACING_ERROR.with(|error| error.get_untracked().map(|error| error.message))
}

#[component]
pub fn Header() -> impl IntoView {
    let state = expect_context::<AppState>();
    let user_error = user_facing_error_signal();

    let status_text_state = state.clone();
    let status_text = Memo::new(move |_| {
        let connected = status_text_state.active_connection_count();
        let total = status_text_state.total_host_count();
        if total == 0 {
            return "No hosts".to_string();
        }

        let selected = status_text_state.selected_host();
        let selected_status = status_text_state.selected_host_connection_status();
        let selected_command_error = status_text_state.selected_host_command_error();
        let selected_label = selected
            .map(|host| host.label)
            .unwrap_or_else(|| "No host".to_string());

        match selected_status {
            ConnectionStatus::Connected => {
                let base = format!("{connected}/{total} hosts connected · {selected_label}");
                match selected_command_error {
                    Some(error) => format!("{base} · last error: {error}"),
                    None => base,
                }
            }
            ConnectionStatus::Connecting => format!("Connecting to {selected_label}"),
            ConnectionStatus::Disconnected => {
                format!("{connected}/{total} hosts connected · {selected_label} offline")
            }
            ConnectionStatus::Error(message) => format!("{selected_label}: {message}"),
        }
    });

    let status_class_state = state.clone();
    let status_class =
        Memo::new(
            move |_| match status_class_state.selected_host_connection_status() {
                ConnectionStatus::Disconnected => "status-dot disconnected",
                ConnectionStatus::Connecting => "status-dot connecting",
                ConnectionStatus::Connected => "status-dot connected",
                ConnectionStatus::Error(_) => "status-dot error",
            },
        );

    let toggle_left = move |_| {
        state.left_dock.update(|dock| {
            *dock = match dock {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let toggle_right = move |_| {
        state.right_dock.update(|dock| {
            *dock = match dock {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let toggle_bottom = move |_| {
        state.bottom_dock.update(|dock| {
            *dock = match dock {
                DockVisibility::Visible => DockVisibility::Hidden,
                DockVisibility::Hidden => DockVisibility::Visible,
            }
        });
    };

    let user_error_for_show = user_error.clone();
    let user_error_for_message = user_error;
    let user_error_message = Memo::new(move |_| {
        user_error_for_message
            .get()
            .map(|error| error.message)
            .unwrap_or_default()
    });

    view! {
        <>
            <header class="header">
                <div class="header-left">
                    <span class="header-title">"Tyde"</span>
                    <div class="header-status">
                        <span class={status_class}></span>
                        <span class="status-text" title={status_text}>{status_text}</span>
                    </div>
                </div>
                <div class="header-right">
                    <button class="header-btn" title="Toggle Left Dock" on:click=toggle_left>"Left"</button>
                    <button class="header-btn" title="Toggle Bottom Dock" on:click=toggle_bottom>"Bottom"</button>
                    <button class="header-btn" title="Toggle Right Dock" on:click=toggle_right>"Right"</button>
                </div>
            </header>
            <Show when=move || user_error_for_show.get().is_some()>
                <div class="user-error-banner" role="alert" aria-atomic="true">
                    <span class="user-error-banner-label">"Action failed"</span>
                    <span class="user-error-banner-message">
                        {move || user_error_message.get()}
                    </span>
                    <button
                        class="user-error-banner-dismiss"
                        title="Dismiss error"
                        aria-label="Dismiss error"
                        on:click=move |_| {
                            USER_FACING_ERROR.with(|error| error.set(None));
                        }
                    >
                        "×"
                    </button>
                </div>
            </Show>
        </>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
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
    async fn reported_error_is_visible_and_dismissible() {
        USER_FACING_ERROR.with(|error| error.set(None));
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            provide_context(AppState::new());
            view! { <Header /> }
        });

        report_user_error("Tyde could not open a terminal because the host is offline.");
        next_tick().await;
        let banner = container
            .query_selector(".user-error-banner")
            .unwrap()
            .expect("reported failures must render visibly");
        assert_eq!(banner.get_attribute("role").as_deref(), Some("alert"));
        assert!(
            banner
                .text_content()
                .unwrap_or_default()
                .contains("host is offline")
        );

        container
            .query_selector(".user-error-banner-dismiss")
            .unwrap()
            .expect("error banner must be dismissible")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            container
                .query_selector(".user-error-banner")
                .unwrap()
                .is_none()
        );
    }
}
