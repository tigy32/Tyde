use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, EmptyState, Pill, PillTone, Spinner,
};
use crate::state::{ActiveProjectRef, AppState, ProjectDiffRef, ReviewRef};
use protocol::{
    ReviewAnchor, ReviewAnchorStatus, ReviewComment, ReviewDiffSide, ReviewLocation,
    ReviewSuggestedComment,
};

/// Unified git diff viewer for an active project's root.
///
/// On mount, fires `ProjectReadDiff` with the requested scope/path.
/// Renders the diff inline below the file tree (Projects view) or
/// inside a review-detail diff tab.
///
/// When `scope == Uncommitted` this is also the primary inline-review
/// surface: it locates the active project's singleton review, overlays
/// its comments and AI suggestions on the matching diff lines/files,
/// and exposes tap-to-comment plus review controls (AI review, clear,
/// submit with a target picker). Staging / discard / commit are
/// intentionally omitted — too destructive for a phone-sized UI in v1.
#[component]
pub fn DiffViewer(
    project: ActiveProjectRef,
    root: protocol::ProjectRootPath,
    scope: protocol::ProjectDiffScope,
    path: Option<String>,
    on_close: Callback<()>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let context_mode: RwSignal<protocol::DiffContextMode> =
        RwSignal::new(protocol::DiffContextMode::Hunks);

    // The inline composer's anchor location, if any. `None` means no
    // composer is open. Comments are project-scoped, so the location
    // (root + relative_path + anchor) fully identifies the thread.
    let composer: RwSignal<Option<ReviewLocation>> = RwSignal::new(None);

    let key = ProjectDiffRef {
        local_host_id: project.local_host_id.clone(),
        project_id: project.project_id.clone(),
        root: root.clone(),
        scope,
        path: path.clone(),
    };

    // Whether inline review is enabled for this surface. Reviews operate
    // on the uncommitted working tree, so only enable for that scope.
    let review_enabled = scope == protocol::ProjectDiffScope::Uncommitted;

    // Resolve the active review id for this project (singleton). Prefer a
    // Draft review; fall back to the most-recently-updated non-cancelled
    // one. Reactive over `review_summaries`.
    let project_for_review = project.clone();
    let active_review_id = Memo::new(move |_| {
        if !review_enabled {
            return None;
        }
        let key = (
            project_for_review.local_host_id.clone(),
            project_for_review.project_id.clone(),
        );
        state.review_summaries.with(|map| {
            let summaries = map.get(&key)?;
            // Prefer a Draft; else most recent non-cancelled.
            let mut candidates: Vec<&protocol::ReviewSummary> = summaries
                .iter()
                .filter(|s| !matches!(s.status, protocol::ReviewStatus::Cancelled { .. }))
                .collect();
            candidates.sort_by(|a, b| {
                let a_draft = matches!(a.status, protocol::ReviewStatus::Draft);
                let b_draft = matches!(b.status, protocol::ReviewStatus::Draft);
                b_draft
                    .cmp(&a_draft)
                    .then(b.updated_at_ms.cmp(&a.updated_at_ms))
            });
            candidates.first().map(|s| s.id.clone())
        })
    });

    // Subscribe to the active review when one appears so the overlay has
    // a live `Review` projection.
    {
        let project = project.clone();
        let state = state.clone();
        Effect::new(move |_| {
            let Some(review_id) = active_review_id.get() else {
                return;
            };
            let host = project.local_host_id.clone();
            let rkey = ReviewRef {
                local_host_id: host.clone(),
                review_id: review_id.clone(),
            };
            let already_known = state.reviews.with_untracked(|r| r.contains_key(&rkey))
                || state
                    .review_streams
                    .with_untracked(|s| s.contains_key(&rkey));
            if already_known {
                return;
            }
            let connected = state
                .host_streams
                .with_untracked(|streams| streams.contains_key(&host));
            if !connected {
                return;
            }
            let state = state.clone();
            spawn_local(async move {
                if let Err(e) = crate::actions::subscribe_review(&state, &host, review_id).await {
                    log::error!("subscribe_review failed: {e}");
                }
            });
        });
    }

    // Kick the diff request on mount and re-fire when context_mode changes.
    {
        let project = project.clone();
        let root = root.clone();
        let path = path.clone();
        let state = state.clone();
        Effect::new(move |_| {
            let mode = context_mode.get();
            // Skip dispatch when no host stream — keeps headless tests
            // from crashing on the absent Tauri bridge and avoids
            // pointless requests when the host is disconnected.
            let connected = state
                .host_streams
                .with_untracked(|streams| streams.contains_key(&project.local_host_id));
            if !connected {
                return;
            }
            let project = project.clone();
            let root = root.clone();
            let path = path.clone();
            let state = state.clone();
            spawn_local(async move {
                if let Err(e) =
                    crate::actions::request_project_diff(&state, &project, root, scope, path, mode)
                        .await
                {
                    log::error!("request_project_diff failed: {e}");
                }
            });
        });
    }

    let key_for_render = key.clone();
    let state_for_diff = state.clone();
    let entry = move || {
        state_for_diff
            .project_diffs
            .with(|diffs| diffs.get(&key_for_render).cloned())
    };

    let scope_label = match scope {
        protocol::ProjectDiffScope::Unstaged => "Unstaged",
        protocol::ProjectDiffScope::Staged => "Staged",
        protocol::ProjectDiffScope::Uncommitted => "Uncommitted",
    };
    let root_short = short_root(&root.0);
    let path_suffix = path
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|p| format!(" · {p}"))
        .unwrap_or_default();

    let on_toggle_full = Callback::new(move |_: ()| {
        context_mode.update(|m| {
            *m = match *m {
                protocol::DiffContextMode::Hunks => protocol::DiffContextMode::FullFile,
                protocol::DiffContextMode::FullFile => protocol::DiffContextMode::Hunks,
            };
        });
    });

    // Build the per-render review overlay context. Cloned into the body
    // closure so it stays reactive over the active review + composer.
    let project_for_ctx = project.clone();
    let state_for_ctx = state.clone();
    let review_ctx = move || {
        let review_id = active_review_id.get()?;
        Some(ReviewCtx {
            host: project_for_ctx.local_host_id.clone(),
            review_id,
            composer,
            state: state_for_ctx.clone(),
        })
    };

    let review_ctx_for_controls = review_ctx.clone();
    let project_for_controls = project.clone();

    view! {
        <div class="project-diff-viewer" data-mobile-test="project-diff-viewer">
            <div class="project-diff-viewer-header">
                <div class="project-diff-viewer-title" data-mobile-test="project-diff-viewer-title">
                    <span>{scope_label}</span>
                    <span class="project-diff-viewer-sep">"·"</span>
                    <span class="project-diff-viewer-root">{root_short}</span>
                    <span class="project-diff-viewer-path">{path_suffix}</span>
                </div>
                <span style="display: flex; gap: var(--space-1); align-items: center;">
                    {move || {
                        let label = match context_mode.get() {
                            protocol::DiffContextMode::Hunks => "Full",
                            protocol::DiffContextMode::FullFile => "Hunks",
                        };
                        view! {
                            <Button
                                label=label
                                variant=ButtonVariant::Ghost
                                size=ButtonSize::Compact
                                data_mobile_test="project-diff-viewer-context-toggle"
                                aria_label="Toggle diff context mode".to_string()
                                on_click=on_toggle_full
                            />
                        }
                    }}
                    <Button
                        label="Close"
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Compact
                        data_mobile_test="project-diff-viewer-close"
                        aria_label="Close diff viewer".to_string()
                        on_click=on_close
                    />
                </span>
            </div>
            {move || {
                if !review_enabled {
                    return view! { <span></span> }.into_any();
                }
                view! {
                    <ReviewControls
                        project=project_for_controls.clone()
                        ctx=review_ctx_for_controls()
                    />
                }
                .into_any()
            }}
            <div class="project-diff-viewer-body">
                {move || render_body(entry(), review_ctx())}
            </div>
        </div>
    }
}

