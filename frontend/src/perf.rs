//! Lightweight client-side performance instrumentation. Each `log_phase` call
//! emits a single line to the browser console tagged with `[perf <flow>]`.
//! Phases that share a `key` (e.g. the file path) report `from_click=Xms` so
//! the breakdown of a single open is readable without tailing timestamps.
//!
//! Usage:
//! ```ignore
//! perf::mark_start("file:src/foo.rs");
//! perf::log_phase("file_open", "click", "file:src/foo.rs", "");
//! // …request flight…
//! perf::log_phase("file_open", "response", "file:src/foo.rs", " bytes=12345");
//! ```
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static OPEN_STARTS: RefCell<HashMap<String, f64>> = RefCell::new(HashMap::new());
}

pub fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

pub fn mark_start(key: &str) {
    let now = now_ms();
    OPEN_STARTS.with(|m| {
        m.borrow_mut().insert(key.to_owned(), now);
    });
}

fn since_start(key: &str) -> Option<f64> {
    OPEN_STARTS.with(|m| m.borrow().get(key).map(|s| now_ms() - s))
}

pub fn log_phase(flow: &str, phase: &str, key: &str, extra: &str) {
    let from_start = since_start(key)
        .map(|d| format!(" from_click={d:.1}ms"))
        .unwrap_or_default();
    log::info!("[perf {flow}] {phase} key={key}{from_start}{extra}");
}
