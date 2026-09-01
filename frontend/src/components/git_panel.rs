use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::components::review_view::ReviewSidebar;
use crate::send::send_frame;
use crate::state::{AppState, DiffKey, DiffViewState, next_client_request_id, root_display_name};

use protocol::{
    FrameKind, ProjectDiffRevision, ProjectDiffScope, ProjectDiscardFilePayload,
    ProjectGitChangeKind, ProjectGitCommitPayload, ProjectGitCommitSummary, ProjectGitFileStatus,
    ProjectPath, ProjectReadDiffPayload, ProjectRootGitStatus, ProjectRootPath,
    ProjectStageFilePayload, ProjectUnstageFilePayload, ReviewId, StreamPath,
};

const HISTORY_PAGE_SIZE: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoricalSelection {
    host_id: String,
    project_id: protocol::ProjectId,
    root: ProjectRootPath,
    base_oid: String,
    tip_oid: String,
    commit_count: u32,
    anchor_oid: String,
    selected_oids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HistoryScope {
    host_id: String,
    project_id: protocol::ProjectId,
    root: ProjectRootPath,
}

impl HistoryScope {
    fn owns(&self, selection: &HistoricalSelection) -> bool {
        selection.host_id == self.host_id
            && selection.project_id == self.project_id
            && selection.root == self.root
    }
}

struct HistoricalCommitSource<'a> {
    host_id: &'a str,
    project_id: &'a protocol::ProjectId,
    root: &'a ProjectRootPath,
    empty_tree_oid: Option<&'a str>,
    commits: &'a [ProjectGitCommitSummary],
}

#[derive(Clone)]
struct HistoricalCommitList {
    scope: HistoryScope,
    empty_tree_oid: Option<String>,
    commits: Vec<ProjectGitCommitSummary>,
}

impl HistoricalCommitList {
    fn source(&self) -> HistoricalCommitSource<'_> {
        HistoricalCommitSource {
            host_id: &self.scope.host_id,
            project_id: &self.scope.project_id,
            root: &self.scope.root,
            empty_tree_oid: self.empty_tree_oid.as_deref(),
            commits: &self.commits,
        }
    }
}

impl HistoricalSelection {
    fn revision(&self) -> ProjectDiffRevision {
        ProjectDiffRevision::CommittedRange {
            base_oid: self.base_oid.clone(),
            tip_oid: self.tip_oid.clone(),
        }
    }
}

/// Panel-wide interaction state shared by every root section. Keyed by
/// `HistoryScope` so a status refresh, which rebuilds the root sections from
/// new `ProjectRootGitStatus` values, never loses what the user opened.
#[derive(Clone, Copy)]
struct PanelSignals {
    historical_selection: RwSignal<Option<HistoricalSelection>>,
    history_visible_limits: RwSignal<HashMap<HistoryScope, usize>>,
    history_roots: RwSignal<HashSet<HistoryScope>>,
    root_expansion: RwSignal<HashMap<HistoryScope, bool>>,
}

#[component]
pub fn GitPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let signals = PanelSignals {
        historical_selection: RwSignal::new(None),
        history_visible_limits: RwSignal::new(HashMap::new()),
        history_roots: RwSignal::new(HashSet::new()),
        root_expansion: RwSignal::new(HashMap::new()),
    };

    let git_roots = Memo::new(move |_| {
        let project = state.active_project.get()?;
        let map = state.git_status.get();
        map.get(&project.project_id)
            .cloned()
            .map(|roots| (project, roots))
    });

    view! {
        <div class="git-panel">
            <div class="gp-content">
                <ReviewStatusRow />
                {move || {
                    match git_roots.get() {
                        Some((project, roots)) => {
                            let multi_root = roots.len() > 1;
                            roots.into_iter().map(|root| {
                                view! {
                                    <GitRootSection
                                        host_id=project.host_id.clone()
                                        project_id=project.project_id.clone()
                                        root=root
                                        multi_root=multi_root
                                        signals=signals
                                    />
                                }.into_any()
                            }).collect()
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

/// One-line summary of the project's workspace review: live counts, whether
/// the AI reviewer is running, and the way into the full review surface. The
/// review controls themselves (AI reviewer form, submit, clear) live on that
/// surface, not in the git panel. Hidden while there is nothing to review and
/// nothing reviewed, so a clean project pays no vertical cost.
#[component]
fn ReviewStatusRow() -> impl IntoView {
    let state = expect_context::<AppState>();

    let target_state = state.clone();
    let target: Memo<Option<(String, ReviewId)>> = Memo::new(move |_| {
        let ap = target_state.active_project.get()?;
        target_state.review_summaries.with(|map| {
            map.get(&ap.project_id).and_then(|summaries| {
                crate::components::review_view::pick_workspace_draft(summaries)
                    .map(|summary| (ap.host_id.clone(), summary.id.clone()))
            })
        })
    });
    crate::components::review_view::subscribe_review_reactive(&state, target);

    let counts_state = state.clone();
    let counts: Memo<Option<(u32, u32, bool)>> = Memo::new(move |_| {
        let (_, rid) = target.get()?;
        let from_record = counts_state.reviews.with(|map| {
            map.get(&rid).map(|review| {
                (
                    review.comments.len() as u32,
                    review
                        .suggestions
                        .iter()
                        .filter(|s| matches!(s.state, protocol::ReviewSuggestionState::Pending))
                        .count() as u32,
                    matches!(
                        review.ai_reviewer.status,
                        protocol::ReviewAiReviewerStatus::Running
                    ),
                )
            })
        });
        if from_record.is_some() {
            return from_record;
        }
        counts_state.review_summaries.with(|map| {
            map.values().find_map(|summaries| {
                summaries
                    .iter()
                    .find(|summary| summary.id == rid)
                    .map(|summary| {
                        (
                            summary.user_comment_count,
                            summary.pending_suggestion_count,
                            false,
                        )
                    })
            })
        })
    });

    let dirty_state = state.clone();
    let has_dirty_root = Memo::new(move |_| {
        let Some(ap) = dirty_state.active_project.get() else {
            return false;
        };
        dirty_state.git_status.with(|map| {
            map.get(&ap.project_id)
                .is_some_and(|roots| roots.iter().any(|root| !root.clean))
        })
    });
    let visible = Memo::new(move |_| {
        counts
            .get()
            .is_some_and(|(comments, suggestions, running)| {
                comments > 0 || suggestions > 0 || running || has_dirty_root.get()
            })
    });

    let open_state = state.clone();
    let on_open = move |_| {
        let Some((host, _)) = target.get_untracked() else {
            return;
        };
        let Some(ap) = open_state.active_project.get_untracked() else {
            return;
        };
        crate::components::review_view::open_comments_for_project(
            &open_state,
            &host,
            &ap.project_id,
        );
    };

    view! {
        <Show when=move || visible.get()>
            <div class="gp-review-status" data-test="gp-review-status">
                <span class="gp-review-status-title">"Review"</span>
                <span class="gp-review-counts" data-test="gp-review-counts">
                    {move || {
                        counts
                            .get()
                            .map(|(comments, suggestions, _)| {
                                format!(
                                    "{comments} comment{} \u{00b7} {suggestions} AI",
                                    if comments == 1 { "" } else { "s" },
                                )
                            })
                            .unwrap_or_default()
                    }}
                </span>
                {move || {
                    counts.get().is_some_and(|(_, _, running)| running).then(|| view! {
                        <span
                            class="gp-review-ai"
                            data-test="gp-review-ai"
                            title="The AI reviewer is running"
                        >
                            "reviewing\u{2026}"
                        </span>
                    })
                }}
                <button
                    class="gp-review-open-btn"
                    data-test="gp-review-open"
                    title="Open the review: comments, AI reviewer, and submit"
                    on:click=on_open.clone()
                >
                    "Open"
                </button>
            </div>
        </Show>
    }
}

fn pick_committed_range_draft<'a>(
    summaries: &'a [protocol::ReviewSummary],
    selection: &HistoricalSelection,
) -> Option<&'a protocol::ReviewSummary> {
    summaries
        .iter()
        .filter(|summary| {
            matches!(summary.status, protocol::ReviewStatus::Draft)
                && matches!(
                    &summary.scope,
                    protocol::ReviewSummaryScope::CommittedRange {
                        root,
                        base_oid,
                        tip_oid,
                        ..
                    } if root == &selection.root
                        && base_oid == &selection.base_oid
                        && tip_oid == &selection.tip_oid
                )
        })
        .max_by_key(|summary| summary.updated_at_ms)
}

/// Review affordance for the expanded commit block: a starter while the
/// range has no draft, and the draft's counts plus the shared review
/// controls once one exists.
#[component]
fn CommittedReviewControls(selection: HistoricalSelection) -> impl IntoView {
    let state = expect_context::<AppState>();
    let target_state = state.clone();
    let target_selection = selection.clone();
    let target: Memo<Option<(String, ReviewId)>> = Memo::new(move |_| {
        target_state.review_summaries.with(|map| {
            map.get(&target_selection.project_id).and_then(|summaries| {
                pick_committed_range_draft(summaries, &target_selection)
                    .map(|summary| (target_selection.host_id.clone(), summary.id.clone()))
            })
        })
    });
    crate::components::review_view::subscribe_review_reactive(&state, target);

    view! {
        {move || match target.get() {
            Some((host, rid)) => view! {
                <CommittedReviewHub host_id=host review_id=rid selection=selection.clone() />
            }.into_any(),
            None => view! {
                <CommittedReviewStarter selection=selection.clone() />
            }.into_any(),
        }}
    }
}

#[component]
fn CommittedReviewStarter(selection: HistoricalSelection) -> impl IntoView {
    let state = expect_context::<AppState>();
    let pending_request_id = RwSignal::new(None::<String>);
    let send_error = RwSignal::new(None::<String>);
    let create_selection = selection.clone();
    let on_start = move |_| {
        let selection = create_selection.clone();
        let request_id = next_client_request_id("committed-review-create");
        state.command_errors_by_request.update(|errors| {
            errors.remove(&request_id);
        });
        send_error.set(None);
        pending_request_id.set(Some(request_id.clone()));
        let payload = protocol::ReviewCreatePayload {
            request_id: Some(request_id.clone()),
            selection: protocol::ReviewDiffSelection::CommittedRange {
                root: selection.root.clone(),
                base_oid: selection.base_oid.clone(),
                tip_oid: selection.tip_oid.clone(),
                commit_count: selection.commit_count,
            },
        };
        let host_id = selection.host_id.clone();
        let project_id = selection.project_id.clone();
        spawn_local(async move {
            if let Err(error) = send_frame(
                &host_id,
                StreamPath(format!("/project/{}", project_id.0)),
                FrameKind::ReviewCreate,
                &payload,
            )
            .await
            {
                log::error!("failed to create committed range review: {error}");
                send_error.set(Some(error));
                pending_request_id.set(None);
            }
        });
    };
    let error_state = state.clone();
    let request_error = Memo::new(move |_| {
        send_error.get().or_else(|| {
            let request_id = pending_request_id.get()?;
            error_state
                .command_errors_by_request
                .with(|errors| errors.get(&request_id).cloned())
        })
    });
    view! {
        <div class="gp-commit-review-row" data-test="gp-committed-review-starter">
            <button
                class="gp-review-open-btn"
                title="Review these committed changes. They are immutable, so feedback is fix-forward."
                disabled=move || pending_request_id.get().is_some() && request_error.get().is_none()
                on:click=on_start
            >
                {move || if pending_request_id.get().is_some() && request_error.get().is_none() { "Starting…" } else { "Start review" }}
            </button>
            {move || request_error.get().map(|message| view! {
                <div class="gp-range-state error" role="alert" data-test="gp-review-create-error">
                    {format!("Could not start review: {message}. Retry after refreshing history.")}
                </div>
            })}
        </div>
    }
}

#[component]
fn CommittedReviewHub(
    host_id: String,
    review_id: ReviewId,
    selection: HistoricalSelection,
) -> impl IntoView {
    let state = expect_context::<AppState>();

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

    // The frozen range is reviewable as long as it produced any file diff.
    let changes_state = state.clone();
    let has_reviewable_changes: Memo<bool> = Memo::new(move |_| {
        let key = DiffKey::with_revision(
            selection.host_id.clone(),
            selection.project_id.clone(),
            selection.root.clone(),
            ProjectDiffScope::Uncommitted,
            selection.revision(),
            "",
        );
        changes_state
            .diff_contents
            .with(|diffs| diffs.get(&key).is_some_and(|diff| !diff.files.is_empty()))
    });

    let sidebar_state = state.clone();
    let sidebar_host = host_id.clone();
    let sidebar_rid = review_id.clone();
    view! {
        <div class="gp-committed-review" data-test="gp-committed-review-hub">
            <div class="gp-commit-review-row">
                <span class="gp-review-status-title">"Review"</span>
                <span class="gp-review-counts" data-test="gp-committed-review-counts">
                    {move || {
                        let (c, s) = counts.get();
                        format!(
                            "{c} comment{} \u{00b7} {s} AI",
                            if c == 1 { "" } else { "s" },
                        )
                    }}
                </span>
            </div>
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
                            can_run_ai=has_reviewable_changes
                        />
                    }.into_any(),
                    None => view! { <div></div> }.into_any(),
                }
            }}
        </div>
    }
}