/// Everything the inline-review overlay needs, resolved once per render.
#[derive(Clone)]
struct ReviewCtx {
    host: crate::state::LocalHostId,
    review_id: protocol::ReviewId,
    composer: RwSignal<Option<ReviewLocation>>,
    state: AppState,
}

impl ReviewCtx {
    fn review_ref(&self) -> ReviewRef {
        ReviewRef {
            local_host_id: self.host.clone(),
            review_id: self.review_id.clone(),
        }
    }

    fn review(&self) -> Option<protocol::Review> {
        let key = self.review_ref();
        self.state.reviews.with(|r| r.get(&key).cloned())
    }
}

fn render_body(
    entry: Option<crate::state::ProjectDiffState>,
    review_ctx: Option<ReviewCtx>,
) -> AnyView {
    let Some(state) = entry else {
        return view! {
            <div class="project-diff-viewer-loading" data-mobile-test="project-diff-viewer-loading">
                <Spinner aria_label="Loading diff".to_string() />
                <span class="project-diff-viewer-loading-text">"Loading diff…"</span>
            </div>
        }
        .into_any();
    };

    if state.pending && state.files.is_empty() {
        return view! {
            <div class="project-diff-viewer-loading" data-mobile-test="project-diff-viewer-loading">
                <Spinner aria_label="Loading diff".to_string() />
                <span class="project-diff-viewer-loading-text">"Loading diff…"</span>
            </div>
        }
        .into_any();
    }

    if state.files.is_empty() {
        return view! {
            <EmptyState
                title="No changes"
                body="Nothing to show — the working tree is clean for this root and scope."
                icon="\u{2728}"
                data_mobile_test="project-diff-viewer-empty"
            />
        }
        .into_any();
    }

    let total_files = state.files.len();
    let pending_now = state.pending;
    let root = state.root.clone();
    view! {
        <div class="project-diff-viewer-ready">
            <div class="project-diff-viewer-meta" data-mobile-test="project-diff-viewer-meta">
                <Pill
                    label=format!("{total_files} file{}", if total_files == 1 { "" } else { "s" })
                    tone=PillTone::Neutral
                    data_mobile_test="project-diff-viewer-file-count"
                />
                {if pending_now {
                    view! {
                        <Pill
                            label="Refreshing…"
                            tone=PillTone::Accent
                            data_mobile_test="project-diff-viewer-refreshing"
                        />
                    }.into_any()
                } else {
                    view! { <span></span> }.into_any()
                }}
            </div>
            <div class="project-diff-files">
                {state.files.into_iter().map(|file| {
                    view! {
                        <DiffFileBlock
                            file=file
                            root=root.clone()
                            review_ctx=review_ctx.clone()
                        />
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
    .into_any()
}

#[component]
fn DiffFileBlock(
    file: protocol::ProjectGitDiffFile,
    root: protocol::ProjectRootPath,
    review_ctx: Option<ReviewCtx>,
) -> impl IntoView {
    let hunk_count = file.hunks.len();
    let mut added = 0u32;
    let mut removed = 0u32;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                protocol::ProjectGitDiffLineKind::Added => added += 1,
                protocol::ProjectGitDiffLineKind::Removed => removed += 1,
                protocol::ProjectGitDiffLineKind::Context => {}
            }
        }
    }
    let path = file.relative_path.clone();
    let path_for_btn = file.relative_path.clone();
    let path_for_thread = file.relative_path.clone();
    let root_for_btn = root.clone();
    let root_for_thread = root.clone();

    // File-level comment affordance + thread region, only with a review.
    let file_comment_btn = {
        let ctx = review_ctx.clone();
        move || match ctx.clone() {
            None => view! { <span></span> }.into_any(),
            Some(ctx) => {
                let composer = ctx.composer;
                let click_root = root_for_btn.clone();
                let click_path = path_for_btn.clone();
                view! {
                    <button
                        type="button"
                        class="project-diff-file-comment-btn"
                        data-mobile-test="diff-file-comment-btn"
                        aria-label="Comment on file"
                        on:click=move |_| {
                            composer.set(Some(ReviewLocation {
                                root: click_root.clone(),
                                relative_path: click_path.clone(),
                                anchor: ReviewAnchor::File,
                            }));
                        }
                    >
                        "+ Comment"
                    </button>
                }
                .into_any()
            }
        }
    };

    let file_thread = {
        let ctx = review_ctx.clone();
        move || {
            let Some(ctx) = ctx.clone() else {
                return view! { <span></span> }.into_any();
            };
            let matcher: ThreadMatcher =
                std::sync::Arc::new(|a: &ReviewAnchor| matches!(a, ReviewAnchor::File));
            view! {
                <ThreadRegion
                    ctx=ctx
                    root=root_for_thread.clone()
                    relative_path=path_for_thread.clone()
                    matcher=matcher
                />
            }
            .into_any()
        }
    };

    view! {
        <details class="project-diff-file" data-mobile-test="project-diff-file" open=true>
            <summary class="project-diff-file-summary">
                <span class="project-diff-file-path" data-mobile-test="project-diff-file-path">{path}</span>
                <span class="project-diff-file-stats">
                    <span class="project-diff-stat-added" data-mobile-test="project-diff-stat-added">"+"{added}</span>
                    <span class="project-diff-stat-removed" data-mobile-test="project-diff-stat-removed">"-"{removed}</span>
                    <span class="project-diff-stat-hunks">{hunk_count}" hunks"</span>
                </span>
            </summary>
            {file_comment_btn}
            {file_thread}
            <div class="project-diff-hunks">
                {file.hunks.into_iter().map(|hunk| {
                    view! {
                        <DiffHunkBlock
                            hunk=hunk
                            root=root.clone()
                            relative_path=file.relative_path.clone()
                            review_ctx=review_ctx.clone()
                        />
                    }
                }).collect::<Vec<_>>()}
            </div>
        </details>
    }
}

#[component]
fn DiffHunkBlock(
    hunk: protocol::ProjectGitDiffHunk,
    root: protocol::ProjectRootPath,
    relative_path: String,
    review_ctx: Option<ReviewCtx>,
) -> impl IntoView {
    let header = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    );
    view! {
        <div class="project-diff-hunk" data-mobile-test="project-diff-hunk">
            <div class="project-diff-hunk-header">{header}</div>
            <div class="project-diff-hunk-lines">
                {hunk.lines.into_iter().map(|line| {
                    view! {
                        <DiffLineRow
                            line=line
                            root=root.clone()
                            relative_path=relative_path.clone()
                            review_ctx=review_ctx.clone()
                        />
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
}

#[component]
fn DiffLineRow(
    line: protocol::ProjectGitDiffLine,
    root: protocol::ProjectRootPath,
    relative_path: String,
    review_ctx: Option<ReviewCtx>,
) -> impl IntoView {
    let (kind_class, test_id, marker) = match line.kind {
        protocol::ProjectGitDiffLineKind::Added => ("added", "diff-line-added", "+"),
        protocol::ProjectGitDiffLineKind::Removed => ("removed", "diff-line-removed", "-"),
        protocol::ProjectGitDiffLineKind::Context => ("context", "diff-line-context", " "),
    };
    let line_no = line
        .new_line_number
        .or(line.old_line_number)
        .map(|n| n.to_string())
        .unwrap_or_default();

    // Anchor side + line for review: prefer the new side (post-change),
    // fall back to the old side for pure deletions. Mirrors the desktop
    // matcher which keys a `LineRange` by `(side, end_line)`.
    let anchor_side_line: Option<(ReviewDiffSide, u32)> = line
        .new_line_number
        .map(|n| (ReviewDiffSide::New, n))
        .or_else(|| line.old_line_number.map(|n| (ReviewDiffSide::Old, n)));

    let line_text = line.text.clone();

    let tap_to_comment = {
        let ctx = review_ctx.clone();
        let root = root.clone();
        let relative_path = relative_path.clone();
        move |_| {
            let Some(ctx) = ctx.clone() else { return };
            let Some((side, line_no)) = anchor_side_line else {
                return;
            };
            ctx.composer.set(Some(ReviewLocation {
                root: root.clone(),
                relative_path: relative_path.clone(),
                anchor: ReviewAnchor::LineRange {
                    side,
                    start_line: line_no,
                    end_line: line_no,
                },
            }));
        }
    };

    let has_review = review_ctx.is_some();

    let line_thread = {
        let ctx = review_ctx.clone();
        let root = root.clone();
        let relative_path = relative_path.clone();
        move || {
            let Some(ctx) = ctx.clone() else {
                return view! { <span></span> }.into_any();
            };
            let Some((side, line_no)) = anchor_side_line else {
                return view! { <span></span> }.into_any();
            };
            let matcher: ThreadMatcher = std::sync::Arc::new(move |a: &ReviewAnchor| match a {
                ReviewAnchor::LineRange {
                    side: s, end_line, ..
                } => *s == side && *end_line == line_no,
                _ => false,
            });
            view! {
                <ThreadRegion
                    ctx=ctx
                    root=root.clone()
                    relative_path=relative_path.clone()
                    matcher=matcher
                />
            }
            .into_any()
        }
    };

    view! {
        <div class="project-diff-line-wrap">
            <div
                class=format!("project-diff-line {kind_class}")
                data-mobile-test=test_id
                on:click=tap_to_comment
            >
                <span class="project-diff-line-no" aria-hidden="true">{line_no}</span>
                <span class="project-diff-line-marker" aria-hidden="true">{marker}</span>
                <span class="project-diff-line-text">{line_text}</span>
                {move || {
                    if has_review && anchor_side_line.is_some() {
                        view! {
                            <span
                                class="project-diff-line-comment-hint"
                                data-mobile-test="diff-line-comment-btn"
                                aria-hidden="true"
                            >"+"</span>
                        }
                        .into_any()
                    } else {
                        view! { <span></span> }.into_any()
                    }
                }}
            </div>
            {line_thread}
        </div>
    }
}

type ThreadMatcher = std::sync::Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync>;

/// Renders the comments + pending suggestions (and the inline composer
/// when its location matches) for one `(root, relative_path, anchor)`
/// thread region. Only emits visible DOM when there's something to show.
#[component]
fn ThreadRegion(
    ctx: ReviewCtx,
    root: protocol::ProjectRootPath,
    relative_path: String,
    matcher: ThreadMatcher,
) -> impl IntoView {
    let state = ctx.state.clone();
    let key = ctx.review_ref();
    let composer = ctx.composer;

    let comments_root = root.clone();
    let comments_path = relative_path.clone();
    let comments_matcher = matcher.clone();
    let comments_state = state.clone();
    let comments_key = key.clone();
    let comments: Memo<Vec<ReviewComment>> = Memo::new(move |_| {
        comments_state.reviews.with(|map| {
            map.get(&comments_key)
                .map(|review| {
                    review
                        .comments
                        .iter()
                        .filter(|c| {
                            c.location.root == comments_root
                                && c.location.relative_path == comments_path
                                && comments_matcher(&c.location.anchor)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    });

    let sg_root = root.clone();
    let sg_path = relative_path.clone();
    let sg_matcher = matcher.clone();
    let sg_state = state.clone();
    let sg_key = key.clone();
    let suggestions: Memo<Vec<ReviewSuggestedComment>> = Memo::new(move |_| {
        sg_state.reviews.with(|map| {
            map.get(&sg_key)
                .map(|review| {
                    review
                        .suggestions
                        .iter()
                        .filter(|s| {
                            matches!(s.state, protocol::ReviewSuggestionState::Pending)
                                && s.location.root == sg_root
                                && s.location.relative_path == sg_path
                                && sg_matcher(&s.location.anchor)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    });

    let composer_root = root.clone();
    let composer_path = relative_path.clone();
    let composer_matcher = matcher.clone();
    let composer_open = Memo::new(move |_| {
        composer.with(|c| {
            c.as_ref().is_some_and(|loc| {
                loc.root == composer_root
                    && loc.relative_path == composer_path
                    && composer_matcher(&loc.anchor)
            })
        })
    });

    let has_content =
        move || !comments.get().is_empty() || !suggestions.get().is_empty() || composer_open.get();

    let ctx_for_composer = ctx.clone();

    view! {
        {move || {
            if !has_content() {
                return view! { <span></span> }.into_any();
            }
            let ctx_accept = ctx_for_composer.clone();
            let ctx_composer = ctx_for_composer.clone();
            view! {
                <div class="project-diff-thread" data-mobile-test="diff-thread">
                    {comments.get().into_iter().map(|c| {
                        view! { <CommentRow comment=c /> }
                    }).collect::<Vec<_>>()}
                    {suggestions.get().into_iter().map(|s| {
                        view! { <SuggestionRow ctx=ctx_accept.clone() suggestion=s /> }
                    }).collect::<Vec<_>>()}
                    {move || {
                        if composer_open.get() {
                            view! { <Composer ctx=ctx_composer.clone() /> }.into_any()
                        } else {
                            view! { <span></span> }.into_any()
                        }
                    }}
                </div>
            }
            .into_any()
        }}
    }
}

#[component]
fn CommentRow(comment: ReviewComment) -> impl IntoView {
    let is_ai = matches!(
        comment.source,
        protocol::ReviewCommentSource::AiSuggestion { .. }
    );
    let source_label = if is_ai { "AI" } else { "You" };
    let stale = stale_reason(&comment.anchor_status);
    let body = comment.body.clone();
    view! {
        <div class="project-diff-comment" data-mobile-test="diff-comment">
            <div class="project-diff-comment-meta">
                <Pill
                    label=source_label
                    tone=if is_ai { PillTone::Accent } else { PillTone::Neutral }
                    data_mobile_test="diff-comment-source"
                />
                {stale_pill(stale)}
            </div>
            <div class="project-diff-comment-body" data-mobile-test="diff-comment-body">{body}</div>
        </div>
    }
}

#[component]
fn SuggestionRow(ctx: ReviewCtx, suggestion: ReviewSuggestedComment) -> impl IntoView {
    let severity = suggestion.severity;
    let severity_tone = match severity {
        protocol::ReviewSeverity::Info => PillTone::Neutral,
        protocol::ReviewSeverity::Warn => PillTone::Warning,
        protocol::ReviewSeverity::Bug => PillTone::Error,
    };
    let body = suggestion.body.clone();
    let rationale = suggestion.rationale.clone();
    let stale = stale_reason(&suggestion.anchor_status);

    let accept_id = suggestion.id.clone();
    let ctx_accept = ctx.clone();
    let on_accept = Callback::new(move |_: ()| {
        let ctx = ctx_accept.clone();
        let id = accept_id.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::AcceptSuggestion {
                    suggestion_id: id,
                    edit: None,
                },
            )
            .await
            {
                log::error!("accept suggestion failed: {e}");
            }
        });
    });

    let reject_id = suggestion.id.clone();
    let ctx_reject = ctx.clone();
    let on_reject = Callback::new(move |_: ()| {
        let ctx = ctx_reject.clone();
        let id = reject_id.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::RejectSuggestion { suggestion_id: id },
            )
            .await
            {
                log::error!("reject suggestion failed: {e}");
            }
        });
    });

    view! {
        <div class="project-diff-suggestion" data-mobile-test="diff-suggestion">
            <div class="project-diff-comment-meta">
                <Pill
                    label="Suggestion"
                    tone=PillTone::Accent
                    data_mobile_test="diff-suggestion-source"
                />
                <Pill
                    label=severity.label().to_string()
                    tone=severity_tone
                    data_mobile_test="diff-suggestion-severity"
                />
                {stale_pill(stale)}
            </div>
            <div class="project-diff-comment-body" data-mobile-test="diff-suggestion-body">{body}</div>
            {rationale.map(|r| view! {
                <div class="project-diff-suggestion-rationale">{r}</div>
            })}
            <div class="project-diff-suggestion-actions">
                <Button
                    label="Accept"
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Compact
                    data_mobile_test="diff-suggestion-accept"
                    on_click=on_accept
                />
                <Button
                    label="Reject"
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Compact
                    data_mobile_test="diff-suggestion-reject"
                    on_click=on_reject
                />
            </div>
        </div>
    }
}

#[component]
fn Composer(ctx: ReviewCtx) -> impl IntoView {
    let body = RwSignal::new(String::new());
    let composer = ctx.composer;

    let ctx_submit = ctx.clone();
    let on_submit = Callback::new(move |_: ()| {
        let text = body.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        let Some(location) = composer.get_untracked() else {
            return;
        };
        let ctx = ctx_submit.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::AddComment {
                    location,
                    body: text,
                },
            )
            .await
            {
                log::error!("add comment failed: {e}");
            }
        });
        composer.set(None);
        body.set(String::new());
    });

    let on_cancel = Callback::new(move |_: ()| {
        composer.set(None);
        body.set(String::new());
    });

    view! {
        <div class="project-diff-composer" data-mobile-test="diff-composer">
            <textarea
                class="project-diff-composer-input"
                data-mobile-test="diff-composer-input"
                placeholder="Add a comment…"
                prop:value=move || body.get()
                on:input=move |ev| body.set(event_target_value(&ev))
            ></textarea>
            <div class="project-diff-composer-actions">
                <Button
                    label="Comment"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="diff-composer-submit"
                    on_click=on_submit
                />
                <Button
                    label="Cancel"
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Compact
                    data_mobile_test="diff-composer-cancel"
                    on_click=on_cancel
                />
            </div>
        </div>
    }
}

/// Review controls bar: counts + AI review + clear + submit (with a
/// target picker). Rendered above the diff body when review is enabled.
/// When there's no active review yet, exposes a "Start review" affordance.
#[component]
fn ReviewControls(project: ActiveProjectRef, ctx: Option<ReviewCtx>) -> impl IntoView {
    let Some(ctx) = ctx else {
        // No active review: offer to create one over the uncommitted diff.
        let project = project.clone();
        let on_start = Callback::new(move |_: ()| {
            let project = project.clone();
            spawn_local(async move {
                let project_ref = crate::state::ActiveProjectRef {
                    local_host_id: project.local_host_id.clone(),
                    project_id: project.project_id.clone(),
                };
                if let Err(e) = crate::actions::create_review(
                    &project_ref,
                    protocol::ReviewDiffSelection::AllUncommitted,
                )
                .await
                {
                    log::error!("create_review failed: {e}");
                }
            });
        });
        return view! {
            <div class="project-diff-review-controls" data-mobile-test="diff-review-controls">
                <Button
                    label="Start review"
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Compact
                    data_mobile_test="diff-review-start"
                    on_click=on_start
                />
            </div>
        }
        .into_any();
    };

    // Reactive counts from the live review projection.
    let counts_ctx = ctx.clone();
    let counts = Memo::new(move |_| {
        counts_ctx
            .review()
            .map(|r| {
                let comments = r
                    .comments
                    .iter()
                    .filter(|c| matches!(c.source, protocol::ReviewCommentSource::User))
                    .count();
                let pending = r
                    .suggestions
                    .iter()
                    .filter(|s| matches!(s.state, protocol::ReviewSuggestionState::Pending))
                    .count();
                (comments, pending)
            })
            .unwrap_or((0, 0))
    });

    // Whether the AI reviewer is currently running.
    let ai_ctx = ctx.clone();
    let ai_running = Memo::new(move |_| {
        ai_ctx
            .review()
            .map(|r| {
                matches!(
                    r.ai_reviewer.status,
                    protocol::ReviewAiReviewerStatus::Running
                )
            })
            .unwrap_or(false)
    });

    // Submit target picker open state.
    let picker_open = RwSignal::new(false);

    // AI review: spawn a Codex reviewer (sensible default backend).
    let ctx_ai = ctx.clone();
    let on_ai = Callback::new(move |_: ()| {
        let ctx = ctx_ai.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::StartAiReview {
                    backend_kind: protocol::BackendKind::Codex,
                    cost_hint: None,
                    instructions: None,
                },
            )
            .await
            {
                log::error!("start ai review failed: {e}");
            }
        });
    });

    // Clear: drop all comments/suggestions/AI without delivering.
    let ctx_clear = ctx.clone();
    let on_clear = Callback::new(move |_: ()| {
        let ctx = ctx_clear.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::ClearComments,
            )
            .await
            {
                log::error!("clear comments failed: {e}");
            }
        });
    });

    let toggle_picker = Callback::new(move |_: ()| {
        picker_open.update(|o| *o = !*o);
    });

    let project_for_picker = project.clone();
    let ctx_for_picker = ctx.clone();

    view! {
        <div class="project-diff-review-controls" data-mobile-test="diff-review-controls">
            <div class="project-diff-review-counts" data-mobile-test="diff-review-counts">
                {move || {
                    let (comments, pending) = counts.get();
                    view! {
                        <Pill
                            label=format!("{comments} comment{}", if comments == 1 { "" } else { "s" })
                            tone=PillTone::Neutral
                            data_mobile_test="diff-review-comment-count"
                        />
                        <Pill
                            label=format!("{pending} suggestion{}", if pending == 1 { "" } else { "s" })
                            tone=PillTone::Accent
                            data_mobile_test="diff-review-suggestion-count"
                        />
                    }
                }}
            </div>
            <div class="project-diff-review-actions">
                {move || {
                    let label = if ai_running.get() { "AI reviewing…" } else { "AI review" };
                    view! {
                        <Button
                            label=label
                            variant=ButtonVariant::Ghost
                            size=ButtonSize::Compact
                            disabled=ai_running.get()
                            data_mobile_test="diff-review-ai"
                            on_click=on_ai
                        />
                    }
                }}
                <Button
                    label="Clear"
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Compact
                    data_mobile_test="diff-review-clear"
                    on_click=on_clear
                />
                <Button
                    label="Submit"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="diff-review-submit"
                    on_click=toggle_picker
                />
            </div>
            {move || {
                if !picker_open.get() {
                    return view! { <span></span> }.into_any();
                }
                view! {
                    <SubmitTargetPicker
                        project=project_for_picker.clone()
                        ctx=ctx_for_picker.clone()
                        on_done=Callback::new(move |_: ()| picker_open.set(false))
                    />
                }
                .into_any()
            }}
        </div>
    }
    .into_any()
}

/// Lists same-project agents and a "new agent" fallback so the user can
/// choose where the submitted review bundle is delivered.
#[component]
fn SubmitTargetPicker(
    project: ActiveProjectRef,
    ctx: ReviewCtx,
    on_done: Callback<()>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    // Optional instructions for a freshly-spawned target agent (part of the
    // approved submit-to-new-agent UX). Only sent with a NewAgent target.
    let new_instructions = RwSignal::new(String::new());

    let project_id = project.project_id.clone();
    let host = project.local_host_id.clone();
    let candidates = Memo::new(move |_| {
        state.agents.with(|agents| {
            agents
                .iter()
                .filter(|a| {
                    a.local_host_id == host
                        && a.project_id.as_ref() == Some(&project_id)
                        && a.fatal_error.is_none()
                })
                .map(|a| (a.agent_id.clone(), a.name.clone()))
                .collect::<Vec<_>>()
        })
    });

    let submit = move |target: protocol::ReviewSubmitTarget, ctx: ReviewCtx| {
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::Submit { target },
            )
            .await
            {
                log::error!("submit review failed: {e}");
            }
        });
    };

    let ctx_new = ctx.clone();
    let submit_new = submit;
    let on_new_agent = Callback::new(move |_: ()| {
        let text = new_instructions.get_untracked();
        let trimmed = text.trim();
        let instructions = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        submit_new(
            protocol::ReviewSubmitTarget::NewAgent {
                backend_kind: protocol::BackendKind::Codex,
                cost_hint: None,
                custom_agent_id: None,
                name: None,
                instructions,
            },
            ctx_new.clone(),
        );
        on_done.run(());
    });

    view! {
        <div class="project-diff-submit-picker" data-mobile-test="diff-submit-picker">
            <div class="project-diff-submit-picker-title">"Deliver review to…"</div>
            <div class="project-diff-submit-picker-list" data-mobile-test="diff-submit-picker-list">
                {move || {
                    let items = candidates.get();
                    if items.is_empty() {
                        return view! {
                            <div
                                class="project-diff-submit-picker-empty"
                                data-mobile-test="diff-submit-picker-empty"
                            >"No same-project agents running."</div>
                        }
                        .into_any();
                    }
                    let ctx_rows = ctx.clone();
                    items.into_iter().map(move |(agent_id, name)| {
                        let ctx = ctx_rows.clone();
                        let agent_id_for_click = agent_id.clone();
                        let on_pick = Callback::new(move |_: ()| {
                            submit(
                                protocol::ReviewSubmitTarget::ExistingAgent {
                                    agent_id: agent_id_for_click.clone(),
                                },
                                ctx.clone(),
                            );
                            on_done.run(());
                        });
                        let label = if name.is_empty() { agent_id.0.clone() } else { name };
                        view! {
                            <Button
                                label=label
                                variant=ButtonVariant::Secondary
                                size=ButtonSize::Compact
                                full_width=true
                                data_mobile_test="diff-submit-target-agent"
                                on_click=on_pick
                            />
                        }
                    }).collect::<Vec<_>>().into_any()
                }}
            </div>
            <textarea
                class="project-diff-submit-instructions"
                data-mobile-test="diff-submit-instructions"
                placeholder="Optional instructions for the new agent…"
                prop:value=move || new_instructions.get()
                on:input=move |ev| new_instructions.set(event_target_value(&ev))
            />
            <Button
                label="New agent (Codex)"
                variant=ButtonVariant::Ghost
                size=ButtonSize::Compact
                full_width=true
                data_mobile_test="diff-submit-target-new"
                on_click=on_new_agent
            />
        </div>
    }
}

