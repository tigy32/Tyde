use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::components::find_bar::{FindBar, FindState};
use crate::line_source::FileLines;
use crate::state::{AppState, CodeIntelKey, TabContent, TabId, TabScrollState};
use crate::syntax_highlight::{LineHighlighter, LineTokens, color_to_css, syntax_for_path};

use protocol::{
    CodeIntelDiagnostic, CodeIntelSeverity, CodeIntelState, ProjectFileVersion, ProjectPath,
};

/// Honest, user-facing label for a file's code-intelligence state. Mirrors the
/// typed `CodeIntelState` (spec §3) — cold start reads as "Indexing", never as
/// a faked empty result.
fn code_intel_state_label(state: CodeIntelState) -> &'static str {
    match state {
        CodeIntelState::Unsupported => "Unsupported",
        CodeIntelState::Unavailable => "Unavailable",
        CodeIntelState::Starting => "Starting…",
        CodeIntelState::Indexing => "Indexing…",
        CodeIntelState::Ready => "Ready",
        CodeIntelState::Failed => "Failed",
    }
}

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

/// Outer `FileView` is intentionally thin: it tracks the loaded file version
/// (cheap `Memo`) and keys the heavy inner `FileViewLoaded` by that version.
/// Opening a different file in another tab still does not rebuild this tab, but
/// a same-path reload at a new `ProjectFileVersion` remounts the body so the DOM,
/// line table, click/hover versions, and visible-range hints all describe the
/// same contents.
#[component]
pub fn FileView(tab_id: TabId, path: ProjectPath) -> impl IntoView {
    let state = expect_context::<AppState>();
    let file_path = path.clone();
    let loaded_version: Memo<Option<ProjectFileVersion>> = Memo::new(move |_| {
        state
            .open_files
            .with(|files| files.get(&file_path).map(|file| file.version))
    });

    let path_for_loaded = path.clone();
    view! {
        <div class="file-view">
            <Show
                when=move || loaded_version.get().is_some()
                fallback=move || view! { <div class="panel-empty">"No file open"</div> }
            >
                <For
                    each=move || { loaded_version.get().into_iter().collect::<Vec<_>>() }
                    key=|version| *version
                    children={
                        let path_for_loaded = path_for_loaded.clone();
                        move |version| {
                            view! {
                                <FileViewLoaded
                                    tab_id=tab_id
                                    path=path_for_loaded.clone()
                                    version=version
                                />
                            }
                        }
                    }
                />
            </Show>
        </div>
    }
}

