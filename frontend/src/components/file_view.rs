use std::sync::{Arc, Mutex};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::components::find_bar::{FindBar, FindState};
use crate::state::{AppState, TabContent, TabId, TabScrollState};
use crate::syntax_highlight::{LineHighlighter, LineTokens, color_to_css, syntax_for_path};

use protocol::ProjectPath;

/// Below this line count we render every line up-front — no spacers, no
/// scroll math. This keeps the small-file path identical in DOM shape to
/// the pre-virtualization implementation, so existing layout assertions
/// (`renders_lines_single_spaced`) survive unchanged. Above this threshold
/// we switch on viewport windowing.
const VIRTUALIZE_THRESHOLD: usize = 200;

/// Number of buffer lines to render outside the visible viewport on each
/// side, smoothing scroll without rendering the whole file.
const OVERSCAN_LINES: f64 = 50.0;

/// Initial estimate for a single rendered line's height in pixels. Used
/// before the first paint completes and we can measure the real value via
/// `offset_height`. Picking a non-zero default lets virtualization engage
/// on the very first render of a large file rather than rendering every
/// line once and then narrowing — a typical monospace line at the default
/// font size is ~16-20px.
const INITIAL_LINE_HEIGHT_ESTIMATE: f64 = 18.0;

/// Initial estimate for the viewport height before measurement. Combined
/// with `INITIAL_LINE_HEIGHT_ESTIMATE` and `OVERSCAN_LINES` it bounds the
/// first-paint render to ~80-100 lines for any file size.
const INITIAL_VIEWPORT_HEIGHT_ESTIMATE: f64 = 600.0;

/// In-process fallback chunk size when the highlight Web Worker can't be
/// instantiated — typically only in `wasm-bindgen-test` runs (no Trunk
/// bundle in the page) but also covers worker init failures in
/// production.
const FALLBACK_CHUNK_LINES: usize = 200;

/// Above this line count we never highlight — the wasm main thread can't
/// realistically tokenize that much without freezing the UI even with
/// chunking. Mirrors `syntax_highlight::MAX_LINES_TO_HIGHLIGHT`.
const HIGHLIGHT_LINE_CAP: usize = 5000;

fn cancel_highlight_task(active_task_id: &Arc<Mutex<Option<u64>>>) {
    let Some(task_id) = active_task_id
        .lock()
        .expect("highlight task mutex poisoned")
        .take()
    else {
        return;
    };
    if let Some(client) = crate::highlight_worker::shared() {
        client.cancel_task(task_id);
    }
}

fn tab_scroll_state_from_element(el: &web_sys::Element) -> TabScrollState {
    TabScrollState {
        scroll_top: el.scroll_top(),
        scroll_height: el.scroll_height(),
        client_height: el.client_height(),
        user_scrolled_up: true,
    }
}

/// Outer `FileView` is intentionally thin: it tracks only whether the file
/// has been loaded into `open_files` (cheap `bool` Memo) and mounts the
/// heavy inner `FileViewLoaded` exactly once via `Show`. Without this
/// split, the previous `move ||` body re-ran on every `open_files`
/// change — opening a different file in another tab re-built `FileLines`,
/// re-created the highlight `Effect`, and re-spawned the syntect task in
/// every already-open tab. With many tabs that compounds quickly and is
/// the main "feels sluggish" symptom.
#[component]
pub fn FileView(tab_id: TabId, path: ProjectPath) -> impl IntoView {
    let state = expect_context::<AppState>();
    let file_path = path.clone();
    let is_loaded = Memo::new(move |_| {
        state
            .open_files
            .with(|files| files.contains_key(&file_path))
    });

    let path_for_loaded = path.clone();
    view! {
        <div class="file-view">
            <Show
                when=move || is_loaded.get()
                fallback=move || view! { <div class="panel-empty">"No file open"</div> }
            >
                <FileViewLoaded tab_id=tab_id path=path_for_loaded.clone() />
            </Show>
        </div>
    }
}

