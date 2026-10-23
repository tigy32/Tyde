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
use host_config::diagnostics::{FrontendDiagnostic, FrontendDiagnosticRecord, SlowUiWork};
pub(crate) use host_config::diagnostics::{FrontendStartupPhase, UiOperation};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use wasm_bindgen::prelude::*;

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

fn operation_label(operation: UiOperation) -> &'static str {
    match operation {
        UiOperation::HostReceive => "host_receive",
        UiOperation::HostDispatch => "host_dispatch",
        UiOperation::Bridge => "bridge",
        UiOperation::ComposerInput => "composer_input",
        UiOperation::ComposerResize => "composer_resize",
        UiOperation::GlobalKeydown => "global_keydown",
        UiOperation::Markdown => "markdown",
        UiOperation::SyntaxHighlight => "syntax_highlight",
        UiOperation::WorkerSubmit => "highlight_worker_submit",
        UiOperation::WorkerReceive => "highlight_worker_receive",
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
    #[wasm_bindgen(js_name = createLocalMonitor)]
    fn create_local_monitor(env: &JsValue, tick: &JsValue, reset: &JsValue) -> JsValue;
    #[wasm_bindgen(js_name = localMonitorNow)]
    fn local_monitor_now(monitor: &JsValue) -> f64;
    #[wasm_bindgen(js_name = stopLocalMonitor)]
    fn stop_local_monitor(monitor: &JsValue);
}

struct LocalMonitor {
    handle: JsValue,
    _tick: Closure<dyn Fn(f64, bool, bool)>,
    _reset: Closure<dyn Fn(f64)>,
    started: f64,
    last_tick: f64,
    longest: Option<SlowUiWork>,
    pending: u32,
    queue: VecDeque<FrontendDiagnostic>,
    sending: bool,
    failed: bool,
    dropped: u32,
    window_start: f64,
    emitted: u32,
}

impl Drop for LocalMonitor {
    fn drop(&mut self) {
        stop_local_monitor(&self.handle);
    }
}

thread_local! {
    static LOCAL_MONITOR: RefCell<Option<LocalMonitor>> = const { RefCell::new(None) };
}

pub(crate) fn install_local_diagnostics() {
    install_local_monitor(&web_sys::window().unwrap().into());
    startup_phase(FrontendStartupPhase::WasmEntered);
}

fn install_local_monitor(env: &JsValue) {
    let tick = Closure::new(|now: f64, visible: bool, focused: bool| {
        let diagnostic = LOCAL_MONITOR.with_borrow_mut(|slot| {
            let m = slot.as_mut()?;
            let interval = millis(now - m.last_tick);
            m.last_tick = now;
            let longest_work = m.longest.take();
            (visible && interval >= 1250).then_some(FrontendDiagnostic::EventLoopGap {
                interval_ms: interval,
                expected_ms: 1000,
                focused,
                longest_work,
                pending_bridge_calls: m.pending,
            })
        });
        if let Some(diagnostic) = diagnostic {
            emit_local(diagnostic);
        }
    });
    let reset = Closure::new(|now: f64| {
        LOCAL_MONITOR.with_borrow_mut(|slot| {
            if let Some(m) = slot {
                m.last_tick = now;
                m.longest = None;
            }
        });
    });
    let handle = create_local_monitor(env, tick.as_ref(), reset.as_ref());
    let started = local_monitor_now(&handle);
    LOCAL_MONITOR.with_borrow_mut(|slot| {
        *slot = Some(LocalMonitor {
            handle,
            _tick: tick,
            _reset: reset,
            started,
            last_tick: started,
            longest: None,
            pending: 0,
            queue: VecDeque::new(),
            sending: false,
            failed: false,
            dropped: 0,
            window_start: started,
            emitted: 0,
        });
    });
}

fn millis(duration: f64) -> u32 {
    duration.clamp(0.0, u32::MAX as f64) as u32
}

pub(crate) fn startup_phase(phase: FrontendStartupPhase) {
    let diagnostic = LOCAL_MONITOR.with_borrow(|slot| {
        let m = slot.as_ref()?;
        Some(FrontendDiagnostic::Startup {
            phase,
            since_wasm_ms: millis(local_monitor_now(&m.handle) - m.started),
        })
    });
    if let Some(diagnostic) = diagnostic {
        emit_local(diagnostic);
    }
}

