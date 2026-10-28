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
use host_config::diagnostics::{
    FrameName, FrontendDiagnostic, FrontendDiagnosticRecord, SlowUiWork,
};
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
        UiOperation::FileTreeBuild => "file_tree_build",
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
    #[wasm_bindgen(js_name = diagnosticSetFrame)]
    fn diagnostic_set_frame(id: f64, frame: &str);
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
    local: Option<LocalSpan>,
}

struct LocalSpan {
    start: f64,
    operation: UiOperation,
    waiting: bool,
    frame: Option<protocol::FrameKind>,
    bytes: usize,
    items: usize,
}

impl UiSpan {
    fn local(
        operation: UiOperation,
        waiting: bool,
        frame: Option<protocol::FrameKind>,
        bytes: usize,
        items: usize,
    ) -> Option<LocalSpan> {
        LOCAL_MONITOR.with_borrow_mut(|slot| {
            let m = slot.as_mut()?;
            if waiting {
                m.pending = m.pending.saturating_add(1);
            }
            Some(LocalSpan {
                start: local_monitor_now(&m.handle),
                operation,
                waiting,
                frame,
                bytes,
                items,
            })
        })
    }

    pub(crate) fn new(operation: UiOperation, bytes: usize, items: usize) -> Self {
        Self {
            capture: if diagnostic_enabled() {
                diagnostic_begin(operation_label(operation), "", bytes, items, false)
            } else {
                0.0
            },
            local: Self::local(operation, false, None, bytes, items),
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
            local: Self::local(UiOperation::HostDispatch, false, Some(kind), 0, 1),
        }
    }

    pub(crate) fn waiting(command: &'static str) -> Self {
        Self {
            capture: if diagnostic_enabled() {
                diagnostic_begin(operation_label(UiOperation::Bridge), command, 0, 0, true)
            } else {
                0.0
            },
            local: Self::local(UiOperation::Bridge, true, None, 0, 0),
        }
    }

    /// Attributes a receive span to the frame it parsed.
    pub(crate) fn set_frame(&mut self, kind: protocol::FrameKind) {
        if self.capture != 0.0 {
            diagnostic_set_frame(self.capture, &kind.to_string());
        }
        if let Some(local) = &mut self.local {
            local.frame = Some(kind);
        }
    }

