use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::review_view::ReviewSidebar;
use crate::send::send_frame;
use crate::state::{AppState, DiffViewState, root_display_name};

use protocol::{
    FrameKind, ProjectDiffScope, ProjectDiscardFilePayload, ProjectGitChangeKind,
    ProjectGitCommitPayload, ProjectGitFileStatus, ProjectPath, ProjectReadDiffPayload,
    ProjectRootGitStatus, ProjectRootPath, ProjectStageFilePayload, ProjectUnstageFilePayload,
    StreamPath,
};

#[component]
pub fn GitPanel() -> impl IntoView {
    let state = expect_context::<AppState>();

    let git_roots = Memo::new(move |_| {
        let pid = state.active_project.get()?.project_id;
        let map = state.git_status.get();
        map.get(&pid).cloned()
    });

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
            </div>
            <ReviewIndicator />
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

/// Compact review hub for the git panel. With no Draft review it offers a
/// "Review changes" control that starts a project-scoped review AND opens a
/// normal changed-file diff tab to comment on (it does NOT open a separate
/// `ReviewView` workbench). With a Draft it surfaces the live counts plus
/// the full review controls (AI reviewer, Clear, Submit) by reusing
/// `ReviewSidebar`, so submit-target gating stays exactly correct.
#[component]
fn ReviewIndicator() -> impl IntoView {
    let state = expect_context::<AppState>();
    let draft_state = state.clone();
    let draft: Memo<Option<(String, protocol::ReviewId)>> = Memo::new(move |_| {
        crate::components::review_view::open_review_for_active_project(&draft_state)
    });

    let changes_state = state.clone();
    let has_changes = move || {
        let Some(active) = changes_state.active_project.get() else {
            return false;
        };
        changes_state.git_status.with(|map| {
            map.get(&active.project_id)
                .map(|roots| roots.iter().any(|r| !r.clean))
                .unwrap_or(false)
        })
    };

    let pending_state = state.clone();
    let create_pending = move || {
        let Some(active) = pending_state.active_project.get() else {
            return false;
        };
        pending_state.review_create_pending.with(|m| {
            m.get(&(active.host_id.clone(), active.project_id.clone()))
                .copied()
                .unwrap_or(0)
                > 0
        })
    };

    view! {
        {move || match draft.get() {
            None => {
                // No draft: only surface the control when there are
                // uncommitted changes worth reviewing.
                if !has_changes() {
                    return None;
                }
                let pending = create_pending();
                Some(view! {
                    <div class="gp-review-hub">
                        <button
                            class="gp-review-indicator"
                            data-test="gp-review-changes"
                            disabled=pending
                            title="Start an inline review of the uncommitted changes"
                            on:click=move |_| {
                                // Opens the changed-file diff surface and
                                // creates the review (never a Review tab).
                                let state = expect_context::<AppState>();
                                crate::components::review_view::create_or_open_review_for_active_project(
                                    &state,
                                );
                            }
                        >
                            <svg class="gp-review-indicator-icon" viewBox="0 0 16 16" fill="none"
                                 stroke="currentColor" stroke-width="1.5" stroke-linecap="round"
                                 stroke-linejoin="round" aria-hidden="true">
                                <path d="M3 2.5h7l3 3V13a.5.5 0 0 1-.5.5h-9.5A.5.5 0 0 1 2.5 13V3a.5.5 0 0 1 .5-.5z" />
                                <path d="M10 2.5V6h3" />
                                <path d="M5.5 9.25l1.5 1.5L11 7.5" />
                            </svg>
                            <span class="gp-review-indicator-label">"Review changes"</span>
                        </button>
                    </div>
                }.into_any())
            }
            Some((host_id, review_id)) => Some(view! {
                <GitReviewHub host_id=host_id review_id=review_id />
            }.into_any()),
        }}
    }
}

/// Draft-review controls for the git panel. Subscribes to the review so the
/// full record loads, shows the live summary counts, an "Open changes"
/// button that jumps to the changed-file diff surface, and the shared
/// `ReviewSidebar` (AI reviewer / Clear / Submit) once the record arrives.
#[component]
fn GitReviewHub(host_id: String, review_id: protocol::ReviewId) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Reactively keep the review subscribed so `ReviewSidebar` can mount
    // with the full record. The shared helper retries on send failure /
    // record loss / reconnect (see `subscribe_review_reactive`).
    {
        let host = host_id.clone();
        let rid = review_id.clone();
        let target: Memo<Option<(String, protocol::ReviewId)>> =
            Memo::new(move |_| Some((host.clone(), rid.clone())));
        crate::components::review_view::subscribe_review_reactive(&state, target);
    }

    // Live counts: prefer the full record, fall back to the summary list so
    // counts show before the snapshot lands. The fallback searches summaries
    // by `review_id` across all projects rather than keying off the globally
    // active project, so the hub's counts stay correct regardless of which
    // project is active.
    let counts_state = state.clone();
    let counts_rid = review_id.clone();
    let counts: Memo<(u32, u32)> = Memo::new(move |_| {
        if let Some((c, s)) = counts_state.reviews.with(|m| {
            m.get(&counts_rid).map(|r| {
                (
                    r.comments.len() as u32,
                    r.suggestions
                        .iter()
                        .filter(|s| matches!(s.state, protocol::ReviewSuggestionState::Pending))
                        .count() as u32,
                )
            })
        }) {
            return (c, s);
        }
        counts_state
            .review_summaries
            .with(|m| {
                m.values().find_map(|sums| {
                    sums.iter()
                        .find(|s| s.id == counts_rid)
                        .map(|s| (s.user_comment_count, s.pending_suggestion_count))
                })
            })
            .unwrap_or((0, 0))
    });

    let loaded_state = state.clone();
    let loaded_rid = review_id.clone();
    let loaded: Memo<bool> =
        Memo::new(move |_| loaded_state.reviews.with(|m| m.contains_key(&loaded_rid)));

    let isdraft_state = state.clone();
    let isdraft_rid = review_id.clone();
    let is_draft: Memo<bool> = Memo::new(move |_| {
        isdraft_state.reviews.with(|m| {
            m.get(&isdraft_rid)
                .map(|r| matches!(r.status, protocol::ReviewStatus::Draft))
                .unwrap_or(true)
        })
    });

    let sidebar_state = state.clone();
    let sidebar_host = host_id.clone();
    let sidebar_rid = review_id.clone();

    view! {
        <div class="gp-review-hub" data-test="gp-review-hub">
            <div class="gp-review-hub-header">
                <span class="gp-review-hub-title">"Review"</span>
                <span class="gp-review-counts" data-test="gp-review-counts">
                    {move || {
                        let (c, s) = counts.get();
                        format!(
                            "{c} comment{} \u{00b7} {s} AI",
                            if c == 1 { "" } else { "s" },
                        )
                    }}
                </span>
            </div>
            <button
                class="gp-review-open-btn"
                data-test="gp-review-open"
                title="Open the changed files to review"
                on:click=move |_| open_first_changed_diff()
            >
                "Open changes"
            </button>
            {move || {
                if !loaded.get() {
                    return view! {
                        <div class="gp-review-loading">"Loading review\u{2026}"</div>
                    }.into_any();
                }
                let seed = sidebar_state.reviews.with_untracked(|m| m.get(&sidebar_rid).cloned());
                match seed {
                    Some(review) => view! {
                        <ReviewSidebar
                            review=review
                            host_id=sidebar_host.clone()
                            review_id=sidebar_rid.clone()
                            is_draft=is_draft
                        />
                    }.into_any(),
                    None => view! { <div></div> }.into_any(),
                }
            }}
        </div>
    }
}