fn emit_local(diagnostic: FrontendDiagnostic) {
    let start_sender = LOCAL_MONITOR.with_borrow_mut(|slot| {
        let Some(m) = slot else {
            return false;
        };
        if m.failed {
            return false;
        }
        let now = local_monitor_now(&m.handle);
        if now - m.window_start >= 60_000.0 {
            m.window_start = now;
            m.emitted = 0;
        }
        if m.emitted == 32 || m.queue.len() == 16 {
            m.dropped = m.dropped.saturating_add(1);
            return false;
        }
        m.emitted += 1;
        m.queue.push_back(diagnostic);
        if m.sending {
            return false;
        }
        m.sending = true;
        true
    });
    if !start_sender {
        return;
    }
    wasm_bindgen_futures::spawn_local(async {
        loop {
            let record = LOCAL_MONITOR.with_borrow_mut(|slot| {
                let m = slot.as_mut()?;
                match m.queue.pop_front() {
                    Some(diagnostic) => Some(FrontendDiagnosticRecord {
                        diagnostic,
                        dropped: std::mem::take(&mut m.dropped),
                    }),
                    None => {
                        m.sending = false;
                        None
                    }
                }
            });
            let Some(record) = record else {
                return;
            };
            if crate::bridge::log_ui_diagnostic(record).await.is_err() {
                log::error!("Local UI diagnostics delivery failed; logging stopped for this page");
                LOCAL_MONITOR.with_borrow_mut(|slot| {
                    if let Some(m) = slot {
                        m.failed = true;
                        m.sending = false;
                        m.queue.clear();
                    }
                });
                return;
            }
        }
    });
}

// Detail is restricted to code-defined command names or the protocol FrameKind;
// callers must never pass IDs, paths, errors, arguments or user content here.
pub(crate) struct UiSpan {
    capture: f64,
    local: Option<(f64, UiOperation, bool)>,
}

impl UiSpan {
    fn local(operation: UiOperation, waiting: bool) -> Option<(f64, UiOperation, bool)> {
        LOCAL_MONITOR.with_borrow_mut(|slot| {
            let m = slot.as_mut()?;
            if waiting {
                m.pending = m.pending.saturating_add(1);
            }
            Some((local_monitor_now(&m.handle), operation, waiting))
        })
    }

    pub(crate) fn new(operation: UiOperation, bytes: usize, items: usize) -> Self {
        Self {
            capture: if diagnostic_enabled() {
                diagnostic_begin(operation_label(operation), "", bytes, items, false)
            } else {
                0.0
            },
            local: Self::local(operation, false),
        }
    }

    pub(crate) fn frame(kind: protocol::FrameKind) -> Self {
        Self {
            capture: if diagnostic_enabled() {
                diagnostic_begin(
                    operation_label(UiOperation::HostDispatch),
                    &kind.to_string(),
                    0,
                    1,
                    false,
                )
            } else {
                0.0
            },
            local: Self::local(UiOperation::HostDispatch, false),
        }
    }

    pub(crate) fn waiting(command: &'static str) -> Self {
        Self {
            capture: if diagnostic_enabled() {
                diagnostic_begin(operation_label(UiOperation::Bridge), command, 0, 0, true)
            } else {
                0.0
            },
            local: Self::local(UiOperation::Bridge, true),
        }
    }

    pub(crate) fn finish(mut self, success: bool) {
        if self.capture != 0.0 {
            diagnostic_end(self.capture, if success { "ok" } else { "error" });
        }
        self.capture = 0.0;
    }
}

impl Drop for UiSpan {
    fn drop(&mut self) {
        if self.capture != 0.0 {
            diagnostic_end(self.capture, "scope_end");
        }
        let Some((start, operation, waiting)) = self.local else {
            return;
        };
        LOCAL_MONITOR.with_borrow_mut(|slot| {
            let Some(m) = slot else {
                return;
            };
            if waiting {
                m.pending = m.pending.saturating_sub(1);
                return;
            }
            let duration_ms = millis(local_monitor_now(&m.handle) - start);
            if duration_ms >= 100 && m.longest.is_none_or(|work| duration_ms > work.duration_ms) {
                m.longest = Some(SlowUiWork {
                    operation,
                    duration_ms,
                });
            }
        });
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
