use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::find_bar::{FindBar, FindState, render_text_with_highlights};
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, DiffViewState};

use protocol::{
    DiffContextMode, FrameKind, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectPath, ProjectReadDiffPayload,
    ProjectRootPath, ProjectStageHunkPayload, StreamPath,
};

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
            <div class="diff-file-header">
                <span class="diff-file-path">{diff.root.to_string()}</span>
                <span class="diff-scope-badge">{scope_label}</span>
            </div>
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

    view! {
        <div class="diff-file">
            <div class="diff-file-header">
                <span class="diff-file-path">{file.relative_path}</span>
                <span class="diff-scope-badge">{scope_label}</span>
            </div>
            <div class="diff-hunks">
                {file.hunks.into_iter().enumerate().map(|(hi, hunk)| {
                    let offset = hunk_offsets[hi];
                    let hunk_for_unified = hunk.clone();
                    let hunk_for_side = hunk;
                    let root_u = root.clone();
                    let root_s = root.clone();
                    let path_u = relative_path.clone();
                    let path_s = relative_path.clone();
                    view! {
                        {move || match view_mode_sig.get() {
                            DiffViewMode::Unified => view! {
                                <UnifiedHunk hunk=hunk_for_unified.clone() context_mode=context_mode line_offset=offset scope=scope root=root_u.clone() relative_path=path_u.clone() />
                            }.into_any(),
                            DiffViewMode::SideBySide => view! {
                                <SideBySideHunk hunk=hunk_for_side.clone() context_mode=context_mode line_offset=offset scope=scope root=root_s.clone() relative_path=path_s.clone() />
                            }.into_any(),
                        }}
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
}

fn hunk_header_label(hunk: &ProjectGitDiffHunk) -> String {
    format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    )
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

#[component]
fn SideBySideHunk(
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

    let indices = sbs_search_indices(&hunk.lines, line_offset);
    let rows = pair_lines_side_by_side(hunk.lines);

    view! {
        <div class="diff-hunk diff-hunk-side-by-side">
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
            {rows.into_iter().zip(indices).map(|(row, (left_idx, right_idx))| {
                let find_l = find.clone();
                let find_r = find.clone();
                view! {
                    <div class="diff-row-sbs">
                        <div class="diff-col-sbs diff-col-left">
                            {render_cell_search(row.left, left_idx, find_l)}
                        </div>
                        <div class="diff-col-sbs diff-col-right">
                            {render_cell_search(row.right, right_idx, find_r)}
                        </div>
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
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
