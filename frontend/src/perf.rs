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

#[derive(Clone, Copy)]
pub(crate) enum UiOperation {
    HostReceive,
    HostDispatch,
    Bridge,
    ComposerInput,
    ComposerResize,
    GlobalKeydown,
    Markdown,
    SyntaxHighlight,
    #[cfg(target_arch = "wasm32")]
    WorkerSubmit,
    #[cfg(target_arch = "wasm32")]
    WorkerReceive,
}

impl UiOperation {
    fn label(self) -> &'static str {
        match self {
            Self::HostReceive => "host_receive",
            Self::HostDispatch => "host_dispatch",
            Self::Bridge => "bridge",
            Self::ComposerInput => "composer_input",
            Self::ComposerResize => "composer_resize",
            Self::GlobalKeydown => "global_keydown",
            Self::Markdown => "markdown",
            Self::SyntaxHighlight => "syntax_highlight",
            #[cfg(target_arch = "wasm32")]
            Self::WorkerSubmit => "highlight_worker_submit",
            #[cfg(target_arch = "wasm32")]
            Self::WorkerReceive => "highlight_worker_receive",
        }
    }
}

#[wasm_bindgen::prelude::wasm_bindgen(module = "/src/stall_diagnostics.js")]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = installDiagnostics)]
    pub(crate) fn install_diagnostics();
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = diagnosticEnabled)]
    fn diagnostic_enabled() -> bool;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = diagnosticBegin)]
    fn diagnostic_begin(
        operation: &str,
        detail: &str,
        bytes: usize,
        items: usize,
        waiting: bool,
    ) -> f64;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = diagnosticEnd)]
    fn diagnostic_end(id: f64, outcome: &str);
}

// Detail is restricted to code-defined command names or the protocol FrameKind;
// callers must never pass IDs, paths, errors, arguments or user content here.
pub(crate) struct UiSpan(f64);

impl UiSpan {
    pub(crate) fn new(operation: UiOperation, bytes: usize, items: usize) -> Self {
        if !diagnostic_enabled() {
            return Self(0.0);
        }
        Self(diagnostic_begin(operation.label(), "", bytes, items, false))
    }

    pub(crate) fn frame(kind: protocol::FrameKind) -> Self {
        if !diagnostic_enabled() {
            return Self(0.0);
        }
        Self(diagnostic_begin(
            UiOperation::HostDispatch.label(),
            &kind.to_string(),
            0,
            1,
            false,
        ))
    }

    pub(crate) fn waiting(command: &'static str) -> Self {
        if !diagnostic_enabled() {
            return Self(0.0);
        }
        Self(diagnostic_begin(
            UiOperation::Bridge.label(),
            command,
            0,
            0,
            true,
        ))
    }

    pub(crate) fn finish(mut self, success: bool) {
        if self.0 != 0.0 {
            diagnostic_end(self.0, if success { "ok" } else { "error" });
        }
        self.0 = 0.0;
    }
}

impl Drop for UiSpan {
    fn drop(&mut self) {
        if self.0 != 0.0 {
            diagnostic_end(self.0, "scope_end");
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod diagnostic_tests {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen(module = "/src/stall_diagnostics.js")]
    extern "C" {
        #[wasm_bindgen(js_name = installTestDiagnostics)]
        fn install_test_diagnostics(env: &JsValue) -> JsValue;
    }

    #[wasm_bindgen(inline_js = r#"
export function diagnosticTestEnvironment() {
    let time = 1000, timer = null, frame = null, foreground = true;
    const eventDocument = {
        get visibilityState() { return foreground ? 'visible' : 'hidden'; },
        addEventListener(...args) { document.addEventListener(...args); },
        removeEventListener(...args) { document.removeEventListener(...args); },
    };
    return {
        document: eventDocument, Date: {now: () => 1800000000000}, performance: {now: () => time},
        setTimeout(fn) { timer = fn; return 1; }, clearTimeout() { timer = null; },
        requestAnimationFrame(fn) { frame = fn; return 2; }, cancelAnimationFrame() { frame = null; },
        visibility(value) {
            foreground = value === 'visible';
            document.dispatchEvent(new Event('visibilitychange'));
        },
        step(source, amount) {
            time += amount;
            if (source === 'timer') { const fn = timer; timer = null; if (fn) fn(); }
            if (source === 'frame') { const fn = frame; frame = null; if (fn) fn(); }
        },
    };
}
export function diagnosticTestCall(object, method, source, amount) {
    if (method === 'step') return object.step(source, amount);
    if (method === 'visibility') return object.visibility(source);
    return object[method]();
}
"#)]
    extern "C" {
        #[wasm_bindgen(js_name = diagnosticTestEnvironment)]
        fn test_environment() -> JsValue;
        #[wasm_bindgen(js_name = diagnosticTestCall)]
        fn call(object: &JsValue, method: &str, source: &str, amount: f64) -> JsValue;
    }

    pub(crate) struct Capture {
        env: JsValue,
        recorder: JsValue,
    }

    impl Capture {
        pub(crate) fn new() -> Self {
            let env = test_environment();
            let recorder = install_test_diagnostics(&env);
            Self { env, recorder }
        }

        pub(crate) fn start(&self) {
            call(&self.recorder, "start", "", 0.0);
        }

        pub(crate) fn step(&self, source: &str, amount: f64) {
            call(&self.env, "step", source, amount);
        }

        pub(crate) fn visible(&self, value: bool) {
            call(
                &self.env,
                "visibility",
                if value { "visible" } else { "hidden" },
                0.0,
            );
        }

        pub(crate) fn stop(&self) {
            call(&self.recorder, "stop", "", 0.0);
        }

        pub(crate) fn export(&self) -> serde_json::Value {
            let json = call(&self.recorder, "export", "", 0.0).as_string().unwrap();
            serde_json::from_str(&json).unwrap()
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            call(&self.recorder, "clear", "", 0.0);
        }
    }
}
