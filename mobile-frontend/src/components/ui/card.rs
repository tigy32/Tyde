use leptos::prelude::*;

/// Rounded surface container. Use for dashboard cards, list rows that
/// stand on their own, and grouped form sections. Variants below
/// adjust elevation and padding; everything else inherits from the
/// theme tokens in `styles.css`.
#[component]
pub fn Card(
    children: Children,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
    #[prop(optional)] interactive: Option<bool>,
    #[prop(optional)] dense: Option<bool>,
    #[prop(optional)] aria_label: Option<String>,
    #[prop(optional)] on_click: Option<Callback<()>>,
) -> impl IntoView {
    let interactive = interactive.unwrap_or(false);
    let dense = dense.unwrap_or(false);
    let mut classes = String::from("ui-card");
    if interactive {
        classes.push_str(" ui-card-interactive");
    }
    if dense {
        classes.push_str(" ui-card-dense");
    }

    let role = if interactive { "button" } else { "group" };
    let tabindex = if interactive { "0" } else { "-1" };
    let test = data_mobile_test.unwrap_or("card");
    let click_cb = on_click;

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if !interactive {
            return;
        }
        let key = ev.key();
        let is_activation = key == "Enter" || key == " ";
        if is_activation && let Some(cb) = click_cb.as_ref() {
            ev.prevent_default();
            cb.run(());
        }
    };
    let on_click_handler = move |_| {
        if interactive && let Some(cb) = click_cb.as_ref() {
            cb.run(());
        }
    };

    view! {
        <div
            class=classes
            role=role
            tabindex=tabindex
            data-mobile-test=test
            aria-label=aria_label
            on:click=on_click_handler
            on:keydown=on_keydown
        >
            {children()}
        </div>
    }
}
