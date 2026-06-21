use leptos::prelude::*;

use super::{Button, ButtonVariant};

/// In-app confirmation modal — the cross-platform replacement for
/// `window.confirm`, which is a silent no-op inside webviews (CLAUDE.md) and
/// unstyled in browsers. Renders an overlay with a title, message, and
/// confirm/cancel actions; both backends (Tauri shell and browser PWA) reach
/// destructive confirmations through this same Leptos modal rather than any
/// native/`window` dialog.
///
/// The modal is a pure projection of `open`: callers own a `RwSignal<bool>`
/// (or any reactive `bool`), and the modal renders nothing while closed.
#[component]
pub fn ConfirmModal(
    #[prop(into)] open: Signal<bool>,
    #[prop(into)] title: String,
    #[prop(into)] message: String,
    #[prop(optional, into)] confirm_label: Option<String>,
    #[prop(optional, into)] cancel_label: Option<String>,
    #[prop(optional)] destructive: Option<bool>,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
    #[prop(into)] on_confirm: Callback<()>,
    #[prop(into)] on_cancel: Callback<()>,
) -> impl IntoView {
    let confirm_label = confirm_label.unwrap_or_else(|| "Confirm".to_owned());
    let cancel_label = cancel_label.unwrap_or_else(|| "Cancel".to_owned());
    let confirm_variant = if destructive.unwrap_or(false) {
        ButtonVariant::Destructive
    } else {
        ButtonVariant::Primary
    };
    let test = data_mobile_test.unwrap_or("confirm-modal");

    view! {
        <Show when=move || open.get()>
            {
                let title = title.clone();
                let message = message.clone();
                let confirm_label = confirm_label.clone();
                let cancel_label = cancel_label.clone();
                view! {
                    <div class="confirm-modal-backdrop" role="dialog" aria-modal="true" data-mobile-test=test>
                        <div class="confirm-modal">
                            <h2 class="confirm-modal-title">{title}</h2>
                            <p class="confirm-modal-message">{message}</p>
                            <div class="confirm-modal-actions">
                                <Button
                                    label=cancel_label
                                    variant=ButtonVariant::Secondary
                                    full_width=true
                                    data_mobile_test="confirm-modal-cancel"
                                    on_click=Callback::new(move |_: ()| on_cancel.run(()))
                                />
                                <Button
                                    label=confirm_label
                                    variant=confirm_variant
                                    full_width=true
                                    data_mobile_test="confirm-modal-confirm"
                                    on_click=Callback::new(move |_: ()| on_confirm.run(()))
                                />
                            </div>
                        </div>
                    </div>
                }
            }
        </Show>
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

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    #[wasm_bindgen_test]
    async fn modal_hidden_when_closed_and_shows_message_when_open() {
        let open = RwSignal::new(false);
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            view! {
                <ConfirmModal
                    open=open
                    title="Forget host?"
                    message="This removes the saved credential."
                    destructive=true
                    on_confirm=Callback::new(|_: ()| {})
                    on_cancel=Callback::new(|_: ()| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='confirm-modal']")
                .unwrap()
                .is_none(),
            "modal must render nothing while closed"
        );

        open.set(true);
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Forget host?"),
            "modal must show its title: {text}"
        );
        assert!(
            text.contains("This removes the saved credential."),
            "modal must show its message: {text}"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='confirm-modal-confirm']")
                .unwrap()
                .is_some(),
            "modal must surface a confirm action"
        );
    }

    #[wasm_bindgen_test]
    async fn confirm_button_fires_callback() {
        let open = RwSignal::new(true);
        let fired = RwSignal::new(false);
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            view! {
                <ConfirmModal
                    open=open
                    title="Confirm"
                    message="Proceed?"
                    on_confirm=Callback::new(move |_: ()| fired.set(true))
                    on_cancel=Callback::new(|_: ()| {})
                />
            }
        });
        next_tick().await;
        let confirm: HtmlElement = container
            .query_selector("[data-mobile-test='confirm-modal-confirm']")
            .unwrap()
            .expect("confirm button present")
            .dyn_into()
            .unwrap();
        confirm.click();
        next_tick().await;
        assert!(fired.get_untracked(), "confirm click must run on_confirm");
    }
}
