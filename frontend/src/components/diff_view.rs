use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;

use crate::components::find_bar::{FindBar, FindState, render_text_with_highlights};
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, DiffViewState};

use protocol::{
    DiffContextMode, FrameKind, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectPath, ProjectReadDiffPayload,
    ProjectRootPath, ProjectStageHunkPayload, StreamPath,
};

const SBS_MIN_FRACTION: f64 = 0.05;
const SBS_MAX_FRACTION: f64 = 0.95;

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

    view! {
        <div class="diff-content">
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
                view! { <DiffFileView file=file scope_label=scope_label scope=diff.scope root=root context_mode=diff.context_mode hunk_offsets=hunk_offsets /> }
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn DiffFileView(
    file: ProjectGitDiffFile,
    scope_label: &'static str,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    context_mode: DiffContextMode,
    hunk_offsets: Vec<usize>,
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
                DiffViewMode::Unified => render_unified_hunks(
                    file_for_view.clone(),
                    offsets_for_view.clone(),
                    context_mode,
                    scope,
                    root_for_view.clone(),
                    path_for_view.clone(),
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SbsSide {
    Left,
    Right,
}

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

    let file_left = file.clone();
    let file_right = file.clone();
    let offsets_left = hunk_offsets.clone();
    let offsets_right = hunk_offsets.clone();
    let root_left = root.clone();
    let root_right = root.clone();
    let path_left = relative_path.clone();
    let path_right = relative_path.clone();

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
                )}
            </div>
        </div>
    }
    .into_any()
}

fn render_sbs_pane_content(
    side: SbsSide,
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
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
        let rows = pair_lines_side_by_side(lines);

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
            {hunk.lines.into_iter().enumerate().map(|(i, line)| {
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
                            let result: AnyView = render_diff_text(&text, search_idx, &find_for_text);
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
}

/// Pair the lines of a single hunk into side-by-side rows.
///
/// Algorithm: walk lines in order; collect consecutive Removed into a left-run
/// and Added into a right-run. On a Context line (or end of hunk), flush the
/// runs: zip their overlap into paired rows and emit the remainder as
/// half-empty rows. Context lines become rows with the same text on both sides.
pub fn pair_lines_side_by_side(lines: Vec<ProjectGitDiffLine>) -> Vec<SideBySideRow> {
    let mut rows: Vec<SideBySideRow> = Vec::new();
    let mut removed: Vec<ProjectGitDiffLine> = Vec::new();
    let mut added: Vec<ProjectGitDiffLine> = Vec::new();

    let flush = |removed: &mut Vec<ProjectGitDiffLine>,
                 added: &mut Vec<ProjectGitDiffLine>,
                 rows: &mut Vec<SideBySideRow>| {
        let pair_count = removed.len().min(added.len());
        let rem_iter = std::mem::take(removed);
        let add_iter = std::mem::take(added);
        let mut rem_it = rem_iter.into_iter();
        let mut add_it = add_iter.into_iter();
        for _ in 0..pair_count {
            let r = rem_it.next().expect("removed run underflow");
            let a = add_it.next().expect("added run underflow");
            rows.push(SideBySideRow {
                left: Some(SideBySideCell {
                    kind: r.kind,
                    line_number: r.old_line_number,
                    text: r.text,
                }),
                right: Some(SideBySideCell {
                    kind: a.kind,
                    line_number: a.new_line_number,
                    text: a.text,
                }),
            });
        }
        for r in rem_it {
            rows.push(SideBySideRow {
                left: Some(SideBySideCell {
                    kind: r.kind,
                    line_number: r.old_line_number,
                    text: r.text,
                }),
                right: None,
            });
        }
        for a in add_it {
            rows.push(SideBySideRow {
                left: None,
                right: Some(SideBySideCell {
                    kind: a.kind,
                    line_number: a.new_line_number,
                    text: a.text,
                }),
            });
        }
    };

    for line in lines {
        match line.kind {
            ProjectGitDiffLineKind::Removed => removed.push(line),
            ProjectGitDiffLineKind::Added => added.push(line),
            ProjectGitDiffLineKind::Context => {
                flush(&mut removed, &mut added, &mut rows);
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.old_line_number,
                        text: line.text.clone(),
                    }),
                    right: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.new_line_number,
                        text: line.text,
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
                        let result: AnyView = if let Some(idx) = search_idx {
                            render_diff_text(&text, idx, &find_for_text)
                        } else {
                            view! { <span class="diff-text">{text.clone()}</span> }.into_any()
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

fn render_diff_text(text: &str, search_idx: usize, find: &Option<FindState>) -> AnyView {
    if let Some(find) = find {
        let results = find.results.get();
        if let Some(ranges) = results.ranges_by_line.get(&search_idx) {
            return render_text_with_highlights(text, ranges).into_any();
        }
    }
    view! { <span class="diff-text">{text.to_owned()}</span> }.into_any()
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
}
