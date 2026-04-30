use std::cell::RefCell;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;

use crate::components::find_bar::{FindBar, FindState, render_text_with_highlights};
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, DiffViewState};
use crate::syntax_highlight::{LineTokens, color_to_css, compute_hunk_tokens, syntax_for_path};

use protocol::{
    DiffContextMode, FrameKind, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectPath, ProjectReadDiffPayload,
    ProjectRootPath, ProjectStageHunkPayload, StreamPath,
};

const SBS_MIN_FRACTION: f64 = 0.05;
const SBS_MAX_FRACTION: f64 = 0.95;

/// Initial estimates used by virtualization until the first measurement
/// effect refines them. Match `file_view`'s constants so behavior is
/// consistent across the two views.
const INITIAL_LINE_HEIGHT_ESTIMATE: f64 = 18.0;
const INITIAL_VIEWPORT_HEIGHT_ESTIMATE: f64 = 600.0;
/// Buffer rows rendered outside the visible viewport on each side.
const OVERSCAN_LINES: f64 = 40.0;
/// Below this rendered-row total we render every row up front (no spacers,
/// no scroll math). Keeps the small-diff path identical in DOM shape and
/// preserves layout assertions for tiny test diffs.
const VIRTUALIZE_THRESHOLD: usize = 200;

/// Geometry signals shared between `DiffContent` and any descendant that
/// needs to virtualize its rows against the global scroll position. The
/// scroll signal is updated by `DiffContent`'s `on:scroll`; size signals
/// are refined by a measurement effect on first paint.
#[derive(Clone, Copy)]
struct DiffScroll {
    scroll_top: RwSignal<f64>,
    viewport_height: RwSignal<f64>,
    line_height: RwSignal<f64>,
}

