use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::window;

use crate::send::send_frame;
use crate::state::{AppState, DiffViewState, root_display_name};

use protocol::{
    FrameKind, ProjectDiffScope, ProjectDiscardFilePayload, ProjectGitChangeKind,
    ProjectGitCommitPayload, ProjectGitFileStatus, ProjectPath, ProjectReadDiffPayload,
    ProjectRefreshPayload, ProjectRootGitStatus, ProjectRootPath, ProjectStageFilePayload,
    ProjectUnstageFilePayload, StreamPath,
};

#[component]
pub fn GitPanel() -> impl IntoView {
    let state = expect_context::<AppState>();

    let git_roots = Memo::new(move |_| {
        let pid = state.active_project.get()?.project_id;
        let map = state.git_status.get();
        map.get(&pid).cloned()
    });

    let refresh = move |_| {
        let state = state.clone();
        spawn_local(async move {
            send_project_refresh(&state).await;
        });
    };

    view! {
        <div class="git-panel">
            <div class="gp-header">
                <span class="gp-branch">
                    {move || {
                        git_roots.get()
                            .map(|roots| {
                                if roots.len() == 1 {
                                    roots
                                        .first()
                                        .and_then(|r| r.branch.clone())
                                        .map(|b| format!("\u{238b} {b}"))
                                        .unwrap_or_else(|| "\u{238b} --".to_owned())
                                } else {
                                    format!("\u{238b} {} roots", roots.len())
                                }
                            })
                            .unwrap_or_else(|| "\u{238b} --".to_owned())
                    }}
                </span>
                <button class="gp-refresh" title="Refresh" on:click=refresh>
                    "\u{21bb}"
                </button>
            </div>
            <div class="gp-content">
                {move || {
                    match git_roots.get() {
                        Some(roots) => {
                            if roots.iter().all(|r| r.clean) {
                                vec![view! {
                                    <div class="gp-clean">
                                        "\u{2713} Working tree clean"
                                    </div>
                                }.into_any()]
                            } else {
                                roots.into_iter().map(|root| {
                                    view! { <GitRootSection root=root /> }.into_any()
                                }).collect()
                            }
                        }
                        None => vec![view! {
                            <div class="panel-empty">"No git status"</div>
                        }.into_any()],
                    }
                }}
            </div>
        </div>
    }
}

#[component]
fn GitRootSection(root: ProjectRootGitStatus) -> impl IntoView {
    let staged: Vec<_> = root
        .files
        .iter()
        .filter(|f| f.staged.is_some())
        .cloned()
        .collect();
    let unstaged: Vec<_> = root
        .files
        .iter()
        .filter(|f| f.unstaged.is_some() && !f.untracked)
        .cloned()
        .collect();
    let untracked: Vec<_> = root.files.iter().filter(|f| f.untracked).cloned().collect();

    let root_path = root.root.clone();
    let staged_expanded = RwSignal::new(true);
    let unstaged_expanded = RwSignal::new(true);
    let untracked_expanded = RwSignal::new(true);

    let staged_count = staged.len();
    let unstaged_count = unstaged.len();
    let untracked_count = untracked.len();

    let has_staged = staged_count != 0;
    let has_unstaged = unstaged_count != 0;
    let has_untracked = untracked_count != 0;

    let root_for_staged = root_path.clone();
    let root_for_unstaged = root_path.clone();
    let root_for_untracked = root_path.clone();
    let root_for_commit = root_path;
    let root_label = root_display_name(&root.root);
    let root_title = root.root.0.clone();
    let branch_label = root.branch.unwrap_or_else(|| "--".to_owned());

    let ahead_behind = if root.ahead > 0 || root.behind > 0 {
        let mut parts = Vec::new();
        if root.ahead > 0 {
            parts.push(format!("\u{2191}{}", root.ahead));
        }
        if root.behind > 0 {
            parts.push(format!("\u{2193}{}", root.behind));
        }
        Some(parts.join(" "))
    } else {
        None
    };

    let commit_message = RwSignal::new(String::new());

    view! {
        <div class="gp-root-section">
            <div class="gp-root-header" title=root_title>
                <span class="gp-root-name">{root_label}</span>
                <span class="gp-root-branch">{branch_label}</span>
                {ahead_behind.map(|ab| view! {
                    <span class="gp-root-ahead-behind">{ab}</span>
                })}
            </div>
            <Show when=move || has_staged>
                <div class="gp-commit-area">
                    <textarea
                        class="gp-commit-input"
                        placeholder="Commit message"
                        rows="3"
                        prop:value=move || commit_message.get()
                        on:input=move |ev| {
                            commit_message.set(event_target_value(&ev));
                        }
                    />
                    <button
                        class="gp-commit-btn"
                        disabled=move || commit_message.get().trim().is_empty()
                        on:click={
                            let root = root_for_commit.clone();
                            move |_| {
                                let msg = commit_message.get();
                                if !msg.trim().is_empty() {
                                    send_commit(root.clone(), msg);
                                    commit_message.set(String::new());
                                }
                            }
                        }
                    >
                        "Commit"
                    </button>
                </div>
                <GitFileSection
                    title=format!("Staged Changes [{staged_count}]")
                    files=staged.clone()
                    expanded=staged_expanded
                    scope=ProjectDiffScope::Staged
                    root_path=root_for_staged.clone()
                    show_stage_btn=false
                    show_unstage_btn=true
                    show_discard_btn=false
                />
            </Show>
            <Show when=move || has_unstaged>
                <GitFileSection
                    title=format!("Changes [{unstaged_count}]")
                    files=unstaged.clone()
                    expanded=unstaged_expanded
                    scope=ProjectDiffScope::Unstaged
                    root_path=root_for_unstaged.clone()
                    show_stage_btn=true
                    show_unstage_btn=false
                    show_discard_btn=true
                />
            </Show>
            <Show when=move || has_untracked>
                <GitFileSection
                    title=format!("Untracked [{untracked_count}]")
                    files=untracked.clone()
                    expanded=untracked_expanded
                    scope=ProjectDiffScope::Unstaged
                    root_path=root_for_untracked.clone()
                    show_stage_btn=true
                    show_unstage_btn=false
                    show_discard_btn=true
                />
            </Show>
        </div>
    }
}

