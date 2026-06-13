use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, EmptyState, Pill, PillTone, Spinner,
};
use crate::state::{ActiveProjectRef, AppState, ProjectDiffRef, ReviewRef};
use protocol::{
    ReviewAnchor, ReviewAnchorStatus, ReviewComment, ReviewCommentId, ReviewCommentSource,
    ReviewDiffSide, ReviewLocation, ReviewSuggestedComment,
};

/// Inline composer state for the mobile review overlay. Mirrors the desktop
/// `ComposerState`: the body text and a post-save baseline live on this
/// parent-owned, persistent value (not on the `Composer` component, which the
/// thread subtree remounts whenever the review's comment list changes). That
/// persistence is what makes confirmed-echo close remount-safe and keeps the
/// user's text intact on send failure.
#[derive(Clone, Debug)]
struct MobileComposerState {
    location: ReviewLocation,
    body: RwSignal<String>,
    /// Snapshot of the User-comment ids already at `location` when a save is
    /// dispatched. `None` until the first save. The composer closes (and the
    /// text clears) only once a User comment at `location` appears whose id is
    /// not in this snapshot — i.e. the server `CommentUpsert` echo landed.
    submitted_baseline: RwSignal<Option<Vec<ReviewCommentId>>>,
    /// Set while an `AddComment` send is in flight. Disables the Comment button
    /// so a double-tap before the echo can't dispatch duplicate comments.
    /// Cleared on send failure (to allow retry); on success the composer closes
    /// from the echo effect, discarding this state.
    submitting: RwSignal<bool>,
}

impl MobileComposerState {
    fn open(location: ReviewLocation) -> Self {
        Self {
            location,
            body: RwSignal::new(String::new()),
            submitted_baseline: RwSignal::new(None),
            submitting: RwSignal::new(false),
        }
    }
}

