use std::cell::Cell;

use leptos::prelude::*;

use crate::state::{AppState, ConnectionStatus, DockVisibility};

#[derive(Clone, PartialEq, Eq)]
enum UserNoticeKind {
    Error,
    Warning,
}

#[derive(Clone, PartialEq, Eq)]
struct UserNotice {
    id: u64,
    kind: UserNoticeKind,
    message: String,
}

thread_local! {
    static USER_NOTICE: ArcRwSignal<Option<UserNotice>> =
        ArcRwSignal::new(None);
    static NEXT_USER_NOTICE_ID: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn report_user_error(message: impl Into<String>) {
    let message = message.into();
    log::error!("user-visible error: {message}");
    report_user_notice(UserNoticeKind::Error, message);
}

pub(crate) fn report_user_warning(message: impl Into<String>) {
    let message = message.into();
    log::warn!("user-visible warning: {message}");
    report_user_notice(UserNoticeKind::Warning, message);
}

fn report_user_notice(kind: UserNoticeKind, message: String) {
    let id = NEXT_USER_NOTICE_ID.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1));
        id
    });
    USER_NOTICE.with(|notice| {
        notice.set(Some(UserNotice { id, kind, message }));
    });
}

fn user_notice_signal() -> ArcRwSignal<Option<UserNotice>> {
    USER_NOTICE.with(Clone::clone)
}

#[component]
pub fn Header() -> impl IntoView {
    let state = expect_context::<AppState>();
    let user_notice = user_notice_signal();

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
            ConnectionStatus::Reconnecting {
                attempt,
                retry_in_seconds,
                message,
            } => {
                if attempt == 0 {
                    format!("{selected_label}: {message}")
                } else {
                    format!(
                        "{selected_label}: Disconnected — reconnecting… Attempt {attempt} · retry in {retry_in_seconds}s"
                    )
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
                ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. } => {
                    "status-dot connecting"
                }
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

    let user_notice_for_show = user_notice.clone();
    let user_notice_for_kind = user_notice.clone();
    let user_notice_for_role = user_notice.clone();
    let user_notice_for_label = user_notice.clone();
    let user_notice_for_message = user_notice;
    let user_notice_message = Memo::new(move |_| {
        user_notice_for_message
            .get()
            .map(|notice| notice.message)
            .unwrap_or_default()
    });
    let user_notice_class =
        Memo::new(
            move |_| match user_notice_for_kind.get().map(|notice| notice.kind) {
                Some(UserNoticeKind::Warning) => "user-notice-banner warning",
                _ => "user-notice-banner error",
            },
        );
    let user_notice_label =
        Memo::new(
            move |_| match user_notice_for_label.get().map(|notice| notice.kind) {
                Some(UserNoticeKind::Warning) => "SSH warning",
                _ => "Action failed",
            },
        );
    let user_notice_role =
        Memo::new(
            move |_| match user_notice_for_role.get().map(|notice| notice.kind) {
                Some(UserNoticeKind::Warning) => "status",
                _ => "alert",
            },
        );

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
            <Show when=move || user_notice_for_show.get().is_some()>
                <div class={user_notice_class} role={user_notice_role} aria-live="polite" aria-atomic="true">
                    <span class="user-notice-banner-label">{user_notice_label}</span>
                    <span class="user-notice-banner-message">
                        {move || user_notice_message.get()}
                    </span>
                    <button
                        class="user-notice-banner-dismiss"
                        title="Dismiss notice"
                        aria-label="Dismiss notice"
                        on:click=move |_| {
                            USER_NOTICE.with(|notice| notice.set(None));
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
        USER_NOTICE.with(|notice| notice.set(None));
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            provide_context(AppState::new());
            view! { <Header /> }
        });

        report_user_error("Tyde could not open a terminal because the host is offline.");
        next_tick().await;
        let banner = container
            .query_selector(".user-notice-banner.error")
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
            .query_selector(".user-notice-banner-dismiss")
            .unwrap()
            .expect("error banner must be dismissible")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            container
                .query_selector(".user-notice-banner")
                .unwrap()
                .is_none()
        );
    }

    #[wasm_bindgen_test]
    async fn reported_warning_is_visible_without_changing_connection_status() {
        USER_NOTICE.with(|notice| notice.set(None));
        let container = make_container();
        let state = AppState::new();
        state
            .configured_hosts
            .set(vec![crate::bridge::ConfiguredHost {
                id: "remote".to_owned(),
                label: "Remote".to_owned(),
                transport: crate::bridge::HostTransportConfig::LocalEmbedded,
                auto_connect: false,
            }]);
        state.selected_host_id.set(Some("remote".to_owned()));
        state.connection_statuses.update(|statuses| {
            statuses.insert("remote".to_owned(), ConnectionStatus::Connected);
        });
        let state_for_view = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_view);
            view! { <Header /> }
        });

        report_user_warning(
            "ssh: ** WARNING: connection is not using a post-quantum key exchange algorithm.",
        );
        next_tick().await;

        let warning = container
            .query_selector(".user-notice-banner.warning")
            .unwrap()
            .expect("SSH diagnostics must render as a warning");
        assert!(
            warning
                .text_content()
                .unwrap_or_default()
                .contains("not using a post-quantum key exchange")
        );
        assert_eq!(
            container
                .query_selector(".status-text")
                .unwrap()
                .expect("host status must remain visible")
                .text_content()
                .as_deref(),
            Some("1/1 hosts connected · Remote")
        );
        assert!(
            container
                .query_selector(".status-dot.connected")
                .unwrap()
                .is_some(),
            "a warning must not apply error styling to the host"
        );
        assert_eq!(
            state.connection_statuses.get_untracked().get("remote"),
            Some(&ConnectionStatus::Connected)
        );
    }
}
