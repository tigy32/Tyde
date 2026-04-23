use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, DiffViewState};

use protocol::{
    DiffContextMode, FrameKind, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectReadDiffPayload, ProjectRootPath,
    StreamPath,
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

#[component]
fn DiffContent(diff: DiffViewState) -> impl IntoView {
    let scope_label = match diff.scope {
        ProjectDiffScope::Staged => "staged",
        ProjectDiffScope::Unstaged => "unstaged",
    };

    view! {
        <div class="diff-content">
            <div class="diff-file-header">
                <span class="diff-file-path">{diff.root.to_string()}</span>
                <span class="diff-scope-badge">{scope_label}</span>
            </div>
            {diff.files.into_iter().map(|file| {
                view! { <DiffFileView file=file scope_label=scope_label context_mode=diff.context_mode /> }
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn DiffFileView(
    file: ProjectGitDiffFile,
    scope_label: &'static str,
    context_mode: DiffContextMode,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let view_mode_sig = state.diff_view_mode;

    view! {
        <div class="diff-file">
            <div class="diff-file-header">
                <span class="diff-file-path">{file.relative_path}</span>
                <span class="diff-scope-badge">{scope_label}</span>
            </div>
            <div class="diff-hunks">
                {file.hunks.into_iter().map(|hunk| {
                    let hunk_for_unified = hunk.clone();
                    let hunk_for_side = hunk;
                    view! {
                        {move || match view_mode_sig.get() {
                            DiffViewMode::Unified => view! {
                                <UnifiedHunk hunk=hunk_for_unified.clone() context_mode=context_mode />
                            }.into_any(),
                            DiffViewMode::SideBySide => view! {
                                <SideBySideHunk hunk=hunk_for_side.clone() context_mode=context_mode />
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
fn UnifiedHunk(hunk: ProjectGitDiffHunk, context_mode: DiffContextMode) -> impl IntoView {
    let header = hunk_header_label(&hunk);
    let show_header = context_mode == DiffContextMode::Hunks;
    view! {
        <div class="diff-hunk">
            {show_header.then(|| view! {
                <div class="diff-hunk-header">{header}</div>
            })}
            {hunk.lines.into_iter().map(|line| {
                let class = line_class(line.kind);
                let prefix = line_prefix(line.kind);
                let old_str = line.old_line_number.map(|n| n.to_string()).unwrap_or_default();
                let new_str = line.new_line_number.map(|n| n.to_string()).unwrap_or_default();
                view! {
                    <div class=class>
                        <span class="diff-gutter diff-gutter-old">{old_str}</span>
                        <span class="diff-gutter diff-gutter-new">{new_str}</span>
                        <span class="diff-prefix">{prefix}</span>
                        <span class="diff-text">{line.text}</span>
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
        let rem_iter: Vec<_> = removed.drain(..).collect();
        let add_iter: Vec<_> = added.drain(..).collect();
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

#[component]
fn SideBySideHunk(hunk: ProjectGitDiffHunk, context_mode: DiffContextMode) -> impl IntoView {
    let header = hunk_header_label(&hunk);
    let show_header = context_mode == DiffContextMode::Hunks;
    let rows = pair_lines_side_by_side(hunk.lines);
    view! {
        <div class="diff-hunk diff-hunk-side-by-side">
            {show_header.then(|| view! {
                <div class="diff-hunk-header">{header}</div>
            })}
            {rows.into_iter().map(|row| {
                view! {
                    <div class="diff-row-sbs">
                        <div class="diff-col-sbs diff-col-left">
                            {render_cell(row.left)}
                        </div>
                        <div class="diff-col-sbs diff-col-right">
                            {render_cell(row.right)}
                        </div>
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

fn render_cell(cell: Option<SideBySideCell>) -> impl IntoView {
    match cell {
        Some(c) => {
            let class = line_class(c.kind);
            let prefix = line_prefix(c.kind);
            let num = c.line_number.map(|n| n.to_string()).unwrap_or_default();
            view! {
                <div class=class>
                    <span class="diff-gutter">{num}</span>
                    <span class="diff-prefix">{prefix}</span>
                    <span class="diff-text">{c.text}</span>
                </div>
            }
            .into_any()
        }
        None => view! { <div class="diff-line diff-line-empty"></div> }.into_any(),
    }
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