/// Per-tab file body. All heavy setup (line table, find state,
/// virtualization signals, async syntect task) runs **once** at mount —
/// never repeats when other files open or close. Reads contents
/// untracked from `open_files`; the file is guaranteed present because
/// the parent `Show` only mounts this when `open_files` already contains
/// the path.
#[component]
fn FileViewLoaded(tab_id: TabId, path: ProjectPath) -> impl IntoView {
    let state = expect_context::<AppState>();
    let initial_scroll_state = state.tab_scroll_state_untracked(tab_id);

    let f = state
        .open_files
        .with_untracked(|files| files.get(&path).cloned())
        .expect("FileViewLoaded mounted with no open_files entry");

    let close_path = path.clone();
    let path_display = format!("{}/{}", f.path.root.0, f.path.relative_path);
    let content = if f.is_binary {
        "(binary file)".to_owned()
    } else {
        f.contents.unwrap_or_else(|| "(file not found)".to_owned())
    };

    let on_close = move |_| {
        let state = expect_context::<AppState>();
        let tab_id = state.center_zone.with_untracked(|cz| {
            cz.find_tab(&TabContent::File {
                path: close_path.clone(),
            })
        });
        if let Some(id) = tab_id {
            state.close_tab(id);
        }
    };

    // Hold the entire file content as a single
    // `Arc<str>` plus a per-line byte-offset table.
    // This is critical for huge files: the previous
    // `content.lines().map(|l| l.to_owned()).collect()`
    // allocated one `String` per line (50 000 allocs
    // for a 50K-line file), which takes seconds in
    // debug-build wasm. `FileLines::new` does two
    // allocations total, regardless of line count.
    let perf_key = format!("file:{}", f.path.relative_path);
    crate::perf::log_phase("file_open", "mount", &perf_key, "");
    let lines_t0 = crate::perf::now_ms();
    let (find_state, lines) = FindState::from_file(&content);
    let total = lines.len();
    let lines_dt = crate::perf::now_ms() - lines_t0;
    crate::perf::log_phase(
        "file_open",
        "lines_built",
        &perf_key,
        &format!(" lines={total} took={lines_dt:.1}ms"),
    );
    provide_context(find_state.clone());

    // Async syntax highlighting. We do *zero* sync syntect work on
    // mount — even tokenizing 40 lines of Rust costs ~700ms in
    // debug-build wasm on a cold syntect (first time the language is
    // touched in the session pays onig regex compile costs the
    // pre-emptive `warm_up()` only partially amortizes). Plain-text
    // first paint at <50ms beats colored first paint at >700ms.
    //
    // Tokens stream in via the spawn_local task / worker below: each
    // row reads its own index from `highlighted` reactively, so visible
    // lines render plain text immediately and "fill in" with color
    // over the next ~hundred ms as chunks land.
    //
    // Persistent syntect parser state across chunks is critical: it's
    // how multi-line constructs (block comments, raw strings) still
    // color correctly even though we're processing the file in pieces.
    let initial_tokens: Vec<Option<LineTokens>> = vec![None; total];
    let highlighted: ArcRwSignal<Vec<Option<LineTokens>>> = ArcRwSignal::new(initial_tokens);

    // Generation counter for live re-highlighting on
    // theme change. Bumping this invalidates any
    // in-flight chunked task, which checks the
    // generation each chunk and exits if stale.
    let highlight_gen: ArcRwSignal<u32> = ArcRwSignal::new(0);
    let active_worker_task: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let active_worker_task_for_cleanup = active_worker_task.clone();
    on_cleanup(move || cancel_highlight_task(&active_worker_task_for_cleanup));

    let path_for_effect = f.path.relative_path.clone();
    let syntax_theme = state.syntax_theme;
    let lines_for_effect = lines.clone();
    let highlighted_for_effect = highlighted.clone();
    let gen_for_effect = highlight_gen.clone();
    let active_worker_task_for_effect = active_worker_task.clone();
    Effect::new(move |_| {
        // Subscribe to the theme signal so a theme change drops the
        // current tokens and dispatches a fresh request with the new
        // theme name.
        let theme_name = syntax_theme.get();

        let my_gen = gen_for_effect.get_untracked() + 1;
        gen_for_effect.set(my_gen);
        cancel_highlight_task(&active_worker_task_for_effect);

        // Reset highlighted vec to plain text while we re-tokenize.
        // Visible rows momentarily render plain, then fill in with the
        // new theme as chunks stream in from the worker.
        highlighted_for_effect.update(|v| {
            for slot in v.iter_mut() {
                *slot = None;
            }
        });

        if total == 0 || total > HIGHLIGHT_LINE_CAP {
            return;
        }
        let Some(syntax) = syntax_for_path(&path_for_effect) else {
            return;
        };

        // Prefer the worker. If `shared()` returns `None` we're in an
        // environment without the Trunk-emitted bootstrap script
        // (wasm-bindgen-test, e.g.) — fall back to the previous
        // main-thread chunked path so behaviour is preserved for
        // tests and any worker init failure.
        let Some(client) = crate::highlight_worker::shared() else {
            run_fallback_highlight(
                syntax,
                lines_for_effect.clone(),
                highlighted_for_effect.clone(),
                gen_for_effect.clone(),
                my_gen,
                format!("file:{}", path_for_effect),
            );
            return;
        };

        // Snapshot the lines as `Vec<String>` for the worker. We pay
        // one alloc per line on the main thread, then the worker
        // structured-clones them across the postMessage boundary.
        // Cheap enough (~few ms) for files within the 5K-line cap.
        let lines_owned: Vec<String> = (0..lines_for_effect.len())
            .map(|i| lines_for_effect.line(i).to_owned())
            .collect();

        let signal_for_task = highlighted_for_effect.clone();
        let gen_for_task = gen_for_effect.clone();
        let perf_key_for_task = format!("file:{}", path_for_effect);
        let perf_key_for_done = perf_key_for_task.clone();
        let task_t0 = crate::perf::now_ms();
        let first_chunk_logged = std::rc::Rc::new(std::cell::Cell::new(false));
        let first_chunk_logged_for_done = first_chunk_logged.clone();
        crate::perf::log_phase(
            "file_open",
            "hl_started",
            &perf_key_for_task,
            &format!(" lines={total}"),
        );

        let on_chunk = {
            let gen_for_task = gen_for_task.clone();
            Box::new(move |start: usize, tokens: Vec<LineTokens>| {
                // The view cancels its prior worker task before dispatching
                // a new one, but defend in depth — a stale chunk that races
                // a generation bump shouldn't write tokens for a torn-down
                // highlight pass.
                if gen_for_task.get_untracked() != my_gen {
                    return;
                }
                if !first_chunk_logged.get() {
                    first_chunk_logged.set(true);
                    let dt = crate::perf::now_ms() - task_t0;
                    crate::perf::log_phase(
                        "file_open",
                        "hl_first_chunk",
                        &perf_key_for_task,
                        &format!(" through={} took={dt:.1}ms", start + tokens.len()),
                    );
                }
                signal_for_task.update(|v| {
                    for (offset, toks) in tokens.into_iter().enumerate() {
                        if let Some(slot) = v.get_mut(start + offset) {
                            *slot = Some(toks);
                        }
                    }
                });
            })
        };
        let on_done = Box::new(move || {
            // `first_chunk_logged` clone is only present so we don't
            // double-log a "finished" before "first_chunk" fired in
            // the empty-tokens edge case.
            let _ = &first_chunk_logged_for_done;
            let dt = crate::perf::now_ms() - task_t0;
            crate::perf::log_phase(
                "file_open",
                "hl_finished",
                &perf_key_for_done,
                &format!(" took={dt:.1}ms"),
            );
        });

        let task_id = client.highlight_file_concurrent(
            path_for_effect.clone(),
            theme_name,
            lines_owned,
            on_chunk,
            on_done,
        );
        *active_worker_task_for_effect
            .lock()
            .expect("highlight task mutex poisoned") = Some(task_id);
    });

    let pre_ref: NodeRef<leptos::html::Pre> = NodeRef::new();

    // ── Open-at-line (from project search) ─────────────────────────────
    // Resolve a pending goto for THIS file *synchronously*, before the virtual
    // window is first computed, so a deep target line lands in the very first
    // rendered window (no top-then-jump flash). Cleared immediately so it
    // fires once. `pending_line` then drives the measured re-snap below.
    let goto_path = f.path.clone();
    let initial_goto: Option<u32> = state.pending_goto_line.with_untracked(|pending| {
        pending
            .as_ref()
            .and_then(|(path, line)| (*path == goto_path).then_some(*line))
    });
    if initial_goto.is_some() {
        state.pending_goto_line.set(None);
    }

    // Virtualization geometry. Pre-seed the line and
    // viewport heights with reasonable estimates so the
    // very first render of a large file already uses a
    // bounded window. The measurement Effect below
    // refines both values once layout is real. When a goto is pending we seed
    // the scroll to the target line (using the estimate) so the first virtual
    // window already contains it; otherwise we restore any saved scroll.
    let initial_scroll_top = match initial_goto {
        Some(line) => (line.saturating_sub(1) as f64) * INITIAL_LINE_HEIGHT_ESTIMATE,
        None => initial_scroll_state.map_or(0.0_f64, |scroll| scroll.scroll_top as f64),
    };
    let scroll_top = RwSignal::new(initial_scroll_top);
    let viewport_height = RwSignal::new(INITIAL_VIEWPORT_HEIGHT_ESTIMATE);
    let line_height = RwSignal::new(INITIAL_LINE_HEIGHT_ESTIMATE);
    // Set true once the real line height has been measured (drives the
    // one-shot consumption of a goto request — see below).
    let geometry_measured = RwSignal::new(false);

    // Skip the saved-scroll restore when a goto already seeded the scroll.
    let restored_initial_scroll =
        std::rc::Rc::new(std::cell::Cell::new(initial_goto.is_some()));
    let restored_initial_scroll_for_effect = restored_initial_scroll.clone();
    let pre_ref_for_restore = pre_ref;
    let state_for_restore = state.clone();
    Effect::new(move |_| {
        if restored_initial_scroll_for_effect.get() {
            return;
        }
        let Some(saved) = initial_scroll_state else {
            return;
        };
        let Some(el) = pre_ref_for_restore.get() else {
            return;
        };
        restored_initial_scroll_for_effect.set(true);
        el.set_scroll_top(saved.scroll_top);
        scroll_top.set(el.scroll_top() as f64);
        let element: web_sys::Element = el.clone().unchecked_into();
        state_for_restore.save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
    });

    // Measure the geometry once after first paint. The
    // Effect re-runs if the underlying signals fire
    // (rare here — only the initial mount).
    let perf_key_for_measure = format!("file:{}", f.path.relative_path);
    let measure_logged = std::rc::Rc::new(std::cell::Cell::new(false));
    Effect::new(move |_| {
        let Some(el) = pre_ref.get() else { return };
        let vh = el.client_height() as f64;
        if vh > 0.0 {
            viewport_height.set(vh);
        }
        if let Ok(Some(line_el)) = el.query_selector(".file-line")
            && let Some(html_el) = line_el.dyn_ref::<web_sys::HtmlElement>()
        {
            let lh = html_el.offset_height() as f64;
            if lh > 0.0 && (line_height.get_untracked() - lh).abs() > 0.5 {
                line_height.set(lh);
            }
            if !measure_logged.get() {
                measure_logged.set(true);
                // Signal the goto re-snap Effect that the real line height is
                // now known so it can correct + consume the request.
                geometry_measured.set(true);
                crate::perf::log_phase(
                    "file_open",
                    "first_paint_measured",
                    &perf_key_for_measure,
                    &format!(" viewport_h={vh:.0} line_h={lh:.1}"),
                );
            }
        }
    });

    // `pending_line` holds the 1-based target line for THIS file. It is seeded
    // synchronously above (`initial_goto`) for the freshly-opened case, and by
    // the bridge Effect below for an already-open tab being re-targeted.
    let pending_line: RwSignal<Option<u32>> = RwSignal::new(initial_goto);

    // Bridge later global goto requests to this file (already-open tab case).
    // The freshly-opened case was handled by the synchronous seed above.
    let goto_path_for_bridge = f.path.clone();
    let state_for_goto = state.clone();
    Effect::new(move |_| {
        if let Some((target_path, line)) = state_for_goto.pending_goto_line.get()
            && target_path == goto_path_for_bridge
        {
            pending_line.set(Some(line));
            state_for_goto.pending_goto_line.set(None);
        }
    });

    // Apply / re-snap the scroll. Subscribes to `pending_line`, `line_height`,
    // `geometry_measured`, and the element ref: it aligns first with the
    // estimate, then re-snaps once the real line height is measured and
    // *consumes* the request (clears `pending_line`) so later geometry changes
    // never yank the user back to an old result. It does NOT read `scroll_top`,
    // so the user can freely scroll afterwards without this Effect fighting them.
    let pre_ref_for_goto = pre_ref;
    let state_for_goto_scroll = state.clone();
    Effect::new(move |_| {
        let Some(line) = pending_line.get() else {
            return;
        };
        let lh = line_height.get();
        let measured = geometry_measured.get();
        let Some(el) = pre_ref_for_goto.get() else {
            return;
        };
        let target = (line.saturating_sub(1) as f64) * lh;
        el.set_scroll_top(target as i32);
        scroll_top.set(el.scroll_top() as f64);
        let element: web_sys::Element = el.clone().unchecked_into();
        state_for_goto_scroll.save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
        if measured {
            // Real geometry applied — consume the request so it fires once.
            pending_line.set(None);
        }
    });

    // Write the native scrollTop straight into the signal. Leptos
    // batches reactive updates within the same task, so a burst of
    // native scroll events still only re-renders the visible window
    // once per microtask. (We previously throttled with rAF, but the
    // rAF callback fires unreliably in Tauri's WKWebView and that
    // pinned the visible window to its initial range.)
    let state_for_scroll = state.clone();
    let on_scroll = move |_: web_sys::Event| {
        if let Some(el) = pre_ref.get_untracked() {
            scroll_top.set(el.scroll_top() as f64);
            let element: web_sys::Element = el.clone().unchecked_into();
            state_for_scroll.save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
        }
    };

    // Visible window in line-index space. Small files
    // render everything (start=0, end=total) so spacers
    // stay at 0px and the pre-virtualization DOM shape
    // is preserved. Larger files use the seeded
    // (then measured) line_height to bound the window
    // from the very first render.
    let visible_window: Memo<(usize, usize)> = Memo::new(move |_| {
        if total < VIRTUALIZE_THRESHOLD {
            return (0, total);
        }
        let lh = line_height.get();
        let st = scroll_top.get();
        let vh = viewport_height.get();
        let start_f = ((st - OVERSCAN_LINES * lh) / lh).floor().max(0.0);
        let end_f = ((st + vh + OVERSCAN_LINES * lh) / lh)
            .ceil()
            .min(total as f64);
        (start_f as usize, end_f as usize)
    });

    let find_bar_open = state.find_bar_open;

    let lines_for_render = lines.clone();
    let highlighted_for_render = highlighted.clone();
    let find_for_render = find_state.clone();

    view! {
                            <div class="file-view-header">
                                <span class="file-view-path">{path_display}</span>
                                <button class="file-view-close" on:click=on_close title="Close">"×"</button>
                            </div>
                            {move || {
                                if find_bar_open.get() {
                                    Some(view! { <FindBar /> })
                                } else {
                                    None
                                }
                            }}
                            <pre
                                class="file-view-content"
                                node_ref=pre_ref
                                on:scroll=on_scroll
                            >
                                // Spacers are siblings of `<For>`, not inside the
                                // same reactive closure — otherwise the outer
                                // closure rerunning would tear down and recreate
                                // the `<For>` itself, defeating keyed DOM
                                // preservation.
                                {move || {
                                    let (start, _) = visible_window.get();
                                    let h = start as f64 * line_height.get();
                                    (h > 0.0).then(|| view! {
                                        <div
                                            class="file-view-spacer"
                                            style=format!("height: {h}px;")
                                        ></div>
                                    })
                                }}
                                <For
                                    each=move || {
                                        let (s, e) = visible_window.get();
                                        (s..e).collect::<Vec<usize>>()
                                    }
                                    key=|i| *i
                                    let:i
                                >
                                    {
                                        let text = lines_for_render.line(i).to_owned();
                                        let highlighted_for_row = highlighted_for_render.clone();
                                        let find = find_for_render.clone();
                                        view! {
                                            <div
                                                class=move || file_line_class(i, &find)
                                                data-find-idx=i
                                            >
                                                <span
                                                    class="file-line-num"
                                                    data-line-num={(i + 1).to_string()}
                                                ></span>
                                                {move || {
                                                    // Reactive read: when a
                                                    // chunk lands and updates
                                                    // the signal, this closure
                                                    // re-runs and the row
                                                    // swaps from plain text to
                                                    // colored spans.
                                                    let tokens = highlighted_for_row
                                                        .with(|v| v.get(i).and_then(|t| t.clone()));
                                                    render_file_line_content(text.clone(), tokens)
                                                }}
                                            </div>
                                        }
                                    }
                                </For>
                                {move || {
                                    let (_, end) = visible_window.get();
                                    let h = total.saturating_sub(end) as f64
                                        * line_height.get();
                                    (h > 0.0).then(|| view! {
                                        <div
                                            class="file-view-spacer"
                                            style=format!("height: {h}px;")
                                        ></div>
                                    })
                                }}
                            </pre>
    }
}