/// Per-tab file body. All heavy setup (line table, find state,
/// virtualization signals, async syntect task) runs once per rendered file
/// version. Reads contents untracked from `open_files`; the parent keys this
/// component by `version`, so a same-path reload remounts with fresh contents.
#[component]
fn FileViewLoaded(tab_id: TabId, path: ProjectPath, version: ProjectFileVersion) -> impl IntoView {
    let state = expect_context::<AppState>();
    let initial_scroll_state = state.tab_scroll_state_untracked(tab_id);

    let f = state
        .open_files
        .with_untracked(|files| files.get(&path).cloned())
        .filter(|file| file.version == version)
        .expect("FileViewLoaded mounted with no open_files entry for version");

    let close_path = path.clone();
    // Key for the code-intel status indicator. Stable per tab (the owning
    // host/project don't change for an open file); the status itself is read
    // reactively from the signal in the header.
    let code_intel_key = state
        .active_project_ref_untracked()
        .map(|project| CodeIntelKey {
            host_id: project.host_id,
            project_id: project.project_id,
            path: path.clone(),
        });
    let code_intel_signal = state.code_intel;
    // The version of the contents this view renders. Code-intel status is read
    // at exactly this version, enforcing the version-equals-rendered rule at the
    // render site (spec §6): we never paint status computed against other text.
    let code_intel_version = f.version;
    {
        let state_for_focus = state.clone();
        let focus_path = f.path.clone();
        let focus_version = f.version;
        Effect::new(move |_| {
            state_for_focus.code_intel_focus.update(|focus| {
                if focus.as_ref().is_some_and(|(path, _)| path == &focus_path) {
                    *focus = Some((focus_path.clone(), focus_version));
                }
            });
        });
    }
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
    let initial_goto_line: Option<u32> = state.pending_goto_line.with_untracked(|pending| {
        pending
            .as_ref()
            .and_then(|(path, line)| (*path == goto_path).then_some(*line))
    });
    if initial_goto_line.is_some() {
        state.pending_goto_line.set(None);
    }
    // Go-to-definition targets address by byte offset, not line. Convert with
    // this file's `FileLines` to a 1-based line and feed the same goto path.
    let initial_goto_offset_line: Option<u32> =
        state.pending_goto_offset.with_untracked(|pending| {
            pending
                .as_ref()
                .and_then(|(path, byte)| (*path == goto_path).then_some(*byte))
                .map(|byte| lines.line_for_byte(byte) as u32 + 1)
        });
    if initial_goto_offset_line.is_some() {
        state.pending_goto_offset.set(None);
    }
    let initial_goto: Option<u32> = initial_goto_line.or(initial_goto_offset_line);

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
    let restored_initial_scroll = std::rc::Rc::new(std::cell::Cell::new(initial_goto.is_some()));
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

    // Same bridge for byte-offset gotos (go-to-definition into an already-open
    // tab): convert the target byte offset to a 1-based line with this file's
    // `FileLines` and reuse the line-based scroll-snap.
    let goto_path_for_offset_bridge = f.path.clone();
    let state_for_goto_offset = state.clone();
    let lines_for_goto_offset = lines.clone();
    Effect::new(move |_| {
        if let Some((target_path, byte)) = state_for_goto_offset.pending_goto_offset.get()
            && target_path == goto_path_for_offset_bridge
        {
            let line = lines_for_goto_offset.line_for_byte(byte) as u32 + 1;
            pending_line.set(Some(line));
            state_for_goto_offset.pending_goto_offset.set(None);
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
        state_for_goto_scroll
            .save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
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
    // Debounced hover state lives here so scroll / mouseleave / remount cleanup
    // can cancel a pending request, not just the popover.
    let hover_timer: TimeoutClosureSlot = StoredValue::new_local(None);
    on_cleanup(move || clear_timeout_timer(hover_timer));

    let state_for_scroll = state.clone();
    let scroll_hover_timer = hover_timer;
    let on_scroll = move |_: web_sys::Event| {
        if let Some(el) = pre_ref.get_untracked() {
            scroll_top.set(el.scroll_top() as f64);
            let element: web_sys::Element = el.clone().unchecked_into();
            state_for_scroll.save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
        }
        // A scroll moves the hovered span: cancel a pending hover and dismiss
        // (superseding the hover id so a late result is dropped).
        clear_timeout_timer(scroll_hover_timer);
        crate::actions::dismiss_hover(&state_for_scroll);
    };

    // ── Go-to-definition (Cmd/Ctrl+click) + hover ──────────────────────────
    // Cmd/Ctrl+click resolves the definition under the click. M3: it first
    // consults the pushed model for an already-resolved target (instant local
    // jump); only on a miss does it fall back to the on-demand
    // `code_intel_navigate` (M2). A plain click only records this file as the
    // F12 focus.
    let lines_for_click = lines.clone();
    let state_for_click = state.clone();
    let click_path = f.path.clone();
    let click_version = f.version;
    let on_content_click = move |ev: web_sys::MouseEvent| {
        state_for_click
            .code_intel_focus
            .set(Some((click_path.clone(), click_version)));
        if !(ev.ctrl_key() || ev.meta_key()) {
            return;
        }
        ev.prevent_default();
        if let Some(offset) =
            byte_offset_at_point(&lines_for_click, ev.client_x() as f64, ev.client_y() as f64)
        {
            crate::actions::navigate_to_definition(
                &state_for_click,
                click_path.clone(),
                click_version,
                offset,
            );
        }
    };

    // Debounced hover: a settled pointer over an identifier fires a single
    // `code_intel_hover`.
    let lines_for_hover = lines.clone();
    let state_for_hover = state.clone();
    let hover_path = f.path.clone();
    let hover_version = f.version;
    let move_hover_timer = hover_timer;
    let on_content_mousemove = move |ev: web_sys::MouseEvent| {
        let Some(window) = web_sys::window() else {
            return;
        };
        clear_timeout_timer(move_hover_timer);
        let client_x = ev.client_x() as f64;
        let client_y = ev.client_y() as f64;
        let lines = lines_for_hover.clone();
        let state = state_for_hover.clone();
        let path = hover_path.clone();
        let version = hover_version;
        let cb = Closure::<dyn FnMut()>::new(move || {
            maybe_request_hover(&state, &lines, path.clone(), version, client_x, client_y);
        });
        let id = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                HOVER_DEBOUNCE_MS,
            )
            .ok();
        if let Some(id) = id {
            move_hover_timer.update_value(|slot| *slot = Some((id, cb)));
        }
    };

    let state_for_leave = state.clone();
    let leave_hover_timer = hover_timer;
    let on_content_mouseleave = move |_: web_sys::MouseEvent| {
        clear_timeout_timer(leave_hover_timer);
        crate::actions::dismiss_hover(&state_for_leave);
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

    // ── Visible-range prioritization hint (M3) ─────────────────────────────
    // When the visible line window changes, tell the server which byte range is
    // on screen so its background definition resolution resolves those
    // occurrences first. Debounced so a scroll burst sends at most one hint per
    // settle. A pure hint — it never gates which identifiers are clickable.
    let visible_timer: TimeoutClosureSlot = StoredValue::new_local(None);
    on_cleanup(move || clear_timeout_timer(visible_timer));
    {
        let state_for_visible = state.clone();
        let visible_path = f.path.clone();
        let visible_version = f.version;
        let lines_for_visible = lines.clone();
        let visible_timer_for_effect = visible_timer;
        Effect::new(move |_| {
            let (start, end) = visible_window.get();
            if total == 0 || end <= start {
                return;
            }
            // Map the on-screen line window to an absolute byte range.
            let last = total - 1;
            let start_byte = lines_for_visible.line_start(start.min(last));
            let end_byte = lines_for_visible.line_content_end(end.saturating_sub(1).min(last));
            let range = protocol::ByteRange {
                start: start_byte,
                end: end_byte,
            };

            let Some(window) = web_sys::window() else {
                return;
            };
            // Debounce: cancel a pending send and schedule a fresh one.
            clear_timeout_timer(visible_timer_for_effect);
            let state = state_for_visible.clone();
            let path = visible_path.clone();
            let cb = Closure::<dyn FnMut()>::new(move || {
                crate::actions::send_visible_range(&state, path.clone(), visible_version, range);
            });
            let id = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    VISIBLE_RANGE_DEBOUNCE_MS,
                )
                .ok();
            if let Some(id) = id {
                visible_timer_for_effect.update_value(|slot| *slot = Some((id, cb)));
            }
        });
    }

    // Per-line diagnostic decorations derived from the code-intel signal. Kept
    // in a `Memo` (not folded into `Token`) so decoration logic stays off the
    // per-row text path the wasm test guards. Honors the
    // version-equals-rendered rule: only diagnostics whose `ProjectFileVersion`
    // matches the rendered file version contribute, so v4 squiggles never paint
    // over v5 text.
    let decorations: Memo<HashMap<usize, LineDecorations>> = {
        let key = code_intel_key.clone();
        let lines = lines.clone();
        let version = code_intel_version;
        Memo::new(move |_| {
            let Some(key) = key.clone() else {
                return HashMap::new();
            };
            code_intel_signal.with(|map| {
                let Some(file) = map.get(&key) else {
                    return HashMap::new();
                };
                if file.rendered_version != Some(version) {
                    return HashMap::new();
                }
                match file.applied() {
                    Some(data) if !data.diagnostics.is_empty() => {
                        build_line_decorations(&lines, &data.diagnostics)
                    }
                    _ => HashMap::new(),
                }
            })
        })
    };

    let lines_for_render = lines.clone();
    let highlighted_for_render = highlighted.clone();
    let find_for_render = find_state.clone();

    view! {
                            <div class="file-view-header">
                                <span class="file-view-path">{path_display}</span>
                                {
                                    let code_intel_key = code_intel_key.clone();
                                    move || {
                                        let key = code_intel_key.clone()?;
                                        let label = code_intel_signal.with(|map| {
                                            let file = map.get(&key)?;
                                            // Only show status the server resolved
                                            // against the exact text this view is
                                            // rendering (version-equals-rendered).
                                            if file.rendered_version != Some(code_intel_version) {
                                                return None;
                                            }
                                            file.applied().and_then(|data| {
                                                data.error
                                                    .as_ref()
                                                    .map(|error| format!("Error: {}", error.message))
                                                    .or_else(|| {
                                                        data.status.as_ref().map(|status| {
                                                            code_intel_state_label(status.state).to_owned()
                                                        })
                                                    })
                                            })
                                        })?;
                                        Some(view! {
                                            <span class="file-view-code-intel-status">{label}</span>
                                        })
                                    }
                                }
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
                                on:click=on_content_click
                                on:mousemove=on_content_mousemove
                                on:mouseleave=on_content_mouseleave
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
                                        let find_for_class = find_for_render.clone();
                                        view! {
                                            <div
                                                class=move || file_line_class_with_diagnostics(
                                                    i, &find_for_class, decorations,
                                                )
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
                                                    // colored spans. Diagnostic
                                                    // squiggles overlay via the
                                                    // same reactive read of the
                                                    // decorations memo.
                                                    let tokens = highlighted_for_row
                                                        .with(|v| v.get(i).and_then(|t| t.clone()));
                                                    let decos = decorations
                                                        .with(|m| m.get(&i).cloned());
                                                    render_file_line_content(
                                                        text.clone(), tokens, decos,
                                                    )
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

/// The row class plus a gutter-severity class derived from this line's
/// diagnostics. Reads both the find results and the decorations memo so the row
/// re-renders when either changes. The base `file-line` class is preserved so
/// structural test selectors and layout are unaffected.
fn file_line_class_with_diagnostics(
    line_idx: usize,
    find: &FindState,
    decorations: Memo<HashMap<usize, LineDecorations>>,
) -> String {
    let base = file_line_class(line_idx, find);
    let gutter = decorations.with(|map| map.get(&line_idx).and_then(|d| d.gutter));
    format!("{base}{}", gutter_class(gutter))
}

/// Per-line diagnostic decorations: a gutter dot severity and the squiggle
/// spans (byte ranges relative to the line start).
#[derive(Clone, Debug, Default, PartialEq)]
struct LineDecorations {
    /// Highest-severity diagnostic touching this line → the gutter dot.
    gutter: Option<CodeIntelSeverity>,
    /// `(start_byte, end_byte, severity)` spans relative to the line start.
    /// Half-open and non-empty.
    spans: Vec<(u32, u32, CodeIntelSeverity)>,
}

fn severity_rank(severity: CodeIntelSeverity) -> u8 {
    match severity {
        CodeIntelSeverity::Error => 3,
        CodeIntelSeverity::Warning => 2,
        CodeIntelSeverity::Information => 1,
        CodeIntelSeverity::Hint => 0,
    }
}

/// Keep whichever severity is more severe.
fn more_severe(
    current: Option<CodeIntelSeverity>,
    candidate: CodeIntelSeverity,
) -> Option<CodeIntelSeverity> {
    match current {
        Some(existing) if severity_rank(existing) >= severity_rank(candidate) => Some(existing),
        _ => Some(candidate),
    }
}

fn severity_token(severity: CodeIntelSeverity) -> &'static str {
    match severity {
        CodeIntelSeverity::Error => "error",
        CodeIntelSeverity::Warning => "warning",
        CodeIntelSeverity::Information => "info",
        CodeIntelSeverity::Hint => "hint",
    }
}

fn gutter_class(severity: Option<CodeIntelSeverity>) -> &'static str {
    match severity {
        Some(CodeIntelSeverity::Error) => " code-intel-gutter-error",
        Some(CodeIntelSeverity::Warning) => " code-intel-gutter-warning",
        Some(CodeIntelSeverity::Information) => " code-intel-gutter-info",
        Some(CodeIntelSeverity::Hint) => " code-intel-gutter-hint",
        None => "",
    }
}

/// Map absolute-file-byte diagnostics onto per-line decorations. A diagnostic
/// spanning multiple lines contributes a squiggle on each covered line and a
/// gutter dot on each. Byte offsets are clamped to each line's content.
fn build_line_decorations(
    lines: &FileLines,
    diagnostics: &[CodeIntelDiagnostic],
) -> HashMap<usize, LineDecorations> {
    let mut map: HashMap<usize, LineDecorations> = HashMap::new();
    let line_count = lines.len();
    if line_count == 0 {
        return map;
    }
    for diagnostic in diagnostics {
        let start = diagnostic.range.start;
        let end = diagnostic.range.end.max(start);
        let last_byte = end.saturating_sub(1).max(start);
        let start_line = lines.line_for_byte(start);
        let end_line = lines.line_for_byte(last_byte);
        for line_idx in start_line..=end_line {
            if line_idx >= line_count {
                break;
            }
            let line_start = lines.line_start(line_idx);
            let line_end = lines.line_content_end(line_idx);
            let entry = map.entry(line_idx).or_default();
            entry.gutter = more_severe(entry.gutter, diagnostic.severity);
            let seg_start = start.max(line_start);
            let seg_end = end.min(line_end);
            if seg_end > seg_start {
                entry.spans.push((
                    seg_start - line_start,
                    seg_end - line_start,
                    diagnostic.severity,
                ));
            }
        }
    }
    map
}

/// The most severe squiggle covering byte position `pos` (relative to the line
/// start), if any.
fn squiggle_severity_at(
    pos: u32,
    spans: &[(u32, u32, CodeIntelSeverity)],
) -> Option<CodeIntelSeverity> {
    let mut best = None;
    for (start, end, severity) in spans {
        if *start <= pos && pos < *end {
            best = more_severe(best, *severity);
        }
    }
    best
}

/// Render a file line's text inside the row. Emits a `<code>` element so
/// monospace/whitespace styling stays scoped, with either pre-tokenized colored
/// spans or a single plain-text node when no tokens are available. Diagnostic
/// squiggles (if any) split spans at byte boundaries and add a wavy-underline
/// class — the **rendered characters are identical** with or without the
/// overlay (the split-at-boundary invariant the wasm test guards).
fn render_file_line_content(
    text: String,
    tokens: Option<LineTokens>,
    decorations: Option<LineDecorations>,
) -> AnyView {
    let squiggles: &[(u32, u32, CodeIntelSeverity)] = decorations
        .as_ref()
        .map(|d| d.spans.as_slice())
        .unwrap_or(&[]);
    match tokens {
        Some(toks) if !toks.is_empty() => {
            if squiggles.is_empty() {
                // Unchanged fast path: one styled span per token.
                let spans: Vec<AnyView> = toks
                    .into_iter()
                    .map(|t| {
                        let style = format!("color:{}", color_to_css(t.fg));
                        view! { <span style=style>{t.text}</span> }.into_any()
                    })
                    .collect();
                view! { <code class="file-line-code">{spans}</code> }.into_any()
            } else {
                let spans = decorate_tokens(toks, squiggles);
                view! { <code class="file-line-code">{spans}</code> }.into_any()
            }
        }
        _ => {
            if squiggles.is_empty() {
                view! { <code class="file-line-code">{text}</code> }.into_any()
            } else {
                let spans = decorate_plain(&text, squiggles);
                view! { <code class="file-line-code">{spans}</code> }.into_any()
            }
        }
    }
}

/// Split tokenized spans at squiggle boundaries, preserving color and text.
fn decorate_tokens(
    tokens: LineTokens,
    squiggles: &[(u32, u32, CodeIntelSeverity)],
) -> Vec<AnyView> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    for token in tokens {
        let color = color_to_css(token.fg);
        push_decorated_segment(&token.text, offset, Some(&color), squiggles, &mut out);
        offset += token.text.len() as u32;
    }
    out
}

/// Split a single plain-text line at squiggle boundaries (no syntax color).
fn decorate_plain(text: &str, squiggles: &[(u32, u32, CodeIntelSeverity)]) -> Vec<AnyView> {
    let mut out = Vec::new();
    push_decorated_segment(text, 0, None, squiggles, &mut out);
    out
}

/// Emit one base segment (`seg_text` starting at byte `seg_start` within the
/// line) as one or more `<span>`s, cutting at any squiggle edges that fall on a
/// char boundary inside the segment. Each piece keeps the segment's color and
/// gains a squiggle class if it lies within a diagnostic range. Non-boundary
/// edges are ignored so the text is never sliced mid-char (and never dropped).
fn push_decorated_segment(
    seg_text: &str,
    seg_start: u32,
    color: Option<&str>,
    squiggles: &[(u32, u32, CodeIntelSeverity)],
    out: &mut Vec<AnyView>,
) {
    let seg_end = seg_start + seg_text.len() as u32;
    let mut cuts: Vec<u32> = vec![seg_start, seg_end];
    for (start, end, _) in squiggles {
        for edge in [*start, *end] {
            if edge > seg_start && edge < seg_end {
                let rel = (edge - seg_start) as usize;
                if seg_text.is_char_boundary(rel) {
                    cuts.push(edge);
                }
            }
        }
    }
    cuts.sort_unstable();
    cuts.dedup();

    for window in cuts.windows(2) {
        let (p, q) = (window[0], window[1]);
        if q <= p {
            continue;
        }
        let piece = &seg_text[(p - seg_start) as usize..(q - seg_start) as usize];
        if piece.is_empty() {
            continue;
        }
        let severity = squiggle_severity_at(p, squiggles);
        out.push(make_line_span(piece, color, severity));
    }
}

// ── Go-to-definition / hover: byte offset under a point ─────────────────────

/// Byte offset within `line` of UTF-16 column `utf16_col`. This is the inverse
/// of walking a line's chars accumulating UTF-16 widths — the same conversion
/// the server's `LineIndex` does, kept here so a click's DOM caret column maps
/// to a Tyde byte offset. Multibyte-safe: a column past the line end clamps to
/// the line's byte length, and a column landing between surrogate halves is not
/// representable from a real caret so it never slices mid-char.
fn line_byte_for_utf16_col(line: &str, utf16_col: u32) -> u32 {
    let mut seen = 0u32;
    for (byte, ch) in line.char_indices() {
        if seen >= utf16_col {
            return byte as u32;
        }
        seen += ch.len_utf16() as u32;
    }
    line.len() as u32
}

/// All descendant text nodes of `root`, in document order.
fn descendant_text_nodes(root: &web_sys::Node) -> Vec<web_sys::Node> {
    let mut out = Vec::new();
    let children = root.child_nodes();
    for i in 0..children.length() {
        let Some(child) = children.item(i) else {
            continue;
        };
        if child.node_type() == web_sys::Node::TEXT_NODE {
            out.push(child);
        } else {
            out.extend(descendant_text_nodes(&child));
        }
    }
    out
}

/// The UTF-16 column within a line's rendered `<code>` element for a caret at
/// (`target` text node, `target_offset`). Sums the UTF-16 lengths of the text
/// nodes preceding `target` (the line may be split into many colored / squiggle
/// spans) and adds the in-node offset. `None` if the caret isn't on one of the
/// code element's text nodes.
fn utf16_col_in_code(
    code: &web_sys::Node,
    target: &web_sys::Node,
    target_offset: u32,
) -> Option<u32> {
    let mut acc = 0u32;
    for text_node in descendant_text_nodes(code) {
        if text_node.is_same_node(Some(target)) {
            return Some(acc + target_offset);
        }
        let len = text_node
            .text_content()
            .unwrap_or_default()
            .encode_utf16()
            .count() as u32;
        acc += len;
    }
    None
}

/// Map an `Element`-or-text `Node` to its containing `Element`.
fn node_to_element(node: &web_sys::Node) -> Option<web_sys::Element> {
    if let Some(element) = node.dyn_ref::<web_sys::Element>() {
        return Some(element.clone());
    }
    node.parent_element()
}

/// Absolute file byte offset for a DOM caret at (`node`, `offset`) — the shared
/// core of click navigation, hover, and F12. Returns `None` when the caret
/// isn't over file text (e.g. the gutter, a spacer, or outside any
/// `.file-line`).
fn byte_offset_from_caret(lines: &FileLines, node: &web_sys::Node, offset: u32) -> Option<u32> {
    let element = node_to_element(node)?;
    let row = element.closest(".file-line").ok().flatten()?;
    let line_idx: usize = row.get_attribute("data-find-idx")?.parse().ok()?;
    if line_idx >= lines.len() {
        return None;
    }
    let code = row.query_selector(".file-line-code").ok().flatten()?;
    let code_node: &web_sys::Node = code.unchecked_ref();
    let utf16_col = utf16_col_in_code(code_node, node, offset)?;
    let line_byte = line_byte_for_utf16_col(lines.line(line_idx), utf16_col);
    Some(lines.line_start(line_idx) + line_byte)
}

/// Absolute file byte offset for a DOM `Range`'s start (used by F12, which reads
/// the current selection).
fn byte_offset_from_range(lines: &FileLines, range: &web_sys::Range) -> Option<u32> {
    let node = range.start_container().ok()?;
    let offset = range.start_offset().ok()?;
    byte_offset_from_caret(lines, &node, offset)
}

/// Call WebKit's non-standard `caretRangeFromPoint`, which `web-sys` does not
/// bind. Tyde runs in WKWebView, which historically ships `caretRangeFromPoint`
/// but **not** the standard `caretPositionFromPoint`. Invoked via `Reflect`
/// (a `#[wasm_bindgen] method` on the foreign `Document` type isn't allowed),
/// only after `document_has_method` confirms it exists.
fn caret_range_from_point(document: &web_sys::Document, x: f64, y: f64) -> Option<web_sys::Range> {
    let func = js_sys::Reflect::get(
        document,
        &wasm_bindgen::JsValue::from_str("caretRangeFromPoint"),
    )
    .ok()?
    .dyn_into::<js_sys::Function>()
    .ok()?;
    func.call2(
        document.as_ref(),
        &wasm_bindgen::JsValue::from_f64(x),
        &wasm_bindgen::JsValue::from_f64(y),
    )
    .ok()?
    .dyn_into::<web_sys::Range>()
    .ok()
}

/// A resolved caret: the text node + UTF-16 offset under a point, plus an
/// optional client rect for anchoring a popover. Unifies the two browser APIs
/// (`caretPositionFromPoint` and WebKit's `caretRangeFromPoint`).
struct CaretHit {
    node: web_sys::Node,
    offset: u32,
    rect: Option<web_sys::DomRect>,
}

/// Whether `document` exposes a callable method `name` (walks the prototype
/// chain). Used to feature-detect the caret API so we never call a method that
/// doesn't exist (which would throw in WKWebView).
fn document_has_method(document: &web_sys::Document, name: &str) -> bool {
    js_sys::Reflect::get(document, &wasm_bindgen::JsValue::from_str(name))
        .map(|value| value.is_function())
        .unwrap_or(false)
}

/// The caret under a viewport point. Prefers the standard
/// `caretPositionFromPoint` (Chromium/Firefox); falls back to WebKit's
/// `caretRangeFromPoint` (WKWebView). `None` if neither API exists or the point
/// isn't over text — both degrade gracefully to "click does nothing".
fn caret_at_point(client_x: f64, client_y: f64) -> Option<CaretHit> {
    let document = web_sys::window()?.document()?;
    if document_has_method(&document, "caretPositionFromPoint") {
        let caret = document.caret_position_from_point(client_x as f32, client_y as f32)?;
        return Some(CaretHit {
            node: caret.offset_node()?,
            offset: caret.offset(),
            rect: caret.get_client_rect(),
        });
    }
    if document_has_method(&document, "caretRangeFromPoint") {
        let range = caret_range_from_point(&document, client_x, client_y)?;
        return Some(CaretHit {
            node: range.start_container().ok()?,
            offset: range.start_offset().ok()?,
            rect: Some(range.get_bounding_client_rect()),
        });
    }
    None
}

/// Absolute file byte offset under a viewport point. `None` if the point isn't
/// over file text.
fn byte_offset_at_point(lines: &FileLines, client_x: f64, client_y: f64) -> Option<u32> {
    let hit = caret_at_point(client_x, client_y)?;
    byte_offset_from_caret(lines, &hit.node, hit.offset)
}

/// Whether the byte at `offset` in the file begins an identifier-ish char
/// (alphanumeric or `_`). Used to gate hover requests so we don't pop a hover
/// over whitespace / punctuation.
fn is_identifier_byte(lines: &FileLines, line_idx: usize, line_byte: u32) -> bool {
    let line = lines.line(line_idx);
    line[line_byte as usize..]
        .chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false)
}

/// Debounce before firing a hover request, so a moving pointer doesn't spam the
/// server. Matches the typical editor hover delay.
const HOVER_DEBOUNCE_MS: i32 = 250;

/// Debounce before sending a `code_intel_set_visible_range` hint, so a scroll
/// burst coalesces into a single prioritization update once scrolling settles.
const VISIBLE_RANGE_DEBOUNCE_MS: i32 = 120;

/// Keeps a pending `setTimeout` handle and its callback alive until it fires,
/// is replaced, or the file view remounts.
type TimeoutClosureSlot = StoredValue<Option<(i32, Closure<dyn FnMut()>)>, LocalStorage>;

fn clear_timeout_timer(timer: TimeoutClosureSlot) {
    timer.update_value(|slot| {
        if let Some((id, _cb)) = slot.take()
            && let Some(window) = web_sys::window()
        {
            window.clear_timeout_with_handle(id);
        }
    });
}

/// Go-to-definition from the current caret / selection (the F12 keybinding,
/// which has no file context of its own). Resolves against the file most
/// recently interacted with (`code_intel_focus`) using the live DOM selection;
/// a no-op when nothing is focused or selected. Public so `app.rs`'s global
/// keydown listener can call it.
pub fn navigate_from_current_selection(state: &AppState) {
    let Some((path, _)) = state.code_intel_focus.get_untracked() else {
        return;
    };
    let Some((version, content)) = state.open_files.with_untracked(|files| {
        files
            .get(&path)
            .and_then(|of| of.contents.clone().map(|content| (of.version, content)))
    }) else {
        return;
    };
    let lines = FileLines::new(&content);
    let Some(selection) = web_sys::window().and_then(|w| w.get_selection().ok().flatten()) else {
        return;
    };
    let Ok(range) = selection.get_range_at(0) else {
        return;
    };
    if let Some(offset) = byte_offset_from_range(&lines, &range) {
        crate::actions::navigate_to_definition(state, path, version, offset);
    }
}

/// Find-references from the current caret / selection (the Shift+F12
/// keybinding). Like [`navigate_from_current_selection`] it resolves against the
/// file most recently interacted with (`code_intel_focus`) using the live DOM
/// selection; a no-op when nothing is focused or selected. The selected text (if
/// it's a short single token) is captured as the panel's symbol label. Public so
/// `app.rs`'s global keydown listener can call it.
pub fn find_references_from_current_selection(state: &AppState) {
    let Some((path, _)) = state.code_intel_focus.get_untracked() else {
        return;
    };
    let Some((version, content)) = state.open_files.with_untracked(|files| {
        files
            .get(&path)
            .and_then(|of| of.contents.clone().map(|content| (of.version, content)))
    }) else {
        return;
    };
    let lines = FileLines::new(&content);
    let Some(selection) = web_sys::window().and_then(|w| w.get_selection().ok().flatten()) else {
        return;
    };
    let Ok(range) = selection.get_range_at(0) else {
        return;
    };
    // A short, single-token selection makes a nice "References to `foo`" header;
    // anything longer (or empty) is dropped and the panel shows a generic title.
    let symbol = {
        let text = selection.to_string().as_string().unwrap_or_default();
        let trimmed = text.trim();
        if !trimmed.is_empty() && trimmed.len() <= 64 && !trimmed.contains(char::is_whitespace) {
            Some(trimmed.to_owned())
        } else {
            None
        }
    };
    if let Some(offset) = byte_offset_from_range(&lines, &range) {
        crate::actions::start_find_references(state, path, version, offset, symbol);
    }
}

/// Compute the byte offset under (`client_x`, `client_y`) and, if it lands on an
/// identifier on this file, fire a debounced hover request anchored to the
/// hovered char. Non-identifier targets (whitespace, punctuation) dismiss any
/// open popover. Deduped against the current popover's offset to avoid flicker
/// while the pointer sits on the same identifier.
fn maybe_request_hover(
    state: &AppState,
    lines: &FileLines,
    path: ProjectPath,
    version: protocol::ProjectFileVersion,
    client_x: f64,
    client_y: f64,
) {
    let Some(caret) = caret_at_point(client_x, client_y) else {
        crate::actions::dismiss_hover(state);
        return;
    };
    let Some(offset) = byte_offset_from_caret(lines, &caret.node, caret.offset) else {
        crate::actions::dismiss_hover(state);
        return;
    };
    let line_idx = lines.line_for_byte(offset);
    let line_byte = offset - lines.line_start(line_idx);
    if !is_identifier_byte(lines, line_idx, line_byte) {
        crate::actions::dismiss_hover(state);
        return;
    }
    // Already showing/awaiting a hover for this exact identifier: leave it.
    if state
        .code_intel_hover
        .with_untracked(|h| h.as_ref().map(|p| p.offset) == Some(offset))
    {
        return;
    }
    let (left, top, bottom) = match caret.rect {
        Some(rect) => (rect.left(), rect.top(), rect.bottom()),
        None => (client_x, client_y, client_y + 16.0),
    };
    crate::actions::request_hover(state, path, version, offset, left, top, bottom);
}

fn make_line_span(text: &str, color: Option<&str>, severity: Option<CodeIntelSeverity>) -> AnyView {
    let style = color.map(|c| format!("color:{c}"));
    let class = severity.map(|s| {
        format!(
            "code-intel-squiggle code-intel-squiggle-{}",
            severity_token(s)
        )
    });
    view! { <span class=class style=style>{text.to_owned()}</span> }.into_any()
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
/// Native (non-DOM) unit tests for the byte-offset-under-click conversion.
/// This is the net-new, error-prone arithmetic, so it is checked directly on
/// multibyte input independent of the browser caret API.
#[cfg(test)]
mod conversion_tests {
    use super::line_byte_for_utf16_col;

    #[test]
    fn ascii_columns_map_to_themselves() {
        let line = "let x = 1;";
        assert_eq!(line_byte_for_utf16_col(line, 0), 0);
        assert_eq!(line_byte_for_utf16_col(line, 4), 4); // 'x'
        assert_eq!(line_byte_for_utf16_col(line, 99), line.len() as u32); // clamp
    }

    #[test]
    fn cjk_three_byte_one_utf16_unit() {
        // "let 名前 = 1": "let " is 4 bytes / 4 UTF-16 units; each CJK char is
        // 3 bytes but 1 UTF-16 unit.
        let line = "let 名前 = 1";
        assert_eq!(line_byte_for_utf16_col(line, 4), 4); // start of 名
        assert_eq!(line_byte_for_utf16_col(line, 5), 7); // between 名/前 (名 is 3 bytes)
        assert_eq!(line_byte_for_utf16_col(line, 6), 10); // after 前 (名前 = 6 bytes)
    }

    #[test]
    fn astral_char_two_utf16_units() {
        // "😀" is U+1F600: 4 UTF-8 bytes, 2 UTF-16 code units (a surrogate pair).
        let line = "a😀b";
        assert_eq!(line_byte_for_utf16_col(line, 0), 0); // 'a'
        assert_eq!(line_byte_for_utf16_col(line, 1), 1); // start of 😀
        // Column 3 is just after the surrogate pair → 'b' at byte 5 (1 + 4).
        assert_eq!(line_byte_for_utf16_col(line, 3), 5);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, OpenFile};
    use leptos::mount::mount_to;
    use protocol::{ProjectFileVersion, ProjectPath, ProjectRootPath};
    use std::cell::RefCell;
    use std::rc::Rc;
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
                "position: fixed; top: 0; left: 0; width: 800px; height: 600px; \
                 z-index: 2147483647; background: white; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    /// Install a stub that captures outbound `send_host_line` Tauri invokes
    /// (instead of hitting Tauri), so a test can inspect frames put on the wire.
    fn install_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
            "#,
        )
        .expect("install send stub");
    }

    /// Synthesize a Cmd/Ctrl+click at the left edge of the first rendered line's
    /// code, over an identifier (so a byte offset resolves under the caret).
    fn cmd_click_first_line(container: &HtmlElement) {
        let code = container
            .query_selector(".file-line-code")
            .unwrap()
            .expect("code element present");
        let rect = code.get_bounding_client_rect();
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_ctrl_key(true);
        init.set_meta_key(true);
        init.set_client_x((rect.left() + 1.0) as i32);
        init.set_client_y((rect.top() + rect.height() / 2.0) as i32);
        let event = web_sys::MouseEvent::new_with_mouse_event_init_dict("click", &init).unwrap();
        code.dispatch_event(&event).unwrap();
    }

    /// Whether a `code_intel_navigate` frame was put on the wire via the
    /// `install_send_stub` capture buffer.
    fn navigate_frame_was_sent() -> bool {
        js_sys::eval(
            r#"
            (function() {
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "code_intel_navigate") return true;
                }
                return false;
            })()
            "#,
        )
        .expect("probe send calls")
        .as_bool()
        .unwrap_or(false)
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
                        version: ProjectFileVersion(1),
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
    async fn same_path_new_version_remounts_with_fresh_contents() {
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "reload.txt".to_owned(),
        };

        let container = make_container();
        let mount_path = path.clone();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some("old contents".to_owned()),
                        is_binary: false,
                    },
                );
            });
            *cap.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <FileView tab_id=TabId(20_040) path=mount_path.clone() /> }
        });

        next_tick().await;
        let row = container
            .query_selector(".file-line-code")
            .unwrap()
            .expect("initial code line");
        assert_eq!(row.text_content().unwrap_or_default(), "old contents");

        let state = captured.borrow().clone().unwrap();
        state
            .code_intel_focus
            .set(Some((path.clone(), ProjectFileVersion(1))));
        state.open_files.update(|files| {
            files.insert(
                path.clone(),
                OpenFile {
                    path: path.clone(),
                    version: ProjectFileVersion(2),
                    contents: Some("new contents".to_owned()),
                    is_binary: false,
                },
            );
        });

        for _ in 0..5 {
            next_tick().await;
        }
        let row = container
            .query_selector(".file-line-code")
            .unwrap()
            .expect("reloaded code line");
        assert_eq!(
            row.text_content().unwrap_or_default(),
            "new contents",
            "same-path ProjectFileContents at a new version must update the rendered DOM"
        );
        assert_eq!(
            state.code_intel_focus.get_untracked(),
            Some((path.clone(), ProjectFileVersion(2))),
            "same-path remount must advance keyboard code-intel focus to the rendered version"
        );
    }

    #[wasm_bindgen_test]
    async fn code_intel_error_renders_in_file_header() {
        use crate::state::{ActiveProjectRef, CodeIntelKey};
        use protocol::{
            CodeIntelErrorCode, CodeIntelErrorContext, CodeIntelErrorPayload, ProjectId,
        };

        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        let host_id = "h";
        let project_id = ProjectId("p".to_owned());
        let container = make_container();
        let mount_path = path.clone();
        let mount_project = project_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some("fn main() {}".to_owned()),
                        is_binary: false,
                    },
                );
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: host_id.to_owned(),
                project_id: mount_project.clone(),
            }));
            let key = CodeIntelKey {
                host_id: host_id.to_owned(),
                project_id: mount_project.clone(),
                path: file_path.clone(),
            };
            state.code_intel.update(|map| {
                let entry = map.entry(key).or_default();
                entry.set_rendered_version(ProjectFileVersion(1));
                entry.merge_versioned(ProjectFileVersion(1), |data| {
                    data.error = Some(CodeIntelErrorPayload {
                        code: CodeIntelErrorCode::Internal,
                        message: "semanticTokens/full failed".to_owned(),
                        context: CodeIntelErrorContext::Subscribe {
                            path: file_path.clone(),
                        },
                        fatal: false,
                    });
                });
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_041) path=mount_path.clone() /> }
        });

        next_tick().await;
        let header = container
            .query_selector(".file-view-header")
            .unwrap()
            .expect("file header");
        let text = header.text_content().unwrap_or_default();
        assert!(
            text.contains("semanticTokens/full failed"),
            "code-intel errors must be visible in the file header; header was {text:?}"
        );
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
                        version: ProjectFileVersion(1),
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
                        version: ProjectFileVersion(1),
                        contents: Some("fn first() {}".to_owned()),
                        is_binary: false,
                    },
                );
                files.insert(
                    mount_second_path.clone(),
                    OpenFile {
                        path: mount_second_path.clone(),
                        version: ProjectFileVersion(1),
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
                        version: ProjectFileVersion(1),
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
    /// Diagnostics overlay: a `code_intel_diagnostics` snapshot in the
    /// code-intel signal must render an inline squiggle over exactly the
    /// diagnostic's byte range and a gutter dot on that line — and crucially the
    /// row's **visible text is byte-for-byte unchanged** when the squiggle
    /// overlays a syntax span (the split-at-boundary invariant). Mirrors the
    /// hard rule in CLAUDE.md: this guards the overlay path so an AI refactor
    /// can't silently mangle per-row text.
    #[wasm_bindgen_test]
    async fn diagnostics_render_squiggle_and_gutter_on_correct_line() {
        use crate::state::{ActiveProjectRef, CodeIntelKey};
        use protocol::{ByteRange, CodeIntelDiagnostic, CodeIntelSeverity, ProjectId};

        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        // Line 0 "fn main() {" is bytes 0..11, '\n' at 11. Line 1 starts at 12;
        // its prefix "    let " is 8 bytes, so the identifier `bad` occupies
        // line-bytes 8..11 → absolute file bytes 20..23.
        let content = "fn main() {\n    let bad: i32 = 5;\n}";
        let host_id = "test-host";
        let project_id = ProjectId("test-project".to_owned());

        let container = make_container();
        let mount_path = path.clone();
        let mount_host = host_id.to_owned();
        let mount_project = project_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            // The component derives its code-intel key from the active project.
            state.active_project.set(Some(ActiveProjectRef {
                host_id: mount_host.clone(),
                project_id: mount_project.clone(),
            }));
            // Seed a diagnostic over `bad` at the rendered version.
            let key = CodeIntelKey {
                host_id: mount_host.clone(),
                project_id: mount_project.clone(),
                path: file_path.clone(),
            };
            state.code_intel.update(|map| {
                let entry = map.entry(key).or_default();
                entry.set_rendered_version(ProjectFileVersion(1));
                entry.merge_versioned(ProjectFileVersion(1), |data| {
                    data.diagnostics = vec![CodeIntelDiagnostic {
                        range: ByteRange { start: 20, end: 23 },
                        severity: CodeIntelSeverity::Error,
                        message: "mismatched types".to_owned(),
                        source: Some("rustc".to_owned()),
                    }];
                });
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_010) path=mount_path.clone() /> }
        });

        next_tick().await;

        // Exactly one line carries an error gutter dot.
        let error_gutter = container
            .query_selector_all(".file-line.code-intel-gutter-error")
            .unwrap();
        assert_eq!(
            error_gutter.length(),
            1,
            "exactly one line should carry an error gutter dot, got {}",
            error_gutter.length()
        );

        // The squiggle appears immediately (plain text path) and persists once
        // syntax tokens land (tokenized overlay path). Loop a few ticks so we
        // also exercise the tokenized split.
        let mut squiggles = container
            .query_selector_all(".code-intel-squiggle")
            .unwrap();
        for _ in 0..20 {
            let styled = container
                .query_selector_all(".file-line code span[style]")
                .unwrap();
            if squiggles.length() > 0 && styled.length() > 0 {
                break;
            }
            next_tick().await;
            squiggles = container
                .query_selector_all(".code-intel-squiggle")
                .unwrap();
        }
        assert!(
            squiggles.length() > 0,
            "expected a squiggle span over the diagnostic range"
        );

        // The squiggle covers exactly the bytes of `bad` — proof the spans were
        // split at the diagnostic's byte boundaries, not over the whole line.
        let mut squiggle_texts = Vec::new();
        for i in 0..squiggles.length() {
            if let Some(node) = squiggles.item(i) {
                squiggle_texts.push(node.text_content().unwrap_or_default());
            }
        }
        assert!(
            squiggle_texts.iter().any(|t| t == "bad"),
            "squiggle should cover exactly `bad`; squiggle texts were {squiggle_texts:?}"
        );

        // The diagnostic row's visible text is byte-for-byte the source line —
        // the overlay added spans but changed no characters.
        let rows = container
            .query_selector_all(".file-view-content > .file-line")
            .unwrap();
        assert_eq!(rows.length(), 3, "expected one row per source line");
        let row1 = rows.item(1).unwrap();
        assert_eq!(
            row1.text_content().unwrap_or_default(),
            "    let bad: i32 = 5;",
            "diagnostic row text must equal the source line exactly"
        );

        // Line 0 (no diagnostic) carries neither a gutter dot nor a squiggle.
        let row0: Element = rows.item(0).unwrap().dyn_into().unwrap();
        assert!(
            !row0.class_name().contains("code-intel-gutter"),
            "line without a diagnostic must not get a gutter dot"
        );
        assert_eq!(
            row0.query_selector_all(".code-intel-squiggle")
                .unwrap()
                .length(),
            0,
            "line without a diagnostic must not get a squiggle"
        );
    }

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
                        version: ProjectFileVersion(1),
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

    /// The byte-offset-under-caret computation over a **real rendered DOM** with
    /// a multibyte line. Mounts a plain-text file (no syntax splitting, so the
    /// line is one text node), then drives `byte_offset_from_caret` with chosen
    /// UTF-16 caret offsets and asserts the absolute byte offset — the exact
    /// arithmetic a Cmd/Ctrl+click relies on.
    #[wasm_bindgen_test]
    async fn byte_offset_under_caret_handles_multibyte() {
        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "notes.txt".to_owned(),
        };
        // "let " is 4 bytes/UTF-16 units; 名前 is 6 bytes / 2 UTF-16 units.
        let content = "let 名前 = 1";

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
                        version: ProjectFileVersion(1),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_020) path=mount_path.clone() /> }
        });

        next_tick().await;

        let lines = FileLines::new(content);
        let code = container
            .query_selector(".file-line-code")
            .unwrap()
            .expect("code element present");
        let text_node = code.first_child().expect("a text node for the line");

        // UTF-16 col 4 = start of 名 → byte 4.
        assert_eq!(
            super::byte_offset_from_caret(&lines, &text_node, 4),
            Some(4)
        );
        // UTF-16 col 5 = between 名 and 前 → byte 7 (名 is 3 bytes).
        assert_eq!(
            super::byte_offset_from_caret(&lines, &text_node, 5),
            Some(7)
        );
        // UTF-16 col 6 = after 前 → byte 10 (名前 = 6 bytes).
        assert_eq!(
            super::byte_offset_from_caret(&lines, &text_node, 6),
            Some(10)
        );
    }

    /// A synthesized Cmd/Ctrl+click over an identifier dispatches a
    /// `code_intel_navigate` frame carrying a byte `offset`. A JS stub captures
    /// the outbound Tauri `send_host_line` calls so we can inspect the actual
    /// frame put on the wire. The exact byte-offset arithmetic on multibyte
    /// input is covered by `byte_offset_under_caret_handles_multibyte`.
    #[wasm_bindgen_test]
    async fn cmd_click_dispatches_navigate_frame() {
        use crate::state::ActiveProjectRef;
        use protocol::ProjectId;

        ensure_styles_loaded();

        // Capture outbound `send_host_line` invokes instead of hitting Tauri.
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
            "#,
        )
        .expect("install send stub");

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
                        version: ProjectFileVersion(1),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            }));
            provide_context(state);
            view! { <FileView tab_id=TabId(20_021) path=mount_path.clone() /> }
        });

        next_tick().await;

        // Click at the very left of the first line's code (over `fn`) with the
        // command modifier held.
        let code = container
            .query_selector(".file-line-code")
            .unwrap()
            .expect("code element present");
        let rect = code.get_bounding_client_rect();
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_ctrl_key(true);
        init.set_meta_key(true);
        init.set_client_x((rect.left() + 1.0) as i32);
        init.set_client_y((rect.top() + rect.height() / 2.0) as i32);
        let event = web_sys::MouseEvent::new_with_mouse_event_init_dict("click", &init).unwrap();
        code.dispatch_event(&event).unwrap();

        // Let the spawn_local send run.
        for _ in 0..10 {
            next_tick().await;
        }

        // A `code_intel_navigate` frame with a numeric byte `offset` was sent.
        let probe = js_sys::eval(
            r#"
            (function() {
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "code_intel_navigate") {
                        return env.kind + ":" + (typeof env.payload.offset);
                    }
                }
                return "";
            })()
            "#,
        )
        .expect("probe send calls")
        .as_string()
        .unwrap_or_default();
        assert_eq!(
            probe, "code_intel_navigate:number",
            "Cmd/Ctrl+click should dispatch a code_intel_navigate frame with a byte offset"
        );
    }

    /// M3: a Cmd/Ctrl+click on an occurrence whose `definition` target was
    /// already pushed navigates **locally** — it stashes the target in
    /// `pending_goto_offset` and emits **no** `code_intel_navigate` request.
    #[wasm_bindgen_test]
    async fn cmd_click_with_pushed_definition_navigates_locally() {
        use crate::state::{ActiveProjectRef, CodeIntelKey};
        use protocol::{
            ByteRange, CodeIntelCompleteness, CodeIntelFileModelPayload, CodeIntelLanguageId,
            CodeIntelLocation, CodeIntelModelRange, CodeIntelOccurrence, CodeIntelProviderId,
            CodeIntelRole, ProjectId,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        ensure_styles_loaded();
        install_send_stub();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        let target = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "lib.rs".to_owned(),
        };
        let content = "fn main() {}";

        let container = make_container();
        let mount_path = path.clone();
        let target_for_mount = target.clone();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            }));
            // A pushed model: the whole first line is one occurrence already
            // resolved to a target in lib.rs.
            let model = CodeIntelFileModelPayload {
                path: file_path.clone(),
                version: ProjectFileVersion(1),
                provider: CodeIntelProviderId("rust-analyzer".to_owned()),
                language: CodeIntelLanguageId("rust".to_owned()),
                model_range: CodeIntelModelRange::FullFile,
                completeness: CodeIntelCompleteness::Complete,
                occurrences: vec![CodeIntelOccurrence {
                    range: ByteRange { start: 0, end: 12 },
                    role: CodeIntelRole::Reference,
                    display: "main".to_owned(),
                    definition: vec![CodeIntelLocation {
                        path: target_for_mount.clone(),
                        range: ByteRange { start: 42, end: 48 },
                    }],
                }],
            };
            state.code_intel.update(|map| {
                let file = map
                    .entry(CodeIntelKey {
                        host_id: "h".to_owned(),
                        project_id: ProjectId("p".to_owned()),
                        path: file_path.clone(),
                    })
                    .or_default();
                file.set_rendered_version(ProjectFileVersion(1));
                file.merge_versioned(ProjectFileVersion(1), |d| d.merge_model(model));
            });
            *cap.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <FileView tab_id=TabId(20_030) path=mount_path.clone() /> }
        });

        next_tick().await;
        let state = captured.borrow().clone().unwrap();

        cmd_click_first_line(&container);
        for _ in 0..10 {
            next_tick().await;
        }

        // Local jump: the pushed target offset is stashed for the scroll-snap…
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            Some((target.clone(), 42)),
            "a pushed-definition click should jump locally to the target offset"
        );
        // …and NO on-demand navigate request was sent.
        assert!(
            !navigate_frame_was_sent(),
            "a pushed-definition click must not emit a code_intel_navigate frame"
        );
    }

    /// M6: a large file's model is delivered as transient `ByteRange` + `Partial`
    /// chunks converging on a `FullFile` + `Complete` marker. An occurrence
    /// delivered via a ByteRange chunk (with its target) must render as a normal
    /// clickable decoration — a Cmd/Ctrl+click navigates **locally**, emitting no
    /// `code_intel_navigate` request, exactly as if it had arrived FullFile.
    /// ByteRange is a pacing window, not a second-class delivery.
    #[wasm_bindgen_test]
    async fn cmd_click_on_byte_range_delivered_occurrence_navigates_locally() {
        use crate::state::{ActiveProjectRef, CodeIntelKey};
        use protocol::{
            ByteRange, CodeIntelCompleteness, CodeIntelFileModelPayload, CodeIntelLanguageId,
            CodeIntelLocation, CodeIntelModelRange, CodeIntelOccurrence, CodeIntelProviderId,
            CodeIntelRole, ProjectId,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        ensure_styles_loaded();
        install_send_stub();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        let target = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "lib.rs".to_owned(),
        };
        let content = "fn main() {}";

        let container = make_container();
        let mount_path = path.clone();
        let target_for_mount = target.clone();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let file_path = mount_path.clone();
            state.open_files.update(|files| {
                files.insert(
                    file_path.clone(),
                    OpenFile {
                        path: file_path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            }));
            // The occurrence arrives in a transient ByteRange + Partial chunk,
            // already resolved to a target in lib.rs.
            let chunk = CodeIntelFileModelPayload {
                path: file_path.clone(),
                version: ProjectFileVersion(1),
                provider: CodeIntelProviderId("rust-analyzer".to_owned()),
                language: CodeIntelLanguageId("rust".to_owned()),
                model_range: CodeIntelModelRange::ByteRange {
                    range: ByteRange { start: 0, end: 12 },
                },
                completeness: CodeIntelCompleteness::Partial,
                occurrences: vec![CodeIntelOccurrence {
                    range: ByteRange { start: 0, end: 12 },
                    role: CodeIntelRole::Reference,
                    display: "main".to_owned(),
                    definition: vec![CodeIntelLocation {
                        path: target_for_mount.clone(),
                        range: ByteRange { start: 42, end: 48 },
                    }],
                }],
            };
            // …followed by the FullFile + Complete convergence marker.
            let complete = CodeIntelFileModelPayload {
                path: file_path.clone(),
                version: ProjectFileVersion(1),
                provider: CodeIntelProviderId("rust-analyzer".to_owned()),
                language: CodeIntelLanguageId("rust".to_owned()),
                model_range: CodeIntelModelRange::FullFile,
                completeness: CodeIntelCompleteness::Complete,
                occurrences: vec![],
            };
            state.code_intel.update(|map| {
                let file = map
                    .entry(CodeIntelKey {
                        host_id: "h".to_owned(),
                        project_id: ProjectId("p".to_owned()),
                        path: file_path.clone(),
                    })
                    .or_default();
                file.set_rendered_version(ProjectFileVersion(1));
                file.merge_versioned(ProjectFileVersion(1), |d| d.merge_model(chunk));
                file.merge_versioned(ProjectFileVersion(1), |d| d.merge_model(complete));
            });
            *cap.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <FileView tab_id=TabId(20_032) path=mount_path.clone() /> }
        });

        next_tick().await;
        let state = captured.borrow().clone().unwrap();

        cmd_click_first_line(&container);
        for _ in 0..10 {
            next_tick().await;
        }

        // The ByteRange-delivered target is jumped to locally…
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            Some((target.clone(), 42)),
            "a ByteRange-delivered, resolved occurrence navigates locally"
        );
        // …with no on-demand navigate request.
        assert!(
            !navigate_frame_was_sent(),
            "a ByteRange-delivered pushed definition must not emit a navigate frame"
        );
    }

    /// M3: a Cmd/Ctrl+click on an occurrence whose definition has **not** been
    /// pushed yet falls back to the on-demand `code_intel_navigate` miss-fill.
    #[wasm_bindgen_test]
    async fn cmd_click_on_unresolved_occurrence_falls_back_to_navigate() {
        use crate::state::{ActiveProjectRef, CodeIntelKey};
        use protocol::{
            ByteRange, CodeIntelCompleteness, CodeIntelFileModelPayload, CodeIntelLanguageId,
            CodeIntelModelRange, CodeIntelOccurrence, CodeIntelProviderId, CodeIntelRole,
            ProjectId,
        };

        ensure_styles_loaded();
        install_send_stub();

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
                        version: ProjectFileVersion(1),
                        contents: Some(content.to_owned()),
                        is_binary: false,
                    },
                );
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
            }));
            // The occurrence exists but its definition is still empty (resolving).
            let model = CodeIntelFileModelPayload {
                path: file_path.clone(),
                version: ProjectFileVersion(1),
                provider: CodeIntelProviderId("rust-analyzer".to_owned()),
                language: CodeIntelLanguageId("rust".to_owned()),
                model_range: CodeIntelModelRange::FullFile,
                completeness: CodeIntelCompleteness::Partial,
                occurrences: vec![CodeIntelOccurrence {
                    range: ByteRange { start: 0, end: 12 },
                    role: CodeIntelRole::Reference,
                    display: "main".to_owned(),
                    definition: vec![],
                }],
            };
            state.code_intel.update(|map| {
                let file = map
                    .entry(CodeIntelKey {
                        host_id: "h".to_owned(),
                        project_id: ProjectId("p".to_owned()),
                        path: file_path.clone(),
                    })
                    .or_default();
                file.set_rendered_version(ProjectFileVersion(1));
                file.merge_versioned(ProjectFileVersion(1), |d| d.merge_model(model));
            });
            provide_context(state);
            view! { <FileView tab_id=TabId(20_031) path=mount_path.clone() /> }
        });

        next_tick().await;
        cmd_click_first_line(&container);
        for _ in 0..10 {
            next_tick().await;
        }

        assert!(
            navigate_frame_was_sent(),
            "an unresolved-occurrence click must fall back to a code_intel_navigate frame"
        );
    }

    /// A `code_intel_navigate_result` correlated to the active navigate id
    /// triggers navigation: the target's byte offset is stashed in
    /// `pending_goto_offset` (consumed by the file view's scroll-snap) and the
    /// project read for the target file is kicked off. A superseded result
    /// (stale id) is ignored.
    #[wasm_bindgen_test]
    async fn navigate_result_triggers_goto() {
        use crate::state::{ActiveProjectRef, CodeIntelKey, CodeIntelNavigateContext, OpenFile};
        use protocol::{ByteRange, CodeIntelLocation, CodeIntelNavigateResultPayload, ProjectId};
        use std::cell::RefCell;
        use std::rc::Rc;

        let source = ProjectPath {
            root: ProjectRootPath("r".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        let target = ProjectPath {
            root: ProjectRootPath("r".to_owned()),
            relative_path: "lib.rs".to_owned(),
        };

        // Build the navigate context the dispatch guard requires: a recorded
        // request, the active project unchanged, and the source file still open
        // at the matching rendered version.
        let set_context = {
            let source = source.clone();
            move |state: &AppState, navigate_id: u64| {
                state.active_project.set(Some(ActiveProjectRef {
                    host_id: "h".to_owned(),
                    project_id: ProjectId("p".to_owned()),
                }));
                state
                    .code_intel_navigate_ctx
                    .set(Some(CodeIntelNavigateContext {
                        navigate_id,
                        host_id: "h".to_owned(),
                        project_id: ProjectId("p".to_owned()),
                        path: source.clone(),
                        version: ProjectFileVersion(1),
                    }));
                state.open_files.update(|files| {
                    files.insert(
                        source.clone(),
                        OpenFile {
                            path: source.clone(),
                            version: ProjectFileVersion(1),
                            contents: Some("fn main() {}".to_owned()),
                            is_binary: false,
                        },
                    );
                });
                state.code_intel.update(|map| {
                    map.entry(CodeIntelKey {
                        host_id: "h".to_owned(),
                        project_id: ProjectId("p".to_owned()),
                        path: source.clone(),
                    })
                    .or_default()
                    .set_rendered_version(ProjectFileVersion(1));
                });
            }
        };

        let container = make_container();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *cap.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <div></div> }
        });
        next_tick().await;
        let state = captured.borrow().clone().unwrap();
        set_context(&state, 9);

        let payload = CodeIntelNavigateResultPayload {
            navigate_id: 9,
            path: source.clone(),
            version: ProjectFileVersion(1),
            targets: vec![CodeIntelLocation {
                path: target.clone(),
                range: ByteRange { start: 42, end: 48 },
            }],
        };
        crate::dispatch::apply_code_intel_navigate_result(&state, payload);

        // Navigation: the target byte offset is stashed for the file view's
        // scroll-snap (the open_project_path read is fired via spawn_local).
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            Some((target.clone(), 42)),
            "navigate result should set a pending byte-offset goto to the target"
        );

        // A superseded result (id 1 != the active context id 9) is ignored.
        state.pending_goto_offset.set(None);
        set_context(&state, 9);
        let stale = CodeIntelNavigateResultPayload {
            navigate_id: 1,
            path: source.clone(),
            version: ProjectFileVersion(1),
            targets: vec![CodeIntelLocation {
                path: target.clone(),
                range: ByteRange { start: 5, end: 9 },
            }],
        };
        crate::dispatch::apply_code_intel_navigate_result(&state, stale);
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            None,
            "a superseded navigate result must not trigger navigation"
        );

        // A result that arrives after the user switched projects is dropped even
        // when the id matches.
        set_context(&state, 11);
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "h".to_owned(),
            project_id: ProjectId("other".to_owned()),
        }));
        let after_switch = CodeIntelNavigateResultPayload {
            navigate_id: 11,
            path: source,
            version: ProjectFileVersion(1),
            targets: vec![CodeIntelLocation {
                path: target,
                range: ByteRange { start: 5, end: 9 },
            }],
        };
        crate::dispatch::apply_code_intel_navigate_result(&state, after_switch);
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            None,
            "a navigate result after a project switch must be dropped"
        );
    }

    /// M3: a local jump must **supersede** an earlier in-flight `code_intel_navigate`.
    /// If an unresolved click already sent a navigate (recording its context) and
    /// a later click resolves locally from the pushed model, the local jump clears
    /// the navigate context — so when the stale `code_intel_navigate_result`
    /// finally arrives it is dropped and does NOT yank the cursor to its target.
    #[wasm_bindgen_test]
    async fn local_jump_supersedes_in_flight_navigate() {
        use crate::state::{ActiveProjectRef, CodeIntelKey, CodeIntelNavigateContext};
        use protocol::{
            ByteRange, CodeIntelCompleteness, CodeIntelFileModelPayload, CodeIntelLanguageId,
            CodeIntelLocation, CodeIntelModelRange, CodeIntelNavigateResultPayload,
            CodeIntelOccurrence, CodeIntelProviderId, CodeIntelRole, ProjectId,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        let source = ProjectPath {
            root: ProjectRootPath("r".to_owned()),
            relative_path: "main.rs".to_owned(),
        };
        let local_target = ProjectPath {
            root: ProjectRootPath("r".to_owned()),
            relative_path: "lib.rs".to_owned(),
        };
        let stale_target = ProjectPath {
            root: ProjectRootPath("r".to_owned()),
            relative_path: "other.rs".to_owned(),
        };

        let container = make_container();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *cap.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <div></div> }
        });
        next_tick().await;
        let state = captured.borrow().clone().unwrap();

        state.active_project.set(Some(ActiveProjectRef {
            host_id: "h".to_owned(),
            project_id: ProjectId("p".to_owned()),
        }));
        state.open_files.update(|files| {
            files.insert(
                source.clone(),
                OpenFile {
                    path: source.clone(),
                    version: ProjectFileVersion(1),
                    contents: Some("fn main() {}".to_owned()),
                    is_binary: false,
                },
            );
        });
        // Pushed model: occurrence [0,12) resolved to lib.rs.
        let model = CodeIntelFileModelPayload {
            path: source.clone(),
            version: ProjectFileVersion(1),
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            model_range: CodeIntelModelRange::FullFile,
            completeness: CodeIntelCompleteness::Complete,
            occurrences: vec![CodeIntelOccurrence {
                range: ByteRange { start: 0, end: 12 },
                role: CodeIntelRole::Reference,
                display: "main".to_owned(),
                definition: vec![CodeIntelLocation {
                    path: local_target.clone(),
                    range: ByteRange {
                        start: 100,
                        end: 104,
                    },
                }],
            }],
        };
        state.code_intel.update(|map| {
            let file = map
                .entry(CodeIntelKey {
                    host_id: "h".to_owned(),
                    project_id: ProjectId("p".to_owned()),
                    path: source.clone(),
                })
                .or_default();
            file.set_rendered_version(ProjectFileVersion(1));
            file.merge_versioned(ProjectFileVersion(1), |d| d.merge_model(model));
        });
        // An earlier unresolved click already sent a navigate (id 7) and recorded
        // its context.
        state
            .code_intel_navigate_ctx
            .set(Some(CodeIntelNavigateContext {
                navigate_id: 7,
                host_id: "h".to_owned(),
                project_id: ProjectId("p".to_owned()),
                path: source.clone(),
                version: ProjectFileVersion(1),
            }));

        // A later click resolves LOCALLY from the pushed model.
        crate::actions::navigate_to_definition(&state, source.clone(), ProjectFileVersion(1), 3);

        // The local jump happened and superseded the in-flight navigate context.
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            Some((local_target.clone(), 100)),
            "local jump should target the pushed definition"
        );
        assert!(
            state.code_intel_navigate_ctx.get_untracked().is_none(),
            "a successful local jump must clear the in-flight navigate context"
        );

        // The stale navigate result (old id 7) now arrives late.
        crate::dispatch::apply_code_intel_navigate_result(
            &state,
            CodeIntelNavigateResultPayload {
                navigate_id: 7,
                path: source.clone(),
                version: ProjectFileVersion(1),
                targets: vec![CodeIntelLocation {
                    path: stale_target.clone(),
                    range: ByteRange { start: 5, end: 9 },
                }],
            },
        );

        // It must be dropped: the cursor stays on the local target, not yanked to
        // the stale one.
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            Some((local_target, 100)),
            "a late navigate result for a superseded click must not move the cursor"
        );
    }

    /// A hover result fills the popover seeded at request time and the
    /// `HoverPopover` component renders the markdown near the captured anchor.
    #[wasm_bindgen_test]
    async fn hover_result_renders_popover_near_anchor() {
        use crate::components::hover_popover::HoverPopover;
        use crate::state::HoverPopover as HoverPopoverState;
        use std::cell::RefCell;
        use std::rc::Rc;

        ensure_styles_loaded();

        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "main.rs".to_owned(),
        };

        let container = make_container();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let popover_path = path.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Seed a popover as if a hover request fired and its result landed.
            state.code_intel_active_hover.set(3);
            state.code_intel_hover.set(Some(HoverPopoverState {
                hover_id: 3,
                path: popover_path.clone(),
                version: ProjectFileVersion(1),
                offset: 0,
                anchor_left: 120.0,
                anchor_top: 40.0,
                anchor_bottom: 58.0,
                contents: Some("**Type**: `u32`".to_owned()),
            }));
            *cap.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <HoverPopover /> }
        });

        next_tick().await;

        let popover = container
            .query_selector(".code-intel-hover-popover")
            .unwrap()
            .expect("hover popover should render when contents are present");
        let text = popover.text_content().unwrap_or_default();
        assert!(
            text.contains("Type") && text.contains("u32"),
            "popover should render the hover markdown; got {text:?}"
        );
        // Positioned from the captured anchor (left edge of the hovered span).
        let style = popover.get_attribute("style").unwrap_or_default();
        assert!(
            style.contains("left: 120px"),
            "popover should be anchored at the span's left; style was {style:?}"
        );

        // Dismissing clears the popover from the DOM.
        let state = captured.borrow().clone().unwrap();
        crate::actions::dismiss_hover(&state);
        next_tick().await;
        assert!(
            container
                .query_selector(".code-intel-hover-popover")
                .unwrap()
                .is_none(),
            "dismiss should remove the popover"
        );
    }
}
