use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::components::find_bar::{FindBar, FindState};
use crate::state::{AppState, TabContent};
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

/// How many lines to highlight per yield slice when async-highlighting a
/// large file. Small enough that the browser stays responsive (each slice
/// is ~1ms in release, ~30ms in debug for a typical Rust file); large
/// enough that the per-yield overhead doesn't dominate. Tune if you see
/// stuttering scroll during initial load.
const HIGHLIGHT_CHUNK_LINES: usize = 200;

/// Above this line count we never highlight — the wasm main thread can't
/// realistically tokenize that much without freezing the UI even with
/// chunking. Mirrors `syntax_highlight::MAX_LINES_TO_HIGHLIGHT`.
const HIGHLIGHT_LINE_CAP: usize = 5000;

#[component]
pub fn FileView(path: ProjectPath) -> impl IntoView {
    let state = expect_context::<AppState>();

    let file_path = path.clone();
    let file_info = move || {
        state
            .open_files
            .with(|files| files.get(&file_path).cloned())
    };

    let close_path = path.clone();

    view! {
        <div class="file-view">
            {move || {
                let close_path = close_path.clone();
                match file_info() {
                    Some(f) => {
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
                        let (find_state, lines) = FindState::from_file(&content);
                        let total = lines.len();
                        provide_context(find_state.clone());

                        // Async syntax highlighting. We don't tokenize on
                        // mount — that would block first paint for ~1s on a
                        // moderate file in debug builds. Instead we kick off
                        // a spawn_local task that fills tokens into a signal
                        // in `HIGHLIGHT_CHUNK_LINES`-sized chunks, yielding
                        // to the browser between chunks. Each row reads its
                        // own index from the signal, so visible lines render
                        // plain text immediately and "fill in" with color
                        // over the next ~hundred ms as chunks land.
                        //
                        // Persistent syntect parser state across chunks is
                        // critical: it's how multi-line constructs (block
                        // comments, raw strings) still color correctly even
                        // though we're processing the file in pieces.
                        let highlighted: ArcRwSignal<Vec<Option<LineTokens>>> =
                            ArcRwSignal::new(vec![None; total]);

                        // Generation counter for live re-highlighting on
                        // theme change. Bumping this invalidates any
                        // in-flight chunked task, which checks the
                        // generation each chunk and exits if stale.
                        let highlight_gen: ArcRwSignal<u32> = ArcRwSignal::new(0);

                        let path_for_effect = f.path.relative_path.clone();
                        let syntax_theme = state.syntax_theme;
                        let lines_for_effect = lines.clone();
                        let highlighted_for_effect = highlighted.clone();
                        let gen_for_effect = highlight_gen.clone();
                        Effect::new(move |_| {
                            // Subscribe to the theme signal. Whenever it
                            // changes we discard prior tokens and kick off
                            // a fresh chunked highlight pass with the new
                            // theme.
                            let _ = syntax_theme.get();

                            let my_gen = gen_for_effect.get_untracked() + 1;
                            gen_for_effect.set(my_gen);

                            // Reset highlighted vec to plain text while we
                            // re-tokenize. Visible rows momentarily render
                            // plain, then fill in with the new theme.
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

                            let lines_for_task = lines_for_effect.clone();
                            let signal_for_task = highlighted_for_effect.clone();
                            let gen_for_task = gen_for_effect.clone();
                            spawn_local(async move {
                                let mut hl = LineHighlighter::new(syntax);
                                let mut i = 0usize;
                                while i < lines_for_task.len() {
                                    if gen_for_task.get_untracked() != my_gen {
                                        return; // newer task superseded us
                                    }
                                    let end = (i + HIGHLIGHT_CHUNK_LINES)
                                        .min(lines_for_task.len());
                                    let chunk_tokens: Vec<LineTokens> = (i..end)
                                        .map(|j| hl.highlight_one(lines_for_task.line(j)))
                                        .collect();
                                    if gen_for_task.get_untracked() != my_gen {
                                        return;
                                    }
                                    signal_for_task.update(|v| {
                                        for (offset, toks) in
                                            chunk_tokens.into_iter().enumerate()
                                        {
                                            if let Some(slot) = v.get_mut(i + offset) {
                                                *slot = Some(toks);
                                            }
                                        }
                                    });
                                    i = end;
                                    yield_to_browser().await;
                                }
                            });
                        });

                        let pre_ref: NodeRef<leptos::html::Pre> = NodeRef::new();

                        // Virtualization geometry. Pre-seed the line and
                        // viewport heights with reasonable estimates so the
                        // very first render of a large file already uses a
                        // bounded window. The measurement Effect below
                        // refines both values once layout is real.
                        let scroll_top = RwSignal::new(0.0_f64);
                        let viewport_height =
                            RwSignal::new(INITIAL_VIEWPORT_HEIGHT_ESTIMATE);
                        let line_height = RwSignal::new(INITIAL_LINE_HEIGHT_ESTIMATE);

                        // Measure the geometry once after first paint. The
                        // Effect re-runs if the underlying signals fire
                        // (rare here — only the initial mount).
                        Effect::new(move |_| {
                            let Some(el) = pre_ref.get() else { return };
                            let vh = el.client_height() as f64;
                            if vh > 0.0 {
                                viewport_height.set(vh);
                            }
                            if let Ok(Some(line_el)) = el.query_selector(".file-line")
                                && let Some(html_el) =
                                    line_el.dyn_ref::<web_sys::HtmlElement>()
                            {
                                let lh = html_el.offset_height() as f64;
                                if lh > 0.0 && (line_height.get_untracked() - lh).abs() > 0.5 {
                                    line_height.set(lh);
                                }
                            }
                        });

                        // Throttle to one update per animation frame so the
                        // visible_window memo only invalidates once per paint
                        // even on a high-DPI trackpad firing scroll events
                        // hundreds of times per second.
                        let scroll_pending = std::rc::Rc::new(std::cell::Cell::new(false));
                        let on_scroll = {
                            let pending = scroll_pending.clone();
                            move |_: web_sys::Event| {
                                if pending.get() {
                                    return;
                                }
                                pending.set(true);
                                let pending = pending.clone();
                                leptos::prelude::request_animation_frame(move || {
                                    pending.set(false);
                                    if let Some(el) = pre_ref.get() {
                                        scroll_top.set(el.scroll_top() as f64);
                                    }
                                });
                            }
                        };

                        // Visible window in line-index space. Small files
                        // render everything (start=0, end=total) so spacers
                        // stay at 0px and the pre-virtualization DOM shape
                        // is preserved. Larger files use the seeded
                        // (then measured) line_height to bound the window
                        // from the very first render.
                        let visible_window: Memo<(usize, usize)> =
                            Memo::new(move |_| {
                                if total < VIRTUALIZE_THRESHOLD {
                                    return (0, total);
                                }
                                let lh = line_height.get();
                                let st = scroll_top.get();
                                let vh = viewport_height.get();
                                let start_f =
                                    ((st - OVERSCAN_LINES * lh) / lh).floor().max(0.0);
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
                                                attr:data-find-idx=i
                                            >
                                                <span class="file-line-num">{i + 1}</span>
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
                        }.into_any()
                    }
                    None => view! {
                        <div class="panel-empty">"No file open"</div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

/// Yield once to the browser event loop. Wasm has no real background
/// threads; this is the closest we get to "let the page paint." Used between
/// syntect highlight chunks so the UI stays responsive on large files.
async fn yield_to_browser() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
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
            view! { <FileView path=mount_path.clone() /> }
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

        // Text guard: each row's rendered text should equal the line number
        // followed by the source line, with no extra characters. Catches the
        // class of bug where stray characters (e.g. a trailing "\n") leak into
        // the rendered output — which is the original double-spacing
        // regression we hit. Text-equality is the right assertion shape here:
        // the user perceives "what text appears in the row," and any character
        // we didn't intend to add is a defect regardless of whether the
        // headless renderer expands the row's pixel height for it.
        let expected = [
            "line one",
            "line two",
            "line three",
            "line four",
            "line five",
        ];
        for (i, row) in rows.iter().enumerate() {
            let text = row.text_content().unwrap_or_default();
            let want = format!("{}{}", i + 1, expected[i]);
            assert_eq!(
                text, want,
                "row {i} rendered text does not match source line exactly"
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
            view! { <FileView path=mount_path.clone() /> }
        });
        next_tick().await;

        let nodes = container
            .query_selector_all(".file-line code span[style]")
            .unwrap();
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
            view! { <FileView path=mount_path.clone() /> }
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
}
