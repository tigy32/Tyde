use leptos::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone {
    /// Healthy / online / idle-ready.
    Online,
    /// In progress / working / streaming.
    Active,
    /// Soft attention — connecting, queued, paused.
    Pending,
    /// Error / fatal / revoked.
    Error,
    /// Offline / disconnected / inactive.
    Muted,
}

impl StatusTone {
    fn class(self) -> &'static str {
        match self {
            Self::Online => "ui-status-dot ui-status-dot-online",
            Self::Active => "ui-status-dot ui-status-dot-active",
            Self::Pending => "ui-status-dot ui-status-dot-pending",
            Self::Error => "ui-status-dot ui-status-dot-error",
            Self::Muted => "ui-status-dot ui-status-dot-muted",
        }
    }
}

/// Colored status dot with a required accessible label so the state
/// is reachable without seeing color. Color alone is never the only
/// signal.
#[component]
pub fn StatusDot(
    #[prop(into)] label: String,
    #[prop(optional)] tone: Option<StatusTone>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
) -> impl IntoView {
    let tone = tone.unwrap_or(StatusTone::Muted);
    let test = data_mobile_test.unwrap_or("status-dot");
    let label_for_title = label.clone();
    let label_for_aria = label.clone();
    view! {
        <span
            class=tone.class()
            role="img"
            data-mobile-test=test
            title=label_for_title
            aria-label=label_for_aria
        />
    }
}
