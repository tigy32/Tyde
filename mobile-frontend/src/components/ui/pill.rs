use leptos::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillTone {
    Neutral,
    Accent,
    Success,
    Warning,
    Error,
}

impl PillTone {
    fn class(self) -> &'static str {
        match self {
            Self::Neutral => "ui-pill ui-pill-neutral",
            Self::Accent => "ui-pill ui-pill-accent",
            Self::Success => "ui-pill ui-pill-success",
            Self::Warning => "ui-pill ui-pill-warning",
            Self::Error => "ui-pill ui-pill-error",
        }
    }
}

/// Compact label chip. Use for status, counts, role badges, and other
/// scannable metadata. Always pairs visible text with the tone — color
/// alone is never the only signal (a11y).
#[component]
pub fn Pill(
    #[prop(into)] label: String,
    #[prop(optional)] tone: Option<PillTone>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
) -> impl IntoView {
    let tone = tone.unwrap_or(PillTone::Neutral);
    let test = data_mobile_test.unwrap_or("pill");
    view! {
        <span class=tone.class() data-mobile-test=test>{label}</span>
    }
}