/// Unified git diff viewer for an active project's root.
///
/// On mount, fires `ProjectReadDiff` with the requested scope/path.
/// Renders the diff inline below the file tree (Projects view) or
/// inside a review-detail diff tab.
///
/// When `scope == Unstaged` this is also the primary inline-review
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

    // The inline composer, if open (`None` ⇒ closed). The state carries the
    // anchor location plus persistent body/baseline so confirmed-echo close
    // survives the thread subtree's remounts.
    let composer: RwSignal<Option<MobileComposerState>> = RwSignal::new(None);

    let key = ProjectDiffRef {
        local_host_id: project.local_host_id.clone(),
        project_id: project.project_id.clone(),
        root: root.clone(),
        scope,
        path: path.clone(),
    };

    // Whether inline review is enabled for this surface. Active reviews are
    // anchored server-side to `Unstaged` (index↔worktree), so only that scope
    // overlays comments — a `Staged` or `Uncommitted` surface would mismatch
    // the review's anchors.
    let review_enabled = scope == protocol::ProjectDiffScope::Unstaged;

    // Resolve the active review id for this project. The inline diff surface
    // is only for the *editable* review, so it binds to a Draft only —
    // Submitted/Consumed/Cancelled reviews are terminal and must not overlay
    // the live unstaged diff or expose comment/AI/Clear/Submit controls
    // (matching the desktop integrated diff surface). Reactive over
    // `review_summaries`.
    let project_for_review = project.clone();
    let active_review_id = Memo::new(move |_| {
        if !review_enabled {
            return None;
        }
        let key = (
            project_for_review.local_host_id.clone(),
            project_for_review.project_id.clone(),
        );
        // One active review per project spans all roots; bind to the single
        // workspace draft (this diff tab renders its own root's slice — its
        // comment decorations filter by `ReviewLocation.root`). Legacy
        // root-scoped summaries are never active and are ignored.
        let id = state.review_summaries.with(|map| {
            map.get(&key)?
                .iter()
                .filter(|s| {
                    matches!(s.status, protocol::ReviewStatus::Draft)
                        && matches!(s.scope, protocol::ReviewSummaryScope::Workspace)
                })
                .max_by_key(|s| s.updated_at_ms)
                .map(|s| s.id.clone())
        })?;
        // A live `StatusChanged` updates `state.reviews` before
        // `review_summaries` refreshes. If we already hold the full record,
        // require *it* to still be Draft so the inline surface drops a
        // just-submitted/consumed review immediately rather than trusting a
        // stale Draft summary. (Record absent ⇒ keep the id so the first
        // subscribe can fetch it.)
        let rkey = ReviewRef {
            local_host_id: key.0.clone(),
            review_id: id.clone(),
        };
        let live_non_draft = state.reviews.with(|r| {
            r.get(&rkey)
                .map(|rev| !matches!(rev.status, protocol::ReviewStatus::Draft))
                .unwrap_or(false)
        });
        if live_non_draft {
            return None;
        }
        Some(id)
    });

    // Subscribe to the active review when one appears so the overlay has
    // a live `Review` projection.
    {
        let project = project.clone();
        let state = state.clone();
        // Guards against duplicate concurrent subscribes when the effect
        // re-runs while one is still in flight.
        let in_flight: StoredValue<bool, LocalStorage> = StoredValue::new_local(false);
        Effect::new(move |_| {
            let Some(review_id) = active_review_id.get() else {
                return;
            };
            let host = project.local_host_id.clone();
            let rkey = ReviewRef {
                local_host_id: host.clone(),
                review_id: review_id.clone(),
            };
            // Tracked reads: a record/stream arrival or a (re)connection
            // re-runs this effect, so a previously-failed subscribe can
            // retry. `subscribe_review` only records the stream after the
            // send succeeds, so a failed send does NOT latch "already known".
            let has_review = state.reviews.with(|r| r.contains_key(&rkey));
            let has_stream = state.review_streams.with(|s| s.contains_key(&rkey));
            if has_review || has_stream {
                return;
            }
            let connected = state
                .host_streams
                .with(|streams| streams.contains_key(&host));
            if !connected {
                return;
            }
            if in_flight.get_value() {
                return;
            }
            in_flight.set_value(true);
            let state = state.clone();
            spawn_local(async move {
                if let Err(e) = crate::actions::subscribe_review(&state, &host, review_id).await {
                    log::error!("subscribe_review failed: {e}");
                }
                in_flight.set_value(false);
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
    composer: RwSignal<Option<MobileComposerState>>,
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

    /// Effective backend for review actions on this host: the host's
    /// `default_backend`, else its first enabled backend. `None` only when
    /// the host has no enabled backends. Mobile has no explicit reviewer
    /// backend picker, so this is the whole precedence.
    fn effective_backend(&self) -> Option<protocol::BackendKind> {
        self.state.host_settings_by_host.with_untracked(|m| {
            m.get(&self.host).and_then(|s| {
                s.default_backend
                    .or_else(|| s.enabled_backends.first().copied())
            })
        })
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

    // Binary / no-hunk files have no lines to anchor line comments to —
    // render a clear placeholder and skip the hunk list. The file-level
    // comment affordance + thread region above still work.
    let is_binary = file.is_binary;
    let show_placeholder = is_binary || file.hunks.is_empty();
    let placeholder_text = if is_binary {
        "Binary file changed"
    } else {
        "No textual changes"
    };

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
                    <Button
                        label="+ Comment"
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Compact
                        class="project-diff-file-comment-btn"
                        data_mobile_test="diff-file-comment-btn"
                        aria_label="Comment on file".to_string()
                        on_click=Callback::new(move |_: ()| {
                            composer.set(Some(MobileComposerState::open(ReviewLocation {
                                root: click_root.clone(),
                                relative_path: click_path.clone(),
                                anchor: ReviewAnchor::File,
                            })));
                        })
                    />
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
            {if show_placeholder {
                view! {
                    <div
                        class="project-diff-binary-placeholder"
                        data-mobile-test="diff-binary-placeholder"
                    >
                        {placeholder_text}
                    </div>
                }.into_any()
            } else {
                view! {
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
                }.into_any()
            }}
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
            ctx.composer
                .set(Some(MobileComposerState::open(ReviewLocation {
                    root: root.clone(),
                    relative_path: relative_path.clone(),
                    anchor: ReviewAnchor::LineRange {
                        side,
                        start_line: line_no,
                        end_line: line_no,
                    },
                })));
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
            c.as_ref().is_some_and(|cs| {
                cs.location.root == composer_root
                    && cs.location.relative_path == composer_path
                    && composer_matcher(&cs.location.anchor)
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
    let composer = ctx.composer;
    // Body + baseline live on the persistent composer state, not here: this
    // component remounts whenever the thread's comment list changes.
    let Some(state) = composer.get_untracked() else {
        return view! { <span></span> }.into_any();
    };
    let body = state.body;
    let submitted_baseline = state.submitted_baseline;
    let submitting = state.submitting;
    let location = state.location.clone();

    // Level-triggered confirmed-echo close. Once a save is dispatched,
    // `submitted_baseline` holds the User-comment ids that already existed at
    // `location`. When a User comment at `location` appears whose id is not in
    // that snapshot — the server `CommentUpsert` echo — close the composer.
    // Tracks `submitted_baseline` and `reviews`, so it re-fires on the echo
    // regardless of ordering and survives a mid-flight remount (it re-reads
    // the persistent baseline). On send failure the gate never matches, so the
    // composer stays open with its text intact.
    {
        let echo_ctx = ctx.clone();
        let echo_location = location.clone();
        Effect::new(move |_| {
            let Some(baseline) = submitted_baseline.get() else {
                return;
            };
            let key = echo_ctx.review_ref();
            let confirmed = echo_ctx.state.reviews.with(|map| {
                map.get(&key)
                    .map(|review| {
                        review.comments.iter().any(|c| {
                            matches!(c.source, ReviewCommentSource::User)
                                && c.location == echo_location
                                && !baseline.contains(&c.id)
                        })
                    })
                    .unwrap_or(false)
            });
            if confirmed {
                composer.set(None);
            }
        });
    }

    let ctx_submit = ctx.clone();
    let submit_location = location.clone();
    let on_submit = Callback::new(move |_: ()| {
        // Pending gate: ignore taps while a send is in flight so a double-tap
        // before the echo can't dispatch duplicate AddComment actions.
        if submitting.get_untracked() {
            return;
        }
        let text = body.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        let ctx = ctx_submit.clone();
        let location = submit_location.clone();
        // Snapshot the User comments already at this location so the echo
        // effect can recognize the newly added one. Do NOT close optimistically
        // — the composer closes from the effect once the echo lands.
        let key = ctx.review_ref();
        let baseline: Vec<ReviewCommentId> = ctx.state.reviews.with_untracked(|map| {
            map.get(&key)
                .map(|review| {
                    review
                        .comments
                        .iter()
                        .filter(|c| {
                            matches!(c.source, ReviewCommentSource::User) && c.location == location
                        })
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default()
        });
        submitted_baseline.set(Some(baseline));
        submitting.set(true);
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
                // Re-enable the button so the user can retry. On success the
                // composer closes from the echo effect, discarding this state.
                submitting.set(false);
            }
        });
    });

    let on_cancel = Callback::new(move |_: ()| {
        composer.set(None);
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
                {move || {
                    // Re-rendered when `submitting` flips so the disabled state
                    // (and the Button's own on_click guard) tracks it.
                    let pending = submitting.get();
                    view! {
                        <Button
                            label="Comment"
                            variant=ButtonVariant::Primary
                            size=ButtonSize::Compact
                            data_mobile_test="diff-composer-submit"
                            disabled=pending
                            on_click=on_submit
                        />
                    }
                }}
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
    .into_any()
}

/// Review controls bar: counts + AI review + clear + submit (with a
/// target picker). Rendered above the diff body when review is enabled.
/// Reviews are always-on server-side; the client simply waits for the
/// Draft summary to arrive. When no active review is present yet, render
/// nothing — no "Start review" button.
#[component]
fn ReviewControls(project: ActiveProjectRef, ctx: Option<ReviewCtx>) -> impl IntoView {
    let Some(ctx) = ctx else {
        // No active Draft review visible yet. The server manages review
        // lifecycle automatically (always-on). Render a silent placeholder
        // so tests can assert the absence of controls.
        return view! {
            <span data-mobile-test="diff-review-controls-none"></span>
        }
        .into_any();
    };

    // Reactive counts from the live review projection.
    let counts_ctx = ctx.clone();
    let counts = Memo::new(move |_| {
        counts_ctx
            .review()
            .map(|r| {
                // Count every comment record — human comments AND accepted AI
                // comments (the server promotes accepted suggestions into
                // `comments`) — matching desktop/server semantics. Pending AI
                // suggestions stay a separate count.
                let comments = r.comments.len();
                let pending = r
                    .suggestions
                    .iter()
                    .filter(|s| matches!(s.state, protocol::ReviewSuggestionState::Pending))
                    .count();
                (comments, pending)
            })
            .unwrap_or((0, 0))
    });

    // Whether the host has any backend the AI reviewer could run on (default
    // or first enabled). When false the AI review button is disabled, matching
    // desktop — there's nothing to run.
    let backend_ctx = ctx.clone();
    let ai_backend_available = Memo::new(move |_| {
        backend_ctx.state.host_settings_by_host.with(|m| {
            m.get(&backend_ctx.host)
                .map(|s| s.default_backend.is_some() || !s.enabled_backends.is_empty())
                .unwrap_or(false)
        })
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

    // AI review: the host resolves the backend. Mobile has no reviewer
    // backend picker, so always send `None` ⇒ server uses its
    // `default_backend` (else first enabled). Gate on there being some
    // runnable backend so we don't fire a request the host can't satisfy.
    let ctx_ai = ctx.clone();
    let on_ai = Callback::new(move |_: ()| {
        let ctx = ctx_ai.clone();
        spawn_local(async move {
            if ctx.effective_backend().is_none() {
                log::error!("start ai review skipped: host has no enabled backend");
                return;
            }
            if let Err(e) = crate::actions::send_review_action(
                &ctx.state,
                &ctx.host,
                ctx.review_id.clone(),
                protocol::ReviewActionPayload::StartAiReview {
                    backend_kind: None,
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
                            disabled=ai_running.get() || !ai_backend_available.get()
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
        let Some(backend_kind) = ctx_new.effective_backend() else {
            log::error!("submit to new agent skipped: host has no enabled backend");
            return;
        };
        submit_new(
            protocol::ReviewSubmitTarget::NewAgent {
                backend_kind,
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
        AgentId, AgentOrigin, BackendKind, DiffContextMode, HostSettings, ProjectDiffScope,
        ProjectGitDiffFile, ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind,
        ProjectId, ProjectRootPath, Review, ReviewAiReviewerState, ReviewAiReviewerStatus,
        ReviewCommentId, ReviewCommentSource, ReviewDiffSelection, ReviewId, ReviewLocation,
        ReviewStatus, ReviewSummary, SessionId, StreamPath,
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
            scope: ProjectDiffScope::Unstaged,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: "src/main.rs".to_owned(),
                is_binary: false,
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
            scope: protocol::ReviewSummaryScope::Workspace,
            status: ReviewStatus::Draft,
            origin_session_id: SessionId("s1".to_owned()),
            origin_agent_id: AgentId("a1".to_owned()),
            created_at_ms: 0,
            updated_at_ms: 1,
            user_comment_count: 1,
            pending_suggestion_count: 0,
            file_comment_counts: vec![],
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
                    scope=ProjectDiffScope::Unstaged
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
            scope: ProjectDiffScope::Unstaged,
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
                    scope=ProjectDiffScope::Unstaged
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
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let empty = ProjectDiffState {
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
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
                    scope=ProjectDiffScope::Unstaged
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
            scope: ProjectDiffScope::Unstaged,
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
                    scope=ProjectDiffScope::Unstaged
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
            scope: ProjectDiffScope::Unstaged,
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
                    scope=ProjectDiffScope::Unstaged
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

    fn fixture_binary_diff_state() -> ProjectDiffState {
        ProjectDiffState {
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: "assets/logo.png".to_owned(),
                is_binary: true,
                hunks: vec![],
            }],
        }
    }

    /// A binary file renders the placeholder and no diff lines, while still
    /// exposing the file-level comment affordance when a review is active.
    #[wasm_bindgen_test]
    async fn diff_viewer_binary_file_shows_placeholder() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let review_id = ReviewId("rev-1".to_owned());
        let key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let project_for_mount = project.clone();
        let review_for_mount = review_id.clone();
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(key.clone(), fixture_binary_diff_state());
            });
            // A draft review so the file-level comment affordance shows.
            state.review_summaries.update(|m| {
                m.insert(
                    (host_for_mount.clone(), ProjectId("p-1".to_owned())),
                    vec![fixture_summary(&review_for_mount)],
                );
            });
            state.reviews.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host_for_mount.clone(),
                        review_id: review_for_mount.clone(),
                    },
                    fixture_review(&review_for_mount, "p-1"),
                );
            });
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Unstaged
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='diff-binary-placeholder']")
                .unwrap()
                .is_some(),
            "binary file must render a placeholder"
        );
        assert_eq!(
            container
                .query_selector_all("[data-mobile-test='diff-line-added']")
                .unwrap()
                .length(),
            0,
            "binary file must not render diff lines"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-file-comment-btn']")
                .unwrap()
                .is_some(),
            "binary file must still expose a file-level comment affordance"
        );
    }

    /// Reviews are always-on: there is no "Start review" button. Before a
    /// Draft summary arrives, the controls area is empty (diff-review-controls-
    /// none). Once the summary arrives, the full review controls render.
    #[wasm_bindgen_test]
    async fn always_on_review_no_start_button() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let project_for_mount = project.clone();
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(key.clone(), fixture_diff_state());
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Unstaged
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        // No "Start review" button — reviews are always-on.
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-start']")
                .unwrap()
                .is_none(),
            "diff-review-start button must NOT exist (reviews are always-on)"
        );
        // Before a review arrives the placeholder is shown.
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-controls-none']")
                .unwrap()
                .is_some(),
            "diff-review-controls-none placeholder must be shown before a review arrives"
        );

        // The review projection arrives → the full controls appear.
        let state = holder.borrow().clone().unwrap();
        let review_id = ReviewId("rev-1".to_owned());
        state.review_summaries.update(|m| {
            m.insert(
                (host.clone(), ProjectId("p-1".to_owned())),
                vec![fixture_summary(&review_id)],
            );
        });
        state.reviews.update(|m| {
            m.insert(
                ReviewRef {
                    local_host_id: host.clone(),
                    review_id: review_id.clone(),
                },
                fixture_review(&review_id, "p-1"),
            );
        });
        // Pre-register the stream so the subscribe Effect short-circuits.
        state.review_streams.update(|m| {
            m.insert(
                ReviewRef {
                    local_host_id: host.clone(),
                    review_id: review_id.clone(),
                },
                StreamPath("/review/rev-1".to_owned()),
            );
        });
        next_tick().await;

        // Full review controls are now visible; no start button, no placeholder.
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-controls']")
                .unwrap()
                .is_some(),
            "full review controls must appear once the review summary arrives"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-start']")
                .unwrap()
                .is_none(),
            "diff-review-start must never exist"
        );
    }

    fn host_settings(default: Option<BackendKind>, enabled: Vec<BackendKind>) -> HostSettings {
        HostSettings {
            enabled_backends: enabled,
            default_backend: default,
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: false,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
        }
    }

    /// Mobile AI review no longer hard-codes Codex: `ReviewCtx::effective_backend`
    /// resolves the host `default_backend`, falling back to the first enabled
    /// backend, and is `None` only when no backend is enabled.
    #[wasm_bindgen_test]
    async fn ai_review_resolves_default_backend() {
        let container = make_container();
        let host = LocalHostId("host-1".to_owned());
        let observed: std::rc::Rc<std::cell::RefCell<Vec<Option<BackendKind>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed_for_mount = observed.clone();
        let host_for_mount = host.clone();
        let _h = mount_to(container, move || {
            let state = AppState::new();
            let ctx = ReviewCtx {
                host: host_for_mount.clone(),
                review_id: ReviewId("rev-1".to_owned()),
                composer: RwSignal::new(None),
                state: state.clone(),
            };

            // default_backend present ⇒ it wins over the first enabled backend.
            state.host_settings_by_host.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    host_settings(
                        Some(BackendKind::Antigravity),
                        vec![BackendKind::Codex, BackendKind::Antigravity],
                    ),
                );
            });
            observed_for_mount
                .borrow_mut()
                .push(ctx.effective_backend());

            // No default ⇒ first enabled backend.
            state.host_settings_by_host.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    host_settings(None, vec![BackendKind::Codex, BackendKind::Antigravity]),
                );
            });
            observed_for_mount
                .borrow_mut()
                .push(ctx.effective_backend());

            // No enabled backends ⇒ nothing to run.
            state.host_settings_by_host.update(|m| {
                m.insert(host_for_mount.clone(), host_settings(None, vec![]));
            });
            observed_for_mount
                .borrow_mut()
                .push(ctx.effective_backend());

            view! { <div></div> }
        });
        next_tick().await;

        let got = observed.borrow();
        assert_eq!(
            got.as_slice(),
            &[
                Some(BackendKind::Antigravity),
                Some(BackendKind::Codex),
                None
            ],
            "effective backend must be default → first-enabled → none"
        );
    }

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

    /// Mobile "AI review" delegates the backend to the server: it sends
    /// `StartAiReview.backend_kind = null` (no hard-coded Codex, no
    /// client-side default resolution).
    #[wasm_bindgen_test]
    async fn mobile_ai_review_sends_none_backend() {
        record_bridge();
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let project_for_mount = project.clone();
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(key.clone(), fixture_diff_state());
            });
            let review_id = ReviewId("rev-1".to_owned());
            state.review_summaries.update(|m| {
                m.insert(
                    (host_for_mount.clone(), ProjectId("p-1".to_owned())),
                    vec![fixture_summary(&review_id)],
                );
            });
            state.reviews.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host_for_mount.clone(),
                        review_id: review_id.clone(),
                    },
                    fixture_review(&review_id, "p-1"),
                );
            });
            // Pre-register the stream so the subscribe Effect short-circuits
            // (no incidental ReviewSubscribe frame).
            state.review_streams.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host_for_mount.clone(),
                        review_id: review_id.clone(),
                    },
                    StreamPath("/review/rev-1".to_owned()),
                );
            });
            // A backend exists so the zero-backend gate passes.
            state.host_settings_by_host.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    host_settings(Some(BackendKind::Codex), vec![BackendKind::Codex]),
                );
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Unstaged
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        next_tick().await;

        let ai_btn = container
            .query_selector("[data-mobile-test='diff-review-ai']")
            .unwrap()
            .expect("AI review button present");
        ai_btn.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;

        let sent = sent_lines_joined();
        assert!(
            sent.contains("start_ai_review"),
            "a StartAiReview frame must be sent; sent: {sent}"
        );
        // `backend_kind: None` is omitted on the wire (skip_serializing_if),
        // so mobile's default AI review carries no concrete backend.
        assert!(
            !sent.contains("\"backend_kind\""),
            "mobile AI review must omit backend_kind (server resolves the \
             default); sent: {sent}"
        );
    }

    /// Rejecting bridge: every `invoke` fails, so `send_review_action`
    /// returns an error (simulating a send failure).
    fn reject_bridge() {
        let _ = js_sys::eval(
            "(function(){ \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(){ return Promise.reject('boom'); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
    }

    fn review_without_comments(review_id: &ReviewId) -> Review {
        let mut r = fixture_review(review_id, "p-1");
        r.comments.clear();
        r
    }

    fn composer_at(line: u32) -> (ReviewLocation, MobileComposerState) {
        let location = ReviewLocation {
            root: ProjectRootPath("/x".to_owned()),
            relative_path: "src/main.rs".to_owned(),
            anchor: ReviewAnchor::LineRange {
                side: ReviewDiffSide::New,
                start_line: line,
                end_line: line,
            },
        };
        let state = MobileComposerState {
            location: location.clone(),
            body: RwSignal::new("needs a test".to_owned()),
            submitted_baseline: RwSignal::new(None),
            submitting: RwSignal::new(false),
        };
        (location, state)
    }

    /// Mounts a bare `Composer` over an empty Draft review and returns the
    /// (composer signal, app state, anchor location) so a test can drive the
    /// save and the confirmed echo.
    fn mount_composer(
        container: HtmlElement,
        line: u32,
    ) -> (
        RwSignal<Option<MobileComposerState>>,
        AppState,
        ReviewLocation,
    ) {
        let host = LocalHostId("host-1".to_owned());
        let review_id = ReviewId("rev-1".to_owned());
        let (location, composer_state) = composer_at(line);
        let holder: std::rc::Rc<
            std::cell::RefCell<Option<(RwSignal<Option<MobileComposerState>>, AppState)>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let host_for_mount = host.clone();
        let review_for_mount = review_id.clone();
        let _h = mount_to(container, move || {
            let state = AppState::new();
            state.reviews.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host_for_mount.clone(),
                        review_id: review_for_mount.clone(),
                    },
                    review_without_comments(&review_for_mount),
                );
            });
            let composer: RwSignal<Option<MobileComposerState>> =
                RwSignal::new(Some(composer_state.clone()));
            let ctx = ReviewCtx {
                host: host_for_mount.clone(),
                review_id: review_for_mount.clone(),
                composer,
                state: state.clone(),
            };
            *holder_for_mount.borrow_mut() = Some((composer, state.clone()));
            provide_context(state);
            view! { <Composer ctx=ctx /> }
        });
        std::mem::forget(_h);
        let (composer, state) = holder.borrow().clone().unwrap();
        (composer, state, location)
    }

    /// The mobile composer does NOT close optimistically on save: it stays
    /// open until the server's confirmed `CommentUpsert` echo (a new User
    /// comment at the anchor) lands.
    #[wasm_bindgen_test]
    async fn mobile_composer_closes_only_on_confirmed_echo() {
        record_bridge();
        let container = make_container();
        let (composer, state, location) = mount_composer(container.clone(), 2);

        next_tick().await;
        let submit = container
            .query_selector("[data-mobile-test='diff-composer-submit']")
            .unwrap()
            .expect("composer submit button present");
        submit.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;

        // Send succeeded at transport, but no echo yet ⇒ composer stays open.
        assert!(
            composer.get_untracked().is_some(),
            "composer must stay open until the confirmed echo lands"
        );

        // Server echo: a new User comment at the anchor appears.
        state.reviews.update(|m| {
            let key = ReviewRef {
                local_host_id: LocalHostId("host-1".to_owned()),
                review_id: ReviewId("rev-1".to_owned()),
            };
            if let Some(review) = m.get_mut(&key) {
                review.comments.push(ReviewComment {
                    id: ReviewCommentId("c-new".to_owned()),
                    location: location.clone(),
                    anchor_status: ReviewAnchorStatus::Current,
                    body: "needs a test".to_owned(),
                    source: ReviewCommentSource::User,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                });
            }
        });
        next_tick().await;

        assert!(
            composer.get_untracked().is_none(),
            "composer must close once the confirmed echo (new User comment) lands"
        );
    }

    /// On send failure the composer stays open and the typed text is retained
    /// (no optimistic close/clear).
    #[wasm_bindgen_test]
    async fn mobile_composer_retains_text_on_send_failure() {
        reject_bridge();
        let container = make_container();
        let (composer, _state, _location) = mount_composer(container.clone(), 2);

        next_tick().await;
        let submit = container
            .query_selector("[data-mobile-test='diff-composer-submit']")
            .unwrap()
            .expect("composer submit button present");
        submit.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;
        next_tick().await;

        let still_open = composer.get_untracked();
        assert!(
            still_open.is_some(),
            "composer must stay open when the send fails"
        );
        assert_eq!(
            still_open.unwrap().body.get_untracked(),
            "needs a test",
            "the typed text must be retained on send failure"
        );
    }

    /// A double-tap of Comment before the server echo must dispatch only one
    /// `AddComment` — the pending gate blocks the second tap.
    #[wasm_bindgen_test]
    async fn mobile_composer_prevents_duplicate_sends() {
        record_bridge();
        let container = make_container();
        let (_composer, _state, _location) = mount_composer(container.clone(), 2);

        next_tick().await;
        let submit = container
            .query_selector("[data-mobile-test='diff-composer-submit']")
            .unwrap()
            .expect("composer submit button present");
        let btn = submit.dyn_ref::<HtmlElement>().unwrap();
        // Two synchronous taps before any re-render/echo: the second must be
        // blocked by the in-flight gate (`submitting`), not by a later DOM
        // disabled re-render.
        btn.click();
        btn.click();
        next_tick().await;
        next_tick().await;

        let sent = sent_lines_joined();
        let add_comments = sent.matches("add_comment").count();
        assert_eq!(
            add_comments, 1,
            "a double-tap must dispatch exactly one AddComment; sent: {sent}"
        );
    }

    fn invoke_count() -> i32 {
        js_sys::eval("window.__invoke_count")
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as i32
    }

    /// A failed `ReviewSubscribe` must NOT latch a stream registration (which
    /// would block any retry), and the subscribe Effect must retry when a
    /// later reactive change re-runs it.
    #[wasm_bindgen_test]
    async fn subscribe_failure_does_not_latch_and_retries() {
        // Rejecting recording bridge: every send fails; count invokes.
        let _ = js_sys::eval(
            "(function(){ \
               window.__invoke_count = 0; \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(){ \
                 window.__invoke_count++; return Promise.reject('boom'); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(null); }; \
             })();",
        );
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let diff_key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let summary_key = (host.clone(), ProjectId("p-1".to_owned()));
        let review_id = ReviewId("rev-1".to_owned());
        let review_key = ReviewRef {
            local_host_id: host.clone(),
            review_id: review_id.clone(),
        };
        let project_for_mount = project.clone();
        let host_for_mount = host.clone();
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(diff_key.clone(), fixture_diff_state());
            });
            // A draft summary so a review id resolves...
            state.review_summaries.update(|m| {
                m.insert(summary_key.clone(), vec![fixture_summary(&review_id)]);
            });
            // ...and a connected host so the subscribe Effect proceeds. No
            // review record and no review_streams entry, so it must subscribe.
            state.host_streams.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    StreamPath("/host/host-1".to_owned()),
                );
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Unstaged
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        // The failed subscribe must not have registered the stream.
        assert!(
            !state
                .review_streams
                .with_untracked(|s| s.contains_key(&review_key)),
            "a failed subscribe must not register the review stream (it would block retry)"
        );
        let after_first = invoke_count();
        assert!(after_first >= 1, "a subscribe must have been attempted");

        // A reactive change re-runs the effect ⇒ it retries (not latched).
        state.reviews.update(|_| {});
        next_tick().await;
        assert!(
            invoke_count() > after_first,
            "the subscribe must retry after a reactive change (was {after_first}, now {})",
            invoke_count()
        );
    }

    /// The inline diff surface binds only to a Draft review. A Submitted
    /// (terminal) review must NOT become the inline ctx: it must not overlay
    /// comments or expose comment/submit controls, and the Start-review
    /// affordance should be shown instead.
    #[wasm_bindgen_test]
    async fn submitted_review_does_not_bind_inline_diff() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let diff_key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let summary_key = (host.clone(), ProjectId("p-1".to_owned()));
        let review_id = ReviewId("rev-1".to_owned());
        let project_for_mount = project.clone();
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(diff_key.clone(), fixture_diff_state());
            });
            // A SUBMITTED (terminal) review — not a Draft.
            let mut summary = fixture_summary(&review_id);
            summary.status = protocol::ReviewStatus::Submitted { submitted_at_ms: 1 };
            state.review_summaries.update(|m| {
                m.insert(summary_key.clone(), vec![summary]);
            });
            // Even with the full record present, it must not bind inline.
            let mut review = fixture_review(&review_id, "p-1");
            review.status = protocol::ReviewStatus::Submitted { submitted_at_ms: 1 };
            state.reviews.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host_for_mount.clone(),
                        review_id: review_id.clone(),
                    },
                    review,
                );
            });
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Unstaged
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        // No active Draft ⇒ the controls-none placeholder shows, no start button.
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-controls-none']")
                .unwrap()
                .is_some(),
            "with only a submitted review, the diff-review-controls-none placeholder must show"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-start']")
                .unwrap()
                .is_none(),
            "diff-review-start button must never exist (reviews are always-on)"
        );
        // The submitted review must not overlay its comment...
        assert!(
            container
                .query_selector("[data-mobile-test='diff-comment-body']")
                .unwrap()
                .is_none(),
            "a submitted (non-draft) review must not overlay inline comments"
        );
        // ...nor expose any comment affordance (those are ctx-only).
        assert!(
            container
                .query_selector("[data-mobile-test='diff-file-comment-btn']")
                .unwrap()
                .is_none(),
            "a submitted review must not expose inline comment controls"
        );
    }

    /// A live `StatusChanged` to Submitted must drop the inline ctx even while
    /// the Draft summary is still stale: comments/controls disappear and the
    /// Start-review affordance returns.
    #[wasm_bindgen_test]
    async fn live_status_change_drops_inline_ctx_with_stale_summary() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project(&host, "p-1");
        let diff_key = ProjectDiffRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            root: ProjectRootPath("/x".to_owned()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        };
        let summary_key = (host.clone(), ProjectId("p-1".to_owned()));
        let review_id = ReviewId("rev-1".to_owned());
        let review_key = ReviewRef {
            local_host_id: host.clone(),
            review_id: review_id.clone(),
        };
        let project_for_mount = project.clone();
        let review_key_for_mount = review_key.clone();
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_diffs.update(|m| {
                m.insert(diff_key.clone(), fixture_diff_state());
            });
            // Draft summary + Draft full record.
            state.review_summaries.update(|m| {
                m.insert(summary_key.clone(), vec![fixture_summary(&review_id)]);
            });
            state.reviews.update(|m| {
                m.insert(
                    review_key_for_mount.clone(),
                    fixture_review(&review_id, "p-1"),
                );
            });
            // Pre-register the stream so the subscribe Effect short-circuits.
            state.review_streams.update(|m| {
                m.insert(
                    review_key_for_mount.clone(),
                    StreamPath("/review/rev-1".to_owned()),
                );
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <DiffViewer
                    project=project_for_mount.clone()
                    root=ProjectRootPath("/x".to_owned())
                    scope=ProjectDiffScope::Unstaged
                    path=None
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        // Draft: the inline comment is shown and no controls-none placeholder.
        assert!(
            container
                .query_selector("[data-mobile-test='diff-comment-body']")
                .unwrap()
                .is_some(),
            "draft review must overlay its comment inline"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-controls-none']")
                .unwrap()
                .is_none(),
            "diff-review-controls-none must not show while a draft is active"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-start']")
                .unwrap()
                .is_none(),
            "diff-review-start must never exist (reviews are always-on)"
        );

        // Live status flips to Submitted; the Draft *summary* stays stale.
        let state = holder.borrow().clone().unwrap();
        state.reviews.update(|m| {
            if let Some(r) = m.get_mut(&review_key) {
                r.status = protocol::ReviewStatus::Submitted { submitted_at_ms: 1 };
            }
        });
        next_tick().await;

        // ctx drops: comment gone, controls-none placeholder returns.
        assert!(
            container
                .query_selector("[data-mobile-test='diff-comment-body']")
                .unwrap()
                .is_none(),
            "inline comment must disappear once the live review is non-draft"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-controls-none']")
                .unwrap()
                .is_some(),
            "diff-review-controls-none placeholder must return once the live review is non-draft"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='diff-review-start']")
                .unwrap()
                .is_none(),
            "diff-review-start must never exist (reviews are always-on)"
        );
    }

    /// Regression: only an `Unstaged` surface is a review surface. Active
    /// reviews are anchored server-side to `Unstaged` (index↔worktree), so a
    /// `Staged` or `Uncommitted` tab must NOT overlay a draft's comments or
    /// expose review controls — even with a live Draft review present. (The
    /// `Unstaged` positive is covered by `inline_comment_renders_under_anchored_line`.)
    #[wasm_bindgen_test]
    async fn non_unstaged_scopes_do_not_expose_review_overlay() {
        for scope in [ProjectDiffScope::Uncommitted, ProjectDiffScope::Staged] {
            let host = LocalHostId("host-1".to_owned());
            let project = make_project(&host, "p-1");
            let review_id = ReviewId("rev-1".to_owned());
            let diff_key = ProjectDiffRef {
                local_host_id: host.clone(),
                project_id: ProjectId("p-1".to_owned()),
                root: ProjectRootPath("/x".to_owned()),
                scope,
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
                // Diff content keyed at the (non-unstaged) surface's scope...
                let mut diff = fixture_diff_state();
                diff.scope = scope;
                state.project_diffs.update(|m| {
                    m.insert(diff_key.clone(), diff);
                });
                // ...and a live Draft review that WOULD overlay on Unstaged.
                state.review_summaries.update(|m| {
                    m.insert(summary_key.clone(), vec![fixture_summary(&review_id)]);
                });
                state.reviews.update(|m| {
                    m.insert(review_key.clone(), fixture_review(&review_id, "p-1"));
                });
                state.review_streams.update(|m| {
                    m.insert(review_key.clone(), StreamPath("/review/rev-1".to_owned()));
                });
                provide_context(state);
                view! {
                    <DiffViewer
                        project=project_for_mount.clone()
                        root=ProjectRootPath("/x".to_owned())
                        scope=scope
                        path=None
                        on_close=Callback::new(|_| {})
                    />
                }
            });
            next_tick().await;

            // The diff body still renders...
            assert!(
                container
                    .query_selector("[data-mobile-test='diff-line-added']")
                    .unwrap()
                    .is_some(),
                "the diff itself must still render on a {scope:?} surface"
            );
            // ...but no review overlay or controls.
            assert!(
                container
                    .query_selector("[data-mobile-test='diff-comment-body']")
                    .unwrap()
                    .is_none(),
                "a {scope:?} surface must not overlay review comments (anchored to Unstaged)"
            );
            assert!(
                container
                    .query_selector("[data-mobile-test='diff-review-controls']")
                    .unwrap()
                    .is_none(),
                "a {scope:?} surface must not render review controls"
            );
            assert!(
                container
                    .query_selector("[data-mobile-test='diff-file-comment-btn']")
                    .unwrap()
                    .is_none(),
                "a {scope:?} surface must not expose inline comment affordances"
            );
        }
    }
}
