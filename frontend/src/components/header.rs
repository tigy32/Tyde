use std::cell::{Cell, RefCell};
use std::time::Duration;

use leptos::prelude::*;

use crate::state::{AppState, ConnectionStatus, DockVisibility};

/// ssh reports a dropped connection on stderr before the transport loss reaches
/// the recovery listener, so an arriving host warning cannot be classified yet.
/// Hold it this long rather than banner it immediately: a drop the host
/// reconnects inside the window resolves the warning instead of leaving the
/// user something to dismiss by hand.
pub(crate) const HOST_WARNING_HOLD: Duration = Duration::from_secs(3);

#[derive(Clone, PartialEq, Eq)]
enum UserNoticeKind {
    Error,
    Warning,
}

#[derive(Clone, PartialEq, Eq)]
struct UserNotice {
    id: u64,
    kind: UserNoticeKind,
    host_id: Option<String>,
    message: String,
}

struct HeldHostWarning {
    host_id: String,
    timer: TimeoutHandle,
}

thread_local! {
    static USER_NOTICE: ArcRwSignal<Option<UserNotice>> =
        ArcRwSignal::new(None);
    static NEXT_USER_NOTICE_ID: Cell<u64> = const { Cell::new(0) };
    static HELD_HOST_WARNING: RefCell<Option<HeldHostWarning>> = const { RefCell::new(None) };
}

pub(crate) fn report_user_error(message: impl Into<String>) {
    let message = message.into();
    log::error!("user-visible error: {message}");
    report_user_notice(UserNoticeKind::Error, None, message);
}

/// Queues a host's diagnostic behind [`HOST_WARNING_HOLD`]. It reaches the
/// banner only if the host is still connected when the hold expires, so a
/// diagnostic that merely narrated a connection the host went on to lose or
/// rebuild never interrupts the user.
pub(crate) fn hold_host_warning(
    state: &AppState,
    host_id: impl Into<String>,
    message: impl Into<String>,
) {
    let host_id = host_id.into();
    let message = message.into();
    log::warn!("host {host_id} warning held: {message}");
    if let Some(previous) = take_held_host_warning() {
        previous.timer.clear();
    }

    let state = state.clone();
    let held_host_id = host_id.clone();
    let timer = set_timeout_with_handle(
        move || {
            take_held_host_warning();
            if !host_is_connected(&state, &held_host_id) {
                log::warn!("host {held_host_id} warning dropped, host is not connected: {message}");
                return;
            }
            report_user_notice(UserNoticeKind::Warning, Some(held_host_id), message);
        },
        HOST_WARNING_HOLD,
    );
    match timer {
        Ok(timer) => HELD_HOST_WARNING.with(|held| {
            *held.borrow_mut() = Some(HeldHostWarning { host_id, timer });
        }),
        Err(error) => log::error!("failed to hold warning for host {host_id}: {error:?}"),
    }
}

/// Retires a host's warning once its connection moves again: the held one
/// before it can surface, and a shown one the host has since recovered from.
pub(crate) fn resolve_host_warnings(host_id: &str) {
    let held = HELD_HOST_WARNING.with(|held| {
        let mut held = held.borrow_mut();
        match held.as_ref() {
            Some(warning) if warning.host_id == host_id => held.take(),
            _ => None,
        }
    });
    if let Some(held) = held {
        held.timer.clear();
    }

    USER_NOTICE.with(|notice| {
        let shown_for_host = notice.with_untracked(|shown| {
            shown.as_ref().is_some_and(|shown| {
                shown.kind == UserNoticeKind::Warning && shown.host_id.as_deref() == Some(host_id)
            })
        });
        if shown_for_host {
            notice.set(None);
        }
    });
}

fn take_held_host_warning() -> Option<HeldHostWarning> {
    HELD_HOST_WARNING.with(|held| held.borrow_mut().take())
}

fn host_is_connected(state: &AppState, host_id: &str) -> bool {
    state.connection_statuses.with_untracked(|statuses| {
        matches!(statuses.get(host_id), Some(ConnectionStatus::Connected))
    })
}

