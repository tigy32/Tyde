use leptos::prelude::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/src/image-viewer.js")]
extern "C" {
    type ImageViewerHandle;

    #[wasm_bindgen(catch, js_name = attachImageViewer)]
    fn attach_image_viewer(
        dialog: &web_sys::HtmlDialogElement,
    ) -> Result<ImageViewerHandle, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn open(this: &ImageViewerHandle, opener: &web_sys::Element) -> Result<(), JsValue>;

    #[wasm_bindgen(method)]
    fn reset(this: &ImageViewerHandle);

    #[wasm_bindgen(method)]
    fn destroy(this: &ImageViewerHandle);
}

#[derive(Clone, Copy)]
struct ImageViewerOpener(Callback<(Vec<protocol::ImageData>, usize, web_sys::Element)>);

#[component]
pub(super) fn ImageGallery(images: Vec<protocol::ImageData>) -> impl IntoView {
    let images = StoredValue::new(images);
    let opener = expect_context::<ImageViewerOpener>();
    view! {
        <div class="chat-message-images" data-mobile-test="chat-message-images">
            {images.with_value(|items| items.iter().enumerate().map(|(index, image)| {
                let src = image_source(image);
                view! {
                    <button
                        type="button"
                        class="chat-message-image-button"
                        aria-label="Open image full size"
                        // Cancel mouse focus without suppressing WebKit's completed touch click.
                        on:mousedown=move |ev| ev.prevent_default()
                        on:click=move |ev| opener.0.run((images.get_value(), index, event_target::<web_sys::Element>(&ev)))
                    >
                        <img class="chat-message-image" src=src alt="Chat image" loading="lazy" />
                    </button>
                }
            }).collect::<Vec<_>>())}
        </div>
    }
}

fn image_source(image: &protocol::ImageData) -> String {
    format!("data:{};base64,{}", image.media_type, image.data)
}

// Chat owns the modal, not a reactive transcript row. The open selection is
// presentation state and retains its typed pixels while later messages render.
pub(super) fn image_viewer() -> impl IntoView {
    let sources = RwSignal::new(Vec::<String>::new());
    let count = move || sources.with(Vec::len);
    let selected = RwSignal::new(0usize);
    let dialog_ref = NodeRef::<leptos::html::Dialog>::new();
    let handle = StoredValue::new_local(None::<ImageViewerHandle>);
    let error = RwSignal::new(None::<&'static str>);

    Effect::new(move |_| {
        let Some(dialog) = dialog_ref.get() else {
            return;
        };
        match attach_image_viewer(&dialog) {
            Ok(viewer) => handle.set_value(Some(viewer)),
            Err(_) => {
                log::error!("Mobile image viewer could not attach");
                error.set(Some("Image viewer could not open."));
            }
        }
    });
    on_cleanup(move || {
        handle.with_value(|handle| {
            if let Some(handle) = handle {
                handle.destroy();
            }
        });
    });
    provide_context(ImageViewerOpener(Callback::new(
        move |(images, index, opener): (Vec<protocol::ImageData>, usize, web_sys::Element)| {
            sources.set(images.iter().map(image_source).collect());
            selected.set(index);
            let result = handle.with_value(|handle| {
                handle
                    .as_ref()
                    .ok_or(JsValue::NULL)
                    .and_then(|handle| handle.open(&opener))
            });
            if result.is_err() {
                log::error!("Mobile image viewer could not open");
                error.set(Some("Image viewer could not open."));
            }
        },
    )));

    let step = move |forward: bool| {
        selected.update(|index| {
            *index = if forward {
                (*index + 1).min(count().saturating_sub(1))
            } else {
                index.saturating_sub(1)
            };
        });
        handle.with_value(|handle| {
            if let Some(handle) = handle {
                handle.reset();
            }
        });
    };

    view! {
        <div>
            {move || error.get().map(|message| view! { <div role="alert">{message}</div> })}
            <dialog
                class="phone-image-viewer"
                aria-label="Image viewer"
                aria-modal="true"
                node_ref=dialog_ref
            >
                <div class="phone-image-stage">
                    <img
                        src=move || sources.with(|sources| sources.get(selected.get()).cloned())
                        alt="Chat image, full size"
                        draggable="false"
                    />
                    <p class="phone-image-error" role="alert" hidden>"This image could not be displayed."</p>
                </div>
                <header class="phone-image-header">
                    <span>"Image " {move || selected.get() + 1} " / " {count}</span>
                    <button type="button" aria-label="Close image viewer" data-image-close autofocus>
                        <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
                            <path d="m6 6 12 12M18 6 6 18" />
                        </svg>
                    </button>
                </header>
                <footer class="phone-image-footer">
                    <div class="phone-image-controls">
                        <button type="button" aria-label="Previous image" data-image-previous
                            disabled=move || selected.get() == 0
                            on:click=move |_| step(false)>"‹"</button>
                        <button type="button" aria-label="Zoom out" data-image-zoom-out>"−"</button>
                        <button type="button" aria-label="Reset zoom" data-image-reset>"100%"</button>
                        <button type="button" aria-label="Zoom in" data-image-zoom-in>"+"</button>
                        <button type="button" aria-label="Next image" data-image-next
                            disabled={move || selected.get() + 1 >= count()}
                            on:click=move |_| step(true)>"›"</button>
                    </div>
                    <p>"Pinch or double-tap to zoom · Drag to explore"</p>
                </footer>
            </dialog>
        </div>
    }
}