/// Yield to the browser between fallback-highlight chunks so the UI
/// doesn't freeze on large files when the worker isn't available.
async fn yield_to_browser() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// First-chunk size for the fallback path. Intentionally smaller than
/// `FALLBACK_CHUNK_LINES` so colored text appears in the visible
/// viewport quickly even on cold syntect (where each line costs
/// ~10-20ms in debug builds — a 200-line first chunk would block the
/// main thread for several seconds). After the first chunk lands the
/// regex/parser caches are warm and full-size chunks are cheap.
const FALLBACK_FIRST_CHUNK_LINES: usize = 40;

/// Main-thread fallback used when the highlight worker can't be
/// instantiated. Mirrors the pre-worker behavior: spawn_local + chunked
/// LineHighlighter with a per-chunk yield. Same generation-counter
/// cancellation as the worker path.
fn run_fallback_highlight(
    syntax: &'static syntect::parsing::SyntaxReference,
    lines: crate::line_source::FileLines,
    highlighted: ArcRwSignal<Vec<Option<LineTokens>>>,
    generation: ArcRwSignal<u32>,
    my_gen: u32,
    perf_key: String,
) {
    spawn_local(async move {
        // Yield once before any syntect work so the parent component
        // can paint its plain-text DOM. Without this the spawn_local
        // body runs in the same microtask burst as the mount, and the
        // first chunk's regex-compile cost (~hundreds of ms on cold
        // syntect) shows up as click latency.
        yield_to_browser().await;
        if generation.get_untracked() != my_gen {
            return;
        }
        let task_t0 = crate::perf::now_ms();
        crate::perf::log_phase(
            "file_open",
            "hl_started",
            &perf_key,
            &format!(" lines={} via=fallback", lines.len()),
        );
        let mut hl = LineHighlighter::new(syntax);
        let mut i = 0usize;
        let mut chunks = 0u32;
        let mut first_chunk_logged = false;
        let mut chunk_size = FALLBACK_FIRST_CHUNK_LINES;
        while i < lines.len() {
            if generation.get_untracked() != my_gen {
                return;
            }
            let end = (i + chunk_size).min(lines.len());
            let chunk_tokens: Vec<LineTokens> =
                (i..end).map(|j| hl.highlight_one(lines.line(j))).collect();
            if generation.get_untracked() != my_gen {
                return;
            }
            highlighted.update(|v| {
                for (offset, toks) in chunk_tokens.into_iter().enumerate() {
                    if let Some(slot) = v.get_mut(i + offset) {
                        *slot = Some(toks);
                    }
                }
            });
            i = end;
            chunks += 1;
            if !first_chunk_logged {
                let dt = crate::perf::now_ms() - task_t0;
                crate::perf::log_phase(
                    "file_open",
                    "hl_first_chunk",
                    &perf_key,
                    &format!(" through={i} took={dt:.1}ms via=fallback"),
                );
                first_chunk_logged = true;
            }
            chunk_size = FALLBACK_CHUNK_LINES;
            yield_to_browser().await;
        }
        let dt = crate::perf::now_ms() - task_t0;
        crate::perf::log_phase(
            "file_open",
            "hl_finished",
            &perf_key,
            &format!(" chunks={chunks} took={dt:.1}ms via=fallback"),
        );
    });
}