    pub(crate) fn finish(mut self, success: bool) {
        if self.capture != 0.0 {
            diagnostic_end(self.capture, if success { "ok" } else { "error" });
        }
        self.capture = 0.0;
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl Drop for UiSpan {
    fn drop(&mut self) {
        if self.capture != 0.0 {
            diagnostic_end(self.capture, "scope_end");
        }
        let Some(span) = self.local.take() else {
            return;
        };
        LOCAL_MONITOR.with_borrow_mut(|slot| {
            let Some(m) = slot else {
                return;
            };
            if span.waiting {
                m.pending = m.pending.saturating_sub(1);
                return;
            }
            let duration_ms = millis(local_monitor_now(&m.handle) - span.start);
            if duration_ms >= 100
                && m.longest
                    .as_ref()
                    .is_none_or(|work| duration_ms > work.duration_ms)
            {
                m.longest = Some(SlowUiWork {
                    operation: span.operation,
                    duration_ms,
                    frame: span.frame.map(|kind| {
                        serde_json::to_value(kind)
                            .and_then(serde_json::from_value::<FrameName>)
                            .expect("protocol frame kinds serialize as snake_case identifiers")
                    }),
                    bytes: count(span.bytes),
                    items: count(span.items),
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
export function diagnosticTestEnvironment(browserObserver) {
    let time = 1000, timer = null, frame = null, foreground = true;
    let delivered;
    const eventDocument = {
        get visibilityState() { return foreground ? 'visible' : 'hidden'; },
        addEventListener(...args) { document.addEventListener(...args); },
        removeEventListener(...args) { document.removeEventListener(...args); },
    };
    return {
        document: eventDocument, Date: {now: () => 1800000000000}, performance: {now: () => time},
        // Use real asynchronous browser observer delivery with explicitly timed
        // measures; generating long tasks would make this test CPU-dependent.
        PerformanceObserver: browserObserver ? class {
            static supportedEntryTypes = ['longtask'];
            constructor(callback) {
                this.observer = new PerformanceObserver(list => {
                    callback(list);
                    delivered();
                });
            }
            observe() { this.observer.observe({type: 'measure'}); }
            takeRecords() { return this.observer.takeRecords(); }
            disconnect() { this.observer.disconnect(); }
        } : undefined,
        browserWork(start, end) {
            const delivery = new Promise(resolve => { delivered = resolve; });
            performance.measure('private source credential argv bearer', {
                start, end, detail: {path: '/private/source', nativeId: 'private-native-id'},
            });
            performance.clearMeasures('private source credential argv bearer');
            return delivery;
        },
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
export function diagnosticTestBrowserWork(env, start, end) { return env.browserWork(start, end); }
"#)]
    extern "C" {
        #[wasm_bindgen(js_name = diagnosticTestEnvironment)]
        fn test_environment(browser_observer: bool) -> JsValue;
        #[wasm_bindgen(js_name = diagnosticTestCall)]
        fn call(object: &JsValue, method: &str, source: &str, amount: f64) -> JsValue;
        #[wasm_bindgen(js_name = diagnosticTestBrowserWork)]
        fn browser_work(env: &JsValue, start: f64, end: f64) -> js_sys::Promise;
    }

    #[wasm_bindgen(inline_js = r#"
export function localMonitorTestEnvironment() {
    let time = 1000, timer = null;
    return {
        document: {
            visibilityState: 'visible', hasFocus: () => true,
            addEventListener() {}, removeEventListener() {},
        },
        // Every reading advances the injected clock, so synchronous work has a
        // deterministic nonzero duration without real time passing.
        performance: {now: () => (time += 150)},
        setTimeout(fn) { timer = fn; return 1; }, clearTimeout() { timer = null; },
        advance(amount) { time += amount; },
        tick() { const fn = timer; timer = null; if (fn) fn(); },
    };
}
export function localMonitorTestCall(env, method, amount) { return env[method](amount); }
"#)]
    extern "C" {
        #[wasm_bindgen(js_name = localMonitorTestEnvironment)]
        fn local_monitor_test_environment() -> JsValue;
        #[wasm_bindgen(js_name = localMonitorTestCall)]
        fn local_monitor_test_call(env: &JsValue, method: &str, amount: f64);
    }

    /// The automatic local monitor on an injected clock and timer; dropping it
    /// uninstalls the monitor.
    pub(crate) struct LocalMonitorClock {
        env: JsValue,
    }

    impl LocalMonitorClock {
        pub(crate) fn install() -> Self {
            let env = local_monitor_test_environment();
            super::install_local_monitor(&env);
            Self { env }
        }

        pub(crate) fn advance(&self, amount: f64) {
            local_monitor_test_call(&self.env, "advance", amount);
        }

        pub(crate) fn tick(&self) {
            local_monitor_test_call(&self.env, "tick", 0.0);
        }
    }

    impl Drop for LocalMonitorClock {
        fn drop(&mut self) {
            super::LOCAL_MONITOR.with_borrow_mut(|slot| *slot = None);
        }
    }

    pub(crate) struct Capture {
        env: JsValue,
        recorder: JsValue,
    }

    impl Capture {
        pub(crate) fn new() -> Self {
            Self::with_observer(false)
        }

        pub(crate) fn with_observer(browser_observer: bool) -> Self {
            let env = test_environment(browser_observer);
            let recorder = install_test_diagnostics(&env);
            Self { env, recorder }
        }

        pub(crate) fn browser_work(&self, start: f64, end: f64) -> js_sys::Promise {
            browser_work(&self.env, start, end)
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