#[component]
pub fn DiffView(root: ProjectRootPath, scope: ProjectDiffScope) -> impl IntoView {
    let state = expect_context::<AppState>();

    let key = (root.clone(), scope);
    let diff_key = key.clone();
    let diff = move || {
        state
            .diff_contents
            .with(|diffs| diffs.get(&diff_key).cloned())
    };

    // Reactive effect: when the context-mode signal differs from the stored
    // entry's requested mode, dispatch a fresh ProjectReadDiff. The stored
    // entry (created by `git_panel::view_diff` before the first response
    // arrives) is the authority on "what was last requested", so this works
    // even during an in-flight initial request: the pending entry is mutated
    // to reflect the new mode, and the dispatch reducer will reject any
    // response whose `context_mode` doesn't match.
    let effect_state = state.clone();
    let effect_key = key.clone();
    Effect::new(move |_| {
        let signal_mode = effect_state.diff_context_mode.get();
        let Some(current) = effect_state
            .diff_contents
            .with(|diffs| diffs.get(&effect_key).cloned())
        else {
            return;
        };
        if current.context_mode == signal_mode {
            return;
        }
        let Some(active_project) = effect_state.active_project_ref_untracked() else {
            return;
        };
        let project_id = active_project.project_id.clone();
        let stream = StreamPath(format!("/project/{}", project_id.0));
        let root = current.root.clone();
        let scope = current.scope;
        let path = current.path.clone();

        let update_key = effect_key.clone();
        let root_for_update = root.clone();
        let path_for_update = path.clone();
        effect_state.diff_contents.update(|diffs| {
            let previous = diffs.get(&update_key);
            let next = DiffViewState::for_request(
                previous,
                root_for_update,
                scope,
                path_for_update,
                signal_mode,
            );
            diffs.insert(update_key, next);
        });

        let payload = ProjectReadDiffPayload {
            root,
            scope,
            path,
            context_mode: signal_mode,
        };
        let host_id = active_project.host_id.clone();
        spawn_local(async move {
            if let Err(e) = send_frame(&host_id, stream, FrameKind::ProjectReadDiff, &payload).await
            {
                log::error!("failed to send ProjectReadDiff on context-mode change: {e}");
            }
        });
    });

    view! {
        <div class="diff-view">
            <DiffToolbar />
            {move || match diff() {
                Some(dv) if dv.pending && dv.files.is_empty() => view! {
                    <div class="diff-empty">
                        <p class="placeholder-text">"Loading diff…"</p>
                    </div>
                }.into_any(),
                Some(dv) => view! { <DiffContent diff=dv /> }.into_any(),
                None => view! {
                    <div class="diff-empty">
                        <p class="placeholder-text">"Select a file to view its diff"</p>
                    </div>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn DiffToolbar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let view_mode = state.diff_view_mode;
    let context_mode = state.diff_context_mode;

    view! {
        <div class="diff-toolbar">
            <div class="diff-toolbar-group">
                <span class="diff-toolbar-label">"Layout"</span>
                <div class="settings-segmented-control">
                    <button
                        class=move || if view_mode.get() == DiffViewMode::Unified { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_view_mode(view_mode, DiffViewMode::Unified)
                    >
                        "Unified"
                    </button>
                    <button
                        class=move || if view_mode.get() == DiffViewMode::SideBySide { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_view_mode(view_mode, DiffViewMode::SideBySide)
                    >
                        "Side by Side"
                    </button>
                </div>
            </div>
            <div class="diff-toolbar-group">
                <span class="diff-toolbar-label">"Context"</span>
                <div class="settings-segmented-control">
                    <button
                        class=move || if context_mode.get() == DiffContextMode::Hunks { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_context_mode(context_mode, DiffContextMode::Hunks)
                    >
                        "Hunks"
                    </button>
                    <button
                        class=move || if context_mode.get() == DiffContextMode::FullFile { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_context_mode(context_mode, DiffContextMode::FullFile)
                    >
                        "Full File"
                    </button>
                </div>
            </div>
        </div>
    }
}

fn set_diff_view_mode(signal: RwSignal<DiffViewMode>, mode: DiffViewMode) {
    signal.set(mode);
    crate::components::settings_panel::persist_diff_view_mode(mode);
}

fn set_diff_context_mode(signal: RwSignal<DiffContextMode>, mode: DiffContextMode) {
    signal.set(mode);
    crate::components::settings_panel::persist_diff_context_mode(mode);
}

fn stage_hunk(root: ProjectRootPath, relative_path: String, hunk_id: String) {
    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        let payload = ProjectStageHunkPayload {
            path: ProjectPath {
                root,
                relative_path,
            },
            hunk_id,
        };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectStageHunk,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectStageHunk: {e}");
        }
    });
}

#[component]
fn DiffContent(diff: DiffViewState) -> impl IntoView {
    let state = expect_context::<AppState>();
    let scope_label = match diff.scope {
        ProjectDiffScope::Staged => "staged",
        ProjectDiffScope::Unstaged => "unstaged",
    };

    // Flatten all diff lines into a searchable list and compute per-hunk
    // starting offsets so each rendered line knows its flat search index.
    let mut searchable_lines: Vec<String> = Vec::new();
    let mut file_hunk_offsets: Vec<Vec<usize>> = Vec::new();
    for file in &diff.files {
        let mut hunk_offsets = Vec::new();
        for hunk in &file.hunks {
            hunk_offsets.push(searchable_lines.len());
            for line in &hunk.lines {
                searchable_lines.push(line.text.clone());
            }
        }
        file_hunk_offsets.push(hunk_offsets);
    }

    let find_state = FindState::new(searchable_lines);
    provide_context(find_state);

    // Compute each file's rendered-row offset (its row index in the global
    // virtual scrolling space). A "rendered row" is anything taking up a
    // line of vertical real estate: file header, hunk header (only in
    // Hunks mode), or one diff line. SBS pair rows count as one rendered
    // row even though they show two cells.
    //
    // KNOWN LIMITATION (from codex review): the row-height model treats
    // file/hunk headers as if they have the same height as a diff line.
    // They don't, so spacer math accumulates a small drift across many
    // files. Overscan hides most of it; a future fix is to count distinct
    // height classes or pin all rows to a uniform line-height.
    let mut file_rendered_offsets: Vec<usize> = Vec::with_capacity(diff.files.len());
    let mut acc: usize = 0;
    for file in &diff.files {
        file_rendered_offsets.push(acc);
        acc += rendered_rows_for_file(file, diff.context_mode);
    }

    // Scroll geometry. Pre-seed line/viewport height estimates so the
    // visible window is bounded from the very first paint, before the
    // measurement effect runs. Mirrors `file_view` exactly.
    let scroll_top = RwSignal::new(0.0_f64);
    let viewport_height = RwSignal::new(INITIAL_VIEWPORT_HEIGHT_ESTIMATE);
    let line_height = RwSignal::new(INITIAL_LINE_HEIGHT_ESTIMATE);
    let scroll_ctx = DiffScroll {
        scroll_top,
        viewport_height,
        line_height,
    };
    provide_context(scroll_ctx);

    let scroll_ref: NodeRef<leptos::html::Div> = NodeRef::new();

    // Measure the geometry once after first paint. Re-runs are cheap; we
    // only update the signal if the measured value differs meaningfully.
    Effect::new(move |_| {
        let Some(el) = scroll_ref.get() else { return };
        let vh = el.client_height() as f64;
        if vh > 0.0 && (viewport_height.get_untracked() - vh).abs() > 0.5 {
            viewport_height.set(vh);
        }
        if let Ok(Some(line_el)) = el.query_selector(".diff-line")
            && let Some(html_el) = line_el.dyn_ref::<web_sys::HtmlElement>()
        {
            let lh = html_el.offset_height() as f64;
            if lh > 0.0 && (line_height.get_untracked() - lh).abs() > 0.5 {
                line_height.set(lh);
            }
        }
    });

    let on_scroll = move |_: web_sys::Event| {
        if let Some(el) = scroll_ref.get() {
            scroll_top.set(el.scroll_top() as f64);
        }
    };

    view! {
        <div class="diff-content" node_ref=scroll_ref on:scroll=on_scroll>
            {move || {
                if state.find_bar_open.get() {
                    Some(view! { <FindBar /> })
                } else {
                    None
                }
            }}
            {diff.files.into_iter().enumerate().map(|(fi, file)| {
                let hunk_offsets = file_hunk_offsets[fi].clone();
                let root = diff.root.clone();
                let rendered_offset = file_rendered_offsets[fi];
                view! { <DiffFileView file=file scope_label=scope_label scope=diff.scope root=root context_mode=diff.context_mode hunk_offsets=hunk_offsets rendered_offset=rendered_offset /> }
            }).collect::<Vec<_>>()}
        </div>
    }
}

/// Count how many vertical rows a single file's rendering takes: the file
/// header + (per hunk: optional hunk header + every line). Used to lay out
/// each file at a known position in the global virtual scroll space.
fn rendered_rows_for_file(file: &ProjectGitDiffFile, context_mode: DiffContextMode) -> usize {
    let mut total = 1; // file header
    for hunk in &file.hunks {
        if context_mode == DiffContextMode::Hunks {
            total += 1;
        }
        // SBS pairing collapses some lines but worst-case (no pairing
        // overlap) is 1 paired-row per source line, so use the line count
        // as the upper bound. Slight over-estimate is fine — it just means
        // a few extra pixels of bottom padding in SBS mode.
        total += hunk.lines.len();
    }
    total
}

#[component]
fn DiffFileView(
    file: ProjectGitDiffFile,
    scope_label: &'static str,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    context_mode: DiffContextMode,
    hunk_offsets: Vec<usize>,
    rendered_offset: usize,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let view_mode_sig = state.diff_view_mode;
    let relative_path = file.relative_path.clone();

    let initial_split = match classify_diff_file(&file) {
        DiffFileKind::PureAdd => 0.0,
        DiffFileKind::PureDelete => 1.0,
        DiffFileKind::Mixed => 0.5,
    };
    let split = RwSignal::new(initial_split);

    on_cleanup(clear_sbs_drag_listeners);

    let file_for_view = file.clone();
    let offsets_for_view = hunk_offsets.clone();
    let root_for_view = root.clone();
    let path_for_view = relative_path.clone();

    view! {
        <div class="diff-file">
            <div class="diff-file-header">
                <span class="diff-file-path">{file.relative_path}</span>
                <span class="diff-scope-badge">{scope_label}</span>
            </div>
            {move || match view_mode_sig.get() {
                DiffViewMode::Unified => render_unified_virtualized(
                    file_for_view.clone(),
                    offsets_for_view.clone(),
                    context_mode,
                    scope,
                    root_for_view.clone(),
                    path_for_view.clone(),
                    rendered_offset,
                ),
                DiffViewMode::SideBySide => render_sbs_panes(
                    file_for_view.clone(),
                    offsets_for_view.clone(),
                    context_mode,
                    scope,
                    root_for_view.clone(),
                    path_for_view.clone(),
                    split,
                ),
            }}
        </div>
    }
}

fn render_unified_hunks(
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
) -> AnyView {
    view! {
        <div class="diff-hunks">
            {file.hunks.into_iter().enumerate().map(|(hi, hunk)| {
                let offset = hunk_offsets[hi];
                view! {
                    <UnifiedHunk hunk=hunk context_mode=context_mode line_offset=offset scope=scope root=root.clone() relative_path=relative_path.clone() />
                }
            }).collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

/// One vertical row inside a virtualized unified-diff file. The file
/// header sits at index 0 in `rendered_offset` space (one above this list,
/// rendered by `DiffFileView` itself), so the row indices here begin at 1
/// for the first hunk header (or first line in FullFile mode). Each
/// variant carries the indices it needs to render on demand.
#[derive(Clone)]
enum UnifiedFileItem {
    HunkHeader { hi: usize },
    Line { hi: usize, li: usize },
}

/// Build the per-file `Vec<UnifiedFileItem>` once. Item index i corresponds
/// to rendered row `rendered_offset + 1 + i` in the global virtual scroll
/// space (the `+1` is for the file header rendered separately above).
fn build_unified_file_items(
    file: &ProjectGitDiffFile,
    context_mode: DiffContextMode,
) -> Vec<UnifiedFileItem> {
    let mut items = Vec::new();
    for (hi, hunk) in file.hunks.iter().enumerate() {
        if context_mode == DiffContextMode::Hunks {
            items.push(UnifiedFileItem::HunkHeader { hi });
        }
        for li in 0..hunk.lines.len() {
            items.push(UnifiedFileItem::Line { hi, li });
        }
    }
    items
}

/// Per-file unified virtualized renderer. Reads the global `DiffScroll`
/// context, computes its own visible window clipped to its row range, and
/// emits top/bottom spacers + a `<For>` over visible items so off-screen
/// rows incur zero DOM/paint cost. Falls through to the non-virtualized
/// path for tiny files (preserves DOM shape for layout assertions).
fn render_unified_virtualized(
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    rendered_offset: usize,
) -> AnyView {
    let items = Arc::new(build_unified_file_items(&file, context_mode));
    let total_items = items.len();

    if total_items < VIRTUALIZE_THRESHOLD {
        // Small file: render every row up front. Avoids spacer math and
        // keeps the DOM identical to the pre-virtualization path.
        return render_unified_hunks(file, hunk_offsets, context_mode, scope, root, relative_path);
    }

    let scroll = expect_context::<DiffScroll>();
    let file_arc = Arc::new(file);
    let hunk_offsets = Arc::new(hunk_offsets);

    // Pre-compute syntax tokens per hunk once for the file so visible-row
    // rendering is just an index lookup. Done eagerly here; for very large
    // files we could chunk this with `spawn_local` like file_view does.
    let syntax = syntax_for_path(&relative_path);
    let mut hunk_tokens: Vec<Vec<Option<LineTokens>>> = Vec::with_capacity(file_arc.hunks.len());
    for hunk in &file_arc.hunks {
        let tokens = match syntax {
            Some(syn) => compute_hunk_tokens(hunk, syn),
            None => vec![None; hunk.lines.len()],
        };
        hunk_tokens.push(tokens);
    }
    let hunk_tokens = Arc::new(hunk_tokens);

    let visible_window: Memo<(usize, usize)> = Memo::new(move |_| {
        let lh = scroll.line_height.get().max(1.0);
        let st = scroll.scroll_top.get();
        let vh = scroll.viewport_height.get();
        // File header sits at row `rendered_offset`; items occupy
        // `rendered_offset + 1 ..= rendered_offset + total_items`. Map
        // global scroll into local item indices.
        let file_first_item_row = rendered_offset + 1;
        let global_visible_first = ((st - OVERSCAN_LINES * lh) / lh).floor().max(0.0) as i64;
        let global_visible_last = ((st + vh + OVERSCAN_LINES * lh) / lh).ceil() as i64;
        let local_start = (global_visible_first - file_first_item_row as i64).max(0) as usize;
        let local_end = (global_visible_last - file_first_item_row as i64).max(0) as usize;
        let local_start = local_start.min(total_items);
        let local_end = local_end.min(total_items);
        (local_start, local_end)
    });

    let items_for_each = items.clone();
    let lh_for_top = scroll.line_height;
    let lh_for_bottom = scroll.line_height;
    let window_for_top = visible_window;
    let window_for_bottom = visible_window;

    view! {
        <div class="diff-hunks">
            {move || {
                let (start, _) = window_for_top.get();
                let h = start as f64 * lh_for_top.get();
                (h > 0.0).then(|| view! {
                    <div class="diff-virt-spacer" style=format!("height: {h}px;")></div>
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
                    let item = items_for_each[i].clone();
                    let file = file_arc.clone();
                    let hunk_offsets = hunk_offsets.clone();
                    let hunk_tokens = hunk_tokens.clone();
                    let root = root.clone();
                    let path = relative_path.clone();
                    render_unified_item(
                        &item,
                        file.as_ref(),
                        hunk_offsets.as_ref(),
                        hunk_tokens.as_ref(),
                        context_mode,
                        scope,
                        &root,
                        &path,
                    )
                }
            </For>
            {move || {
                let (_, end) = window_for_bottom.get();
                let h = total_items.saturating_sub(end) as f64 * lh_for_bottom.get();
                (h > 0.0).then(|| view! {
                    <div class="diff-virt-spacer" style=format!("height: {h}px;")></div>
                })
            }}
        </div>
    }
    .into_any()
}

/// Render a single item from the per-file unified items list.
#[allow(clippy::too_many_arguments)]
fn render_unified_item(
    item: &UnifiedFileItem,
    file: &ProjectGitDiffFile,
    hunk_offsets: &[usize],
    hunk_tokens: &[Vec<Option<LineTokens>>],
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: &ProjectRootPath,
    relative_path: &str,
) -> AnyView {
    match *item {
        UnifiedFileItem::HunkHeader { hi } => {
            let hunk = &file.hunks[hi];
            let header = hunk_header_label(hunk);
            let show_stage =
                scope == ProjectDiffScope::Unstaged && context_mode == DiffContextMode::Hunks;
            let stage_btn = if show_stage {
                let r = root.clone();
                let p = relative_path.to_owned();
                let h = hunk.hunk_id.clone();
                Some(view! {
                    <button
                        class="diff-hunk-stage-btn"
                        title="Stage hunk"
                        on:click=move |_| stage_hunk(r.clone(), p.clone(), h.clone())
                    >
                        "+"
                    </button>
                })
            } else {
                None
            };
            view! {
                <div class="diff-hunk-header">
                    {header}
                    {stage_btn}
                </div>
            }
            .into_any()
        }
        UnifiedFileItem::Line { hi, li } => {
            let hunk = &file.hunks[hi];
            let line = &hunk.lines[li];
            let search_idx = hunk_offsets[hi] + li;
            let base_class = line_class(line.kind);
            let prefix = line_prefix(line.kind);
            let old_str = line
                .old_line_number
                .map(|n| n.to_string())
                .unwrap_or_default();
            let new_str = line
                .new_line_number
                .map(|n| n.to_string())
                .unwrap_or_default();
            let text = line.text.clone();
            let tokens = hunk_tokens[hi][li].clone();
            let find = use_context::<FindState>();
            let find_for_class = find.clone();
            let find_for_text = find;
            view! {
                <div
                    class=move || diff_line_class(base_class, search_idx, &find_for_class)
                    attr:data-find-idx=search_idx
                >
                    <span class="diff-gutter diff-gutter-old">{old_str}</span>
                    <span class="diff-gutter diff-gutter-new">{new_str}</span>
                    <span class="diff-prefix">{prefix}</span>
                    {move || render_diff_text(&text, tokens.as_ref(), search_idx, &find_for_text)}
                </div>
            }
            .into_any()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SbsSide {
    Left,
    Right,
}

/// Per-hunk dual highlight: `(old_per_line, new_per_line)`.
type DualHunkTokens = (Vec<Option<LineTokens>>, Vec<Option<LineTokens>>);

fn render_sbs_panes(
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    split: RwSignal<f64>,
) -> AnyView {
    let pair_ref = NodeRef::<leptos::html::Div>::new();

    // Compute per-hunk dual syntax tokens **once** for the file, then share
    // between left and right panes. Without this both panes would re-tokenize
    // independently, doubling syntect work and amplifying first-render stall.
    let syntax = syntax_for_path(&relative_path);
    let hunk_tokens: Vec<DualHunkTokens> = file
        .hunks
        .iter()
        .map(|hunk| match syntax {
            Some(syn) => crate::syntax_highlight::compute_hunk_tokens_dual(hunk, syn),
            None => (vec![None; hunk.lines.len()], vec![None; hunk.lines.len()]),
        })
        .collect();

    let file_left = file.clone();
    let file_right = file.clone();
    let offsets_left = hunk_offsets.clone();
    let offsets_right = hunk_offsets.clone();
    let root_left = root.clone();
    let root_right = root.clone();
    let path_left = relative_path.clone();
    let path_right = relative_path.clone();
    let tokens_left = hunk_tokens.clone();
    let tokens_right = hunk_tokens;

    let left_style = move || {
        let pct = (split.get() * 100.0).clamp(0.0, 100.0);
        format!("flex: 0 0 {pct:.4}%")
    };

    let on_divider_mousedown = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        start_divider_drag(pair_ref, split);
    };

    view! {
        <div class="diff-pair" node_ref=pair_ref>
            <div class="diff-pane diff-pane-left" style=left_style>
                {render_sbs_pane_content(
                    SbsSide::Left,
                    file_left,
                    offsets_left,
                    context_mode,
                    scope,
                    root_left,
                    path_left,
                    tokens_left,
                )}
            </div>
            <div
                class="diff-divider"
                title="Drag to resize"
                on:mousedown=on_divider_mousedown
            ></div>
            <div class="diff-pane diff-pane-right">
                {render_sbs_pane_content(
                    SbsSide::Right,
                    file_right,
                    offsets_right,
                    context_mode,
                    scope,
                    root_right,
                    path_right,
                    tokens_right,
                )}
            </div>
        </div>
    }
    .into_any()
}

#[allow(clippy::too_many_arguments)]
fn render_sbs_pane_content(
    side: SbsSide,
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    hunk_tokens: Vec<DualHunkTokens>,
) -> AnyView {
    let find = use_context::<FindState>();
    let show_stage_btn = scope == ProjectDiffScope::Unstaged && side == SbsSide::Right;
    let show_header = context_mode == DiffContextMode::Hunks;

    let mut hunk_views: Vec<AnyView> = Vec::with_capacity(file.hunks.len());
    for (hi, hunk) in file.hunks.into_iter().enumerate() {
        let line_offset = hunk_offsets[hi];
        let header = hunk_header_label(&hunk);
        let hunk_id = hunk.hunk_id.clone();
        let lines = hunk.lines.clone();
        let indices = sbs_search_indices(&lines, line_offset);
        let (old_tokens, new_tokens) = hunk_tokens[hi].clone();
        let rows = pair_lines_side_by_side_with_tokens(lines, old_tokens, new_tokens);

        let cell_views: Vec<AnyView> = rows
            .into_iter()
            .zip(indices)
            .map(|(row, (left_idx, right_idx))| {
                let (cell, idx) = match side {
                    SbsSide::Left => (row.left, left_idx),
                    SbsSide::Right => (row.right, right_idx),
                };
                render_cell_search(cell, idx, find.clone())
            })
            .collect();

        let header_view = if show_header {
            let stage_btn = if show_stage_btn {
                let r = root.clone();
                let p = relative_path.clone();
                let h = hunk_id.clone();
                Some(view! {
                    <button
                        class="diff-hunk-stage-btn"
                        title="Stage hunk"
                        on:click=move |_| stage_hunk(r.clone(), p.clone(), h.clone())
                    >
                        "+"
                    </button>
                })
            } else {
                None
            };
            Some(view! {
                <div class="diff-hunk-header">
                    {header}
                    {stage_btn}
                </div>
            })
        } else {
            None
        };

        hunk_views.push(
            view! {
                <div class="diff-hunk diff-hunk-side-by-side">
                    {header_view}
                    {cell_views}
                </div>
            }
            .into_any(),
        );
    }

    hunk_views.into_any()
}

thread_local! {
    static SBS_DRAG_LISTENERS: RefCell<Option<SbsDragListeners>> = const { RefCell::new(None) };
}

struct SbsDragListeners {
    window: web_sys::Window,
    mousemove: Closure<dyn Fn(web_sys::MouseEvent)>,
    mouseup: Closure<dyn Fn(web_sys::MouseEvent)>,
}

impl SbsDragListeners {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback(
            "mousemove",
            self.mousemove.as_ref().unchecked_ref(),
        );
        let _ = self
            .window
            .remove_event_listener_with_callback("mouseup", self.mouseup.as_ref().unchecked_ref());
    }
}

fn clear_sbs_drag_listeners() {
    SBS_DRAG_LISTENERS.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            handle.remove();
        }
    });
}

fn start_divider_drag(pair_ref: NodeRef<leptos::html::Div>, split: RwSignal<f64>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    clear_sbs_drag_listeners();

    if let Some(body) = window.document().and_then(|d| d.body()) {
        let _ = body.style().set_property("cursor", "col-resize");
        let _ = body.style().set_property("user-select", "none");
    }

    let mousemove = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        let Some(el) = pair_ref.get_untracked() else {
            return;
        };
        let rect = el.get_bounding_client_rect();
        let width = rect.width();
        if width <= 0.0 {
            return;
        }
        let x = ev.client_x() as f64 - rect.left();
        let f = (x / width).clamp(SBS_MIN_FRACTION, SBS_MAX_FRACTION);
        split.set(f);
    });

    let mouseup = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |_: web_sys::MouseEvent| {
        clear_sbs_drag_listeners();
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            let _ = body.style().remove_property("cursor");
            let _ = body.style().remove_property("user-select");
        }
    });

    let _ =
        window.add_event_listener_with_callback("mousemove", mousemove.as_ref().unchecked_ref());
    let _ = window.add_event_listener_with_callback("mouseup", mouseup.as_ref().unchecked_ref());

    SBS_DRAG_LISTENERS.with(|slot| {
        slot.borrow_mut().replace(SbsDragListeners {
            window,
            mousemove,
            mouseup,
        });
    });
}

fn fmt_line_range(start: u32, count: u32) -> String {
    if count == 0 {
        "—".to_string()
    } else if count == 1 {
        start.to_string()
    } else {
        format!("{}-{}", start, start + count - 1)
    }
}

fn hunk_header_label(hunk: &ProjectGitDiffHunk) -> String {
    let old = fmt_line_range(hunk.old_start, hunk.old_count);
    let new = fmt_line_range(hunk.new_start, hunk.new_count);
    format!("Lines {old} → {new}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffFileKind {
    PureAdd,
    PureDelete,
    Mixed,
}

fn classify_diff_file(file: &ProjectGitDiffFile) -> DiffFileKind {
    let mut has_added = false;
    let mut has_removed = false;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                ProjectGitDiffLineKind::Added => has_added = true,
                ProjectGitDiffLineKind::Removed => has_removed = true,
                ProjectGitDiffLineKind::Context => {}
            }
        }
    }
    match (has_added, has_removed) {
        (true, false) => DiffFileKind::PureAdd,
        (false, true) => DiffFileKind::PureDelete,
        _ => DiffFileKind::Mixed,
    }
}

fn line_class(kind: ProjectGitDiffLineKind) -> &'static str {
    match kind {
        ProjectGitDiffLineKind::Context => "diff-line diff-line-context",
        ProjectGitDiffLineKind::Added => "diff-line diff-line-added",
        ProjectGitDiffLineKind::Removed => "diff-line diff-line-removed",
    }
}

fn line_prefix(kind: ProjectGitDiffLineKind) -> &'static str {
    match kind {
        ProjectGitDiffLineKind::Context => " ",
        ProjectGitDiffLineKind::Added => "+",
        ProjectGitDiffLineKind::Removed => "-",
    }
}

#[component]
fn UnifiedHunk(
    hunk: ProjectGitDiffHunk,
    context_mode: DiffContextMode,
    line_offset: usize,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
) -> impl IntoView {
    let find = use_context::<FindState>();
    let header = hunk_header_label(&hunk);
    let show_header = context_mode == DiffContextMode::Hunks;
    let show_stage = scope == ProjectDiffScope::Unstaged;
    let hunk_id = hunk.hunk_id.clone();
    let token_lines: Vec<Option<LineTokens>> = match syntax_for_path(&relative_path) {
        Some(syn) => compute_hunk_tokens(&hunk, syn),
        None => vec![None; hunk.lines.len()],
    };
    view! {
        <div class="diff-hunk">
            {show_header.then(|| {
                let stage_root = root.clone();
                let stage_path = relative_path.clone();
                let stage_hunk_id = hunk_id.clone();
                view! {
                    <div class="diff-hunk-header">
                        {header}
                        {show_stage.then(move || {
                            let r = stage_root.clone();
                            let p = stage_path.clone();
                            let h = stage_hunk_id.clone();
                            view! {
                                <button
                                    class="diff-hunk-stage-btn"
                                    title="Stage hunk"
                                    on:click=move |_| {
                                        stage_hunk(r.clone(), p.clone(), h.clone());
                                    }
                                >
                                    "+"
                                </button>
                            }
                        })}
                    </div>
                }
            })}
            {hunk.lines.into_iter().zip(token_lines).enumerate().map(|(i, (line, tokens))| {
                let search_idx = line_offset + i;
                let base_class = line_class(line.kind);
                let prefix = line_prefix(line.kind);
                let old_str = line.old_line_number.map(|n| n.to_string()).unwrap_or_default();
                let new_str = line.new_line_number.map(|n| n.to_string()).unwrap_or_default();
                let text = line.text;
                let find_for_class = find.clone();
                let find_for_text = find.clone();
                view! {
                    <div
                        class=move || diff_line_class(base_class, search_idx, &find_for_class)
                        attr:data-find-idx=search_idx
                    >
                        <span class="diff-gutter diff-gutter-old">{old_str}</span>
                        <span class="diff-gutter diff-gutter-new">{new_str}</span>
                        <span class="diff-prefix">{prefix}</span>
                        {move || {
                            let result: AnyView = render_diff_text(&text, tokens.as_ref(), search_idx, &find_for_text);
                            result
                        }}
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

/// A single paired row in the side-by-side layout. Either side may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideRow {
    pub left: Option<SideBySideCell>,
    pub right: Option<SideBySideCell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCell {
    pub kind: ProjectGitDiffLineKind,
    pub line_number: Option<u32>,
    pub text: String,
    pub tokens: Option<LineTokens>,
}

/// Convenience wrapper used by tests; pairs lines with no syntax tokens.
pub fn pair_lines_side_by_side(lines: Vec<ProjectGitDiffLine>) -> Vec<SideBySideRow> {
    let n = lines.len();
    pair_lines_side_by_side_with_tokens(lines, vec![None; n], vec![None; n])
}

/// Pair the lines of a single hunk into side-by-side rows, attaching the
/// matching old-side / new-side syntax tokens to each cell.
///
/// `old_tokens[i]` and `new_tokens[i]` correspond to `lines[i]`. For Removed
/// lines only `old_tokens[i]` should be `Some`; for Added only `new_tokens[i]`;
/// Context lines may have both populated (with potentially different state).
///
/// Pairing algorithm: walk lines in order; collect consecutive Removed into a
/// left-run and Added into a right-run. On a Context line (or end of hunk),
/// flush the runs: zip their overlap into paired rows and emit the remainder
/// as half-empty rows. Context lines become rows with text on both sides.
pub fn pair_lines_side_by_side_with_tokens(
    lines: Vec<ProjectGitDiffLine>,
    old_tokens: Vec<Option<LineTokens>>,
    new_tokens: Vec<Option<LineTokens>>,
) -> Vec<SideBySideRow> {
    debug_assert_eq!(lines.len(), old_tokens.len());
    debug_assert_eq!(lines.len(), new_tokens.len());

    type Entry = (ProjectGitDiffLine, Option<LineTokens>, Option<LineTokens>);

    let mut rows: Vec<SideBySideRow> = Vec::new();
    let mut removed: Vec<Entry> = Vec::new();
    let mut added: Vec<Entry> = Vec::new();

    let flush =
        |removed: &mut Vec<Entry>, added: &mut Vec<Entry>, rows: &mut Vec<SideBySideRow>| {
            let pair_count = removed.len().min(added.len());
            let rem_iter = std::mem::take(removed);
            let add_iter = std::mem::take(added);
            let mut rem_it = rem_iter.into_iter();
            let mut add_it = add_iter.into_iter();
            for _ in 0..pair_count {
                let (r, r_old, _r_new) = rem_it.next().expect("removed run underflow");
                let (a, _a_old, a_new) = add_it.next().expect("added run underflow");
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: r.kind,
                        line_number: r.old_line_number,
                        text: r.text,
                        tokens: r_old,
                    }),
                    right: Some(SideBySideCell {
                        kind: a.kind,
                        line_number: a.new_line_number,
                        text: a.text,
                        tokens: a_new,
                    }),
                });
            }
            for (r, r_old, _) in rem_it {
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: r.kind,
                        line_number: r.old_line_number,
                        text: r.text,
                        tokens: r_old,
                    }),
                    right: None,
                });
            }
            for (a, _, a_new) in add_it {
                rows.push(SideBySideRow {
                    left: None,
                    right: Some(SideBySideCell {
                        kind: a.kind,
                        line_number: a.new_line_number,
                        text: a.text,
                        tokens: a_new,
                    }),
                });
            }
        };

    for ((line, old_tok), new_tok) in lines
        .into_iter()
        .zip(old_tokens.into_iter())
        .zip(new_tokens.into_iter())
    {
        match line.kind {
            ProjectGitDiffLineKind::Removed => removed.push((line, old_tok, new_tok)),
            ProjectGitDiffLineKind::Added => added.push((line, old_tok, new_tok)),
            ProjectGitDiffLineKind::Context => {
                flush(&mut removed, &mut added, &mut rows);
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.old_line_number,
                        text: line.text.clone(),
                        tokens: old_tok,
                    }),
                    right: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.new_line_number,
                        text: line.text,
                        tokens: new_tok,
                    }),
                });
            }
        }
    }
    flush(&mut removed, &mut added, &mut rows);
    rows
}

