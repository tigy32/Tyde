use js_sys::{Function, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::HtmlElement;

/// Run highlight.js on every `<pre><code>` inside `root` that hasn't been
/// processed yet. No-ops if hljs isn't loaded.
pub fn highlight_code_blocks(root: &HtmlElement) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(hljs) = Reflect::get(&window, &JsValue::from_str("hljs")) else {
        return;
    };
    if hljs.is_undefined() || hljs.is_null() {
        return;
    }
    let Ok(func) = Reflect::get(&hljs, &JsValue::from_str("highlightElement")) else {
        return;
    };
    let Ok(func) = func.dyn_into::<Function>() else {
        return;
    };

    let Ok(nodes) = root.query_selector_all("pre code:not(.hljs)") else {
        return;
    };
    for i in 0..nodes.length() {
        if let Some(node) = nodes.item(i) {
            let _ = func.call1(&hljs, &node);
        }
    }
}
