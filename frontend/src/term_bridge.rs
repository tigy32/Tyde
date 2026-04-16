//! Rust side of the xterm.js bridge defined in `vendor/xterm-bridge.js`.
//!
//! A terminal instance is identified by its `TerminalId` string. Bridge calls
//! are no-ops when `window.TydeTerm` is not present, so failing to load the
//! vendor JS only degrades to a blank terminal rather than a panic.

use std::cell::RefCell;
use std::collections::HashMap;

use js_sys::{Function, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::HtmlElement;

// The JS callbacks we hand to xterm.js must outlive the lifetime of the
// emulator, but they are not `Send + Sync` (they wrap Rust closures through
// wasm_bindgen). Leptos `on_cleanup` requires `Send + Sync`, so stash the
// callbacks in a thread-local map keyed by terminal id. The frontend is
// single-threaded wasm, so this is safe.
thread_local! {
    static HANDLES: RefCell<HashMap<String, Handles>> = RefCell::new(HashMap::new());
}

struct Handles {
    _on_data: Closure<dyn Fn(String)>,
    _on_resize: Closure<dyn Fn(f64, f64)>,
}

/// Returns `window.TydeTerm` if it exists.
fn bridge() -> Option<JsValue> {
    let window = web_sys::window()?;
    let handle = Reflect::get(&window, &JsValue::from_str("TydeTerm")).ok()?;
    if handle.is_undefined() || handle.is_null() {
        return None;
    }
    Some(handle)
}

fn method(bridge: &JsValue, name: &str) -> Option<Function> {
    Reflect::get(bridge, &JsValue::from_str(name))
        .ok()?
        .dyn_into::<Function>()
        .ok()
}

/// Create an xterm attached to `container`. Returns true if the bridge call
/// succeeded.
///
/// `on_data` is invoked with user keystrokes / pastes from the emulator.
/// `on_resize` is invoked with `(cols, rows)` whenever the PTY size changes
/// (typically from `fit()` after a container resize).
///
/// The callbacks are stored in a process-local map and freed by [`dispose`].
pub fn create(
    id: &str,
    container: &HtmlElement,
    on_data: Closure<dyn Fn(String)>,
    on_resize: Closure<dyn Fn(f64, f64)>,
) -> bool {
    let Some(bridge) = bridge() else { return false };
    let Some(create_fn) = method(&bridge, "create") else {
        return false;
    };
    let call = create_fn.call4(
        &bridge,
        &JsValue::from_str(id),
        container.as_ref(),
        on_data.as_ref().unchecked_ref(),
        on_resize.as_ref().unchecked_ref(),
    );
    if call.is_err() {
        return false;
    }
    HANDLES.with(|handles| {
        handles.borrow_mut().insert(
            id.to_string(),
            Handles {
                _on_data: on_data,
                _on_resize: on_resize,
            },
        );
    });
    true
}

/// Write a chunk of terminal output into the emulator.
pub fn write(id: &str, data: &str) {
    let Some(bridge) = bridge() else { return };
    let Some(func) = method(&bridge, "write") else {
        return;
    };
    let _ = func.call2(&bridge, &JsValue::from_str(id), &JsValue::from_str(data));
}

/// Dispose of the terminal and release its resources.
pub fn dispose(id: &str) {
    if let Some(bridge) = bridge()
        && let Some(func) = method(&bridge, "dispose")
    {
        let _ = func.call1(&bridge, &JsValue::from_str(id));
    }
    HANDLES.with(|handles| {
        handles.borrow_mut().remove(id);
    });
}

/// Trigger a fit-to-container layout.
pub fn fit(id: &str) {
    let Some(bridge) = bridge() else { return };
    let Some(func) = method(&bridge, "fit") else {
        return;
    };
    let _ = func.call1(&bridge, &JsValue::from_str(id));
}

/// Focus the terminal.
pub fn focus(id: &str) {
    let Some(bridge) = bridge() else { return };
    let Some(func) = method(&bridge, "focus") else {
        return;
    };
    let _ = func.call1(&bridge, &JsValue::from_str(id));
}