/// Compute search indices for each cell in the paired side-by-side rows.
///
/// Returns `Vec<(Option<usize>, Option<usize>)>` — one entry per row,
/// giving the flat search index of the left and right cell respectively.
fn sbs_search_indices(
    lines: &[ProjectGitDiffLine],
    line_offset: usize,
) -> Vec<(Option<usize>, Option<usize>)> {
    // Partition original line indices by kind.
    let mut removed_idx: Vec<usize> = Vec::new();
    let mut added_idx: Vec<usize> = Vec::new();
    let mut context_idx: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match line.kind {
            ProjectGitDiffLineKind::Removed => removed_idx.push(line_offset + i),
            ProjectGitDiffLineKind::Added => added_idx.push(line_offset + i),
            ProjectGitDiffLineKind::Context => context_idx.push(line_offset + i),
        }
    }

    // Replay the same pairing logic to assign indices to rows.
    let rows = pair_lines_side_by_side(lines.to_vec());
    let mut ri = 0usize;
    let mut ai = 0usize;
    let mut ci = 0usize;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let is_context = row
            .left
            .as_ref()
            .is_some_and(|c| c.kind == ProjectGitDiffLineKind::Context);
        if is_context {
            let idx = context_idx.get(ci).copied();
            ci += 1;
            out.push((idx, idx));
        } else {
            let li = row.left.as_ref().and_then(|c| {
                if c.kind == ProjectGitDiffLineKind::Removed {
                    let idx = removed_idx.get(ri).copied();
                    ri += 1;
                    idx
                } else {
                    None
                }
            });
            let r_idx = row.right.as_ref().and_then(|c| {
                if c.kind == ProjectGitDiffLineKind::Added {
                    let idx = added_idx.get(ai).copied();
                    ai += 1;
                    idx
                } else {
                    None
                }
            });
            out.push((li, r_idx));
        }
    }
    out
}