fn file_line_class(line_idx: usize, find: &FindState) -> &'static str {
    let results = find.results.get();
    if !results.match_set.contains(&line_idx) {
        return "file-line";
    }
    let active = find.active_index.get();
    if active >= 0 && results.match_lines.get(active as usize) == Some(&line_idx) {
        "file-line find-hit-active"
    } else {
        "file-line find-hit"
    }
}

/// Render a file line's text inside the row. Emits a `<code>` element so
/// monospace/whitespace styling stays scoped, with either pre-tokenized
/// colored spans or a single plain-text node when no tokens are available
/// (unknown language, or file over the highlight cap).
fn render_file_line_content(text: String, tokens: Option<LineTokens>) -> AnyView {
    match tokens {
        Some(toks) if !toks.is_empty() => {
            let spans: Vec<AnyView> = toks
                .into_iter()
                .map(|t| {
                    let style = format!("color:{}", color_to_css(t.fg));
                    view! { <span style=style>{t.text}</span> }.into_any()
                })
                .collect();
            view! { <code class="file-line-code">{spans}</code> }.into_any()
        }
        _ => view! { <code class="file-line-code">{text}</code> }.into_any(),
    }
}

/// Render-layer tests for `FileView`.
///
/// Asserts on what the user perceives: line count, visible text, and geometry
/// (the gap between consecutive rendered lines vs. the height of a single row).
/// Avoid asserting on internal class names or DOM structure so the tests
/// survive refactors of the component as long as the rendered output stays
/// correct.
///
/// Run with: `tools/run-wasm-tests.sh wasm_tests::` (the script handles
/// chromedriver and `wasm-bindgen-cli` setup automatically — see CLAUDE.md).
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, OpenFile};
    use leptos::mount::mount_to;
    use protocol::{ProjectPath, ProjectRootPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    /// Inject the production stylesheet once per test session so layout
    /// assertions reflect real styling.
    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document.get_element_by_id("test-prod-styles-app").is_none() {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-app");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    /// Create a fresh, sized container appended to the document body so child
    /// elements have a real layout box. Returns the container as an
    /// `HtmlElement` ready to pass to `mount_to`.
    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        // Give the container a deterministic size so flex children lay out
        // predictably regardless of the headless browser viewport.
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 800px; height: 600px; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn line_rows(container: &HtmlElement) -> Vec<Element> {
        // Query for rendered rows by structural pattern: the direct children of
        // the file-content `<pre>`. Using a structural query (rather than a
        // specific class on each row) keeps the test resilient to row-level
        // class renames as long as one row per source line is rendered.
        let nodes = container
            .query_selector_all(".file-view-content > *")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
            .collect()
    }

    /// Count rendered line rows, ignoring virtualization spacers. Spacers
    /// have the `file-view-spacer` class and are only present for files
    /// large enough to engage windowing.
    fn rendered_line_count(container: &HtmlElement) -> usize {
        container
            .query_selector_all(".file-view-content > .file-line")
            .unwrap()
            .length() as usize
    }

    /// Yield to the browser event loop so reactive effects flush and the DOM
    /// reflects the rendered view before we assert on it.
    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    #[wasm_bindgen_test]
    async fn renders_lines_single_spaced() {
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "hello.rs".to_owned(),
        };
        let content = "line one\nline two\nline three\nline four\nline five";

        let container = make_container();
        let mount_path = path.clone();
        // Create the AppState inside the mount closure so its signals belong to
        // the mount's reactive Owner and the provided-context lookup resolves
        // them correctly.
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_001) path=mount_path.clone() /> }
        });

        next_tick().await;

        let rows = line_rows(&container);
        assert_eq!(
            rows.len(),
            5,
            "expected one rendered row per source line, got {}",
            rows.len()
        );

        // Geometry assertion — the heart of the test.
        let row0 = rows[0].get_bounding_client_rect();
        let row1 = rows[1].get_bounding_client_rect();
        let row4 = rows[4].get_bounding_client_rect();

        let row_height = row0.height();
        let gap = row1.top() - row0.top();
        let total = row4.top() - row0.top();

        assert!(row_height > 0.0, "row0 has no height — layout failed");

        // Geometry guard: the gap between consecutive rows should be ~one row
        // tall. Catches CSS regressions (line-height, padding) that would
        // visually space rows apart.
        let ratio = gap / row_height;
        assert!(
            (0.95..=1.10).contains(&ratio),
            "lines are not single-spaced: gap={gap:.2}px, row_height={row_height:.2}px, \
             ratio={ratio:.2}"
        );

        // Spacing should be uniform across all rows, not just the first pair.
        let expected_total = gap * 4.0;
        assert!(
            (total - expected_total).abs() < row_height * 0.5,
            "row spacing is not uniform: total={total:.2}px, expected≈{expected_total:.2}px"
        );

        // Text guard: each row's rendered text content must equal the
        // source line exactly — no leading line number, no stray
        // characters. Two concerns ride on this assertion:
        //
        // 1. The original double-spacing regression: stray characters
        //    (e.g. trailing "\n") leaking into the rendered output.
        // 2. Gutter line numbers must NOT be part of the row's text
        //    content. `text_content()` is the same surface the browser
        //    uses for copy operations, so any line number appearing here
        //    would also appear in the user's clipboard when they drag
        //    over a row — the bug this design avoids by rendering line
        //    numbers via a CSS pseudo-element with `content: attr(...)`.
        let expected = [
            "line one",
            "line two",
            "line three",
            "line four",
            "line five",
        ];
        for (i, row) in rows.iter().enumerate() {
            let text = row.text_content().unwrap_or_default();
            assert_eq!(
                text, expected[i],
                "row {i} rendered text must equal source line exactly \
                 (no line number, no stray characters)"
            );
            let line_num = (i + 1).to_string();
            assert!(
                !text.contains(&line_num),
                "row {i} text must not contain the gutter line number {line_num:?}; \
                 the line number lives in a CSS pseudo-element so it stays out of \
                 selection / copy: text was {text:?}"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn syntax_highlighted_line_renders_styled_spans() {
        // FileView with a Rust file should produce per-token styled spans,
        // sourced from syntect rather than runtime DOM mutation. Asserts on
        // visible rendering: at least one inline `style="color:..."` exists,
        // and the line's text content reconstructs the source exactly.
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        let content = "fn main() {}";

        let container = make_container();
        let mount_path = path.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_002) path=mount_path.clone() /> }
        });
        // The fallback highlighter yields once before doing any syntect
        // work (so the parent component can paint plain text first), so
        // we need at least two ticks for the first chunk to land. Loop
        // a few extra times in case the chunk crosses additional
        // macrotask boundaries on slower CI runners.
        let mut nodes = container
            .query_selector_all(".file-line code span[style]")
            .unwrap();
        for _ in 0..20 {
            if nodes.length() > 0 {
                break;
            }
            next_tick().await;
            nodes = container
                .query_selector_all(".file-line code span[style]")
                .unwrap();
        }
        assert!(
            nodes.length() > 0,
            "expected at least one styled span in the rendered file line"
        );
        let mut found_color = false;
        for i in 0..nodes.length() {
            if let Some(n) = nodes.item(i) {
                let el: Element = n.dyn_into().unwrap();
                if el
                    .get_attribute("style")
                    .unwrap_or_default()
                    .contains("color:")
                {
                    found_color = true;
                    break;
                }
            }
        }
        assert!(found_color, "no span had a `color:` style");

        let code_text = container
            .query_selector(".file-line code")
            .unwrap()
            .expect("file-line code element present")
            .text_content()
            .unwrap_or_default();
        assert_eq!(code_text, content);
    }

    #[wasm_bindgen_test]
    async fn multiple_mounted_files_keep_syntax_highlighting() {
        ensure_styles_loaded();

        let first_path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "first.rs".to_owned(),
        };
        let second_path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "second.rs".to_owned(),
        };

        let container = make_container();
        let mount_first_path = first_path.clone();
        let mount_second_path = second_path.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.open_files.update(|files| {
                files.insert(
                    mount_first_path.clone(),
                    OpenFile {
                        path: mount_first_path.clone(),
                        contents: Some("fn first() {}".to_owned()),
                        is_binary: false,
                    },
                );
                files.insert(
                    mount_second_path.clone(),
                    OpenFile {
                        path: mount_second_path.clone(),
                        contents: Some("fn second() {}".to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! {
                <div id="file-one">
                    <FileView tab_id=TabId(20_003) path=mount_first_path.clone() />
                </div>
                <div id="file-two">
                    <FileView tab_id=TabId(20_004) path=mount_second_path.clone() />
                </div>
            }
        });

        let selectors = [
            "#file-one .file-line code span[style]",
            "#file-two .file-line code span[style]",
        ];
        for _ in 0..20 {
            let both_highlighted = selectors
                .iter()
                .all(|selector| container.query_selector_all(selector).unwrap().length() > 0);
            if both_highlighted {
                break;
            }
            next_tick().await;
        }

        for selector in selectors {
            assert!(
                container.query_selector_all(selector).unwrap().length() > 0,
                "expected styled syntax spans for {selector}"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn large_file_only_renders_visible_window() {
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "big.txt".to_owned(),
        };
        // 5000 lines — comfortably above VIRTUALIZE_THRESHOLD (200).
        let total_lines = 5000;
        let content: String = (0..total_lines)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let container = make_container();
        let mount_path = path.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_003) path=mount_path.clone() /> }
        });

        // Virtualization must engage on the very first paint — pre-seeded
        // line/viewport height estimates bound the window before any
        // measurement Effect runs.
        next_tick().await;

        let rendered_first_paint = rendered_line_count(&container);
        assert!(
            rendered_first_paint > 0,
            "expected some lines to render on first paint, got {rendered_first_paint}"
        );
        assert!(
            rendered_first_paint < total_lines / 4,
            "virtualization did not engage on first paint: rendered={rendered_first_paint} \
             out of {total_lines} total lines"
        );

        // Subsequent ticks let the measurement Effect refine the geometry;
        // the rendered count should stay bounded.
        next_tick().await;
        let rendered = rendered_line_count(&container);
        assert!(
            rendered < total_lines / 4,
            "virtualization regressed after measurement: rendered={rendered} \
             out of {total_lines} total lines"
        );

        // The two spacer divs preserve scrollbar geometry: the `<pre>`'s
        // total scrollable height should be roughly total_lines * line_height.
        let pre = container
            .query_selector(".file-view-content")
            .unwrap()
            .expect("pre present")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let row = container
            .query_selector(".file-view-content > .file-line")
            .unwrap()
            .expect("at least one rendered line")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let row_height = row.offset_height() as f64;
        let scroll_height = pre.scroll_height() as f64;
        let expected_total = row_height * total_lines as f64;
        // Allow a generous tolerance: scroll_height includes spacer rounding
        // and any padding on the `<pre>` itself.
        let ratio = scroll_height / expected_total;
        assert!(
            (0.9..=1.15).contains(&ratio),
            "scroll geometry is wrong: scroll_height={scroll_height:.0}px, \
             expected≈{expected_total:.0}px (row_height={row_height:.1}px × \
             {total_lines} lines), ratio={ratio:.3}"
        );
    }

    /// The open-at-line path (from project search): a `pending_goto_line`
    /// request set before the file view mounts must seed the virtualization
    /// scroll so the target line's window renders on the *first* paint, even
    /// before the measurement Effect refines the geometry. This is the
    /// behaviour that makes "click a search result deep in a big file" land on
    /// the right line instead of the top.
    #[wasm_bindgen_test]
    async fn goto_line_seeds_window_to_target_on_first_paint() {
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "big.txt".to_owned(),
        };
        let total_lines = 5000;
        let content: String = (0..total_lines)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        // Target the 1-based line 400 → source index 399 → text "line 399",
        // far below any window that would render if the scroll stayed at 0.
        let target_line: u32 = 400;
        let target_text = format!("line {}", target_line - 1);

        let container = make_container();
        let mount_path = path.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            // Request the goto BEFORE mounting the view.
            state
                .pending_goto_line
                .set(Some((file_path.clone(), target_line)));
            provide_context(state);
            view! { <FileView tab_id=TabId(20_004) path=mount_path.clone() /> }
        });

        next_tick().await;

        // The rendered window must contain the target line on first paint.
        let rows = container
            .query_selector_all(".file-view-content > .file-line")
            .unwrap();
        let mut texts = Vec::new();
        for i in 0..rows.length() {
            if let Some(node) = rows.item(i) {
                texts.push(node.text_content().unwrap_or_default());
            }
        }
        assert!(
            texts.iter().any(|t| t == &target_text),
            "target line {target_line:?} (\"{target_text}\") not in the rendered \
             window on first paint; rendered first={:?} last={:?} count={}",
            texts.first(),
            texts.last(),
            texts.len(),
        );

        // And virtualization must still be engaged (we did not render the
        // whole 5000-line file to get there).
        assert!(
            (rows.length() as usize) < total_lines / 4,
            "virtualization regressed: rendered {} of {total_lines} lines",
            rows.length()
        );

        // After geometry is measured the target line stays in view.
        next_tick().await;
        let rows_after = container
            .query_selector_all(".file-view-content > .file-line")
            .unwrap();
        let mut still_present = false;
        for i in 0..rows_after.length() {
            if let Some(node) = rows_after.item(i)
                && node.text_content().unwrap_or_default() == target_text
            {
                still_present = true;
                break;
            }
        }
        assert!(
            still_present,
            "target line dropped out of the window after measurement"
        );
    }
}
