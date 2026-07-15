mod agents_view;
mod backend_capacity;
mod bottom_nav;
mod chat_input;
mod chat_message;
pub mod chat_view;
mod connection_banner;
mod diff_viewer;
mod error_banner;
mod file_viewer;
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
