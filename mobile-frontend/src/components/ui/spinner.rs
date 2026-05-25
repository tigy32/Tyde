use leptos::prelude::*;

/// Indeterminate progress indicator. Default is inline-sized for use
/// inside buttons; `large=true` is for full-card / centered states.
#[component]
pub fn Spinner(
    #[prop(optional)] large: Option<bool>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
    #[prop(optional)] aria_label: Option<String>,
) -> impl IntoView {
    let large = large.unwrap_or(false);
    let class = if large {
        "ui-spinner ui-spinner-large"
    } else {
        "ui-spinner"
    };
    let test = data_mobile_test.unwrap_or("spinner");
    let aria = aria_label.unwrap_or_else(|| "Loading".to_owned());
    view! {
        <span
            class=class
            data-mobile-test=test
            role="status"
            aria-live="polite"
            aria-label=aria
        />
    }
}
