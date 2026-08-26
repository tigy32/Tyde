mod agents_view;
mod backend_capacity;
mod bottom_nav;
mod chat_input;
mod chat_message;
pub mod chat_view;
mod connection_banner;
mod diff_viewer;
mod error_banner;
mod home_view;
mod host_browser;
mod onboarding_view;
mod paired_hosts_picker;
mod pairing_flow;
pub mod pending_submissions;
mod projects_view;
mod sessions_view;
pub mod settings_view;
mod teams_view;
mod tool_card;
pub mod ui;

pub use agents_view::AgentsView;
pub use bottom_nav::BottomNav;
pub use chat_view::ChatView;
pub use connection_banner::ConnectionBanner;
pub use error_banner::MobileShellErrorBanner;
pub use home_view::HomeView;
pub use onboarding_view::OnboardingView;
pub use paired_hosts_picker::PairedHostsPicker;
pub use pairing_flow::PairingFlow;
// Only the host-scoped surface is re-exported: `app.rs` mounts it as a top-level
// view, which is what this list is for. `AgentPendingSubmissions` is chat-scoped
// and is mounted by `chat_view.rs`, which reaches it by module path — the same way
// it reaches `ChatInput` and `ChatMessageView`, and the reason a re-export here was
// dead.
pub use pending_submissions::PendingSubmissions;
pub use projects_view::ProjectsView;
pub use sessions_view::SessionsView;
pub use settings_view::SettingsView;

/// The production stylesheet for wasm tests that assert on geometry or
/// pseudo-elements — what the user actually sees — rather than class names.
#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod test_styles {
    const PROD_STYLES: &str = include_str!("../../styles.css");

    pub(crate) fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document.get_element_by_id("prod-styles").is_none() {
            let style = document.create_element("style").unwrap();
            style.set_id("prod-styles");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
        // Unconditional, not folded into the guard above: `voice.rs` carries
        // its own private loader for the same `#prod-styles` id and does not
        // set a theme, so if it runs first in the shared test document, a
        // guarded set here would silently no-op and every `var(--accent-*)`
        // reference would fail to resolve for the rest of the suite. The app
        // themes its root with `data-theme`; the colour tokens only resolve
        // beneath one.
        document
            .document_element()
            .unwrap()
            .set_attribute("data-theme", "dark")
            .unwrap();
    }
}
