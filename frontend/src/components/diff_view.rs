use leptos::prelude::*;

use crate::state::{AppState, DiffViewState};

use protocol::{ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk, ProjectGitDiffLineKind};

#[component]
pub fn DiffView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let diff = move || state.diff_content.get();

    view! {
        <div class="diff-view">
            {move || match diff() {
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
                view! { <DiffFileView file=file scope_label=scope_label /> }
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn DiffFileView(file: ProjectGitDiffFile, scope_label: &'static str) -> impl IntoView {
    view! {
        <div class="diff-file">
            <div class="diff-file-header">
                <span class="diff-file-path">{file.relative_path}</span>
                <span class="diff-scope-badge">{scope_label}</span>
            </div>
            <div class="diff-hunks">
                {file.hunks.into_iter().map(|hunk| {
                    view! { <DiffHunkView hunk=hunk /> }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
}

#[component]
fn DiffHunkView(hunk: ProjectGitDiffHunk) -> impl IntoView {
    let mut old_line: u32 = 0;
    let mut new_line: u32 = 0;

    // Parse hunk header for starting line numbers (e.g., @@ -10,5 +10,7 @@)
    if let Some(start) = hunk.header.find("@@")
        && let Some(rest) = hunk.header.get(start + 3..)
    {
        let parts: Vec<&str> = rest.splitn(3, ' ').collect();
        if let Some(old_part) = parts.first()
            && let Some(num) = old_part.strip_prefix('-')
            && let Some(line_str) = num.split(',').next()
        {
            old_line = line_str.parse().unwrap_or(0);
        }
        if parts.len() > 1
            && let Some(num) = parts[1].strip_prefix('+')
            && let Some(line_str) = num.split(',').next()
        {
            new_line = line_str.parse().unwrap_or(0);
        }
    }

    let lines: Vec<_> = hunk
        .lines
        .into_iter()
        .map(|line| {
            let (old_num, new_num) = match line.kind {
                ProjectGitDiffLineKind::Context => {
                    old_line += 1;
                    new_line += 1;
                    (Some(old_line), Some(new_line))
                }
                ProjectGitDiffLineKind::Removed => {
                    old_line += 1;
                    (Some(old_line), None)
                }
                ProjectGitDiffLineKind::Added => {
                    new_line += 1;
                    (None, Some(new_line))
                }
            };
            let line_class = match line.kind {
                ProjectGitDiffLineKind::Context => "diff-line diff-line-context",
                ProjectGitDiffLineKind::Added => "diff-line diff-line-added",
                ProjectGitDiffLineKind::Removed => "diff-line diff-line-removed",
            };
            let prefix = match line.kind {
                ProjectGitDiffLineKind::Context => " ",
                ProjectGitDiffLineKind::Added => "+",
                ProjectGitDiffLineKind::Removed => "-",
            };
            (old_num, new_num, line_class, prefix, line.text)
        })
        .collect();

    view! {
        <div class="diff-hunk">
            <div class="diff-hunk-header">{hunk.header}</div>
            {lines.into_iter().map(|(old_num, new_num, class, prefix, text)| {
                view! {
                    <div class=class>
                        <span class="diff-gutter diff-gutter-old">
                            {old_num.map(|n| n.to_string()).unwrap_or_default()}
                        </span>
                        <span class="diff-gutter diff-gutter-new">
                            {new_num.map(|n| n.to_string()).unwrap_or_default()}
                        </span>
                        <span class="diff-prefix">{prefix}</span>
                        <span class="diff-text">{text}</span>
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}