fn stale_reason(status: &ReviewAnchorStatus) -> Option<String> {
    match status {
        ReviewAnchorStatus::Current => None,
        ReviewAnchorStatus::Stale { reason } => Some(reason.clone()),
    }
}

fn stale_pill(reason: Option<String>) -> AnyView {
    match reason {
        None => view! { <span></span> }.into_any(),
        Some(reason) => view! {
            <Pill
                label=format!("stale · {reason}")
                tone=PillTone::Warning
                data_mobile_test="diff-anchor-stale"
            />
        }
        .into_any(),
    }
}

fn short_root(root: &str) -> String {
    root.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(root)
        .to_owned()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId, ProjectDiffState};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, DiffContextMode, ProjectDiffScope, ProjectGitDiffFile,
        ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectId, ProjectRootPath,
        Review, ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewCommentId,
        ReviewCommentSource, ReviewDiffSelection, ReviewId, ReviewLocation, ReviewStatus,
        ReviewSummary, SessionId, StreamPath,
    };
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

    fn make_project(host: &LocalHostId, project: &str) -> ActiveProjectRef {
        ActiveProjectRef {
            local_host_id: host.clone(),
            project_id: ProjectId(project.to_owned()),
        }
    }

    fn fixture_diff_state() -> ProjectDiffState {
        ProjectDiffState {
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: "src/main.rs".to_owned(),
                hunks: vec![ProjectGitDiffHunk {
                    hunk_id: "h-1".to_owned(),
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 3,
                    lines: vec![
                        ProjectGitDiffLine {
                            kind: ProjectGitDiffLineKind::Context,
                            text: "fn main() {".to_owned(),
                            old_line_number: Some(1),
                            new_line_number: Some(1),
                        },
                        ProjectGitDiffLine {
                            kind: ProjectGitDiffLineKind::Removed,
                            text: "    println!(\"old\");".to_owned(),
                            old_line_number: Some(2),
                            new_line_number: None,
                        },
                        ProjectGitDiffLine {
                            kind: ProjectGitDiffLineKind::Added,
                            text: "    println!(\"new\");".to_owned(),
                            old_line_number: None,
                            new_line_number: Some(2),
                        },
                    ],
                }],
            }],
        }
    }

    fn fixture_review(review_id: &ReviewId, project: &str) -> Review {
        Review {
            id: review_id.clone(),
            project_id: ProjectId(project.to_owned()),
            origin_agent_id: AgentId("a1".to_owned()),
            origin_session_id: SessionId("s1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: Vec::new(),
            comments: vec![ReviewComment {
                id: ReviewCommentId("c1".to_owned()),
                location: ReviewLocation {
                    root: ProjectRootPath("/x".to_owned()),
                    relative_path: "src/main.rs".to_owned(),
                    // Anchored to the added line (new line number 2).
                    anchor: ReviewAnchor::LineRange {
                        side: ReviewDiffSide::New,
                        start_line: 2,
                        end_line: 2,
                    },
                },
                anchor_status: ReviewAnchorStatus::Current,
                body: "needs a test".to_owned(),
                source: ReviewCommentSource::User,
                created_at_ms: 0,
                updated_at_ms: 0,
            }],
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn fixture_summary(review_id: &ReviewId) -> ReviewSummary {
        ReviewSummary {
            id: review_id.clone(),
            status: ReviewStatus::Draft,
            origin_session_id: SessionId("s1".to_owned()),
            origin_agent_id: AgentId("a1".to_owned()),
            created_at_ms: 0,
            updated_at_ms: 1,
            user_comment_count: 1,
            pending_suggestion_count: 0,
        }
    }

    fn fixture_agent(
        host: &LocalHostId,
        project: &str,
        id: &str,
        name: &str,
    ) -> crate::state::AgentInfo {
        crate::state::AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            workspace_roots: vec!["/x".to_owned()],
            project_id: Some(ProjectId(project.to_owned())),
            parent_agent_id: None,
            session_id: Some(SessionId("s1".to_owned())),
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath("/agent/a".to_owned()),
            started: true,
            fatal_error: None,
        }
    }

    /// No cached entry → loading spinner.
    #[wasm_bindgen_test]
    async fn diff_viewer_shows_loading_initially() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let project_for_mount = project.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Uncommitted
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-diff-viewer-loading']")
                .unwrap()
                .is_some(),
            "loading must render before contents arrive"
        );
    }

    /// Files with added/removed/context lines render distinct selectors.
    #[wasm_bindgen_test]
    async fn diff_viewer_renders_lines_with_distinct_selectors() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
        };
        let project_for_mount = project.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(key.clone(), fixture_diff_state());
            });
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Uncommitted
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='diff-line-added']")
                .unwrap()
                .is_some(),
            "added line selector must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-line-removed']")
                .unwrap()
                .is_some(),
            "removed line selector must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-line-context']")
                .unwrap()
                .is_some(),
            "context line selector must render"
        );
        let stats = container
            .query_selector("[data-mobile-test='project-diff-stat-added']")
            .unwrap()
            .unwrap();
        assert!(
            stats.text_content().unwrap_or_default().contains("+1"),
            "added line count must surface"
        );
    }

    /// Empty file list with `pending: false` shows the "No changes" empty state.
    #[wasm_bindgen_test]
    async fn diff_viewer_shows_empty_state_when_no_changes() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
        };
        let empty = ProjectDiffState {
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: Vec::new(),
        };
        let project_for_mount = project.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(key.clone(), empty.clone());
            });
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Uncommitted
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-diff-viewer-empty']")
                .unwrap()
                .is_some(),
            "empty state must render when no changes"
        );
    }

    /// A review comment anchored to a diff line renders inline under that
    /// line's wrapper. We assert the comment body text is present and that
    /// the thread sits inside the same `.project-diff-line-wrap` as the
    /// added line it is anchored to.
    #[wasm_bindgen_test]
    async fn inline_comment_renders_under_anchored_line() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let review_id = ReviewId("rev-1".to_owned());
        let diff_key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
        };
        let review_key = ReviewRef {
            local_host_id: host.clone(),
            review_id: review_id.clone(),
        };
        let summary_key = (host.clone(), ProjectId("p-1".to_owned()));
        let project_for_mount = project.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(diff_key.clone(), fixture_diff_state());
            });
            state.review_summaries.update(|m| {
                m.insert(summary_key.clone(), vec![fixture_summary(&review_id)]);
            });
            state.reviews.update(|m| {
                m.insert(review_key.clone(), fixture_review(&review_id, "p-1"));
            });
            // Pre-register the stream so the subscribe Effect short-circuits
            // (no Tauri bridge in headless Chrome).
            state.review_streams.update(|m| {
                m.insert(review_key.clone(), StreamPath("/review/rev-1".to_owned()));
            });
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Uncommitted
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        let comment = container
            .query_selector("[data-mobile-test='diff-comment-body']")
            .unwrap()
            .expect("inline comment body must render");
        assert!(
            comment
                .text_content()
                .unwrap_or_default()
                .contains("needs a test"),
            "comment body text must surface inline"
        );

        // The comment must live inside the added line's wrapper, not the
        // removed/context line. The added line (new line 2) is the anchor.
        let added = container
            .query_selector("[data-mobile-test='diff-line-added']")
            .unwrap()
            .expect("added line must render");
        let wrap = added
            .closest(".project-diff-line-wrap")
            .unwrap()
            .expect("added line must be wrapped");
        assert!(
            wrap.query_selector("[data-mobile-test='diff-comment-body']")
                .unwrap()
                .is_some(),
            "comment must render inside the anchored line's wrapper"
        );
    }

    /// Submit controls render and the target picker lists same-project agents.
    #[wasm_bindgen_test]
    async fn submit_picker_lists_same_project_agents() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let review_id = ReviewId("rev-1".to_owned());
        let diff_key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
        };
        let review_key = ReviewRef {
            local_host_id: host.clone(),
            review_id: review_id.clone(),
        };
        let summary_key = (host.clone(), ProjectId("p-1".to_owned()));
        let host_for_agents = host.clone();
        let project_for_mount = project.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(diff_key.clone(), fixture_diff_state());
            });
            state.review_summaries.update(|m| {
                m.insert(summary_key.clone(), vec![fixture_summary(&review_id)]);
            });
            state.reviews.update(|m| {
                m.insert(review_key.clone(), fixture_review(&review_id, "p-1"));
            });
            state.review_streams.update(|m| {
                m.insert(review_key.clone(), StreamPath("/review/rev-1".to_owned()));
            });
            state.agents.update(|a| {
                // Same project — should appear.
                a.push(fixture_agent(
                    &host_for_agents,
                    "p-1",
                    "agent-same",
                    "Same Proj",
                ));
                // Different project — must NOT appear.
                a.push(fixture_agent(
                    &host_for_agents,
                    "p-2",
                    "agent-other",
                    "Other Proj",
                ));
            });
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Uncommitted
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        // Submit control must render.
        let submit = container
            .query_selector("[data-mobile-test='diff-review-submit']")
            .unwrap()
            .expect("submit control must render");
        // Open the picker.
        submit.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;

        let picker = container
            .query_selector("[data-mobile-test='diff-submit-picker']")
            .unwrap()
            .expect("submit target picker must open");
        let agent_rows = picker
            .query_selector_all("[data-mobile-test='diff-submit-target-agent']")
            .unwrap();
        assert_eq!(
            agent_rows.length(),
            1,
            "picker must list exactly the one same-project agent"
        );
        let row = agent_rows.item(0).unwrap();
        let row_el = row.dyn_ref::<web_sys::Element>().unwrap();
        assert!(
            row_el
                .text_content()
                .unwrap_or_default()
                .contains("Same Proj"),
            "picker row must show the same-project agent name"
        );
        // The new-agent fallback must also be present.
        assert!(
            picker
                .query_selector("[data-mobile-test='diff-submit-target-new']")
                .unwrap()
                .is_some(),
            "new-agent fallback must be offered"
        );
    }
}
