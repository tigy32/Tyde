use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, DiffViewState};

use protocol::{
    FrameKind, ProjectDiffScope, ProjectGitChangeKind, ProjectGitFileStatus, ProjectPath,
    ProjectReadDiffPayload, ProjectRefreshPayload, ProjectRootGitStatus, ProjectRootPath,
    ProjectStageFilePayload, StreamPath,
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
                            .and_then(|roots| roots.first().cloned())
                            .and_then(|r| r.branch.clone())
                            .map(|b| format!("\u{238b} {b}"))
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
    let root_for_untracked = root_path;

    view! {
        <div class="gp-root-section">
            <Show when=move || has_staged>
                <GitFileSection
                    title=format!("Staged Changes [{staged_count}]")
                    files=staged.clone()
                    expanded=staged_expanded
                    scope=ProjectDiffScope::Staged
                    root_path=root_for_staged.clone()
                    show_stage_btn=false
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
) -> impl IntoView {
    let toggle = move |_| expanded.update(|v| *v = !*v);

    view! {
        <div class="gp-section">
            <button class="gp-section-header" on:click=toggle>
                <span class="fe-chevron">{move || if expanded.get() { "\u{25be}" } else { "\u{25b8}" }}</span>
                <span class="gp-section-title">{title}</span>
            </button>
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
    let label = format!("Diff: {}", path.rsplit('/').next().unwrap_or(&path));
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
