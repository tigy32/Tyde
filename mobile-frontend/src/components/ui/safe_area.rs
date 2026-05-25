use leptos::prelude::*;

/// Container that pads its content for iOS notch / Android navigation
/// bar via `env(safe-area-inset-*)`. Pass `inset_bottom=false` to opt
/// out of bottom padding when the parent is already handling it (e.g.
/// when sitting above a bottom-tab bar).
#[component]
pub fn SafeArea(
    children: Children,
    #[prop(optional)] inset_top: Option<bool>,
    #[prop(optional)] inset_bottom: Option<bool>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
) -> impl IntoView {
    let inset_top = inset_top.unwrap_or(true);
    let inset_bottom = inset_bottom.unwrap_or(true);
    let mut classes = String::from("ui-safe-area");
    if inset_top {
        classes.push_str(" ui-safe-area-top");
    }
    if inset_bottom {
        classes.push_str(" ui-safe-area-bottom");
    }
    let test = data_mobile_test.unwrap_or("safe-area");
    view! {
        <div class=classes data-mobile-test=test>
            {children()}
        </div>
    }
}
