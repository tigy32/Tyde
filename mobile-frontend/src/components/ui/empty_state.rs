use leptos::prelude::*;

/// Standard mobile empty state: icon glyph, headline, body, optional
/// CTA. Used by every list view so absence-of-data is informative
/// rather than blank.
#[component]
pub fn EmptyState(
    #[prop(into)] title: String,
    #[prop(into)] body: String,
    #[prop(optional, into)] icon: Option<String>,
    #[prop(optional, into)] cta_label: Option<String>,
    #[prop(optional)] cta_test: Option<&'static str>,
    #[prop(optional)] on_cta: Option<Callback<()>>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
) -> impl IntoView {
    let icon = icon.unwrap_or_else(|| "\u{2728}".to_owned());
    let test = data_mobile_test.unwrap_or("empty-state");
    let cta_test = cta_test.unwrap_or("empty-state-cta");
    let on_cta_for_btn = on_cta;

    view! {
        <div class="ui-empty-state" role="status" data-mobile-test=test>
            <div class="ui-empty-state-icon" aria-hidden="true">{icon}</div>
            <h2 class="ui-empty-state-title">{title}</h2>
            <p class="ui-empty-state-body">{body}</p>
            {cta_label.map(|label| view! {
                <button
                    type="button"
                    class="ui-button ui-button-primary ui-empty-state-cta"
                    data-mobile-test=cta_test
                    on:click=move |_| {
                        if let Some(cb) = on_cta_for_btn.as_ref() {
                            cb.run(());
                        }
                    }
                >
                    <span class="ui-button-label">{label}</span>
                </button>
            })}
        </div>
    }
}
