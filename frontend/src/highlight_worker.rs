//! Web Worker that runs syntect off the main thread.
//!
//! Why: tokenizing a moderate Rust file (≈2000 lines) in syntect costs ~2s
//! of CPU in debug builds. Even chunked with `spawn_local` + yields, each
//! chunk blocks the main thread for ~250ms — long enough to drop frames
//! during scroll. Moving the work to a Worker keeps the UI thread at
//! 60fps regardless of file size.
//!
//! Architecture:
//! - The same wasm binary is loaded in both the main thread and the
//!   worker (see `main.rs`'s `is_worker_context` branch). `worker_main`
//!   sets up a `message` listener on the worker global scope.
//! - Main thread holds a single lazily-spawned `HighlightClient` keyed by
//!   a thread-local. `client::spawn_highlight` ships a request to the
//!   worker and registers a callback for incoming chunks.
//! - The worker streams `Chunk` responses (one per N lines) back to the
//!   main thread; the most recent task wins (older callbacks are
//!   discarded as soon as a new task starts).
//!
//! Cancellation model: a new request supersedes any in-flight one. The
//! worker checks the active task id at every chunk boundary and drops
//! processing if the id has changed. Mid-chunk preemption is impossible
//! (a Worker is single-threaded), so chunk size sets the worst-case
//! cancellation latency. We use 200 lines, matching the previous
//! main-thread chunk size.

#[cfg(target_arch = "wasm32")]
use serde::{Deserialize, Serialize};

#[cfg(target_arch = "wasm32")]
use crate::syntax_highlight::LineTokens;

/// Lines per chunk batched back to main. Smaller chunks mean each
/// arrives sooner and the visible viewport gets coloured earlier — the
/// "first chunk" perceived latency is roughly chunk_size × per-line
/// tokenize cost (~0.3ms in release, ~3ms in debug). 50 lines lands the
/// first chunk in ~15ms release / ~150ms debug, well under the
/// "uncoloured flash" threshold for a typical viewport.
///
/// Also sets cancellation latency: a superseding request waits for the
/// current chunk to finish before the worker checks the active task id.
#[cfg(target_arch = "wasm32")]
const CHUNK_SIZE: usize = 50;

/// Wire format for `main → worker`. Tagged enum keeps future variants
/// (e.g. unified-diff hunk highlighting) easy to add.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Tokenize a whole file. The worker processes it in `CHUNK_SIZE`
    /// slices and emits one `Chunk` per slice.
    HighlightFile {
        task_id: u64,
        syntax_name: String,
        theme_name: String,
        lines: Vec<String>,
    },
}

/// Wire format for `worker → main`. Streamed; one `Chunk` per slice
/// followed by exactly one `Done`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Chunk {
        task_id: u64,
        start: usize,
        tokens: Vec<LineTokens>,
    },
    Done {
        task_id: u64,
    },
}

#[cfg(target_arch = "wasm32")]
mod worker_impl {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;

    /// Entry point invoked by `syntax_worker.js` after wasm-init. Hooks a
    /// `message` listener on the worker global scope; never returns.
    pub fn worker_main() {
        let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();

        // The worker global is a `JsValue` we keep so the message
        // handler can `post_message` results back. Boxed so the closure
        // can move it.
        let scope_for_handler = scope.clone();

        // Latest task id seen. The handler drops a task as soon as a
        // newer task id arrives. Cell is fine — workers are
        // single-threaded.
        let active_task: Rc<Cell<u64>> = Rc::new(Cell::new(0));

        let handler = Closure::<dyn FnMut(web_sys::MessageEvent)>::new({
            let active_task = active_task.clone();
            move |evt: web_sys::MessageEvent| {
                let value = evt.data();
                let req: Request = match serde_wasm_bindgen::from_value(value) {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("[hl-worker] bad request: {e}");
                        return;
                    }
                };
                handle_request(req, &scope_for_handler, &active_task);
            }
        });

