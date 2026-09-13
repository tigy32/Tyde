use std::sync::Arc;

use leptos::portal::Portal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// Full-screen view of one image from a set, stepped with the arrow keys.
///
/// Portaled to `<body>` so no transcript ancestor's overflow or transform can
/// clip the fixed overlay. `current` is owned by the thumbnails: `Some(i)`
/// shows image `i`, and every way out of the viewer sets it back to `None`.
#[component]
pub fn ImageLightbox(sources: Arc<[String]>, current: RwSignal<Option<usize>>) -> impl IntoView {
    let count = sources.len();
    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    let prev_ref = NodeRef::<leptos::html::Button>::new();
    let next_ref = NodeRef::<leptos::html::Button>::new();

    // Captured at construction, while focus is still on the thumbnail that
    // opened the viewer; by the time the mount effect runs it has moved.
    let opener: StoredValue<Option<web_sys::HtmlElement>, LocalStorage> = StoredValue::new_local(
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element())
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok()),
    );

    Effect::new(move |_| {
        if let Some(dialog) = dialog_ref.get() {
            let _ = dialog.focus();
        }
    });

    on_cleanup(move || {
        if let Some(opener) = opener.get_value() {
            let _ = opener.focus();
        }
    });

    let close = move || current.set(None);

    // A nav button is disabled at its end of the set, and a disabled button
    // drops focus to `<body>` — where the viewer's key handler can no longer
    // hear Escape. Hand focus back to the dialog before that happens.
    let step = move |forward: bool| {
        let Some(index) = current.get_untracked() else {
            return;
        };
        let target = if forward {
            (index + 1).min(count.saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        if target == index {
            return;
        }
        let exhausted = if forward { next_ref } else { prev_ref };
        let at_end = if forward {
            target + 1 == count
        } else {
            target == 0
        };
        if at_end
            && exhausted
                .get_untracked()
                .is_some_and(|button| is_focused(&button))
            && let Some(dialog) = dialog_ref.get_untracked()
        {
            let _ = dialog.focus();
        }
        current.set(Some(target));
    };

    let on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Escape" => {
            // The app's global Escape closes Settings and the find bar; the
            // viewer is the top layer, so this press is spent on it alone.
            ev.prevent_default();
            ev.stop_propagation();
            close();
        }
        "ArrowLeft" => {
            ev.prevent_default();
            step(false);
        }
        "ArrowRight" => {
            ev.prevent_default();
            step(true);
        }
        "Tab" => {
            let Some(dialog) = dialog_ref.get_untracked() else {
                return;
            };
            let Ok(buttons) = dialog.query_selector_all("button:not([disabled])") else {
                return;
            };
            let (Some(first), Some(last)) = (
                buttons
                    .item(0)
                    .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()),
                buttons
                    .item(buttons.length().saturating_sub(1))
                    .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()),
            ) else {
                return;
            };
            let on_dialog = is_focused(&dialog);
            if ev.shift_key() && (on_dialog || is_focused(&first)) {
                ev.prevent_default();
                let _ = last.focus();
            } else if !ev.shift_key() && (on_dialog || is_focused(&last)) {
                ev.prevent_default();
                let _ = first.focus();
            }
        }
        _ => {}
    };

    let src = Memo::new(move |_| {
        current
            .get()
            .and_then(|index| sources.get(index).cloned())
            .unwrap_or_default()
    });
    let position = move || current.get().map_or(0, |index| index + 1);

    view! {
        <Portal>
            <div
                class="image-lightbox"
                role="dialog"
                aria-modal="true"
                aria-label="Image viewer"
                tabindex="-1"
                node_ref=dialog_ref
                on:keydown=on_keydown
                on:click=move |_| close()
            >
                <img class="image-lightbox-image" src=src alt="Chat image, full size" />
                <Show when=move || { count > 1 }>
                    <div class="image-lightbox-counter" aria-live="polite">
                        {position} " / " {count}
                    </div>
                    <button
                        type="button"
                        class="image-lightbox-nav image-lightbox-prev"
                        aria-label="Previous image"
                        node_ref=prev_ref
                        prop:disabled=move || current.get() == Some(0)
                        on:click=move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            step(false);
                        }
                    >
                        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                            <polyline points="15 18 9 12 15 6" />
                        </svg>
                    </button>
                    <button
                        type="button"
                        class="image-lightbox-nav image-lightbox-next"
                        aria-label="Next image"
                        node_ref=next_ref
                        prop:disabled=move || current.get() == Some(count.saturating_sub(1))
                        on:click=move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            step(true);
                        }
                    >
                        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                            <polyline points="9 18 15 12 9 6" />
                        </svg>
                    </button>
                </Show>
                <button
                    type="button"
                    class="image-lightbox-close"
                    aria-label="Close image viewer"
                    on:click=move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        close();
                    }
                >
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="18" y1="6" x2="6" y2="18" />
                        <line x1="6" y1="6" x2="18" y2="18" />
                    </svg>
                </button>
            </div>
        </Portal>
    }
}

fn is_focused(element: &web_sys::HtmlElement) -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .is_some_and(|active| active.is_same_node(Some(element.unchecked_ref())))
}