fn render_cell_search(
    cell: Option<SideBySideCell>,
    search_idx: Option<usize>,
    find: Option<FindState>,
) -> AnyView {
    match cell {
        Some(c) => {
            let base_class = line_class(c.kind);
            let prefix = line_prefix(c.kind);
            let num = c.line_number.map(|n| n.to_string()).unwrap_or_default();
            let text = c.text;
            let tokens = c.tokens;
            let find_for_class = find.clone();
            let find_for_text = find;
            let idx_str = search_idx.map(|i| i.to_string()).unwrap_or_default();
            view! {
                <div
                    class=move || {
                        if let (Some(idx), Some(find)) = (search_idx, &find_for_class) {
                            diff_line_class(base_class, idx, &Some(find.clone()))
                        } else {
                            base_class.to_string()
                        }
                    }
                    attr:data-find-idx=idx_str
                >
                    <span class="diff-gutter">{num}</span>
                    <span class="diff-prefix">{prefix}</span>
                    {move || {
                        let result: AnyView = match search_idx {
                            Some(idx) => render_diff_text(&text, tokens.as_ref(), idx, &find_for_text),
                            None => render_diff_text_plain(&text, tokens.as_ref()),
                        };
                        result
                    }}
                </div>
            }
            .into_any()
        }
        None => view! { <div class="diff-line diff-line-empty"></div> }.into_any(),
    }
}