        // `set_onmessage` is more reliable here than
        // `addEventListener("message", …)` — some browsers route the
        // worker-message channel only through the property setter.
        scope.set_onmessage(Some(handler.as_ref().unchecked_ref()));
        // Forget so the closure outlives the function. The worker lives
        // for as long as the page; no cleanup needed.
        handler.forget();
    }

    fn handle_request(
        req: Request,
        scope: &web_sys::DedicatedWorkerGlobalScope,
        active_task: &Rc<Cell<u64>>,
    ) {
        match req {
            Request::HighlightFile {
                task_id,
                syntax_name,
                theme_name,
                lines,
            } => {
                active_task.set(task_id);

                // Apply theme inside the worker so the per-line
                // highlight call uses the right palette.
                if !theme_name.is_empty() {
                    crate::syntax_highlight::set_selected_theme(&theme_name);
                }

                let Some(syntax) = crate::syntax_highlight::syntax_for_lang_token(&syntax_name)
                    .or_else(|| crate::syntax_highlight::syntax_for_path(&syntax_name))
                else {
                    // Unknown syntax: just emit Done so the client
                    // detaches its callback.
                    post(scope, &Response::Done { task_id });
                    return;
                };

                let mut hl = crate::syntax_highlight::LineHighlighter::new(syntax);
                let mut start = 0usize;
                while start < lines.len() {
                    if active_task.get() != task_id {
                        // Newer task arrived between chunks; abandon.
                        return;
                    }
                    let end = (start + CHUNK_SIZE).min(lines.len());
                    let mut chunk: Vec<LineTokens> = Vec::with_capacity(end - start);
                    for line in &lines[start..end] {
                        chunk.push(hl.highlight_one(line));
                    }
                    post(
                        scope,
                        &Response::Chunk {
                            task_id,
                            start,
                            tokens: chunk,
                        },
                    );
                    start = end;
                }
                post(scope, &Response::Done { task_id });
            }
        }
    }

    fn post(scope: &web_sys::DedicatedWorkerGlobalScope, resp: &Response) {
        match serde_wasm_bindgen::to_value(resp) {
            Ok(v) => {
                if let Err(e) = scope.post_message(&v) {
                    log::error!("[hl-worker] post_message failed: {e:?}");
                }
            }
            Err(e) => log::error!("[hl-worker] serialize response: {e}"),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use worker_impl::worker_main;

/// Native stub so `cargo check` (which runs against the host target,
/// not wasm32) accepts `main.rs`'s call into this module. The real
/// implementation only exists in the wasm32 build path.
#[cfg(not(target_arch = "wasm32"))]
pub fn worker_main() {
    unreachable!("worker_main is wasm32-only")
}

// ── Main-thread client ──────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
pub mod client {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;

    thread_local! {
        /// Lazily-created singleton worker. `None` until the first
        /// highlight request, then held for the page lifetime.
        static SHARED: RefCell<Option<Rc<HighlightClient>>> = const { RefCell::new(None) };
    }

    /// Locate the wasm-bindgen JS + wasm URLs.
    ///
    /// Modern Trunk (≥ 0.18) emits an *inline* `<script type="module">`
    /// containing
    /// ```js
    /// import init from '/frontend-<hash>.js';
    /// const wasm = await init({ module_or_path: '/frontend-<hash>_bg.wasm' });
    /// ```
    /// and does NOT set a `src` attribute, so we parse the textContent
    /// to extract both URLs. Falls back to the older `src=` shape for
    /// robustness.
    ///
    /// Returns `None` if neither pattern matches, in which case the
    /// worker can't be spawned and the file view falls back to its
    /// in-process highlighter.
    fn discover_wasm_urls() -> Option<(String, String)> {
        let document = web_sys::window()?.document()?;
        let scripts = document.get_elements_by_tag_name("script");
        for i in 0..scripts.length() {
            let Some(node) = scripts.item(i) else {
                continue;
            };
            let Some(el) = node.dyn_ref::<web_sys::HtmlScriptElement>() else {
                continue;
            };

            // 1) External `src=…` (older Trunk, or hand-written shape).
            let src = el.src();
            if !src.is_empty() && src.ends_with(".js") && src.contains("frontend") {
                let wasm = src.replacen(".js", "_bg.wasm", 1);
                return Some((src, wasm));
            }

            // 2) Inline module script: parse `import … from '…'` and the
            // explicit `module_or_path: '…'` argument out of the body.
            if el.type_() == "module" {
                let text = el.text().unwrap_or_default();
                let js = extract_quoted_after(&text, "from")?;
                let wasm = extract_quoted_after(&text, "module_or_path:")
                    .unwrap_or_else(|| js.replacen(".js", "_bg.wasm", 1));
                if !js.is_empty() {
                    return Some((js, wasm));
                }
            }
        }
        None
    }

    /// Find the next single- or double-quoted string after `marker` in
    /// `text` and return its contents. Used to pull URL literals out of
    /// Trunk's inline boot module without pulling in a full JS parser.
    fn extract_quoted_after(text: &str, marker: &str) -> Option<String> {
        let after = text.split_once(marker)?.1;
        let q_pos = after.find(['\'', '"'])?;
        let quote = after.as_bytes()[q_pos] as char;
        let after_quote = &after[q_pos + 1..];
        let end = after_quote.find(quote)?;
        Some(after_quote[..end].to_owned())
    }

    fn build_worker_url() -> Option<String> {
        let (js, wasm) = discover_wasm_urls()?;
        Some(format!(
            "/syntax_worker.js?js={}&wasm={}",
            js_sys::encode_uri_component(&js),
            js_sys::encode_uri_component(&wasm),
        ))
    }

    type ChunkCb = Box<dyn FnMut(usize, Vec<LineTokens>)>;
    type DoneCb = Box<dyn FnOnce()>;

    struct Pending {
        on_chunk: ChunkCb,
        on_done: Option<DoneCb>,
    }

    pub struct HighlightClient {
        worker: web_sys::Worker,
        next_task_id: Cell<u64>,
        // Active task callbacks. Older entries are dropped on each new
        // spawn — only the most recent task fires its callbacks.
        active: Rc<RefCell<HashMap<u64, Pending>>>,
        // Closure pinned for the worker's `message` listener.
        _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    }

    /// Lazy accessor. First call instantiates the worker; subsequent
    /// calls reuse the same one. Returns `None` if the wasm URLs can't
    /// be discovered (offline-only fallback path) — caller treats that
    /// as "no highlighting".
    pub fn shared() -> Option<Rc<HighlightClient>> {
        SHARED.with(|cell| {
            if let Some(c) = cell.borrow().clone() {
                return Some(c);
            }
            let client = HighlightClient::new()?;
            let rc = Rc::new(client);
            *cell.borrow_mut() = Some(rc.clone());
            Some(rc)
        })
    }

    impl HighlightClient {
        fn new() -> Option<Self> {
            let url = build_worker_url()?;
            let opts = web_sys::WorkerOptions::new();
            opts.set_type(web_sys::WorkerType::Module);
            let worker = web_sys::Worker::new_with_options(&url, &opts)
                .map_err(|e| log::error!("Worker::new failed: {e:?}"))
                .ok()?;

            let active: Rc<RefCell<HashMap<u64, Pending>>> = Rc::new(RefCell::new(HashMap::new()));
            let active_for_handler = active.clone();
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
                move |evt: web_sys::MessageEvent| {
                    let value = evt.data();
                    let resp: Response = match serde_wasm_bindgen::from_value(value) {
                        Ok(r) => r,
                        Err(e) => {
                            log::error!("[hl-client] bad response: {e}");
                            return;
                        }
                    };
                    match resp {
                        Response::Chunk {
                            task_id,
                            start,
                            tokens,
                        } => {
                            // Look up by task id. If the task has been
                            // superseded the entry is gone and the
                            // chunk is silently dropped — exactly the
                            // cancellation behavior we want.
                            if let Some(pending) = active_for_handler.borrow_mut().get_mut(&task_id)
                            {
                                (pending.on_chunk)(start, tokens);
                            }
                        }
                        Response::Done { task_id } => {
                            let removed = active_for_handler.borrow_mut().remove(&task_id);
                            if let Some(mut pending) = removed
                                && let Some(done) = pending.on_done.take()
                            {
                                done();
                            }
                        }
                    }
                },
            );
            worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

            Some(Self {
                worker,
                next_task_id: Cell::new(1),
                active,
                _on_message: on_message,
            })
        }

        /// Ship a file's lines off to the worker for highlighting.
        /// `on_chunk` fires for each chunk of tokens (with the starting
        /// line index of that chunk). `on_done` fires once when
        /// processing finishes. Returns the task id; callers can
        /// implicitly cancel the previous task by spawning another (the
        /// previous one's callbacks are dropped).
        pub fn highlight_file(
            self: &Rc<Self>,
            syntax_name: String,
            theme_name: String,
            lines: Vec<String>,
            on_chunk: ChunkCb,
            on_done: DoneCb,
        ) -> u64 {
            let task_id = self.next_task_id.get();
            self.next_task_id.set(task_id + 1);

            // Drop callbacks for any prior in-flight tasks before
            // recording the new one. This is the cancellation hook on
            // the client side; the worker also checks the active task
            // id at chunk boundaries.
            self.active.borrow_mut().clear();
            self.active.borrow_mut().insert(
                task_id,
                Pending {
                    on_chunk,
                    on_done: Some(on_done),
                },
            );

            let req = Request::HighlightFile {
                task_id,
                syntax_name,
                theme_name,
                lines,
            };
            match serde_wasm_bindgen::to_value(&req) {
                Ok(v) => {
                    if let Err(e) = self.worker.post_message(&v) {
                        log::error!("[hl-client] post_message failed: {e:?}");
                        // Roll back the registration so the slot
                        // doesn't leak forever.
                        self.active.borrow_mut().remove(&task_id);
                    }
                }
                Err(e) => {
                    log::error!("[hl-client] serialize request: {e}");
                    self.active.borrow_mut().remove(&task_id);
                }
            }
            task_id
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use client::shared;

// ── Native (`cargo check` host target) stubs ────────────────────────────
//
// The frontend crate is wasm-only at runtime; these stubs only exist so
// `cargo check` (no `--target wasm32-…`) accepts the call sites in
// `file_view.rs`. The bodies are unreachable in any real build.

#[cfg(not(target_arch = "wasm32"))]
pub struct HighlightClient;

#[cfg(not(target_arch = "wasm32"))]
impl HighlightClient {
    pub fn highlight_file(
        self: &std::rc::Rc<Self>,
        _path: String,
        _theme_name: String,
        _lines: Vec<String>,
        _on_chunk: Box<dyn FnMut(usize, Vec<crate::syntax_highlight::LineTokens>)>,
        _on_done: Box<dyn FnOnce()>,
    ) -> u64 {
        unreachable!("HighlightClient::highlight_file is wasm32-only")
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn shared() -> Option<std::rc::Rc<HighlightClient>> {
    None
}