fn report_user_notice(kind: UserNoticeKind, host_id: Option<String>, message: String) {
    let id = NEXT_USER_NOTICE_ID.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1));
        id
    });
    USER_NOTICE.with(|notice| {
        notice.set(Some(UserNotice {
            id,
            kind,
            host_id,
            message,
        }));
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
        sleep_millis(0).await;
    }

    async fn sleep_millis(millis: i32) {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, millis)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    async fn sleep_past_warning_hold() {
        sleep_millis(HOST_WARNING_HOLD.as_millis() as i32 + 250).await;
    }

    fn connected_host_state(host_id: &str) -> AppState {
        let state = AppState::new();
        state
            .configured_hosts
            .set(vec![crate::bridge::ConfiguredHost {
                id: host_id.to_owned(),
                label: "Tyggs".to_owned(),
                transport: crate::bridge::HostTransportConfig::LocalEmbedded,
                auto_connect: false,
            }]);
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.connection_statuses.update(|statuses| {
            statuses.insert(host_id.to_owned(), ConnectionStatus::Connected);
        });
        state
    }

    fn reset_notices(host_id: &str) {
        resolve_host_warnings(host_id);
        USER_NOTICE.with(|notice| notice.set(None));
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
        reset_notices("remote");
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

        hold_host_warning(
            &state,
            "remote",
            "ssh: ** WARNING: connection is not using a post-quantum key exchange algorithm.",
        );
        sleep_past_warning_hold().await;

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

    #[wasm_bindgen_test]
    async fn transient_ssh_drop_never_reaches_the_banner() {
        reset_notices("host");
        let container = make_container();
        let state = connected_host_state("host");
        let state_for_view = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_view);
            view! { <Header /> }
        });

        hold_host_warning(
            &state,
            "host",
            "Host \u{201c}Tyggs\u{201d} reported: ssh: Connection to hersheys.tycode.dev closed by remote host.",
        );
        next_tick().await;
        assert!(
            container
                .query_selector(".user-notice-banner")
                .unwrap()
                .is_none(),
            "a host diagnostic must not interrupt the user before the transport has been given a chance to recover"
        );

        resolve_host_warnings("host");
        sleep_past_warning_hold().await;
        assert!(
            container
                .query_selector(".user-notice-banner")
                .unwrap()
                .is_none(),
            "a dropped connection the host reconnected must leave no banner to dismiss"
        );
    }

    #[wasm_bindgen_test]
    async fn standing_host_warning_shows_then_clears_when_the_host_recovers() {
        reset_notices("host");
        let container = make_container();
        let state = connected_host_state("host");
        let state_for_view = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_view);
            view! { <Header /> }
        });

        hold_host_warning(
            &state,
            "host",
            "Host \u{201c}Tyggs\u{201d} reported: ssh: Connection to hersheys.tycode.dev closed by remote host.",
        );
        sleep_past_warning_hold().await;
        let banner = container
            .query_selector(".user-notice-banner.warning")
            .unwrap()
            .expect("a diagnostic the host never recovered from must reach the user");
        assert!(
            banner
                .text_content()
                .unwrap_or_default()
                .contains("closed by remote host"),
            "the banner must carry the diagnostic the host reported"
        );

        resolve_host_warnings("host");
        next_tick().await;
        assert!(
            container
                .query_selector(".user-notice-banner")
                .unwrap()
                .is_none(),
            "a warning the host has since recovered from must clear itself"
        );
    }

    #[wasm_bindgen_test]
    async fn recovering_one_host_leaves_another_hosts_warning_alone() {
        reset_notices("host");
        let container = make_container();
        let state = connected_host_state("host");
        let state_for_view = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_view);
            view! { <Header /> }
        });

        hold_host_warning(
            &state,
            "host",
            "Host \u{201c}Tyggs\u{201d} reported: ssh: broken pipe",
        );
        sleep_past_warning_hold().await;
        resolve_host_warnings("other");
        next_tick().await;
        assert!(
            container
                .query_selector(".user-notice-banner.warning")
                .unwrap()
                .is_some(),
            "one host reconnecting says nothing about a warning another host raised"
        );
    }
}