// ── Search-aware rendering helpers ──────────────────────────────────────

fn diff_line_class(base: &'static str, search_idx: usize, find: &Option<FindState>) -> String {
    let Some(find) = find else {
        return base.to_string();
    };
    let results = find.results.get();
    if !results.match_set.contains(&search_idx) {
        return base.to_string();
    }
    let active = find.active_index.get();
    if active >= 0 && results.match_lines.get(active as usize) == Some(&search_idx) {
        format!("{base} find-hit-active")
    } else {
        format!("{base} find-hit")
    }
}

fn render_diff_text(
    text: &str,
    tokens: Option<&LineTokens>,
    search_idx: usize,
    find: &Option<FindState>,
) -> AnyView {
    let find_ranges: Option<Vec<(usize, usize)>> = find.as_ref().and_then(|f| {
        let results = f.results.get();
        results.ranges_by_line.get(&search_idx).cloned()
    });
    match (tokens, find_ranges) {
        (None, None) => view! { <span class="diff-text">{text.to_owned()}</span> }.into_any(),
        (None, Some(ranges)) => render_text_with_highlights(text, &ranges).into_any(),
        (Some(toks), None) => render_tokens(toks).into_any(),
        (Some(toks), Some(ranges)) => render_tokens_with_find(text, toks, &ranges).into_any(),
    }
}

