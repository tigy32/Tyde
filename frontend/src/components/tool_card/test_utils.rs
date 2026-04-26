//! Shared helpers for per-renderer `wasm_tests` modules.
//!
//! Each renderer file owns its own `mod wasm_tests` (per CLAUDE.md), but they
//! all need the same scaffolding: a sized container, a deterministic `next_tick`
//! flush, a way to mount a plain renderer fn, and a way to query body text. The
//! helpers below keep the renderer tests small and focused on user-perceived
//! output.

use leptos::IntoView;
use leptos::mount::mount_to;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;

/// Append a sized container to the body so child elements have real layout.
pub fn make_container() -> HtmlElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let container = document.create_element("div").unwrap();
    container
        .set_attribute(
            "style",
            "position: absolute; top: 0; left: 0; width: 800px; height: 600px;",
        )
        .unwrap();
    document.body().unwrap().append_child(&container).unwrap();
    container.dyn_into::<HtmlElement>().unwrap()
}

/// Yield to the browser event loop so reactive effects flush.
pub async fn next_tick() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
            .unwrap();
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Mount a renderer-returning closure and return the surrounding container.
/// Holds the mount handle in a thread-local so the DOM survives the test until
/// the next `mount`.
pub fn mount<V, F>(view_fn: F) -> HtmlElement
where
    V: IntoView + 'static,
    F: FnOnce() -> V + 'static,
{
    let container = make_container();
    let handle = mount_to(container.clone(), view_fn);
    handle.forget();
    container
}

/// Visible text content of the mounted DOM, with whitespace collapsed.
pub fn text(container: &HtmlElement) -> String {
    let raw = container.text_content().unwrap_or_default();
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True if a `.tool-show-more` button exists somewhere under the container.
pub fn has_show_more(container: &HtmlElement) -> bool {
    container
        .query_selector(".tool-show-more")
        .ok()
        .flatten()
        .is_some()
}

/// Count of elements matching `selector`.
pub fn count(container: &HtmlElement, selector: &str) -> usize {
    container
        .query_selector_all(selector)
        .map(|nodes| nodes.length() as usize)
        .unwrap_or(0)
}