#[component]
fn GitFileSection(
    title: String,
    files: Vec<ProjectGitFileStatus>,
    expanded: RwSignal<bool>,
    scope: ProjectDiffScope,
    root_path: ProjectRootPath,
    show_stage_btn: bool,
    show_unstage_btn: bool,
    show_discard_btn: bool,
) -> impl IntoView {
    let toggle = move |_| expanded.update(|v| *v = !*v);

    // Bulk action data
    let bulk_paths: Vec<String> = files.iter().map(|f| f.relative_path.clone()).collect();
    let bulk_root = root_path.clone();

    view! {
        <div class="gp-section">
            <div class="gp-section-header-row">
                <button class="gp-section-header" on:click=toggle>
                    <span class="fe-chevron">{move || if expanded.get() { "\u{25be}" } else { "\u{25b8}" }}</span>
                    <span class="gp-section-title">{title}</span>
                </button>
                <div class="gp-section-actions">
                    {show_stage_btn.then(|| {
                        let root = bulk_root.clone();
                        let paths = bulk_paths.clone();
                        view! {
                            <button
                                class="gp-section-action"
                                title="Stage all"
                                on:click=move |_| {
                                    for path in &paths {
                                        stage_file(root.clone(), path.clone());
                                    }
                                }
                            >
                                "++"
                            </button>
                        }
                    })}
                    {show_unstage_btn.then(|| {
                        let root = bulk_root.clone();
                        let paths = bulk_paths.clone();
                        view! {
                            <button
                                class="gp-section-action"
                                title="Unstage all"
                                on:click=move |_| {
                                    for path in &paths {
                                        unstage_file(root.clone(), path.clone());
                                    }
                                }
                            >
                                "\u{2212}\u{2212}"
                            </button>
                        }
                    })}
                </div>
            </div>
            <Show when=move || expanded.get()>
                <div class="gp-section-files">
                    {files.iter().map(|file| {
                        let path = file.relative_path.clone();
                        let change_kind = match scope {
                            ProjectDiffScope::Staged => file.staged,
                            ProjectDiffScope::Unstaged => file.unstaged,
                        };
                        let is_untracked = file.untracked;
                        let icon = if is_untracked {
                            "?"
                        } else {
                            change_kind_icon(change_kind)
                        };
                        let icon_class = if is_untracked {
                            "gp-status-icon untracked"
                        } else {
                            change_kind_class(change_kind)
                        };

                        let root_for_click = root_path.clone();
                        let path_for_click = path.clone();
                        let root_for_stage = root_path.clone();
                        let path_for_stage = path.clone();
                        let root_for_unstage = root_path.clone();
                        let path_for_unstage = path.clone();
                        let root_for_discard = root_path.clone();
                        let path_for_discard = path.clone();

                        view! {
                            <div class="gp-file-row">
                                <button
                                    class="gp-file-btn"
                                    on:click=move |_| {
                                        view_diff(root_for_click.clone(), scope, path_for_click.clone());
                                    }
                                >
                                    <span class=icon_class>{icon}</span>
                                    <span class="gp-file-path">{path.clone()}</span>
                                </button>
                                <div class="gp-file-actions">
                                    {show_discard_btn.then(|| {
                                        let root = root_for_discard.clone();
                                        let path = path_for_discard.clone();
                                        view! {
                                            <button
                                                class="gp-discard-btn"
                                                title="Discard changes"
                                                on:click=move |_| {
                                                    discard_file(root.clone(), path.clone());
                                                }
                                            >
                                                "\u{2715}"
                                            </button>
                                        }
                                    })}
                                    {show_stage_btn.then(|| {
                                        let root = root_for_stage.clone();
                                        let path = path_for_stage.clone();
                                        view! {
                                            <button
                                                class="gp-stage-btn"
                                                title="Stage file"
                                                on:click=move |_| {
                                                    stage_file(root.clone(), path.clone());
                                                }
                                            >
                                                "+"
                                            </button>
                                        }
                                    })}
                                    {show_unstage_btn.then(|| {
                                        let root = root_for_unstage.clone();
                                        let path = path_for_unstage.clone();
                                        view! {
                                            <button
                                                class="gp-unstage-btn"
                                                title="Unstage file"
                                                on:click=move |_| {
                                                    unstage_file(root.clone(), path.clone());
                                                }
                                            >
                                                "\u{2212}"
                                            </button>
                                        }
                                    })}
                                </div>
                            </div>
                        }
                    }).collect::<Vec<_>>()}
                </div>
            </Show>
        </div>
    }
}