fn render_diff_text_plain(text: &str, tokens: Option<&LineTokens>) -> AnyView {
    match tokens {
        Some(toks) => render_tokens(toks).into_any(),
        None => view! { <span class="diff-text">{text.to_owned()}</span> }.into_any(),
    }
}

/// Render syntax tokens as colored `<span>` children inside a `.diff-text`
/// wrapper. Uses inline `style="color:#…"` so we don't have to ship a syntect
/// theme stylesheet — we keep one bundled theme in `syntax_highlight::THEME`.
fn render_tokens(tokens: &LineTokens) -> impl IntoView + use<> {
    let spans: Vec<AnyView> = tokens
        .iter()
        .map(|t| {
            let style = format!("color:{}", color_to_css(t.fg));
            let txt = t.text.clone();
            view! { <span style=style>{txt}</span> }.into_any()
        })
        .collect();
    view! { <span class="diff-text">{spans}</span> }
}

/// Render syntax tokens AND inline find-bar match highlighting on the same
/// line. Walks tokens by byte offset; for each token, splits any overlapping
/// find ranges into nested `<span class="find-inline-match">` siblings while
/// keeping the token's color. Find ranges are pre-clipped to the line in
/// `find_bar`; byte offsets correspond to the concatenated text of all
/// `tokens[i].text` (which equals `text`).
///
/// Safety/robustness:
/// - Each slice index is snapped to the nearest UTF-8 char boundary at-or-before,
///   guarding against `find_bar` ranges that may currently come from JS UTF-16
///   `lastIndex` offsets (TODO upstream: convert JS offsets to Rust byte
///   offsets at source).
/// - Overlapping find ranges are tolerated: the running cursor `p` keeps each
///   byte appearing at most once.
fn render_tokens_with_find(
    text: &str,
    tokens: &LineTokens,
    find_ranges: &[(usize, usize)],
) -> impl IntoView + use<> {
    let mut fragments: Vec<AnyView> = Vec::new();
    let mut byte_pos: usize = 0;
    for tok in tokens {
        let tok_start = byte_pos;
        let tok_end = byte_pos + tok.text.len();
        let style = format!("color:{}", color_to_css(tok.fg));

        let mut sub: Vec<AnyView> = Vec::new();
        let mut p = tok_start;
        for &(rs, re) in find_ranges {
            // Clamp to current cursor (`p`) so overlapping ranges don't
            // duplicate bytes, and to the token bounds.
            let s = snap_char_boundary(text, rs.max(p).max(tok_start));
            let e = snap_char_boundary(text, re.min(tok_end));
            if s >= e {
                continue;
            }
            if s > p {
                let slice = text[p..s].to_owned();
                sub.push(view! { <>{slice}</> }.into_any());
            }
            let slice = text[s..e].to_owned();
            sub.push(view! { <span class="find-inline-match">{slice}</span> }.into_any());
            p = e;
        }
        if p < tok_end {
            let slice = text[p..tok_end].to_owned();
            sub.push(view! { <>{slice}</> }.into_any());
        }
        fragments.push(view! { <span style=style>{sub}</span> }.into_any());
        byte_pos = tok_end;
    }
    view! { <span class="diff-text">{fragments}</span> }
}

