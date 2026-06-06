use std::collections::HashMap;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::review_view::ReviewSidebar;
use crate::send::send_frame;
use crate::state::{AppState, DiffViewState, root_display_name};

use protocol::{
    FrameKind, ProjectDiffScope, ProjectDiscardFilePayload, ProjectGitChangeKind,
    ProjectGitCommitPayload, ProjectGitFileStatus, ProjectId, ProjectPath, ProjectReadDiffPayload,
    ProjectRootGitStatus, ProjectRootPath, ProjectStageFilePayload, ProjectUnstageFilePayload,
    ReviewId, ReviewStatus, StreamPath,
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

/// Per-root always-on review controls. Finds the Draft review for
/// `(project_id, root)` in `review_summaries`, subscribes to it, and
/// surfaces compact hub controls (AI Review button, live counts, Submit).
/// No "Start review" or "Cancel" buttons — reviews are always-on and
/// managed by the server.
#[component]
fn RootReviewControls(
    host_id: String,
    project_id: ProjectId,
    root: ProjectRootPath,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Find the Draft review for this (project_id, root).
    let find_state = state.clone();
    let find_root = root.clone();
    let find_pid = project_id.clone();
    let draft: Memo<Option<(String, ReviewId)>> = {
        let host = host_id.clone();
        Memo::new(move |_| {
            find_state.review_summaries.with(|map| {
                map.get(&find_pid).and_then(|summaries| {
                    summaries
                        .iter()
                        .filter(|s| s.root == find_root && matches!(s.status, ReviewStatus::Draft))
                        .max_by_key(|s| s.updated_at_ms)
                        .map(|s| (host.clone(), s.id.clone()))
                })
            })
        })
    };

    let hub_pid = project_id.clone();
    let hub_root = root.clone();
    view! {
        {move || draft.get().map(|(h, rid)| view! {
            <RootReviewHub
                host_id=h
                project_id=hub_pid.clone()
                root=hub_root.clone()
                review_id=rid
            />
        })}
    }
}

/// Draft-review hub for a single root inside the git panel. Subscribes to
/// the review, shows live counts, an "Open changes" button, and the shared
/// `ReviewSidebar` (AI reviewer / Submit) once the record arrives.
#[component]
fn RootReviewHub(
    host_id: String,
    project_id: ProjectId,
    root: ProjectRootPath,
    review_id: ReviewId,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // The "Open changes" button opens *this* root's unstaged diff, not the
    // project's first dirty root — multi-root projects must land on the root
    // whose hub was clicked.
    let open_state = state.clone();
    let open_host = host_id.clone();
    let open_pid = project_id.clone();
    let open_root = root.clone();

    // Keep the review subscribed so `ReviewSidebar` can mount with the
    // full record. The shared helper retries on send failure / record
    // loss / reconnect.
    {
        let host = host_id.clone();
        let rid = review_id.clone();
        let target: Memo<Option<(String, ReviewId)>> =
            Memo::new(move |_| Some((host.clone(), rid.clone())));
        crate::components::review_view::subscribe_review_reactive(&state, target);
    }

    // Live counts: prefer the full record, fall back to the summary list.
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
        <div class="gp-review-hub" data-test="gp-root-review-hub">
            <div class="gp-review-hub-header">
                <span class="gp-review-hub-title">"Review"</span>
                <span class="gp-review-counts" data-test="gp-root-review-counts">
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
                data-test="gp-root-review-open"
                title="Open the review comments for this root"
                on:click=move |_| {
                    crate::components::review_view::open_comments_for_root(
                        &open_state,
                        &open_host,
                        &open_pid,
                        &open_root,
                    )
                }
            >
                "Comments"
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
    let root_for_commit = root_path.clone();
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

    // Per-root review controls: binds to (project_id, root) from the active project.
    let state = expect_context::<AppState>();
    let review_root = root_path.clone();
    let review_project_signal: Memo<Option<(String, ProjectId, ProjectRootPath)>> =
        Memo::new(move |_| {
            state.active_project.get().map(|ap| {
                (
                    ap.host_id.clone(),
                    ap.project_id.clone(),
                    review_root.clone(),
                )
            })
        });

    // Per-file review-comment counts for this root's draft review. Drives the
    // "(N)" badges on the file rows. Prefers the server-computed per-file
    // counts on the draft `ReviewSummary` (available without a full review
    // subscribe); falls back to computing from the loaded `Review` when no
    // summary is present yet.
    let counts_state = state.clone();
    let counts_root = root_path.clone();
    let file_counts: Memo<HashMap<String, u32>> = Memo::new(move |_| {
        let Some(ap) = counts_state.active_project.get() else {
            return HashMap::new();
        };
        let summary = counts_state.review_summaries.with(|map| {
            map.get(&ap.project_id).and_then(|summaries| {
                summaries
                    .iter()
                    .filter(|s| s.root == counts_root && matches!(s.status, ReviewStatus::Draft))
                    .max_by_key(|s| s.updated_at_ms)
                    .map(|s| (s.id.clone(), s.file_comment_counts.clone()))
            })
        });
        let Some((rid, file_comment_counts)) = summary else {
            return HashMap::new();
        };
        if !file_comment_counts.is_empty() {
            return file_comment_counts
                .iter()
                .map(|f| (f.relative_path.clone(), f.total_count()))
                .collect();
        }
        // Summary carries no per-file counts (older server, or counts not yet
        // populated) — fall back to the loaded review record if present.
        counts_state
            .reviews
            .with(|m| m.get(&rid).map(per_file_comment_counts).unwrap_or_default())
    });

    view! {
        <div class="gp-root-section">
            <div class="gp-root-header" title=root_title>
                <span class="gp-root-name">{root_label}</span>
                <span class="gp-root-branch">{branch_label}</span>
                {ahead_behind.map(|ab| view! {
                    <span class="gp-root-ahead-behind">{ab}</span>
                })}
            </div>
            {move || review_project_signal.get().map(|(host_id, project_id, root)| view! {
                <RootReviewControls host_id=host_id project_id=project_id root=root />
            })}
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
                    file_counts=file_counts
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
                    file_counts=file_counts
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
                    file_counts=file_counts
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
    file_counts: Memo<HashMap<String, u32>>,
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
                        let path_for_badge = path.clone();
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
                                    {move || {
                                        let n = file_counts
                                            .get()
                                            .get(&path_for_badge)
                                            .copied()
                                            .unwrap_or(0);
                                        (n > 0).then(|| view! {
                                            <span
                                                class="gp-file-comment-count"
                                                data-test="gp-file-comment-count"
                                                title="Review comments"
                                            >
                                                {format!("({n})")}
                                            </span>
                                        })
                                    }}
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

/// Per-file review-comment counts keyed by `relative_path`. Counts every
/// comment (human comments and accepted AI suggestions, which the server
/// promotes into `comments`) plus pending AI suggestions. Rejected
/// suggestions are excluded — they are neither `Pending` nor promoted to a
/// comment.
///
/// Computed from the loaded `Review` as a fallback until `ReviewSummary`
/// carries per-file counts directly; swapping to the summary field is a
/// single change at the call site in `GitRootSection`.
fn per_file_comment_counts(review: &protocol::Review) -> HashMap<String, u32> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for c in &review.comments {
        *counts.entry(c.location.relative_path.clone()).or_insert(0) += 1;
    }
    for s in &review.suggestions {
        if matches!(s.state, protocol::ReviewSuggestionState::Pending) {
            *counts.entry(s.location.relative_path.clone()).or_insert(0) += 1;
        }
    }
    counts
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

    fn root_with_unstaged(path: &str) -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath(path.to_owned()),
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
            root: ProjectRootPath("/repo".to_owned()),
            status: ReviewStatus::Draft,
            origin_session_id: SessionId("s".to_owned()),
            origin_agent_id: AgentId("project-review:rev-1".to_owned()),
            created_at_ms: 0,
            updated_at_ms: 1,
            user_comment_count: 1,
            pending_suggestion_count: 0,
            file_comment_counts: vec![],
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

    /// No draft review + uncommitted changes ⇒ the git panel does NOT show a
    /// per-root review hub (there is no draft to bind to).
    #[wasm_bindgen_test]
    async fn no_draft_shows_review_changes_control() {
        let container = make_container();
        let _ = mount_git_panel(container.clone(), false);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"gp-root-review-hub\"]")
                .unwrap()
                .is_none(),
            "per-root review hub must not show without a draft review"
        );
    }

    /// A Draft review ⇒ the git panel shows a per-root review hub with live counts.
    #[wasm_bindgen_test]
    async fn draft_shows_review_hub_with_counts() {
        let container = make_container();
        let _ = mount_git_panel(container.clone(), true);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"gp-root-review-hub\"]")
                .unwrap()
                .is_some(),
            "expected the per-root review hub with a draft review present"
        );
        let counts = container
            .query_selector("[data-test=\"gp-root-review-counts\"]")
            .unwrap()
            .expect("counts element present");
        let text = counts.text_content().unwrap_or_default();
        assert!(
            text.contains("1 comment"),
            "expected the summary comment count in the hub; got: {text}"
        );
    }

    /// A file with review comments shows a per-file "(N)" badge in the file
    /// list. `mount_git_panel` seeds one User comment on `src/foo.rs`; with
    /// the draft summary carrying no per-file counts, the badge derives from
    /// the loaded review record (the fallback path).
    #[wasm_bindgen_test]
    async fn file_row_shows_comment_count_badge() {
        let container = make_container();
        let _ = mount_git_panel(container.clone(), true);
        next_tick().await;
        next_tick().await;

        let badge = container
            .query_selector("[data-test=\"gp-file-comment-count\"]")
            .unwrap()
            .expect("a comment-count badge must render for a file with comments");
        let text = badge.text_content().unwrap_or_default();
        assert!(
            text.contains("(1)"),
            "badge must show the per-file comment count; got: {text}"
        );
    }

    /// No draft review ⇒ no per-file badges at all.
    #[wasm_bindgen_test]
    async fn file_row_has_no_badge_without_review() {
        let container = make_container();
        let _ = mount_git_panel(container.clone(), false);
        next_tick().await;
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"gp-file-comment-count\"]")
                .unwrap()
                .is_none(),
            "no comment-count badge without a draft review"
        );
    }

    /// Multi-root: clicking root B's "Comments" opens root B's comments
    /// surface, not the project's first dirty root (A). Regression for the hub
    /// dropping the root and falling back to a project-first-root opener.
    #[wasm_bindgen_test]
    async fn root_hub_comments_opens_clicked_root() {
        // Opening the comments surface only pushes a tab (no diff fetch), but
        // keep the recording bridge so any incidental invoke resolves cleanly
        // in headless Chrome.
        stub_recording_bridge();
        let container = make_container();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h1".to_owned(),
                project_id: ProjectId("proj-1".to_owned()),
            }));
            // Two dirty roots; A is first, so a first-root opener would pick A.
            state.git_status.update(|m| {
                m.insert(
                    ProjectId("proj-1".to_owned()),
                    vec![root_with_unstaged("/repo-a"), root_with_unstaged("/repo-b")],
                );
            });
            // A Draft review only for root B ⇒ only B's hub (and button) render.
            let mut summary = draft_summary();
            summary.id = ReviewId("rev-b".to_owned());
            summary.root = ProjectRootPath("/repo-b".to_owned());
            state.review_summaries.update(|m| {
                m.insert(ProjectId("proj-1".to_owned()), vec![summary]);
            });
            // Seed the full record so the hub doesn't network-subscribe.
            let mut review = full_review();
            review.id = ReviewId("rev-b".to_owned());
            review.selection = protocol::ReviewDiffSelection::Root {
                root: ProjectRootPath("/repo-b".to_owned()),
                scope: protocol::ProjectDiffScope::Unstaged,
                path: None,
            };
            state.reviews.update(|m| {
                m.insert(ReviewId("rev-b".to_owned()), review);
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <GitPanel /> }
        });
        std::mem::forget(handle);
        next_tick().await;

        // Only root B has a draft, so there is exactly one Comments button.
        let open_btn = container
            .query_selector("[data-test=\"gp-root-review-open\"]")
            .unwrap()
            .expect("root B's Comments button must render");
        open_btn.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        let opened = state.center_zone.with_untracked(|cz| {
            cz.tabs.iter().find_map(|t| match &t.content {
                TabContent::Comments { root, .. } => Some(root.clone()),
                _ => None,
            })
        });
        let root = opened.expect("a comments tab must open on Comments");
        assert_eq!(
            root,
            ProjectRootPath("/repo-b".to_owned()),
            "Comments must open the clicked root (B), not the first dirty root (A)"
        );
    }

    /// The create flow (server echoes `ReviewListChanged` for a pending
    /// create) must NOT auto-open any review surface tab — it only releases
    /// the pending token. Reviews live on the normal diff surfaces now, and
    /// the standalone review-workbench tab has been removed entirely. Driven
    /// through `dispatch_envelope` so no network is touched.
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
        // … and dispatch did not auto-open any diff surface tab (only an
        // explicit click handler opens the changed-file diff; the standalone
        // review-workbench tab no longer exists at all).
        let opened_surface = state.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .any(|t| matches!(t.content, TabContent::Diff { .. }))
        });
        assert!(
            !opened_surface,
            "ReviewListChanged must not auto-open a diff surface tab"
        );
        // The summary list was still folded in.
        let known = state
            .review_summaries
            .with_untracked(|m| m.get(&ProjectId("proj-1".to_owned())).map(|v| v.len()))
            .unwrap_or(0);
        assert_eq!(known, 1, "the review summary should be recorded");
    }

    /// Regression: a fallback `ReviewCreate` resolves to an *existing* draft,
    /// and a `ProjectBootstrap` (reconnect / re-subscribe) folds that draft
    /// summary into `review_summaries` before the server's `ReviewListChanged`
    /// echo is handled. The echo then carries no *new* id, but the pending
    /// create token must still be released — otherwise the "Review changes"
    /// button wedges forever (a successful create emits no `CommandError`).
    #[wasm_bindgen_test]
    async fn create_flow_releases_pending_without_new_id() {
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
        crate::dispatch::prime_host_for_tests(&state, "h1");
        let project_stream = StreamPath("/project/proj-1".to_owned());
        // Bootstrap already carries the existing draft summary — this models
        // the race where the draft is folded into state before the echo lands.
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
                review_summaries: vec![draft_summary()],
            },
        )
        .expect("synthetic ProjectBootstrap");
        crate::dispatch::dispatch_envelope(&state, "h1", bootstrap_env);

        // The user fired a fallback create (state lacked the draft at click
        // time); its token is in flight.
        let key = ("h1".to_owned(), ProjectId("proj-1".to_owned()));
        state.review_create_pending.update(|m| {
            m.insert(key.clone(), 1);
        });

        // The echo confirms the same already-known draft — `new_ids` is empty.
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

        let pending = state
            .review_create_pending
            .with_untracked(|m| m.get(&key).copied().unwrap_or(0));
        assert_eq!(
            pending, 0,
            "create-pending token must release even with no new id"
        );
        // Still no auto-opened diff surface tab — behavior is unchanged.
        let opened_surface = state.center_zone.with_untracked(|cz| {
            cz.tabs
                .iter()
                .any(|t| matches!(t.content, TabContent::Diff { .. }))
        });
        assert!(
            !opened_surface,
            "ReviewListChanged must not auto-open a diff surface tab"
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
                <RootReviewHub
                    host_id="h1".to_owned()
                    project_id=ProjectId("proj-1".to_owned())
                    root=ProjectRootPath("/repo".to_owned())
                    review_id=review_for_mount.clone()
                />
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
                <RootReviewHub
                    host_id="h1".to_owned()
                    project_id=ProjectId("proj-1".to_owned())
                    root=ProjectRootPath("/repo".to_owned())
                    review_id=ReviewId("rev-1".to_owned())
                />
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
                <RootReviewHub
                    host_id="h1".to_owned()
                    project_id=ProjectId("proj-1".to_owned())
                    root=ProjectRootPath("/repo".to_owned())
                    review_id=ReviewId("rev-1".to_owned())
                />
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
