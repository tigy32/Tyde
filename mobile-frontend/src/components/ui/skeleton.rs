use leptos::prelude::*;

/// Animated placeholder block. Use during initial subscribe / data
/// load so lists never flash empty before content arrives. Respects
/// `prefers-reduced-motion` via CSS (shimmer disabled there).
#[component]
pub fn Skeleton(
    #[prop(optional)] width: Option<String>,
    #[prop(optional)] height: Option<String>,
    #[prop(optional)] rounded: Option<bool>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
) -> impl IntoView {
    let rounded = rounded.unwrap_or(false);
    let mut classes = String::from("ui-skeleton");
    if rounded {
        classes.push_str(" ui-skeleton-rounded");
    }
    let test = data_mobile_test.unwrap_or("skeleton");
    let style = format!(
        "{}{}",
        width.map(|w| format!("width: {w};")).unwrap_or_default(),
        height.map(|h| format!("height: {h};")).unwrap_or_default()
    );
    view! {
        <div
            class=classes
            style=style
            data-mobile-test=test
            aria-hidden="true"
        />
    }
}
