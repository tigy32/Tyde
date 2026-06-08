use leptos::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Destructive,
}

impl ButtonVariant {
    fn class(self) -> &'static str {
        match self {
            Self::Primary => "ui-button ui-button-primary",
            Self::Secondary => "ui-button ui-button-secondary",
            Self::Ghost => "ui-button ui-button-ghost",
            Self::Destructive => "ui-button ui-button-destructive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonSize {
    Compact,
    Default,
    Large,
}

impl ButtonSize {
    fn class(self) -> &'static str {
        match self {
            Self::Compact => "ui-button-compact",
            Self::Default => "",
            Self::Large => "ui-button-large",
        }
    }
}

/// Primary mobile button. Minimum 44pt tap target via CSS.
/// `data_mobile_test` is the structural test selector — wasm tests
/// look for `[data-mobile-test="<value>"]` rather than CSS classes,
/// so visual refactors don't break tests.
///
/// `disabled` is a [`MaybeProp`] so callers can pass either a static
/// `bool` or a reactive closure/signal — the disabled state stays a
/// pure projection of state without re-mounting the button. `icon`
/// renders a square 44×44 tap target (no label padding) for glyph-only
/// actions. `class` appends caller layout classes (margins, spacing)
/// on top of the shared `.ui-button` styling.
#[component]
pub fn Button(
    #[prop(into)] label: String,
    #[prop(optional)] variant: Option<ButtonVariant>,
    #[prop(optional)] size: Option<ButtonSize>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
    #[prop(optional, into)] disabled: MaybeProp<bool>,
    #[prop(optional)] full_width: Option<bool>,
    #[prop(optional)] icon: Option<bool>,
    #[prop(optional, into)] class: Option<String>,
    #[prop(optional)] aria_label: Option<String>,
    #[prop(optional)] on_click: Option<Callback<()>>,
) -> impl IntoView {
    let variant = variant.unwrap_or(ButtonVariant::Primary);
    let size = size.unwrap_or(ButtonSize::Default);
    let full_width = full_width.unwrap_or(false);
    let icon = icon.unwrap_or(false);

    let class = format!(
        "{} {} {} {} {}",
        variant.class(),
        size.class(),
        if full_width { "ui-button-full" } else { "" },
        if icon { "ui-button-icon" } else { "" },
        class.as_deref().unwrap_or(""),
    );
    let test = data_mobile_test.unwrap_or("button");
    let aria_label_value = aria_label.unwrap_or_else(|| label.clone());

    let is_disabled = move || disabled.get().unwrap_or(false);

    let on_click_handler = move |_| {
        if !is_disabled()
            && let Some(cb) = on_click.as_ref()
        {
            cb.run(());
        }
    };

    view! {
        <button
            type="button"
            class=class
            data-mobile-test=test
            disabled=is_disabled
            aria-disabled=move || if is_disabled() { "true" } else { "false" }
            aria-label=aria_label_value
            on:click=on_click_handler
        >
            <span class="ui-button-label">{label}</span>
        </button>
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

    #[wasm_bindgen_test]
    fn button_renders_label_and_test_selector() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            view! {
                <Button
                    label="Pair host"
                    data_mobile_test="pair-host-cta"
                />
            }
        });
        let btn = container
            .query_selector("[data-mobile-test='pair-host-cta']")
            .unwrap()
            .expect("button must surface its test selector");
        assert_eq!(
            btn.text_content().unwrap_or_default().trim(),
            "Pair host",
            "button must render its label"
        );
    }

    #[wasm_bindgen_test]
    fn disabled_button_sets_disabled_and_aria() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            view! {
                <Button
                    label="Send"
                    data_mobile_test="send-btn"
                    disabled=true
                />
            }
        });
        let btn = container
            .query_selector("[data-mobile-test='send-btn']")
            .unwrap()
            .unwrap();
        assert!(
            btn.has_attribute("disabled"),
            "disabled prop must propagate to native disabled attr"
        );
        assert_eq!(
            btn.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "aria-disabled must mirror the disabled state"
        );
    }
}