/// Open a normal diff tab for the first changed file in the active
/// project — the review comment surface. Delegates to the shared
/// `open_changed_diff_for_project` so the git panel, the chat header CTA,
/// and the diff-tab banner all land on the same surface.
fn open_first_changed_diff() {
    let state = expect_context::<AppState>();
    let Some(active) = state.active_project.get_untracked() else {
        return;
    };
    crate::components::review_view::open_changed_diff_for_project(
        &state,
        &active.host_id,
        &active.project_id,
    );
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
                            // Git panel only opens diffs in Staged/Unstaged scopes;
                            // Uncommitted is reserved for review snapshots.
                            ProjectDiffScope::Uncommitted => file.unstaged.or(file.staged),
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
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let perf_key = format!("diff:{}:{path}", root.0);
    crate::perf::mark_start(&perf_key);
    crate::perf::log_phase("diff_open", "click", &perf_key, "");
    let label = format!(
        "Diff: {}/{}",
        root_display_name(&root),
        path.rsplit('/').next().unwrap_or(&path)
    );
    state.open_tab(
        crate::state::TabContent::Diff {
            host_id: active_project.host_id.clone(),
            project_id: active_project.project_id.clone(),
            root: root.clone(),
            scope,
            path: path.clone(),
        },
        label,
        true,
    );

    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let context_mode = state.diff_context_mode.get_untracked();

    // Insert a pending DiffViewState BEFORE dispatching. This is the source of
    // truth for "what was most recently requested" — the reactive re-request
    // effect compares the signal against this entry's `context_mode`, and the
    // dispatch reducer rejects responses that don't match it. Without this,
    // a context-mode flip before the first response arrives would leave the
    // view empty with nothing to re-dispatch against.
    let key = crate::state::DiffKey::new(
        active_project.host_id.clone(),
        project_id.clone(),
        root.clone(),
        scope,
        path.clone(),
    );
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
    let message = format!("Discard changes to \"{}\"? This cannot be undone.", path);

    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        if !crate::bridge::confirm_dialog("Discard changes", &message).await {
            return;
        }
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveProjectRef, TabContent};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, Envelope, FrameKind, Project, ProjectBootstrapPayload, ProjectEventPayload,
        ProjectFileListPayload, ProjectGitChangeKind, ProjectGitFileStatus,
        ProjectGitStatusPayload, ProjectId, ProjectRootGitStatus, ReviewId, ReviewStatus,
        ReviewSummary, SessionId,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
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

    fn changed_root() -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath("/repo".to_owned()),
            branch: Some("main".to_owned()),
            ahead: 0,
            behind: 0,
            clean: false,
            files: vec![ProjectGitFileStatus {
                relative_path: "src/foo.rs".to_owned(),
                staged: None,
                unstaged: Some(ProjectGitChangeKind::Modified),
                untracked: false,
            }],
        }
    }

    fn draft_summary() -> ReviewSummary {
        ReviewSummary {
            id: ReviewId("rev-1".to_owned()),
            status: ReviewStatus::Draft,
            origin_session_id: SessionId("s".to_owned()),
            origin_agent_id: AgentId("project-review:rev-1".to_owned()),
            created_at_ms: 0,
            updated_at_ms: 1,
            user_comment_count: 1,
            pending_suggestion_count: 0,
        }
    }

    fn full_review() -> protocol::Review {
        use protocol::*;
        Review {
            id: ReviewId("rev-1".to_owned()),
            project_id: ProjectId("proj-1".to_owned()),
            origin_agent_id: AgentId("project-review:rev-1".to_owned()),
            origin_session_id: SessionId("s".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![],
            comments: vec![ReviewComment {
                id: ReviewCommentId("c1".to_owned()),
                location: ReviewLocation {
                    root: ProjectRootPath("/repo".to_owned()),
                    relative_path: "src/foo.rs".to_owned(),
                    anchor: ReviewAnchor::File,
                },
                anchor_status: ReviewAnchorStatus::Current,
                body: "note".to_owned(),
                source: ReviewCommentSource::User,
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            suggestions: vec![],
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn mount_git_panel(container: HtmlElement, with_draft: bool) -> Rc<RefCell<Option<AppState>>> {
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h1".to_owned(),
                project_id: ProjectId("proj-1".to_owned()),
            }));
            state.git_status.update(|m| {
                m.insert(ProjectId("proj-1".to_owned()), vec![changed_root()]);
            });
            if with_draft {
                state.review_summaries.update(|m| {
                    m.insert(ProjectId("proj-1".to_owned()), vec![draft_summary()]);
                });
                // Seed the full record so the hub does not fire a network
                // subscribe (which the headless bridge can't satisfy).
                state.reviews.update(|m| {
                    m.insert(ReviewId("rev-1".to_owned()), full_review());
                });
            }
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <GitPanel /> }
        });
        std::mem::forget(handle);
        holder
    }

    /// No draft review + uncommitted changes ⇒ the git panel surfaces the
    /// "Review changes" control (and not the draft hub).
    #[wasm_bindgen_test]
    async fn no_draft_shows_review_changes_control() {
        let container = make_container();
        let _ = mount_git_panel(container.clone(), false);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"gp-review-changes\"]")
                .unwrap()
                .is_some(),
            "expected a 'Review changes' control with uncommitted changes"
        );
        assert!(
            container
                .query_selector("[data-test=\"gp-review-hub\"]")
                .unwrap()
                .is_none(),
            "draft hub must not show without a draft review"
        );
    }

    /// A Draft review ⇒ the git panel shows the review hub with live counts.
    #[wasm_bindgen_test]
    async fn draft_shows_review_hub_with_counts() {
        let container = make_container();
        let _ = mount_git_panel(container.clone(), true);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"gp-review-hub\"]")
                .unwrap()
                .is_some(),
            "expected the review hub with a draft review present"
        );
        let counts = container
            .query_selector("[data-test=\"gp-review-counts\"]")
            .unwrap()
            .expect("counts element present");
        let text = counts.text_content().unwrap_or_default();
        assert!(
            text.contains("1 comment"),
            "expected the summary comment count in the hub; got: {text}"
        );
    }

    /// The create flow (server echoes `ReviewListChanged` for a pending
    /// create) must NOT open a standalone `TabContent::Review` workbench —
    /// it only releases the pending token. Reviews live on the normal diff
    /// surfaces now. Driven through `dispatch_envelope` so no network is
    /// touched.
    #[wasm_bindgen_test]
    async fn create_flow_does_not_open_review_tab() {
        let container = make_container();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <div></div> }
        });
        std::mem::forget(handle);
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        // Prime the host validators, then bootstrap the project stream so a
        // follow-up `ProjectEvent` passes the bootstrap-first protocol check.
        crate::dispatch::prime_host_for_tests(&state, "h1");
        let project_stream = StreamPath("/project/proj-1".to_owned());
        let bootstrap_env = Envelope::from_payload(
            project_stream.clone(),
            FrameKind::ProjectBootstrap,
            0,
            &ProjectBootstrapPayload {
                project: Project {
                    id: ProjectId("proj-1".to_owned()),
                    name: "proj".to_owned(),
                    roots: vec!["/repo".to_owned()],
                    sort_order: 0,
                },
                file_list: ProjectFileListPayload {
                    incremental: false,
                    roots: vec![],
                },
                git_status: ProjectGitStatusPayload { roots: vec![] },
                review_summaries: vec![],
            },
        )
        .expect("synthetic ProjectBootstrap");
        crate::dispatch::dispatch_envelope(&state, "h1", bootstrap_env);

        let key = ("h1".to_owned(), ProjectId("proj-1".to_owned()));
        state.review_create_pending.update(|m| {
            m.insert(key.clone(), 1);
        });

        let env = Envelope::from_payload(
            project_stream,
            FrameKind::ProjectEvent,
            1,
            &ProjectEventPayload::ReviewListChanged {
                reviews: vec![draft_summary()],
            },
        )
        .expect("synthetic ReviewListChanged");
        crate::dispatch::dispatch_envelope(&state, "h1", env);

        // The pending token is released …
        let pending = state
            .review_create_pending
            .with_untracked(|m| m.get(&key).copied().unwrap_or(0));
        assert_eq!(pending, 0, "create-pending token must be released");
        // … and no standalone review workbench tab was opened.
        let has_review = state.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .any(|t| matches!(t.content, TabContent::Review { .. }))
        });
        assert!(
            !has_review,
            "ReviewListChanged must not open a standalone Review tab"
        );
        // The summary list was still folded in.
        let known = state
            .review_summaries
            .with_untracked(|m| m.get(&ProjectId("proj-1".to_owned())).map(|v| v.len()))
            .unwrap_or(0);
        assert_eq!(known, 1, "the review summary should be recorded");
    }

    /// Stub the Tauri bridge global so `send_frame` resolves `Ok` instead
    /// of throwing on the missing `window.__TAURI__` (which would abort the
    /// headless test). Resolving (not rejecting) keeps the create-pending
    /// gate set — a rejection would trip the send-failure path that clears
    /// it. Lets us exercise CTAs that dispatch frames without a backend.
    fn stub_tauri_bridge() {
        let _ = js_sys::eval(
            "(function(){ \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(){ return Promise.resolve(); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
    }

    /// The "Review changes" CTA opens a normal changed-file Diff tab (the
    /// comment surface) and starts a review — never a standalone Review tab.
    #[wasm_bindgen_test]
    async fn cta_opens_diff_surface_and_starts_review() {
        stub_tauri_bridge();
        let container = make_container();
        let holder = mount_git_panel(container.clone(), false);
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        crate::components::review_view::create_or_open_review_for_active_project(&state);
        next_tick().await;

        let (all_uncommitted_diff, has_review) = state.center_zone.with_untracked(|cz| {
            (
                cz.tabs.iter().any(|t| {
                    matches!(
                        &t.content,
                        TabContent::Diff { scope, path, .. }
                            // Uncommitted scope + empty path = the whole-root
                            // all-files surface (every changed file in the
                            // review can display and accept comments).
                            if *scope == ProjectDiffScope::Uncommitted && path.is_empty()
                    )
                }),
                cz.tabs
                    .iter()
                    .any(|t| matches!(t.content, TabContent::Review { .. })),
            )
        });
        assert!(
            all_uncommitted_diff,
            "CTA must open the all-uncommitted (empty-path) Diff surface for review"
        );
        assert!(!has_review, "CTA must not open a standalone Review tab");

        let pending = state.review_create_pending.with_untracked(|m| {
            m.get(&("h1".to_owned(), ProjectId("proj-1".to_owned())))
                .copied()
                .unwrap_or(0)
        });
        assert!(pending > 0, "a ReviewCreate should be in flight");
    }

    /// With an existing Draft, the CTA still opens the diff surface but does
    /// NOT open a Review tab and does NOT fire a duplicate create.
    #[wasm_bindgen_test]
    async fn cta_with_existing_draft_opens_diff_without_recreate() {
        stub_tauri_bridge();
        let container = make_container();
        let holder = mount_git_panel(container.clone(), true);
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        crate::components::review_view::create_or_open_review_for_active_project(&state);
        next_tick().await;

        let (has_diff, has_review) = state.center_zone.with_untracked(|cz| {
            (
                cz.tabs
                    .iter()
                    .any(|t| matches!(t.content, TabContent::Diff { .. })),
                cz.tabs
                    .iter()
                    .any(|t| matches!(t.content, TabContent::Review { .. })),
            )
        });
        assert!(has_diff, "CTA must open the diff surface even with a draft");
        assert!(!has_review, "CTA must not open a standalone Review tab");

        let pending = state.review_create_pending.with_untracked(|m| {
            m.get(&("h1".to_owned(), ProjectId("proj-1".to_owned())))
                .copied()
                .unwrap_or(0)
        });
        assert_eq!(
            pending, 0,
            "an existing draft must not trigger a new create"
        );
    }

    /// Recording bridge stub: counts `invoke` calls (i.e. frame sends) in a
    /// global so a test can assert how many `ReviewSubscribe`s went out.
    fn stub_recording_bridge() {
        let _ = js_sys::eval(
            "(function(){ \
               window.__invoke_count = 0; \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(){ window.__invoke_count++; return Promise.resolve(); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
    }

    fn invoke_count() -> i32 {
        js_sys::eval("window.__invoke_count")
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as i32
    }

    /// `subscribe_review_reactive` must retry reactively: it subscribes when
    /// the record is absent, stays quiet while it's present, and
    /// **resubscribes when the record is later lost** (the bug fix — a
    /// `StoredValue` guard would have stayed latched and never resubscribed).
    #[wasm_bindgen_test]
    async fn hub_resubscribes_when_record_lost() {
        stub_recording_bridge();
        let review_id = ReviewId("rev-1".to_owned());
        let container = make_container();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let review_for_mount = review_id.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            // The hub only subscribes at a connected host.
            state.connection_statuses.update(|m| {
                m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <GitReviewHub host_id="h1".to_owned() review_id=review_for_mount.clone() />
            }
        });
        std::mem::forget(handle);
        next_tick().await;

        // Record absent ⇒ one subscribe.
        assert_eq!(
            invoke_count(),
            1,
            "the hub must subscribe while the record is absent"
        );

        let state = holder.borrow().clone().unwrap();
        // Record arrives ⇒ no further subscribe.
        state.reviews.update(|m| {
            m.insert(review_id.clone(), full_review());
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            1,
            "no resubscribe should fire while the record is present"
        );

        // Record is lost (e.g. cleared) ⇒ resubscribe.
        state.reviews.update(|m| {
            m.remove(&review_id);
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            2,
            "the hub must resubscribe after the record is lost"
        );
    }

    async fn sleep_ms(ms: i32) {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// A persistently-failing subscribe must NOT tight-loop: the first
    /// attempt fires, then retries are deferred behind a backoff timer, so a
    /// burst of microtasks does not spin out hundreds of sends. The backoff
    /// retry does eventually fire.
    #[wasm_bindgen_test]
    async fn hub_subscribe_failure_backs_off_no_tight_loop() {
        // Every invoke rejects — a tight loop would be observable as a large
        // count after the synchronous/microtask burst.
        let _ = js_sys::eval(
            "(function(){ \
               window.__invoke_count = 0; \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(){ \
                 window.__invoke_count++; return Promise.reject('boom'); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
        let container = make_container();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.connection_statuses.update(|m| {
                m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
            });
            provide_context(state);
            view! {
                <GitReviewHub host_id="h1".to_owned() review_id=ReviewId("rev-1".to_owned()) />
            }
        });
        std::mem::forget(handle);

        // Burst of microtasks: only the first attempt should have fired (the
        // retry is parked behind the ~250ms backoff timer, not re-issued
        // immediately).
        next_tick().await;
        next_tick().await;
        next_tick().await;
        assert_eq!(
            invoke_count(),
            1,
            "a failed subscribe must not re-issue immediately (tight loop)"
        );

        // After the first backoff window, the retry fires — and still does
        // not spin (the next retry sits behind a longer backoff).
        sleep_ms(400).await;
        let after = invoke_count();
        assert!(
            (2..=4).contains(&after),
            "backoff retry must fire but not tight-loop (got {after} attempts)"
        );
    }

    /// A subscribe that succeeded but never received a bootstrap must recover
    /// on reconnect: a disconnect clears the in-flight latch, and reconnect
    /// re-runs the effect and resubscribes.
    #[wasm_bindgen_test]
    async fn hub_resubscribes_on_reconnect() {
        stub_recording_bridge();
        let container = make_container();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.connection_statuses.update(|m| {
                m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <GitReviewHub host_id="h1".to_owned() review_id=ReviewId("rev-1".to_owned()) />
            }
        });
        std::mem::forget(handle);
        next_tick().await;
        // First subscribe (sent OK, but no bootstrap record arrives).
        assert_eq!(invoke_count(), 1, "initial subscribe");

        let state = holder.borrow().clone().unwrap();
        // Disconnect: the in-flight latch is dropped.
        state.connection_statuses.update(|m| {
            m.insert(
                "h1".to_owned(),
                crate::state::ConnectionStatus::Disconnected,
            );
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            1,
            "no subscribe should be sent while disconnected"
        );

        // Reconnect: resubscribe.
        state.connection_statuses.update(|m| {
            m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            2,
            "the hub must resubscribe after reconnecting"
        );
    }

    /// If the subscribe target temporarily becomes `None` (e.g. the owning
    /// project's draft resolves away, or `state.projects` is cleared on
    /// disconnect) after a subscribe that never received a bootstrap, the
    /// in-flight latch must be dropped so the same target reappearing
    /// resubscribes — it must not stay wedged.
    #[wasm_bindgen_test]
    async fn subscribe_resubscribes_when_target_disappears_and_returns() {
        stub_recording_bridge();
        let container = make_container();
        let holder: Rc<RefCell<Option<RwSignal<Option<(String, ReviewId)>>>>> =
            Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.connection_statuses.update(|m| {
                m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
            });
            let target_sig: RwSignal<Option<(String, ReviewId)>> =
                RwSignal::new(Some(("h1".to_owned(), ReviewId("rev-1".to_owned()))));
            let target: Memo<Option<(String, ReviewId)>> = Memo::new(move |_| target_sig.get());
            crate::components::review_view::subscribe_review_reactive(&state, target);
            *holder_for_mount.borrow_mut() = Some(target_sig);
            provide_context(state);
            view! { <div></div> }
        });
        std::mem::forget(handle);
        next_tick().await;
        assert_eq!(invoke_count(), 1, "initial subscribe");

        let target_sig = holder.borrow().clone().unwrap();
        // Target disappears (no bootstrap had arrived).
        target_sig.set(None);
        next_tick().await;
        assert_eq!(invoke_count(), 1, "no subscribe while the target is None");

        // Same target returns ⇒ must resubscribe (not stay latched).
        target_sig.set(Some(("h1".to_owned(), ReviewId("rev-1".to_owned()))));
        next_tick().await;
        assert_eq!(
            invoke_count(),
            2,
            "the same target reappearing must resubscribe"
        );
    }
}