/// Round `idx` down to the nearest valid UTF-8 char boundary in `text`.
/// Idempotent on already-aligned indices and on `text.len()`.
fn snap_char_boundary(text: &str, mut idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        kind: ProjectGitDiffLineKind,
        old: Option<u32>,
        new: Option<u32>,
        text: &str,
    ) -> ProjectGitDiffLine {
        ProjectGitDiffLine {
            kind,
            text: text.to_string(),
            old_line_number: old,
            new_line_number: new,
        }
    }

    #[test]
    fn pair_only_removed() {
        let rows = pair_lines_side_by_side(vec![
            line(ProjectGitDiffLineKind::Removed, Some(1), None, "a"),
            line(ProjectGitDiffLineKind::Removed, Some(2), None, "b"),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].right.is_none());
        assert!(rows[1].right.is_none());
        assert_eq!(rows[0].left.as_ref().unwrap().text, "a");
    }

    #[test]
    fn pair_only_added() {
        let rows = pair_lines_side_by_side(vec![
            line(ProjectGitDiffLineKind::Added, None, Some(1), "a"),
            line(ProjectGitDiffLineKind::Added, None, Some(2), "b"),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].left.is_none());
        assert_eq!(rows[1].right.as_ref().unwrap().text, "b");
    }

    #[test]
    fn pair_equal_run_replace() {
        let rows = pair_lines_side_by_side(vec![
            line(ProjectGitDiffLineKind::Removed, Some(1), None, "x1"),
            line(ProjectGitDiffLineKind::Removed, Some(2), None, "x2"),
            line(ProjectGitDiffLineKind::Added, None, Some(1), "y1"),
            line(ProjectGitDiffLineKind::Added, None, Some(2), "y2"),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].left.as_ref().unwrap().text, "x1");
        assert_eq!(rows[0].right.as_ref().unwrap().text, "y1");
        assert_eq!(rows[1].left.as_ref().unwrap().text, "x2");
        assert_eq!(rows[1].right.as_ref().unwrap().text, "y2");
    }

    #[test]
    fn pair_unequal_replace_more_added() {
        let rows = pair_lines_side_by_side(vec![
            line(ProjectGitDiffLineKind::Removed, Some(1), None, "x1"),
            line(ProjectGitDiffLineKind::Added, None, Some(1), "y1"),
            line(ProjectGitDiffLineKind::Added, None, Some(2), "y2"),
            line(ProjectGitDiffLineKind::Added, None, Some(3), "y3"),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].left.as_ref().unwrap().text, "x1");
        assert_eq!(rows[0].right.as_ref().unwrap().text, "y1");
        assert!(rows[1].left.is_none());
        assert_eq!(rows[1].right.as_ref().unwrap().text, "y2");
        assert!(rows[2].left.is_none());
        assert_eq!(rows[2].right.as_ref().unwrap().text, "y3");
    }

    #[test]
    fn pair_unequal_replace_more_removed() {
        let rows = pair_lines_side_by_side(vec![
            line(ProjectGitDiffLineKind::Removed, Some(1), None, "x1"),
            line(ProjectGitDiffLineKind::Removed, Some(2), None, "x2"),
            line(ProjectGitDiffLineKind::Removed, Some(3), None, "x3"),
            line(ProjectGitDiffLineKind::Added, None, Some(1), "y1"),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].right.as_ref().unwrap().text, "y1");
        assert!(rows[1].right.is_none());
        assert!(rows[2].right.is_none());
        assert_eq!(rows[2].left.as_ref().unwrap().text, "x3");
    }

    #[test]
    fn pair_interleaved_context() {
        let rows = pair_lines_side_by_side(vec![
            line(ProjectGitDiffLineKind::Context, Some(1), Some(1), "c1"),
            line(ProjectGitDiffLineKind::Removed, Some(2), None, "x"),
            line(ProjectGitDiffLineKind::Added, None, Some(2), "y"),
            line(ProjectGitDiffLineKind::Context, Some(3), Some(3), "c2"),
            line(ProjectGitDiffLineKind::Added, None, Some(4), "z"),
            line(ProjectGitDiffLineKind::Context, Some(4), Some(5), "c3"),
        ]);
        assert_eq!(rows.len(), 5);
        // c1: paired context
        assert_eq!(rows[0].left.as_ref().unwrap().text, "c1");
        assert_eq!(rows[0].right.as_ref().unwrap().text, "c1");
        // x/y: paired replace
        assert_eq!(rows[1].left.as_ref().unwrap().text, "x");
        assert_eq!(rows[1].right.as_ref().unwrap().text, "y");
        // c2
        assert_eq!(rows[2].left.as_ref().unwrap().text, "c2");
        // z: only-right
        assert!(rows[3].left.is_none());
        assert_eq!(rows[3].right.as_ref().unwrap().text, "z");
        // c3
        assert_eq!(rows[4].right.as_ref().unwrap().text, "c3");
    }

    #[test]
    fn compute_hunk_tokens_returns_some_for_known_language() {
        use crate::syntax_highlight::{compute_hunk_tokens, syntax_for_path};
        use protocol::ProjectGitDiffHunk;

        let syntax = syntax_for_path("hello.rs").expect("rust syntax bundled");
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 2,
            hunk_id: "h1".to_owned(),
            lines: vec![
                line(ProjectGitDiffLineKind::Added, None, Some(1), "fn main() {}"),
                line(
                    ProjectGitDiffLineKind::Added,
                    None,
                    Some(2),
                    "let x: u32 = 1;",
                ),
            ],
        };
        let tokens = compute_hunk_tokens(&hunk, syntax);
        assert_eq!(tokens.len(), 2);
        let first = tokens[0].as_ref().expect("rust line should highlight");
        // syntect emits at least one token per source line
        assert!(!first.is_empty());
        // tokens reconstruct the original line
        let joined: String = first.iter().map(|t| t.text.clone()).collect();
        assert_eq!(joined, "fn main() {}");
        // at least one token has a non-default color (i.e. we're actually
        // colorizing, not falling back to plain text)
        let any_colored = tokens
            .iter()
            .flatten()
            .flat_map(|line_toks| line_toks.iter())
            .any(|t| t.fg.r != 0 || t.fg.g != 0 || t.fg.b != 0);
        assert!(any_colored, "expected at least one colored token");
    }

    #[test]
    fn syntax_for_path_returns_none_for_unknown() {
        use crate::syntax_highlight::syntax_for_path;
        // Unknown extensions must fall back to plain text (None) rather than
        // panic or default to some random syntax.
        assert!(syntax_for_path("project/file.thisextdoesnotexist").is_none());
    }

    #[test]
    fn snap_char_boundary_handles_multibyte() {
        // "é" is 2 bytes in UTF-8 (0xC3 0xA9). An offset of 1 is mid-codepoint.
        let s = "éx";
        assert_eq!(snap_char_boundary(s, 0), 0);
        assert_eq!(snap_char_boundary(s, 1), 0); // snap back to char start
        assert_eq!(snap_char_boundary(s, 2), 2);
        assert_eq!(snap_char_boundary(s, 3), 3);
        assert_eq!(snap_char_boundary(s, 99), s.len()); // past end → end
    }

    #[test]
    fn compute_hunk_tokens_dual_pure_added() {
        // All-added hunk: every old-side entry is None, every new-side entry
        // is Some.
        use crate::syntax_highlight::{compute_hunk_tokens_dual, syntax_for_path};
        use protocol::ProjectGitDiffHunk;

        let syntax = syntax_for_path("foo.rs").unwrap();
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 2,
            hunk_id: "h".into(),
            lines: vec![
                line(ProjectGitDiffLineKind::Added, None, Some(1), "fn main() {}"),
                line(ProjectGitDiffLineKind::Added, None, Some(2), "let x = 1;"),
            ],
        };
        let (old, new) = compute_hunk_tokens_dual(&hunk, syntax);
        assert_eq!(old.len(), 2);
        assert_eq!(new.len(), 2);
        assert!(old.iter().all(|o| o.is_none()));
        assert!(new.iter().all(|n| n.is_some()));
    }

    #[test]
    fn compute_hunk_tokens_dual_pure_removed() {
        use crate::syntax_highlight::{compute_hunk_tokens_dual, syntax_for_path};
        use protocol::ProjectGitDiffHunk;

        let syntax = syntax_for_path("foo.rs").unwrap();
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 0,
            hunk_id: "h".into(),
            lines: vec![
                line(
                    ProjectGitDiffLineKind::Removed,
                    Some(1),
                    None,
                    "fn old() {}",
                ),
                line(ProjectGitDiffLineKind::Removed, Some(2), None, "let y = 2;"),
            ],
        };
        let (old, new) = compute_hunk_tokens_dual(&hunk, syntax);
        assert!(old.iter().all(|o| o.is_some()));
        assert!(new.iter().all(|n| n.is_none()));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::syntax_highlight::{compute_hunk_tokens, syntax_for_path};
    use leptos::mount::mount_to;
    use protocol::ProjectGitDiffHunk;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 800px; height: 600px;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// A line of Rust source rendered by the diff view's per-line render path
    /// must produce DOM children with inline `style="color:#…"` so users
    /// actually see syntax colors. Asserting at the rendered-output level
    /// (rather than on internal class names) keeps the test resilient to
    /// future refactors of `render_tokens`.
    #[wasm_bindgen_test]
    async fn rust_line_renders_colored_spans() {
        let syntax = syntax_for_path("foo.rs").expect("rust syntax bundled");
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            hunk_id: "h1".to_owned(),
            lines: vec![ProjectGitDiffLine {
                kind: ProjectGitDiffLineKind::Added,
                text: "fn main() {}".to_owned(),
                old_line_number: None,
                new_line_number: Some(1),
            }],
        };
        let tokens = compute_hunk_tokens(&hunk, syntax);
        let toks = tokens[0].clone().expect("rust line tokenizes");

        let container = make_container();
        let container_for_mount = container.clone();
        let _handle = mount_to(container_for_mount, move || render_tokens(&toks));
        next_tick().await;

        let nodes = container.query_selector_all("span[style]").unwrap();
        assert!(
            nodes.length() > 0,
            "expected at least one styled span in rendered output"
        );
        let mut found_color = false;
        for i in 0..nodes.length() {
            if let Some(node) = nodes.item(i) {
                let el: web_sys::Element = node.dyn_into().unwrap();
                let style = el.get_attribute("style").unwrap_or_default();
                if style.contains("color:") {
                    found_color = true;
                    break;
                }
            }
        }
        assert!(
            found_color,
            "expected at least one span to have a color: in its style attribute"
        );

        // Concatenated text content of the rendered output must match the
        // original line — rendering must not corrupt or duplicate source.
        let rendered_text = container.text_content().unwrap_or_default();
        assert_eq!(rendered_text, "fn main() {}");
    }
}