#[component]
fn RecentCommits(
    list: StoredValue<HistoricalCommitList>,
    history_has_more: bool,
    signals: PanelSignals,
) -> impl IntoView {
    let (scope, root_label, history_len, row_commits) = list.with_value(|list| {
        (
            list.scope.clone(),
            list.scope.root.0.clone(),
            list.commits.len(),
            list.commits.clone(),
        )
    });
    let limit_scope = scope.clone();
    let visible_limit = Memo::new(move |_| {
        signals.history_visible_limits.with(|limits| {
            limits
                .get(&limit_scope)
                .copied()
                .unwrap_or(HISTORY_PAGE_SIZE)
                .min(history_len)
        })
    });
    let selection_scope = scope.clone();
    let selection: Memo<Option<HistoricalSelection>> = Memo::new(move |_| {
        signals
            .historical_selection
            .get()
            .filter(|selected| selection_scope.owns(selected))
    });
    let load_scope = StoredValue::new(scope);
    let historical_selection = signals.historical_selection;

    view! {
        <div
            class="gp-history"
            role="listbox"
            aria-label=format!("Recent commits for {root_label}")
            aria-multiselectable="true"
            data-test="gp-history"
        >
            {(history_len == 0).then(|| view! {
                <div class="gp-history-note" role="status">"No commits yet"</div>
            })}
            {row_commits.into_iter().enumerate().map(|(index, commit)| {
                let oid = commit.oid.clone();
                let oid_for_anchor = commit.oid.clone();
                let selected = Memo::new(move |_| {
                    selection.get().is_some_and(|selected| selected.selected_oids.contains(&oid))
                });
                let is_anchor = Memo::new(move |_| {
                    selection.get().is_some_and(|selected| {
                        selected.selected_oids.last() == Some(&oid_for_anchor)
                    })
                });
                let aria_label = commit_aria_label(&commit);
                let age = commit_age(commit.authored_at_seconds);
                let age_title = format!("{} · {}", commit.author, short_oid(&commit.oid));
                view! {
                    <button
                        style=move || if index < visible_limit.get() { "" } else { "display: none;" }
                        tabindex=move || if index < visible_limit.get() { "0" } else { "-1" }
                        class=move || {
                            if selected.get() { "gp-commit-row selected" } else { "gp-commit-row" }
                        }
                        role="option"
                        aria-selected=move || selected.get().to_string()
                        aria-label=aria_label
                        data-test="gp-commit-row"
                        on:click=move |event| {
                            list.with_value(|list| {
                                select_commit_range(
                                    historical_selection,
                                    list.source(),
                                    index,
                                    event.shift_key(),
                                );
                            });
                        }
                        on:keydown=move |event| {
                            match event.key().as_str() {
                                "Enter" | " " => {
                                    event.prevent_default();
                                    list.with_value(|list| {
                                        select_commit_range(
                                            historical_selection,
                                            list.source(),
                                            index,
                                            event.shift_key(),
                                        );
                                    });
                                }
                                "Escape" => {
                                    event.prevent_default();
                                    historical_selection.set(None);
                                }
                                "ArrowUp" | "ArrowDown" => {
                                    event.prevent_default();
                                    let offset = if event.key() == "ArrowUp" { -1 } else { 1 };
                                    focus_history_option(&event, offset);
                                    if event.shift_key() {
                                        list.with_value(|list| {
                                            let target = if offset < 0 {
                                                index.saturating_sub(1)
                                            } else {
                                                (index + 1).min(list.commits.len().saturating_sub(1))
                                            };
                                            select_commit_range(
                                                historical_selection,
                                                list.source(),
                                                target,
                                                true,
                                            );
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    >
                        <span class="gp-commit-subject">{commit.subject.clone()}</span>
                        {commit.is_merge.then(|| view! {
                            <span class="gp-merge-badge">"merge"</span>
                        })}
                        <span class="gp-commit-age" title=age_title>{age}</span>
                    </button>
                    <Show when=move || is_anchor.get()>
                        <CommitDetail selection=selection list=list />
                    </Show>
                }
            }).collect::<Vec<_>>()}
            <Show when=move || visible_limit.get() < history_len>
                <button
                    class="gp-history-older"
                    on:click=move |_| signals.history_visible_limits.update(|limits| {
                        let limit = limits
                            .entry(load_scope.get_value())
                            .or_insert(HISTORY_PAGE_SIZE);
                        *limit = (*limit + HISTORY_PAGE_SIZE).min(history_len);
                    })
                >
                    "Older\u{2026}"
                </button>
            </Show>
            <Show when=move || { history_has_more && visible_limit.get() >= history_len }>
                <div class="gp-history-note">"Older history is not loaded"</div>
            </Show>
        </div>
    }
}

/// The accordion body under the selected commit (or the oldest commit of a
/// selected range): identity line, review affordance, and the range's
/// changed files. Rendered adjacent to the row that produced it so the cause
/// and its effect never drift apart.
#[component]
fn CommitDetail(
    selection: Memo<Option<HistoricalSelection>>,
    list: StoredValue<HistoricalCommitList>,
) -> impl IntoView {
    view! {
        <div class="gp-commit-detail" data-test="gp-commit-detail">
            {move || selection.get().map(|selected| {
                let single = selected.commit_count == 1;
                let sha = if single {
                    short_oid(&selected.tip_oid)
                } else {
                    format!("{}\u{2026}{}", short_oid(&selected.base_oid), short_oid(&selected.tip_oid))
                };
                let author = single.then(|| list.with_value(|list| {
                    list.commits
                        .iter()
                        .find(|commit| commit.oid == selected.tip_oid)
                        .map(|commit| commit.author.clone())
                })).flatten();
                let count = format!(
                    "{} commit{}",
                    selected.commit_count,
                    if single { "" } else { "s" },
                );
                view! {
                    <div class="gp-commit-meta" data-test="gp-commit-meta">
                        <span class="gp-commit-sha" title=selected.tip_oid.clone()>{sha}</span>
                        {author.map(|author| view! {
                            <span class="gp-commit-author">{author}</span>
                        })}
                        <span class="gp-commit-count">{count}</span>
                    </div>
                    <CommittedReviewControls selection=selected />
                }
            })}
            <HistoricalChangedFiles
                selection=selection
                commits=list.with_value(|list| list.commits.clone())
            />
        </div>
    }
}

fn select_commit_range(
    selection: RwSignal<Option<HistoricalSelection>>,
    source: HistoricalCommitSource<'_>,
    clicked_index: usize,
    extend: bool,
) {
    if source.commits.get(clicked_index).is_none() {
        return;
    }
    let anchor_index = if extend {
        selection
            .get_untracked()
            .filter(|selected| {
                selected.host_id == source.host_id
                    && selected.project_id == *source.project_id
                    && selected.root == *source.root
            })
            .and_then(|selected| {
                source
                    .commits
                    .iter()
                    .position(|commit| commit.oid == selected.anchor_oid)
            })
            .unwrap_or(clicked_index)
    } else {
        clicked_index
    };
    let start_index = anchor_index.min(clicked_index);
    let end_index = anchor_index.max(clicked_index);
    let selected_oids = source.commits[start_index..=end_index]
        .iter()
        .map(|commit| commit.oid.clone())
        .collect::<Vec<_>>();
    let newest = &source.commits[start_index];
    let oldest = &source.commits[end_index];
    let Some(base_oid) = oldest
        .first_parent_oid
        .clone()
        .or_else(|| source.empty_tree_oid.map(str::to_owned))
    else {
        return;
    };
    let selected = HistoricalSelection {
        host_id: source.host_id.to_owned(),
        project_id: source.project_id.clone(),
        root: source.root.clone(),
        base_oid,
        tip_oid: newest.oid.clone(),
        commit_count: selected_oids.len() as u32,
        anchor_oid: source.commits[anchor_index].oid.clone(),
        selected_oids,
    };
    selection.set(Some(selected.clone()));
    request_historical_diff(selected, String::new(), false);
}

fn focus_history_option(event: &web_sys::KeyboardEvent, offset: i32) {
    let Some(target) = event
        .current_target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let Some(list) = target.parent_element() else {
        return;
    };
    let Ok(options) = list.query_selector_all("[role=option]:not([tabindex='-1'])") else {
        return;
    };
    let current = (0..options.length()).find(|index| {
        options
            .item(*index)
            .is_some_and(|node| node.is_same_node(Some(&target)))
    });
    let Some(current) = current else {
        return;
    };
    let next = (current as i32 + offset).clamp(0, options.length().saturating_sub(1) as i32);
    if let Some(node) = options.item(next as u32)
        && let Ok(element) = node.dyn_into::<web_sys::HtmlElement>()
    {
        let _ = element.focus();
    }
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(8).collect()
}

fn commit_age(authored_at_seconds: i64) -> String {
    if authored_at_seconds <= 0 {
        return "time unknown".to_owned();
    }
    let elapsed = ((js_sys::Date::now() / 1000.0) as i64 - authored_at_seconds).max(0);
    if elapsed < 60 {
        "now".to_owned()
    } else if elapsed < 3_600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3_600)
    } else {
        format!("{}d", elapsed / 86_400)
    }
}

fn commit_aria_label(commit: &ProjectGitCommitSummary) -> String {
    format!(
        "{} {}, by {}, {}{}",
        short_oid(&commit.oid),
        commit.subject,
        commit.author,
        commit_age(commit.authored_at_seconds),
        if commit.is_merge {
            ", merge commit"
        } else {
            ""
        },
    )
}

#[component]
fn GitRootSection(
    host_id: String,
    project_id: protocol::ProjectId,
    root: ProjectRootGitStatus,
    multi_root: bool,
    signals: PanelSignals,
) -> impl IntoView {
    let scope = HistoryScope {
        host_id: host_id.clone(),
        project_id: project_id.clone(),
        root: root.root.clone(),
    };
    let list = HistoricalCommitList {
        scope: scope.clone(),
        empty_tree_oid: root.empty_tree_oid.clone(),
        commits: root.recent_commits.clone(),
    };
    let history_has_more = root.history_has_more;
    let conflicts: Vec<_> = root
        .files
        .iter()
        .filter(|f| {
            f.staged == Some(ProjectGitChangeKind::Unmerged)
                || f.unstaged == Some(ProjectGitChangeKind::Unmerged)
        })
        .cloned()
        .collect();
    let staged: Vec<_> = root
        .files
        .iter()
        .filter(|f| {
            f.staged.is_some()
                && f.staged != Some(ProjectGitChangeKind::Unmerged)
                && f.unstaged != Some(ProjectGitChangeKind::Unmerged)
        })
        .cloned()
        .collect();
    let unstaged: Vec<_> = root
        .files
        .iter()
        .filter(|f| {
            f.unstaged.is_some()
                && f.unstaged != Some(ProjectGitChangeKind::Unmerged)
                && f.staged != Some(ProjectGitChangeKind::Unmerged)
                && !f.untracked
        })
        .cloned()
        .collect();
    let untracked: Vec<_> = root.files.iter().filter(|f| f.untracked).cloned().collect();

    let list = StoredValue::new(list);
    let conflicts = StoredValue::new(conflicts);
    let staged = StoredValue::new(staged);
    let unstaged = StoredValue::new(unstaged);
    let untracked = StoredValue::new(untracked);
    let root_path = root.root.clone();
    let root_handle = StoredValue::new(root.root.clone());
    let conflicts_expanded = RwSignal::new(true);
    let staged_expanded = RwSignal::new(true);
    let unstaged_expanded = RwSignal::new(true);
    let untracked_expanded = RwSignal::new(true);
    let commit_open = RwSignal::new(false);

    let conflicts_count = conflicts.with_value(Vec::len);
    let staged_count = staged.with_value(Vec::len);
    let unstaged_count = unstaged.with_value(Vec::len);
    let untracked_count = untracked.with_value(Vec::len);
    let changed_count = root.files.len();

    let has_conflicts = conflicts_count != 0;
    let has_staged = staged_count != 0;
    let has_unstaged = unstaged_count != 0;
    let has_untracked = untracked_count != 0;
    let clean = root.clean;

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

    // A lone root is always open. With several, dirty roots open and clean
    // ones collapse to their header line, so a project with many clean
    // roots stays one line per root. An explicit toggle wins over the
    // default and survives status refreshes.
    let default_expanded = !multi_root || !clean;
    let expansion_scope = scope.clone();
    let expanded = Memo::new(move |_| {
        signals
            .root_expansion
            .with(|map| map.get(&expansion_scope).copied())
            .unwrap_or(default_expanded)
    });
    let history_scope = scope.clone();
    let history_on = Memo::new(move |_| {
        signals
            .history_roots
            .with(|roots| roots.contains(&history_scope))
    });
    let toggle_scope = scope.clone();
    let on_toggle = move |_| {
        let next = !expanded.get_untracked();
        signals.root_expansion.update(|map| {
            map.insert(toggle_scope.clone(), next);
        });
    };
    let history_toggle_scope = scope.clone();
    let on_toggle_history = move |_| {
        let turning_on = !history_on.get_untracked();
        signals.history_roots.update(|roots| {
            if turning_on {
                roots.insert(history_toggle_scope.clone());
            } else {
                roots.remove(&history_toggle_scope);
            }
        });
        if turning_on {
            signals.root_expansion.update(|map| {
                map.insert(history_toggle_scope.clone(), true);
            });
        } else if signals
            .historical_selection
            .get_untracked()
            .is_some_and(|selected| history_toggle_scope.owns(&selected))
        {
            signals.historical_selection.set(None);
        }
    };

    let commit_message = RwSignal::new(String::new());

    let state = expect_context::<AppState>();

    // Per-file review-comment badges for this root, sourced from the project's
    // single workspace draft review. Prefers the server-computed per-file
    // counts on the workspace `ReviewSummary` (available without a full review
    // subscribe), narrowed to this root via `ReviewFileCommentCount.root`;
    // falls back to computing from the loaded `Review` (also root-filtered)
    // when the summary carries no per-file counts yet.
    let counts_state = state.clone();
    let counts_root = root_path.clone();
    let file_counts: Memo<HashMap<String, u32>> = Memo::new(move |_| {
        let Some(ap) = counts_state.active_project.get() else {
            return HashMap::new();
        };
        let summary = counts_state.review_summaries.with(|map| {
            map.get(&ap.project_id).and_then(|summaries| {
                crate::components::review_view::pick_workspace_draft(summaries)
                    .map(|s| (s.id.clone(), s.file_comment_counts.clone()))
            })
        });
        let Some((rid, file_comment_counts)) = summary else {
            return HashMap::new();
        };
        let this_root: Vec<_> = file_comment_counts
            .iter()
            .filter(|f| f.root == counts_root)
            .collect();
        if !this_root.is_empty() {
            return this_root
                .iter()
                .map(|f| (f.relative_path.clone(), f.total_count()))
                .collect();
        }
        counts_state.reviews.with(|m| {
            m.get(&rid)
                .map(|r| per_file_comment_counts(r, &counts_root))
                .unwrap_or_default()
        })
    });

    let state_label = if clean {
        (
            "gp-root-state clean",
            "\u{2713}".to_owned(),
            "Working tree clean".to_owned(),
        )
    } else if has_conflicts {
        (
            "gp-root-state conflicts",
            format!(
                "{conflicts_count} conflict{}",
                if conflicts_count == 1 { "" } else { "s" }
            ),
            format!("{changed_count} changed files, {conflicts_count} unmerged"),
        )
    } else {
        (
            "gp-root-state",
            changed_count.to_string(),
            format!(
                "{changed_count} changed file{}",
                if changed_count == 1 { "" } else { "s" }
            ),
        )
    };

    view! {
        <section class="gp-root" data-test="gp-root" data-root=root_title.clone()>
            <div class="gp-root-header" title=root_title.clone()>
                <button
                    class="gp-root-toggle"
                    data-test="gp-root-toggle"
                    aria-expanded=move || expanded.get().to_string()
                    on:click=on_toggle
                >
                    <span class="fe-chevron">
                        {move || if expanded.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </span>
                    <span class="gp-root-name">{root_label}</span>
                    <span class="gp-root-branch">{branch_label}</span>
                    {ahead_behind.map(|ab| view! {
                        <span class="gp-root-ahead-behind">{ab}</span>
                    })}
                </button>
                <span class=state_label.0 data-test="gp-root-state" title=state_label.2>
                    {state_label.1}
                </span>
                <button
                    class="gp-root-history-toggle"
                    data-test="gp-root-history-toggle"
                    aria-pressed=move || history_on.get().to_string()
                    title=move || if history_on.get() { "Back to the working tree" } else { "Recent commits" }
                    on:click=on_toggle_history
                >
                    <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
                        <path d="M1.643 3.143 .427 1.927A.25.25 0 0 0 0 2.104V5.75c0 .138.112.25.25.25h3.646a.25.25 0 0 0 .177-.427L2.715 4.215a6.5 6.5 0 1 1-1.18 4.458.75.75 0 1 0-1.493.154 8.001 8.001 0 1 0 3.601-5.684ZM7.75 4a.75.75 0 0 1 .75.75v2.992l2.028.812a.75.75 0 0 1-.557 1.392l-2.5-1A.751.751 0 0 1 7 8.25v-3.5A.75.75 0 0 1 7.75 4Z" />
                    </svg>
                </button>
            </div>
            <Show when=move || expanded.get()>
                <Show
                    when=move || history_on.get()
                    fallback=move || view! {
                            <Show when=move || clean>
                                <div class="gp-clean">"\u{2713} Working tree clean"</div>
                            </Show>
                            <Show when=move || has_conflicts>
                                <GitFileSection
                                    title="Conflicts"
                                    count=conflicts_count
                                    files=conflicts.get_value()
                                    expanded=conflicts_expanded
                                    scope=ProjectDiffScope::Unstaged
                                    root_path=root_handle.get_value()
                                    show_stage_btn=true
                                    show_unstage_btn=false
                                    show_discard_btn=false
                                    file_counts=file_counts
                                />
                            </Show>
                            <Show when=move || has_staged>
                                <GitFileSection
                                    title="Staged"
                                    count=staged_count
                                    files=staged.get_value()
                                    expanded=staged_expanded
                                    scope=ProjectDiffScope::Staged
                                    root_path=root_handle.get_value()
                                    show_stage_btn=false
                                    show_unstage_btn=true
                                    show_discard_btn=false
                                    file_counts=file_counts
                                    commit_toggle=commit_open
                                />
                                <Show when=move || commit_open.get()>
                                    <div class="gp-commit-area" data-test="gp-commit-area">
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
                                            on:click=move |_| {
                                                let msg = commit_message.get();
                                                if !msg.trim().is_empty() {
                                                    send_commit(root_handle.get_value(), msg);
                                                    commit_message.set(String::new());
                                                }
                                            }
                                        >
                                            "Commit"
                                        </button>
                                    </div>
                                </Show>
                            </Show>
                            <Show when=move || has_unstaged>
                                <GitFileSection
                                    title="Changes"
                                    count=unstaged_count
                                    files=unstaged.get_value()
                                    expanded=unstaged_expanded
                                    scope=ProjectDiffScope::Unstaged
                                    root_path=root_handle.get_value()
                                    show_stage_btn=true
                                    show_unstage_btn=false
                                    show_discard_btn=true
                                    file_counts=file_counts
                                />
                            </Show>
                            <Show when=move || has_untracked>
                                <GitFileSection
                                    title="Untracked"
                                    count=untracked_count
                                    files=untracked.get_value()
                                    expanded=untracked_expanded
                                    scope=ProjectDiffScope::Unstaged
                                    root_path=root_handle.get_value()
                                    show_stage_btn=true
                                    show_unstage_btn=false
                                    show_discard_btn=true
                                    file_counts=file_counts
                                />
                            </Show>
                    }
                >
                    <RecentCommits
                        list=list
                        history_has_more=history_has_more
                        signals=signals
                    />
                </Show>
            </Show>
        </section>
    }
}

#[component]
fn HistoricalChangedFiles(
    selection: Memo<Option<HistoricalSelection>>,
    commits: Vec<ProjectGitCommitSummary>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let diff_state = state.clone();
    let diff = Signal::derive(move || {
        let selected = selection.get()?;
        let key = DiffKey::with_revision(
            selected.host_id.clone(),
            selected.project_id.clone(),
            selected.root.clone(),
            ProjectDiffScope::Uncommitted,
            selected.revision(),
            "",
        );
        diff_state
            .diff_contents
            .with(|diffs| diffs.get(&key).cloned())
    });
    let error_state = state.clone();
    let diff_error = Signal::derive(move || {
        let selected = selection.get()?;
        let key = DiffKey::with_revision(
            selected.host_id.clone(),
            selected.project_id.clone(),
            selected.root.clone(),
            ProjectDiffScope::Uncommitted,
            selected.revision(),
            "",
        );
        error_state
            .diff_request_errors
            .with(|errors| errors.get(&key).cloned())
    });
    let available = Memo::new(move |_| {
        selection.get().is_some_and(|selected| {
            selected
                .selected_oids
                .iter()
                .all(|oid| commits.iter().any(|commit| &commit.oid == oid))
        })
    });

    view! {
        <div class="gp-historical-files" data-test="gp-historical-files">
            {move || {
                let Some(selected) = selection.get() else {
                    return view! { <div></div> }.into_any();
                };
                if !available.get() {
                    return view! {
                        <div class="gp-range-state error" role="status">
                            "This committed range is no longer in the current first-parent history. The pinned range was not retargeted."
                        </div>
                    }.into_any();
                }
                if let Some(message) = diff_error.get() {
                    let retry_selection = selected.clone();
                    return view! {
                        <div class="gp-range-state error" role="alert" data-test="gp-historical-diff-error">
                            <span>{format!("Could not load this committed range: {message}")}</span>
                            <button on:click=move |_| {
                                request_historical_diff(retry_selection.clone(), String::new(), false);
                            }>
                                "Retry"
                            </button>
                        </div>
                    }.into_any();
                }
                match diff.get() {
                    Some(diff) if diff.pending => view! {
                        <div class="gp-range-state" role="status">"Loading committed changes…"</div>
                    }.into_any(),
                    Some(diff) if diff.files.is_empty() => view! {
                        <div class="gp-section historical">
                            <div class="gp-section-header-row">
                                <div class="gp-section-header static">
                                    <span class="gp-section-title">"Changed files"</span>
                                    <span class="gp-section-count">"0"</span>
                                </div>
                            </div>
                            <div class="gp-range-state" role="status">"No net file changes in this range"</div>
                        </div>
                    }.into_any(),
                    Some(diff) => {
                        let count = diff.files.len();
                        view! {
                            <div class="gp-section historical">
                                <div class="gp-section-header-row">
                                    <div class="gp-section-header static">
                                        <span class="gp-section-title">"Changed files"</span>
                                        <span class="gp-section-count">{count}</span>
                                    </div>
                                </div>
                                <div class="gp-section-files">
                                    {diff.files.into_iter().map(|file| {
                                        let path = file.relative_path.clone();
                                        let (dir, name) = match path.rsplit_once('/') {
                                            Some((directory, name)) => {
                                                (Some(directory.to_owned()), name.to_owned())
                                            }
                                            None => (None, path.clone()),
                                        };
                                        let kind = file
                                            .change_kind
                                            .unwrap_or_else(|| historical_change_kind(&file));
                                        let icon = change_kind_icon(Some(kind));
                                        let icon_class = change_kind_class(Some(kind));
                                        let selected_for_click = selected.clone();
                                        let title = format!("Open committed diff for {path}");
                                        view! {
                                            <div class="gp-file-row readonly">
                                                <button
                                                    class="gp-file-btn"
                                                    title=title
                                                    aria-keyshortcuts="Enter"
                                                    on:click=move |_| {
                                                        request_historical_diff(
                                                            selected_for_click.clone(),
                                                            path.clone(),
                                                            true,
                                                        );
                                                    }
                                                >
                                                    <span class=icon_class>{icon}</span>
                                                    <span class="gp-file-path">
                                                        <span class="gp-file-name">{name}</span>
                                                        {dir.map(|directory| view! {
                                                            <span class="gp-file-dir">{directory}</span>
                                                        })}
                                                    </span>
                                                </button>
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                </div>
                            </div>
                        }.into_any()
                    }
                    None => view! {
                        <div class="gp-range-state" role="status">"Loading committed changes…"</div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

fn historical_change_kind(file: &protocol::ProjectGitDiffFile) -> ProjectGitChangeKind {
    let mut added = false;
    let mut removed = false;
    for line in file.hunks.iter().flat_map(|hunk| hunk.lines.iter()) {
        match line.kind {
            protocol::ProjectGitDiffLineKind::Added => added = true,
            protocol::ProjectGitDiffLineKind::Removed => removed = true,
            protocol::ProjectGitDiffLineKind::Context => {}
        }
    }
    match (added, removed) {
        (true, false) => ProjectGitChangeKind::Added,
        (false, true) => ProjectGitChangeKind::Deleted,
        _ => ProjectGitChangeKind::Modified,
    }
}

fn request_historical_diff(selection: HistoricalSelection, path: String, open: bool) {
    let state = expect_context::<AppState>();
    let key = DiffKey::with_revision(
        selection.host_id.clone(),
        selection.project_id.clone(),
        selection.root.clone(),
        ProjectDiffScope::Uncommitted,
        selection.revision(),
        path,
    );
    if open {
        let label = diff_label(&key.root, &key.path, &key.revision);
        state.open_tab(
            crate::state::TabContent::Diff {
                host_id: key.host_id.clone(),
                project_id: key.project_id.clone(),
                root: key.root.clone(),
                scope: key.scope,
                revision: key.revision.clone(),
                path: key.path.clone(),
            },
            label,
            true,
        );
    }
    request_diff(&state, key);
}

fn diff_label(root: &ProjectRootPath, path: &str, revision: &ProjectDiffRevision) -> String {
    let kind = if matches!(revision, ProjectDiffRevision::CommittedRange { .. }) {
        "Committed "
    } else {
        ""
    };
    let mut label = format!(
        "Diff: {kind}{}/{}",
        root_display_name(root),
        path.rsplit('/').next().unwrap_or(path)
    );
    if let ProjectDiffRevision::CommittedRange { base_oid, tip_oid } = revision {
        label.push_str(&format!(
            " · {}…{}",
            short_oid(base_oid),
            short_oid(tip_oid)
        ));
    }
    label
}

#[component]
fn GitFileSection(
    title: &'static str,
    count: usize,
    files: Vec<ProjectGitFileStatus>,
    expanded: RwSignal<bool>,
    scope: ProjectDiffScope,
    root_path: ProjectRootPath,
    show_stage_btn: bool,
    show_unstage_btn: bool,
    show_discard_btn: bool,
    file_counts: Memo<HashMap<String, u32>>,
    /// When present, the header offers a "Commit…" action that toggles the
    /// signal; the owner renders the commit form wherever it belongs.
    #[prop(optional)]
    commit_toggle: Option<RwSignal<bool>>,
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
                    <span class="gp-section-count">{count}</span>
                </button>
                <div class="gp-section-actions">
                    {commit_toggle.map(|open| view! {
                        <button
                            class="gp-section-action gp-commit-toggle"
                            data-test="gp-commit-toggle"
                            aria-expanded=move || open.get().to_string()
                            title="Write a commit message for the staged files"
                            on:click=move |_| open.update(|value| *value = !*value)
                        >
                            "Commit\u{2026}"
                        </button>
                    })}
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

                        // Filename leads the row; the parent directory renders
                        // dimmed after it so long paths truncate from the
                        // directory, never the filename.
                        let (dir, name) = match path.rsplit_once('/') {
                            Some((d, n)) => (Some(d.to_owned()), n.to_owned()),
                            None => (None, path.clone()),
                        };

                        let root_for_click = root_path.clone();
                        let path_for_click = path.clone();
                        let path_for_badge = path.clone();
                        let path_for_title = path.clone();
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
                                    title=path_for_title
                                    aria-keyshortcuts="Enter"
                                    on:click=move |_| {
                                        view_diff(root_for_click.clone(), scope, path_for_click.clone());
                                    }
                                >
                                    <span class=icon_class>{icon}</span>
                                    <span class="gp-file-path">
                                        <span class="gp-file-name">{name}</span>
                                        {dir.map(|d| view! {
                                            <span class="gp-file-dir">{d}</span>
                                        })}
                                    </span>
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

/// Per-file review-comment counts for one root, keyed by `relative_path`.
/// Counts every comment (human comments and accepted AI suggestions, which
/// the server promotes into `comments`) plus pending AI suggestions whose
/// location is in `root`. Rejected suggestions are excluded — they are
/// neither `Pending` nor promoted to a comment.
///
/// The workspace review spans every root, so locations are filtered by
/// `ReviewLocation.root` to keep each root's badges separate. Computed from
/// the loaded `Review` as a fallback until `ReviewSummary` carries per-file
/// counts directly.
fn per_file_comment_counts(
    review: &protocol::Review,
    root: &ProjectRootPath,
) -> HashMap<String, u32> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for c in &review.comments {
        if c.location.root == *root {
            *counts.entry(c.location.relative_path.clone()).or_insert(0) += 1;
        }
    }
    for s in &review.suggestions {
        if matches!(s.state, protocol::ReviewSuggestionState::Pending) && s.location.root == *root {
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
        Some(ProjectGitChangeKind::Unmerged) => "U",
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
        Some(ProjectGitChangeKind::Unmerged) => "gp-status-icon unmerged",
        None => "gp-status-icon",
    }
}

fn view_diff(root: ProjectRootPath, scope: ProjectDiffScope, path: String) {
    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let key = DiffKey::new(
        active_project.host_id,
        active_project.project_id,
        root,
        scope,
        path,
    );
    let label = diff_label(&key.root, &key.path, &key.revision);
    state.open_tab(
        crate::state::TabContent::Diff {
            host_id: key.host_id.clone(),
            project_id: key.project_id.clone(),
            root: key.root.clone(),
            scope: key.scope,
            revision: key.revision.clone(),
            path: key.path.clone(),
        },
        label,
        true,
    );
    request_diff(&state, key);
}

fn request_diff(state: &AppState, key: DiffKey) {
    let perf_key = format!("diff:{}:{}", key.root.0, key.path);
    crate::perf::mark_start(&perf_key);
    crate::perf::log_phase("diff_open", "click", &perf_key, "");
    let stream = StreamPath(format!("/project/{}", key.project_id.0));
    let context_mode = state.diff_context_mode.get_untracked();
    let request_id = next_client_request_id("project-diff");
    state.diff_request_ids.update(|requests| {
        requests.insert(key.clone(), request_id.clone());
    });
    state.diff_request_errors.update(|errors| {
        errors.remove(&key);
    });
    state.command_errors_by_request.update(|errors| {
        errors.remove(&request_id);
    });

    // Insert a pending DiffViewState BEFORE dispatching. This is the source of
    // truth for "what was most recently requested" — the reactive re-request
    // effect compares the signal against this entry's `context_mode`, and the
    // dispatch reducer rejects responses that don't match it. Without this,
    // a context-mode flip before the first response arrives would leave the
    // view empty with nothing to re-dispatch against.
    state.diff_contents.update(|diffs| {
        let previous = diffs.get(&key);
        let next = DiffViewState::for_request(
            previous,
            key.root.clone(),
            key.scope,
            Some(key.path.clone()),
            context_mode,
        );
        diffs.insert(key.clone(), next);
    });

    let host_id = key.host_id.clone();
    let failure_state = state.clone();
    let failure_key = key.clone();
    let failure_request_id = request_id.clone();
    spawn_local(async move {
        let path = (!key.path.is_empty()).then_some(key.path);
        let payload = ProjectReadDiffPayload {
            request_id: Some(request_id),
            root: key.root,
            scope: key.scope,
            revision: key.revision,
            path,
            context_mode,
        };
        if let Err(e) = send_frame(&host_id, stream, FrameKind::ProjectReadDiff, &payload).await {
            log::error!("failed to send ProjectReadDiff: {e}");
            let is_current = failure_state
                .diff_request_ids
                .with_untracked(|requests| requests.get(&failure_key) == Some(&failure_request_id));
            if is_current {
                failure_state.diff_request_ids.update(|requests| {
                    requests.remove(&failure_key);
                });
                failure_state.diff_request_errors.update(|errors| {
                    errors.insert(failure_key.clone(), e.clone());
                });
                failure_state.diff_contents.update(|diffs| {
                    if let Some(diff) = diffs.get_mut(&failure_key) {
                        diff.pending = false;
                    }
                });
            }
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
    use crate::components::center_zone::CenterWorkspaceWidth;
    use crate::state::{ActiveProjectRef, CenterZoneState, PaneId, TabContent, TabId};
    use crate::wasm_test_support::Mounted;
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, CommandErrorCode, CommandErrorPayload, Envelope, FrameKind, Project,
        ProjectBootstrapPayload, ProjectEventPayload, ProjectFileListPayload, ProjectGitChangeKind,
        ProjectGitDiffFile, ProjectGitDiffPayload, ProjectGitFileStatus, ProjectGitStatusPayload,
        ProjectId, ProjectRootGitStatus, ProjectRootPath, ProjectSource, ReviewId, ReviewStatus,
        ReviewSummary, ReviewSummaryScope, SessionId,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

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
        root_with_unstaged("/repo")
    }

    fn root_with_unstaged(path: &str) -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath(path.to_owned()),
            branch: Some("main".to_owned()),
            head_oid: None,
            empty_tree_oid: None,
            ahead: 0,
            behind: 0,
            clean: false,
            files: vec![ProjectGitFileStatus {
                relative_path: "src/foo.rs".to_owned(),
                staged: None,
                unstaged: Some(ProjectGitChangeKind::Modified),
                untracked: false,
            }],
            recent_commits: Vec::new(),
            history_has_more: false,
        }
    }

    fn root_with_staged(path: &str) -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath(path.to_owned()),
            branch: Some("main".to_owned()),
            head_oid: None,
            empty_tree_oid: None,
            ahead: 0,
            behind: 0,
            clean: false,
            files: vec![ProjectGitFileStatus {
                relative_path: "src/staged.rs".to_owned(),
                staged: Some(ProjectGitChangeKind::Modified),
                unstaged: None,
                untracked: false,
            }],
            recent_commits: Vec::new(),
            history_has_more: false,
        }
    }

    fn conflicted_root() -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath("/repo".to_owned()),
            branch: Some("main".to_owned()),
            head_oid: None,
            empty_tree_oid: None,
            ahead: 0,
            behind: 0,
            clean: false,
            files: vec![ProjectGitFileStatus {
                relative_path: "src/conflicted.rs".to_owned(),
                staged: Some(ProjectGitChangeKind::Unmerged),
                unstaged: Some(ProjectGitChangeKind::Unmerged),
                untracked: false,
            }],
            recent_commits: Vec::new(),
            history_has_more: false,
        }
    }

    fn clean_root_with_history() -> ProjectRootGitStatus {
        clean_root_with_history_at("/repo")
    }

    fn clean_root_with_history_at(path: &str) -> ProjectRootGitStatus {
        let oldest = "1111111111111111111111111111111111111111".to_owned();
        let newest = "2222222222222222222222222222222222222222".to_owned();
        ProjectRootGitStatus {
            root: ProjectRootPath(path.to_owned()),
            branch: Some("main".to_owned()),
            head_oid: Some(newest.clone()),
            empty_tree_oid: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned()),
            ahead: 0,
            behind: 0,
            clean: true,
            files: Vec::new(),
            recent_commits: vec![
                protocol::ProjectGitCommitSummary {
                    oid: newest,
                    first_parent_oid: Some(oldest.clone()),
                    subject: "Merge reviewed work".to_owned(),
                    author: "Ada".to_owned(),
                    authored_at_seconds: 1,
                    is_merge: true,
                },
                protocol::ProjectGitCommitSummary {
                    oid: oldest,
                    first_parent_oid: None,
                    subject: "Initial commit".to_owned(),
                    author: "Lin".to_owned(),
                    authored_at_seconds: 1,
                    is_merge: false,
                },
            ],
            history_has_more: false,
        }
    }

    fn long_clean_history(count: usize) -> ProjectRootGitStatus {
        let commits = (0..count)
            .map(|index| {
                let value = count - index;
                protocol::ProjectGitCommitSummary {
                    oid: format!("{value:040x}"),
                    first_parent_oid: (value > 1).then(|| format!("{:040x}", value - 1)),
                    subject: format!("Commit {value}"),
                    author: "Ada".to_owned(),
                    authored_at_seconds: 1,
                    is_merge: false,
                }
            })
            .collect::<Vec<_>>();
        ProjectRootGitStatus {
            root: ProjectRootPath("/repo".to_owned()),
            branch: Some("main".to_owned()),
            head_oid: commits.first().map(|commit| commit.oid.clone()),
            empty_tree_oid: Some("0".repeat(40)),
            ahead: 0,
            behind: 0,
            clean: true,
            files: Vec::new(),
            recent_commits: commits,
            history_has_more: false,
        }
    }

    /// The single active workspace draft summary the server emits per project.
    fn draft_summary() -> ReviewSummary {
        ReviewSummary {
            id: ReviewId("rev-1".to_owned()),
            scope: ReviewSummaryScope::Workspace,
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
            selection: ReviewDiffSelection::Workspace {
                scope: ProjectDiffScope::Unstaged,
            },
            status: ReviewStatus::Draft,
            diffs: vec![],
            file_snapshots: Vec::new(),
            comments: vec![ReviewComment {
                id: ReviewCommentId("c1".to_owned()),
                location: ReviewLocation {
                    root: ProjectRootPath("/repo".to_owned()),
                    relative_path: "src/foo.rs".to_owned(),
                    target: protocol::ReviewTarget::UnstagedDiff,
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

    fn mount_git_panel(
        container: HtmlElement,
        with_draft: bool,
    ) -> Mounted<Rc<RefCell<Option<AppState>>>> {
        mount_git_panel_with_roots(container, with_draft, vec![changed_root()])
    }

    fn mount_git_panel_with_root(
        container: HtmlElement,
        with_draft: bool,
        root: ProjectRootGitStatus,
    ) -> Mounted<Rc<RefCell<Option<AppState>>>> {
        mount_git_panel_with_roots(container, with_draft, vec![root])
    }

    fn mount_git_panel_with_roots(
        container: HtmlElement,
        with_draft: bool,
        roots: Vec<ProjectRootGitStatus>,
    ) -> Mounted<Rc<RefCell<Option<AppState>>>> {
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h1".to_owned(),
                project_id: ProjectId("proj-1".to_owned()),
            }));
            state.git_status.update(|m| {
                m.insert(ProjectId("proj-1".to_owned()), roots);
            });
            if with_draft {
                state.review_summaries.update(|m| {
                    m.insert(ProjectId("proj-1".to_owned()), vec![draft_summary()]);
                });
                // Seed the full record so the status row does not fire a
                // network subscribe (which the headless bridge can't satisfy).
                state.reviews.update(|m| {
                    m.insert(ReviewId("rev-1".to_owned()), full_review());
                });
            }
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <GitPanel /> }
        });
        Mounted::new(handle, holder)
    }

    fn diff_key() -> DiffKey {
        DiffKey::new(
            "h1",
            ProjectId("proj-1".to_owned()),
            ProjectRootPath("/repo".to_owned()),
            ProjectDiffScope::Unstaged,
            "src/foo.rs",
        )
    }

    fn diff_content(key: &DiffKey) -> TabContent {
        TabContent::Diff {
            host_id: key.host_id.clone(),
            project_id: key.project_id.clone(),
            root: key.root.clone(),
            scope: key.scope,
            revision: key.revision.clone(),
            path: key.path.clone(),
        }
    }

    fn mount_side_open_panel(
        container: HtmlElement,
        width: f64,
    ) -> Mounted<Rc<RefCell<Option<AppState>>>> {
        stub_recording_bridge();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h1".to_owned(),
                project_id: ProjectId("proj-1".to_owned()),
            }));
            state.git_status.update(|statuses| {
                statuses.insert(ProjectId("proj-1".to_owned()), vec![changed_root()]);
            });
            let workspace_width = CenterWorkspaceWidth::default();
            workspace_width.set(Some(width));
            provide_context(workspace_width);
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <GitPanel /> }
        });
        Mounted::new(handle, holder)
    }

    fn diff_occurrences(state: &AppState, key: &DiffKey) -> Vec<(PaneId, TabId)> {
        state
            .center_zone
            .with_untracked(|center_zone| center_zone.occurrences(&diff_content(key)))
    }

    fn row_button(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector(".gp-file-btn")
            .unwrap()
            .expect("Git diff row button")
            .dyn_into()
            .unwrap()
    }

    fn query(container: &HtmlElement, selector: &str) -> Option<HtmlElement> {
        container
            .query_selector(selector)
            .unwrap()
            .map(|element| element.dyn_into::<HtmlElement>().unwrap())
    }

    fn query_all(container: &HtmlElement, selector: &str) -> Vec<HtmlElement> {
        let nodes = container.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .map(|index| {
                nodes
                    .item(index)
                    .unwrap()
                    .dyn_into::<HtmlElement>()
                    .unwrap()
            })
            .collect()
    }

    fn click(element: &HtmlElement) {
        element.click();
    }

    fn history_toggle(container: &HtmlElement, index: usize) -> HtmlElement {
        query_all(container, "[data-test=gp-root-history-toggle]")
            .into_iter()
            .nth(index)
            .expect("history toggle for root")
    }

    fn commit_rows(container: &HtmlElement) -> Vec<HtmlElement> {
        query_all(container, "[role=option]")
    }

    /// The working tree is the panel's default: recent commits stay behind
    /// the root's history toggle. Once open, choosing a commit expands its
    /// detail directly beneath the row that was clicked (never at the far
    /// end of the panel), a range moves that detail under the oldest
    /// selected commit, and Escape or the toggle put the working tree back.
    #[wasm_bindgen_test]
    async fn history_is_opt_in_and_the_selected_commit_expands_in_place() {
        ensure_styles_loaded();
        let container = make_container();
        stub_recording_bridge();
        crate::dispatch::clear_host_seqs("h1");
        let mounted =
            mount_git_panel_with_root(container.clone(), false, clean_root_with_history());
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Working tree clean"));
        assert!(
            commit_rows(&container).is_empty(),
            "history must not render until the user asks for it"
        );
        assert!(
            !text.contains("Merge reviewed work"),
            "commit subjects must stay hidden while history is off"
        );
        let toggle = history_toggle(&container, 0);
        assert_eq!(
            toggle.get_attribute("aria-pressed").as_deref(),
            Some("false")
        );

        click(&toggle);
        next_tick().await;
        assert_eq!(
            toggle.get_attribute("aria-pressed").as_deref(),
            Some("true")
        );
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Merge reviewed work"));
        assert!(
            !text.contains("Working tree clean"),
            "history replaces the working tree view for that root"
        );
        assert!(query(&container, ".gp-merge-badge").is_some());
        let rows = commit_rows(&container);
        assert_eq!(rows.len(), 2, "one option per commit and no synthetic rows");
        let newest = rows[0].clone();
        let oldest = rows[1].clone();
        assert!(
            !newest.text_content().unwrap_or_default().contains("Ada"),
            "collapsed rows show the subject, not the author"
        );
        let row_height = newest.get_bounding_client_rect().height();
        assert!(
            row_height > 0.0 && row_height <= 28.0,
            "a collapsed commit row must stay a single compact line; got {row_height}px"
        );
        assert!(
            query(&container, "[data-test=gp-commit-detail]").is_none(),
            "nothing is expanded before a commit is chosen"
        );

        click(&newest);
        next_tick().await;
        assert_eq!(
            newest.get_attribute("aria-selected").as_deref(),
            Some("true")
        );
        assert_eq!(
            oldest.get_attribute("aria-selected").as_deref(),
            Some("false")
        );
        let detail = newest
            .next_element_sibling()
            .expect("detail block follows the selected row");
        assert_eq!(
            detail.get_attribute("data-test").as_deref(),
            Some("gp-commit-detail"),
            "the selected commit expands directly beneath its own row"
        );
        let meta = detail.text_content().unwrap_or_default();
        assert!(
            meta.contains("22222222"),
            "expanded meta shows the sha: {meta}"
        );
        assert!(
            meta.contains("Ada"),
            "expanded meta shows the author: {meta}"
        );
        assert!(
            meta.contains("1 commit"),
            "expanded meta shows the count: {meta}"
        );
        assert!(query(&container, "[data-test=gp-committed-review-starter]").is_some());
        let state = mounted.borrow().clone().unwrap();
        assert!(
            state.center_zone.with_untracked(|zone| {
                !zone
                    .all_tabs()
                    .any(|(_, tab)| matches!(tab.content, TabContent::Diff { .. }))
            }),
            "selecting history must not auto-open a diff"
        );
        let (historical_key, request_id) = state.diff_request_ids.with_untracked(|requests| {
            requests
                .iter()
                .find(|(key, _)| {
                    matches!(key.revision, ProjectDiffRevision::CommittedRange { .. })
                        && key.path.is_empty()
                })
                .map(|(key, request_id)| (key.clone(), request_id.clone()))
                .expect("pending historical diff request")
        });
        let response = Envelope::from_payload(
            StreamPath("/project/proj-1".to_owned()),
            FrameKind::ProjectGitDiff,
            0,
            &ProjectGitDiffPayload {
                request_id: Some(request_id),
                root: historical_key.root.clone(),
                scope: historical_key.scope,
                revision: historical_key.revision.clone(),
                path: None,
                context_mode: state.diff_context_mode.get_untracked(),
                files: vec![ProjectGitDiffFile {
                    relative_path: "src/reviewed.rs".to_owned(),
                    change_kind: Some(ProjectGitChangeKind::Modified),
                    is_binary: false,
                    unmerged: false,
                    hunks: Vec::new(),
                }],
            },
        )
        .unwrap();
        crate::dispatch::dispatch_envelope(&state, "h1", response);
        next_tick().await;
        let range_text = container.text_content().unwrap_or_default();
        assert!(range_text.contains("Changed files"));
        assert!(range_text.contains("reviewed.rs"));
        assert!(
            query(&container, ".gp-file-row.readonly").is_some(),
            "committed files render as read-only rows"
        );
        assert!(
            query(&container, ".gp-file-action").is_none(),
            "committed files expose no staging or discard action"
        );
        click(
            &query(&container, ".gp-file-row.readonly .gp-file-btn").expect("committed file row"),
        );
        next_tick().await;
        let historical_tab_label = state.center_zone.with_untracked(|zone| {
            zone.all_tabs()
                .find_map(|(_, tab)| {
                    matches!(
                        &tab.content,
                        TabContent::Diff {
                            revision: ProjectDiffRevision::CommittedRange { .. },
                            path,
                            ..
                        } if path == "src/reviewed.rs"
                    )
                    .then(|| tab.label.clone())
                })
                .expect("committed diff tab")
        });
        assert!(historical_tab_label.contains("Committed"));
        assert!(historical_tab_label.contains("11111111"));
        assert!(historical_tab_label.contains("22222222"));

        let shift_click_init = web_sys::MouseEventInit::new();
        shift_click_init.set_shift_key(true);
        let shift_click =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("click", &shift_click_init)
                .unwrap();
        oldest.dispatch_event(&shift_click).unwrap();
        next_tick().await;
        assert_eq!(
            newest.get_attribute("aria-selected").as_deref(),
            Some("true")
        );
        assert_eq!(
            oldest.get_attribute("aria-selected").as_deref(),
            Some("true")
        );
        let details = query_all(&container, "[data-test=gp-commit-detail]");
        assert_eq!(details.len(), 1, "a range expands exactly one detail block");
        let detail = oldest
            .next_element_sibling()
            .expect("range detail follows the oldest selected row");
        assert_eq!(
            detail.get_attribute("data-test").as_deref(),
            Some("gp-commit-detail")
        );
        let meta = detail.text_content().unwrap_or_default();
        assert!(
            meta.contains("2 commits"),
            "range meta shows the count: {meta}"
        );
        assert!(
            !meta.contains("Ada") && !meta.contains("Lin"),
            "a range has no single author line: {meta}"
        );

        let escape_init = web_sys::KeyboardEventInit::new();
        escape_init.set_key("Escape");
        let escape =
            web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &escape_init)
                .unwrap();
        oldest.dispatch_event(&escape).unwrap();
        next_tick().await;
        assert_eq!(
            newest.get_attribute("aria-selected").as_deref(),
            Some("false")
        );
        assert_eq!(
            oldest.get_attribute("aria-selected").as_deref(),
            Some("false")
        );
        assert!(
            query(&container, "[data-test=gp-commit-detail]").is_none(),
            "Escape collapses the expanded commit"
        );

        click(&newest);
        next_tick().await;
        assert!(query(&container, "[data-test=gp-commit-detail]").is_some());
        click(&history_toggle(&container, 0));
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Working tree clean"),
            "toggling history off restores the working tree"
        );
        assert!(commit_rows(&container).is_empty());
        click(&history_toggle(&container, 0));
        next_tick().await;
        assert!(
            query(&container, "[data-test=gp-commit-detail]").is_none(),
            "leaving history drops the selection so nothing stays expanded on return"
        );
    }

    #[wasm_bindgen_test]
    async fn historical_selection_stays_with_its_host_and_project() {
        let container = make_container();
        stub_recording_bridge();
        let mounted =
            mount_git_panel_with_root(container.clone(), false, clean_root_with_history());
        next_tick().await;
        click(&history_toggle(&container, 0));
        next_tick().await;
        click(&commit_rows(&container)[0]);
        next_tick().await;
        assert!(query(&container, "[data-test=gp-commit-detail]").is_some());

        let state = mounted.borrow().clone().unwrap();
        state.git_status.update(|statuses| {
            statuses.insert(
                ProjectId("proj-2".to_owned()),
                vec![clean_root_with_history()],
            );
        });
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "h2".to_owned(),
            project_id: ProjectId("proj-2".to_owned()),
        }));
        next_tick().await;

        assert!(
            commit_rows(&container).is_empty(),
            "another project's root starts on its working tree"
        );
        assert!(
            query(&container, "[data-test=gp-commit-detail]").is_none(),
            "the previous project's range must not decorate the new project"
        );
        click(&history_toggle(&container, 0));
        next_tick().await;
        let switched_rows = commit_rows(&container);
        assert_eq!(switched_rows.len(), 2);
        assert_eq!(
            switched_rows[0].get_attribute("aria-selected").as_deref(),
            Some("false"),
            "the same oid in another project is not selected"
        );

        click(&switched_rows[0]);
        next_tick().await;
        assert!(state.diff_request_ids.with_untracked(|requests| {
            requests
                .keys()
                .any(|key| key.host_id == "h2" && key.project_id == ProjectId("proj-2".to_owned()))
        }));
    }

    #[wasm_bindgen_test]
    async fn loaded_history_and_hidden_focus_survive_status_refresh() {
        let container = make_container();
        stub_recording_bridge();
        let mounted = mount_git_panel_with_root(container.clone(), false, long_clean_history(25));
        next_tick().await;
        click(&history_toggle(&container, 0));
        next_tick().await;

        assert_eq!(
            query_all(&container, "[role=option]:not([tabindex='-1'])").len(),
            20,
            "only the first history page may take keyboard focus"
        );
        click(&query(&container, ".gp-history-older").expect("load older history button"));
        next_tick().await;
        assert_eq!(
            query_all(&container, "[role=option]:not([tabindex='-1'])").len(),
            25
        );

        let state = mounted.borrow().clone().unwrap();
        let mut refreshed = long_clean_history(25);
        refreshed.ahead = 1;
        state.git_status.update(|statuses| {
            statuses.insert(ProjectId("proj-1".to_owned()), vec![refreshed]);
        });
        next_tick().await;
        assert_eq!(
            query_all(&container, "[role=option]:not([tabindex='-1'])").len(),
            25,
            "an unrelated status refresh must preserve the loaded history page"
        );
        assert!(query(&container, ".gp-history-older").is_none());
        assert_eq!(
            history_toggle(&container, 0)
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("true"),
            "a status refresh must not flip the root back to the working tree"
        );
    }

    #[wasm_bindgen_test]
    async fn committed_range_failures_leave_loading_and_allow_retry() {
        record_bridge();
        crate::dispatch::clear_host_seqs("h1");
        let container = make_container();
        let mounted =
            mount_git_panel_with_root(container.clone(), false, clean_root_with_history());
        next_tick().await;
        click(&history_toggle(&container, 0));
        next_tick().await;
        click(&commit_rows(&container)[0]);
        next_tick().await;

        let state = mounted.borrow().clone().unwrap();
        let (key, request_id) = state.diff_request_ids.with_untracked(|requests| {
            requests
                .iter()
                .find(|(key, _)| matches!(key.revision, ProjectDiffRevision::CommittedRange { .. }))
                .map(|(key, request_id)| (key.clone(), request_id.clone()))
                .expect("historical diff request")
        });
        let error = Envelope::from_payload(
            StreamPath("/host/h1".to_owned()),
            FrameKind::CommandError,
            0,
            &CommandErrorPayload {
                request_id: Some(request_id.clone()),
                stream: StreamPath("/project/proj-1".to_owned()),
                request_kind: FrameKind::ProjectReadDiff,
                operation: "project_read_diff".to_owned(),
                code: CommandErrorCode::NotFound,
                message: "the pinned commit no longer exists".to_owned(),
                fatal: false,
            },
        )
        .unwrap();
        crate::dispatch::dispatch_envelope(&state, "h1", error);
        next_tick().await;
        assert!(query(&container, "[data-test=gp-historical-diff-error]").is_some());
        assert!(
            !container
                .text_content()
                .unwrap_or_default()
                .contains("Loading committed changes")
        );
        assert!(
            !state
                .diff_contents
                .with_untracked(|diffs| { diffs.get(&key).is_some_and(|diff| diff.pending) })
        );

        click(&query(&container, "[data-test=gp-historical-diff-error] button").expect("retry"));
        next_tick().await;
        let retry_id = state
            .diff_request_ids
            .with_untracked(|requests| requests.get(&key).cloned())
            .expect("retry request id");
        assert_ne!(retry_id, request_id);

        click(
            &query(&container, "[data-test=gp-committed-review-starter] button")
                .expect("start review button"),
        );
        next_tick().await;
        let create_request_id = sent_lines_joined()
            .lines()
            .filter_map(|line| serde_json::from_str::<Envelope>(line).ok())
            .filter(|envelope| envelope.kind == FrameKind::ReviewCreate)
            .filter_map(|envelope| {
                envelope
                    .parse_payload::<protocol::ReviewCreatePayload>()
                    .ok()
            })
            .filter_map(|payload| payload.request_id)
            .last()
            .expect("committed review create request id");
        let create_error = Envelope::from_payload(
            StreamPath("/host/h1".to_owned()),
            FrameKind::CommandError,
            1,
            &CommandErrorPayload {
                request_id: Some(create_request_id),
                stream: StreamPath("/project/proj-1".to_owned()),
                request_kind: FrameKind::ReviewCreate,
                operation: "review_create".to_owned(),
                code: CommandErrorCode::NotFound,
                message: "the selected range was rewritten".to_owned(),
                fatal: false,
            },
        )
        .unwrap();
        crate::dispatch::dispatch_envelope(&state, "h1", create_error);
        next_tick().await;
        assert!(query(&container, "[data-test=gp-review-create-error]").is_some());
        let start_button =
            query(&container, "[data-test=gp-committed-review-starter] button").unwrap();
        assert!(!start_button.has_attribute("disabled"));
    }

    /// A project with several roots keeps the panel to one header line per
    /// root that has nothing to show: dirty roots open, clean roots collapse.
    /// The user's own toggle wins over that default and outlives a status
    /// refresh, and a lone root never collapses.
    #[wasm_bindgen_test]
    async fn multi_root_collapses_clean_roots_and_remembers_toggles() {
        ensure_styles_loaded();
        let container = make_container();
        stub_recording_bridge();
        let mounted = mount_git_panel_with_roots(
            container.clone(),
            false,
            vec![
                root_with_unstaged("/repo-a"),
                clean_root_with_history_at("/repo-b"),
                clean_root_with_history_at("/repo-c"),
            ],
        );
        next_tick().await;

        let toggles = query_all(&container, "[data-test=gp-root-toggle]");
        assert_eq!(toggles.len(), 3);
        assert_eq!(
            toggles[0].get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        assert_eq!(
            toggles[1].get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert_eq!(
            toggles[2].get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert_eq!(query_all(&container, ".gp-file-name").len(), 1);
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Working tree clean"),
            "collapsed clean roots must not spend a body line on their state"
        );
        let states = query_all(&container, "[data-test=gp-root-state]");
        assert_eq!(states[0].text_content().as_deref(), Some("1"));
        assert_eq!(states[1].text_content().as_deref(), Some("\u{2713}"));
        let header_height = query(&container, ".gp-root-header")
            .unwrap()
            .get_bounding_client_rect()
            .height();
        assert!(
            header_height > 0.0 && header_height <= 32.0,
            "a root header is one compact line; got {header_height}px"
        );
        assert!(
            query(&container, ".gp-header").is_none() && query(&container, ".gp-branch").is_none(),
            "there is no panel-level header repeating the branch"
        );

        click(&toggles[1]);
        next_tick().await;
        assert_eq!(
            toggles[1].get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Working tree clean")
        );

        let state = mounted.borrow().clone().unwrap();
        let mut refreshed_b = clean_root_with_history_at("/repo-b");
        refreshed_b.ahead = 1;
        state.git_status.update(|statuses| {
            statuses.insert(
                ProjectId("proj-1".to_owned()),
                vec![
                    root_with_unstaged("/repo-a"),
                    refreshed_b,
                    clean_root_with_history_at("/repo-c"),
                ],
            );
        });
        next_tick().await;
        let toggles = query_all(&container, "[data-test=gp-root-toggle]");
        assert_eq!(
            toggles[1].get_attribute("aria-expanded").as_deref(),
            Some("true"),
            "an explicit expand must survive a status refresh"
        );
        assert_eq!(
            toggles[2].get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert!(
            toggles[1]
                .text_content()
                .unwrap_or_default()
                .contains("\u{2191}1")
        );

        click(&history_toggle(&container, 2));
        next_tick().await;
        let toggles = query_all(&container, "[data-test=gp-root-toggle]");
        assert_eq!(
            toggles[2].get_attribute("aria-expanded").as_deref(),
            Some("true"),
            "asking for a collapsed root's history opens that root"
        );
        assert_eq!(commit_rows(&container).len(), 2);

        let single = make_container();
        let _single = mount_git_panel_with_root(single.clone(), false, clean_root_with_history());
        next_tick().await;
        assert_eq!(
            query(&single, "[data-test=gp-root-toggle]")
                .unwrap()
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("true"),
            "a lone root is always open, clean or not"
        );
        assert!(
            single
                .text_content()
                .unwrap_or_default()
                .contains("Working tree clean")
        );
    }

    /// The commit message box is behind the Staged section's "Commit…"
    /// action, so a root with staged files does not permanently spend the
    /// space, and committing sends the typed message for that root.
    #[wasm_bindgen_test]
    async fn commit_form_is_behind_the_staged_commit_action() {
        record_bridge();
        let container = make_container();
        let _mounted =
            mount_git_panel_with_root(container.clone(), false, root_with_staged("/repo"));
        next_tick().await;

        assert!(
            query(&container, ".gp-commit-input").is_none(),
            "the commit box must not render until asked for"
        );
        let toggle = query(&container, "[data-test=gp-commit-toggle]").expect("commit action");
        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        click(&toggle);
        next_tick().await;
        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        let input: web_sys::HtmlTextAreaElement = query(&container, ".gp-commit-input")
            .expect("commit box")
            .dyn_into()
            .unwrap();
        let commit_button = query(&container, ".gp-commit-btn").expect("commit button");
        assert!(commit_button.has_attribute("disabled"));

        input.set_value("Fix staged thing");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(!commit_button.has_attribute("disabled"));
        click(&commit_button);
        next_tick().await;
        let sent = sent_lines_joined();
        assert!(
            sent.contains("project_git_commit") && sent.contains("Fix staged thing"),
            "committing must send the typed message; sent: {sent}"
        );
        assert!(
            sent.contains("/repo"),
            "the commit targets its root; sent: {sent}"
        );
        assert_eq!(input.value(), "", "the box clears after committing");
    }

    /// Splits are created by dragging tabs: a git diff row is a plain
    /// open-on-click control with no side-open action, menu, or chord.
    #[wasm_bindgen_test]
    async fn git_diff_rows_have_no_side_open_control() {
        let container = make_container();
        let _holder = mount_side_open_panel(container.clone(), 900.0);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"gp-diff-open-side\"]")
                .unwrap()
                .is_none(),
            "git diff rows must not expose a side-open control"
        );
        assert!(
            container
                .query_selector("[data-test=\"gp-diff-open-menu\"]")
                .unwrap()
                .is_none(),
            "git diff rows must not expose an open menu"
        );
    }

    #[wasm_bindgen_test]
    async fn unmerged_file_renders_once_in_conflicts_with_safe_actions() {
        let container = make_container();
        stub_recording_bridge();
        let mounted = mount_git_panel_with_root(container.clone(), false, conflicted_root());
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Conflicts"));
        assert!(!text.contains("Changes"));
        assert!(!text.contains("Staged"));
        assert_eq!(
            container
                .query_selector_all(".gp-file-name")
                .unwrap()
                .length(),
            1,
            "the conflicted path must not be duplicated into staged or changes"
        );
        assert_eq!(
            container
                .query_selector(".gp-status-icon.unmerged")
                .unwrap()
                .expect("unmerged status icon")
                .text_content()
                .as_deref(),
            Some("U")
        );
        assert!(
            container.query_selector(".gp-stage-btn").unwrap().is_some(),
            "whole-file staging remains available after conflict resolution"
        );
        assert!(
            container
                .query_selector(".gp-unstage-btn")
                .unwrap()
                .is_none()
        );
        assert!(
            container
                .query_selector(".gp-discard-btn")
                .unwrap()
                .is_none()
        );
        let root_state = query(&container, "[data-test=gp-root-state]").unwrap();
        assert!(
            root_state
                .text_content()
                .unwrap_or_default()
                .contains("1 conflict"),
            "the root header calls out conflicts"
        );

        row_button(&container).click();
        next_tick().await;

        let state = mounted.borrow().clone().unwrap();
        let key = DiffKey::new(
            "h1",
            ProjectId("proj-1".to_owned()),
            ProjectRootPath("/repo".to_owned()),
            ProjectDiffScope::Unstaged,
            "src/conflicted.rs",
        );
        assert_eq!(diff_occurrences(&state, &key).len(), 1);
        assert!(
            state
                .diff_contents
                .with_untracked(|diffs| diffs.contains_key(&key)),
            "the conflict row must request the typed unstaged diff"
        );
    }

    #[wasm_bindgen_test]
    async fn ordinary_diff_click_still_opens_in_focused_pane() {
        let container = make_container();
        let holder = mount_side_open_panel(container.clone(), 900.0);
        next_tick().await;

        row_button(&container).click();
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        let key = diff_key();
        let occurrences = diff_occurrences(&state, &key);
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].0, PaneId::Primary);
        assert!(!state.center_zone.with_untracked(CenterZoneState::is_split));
    }

    /// No draft review ⇒ the git panel shows no review status row: there is
    /// no active workspace draft to summarize.
    #[wasm_bindgen_test]
    async fn no_draft_shows_no_review_status_row() {
        let container = make_container();
        let _mounted = mount_git_panel(container.clone(), false);
        next_tick().await;

        assert!(
            query(&container, "[data-test=gp-review-status]").is_none(),
            "the review status row must not show without an active draft review"
        );
    }

    /// A Draft review ⇒ exactly one compact status row for the project with
    /// the workspace-wide counts and a way into the review surface. The
    /// review controls themselves are not in the panel.
    #[wasm_bindgen_test]
    async fn draft_shows_one_review_status_row_with_counts() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted = mount_git_panel(container.clone(), true);
        next_tick().await;

        let rows = query_all(&container, "[data-test=gp-review-status]");
        assert_eq!(rows.len(), 1, "exactly one review status row must render");
        let text = query(&container, "[data-test=gp-review-counts]")
            .expect("counts element present")
            .text_content()
            .unwrap_or_default();
        assert!(
            text.contains("1 comment"),
            "expected the workspace comment count in the row; got: {text}"
        );
        assert!(query(&container, "[data-test=gp-review-open]").is_some());
        assert!(
            query(&container, "[data-test=review-run-ai]").is_none()
                && query(&container, ".review-submit-btn").is_none(),
            "AI reviewer and submit controls live on the review surface, not the panel"
        );
        let height = rows[0].get_bounding_client_rect().height();
        assert!(
            height > 0.0 && height <= 32.0,
            "the status row is one line; got {height}px"
        );
    }

    /// A file with review comments shows a per-file "(N)" badge in the file
    /// list. `mount_git_panel` seeds one User comment on `src/foo.rs` in
    /// `/repo`; with the workspace summary carrying no per-file counts, the
    /// badge derives from the loaded review record, filtered to this root.
    #[wasm_bindgen_test]
    async fn file_row_shows_comment_count_badge() {
        let container = make_container();
        let _mounted = mount_git_panel(container.clone(), true);
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
        let _mounted = mount_git_panel(container.clone(), false);
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

    /// A multi-root project renders exactly ONE review status row (not one
    /// per root), and its Open button opens the project-level (workspace)
    /// comments surface.
    #[wasm_bindgen_test]
    async fn review_status_open_opens_project_comments() {
        stub_recording_bridge();
        let container = make_container();
        let mounted = mount_git_panel_with_roots(
            container.clone(),
            true,
            vec![root_with_unstaged("/repo-a"), root_with_unstaged("/repo-b")],
        );
        next_tick().await;

        assert_eq!(
            query_all(&container, "[data-test=gp-review-status]").len(),
            1,
            "a multi-root project must render exactly one review status row"
        );

        click(&query(&container, "[data-test=gp-review-open]").expect("Open button"));
        next_tick().await;

        let state = mounted.borrow().clone().unwrap();
        let opened = state.center_zone.with_untracked(|cz| {
            cz.all_tabs().find_map(|(_, t)| match &t.content {
                TabContent::Comments { project_id, .. } => Some(project_id.clone()),
                _ => None,
            })
        });
        let pid = opened.expect("a workspace comments tab must open on Open");
        assert_eq!(
            pid,
            ProjectId("proj-1".to_owned()),
            "Open must show the project's workspace comments surface"
        );
    }

    /// The create flow (server echoes `ReviewListChanged` for a pending
    /// create) must NOT auto-open any review surface tab — it only releases
    /// the pending token. Driven through `dispatch_envelope` so no network is
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
        let _mounted = Mounted::new(handle, ());
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
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
                    source: ProjectSource::Standalone {
                        roots: vec![ProjectRootPath("/repo".to_owned())],
                    },
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

        let pending = state
            .review_create_pending
            .with_untracked(|m| m.get(&key).copied().unwrap_or(0));
        assert_eq!(pending, 0, "create-pending token must be released");
        let opened_surface = state.center_zone.with_untracked(|cz| {
            cz.all_tabs()
                .any(|(_, t)| matches!(t.content, TabContent::Diff { .. }))
        });
        assert!(
            !opened_surface,
            "ReviewListChanged must not auto-open a diff surface tab"
        );
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
    /// create token must still be released.
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
        let _mounted = Mounted::new(handle, ());
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
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
                    source: ProjectSource::Standalone {
                        roots: vec![ProjectRootPath("/repo".to_owned())],
                    },
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

        let pending = state
            .review_create_pending
            .with_untracked(|m| m.get(&key).copied().unwrap_or(0));
        assert_eq!(
            pending, 0,
            "create-pending token must release even with no new id"
        );
        let opened_surface = state.center_zone.with_untracked(|cz| {
            cz.all_tabs()
                .any(|(_, t)| matches!(t.content, TabContent::Diff { .. }))
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

    /// Recording bridge that captures the serialized envelope `line` of every
    /// `send_host_line` invoke into `window.__sent_lines`.
    fn record_bridge() {
        let _ = js_sys::eval(
            "(function(){ \
               window.__sent_lines = []; \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(cmd, args){ \
                 try { \
                   if (cmd === 'send_host_line' && args) { \
                     var line = (args.line !== undefined) ? args.line \
                       : (args.get ? args.get('line') : undefined); \
                     if (line !== undefined) { window.__sent_lines.push(line); } \
                   } \
                 } catch (e) {} \
                 return Promise.resolve(); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
    }

    fn sent_lines_joined() -> String {
        js_sys::eval("(window.__sent_lines||[]).join('\\n')")
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default()
    }

    /// `subscribe_review_reactive` must retry reactively: it subscribes when
    /// the record is absent, stays quiet while it's present, and
    /// **resubscribes when the record is later lost**.
    #[wasm_bindgen_test]
    async fn hub_resubscribes_when_record_lost() {
        stub_recording_bridge();
        let review_id = ReviewId("rev-1".to_owned());
        let container = make_container();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.connection_statuses.update(|m| {
                m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
            });
            let target: Memo<Option<(String, ReviewId)>> =
                Memo::new(move |_| Some(("h1".to_owned(), ReviewId("rev-1".to_owned()))));
            crate::components::review_view::subscribe_review_reactive(&state, target);
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <div></div> }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;

        assert_eq!(
            invoke_count(),
            1,
            "the helper must subscribe while the record is absent"
        );

        let state = holder.borrow().clone().unwrap();
        state.reviews.update(|m| {
            m.insert(review_id.clone(), full_review());
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            1,
            "no resubscribe should fire while the record is present"
        );

        state.reviews.update(|m| {
            m.remove(&review_id);
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            2,
            "the helper must resubscribe after the record is lost"
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

    /// A persistently-failing subscribe must NOT tight-loop: the first attempt
    /// fires, then retries are deferred behind a backoff timer.
    #[wasm_bindgen_test]
    async fn hub_subscribe_failure_backs_off_no_tight_loop() {
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
            let target: Memo<Option<(String, ReviewId)>> =
                Memo::new(move |_| Some(("h1".to_owned(), ReviewId("rev-1".to_owned()))));
            crate::components::review_view::subscribe_review_reactive(&state, target);
            provide_context(state);
            view! { <div></div> }
        });
        let _mounted = Mounted::new(handle, ());

        next_tick().await;
        next_tick().await;
        next_tick().await;
        assert_eq!(
            invoke_count(),
            1,
            "a failed subscribe must not re-issue immediately (tight loop)"
        );

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
            let target: Memo<Option<(String, ReviewId)>> =
                Memo::new(move |_| Some(("h1".to_owned(), ReviewId("rev-1".to_owned()))));
            crate::components::review_view::subscribe_review_reactive(&state, target);
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <div></div> }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;
        assert_eq!(invoke_count(), 1, "initial subscribe");

        let state = holder.borrow().clone().unwrap();
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

        state.connection_statuses.update(|m| {
            m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
        });
        next_tick().await;
        assert_eq!(
            invoke_count(),
            2,
            "the helper must resubscribe after reconnecting"
        );
    }

    /// If the subscribe target temporarily becomes `None` after a subscribe
    /// that never received a bootstrap, the in-flight latch must be dropped so
    /// the same target reappearing resubscribes.
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
        let _mounted = Mounted::new(handle, ());
        next_tick().await;
        assert_eq!(invoke_count(), 1, "initial subscribe");

        let target_sig = holder.borrow().clone().unwrap();
        target_sig.set(None);
        next_tick().await;
        assert_eq!(invoke_count(), 1, "no subscribe while the target is None");

        target_sig.set(Some(("h1".to_owned(), ReviewId("rev-1".to_owned()))));
        next_tick().await;
        assert_eq!(
            invoke_count(),
            2,
            "the same target reappearing must resubscribe"
        );
    }
}