fn change_kind_icon(kind: Option<ProjectGitChangeKind>) -> &'static str {
    match kind {
        Some(ProjectGitChangeKind::Added) => "A",
        Some(ProjectGitChangeKind::Modified) => "M",
        Some(ProjectGitChangeKind::Deleted) => "D",
        Some(ProjectGitChangeKind::Renamed) => "R",
        Some(ProjectGitChangeKind::Copied) => "C",
        Some(ProjectGitChangeKind::TypeChanged) => "T",
        None => " ",
    }
}

fn change_kind_class(kind: Option<ProjectGitChangeKind>) -> &'static str {
    match kind {
        Some(ProjectGitChangeKind::Added) => "gp-status-icon added",
        Some(ProjectGitChangeKind::Modified) => "gp-status-icon modified",
        Some(ProjectGitChangeKind::Deleted) => "gp-status-icon deleted",
        Some(ProjectGitChangeKind::Renamed) => "gp-status-icon renamed",
        Some(ProjectGitChangeKind::Copied) => "gp-status-icon renamed",
        Some(ProjectGitChangeKind::TypeChanged) => "gp-status-icon modified",
        None => "gp-status-icon",
    }
}

fn view_diff(root: ProjectRootPath, scope: ProjectDiffScope, path: String) {
    let state = expect_context::<AppState>();
    let label = format!(
        "Diff: {}/{}",
        root_display_name(&root),
        path.rsplit('/').next().unwrap_or(&path)
    );
    state.open_tab(
        crate::state::TabContent::Diff {
            root: root.clone(),
            scope,
        },
        label,
        true,
    );

    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let context_mode = state.diff_context_mode.get_untracked();

    // Insert a pending DiffViewState BEFORE dispatching. This is the source of
    // truth for "what was most recently requested" — the reactive re-request
    // effect compares the signal against this entry's `context_mode`, and the
    // dispatch reducer rejects responses that don't match it. Without this,
    // a context-mode flip before the first response arrives would leave the
    // view empty with nothing to re-dispatch against.
    let key = (root.clone(), scope);
    state.diff_contents.update(|diffs| {
        let previous = diffs.get(&key);
        let next = DiffViewState::for_request(
            previous,
            root.clone(),
            scope,
            Some(path.clone()),
            context_mode,
        );
        diffs.insert(key, next);
    });

    spawn_local(async move {
        let payload = ProjectReadDiffPayload {
            root,
            scope,
            path: Some(path),
            context_mode,
        };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectReadDiff,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectReadDiff: {e}");
        }
    });
}

fn stage_file(root: ProjectRootPath, path: String) {
    let state = expect_context::<AppState>();

    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));

    spawn_local(async move {
        let payload = ProjectStageFilePayload {
            path: ProjectPath {
                root,
                relative_path: path,
            },
        };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectStageFile,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectStageFile: {e}");
        }
    });
}

fn unstage_file(root: ProjectRootPath, path: String) {
    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        let payload = ProjectUnstageFilePayload {
            path: ProjectPath {
                root,
                relative_path: path,
            },
        };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectUnstageFile,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectUnstageFile: {e}");
        }
    });
}

fn discard_file(root: ProjectRootPath, path: String) {
    let Some(win) = window() else { return };
    let message = format!("Discard changes to \"{}\"? This cannot be undone.", path);
    match win.confirm_with_message(&message) {
        Ok(true) => {}
        _ => return,
    }

    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        let payload = ProjectDiscardFilePayload {
            path: ProjectPath {
                root,
                relative_path: path,
            },
        };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectDiscardFile,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectDiscardFile: {e}");
        }
    });
}

fn send_commit(root: ProjectRootPath, message: String) {
    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        let payload = ProjectGitCommitPayload { root, message };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectGitCommit,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectGitCommit: {e}");
        }
    });
}

async fn send_project_refresh(state: &AppState) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = ProjectRefreshPayload {};
    if let Err(e) = send_frame(
        &active_project.host_id,
        stream,
        FrameKind::ProjectRefresh,
        &payload,
    )
    .await
    {
        log::error!("failed to send ProjectRefresh: {e}");
    }
}
