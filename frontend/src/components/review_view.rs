//! Three-pane review workbench: file list (left), diff for the selected
//! file (center), action sidebar (right). All state derives from
//! `AppState::reviews` keyed by `ReviewId` — the review is mounted lazily
//! on first display (server pushes the `Snapshot` over `/review/<id>` on
//! subscribe).
//!
//! Reactivity rules (`dev-docs/01-philosophy.md`):
//! * No optimistic UI: action buttons disable on click and re-enable when
//!   the corresponding `ReviewEvent` echoes back via dispatch.
//! * No cached counts on the frontend; per-file thread counts derive via
//!   `Memo` from the live `Review` record.
//! * Late-subscribe replay: `ReviewEvent::Snapshot` is the source of truth
//!   on subscribe and replaces any prior partial entry.

use std::collections::BTreeMap;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::components::diff_view::{
    DecorationFileHeaderFn, DecorationLineFn, DiffView, GutterActionFileHeaderFn,
    GutterPointerDownFn, LineExtraClassFn,
};
use crate::send::send_frame;

// The inline-review presentational components live in a sibling module so
// they can be reused on any diff surface. `ThreadRegionFiltered` is the
// one this module mounts directly (via the decoration builders); the cards
// and composer it nests are referenced inside `inline_review` itself.
pub(crate) use crate::components::inline_review::ThreadRegionFiltered;
use crate::state::{
    ActiveAgentRef, AgentInfo, AppState, ReviewActionGate, ReviewActionTarget, TabContent,
    root_display_name,
};

use protocol::{
    BackendKind, FrameKind, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffPayload,
    ProjectRootPath, Review, ReviewActionPayload, ReviewAiReviewerStatus, ReviewAnchor,
    ReviewCommentSource, ReviewDiffSide, ReviewLocation, ReviewStatus, ReviewSuggestionState,
    StreamPath,
};

type ReviewAiIntervalSlot = StoredValue<Option<(i32, Closure<dyn Fn()>)>, LocalStorage>;

/// Identifier for the file currently selected in the left rail. Combining
/// root + relative_path picks a single (root, file) tuple inside the
/// review's flattened file list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SelectedFile {
    root: ProjectRootPath,
    relative_path: String,
}

/// Inline composer state. `Some` ⇒ the composer is open pinned to the
/// given location; `None` ⇒ closed. Body text is local UI state — the
/// committed comment is server state created via `AddComment`.
///
/// Storing the full `ReviewLocation` (root + relative_path + anchor)
/// rather than only an anchor means switching files while the composer
/// is open does not move the comment to the wrong file: render and
/// save are gated on the location matching the currently rendered
/// thread region.
#[derive(Clone, Debug)]
pub(crate) struct ComposerState {
    pub(crate) location: ReviewLocation,
    pub(crate) body: RwSignal<String>,
}

/// Unified/SBS toggle surfaced in the review header.
///
/// We intentionally don't surface the Hunks/FullFile toggle here:
/// review snapshots are always read with `DiffContextMode::FullFile`
/// (`server::host::read_review_diffs`), so flipping to Hunks would
/// silently misrepresent what's actually being reviewed.
#[component]
fn ReviewLayoutToggle() -> impl IntoView {
    let state = expect_context::<AppState>();
    let view_mode = state.diff_view_mode;
    view! {
        <div class="review-layout-toggle">
            <div class="settings-segmented-control">
                <button
                    class=move || if view_mode.get() == crate::state::DiffViewMode::Unified {
                        "segment active"
                    } else {
                        "segment"
                    }
                    on:click=move |_| {
                        view_mode.set(crate::state::DiffViewMode::Unified);
                        crate::components::settings_panel::persist_diff_view_mode(
                            crate::state::DiffViewMode::Unified,
                        );
                    }
                >"Unified"</button>
                <button
                    class=move || if view_mode.get() == crate::state::DiffViewMode::SideBySide {
                        "segment active"
                    } else {
                        "segment"
                    }
                    on:click=move |_| {
                        view_mode.set(crate::state::DiffViewMode::SideBySide);
                        crate::components::settings_panel::persist_diff_view_mode(
                            crate::state::DiffViewMode::SideBySide,
                        );
                    }
                >"Side by Side"</button>
            </div>
        </div>
    }
}

#[component]
pub fn ReviewView(host_id: String, review_id: protocol::ReviewId) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Reactive lookup of the review record. Returns None until the
    // server's `Snapshot` arrives on `/review/<id>`.
    let review_signal: Memo<Option<Review>> = {
        let id = review_id.clone();
        Memo::new(move |_| state.reviews.with(|map| map.get(&id).cloned()))
    };

    // Subscribe to /review/<id> on first mount when we don't already
    // have a full Review record. The summary list (which seeds the
    // tab-open from project surface) does not include the full diff
    // snapshot, so without this subscribe the view would be stuck on
    // "Loading review…" after a reload.
    {
        let host_for_sub = host_id.clone();
        let review_for_sub = review_id.clone();
        let already_present = review_signal.get_untracked().is_some();
        log::info!(
            "review.subscribe.consider host={} review={} already_present={}",
            host_for_sub,
            review_for_sub,
            already_present
        );
        if !already_present {
            spawn_local(async move {
                let stream = StreamPath(format!("/review/{}", review_for_sub.0));
                log::info!(
                    "review.subscribe.send host={} review={}",
                    host_for_sub,
                    review_for_sub
                );
                if let Err(e) = send_frame(
                    &host_for_sub,
                    stream,
                    FrameKind::ReviewSubscribe,
                    &serde_json::json!({}),
                )
                .await
                {
                    log::error!(
                        "review.subscribe.send_err host={} review={} error={}",
                        host_for_sub,
                        review_for_sub,
                        e
                    );
                }
            });
        }
    }

    // Selected file in the left rail. None until the first render with a
    // non-empty diff list — an Effect picks the first file when the
    // snapshot arrives.
    let selected: RwSignal<Option<SelectedFile>> = RwSignal::new(None);
    {
        Effect::new(move |_| {
            if selected.get_untracked().is_some() {
                return;
            }
            let Some(review) = review_signal.get() else {
                return;
            };
            for diff in &review.diffs {
                if let Some(file) = diff.files.first() {
                    selected.set(Some(SelectedFile {
                        root: diff.root.clone(),
                        relative_path: file.relative_path.clone(),
                    }));
                    break;
                }
            }
        });
    }

    let host_id_for_view = host_id.clone();
    let review_id_for_view = review_id.clone();

    // Gate: tracks only whether a Review record exists for this id, not
    // the record itself. The `<ReviewBody>` thus mounts exactly once per
    // review id — subsequent `ReviewEvent` deltas (CommentUpsert,
    // Suggestion, StatusChanged) flip individual sub-signals inside the
    // body rather than remounting the whole diff scrollport, so the
    // user's scroll position survives a comment add.
    let loaded: Memo<bool> = {
        let id = review_id.clone();
        Memo::new(move |_| state.reviews.with(|map| map.contains_key(&id)))
    };

    view! {
        <div class="review-view" data-review-id={review_id.0.clone()}>
            {move || {
                if !loaded.get() {
                    return view! {
                        <div class="review-loading">
                            <p class="placeholder-text">"Loading review\u{2026}"</p>
                        </div>
                    }.into_any();
                }
                // Seed `<ReviewBody>` with the snapshot at mount time
                // via `get_untracked` so the closure doesn't subscribe
                // to subsequent record changes — those flow through
                // `live` inside the body.
                let Some(seed) = review_signal.get_untracked() else {
                    return view! {
                        <div class="review-loading">
                            <p class="placeholder-text">"Loading review\u{2026}"</p>
                        </div>
                    }.into_any();
                };
                view! {
                    <ReviewBody
                        review=seed
                        selected=selected
                        host_id=host_id_for_view.clone()
                        review_id=review_id_for_view.clone()
                    />
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn ReviewBody(
    review: Review,
    selected: RwSignal<Option<SelectedFile>>,
    host_id: String,
    review_id: protocol::ReviewId,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Live re-derivation of the review record — `review` is the snapshot
    // at first render; subsequent updates flow through the AppState
    // signal. Each closure that depends on review fields reads via this
    // memo so it tracks updates instead of freezing the snapshot.
    let live: Memo<Option<Review>> = {
        let id = review_id.clone();
        Memo::new(move |_| state.reviews.with(|map| map.get(&id).cloned()))
    };

    // Single source of truth for "this review is mutable". Every
    // mutation control derives `disabled` from this Memo so a status
    // flip out of Draft (e.g. Submitted via the sidebar button) hides
    // the affordance everywhere at once.
    let is_draft: Memo<bool> = {
        let initial = review.status.clone();
        Memo::new(move |_| {
            matches!(
                live.get()
                    .map(|r| r.status)
                    .unwrap_or_else(|| initial.clone()),
                ReviewStatus::Draft
            )
        })
    };

    let initial_status = review.status.clone();
    let initial_origin = review.origin_agent_id.clone();
    let status_text = {
        let initial = initial_status.clone();
        move || -> String {
            let s = live
                .get()
                .map(|r| r.status)
                .unwrap_or_else(|| initial.clone());
            match s {
                ReviewStatus::Draft => "Draft".to_string(),
                ReviewStatus::Submitted { .. } => "Submitted".to_string(),
                ReviewStatus::Consumed { .. } => "Consumed".to_string(),
                ReviewStatus::Cancelled { .. } => "Cancelled".to_string(),
            }
        }
    };
    let status_kind = {
        let initial = initial_status.clone();
        move || -> &'static str {
            match live
                .get()
                .map(|r| r.status)
                .unwrap_or_else(|| initial.clone())
            {
                ReviewStatus::Draft => "draft",
                ReviewStatus::Submitted { .. } => "submitted",
                ReviewStatus::Consumed { .. } => "consumed",
                ReviewStatus::Cancelled { .. } => "cancelled",
            }
        }
    };
    let header_origin = {
        let initial = initial_origin.clone();
        let agent_state = state.clone();
        move || -> Option<String> {
            let agent_id = live
                .get()
                .map(|r| r.origin_agent_id)
                .unwrap_or_else(|| initial.clone());
            // Project-scoped reviews carry a synthetic, non-deliverable
            // origin (`project-review:<id>`) that is not a real agent —
            // never surface it as an "Origin" to the user.
            if agent_id.0.starts_with("project-review:") {
                return None;
            }
            let name = agent_state.agents.with(|agents| {
                agents
                    .iter()
                    .find(|a| a.agent_id == agent_id)
                    .map(|a| a.name.clone())
            });
            Some(match name {
                Some(name) if !name.is_empty() => format!("Origin: {name}"),
                _ => format!("Origin: {}", short_agent_id(&agent_id.0)),
            })
        }
    };

    let status_text_for_badge = status_text.clone();
    let status_kind_for_attr = status_kind.clone();
    let status_kind_for_class = status_kind;

    // Full-width banner that surfaces the non-Draft lifecycle states. Per
    // dev-doc §5: "Submitted (waiting for delivery), Consumed (delivered),
    // Cancelled (terminal)" should be prominent, not buried in the sidebar.
    let banner_status = {
        let initial = initial_status.clone();
        Memo::new(move |_| {
            live.get()
                .map(|r| r.status)
                .unwrap_or_else(|| initial.clone())
        })
    };

    // Composer state lives at this level so both `ReviewFileList`
    // (which decorates rows for files with an open draft composer) and
    // `ReviewCenter` (which mounts the composer) read the same signal.
    let composer: RwSignal<Option<ComposerState>> = RwSignal::new(None);

    view! {
        <div class="review-body">
            <div class="review-header">
                <span
                    class=move || format!("review-status-badge {}", status_kind_for_class())
                    data-status={move || status_kind_for_attr()}
                >
                    {move || status_text_for_badge()}
                </span>
                {move || header_origin().map(|text| view! {
                    <span class="review-origin">{text}</span>
                })}
                <ReviewLayoutToggle/>
            </div>
            {
                let banner_state = state.clone();
                move || render_status_banner(banner_status.get(), &banner_state)
            }
            <div class="review-three-pane">
                <ReviewFileList review=review.clone() selected=selected composer=composer />
                <ReviewCenter
                    review=review.clone()
                    selected=selected
                    host_id=host_id.clone()
                    review_id=review_id.clone()
                    is_draft=is_draft
                    composer=composer
                />
                <ReviewSidebar
                    review=review
                    host_id=host_id
                    review_id=review_id
                    is_draft=is_draft
                />
            </div>
        </div>
    }
}

/// Renders the full-width status banner for non-Draft reviews. Returns
/// `None` while in Draft so the three-pane workspace fills the surface.
fn render_status_banner(status: ReviewStatus, state: &AppState) -> Option<AnyView> {
    let (kind, message) = match status {
        ReviewStatus::Draft => return None,
        ReviewStatus::Submitted { submitted_at_ms } => (
            "submitted",
            format!(
                "Submitted {} — waiting for delivery to originating agent.",
                format_relative_time(submitted_at_ms)
            ),
        ),
        ReviewStatus::Consumed {
            target_agent_id,
            consumed_at_ms,
            ..
        } => {
            let target_label = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.agent_id == target_agent_id)
                    .map(|a| a.name.clone())
            });
            let agent_label = match target_label {
                Some(name) if !name.is_empty() => name,
                _ => short_agent_id(&target_agent_id.0),
            };
            (
                "consumed",
                format!(
                    "Consumed {} — delivered to {agent_label}.",
                    format_relative_time(consumed_at_ms)
                ),
            )
        }
        ReviewStatus::Cancelled { cancelled_at_ms } => (
            "cancelled",
            format!("Cancelled {}.", format_relative_time(cancelled_at_ms)),
        ),
    };
    Some(
        view! {
            <div class={format!("review-status-banner {kind}")} data-status={kind}>
                {message}
            </div>
        }
        .into_any(),
    )
}

/// Lower-case 8-char prefix of an agent UUID — only used as a fallback
/// label when the agent record isn't in the registry. Real names are
/// preferred everywhere.
fn short_agent_id(id: &str) -> String {
    id.chars().take(8).collect::<String>()
}

/// Label used for the center-tab title when opening a review. Includes
/// the short review id so multiple open reviews are distinguishable in
/// the tab strip ("Review · 3751f659" vs "Review · bb8750a0").
pub fn review_tab_label(review_id: &protocol::ReviewId) -> String {
    let short: String = review_id.0.chars().take(8).collect();
    format!("Review \u{00b7} {short}")
}

/// Local copy of the `format_relative_time` pattern used by chat_message
/// and agents_panel — kept inline here so the review surface doesn't
/// depend on a sibling component's private helper.
pub(crate) fn format_relative_time(timestamp_ms: u64) -> String {
    if timestamp_ms == 0 {
        return String::new();
    }
    let now = js_sys::Date::now() as u64;
    let diff_secs = now.saturating_sub(timestamp_ms) / 1000;
    if diff_secs < 60 {
        "just now".to_owned()
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86400)
    }
}

/// Wire a textarea so its height tracks its content. Mounts an Effect
/// that reads the body signal and resizes the element each render. The
/// height is capped to keep extremely long bodies from eating the
/// composer area entirely; users can still scroll inside the textarea
/// past the cap.
pub(crate) fn autosize_textarea(node_ref: NodeRef<leptos::html::Textarea>, body: RwSignal<String>) {
    Effect::new(move |_| {
        let _ = body.get();
        if let Some(el) = node_ref.get() {
            let html_el: web_sys::HtmlElement = (*el).clone();
            let style = html_el.style();
            // Reset to auto first so scrollHeight reflects content
            // truthfully — without this, shrinking the body wouldn't
            // shrink the box.
            let _ = style.set_property("height", "auto");
            let scroll_h = html_el.scroll_height();
            let capped = scroll_h.min(300);
            let _ = style.set_property("height", &format!("{capped}px"));
        }
    });
}

/// "1m 23s" / "12s" — used for the AI reviewer's elapsed-time chip.
fn format_elapsed(start_ms: u64) -> String {
    if start_ms == 0 {
        return String::new();
    }
    let now = js_sys::Date::now() as u64;
    let secs = now.saturating_sub(start_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Reactive flattened file list for the left rail. Groups files by root,
/// preserving the order they appear in `Review.diffs`. Counts derive via
/// `Memo` from the live review's comments + suggestions — never cached.
#[component]
fn ReviewFileList(
    review: Review,
    selected: RwSignal<Option<SelectedFile>>,
    composer: RwSignal<Option<ComposerState>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Flat list of (root, files in that root) preserving diff order. The
    // diff snapshot is frozen once a review is created, so the file
    // structure itself is non-reactive — only the count badges are.
    let mut roots: Vec<(ProjectRootPath, Vec<ProjectGitDiffFile>)> = Vec::new();
    for diff in &review.diffs {
        roots.push((diff.root.clone(), diff.files.clone()));
    }

    let review_id_for_counts = review.id.clone();

    // Flat (root, path) sequence for arrow-key navigation. Matches the
    // visual order of the rendered rows.
    let flat_order: Vec<(ProjectRootPath, String)> = roots
        .iter()
        .flat_map(|(root, files)| {
            files
                .iter()
                .map(move |f| (root.clone(), f.relative_path.clone()))
        })
        .collect();
    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        let key = ev.key();
        let direction: i32 = match key.as_str() {
            "ArrowDown" | "j" => 1,
            "ArrowUp" | "k" => -1,
            "Home" => -2,
            "End" => 2,
            _ => return,
        };
        if flat_order.is_empty() {
            return;
        }
        ev.prevent_default();
        let current_idx = selected
            .get_untracked()
            .and_then(|s| {
                flat_order
                    .iter()
                    .position(|(r, p)| *r == s.root && *p == s.relative_path)
            })
            .unwrap_or(0);
        let new_idx = match direction {
            -2 => 0,
            2 => flat_order.len() - 1,
            _ => {
                let len = flat_order.len() as i32;
                let next = (current_idx as i32 + direction).rem_euclid(len);
                next as usize
            }
        };
        let (root, path) = flat_order[new_idx].clone();
        selected.set(Some(SelectedFile {
            root,
            relative_path: path,
        }));
        // Move keyboard focus to the newly-selected row so the next
        // keydown event continues to land on this list. Without this the
        // focus would stay on the previous button (which still works
        // because the keydown is on the list container, but rows that
        // scroll out of view feel broken).
        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            let selector = format!(".review-file-row[data-flat-idx=\"{new_idx}\"]");
            if let Ok(Some(el)) = document.query_selector(&selector)
                && let Ok(html_el) = el.dyn_into::<web_sys::HtmlElement>()
            {
                let _ = html_el.focus();
                html_el.scroll_into_view_with_bool(false);
            }
        }
    };

    let mut flat_idx = 0usize;
    view! {
        <div
            class="review-file-list"
            role="listbox"
            tabindex="0"
            on:keydown=on_keydown
        >
            {roots.into_iter().map(|(root, files)| {
                let root_label = root_display_name(&root);
                let root_for_files = root.clone();
                view! {
                    <div class="review-file-list-root">
                        <div class="review-file-list-root-label">{root_label}</div>
                        {files.into_iter().map(|file| {
                            let row_root = root_for_files.clone();
                            let row_path = file.relative_path.clone();
                            let click_root = row_root.clone();
                            let click_path = row_path.clone();

                            // Derived selection state for this row.
                            let is_selected = {
                                let row_root = row_root.clone();
                                let row_path = row_path.clone();
                                move || selected.get().is_some_and(|s|
                                    s.root == row_root && s.relative_path == row_path
                                )
                            };

                            // Per-row reactive counts derived from the
                            // live review record. No cached count fields;
                            // a `Memo` recomputes whenever comments or
                            // suggestions change.
                            let counts_review_id = review_id_for_counts.clone();
                            let counts_root = row_root.clone();
                            let counts_path = row_path.clone();
                            let counts: Memo<(usize, usize)> = Memo::new(move |_| {
                                state.reviews.with(|map| {
                                    let Some(review) = map.get(&counts_review_id) else {
                                        return (0, 0);
                                    };
                                    let user = review.comments.iter().filter(|c| {
                                        c.location.root == counts_root
                                            && c.location.relative_path == counts_path
                                    }).count();
                                    let pending_ai = review.suggestions.iter().filter(|s| {
                                        s.location.root == counts_root
                                            && s.location.relative_path == counts_path
                                            && matches!(s.state, ReviewSuggestionState::Pending)
                                    }).count();
                                    (user, pending_ai)
                                })
                            });

                            // Compact path display: parent dir + basename.
                            // Full path remains in `title` for hover tooltip
                            // and concatenated text content for keyboard /
                            // selection-by-text affordances.
                            let (parent_dir, basename) = split_path_for_display(&row_path);
                            let title_path = row_path.clone();
                            let is_selected_for_class = is_selected.clone();
                            let is_selected_for_aria = is_selected.clone();
                            let is_selected_for_aria_sel = is_selected;
                            let row_idx = flat_idx;
                            flat_idx += 1;
                            view! {
                                <button
                                    class=move || if is_selected_for_class() {
                                        "review-file-row active"
                                    } else {
                                        "review-file-row"
                                    }
                                    role="option"
                                    aria-current=move || if is_selected_for_aria() { "true" } else { "false" }
                                    aria-selected=move || if is_selected_for_aria_sel() { "true" } else { "false" }
                                    data-flat-idx=row_idx.to_string()
                                    title={title_path}
                                    on:click=move |_| {
                                        let perf_key = format!(
                                            "review:{}:{}",
                                            click_root.0, click_path
                                        );
                                        crate::perf::mark_start(&perf_key);
                                        crate::perf::log_phase(
                                            "review_file_open",
                                            "click",
                                            &perf_key,
                                            "",
                                        );
                                        selected.set(Some(SelectedFile {
                                            root: click_root.clone(),
                                            relative_path: click_path.clone(),
                                        }));
                                    }
                                >
                                    {(!parent_dir.is_empty()).then(|| view! {
                                        <span class="review-file-row-dir">{parent_dir}</span>
                                    })}
                                    <span class="review-file-row-name">{basename}</span>
                                    {
                                        let draft_root = row_root.clone();
                                        let draft_path = row_path.clone();
                                        move || {
                                            let (u, a) = counts.get();
                                            let has_draft = composer.with(|c| {
                                                c.as_ref().is_some_and(|c| {
                                                    c.location.root == draft_root
                                                        && c.location.relative_path == draft_path
                                                        && !c.body.get().trim().is_empty()
                                                })
                                            });
                                            let mut badges = Vec::new();
                                            if has_draft {
                                                badges.push(view! {
                                                    <span class="review-count-badge draft"
                                                          title="Unsaved draft comment on this file">
                                                        "draft"
                                                    </span>
                                                }.into_any());
                                            }
                                            if u > 0 {
                                                badges.push(view! {
                                                    <span class="review-count-badge user"
                                                          data-count-user=u.to_string()
                                                          title=format!("{u} comment{}",
                                                              if u == 1 { "" } else { "s" })>
                                                        {u.to_string()}
                                                    </span>
                                                }.into_any());
                                            }
                                            if a > 0 {
                                                badges.push(view! {
                                                    <span class="review-count-badge ai"
                                                          data-count-ai=a.to_string()
                                                          title=format!("{a} pending AI suggestion{}",
                                                              if a == 1 { "" } else { "s" })>
                                                        {a.to_string()}
                                                    </span>
                                                }.into_any());
                                            }
                                            badges
                                        }
                                    }
                                </button>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

/// Live state of an in-progress click+drag line-range selection on the
/// diff gutter. Pure UI state — never sent on the wire. Anchored to a
/// specific (root, file, side) so a drag that wanders onto another file
/// or the opposite side stays clamped to the start.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DragSelection {
    root: ProjectRootPath,
    relative_path: String,
    side: ReviewDiffSide,
    /// Where the user pressed the pointer down. Stays fixed.
    start_line: u32,
    /// Latest line the cursor is over. Updated on pointermove. The
    /// rendered selection range is `min(start,end) ..= max(start,end)`.
    end_line: u32,
    /// Mouse still down. Cleared on pointerup or Escape so a stale
    /// selection doesn't keep highlighting after the gesture ends.
    is_active: bool,
}

#[component]
fn ReviewCenter(
    review: Review,
    selected: RwSignal<Option<SelectedFile>>,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
    composer: RwSignal<Option<ComposerState>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let drag_selection: RwSignal<Option<DragSelection>> = RwSignal::new(None);

    // Reactive Memo over `Review.diffs`. Driven by the live record so a
    // late-arriving Snapshot replaces the initial render's payload list.
    let frozen_review_id = review.id.clone();
    let initial_diffs = review.diffs.clone();
    let frozen_state = state.clone();
    let frozen_payload: Memo<Option<Vec<ProjectGitDiffPayload>>> = Memo::new(move |_| {
        Some(frozen_state.reviews.with(|map| {
            map.get(&frozen_review_id)
                .map(|r| r.diffs.clone())
                .unwrap_or_else(|| initial_diffs.clone())
        }))
    });

    install_drag_listeners(drag_selection, composer);

    view! {
        <div class="review-center">
            {move || {
                let Some(sel) = selected.get() else {
                    return view! {
                        <div class="review-empty">
                            <p class="placeholder-text">"Select a file"</p>
                        </div>
                    }.into_any();
                };
                // Validate the selected file still exists in the (live) snapshot.
                // `with_untracked` is critical: a tracking read would
                // subscribe this closure to every reviews-map update
                // (CommentUpsert, Suggestion, StatusChanged), remounting
                // `DiffView` -> `DiffContent` on each delta and silently
                // resetting the user's scroll position. The file list
                // inside a review is frozen at snapshot time, so we don't
                // need to re-validate when comments change.
                let exists = state.reviews.with_untracked(|map| {
                    map.get(&review_id).is_some_and(|r| {
                        r.diffs.iter().any(|d| {
                            d.root == sel.root
                                && d.files.iter().any(|f| f.relative_path == sel.relative_path)
                        })
                    })
                });
                if !exists {
                    return view! {
                        <div class="review-empty">
                            <p class="placeholder-text">"File not found in review snapshot"</p>
                        </div>
                    }.into_any();
                }

                let host_id = host_id.clone();
                let review_id = review_id.clone();
                let root = sel.root.clone();
                let path = sel.relative_path.clone();
                let perf_key = format!("review:{}:{}", root.0, path);
                crate::perf::log_phase("review_file_open", "rerender_begin", &perf_key, "");

                let gutter_pointer_down = make_gutter_pointer_down(drag_selection, is_draft);
                let line_extra_class = make_line_extra_class(drag_selection);
                let gutter_action_for_file_header = make_gutter_action_for_file_header(
                    composer, is_draft,
                );
                let line_decoration = make_line_decoration(
                    composer,
                    review_id.clone(),
                    host_id.clone(),
                    is_draft,
                );
                let file_header_decoration = make_file_header_decoration(
                    composer,
                    review_id,
                    host_id,
                    is_draft,
                );

                let view = view! {
                    <DiffView
                        root=root
                        scope=ProjectDiffScope::Uncommitted
                        path=path
                        frozen_payload=frozen_payload
                        on_gutter_pointer_down=gutter_pointer_down
                        line_extra_class=line_extra_class
                        gutter_action_for_file_header=gutter_action_for_file_header
                        decoration_below_line=line_decoration
                        decoration_below_file_header=file_header_decoration
                    />
                }.into_any();
                crate::perf::log_phase("review_file_open", "rerender_done", &perf_key, "");
                view
            }}
        </div>
    }
}

/// Pointer-down handler attached to the canonical line-number gutter
/// (`.diff-gutter-new` for Added/Context, `.diff-gutter-old` for Removed).
/// Starts a drag selection anchored at the clicked line. The diff
/// renderer has already called `prevent_default()` on the event so the
/// browser's native text selection doesn't fight us.
///
/// Disabled-while-not-draft is enforced here, not in CSS — a no-op
/// callback in non-Draft state matches the behavior the old `+` button
/// had (the button was rendered but disabled).
fn make_gutter_pointer_down(
    drag_selection: RwSignal<Option<DragSelection>>,
    is_draft: Memo<bool>,
) -> GutterPointerDownFn {
    Arc::new(
        move |root: ProjectRootPath, path: String, side: ReviewDiffSide, line: u32| {
            if !is_draft.get_untracked() {
                return;
            }
            drag_selection.set(Some(DragSelection {
                root,
                relative_path: path,
                side,
                start_line: line,
                end_line: line,
                is_active: true,
            }));
        },
    )
}

/// Reactive class hook: returns `Some("diff-line-selected")` when the
/// `(file, side, line)` falls within the current drag selection range.
/// The diff renderer reads this inside the row's reactive class
/// closure, so updates to `drag_selection` flow to the DOM each time
/// the cursor moves.
fn make_line_extra_class(drag_selection: RwSignal<Option<DragSelection>>) -> LineExtraClassFn {
    Arc::new(
        move |root: ProjectRootPath, path: String, side: ReviewDiffSide, line: u32| {
            drag_selection.with(|sel| {
                let sel = sel.as_ref()?;
                if sel.root != root || sel.relative_path != path || sel.side != side {
                    return None;
                }
                let (lo, hi) = if sel.start_line <= sel.end_line {
                    (sel.start_line, sel.end_line)
                } else {
                    (sel.end_line, sel.start_line)
                };
                if line >= lo && line <= hi {
                    Some("diff-line-selected")
                } else {
                    None
                }
            })
        },
    )
}

/// Install window-level pointer + keyboard listeners that drive the
/// active drag selection. Attaches once when `ReviewCenter` mounts and
/// removes them on cleanup so the listeners don't outlive the workbench.
///
/// Why window-level (not on `.diff-content`): rows mount/unmount under
/// virtualization while the user is mid-drag. Listening on the diff
/// container would lose events when the originating row scrolls out of
/// view. Window listeners survive that.
fn install_drag_listeners(
    drag_selection: RwSignal<Option<DragSelection>>,
    composer: RwSignal<Option<ComposerState>>,
) {
    let Some(window) = web_sys::window() else {
        return;
    };

    let pm_window = window.clone();
    let pointermove =
        Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |ev: web_sys::PointerEvent| {
            let Some(current) = drag_selection.get_untracked() else {
                return;
            };
            if !current.is_active {
                return;
            }
            let Some(document) = pm_window.document() else {
                return;
            };
            let Some(target) =
                document.element_from_point(ev.client_x() as f32, ev.client_y() as f32)
            else {
                return;
            };
            let line_el: web_sys::Element = match target.closest(".diff-line") {
                Ok(Some(el)) => el,
                _ => return,
            };
            // Look up the line number on the SIDE the drag started on.
            // Each `.diff-line` exposes `data-anchor-old-line` and
            // `data-anchor-new-line` (each empty if that row has no
            // counterpart on that side), so dragging from a +new gutter
            // through a Removed line (which lacks a new-side number) is
            // a no-op rather than the gesture freezing entirely.
            let attr_name = match current.side {
                ReviewDiffSide::Old => "data-anchor-old-line",
                ReviewDiffSide::New => "data-anchor-new-line",
            };
            let Some(line_attr) = line_el.get_attribute(attr_name) else {
                return;
            };
            if line_attr.is_empty() {
                // Row exists but has no counterpart on the drag side
                // (e.g. dragging on +new through a Removed row). Keep
                // the current selection range; don't bail the gesture.
                return;
            }
            let Ok(line_no) = line_attr.parse::<u32>() else {
                return;
            };
            if line_no != current.end_line {
                drag_selection.update(|slot| {
                    if let Some(s) = slot.as_mut() {
                        s.end_line = line_no;
                    }
                });
            }
        });

    let pointerup =
        Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |_ev: web_sys::PointerEvent| {
            let Some(current) = drag_selection.get_untracked() else {
                return;
            };
            if !current.is_active {
                return;
            }
            let (start, end) = if current.start_line <= current.end_line {
                (current.start_line, current.end_line)
            } else {
                (current.end_line, current.start_line)
            };
            composer.set(Some(ComposerState {
                location: ReviewLocation {
                    root: current.root.clone(),
                    relative_path: current.relative_path.clone(),
                    anchor: ReviewAnchor::LineRange {
                        side: current.side,
                        start_line: start,
                        end_line: end,
                    },
                },
                body: RwSignal::new(String::new()),
            }));
            drag_selection.set(None);
        });

    let keydown =
        Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
            if ev.key() != "Escape" {
                return;
            }
            if drag_selection.get_untracked().is_some() {
                drag_selection.set(None);
            }
        });

    let pointercancel =
        Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |_ev: web_sys::PointerEvent| {
            if drag_selection.get_untracked().is_some() {
                drag_selection.set(None);
            }
        });

    let _ = window
        .add_event_listener_with_callback("pointermove", pointermove.as_ref().unchecked_ref());
    let _ =
        window.add_event_listener_with_callback("pointerup", pointerup.as_ref().unchecked_ref());
    let _ = window
        .add_event_listener_with_callback("pointercancel", pointercancel.as_ref().unchecked_ref());
    let _ = window.add_event_listener_with_callback("keydown", keydown.as_ref().unchecked_ref());

    // The listeners hold !Send/!Sync web_sys handles; park them in a
    // thread-local `StoredValue::new_local` so the cleanup closure can
    // be `Send + Sync` (Leptos requires it on `on_cleanup`).
    let slot: StoredValue<Option<DragListeners>, LocalStorage> =
        StoredValue::new_local(Some(DragListeners {
            window,
            pointermove,
            pointerup,
            pointercancel,
            keydown,
        }));

    on_cleanup(move || {
        slot.update_value(|s| {
            if let Some(h) = s.take() {
                h.remove();
            }
        });
    });
}

struct DragListeners {
    window: web_sys::Window,
    pointermove: Closure<dyn Fn(web_sys::PointerEvent)>,
    pointerup: Closure<dyn Fn(web_sys::PointerEvent)>,
    pointercancel: Closure<dyn Fn(web_sys::PointerEvent)>,
    keydown: Closure<dyn Fn(web_sys::KeyboardEvent)>,
}

impl DragListeners {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback(
            "pointermove",
            self.pointermove.as_ref().unchecked_ref(),
        );
        let _ = self.window.remove_event_listener_with_callback(
            "pointerup",
            self.pointerup.as_ref().unchecked_ref(),
        );
        let _ = self.window.remove_event_listener_with_callback(
            "pointercancel",
            self.pointercancel.as_ref().unchecked_ref(),
        );
        let _ = self
            .window
            .remove_event_listener_with_callback("keydown", self.keydown.as_ref().unchecked_ref());
    }
}

/// Inline thread region under a single line. Only emitted when there's
/// something to show (comment, pending suggestion, rejected toggle, or
/// the open composer is anchored here) so the diff stays compact when
/// most rows have no annotations.
fn make_line_decoration(
    composer: RwSignal<Option<ComposerState>>,
    review_id: protocol::ReviewId,
    host_id: String,
    is_draft: Memo<bool>,
) -> DecorationLineFn {
    let state = expect_context::<AppState>();
    Arc::new(
        move |root: ProjectRootPath, path: String, side: ReviewDiffSide, line: u32| {
            let matcher: Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync> =
                Arc::new(move |a: &ReviewAnchor| match a {
                    ReviewAnchor::LineRange {
                        side: s, end_line, ..
                    } => *s == side && *end_line == line,
                    _ => false,
                });
            if !thread_region_has_content(&state, &review_id, &root, &path, &matcher, composer) {
                return None;
            }
            let review_id = review_id.clone();
            let host_id = host_id.clone();
            Some(
                view! {
                    <ThreadRegionFiltered
                        review_id=review_id
                        root=root
                        relative_path=path
                        host_id=host_id
                        composer=composer
                        matcher=matcher
                        is_draft=is_draft
                    />
                }
                .into_any(),
            )
        },
    )
}

/// Inline `+` button rendered into the file header. The `.review-file-comment-btn`
/// class is what the wasm tests look up to find this affordance.
fn make_gutter_action_for_file_header(
    composer: RwSignal<Option<ComposerState>>,
    is_draft: Memo<bool>,
) -> GutterActionFileHeaderFn {
    Arc::new(move |root: ProjectRootPath, path: String| {
        let click_root = root.clone();
        let click_path = path.clone();
        view! {
            <button
                class="review-add-comment-btn review-file-comment-btn"
                aria-label="Comment on file"
                disabled=move || !is_draft.get()
                on:click=move |_| {
                    composer.set(Some(ComposerState {
                        location: ReviewLocation {
                            root: click_root.clone(),
                            relative_path: click_path.clone(),
                            anchor: ReviewAnchor::File,
                        },
                        body: RwSignal::new(String::new()),
                    }));
                }
            >
                "+"
            </button>
        }
        .into_any()
    })
}

/// Thread region under the file header. Only emitted when there are
/// file-anchored comments / suggestions or the composer is anchored here.
fn make_file_header_decoration(
    composer: RwSignal<Option<ComposerState>>,
    review_id: protocol::ReviewId,
    host_id: String,
    is_draft: Memo<bool>,
) -> DecorationFileHeaderFn {
    let state = expect_context::<AppState>();
    Arc::new(move |root: ProjectRootPath, path: String| {
        let matcher: Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync> =
            Arc::new(|a: &ReviewAnchor| matches!(a, ReviewAnchor::File));
        if !thread_region_has_content(&state, &review_id, &root, &path, &matcher, composer) {
            return None;
        }
        let review_id = review_id.clone();
        let host_id = host_id.clone();
        Some(
            view! {
                <ThreadRegionFiltered
                    review_id=review_id
                    root=root
                    relative_path=path
                    host_id=host_id
                    composer=composer
                    matcher=matcher
                    is_draft=is_draft
                />
            }
            .into_any(),
        )
    })
}

/// Reactive predicate: does this `(file, anchor-matcher)` thread region
/// have any content worth rendering? Read inside the per-line decoration
/// callback so off-screen lines pay no DOM cost when they have nothing
/// to show, but the closure still re-runs when comments/suggestions/the
/// composer change.
pub(crate) fn thread_region_has_content(
    state: &AppState,
    review_id: &protocol::ReviewId,
    root: &ProjectRootPath,
    relative_path: &str,
    matcher: &Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync>,
    composer: RwSignal<Option<ComposerState>>,
) -> bool {
    let any_review = state.reviews.with(|map| {
        let Some(review) = map.get(review_id) else {
            return false;
        };
        let comment_match = review.comments.iter().any(|c| {
            c.location.root == *root
                && c.location.relative_path == relative_path
                && matcher(&c.location.anchor)
        });
        if comment_match {
            return true;
        }
        review.suggestions.iter().any(|s| {
            s.location.root == *root
                && s.location.relative_path == relative_path
                && matcher(&s.location.anchor)
        })
    });
    if any_review {
        return true;
    }
    composer.with(|c| {
        c.as_ref().is_some_and(|c| {
            c.location.root == *root
                && c.location.relative_path == relative_path
                && matcher(&c.location.anchor)
        })
    })
}

#[component]
fn ReviewSidebar(
    review: Review,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Live re-derivation of the review record so sidebar buttons reflect
    // server-pushed status updates without remount.
    let live: Memo<Option<Review>> = {
        let id = review_id.clone();
        Memo::new(move |_| state.reviews.with(|map| map.get(&id).cloned()))
    };
    let live_for_status = live;
    let live_for_count = live;
    let live_for_ai = live;

    let review_clone = review.clone();
    let status: Memo<ReviewStatus> = Memo::new(move |_| {
        live_for_status
            .get()
            .map(|r| r.status)
            .unwrap_or_else(|| review_clone.status.clone())
    });

    // Reactive counts derived from the live signal — never cached.
    let user_comment_count: Memo<usize> = Memo::new(move |_| {
        live_for_count
            .get()
            .map(|r| {
                r.comments
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.source,
                            ReviewCommentSource::User | ReviewCommentSource::AiSuggestion { .. }
                        )
                    })
                    .count()
            })
            .unwrap_or(0)
    });

    let pending_suggestion_count: Memo<usize> = Memo::new(move |_| {
        live_for_count
            .get()
            .map(|r| {
                r.suggestions
                    .iter()
                    .filter(|s| matches!(s.state, ReviewSuggestionState::Pending))
                    .count()
            })
            .unwrap_or(0)
    });

    // Submit gate: status is Draft, comments not empty, AI not running,
    // and no submit echo currently pending.
    let review_id_for_pending = review_id.clone();
    let pending_state = state.clone();
    let action_pending = move || {
        pending_state
            .review_action_pending
            .with(|map| map.get(&review_id_for_pending).copied().unwrap_or_default())
    };

    // ── Submit target picker. The user must choose an explicit
    // destination for the feedback bundle: an existing same-project agent
    // or a freshly spawned one. `None` ⇒ nothing chosen yet (Submit is
    // gated off until a target is picked). The protocol `ReviewSubmitTarget`
    // shape is still settling, so its construction is isolated in
    // `parse_submit_target` — this signal only ever holds the result.
    let submit_target: RwSignal<Option<protocol::ReviewSubmitTarget>> = RwSignal::new(None);
    // Optional instructions for a freshly-spawned target agent. Only
    // meaningful when the picked target is `NewAgent`; folded into the
    // target at submit time (see `on_submit`).
    let submit_new_instructions: RwSignal<String> = RwSignal::new(String::new());
    let submit_new_instructions_for_submit = submit_new_instructions;

    // Project whose live agents are the deliverable submit targets. The
    // review's own origin is deliberately NOT a fallback: project-scoped
    // reviews carry a synthetic origin that is not a live agent. Target
    // resolution lives in the module-level `live_same_project_candidates` /
    // `effective_submit_target` helpers (kept as fns, not closures, so the
    // captures stay `Send` for Leptos).
    let target_project = review.project_id.clone();

    // Returns either an empty string (button enabled) or a short reason
    // string suitable for binding to `title` so hovering a disabled
    // button explains why.
    let submit_reason = {
        let action_pending = action_pending.clone();
        let live = live_for_ai;
        let reason_state = state.clone();
        let reason_host = host_id.clone();
        let reason_project = target_project.clone();
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            let ai_running = live
                .get()
                .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
                .unwrap_or(false);
            if ai_running {
                return "Wait for the AI reviewer to finish";
            }
            if user_comment_count.get() == 0 {
                if pending_suggestion_count.get() > 0 {
                    return "Accept or reject the AI suggestions first, or add a comment";
                }
                return "Add at least one comment first";
            }
            if effective_submit_target(
                &reason_state,
                &reason_host,
                &reason_project,
                submit_target,
                true,
            )
            .is_none()
            {
                return "Choose which agent receives the review";
            }
            if action_pending().submit {
                return "Submit in progress\u{2026}";
            }
            ""
        }
    };
    let cancel_reason = {
        let action_pending = action_pending.clone();
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            if action_pending().cancel {
                return "Cancel in progress\u{2026}";
            }
            ""
        }
    };
    let submit_disabled = {
        let submit_reason = submit_reason.clone();
        move || !submit_reason().is_empty()
    };
    let cancel_disabled = {
        let cancel_reason = cancel_reason.clone();
        move || !cancel_reason().is_empty()
    };

    // ── Run AI reviewer form
    let backend_pick: RwSignal<Option<BackendKind>> = RwSignal::new(None);
    let cost_pick: RwSignal<Option<protocol::SpawnCostHint>> = RwSignal::new(None);
    let instructions: RwSignal<String> = RwSignal::new(String::new());

    let host_for_submit = host_id.clone();
    let review_for_submit = review_id.clone();
    let state_for_submit = state.clone();
    let submit_project = target_project.clone();
    let user_count_for_submit = user_comment_count;
    let pending_count_for_submit = pending_suggestion_count;
    let live_for_submit = live_for_ai;
    let on_submit = move |_| {
        let host = host_for_submit.clone();
        let rid = review_for_submit.clone();
        // Effective deliverable target: explicit picker choice, else a
        // single auto-selected live candidate. No origin fallback — a
        // project-scoped review's synthetic origin is not deliverable.
        let Some(mut submit_target_value) = effective_submit_target(
            &state_for_submit,
            &host,
            &submit_project,
            submit_target,
            false,
        ) else {
            log::warn!("review.submit.click review={rid} skipped=no_deliverable_target");
            return;
        };
        // Fold the optional instructions into a NewAgent target.
        if let protocol::ReviewSubmitTarget::NewAgent { instructions, .. } =
            &mut submit_target_value
        {
            let text = submit_new_instructions_for_submit.get_untracked();
            let trimmed = text.trim();
            *instructions = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        }
        let gate_before = state_for_submit
            .review_action_pending
            .with_untracked(|m| m.get(&rid).copied().unwrap_or_default());
        let status = live_for_submit
            .get_untracked()
            .map(|r| format!("{:?}", std::mem::discriminant(&r.status)))
            .unwrap_or_else(|| "none".to_owned());
        let ai_status = live_for_submit
            .get_untracked()
            .map(|r| match r.ai_reviewer.status {
                ReviewAiReviewerStatus::Idle => "idle",
                ReviewAiReviewerStatus::Running => "running",
                ReviewAiReviewerStatus::Completed => "completed",
                ReviewAiReviewerStatus::Failed => "failed",
            })
            .unwrap_or("unknown");
        let mut claimed = false;
        state_for_submit.review_action_pending.update(|map| {
            let gate = map.entry(rid.clone()).or_default();
            if !gate.submit {
                gate.submit = true;
                claimed = true;
            }
        });
        log::info!(
            "review.submit.click review={} claimed={} gate_before_submit={} comments={} pending_suggestions={} ai_status={} status_disc={} {}",
            rid,
            claimed,
            gate_before.submit,
            user_count_for_submit.get_untracked(),
            pending_count_for_submit.get_untracked(),
            ai_status,
            status,
            conn_diag(&state_for_submit, &host)
        );
        if !claimed {
            return;
        }
        let target_state = state_for_submit.clone();
        let target_rid = rid.clone();
        spawn_local(async move {
            match send_review_action_inner(
                &host,
                target_rid.clone(),
                ReviewActionPayload::Submit {
                    target: submit_target_value.clone(),
                },
            )
            .await
            {
                Ok(()) => {
                    log::info!("review.submit.send_ok review={target_rid}");
                }
                Err(e) => {
                    log::error!("review.submit.send_err review={target_rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&target_rid) {
                            gate.submit = false;
                            if gate.is_idle() {
                                map.remove(&target_rid);
                            }
                        }
                    });
                    log::info!(
                        "review.submit.local_gate_clear review={target_rid} reason=send_err"
                    );
                }
            }
        });
    };

    let host_for_cancel = host_id.clone();
    let review_for_cancel = review_id.clone();
    let state_for_cancel = state.clone();
    let user_count_for_cancel = user_comment_count;
    let on_cancel = move |_| {
        let host = host_for_cancel.clone();
        let rid = review_for_cancel.clone();
        let target_state = state_for_cancel.clone();
        let comment_count = user_count_for_cancel.get_untracked();
        spawn_local(async move {
            let message = if comment_count == 0 {
                "Cancel this review and discard it? This cannot be undone.".to_owned()
            } else {
                format!(
                    "Cancel this review and discard {comment_count} comment{}? This cannot be undone.",
                    if comment_count == 1 { "" } else { "s" }
                )
            };
            if !crate::bridge::confirm_dialog("Cancel review", &message).await {
                log::info!("review.cancel.click review={rid} dialog=declined");
                return;
            }
            let gate_before = target_state
                .review_action_pending
                .with_untracked(|m| m.get(&rid).copied().unwrap_or_default());
            let mut claimed = false;
            target_state.review_action_pending.update(|map| {
                let gate = map.entry(rid.clone()).or_default();
                if !gate.cancel {
                    gate.cancel = true;
                    claimed = true;
                }
            });
            log::info!(
                "review.cancel.click review={} claimed={} gate_before_cancel={} comments={} {}",
                rid,
                claimed,
                gate_before.cancel,
                comment_count,
                conn_diag(&target_state, &host)
            );
            if !claimed {
                return;
            }
            let target_rid = rid.clone();
            match send_review_action_inner(&host, target_rid.clone(), ReviewActionPayload::Cancel)
                .await
            {
                Ok(()) => {
                    log::info!("review.cancel.send_ok review={target_rid}");
                }
                Err(e) => {
                    log::error!("review.cancel.send_err review={target_rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&target_rid) {
                            gate.cancel = false;
                            if gate.is_idle() {
                                map.remove(&target_rid);
                            }
                        }
                    });
                    log::info!(
                        "review.cancel.local_gate_clear review={target_rid} reason=send_err"
                    );
                }
            }
        });
    };

    // ── Clear: drop all comments + AI suggestions (and reset the AI
    // reviewer) without delivering anything. Distinct from Cancel, which
    // discards the whole review. Server echoes `Cleared` with the reset
    // (empty) review, which dispatch folds in and which clears the gate.
    let host_for_clear = host_id.clone();
    let review_for_clear = review_id.clone();
    let state_for_clear = state.clone();
    let count_for_clear = user_comment_count;
    let pending_for_clear = pending_suggestion_count;
    let on_clear = move |_| {
        let host = host_for_clear.clone();
        let rid = review_for_clear.clone();
        let target_state = state_for_clear.clone();
        let total = count_for_clear.get_untracked() + pending_for_clear.get_untracked();
        spawn_local(async move {
            let message = format!(
                "Clear {total} comment{} and AI suggestion{} from this review? This cannot be undone.",
                if count_for_clear.get_untracked() == 1 {
                    ""
                } else {
                    "s"
                },
                if pending_for_clear.get_untracked() == 1 {
                    ""
                } else {
                    "s"
                },
            );
            if !crate::bridge::confirm_dialog("Clear review", &message).await {
                log::info!("review.clear.click review={rid} dialog=declined");
                return;
            }
            let mut claimed = false;
            target_state.review_action_pending.update(|map| {
                let gate = map.entry(rid.clone()).or_default();
                if !gate.clear {
                    gate.clear = true;
                    claimed = true;
                }
            });
            log::info!("review.clear.click review={rid} claimed={claimed}");
            if !claimed {
                return;
            }
            match send_review_action_inner(&host, rid.clone(), ReviewActionPayload::ClearComments)
                .await
            {
                Ok(()) => log::info!("review.clear.send_ok review={rid}"),
                Err(e) => {
                    log::error!("review.clear.send_err review={rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&rid) {
                            gate.clear = false;
                            if gate.is_idle() {
                                map.remove(&rid);
                            }
                        }
                    });
                }
            }
        });
    };
    let clear_reason = {
        let action_pending = action_pending.clone();
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            if user_comment_count.get() == 0 && pending_suggestion_count.get() == 0 {
                return "Nothing to clear";
            }
            if action_pending().clear {
                return "Clear in progress\u{2026}";
            }
            ""
        }
    };
    let clear_disabled = {
        let clear_reason = clear_reason.clone();
        move || !clear_reason().is_empty()
    };

    let host_for_ai = host_id.clone();
    let review_for_ai = review_id.clone();
    let state_for_ai = state.clone();
    let ai_reason = {
        let action_pending = action_pending.clone();
        let live = live_for_ai;
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            if backend_pick.get().is_none() {
                return "Choose an AI backend first";
            }
            if action_pending().start_ai {
                return "AI reviewer starting\u{2026}";
            }
            let running = live
                .get()
                .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
                .unwrap_or(false);
            if running {
                return "AI reviewer is already running";
            }
            ""
        }
    };
    let ai_disabled = {
        let ai_reason = ai_reason.clone();
        move || !ai_reason().is_empty()
    };
    let live_for_run_ai = live_for_ai;
    let on_run_ai = move |_| {
        let rid = review_for_ai.clone();
        let Some(backend) = backend_pick.get_untracked() else {
            log::info!("review.start_ai.skipped review={rid} reason=no_backend");
            return;
        };
        let host = host_for_ai.clone();
        let cost = cost_pick.get_untracked();
        let inst = {
            let raw = instructions.get_untracked();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        };
        let inst_len = inst.as_deref().map(|s| s.len()).unwrap_or(0);
        let gate_before = state_for_ai
            .review_action_pending
            .with_untracked(|m| m.get(&rid).copied().unwrap_or_default());
        let ai_status = live_for_run_ai
            .get_untracked()
            .map(|r| match r.ai_reviewer.status {
                ReviewAiReviewerStatus::Idle => "idle",
                ReviewAiReviewerStatus::Running => "running",
                ReviewAiReviewerStatus::Completed => "completed",
                ReviewAiReviewerStatus::Failed => "failed",
            })
            .unwrap_or("unknown");
        let mut claimed = false;
        state_for_ai.review_action_pending.update(|map| {
            let gate = map.entry(rid.clone()).or_default();
            if !gate.start_ai {
                gate.start_ai = true;
                claimed = true;
            }
        });
        log::info!(
            "review.start_ai.click review={} claimed={} gate_before_start_ai={} backend={:?} cost={:?} instructions_len={} ai_status={} {}",
            rid,
            claimed,
            gate_before.start_ai,
            backend,
            cost,
            inst_len,
            ai_status,
            conn_diag(&state_for_ai, &host)
        );
        if !claimed {
            return;
        }
        let target_state = state_for_ai.clone();
        let target_rid = rid.clone();
        spawn_local(async move {
            let payload = ReviewActionPayload::StartAiReview {
                backend_kind: backend,
                cost_hint: cost,
                instructions: inst,
            };
            match send_review_action_inner(&host, target_rid.clone(), payload).await {
                Ok(()) => {
                    log::info!("review.start_ai.send_ok review={target_rid}");
                }
                Err(e) => {
                    log::error!("review.start_ai.send_err review={target_rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&target_rid) {
                            gate.start_ai = false;
                            if gate.is_idle() {
                                map.remove(&target_rid);
                            }
                        }
                    });
                    log::info!(
                        "review.start_ai.local_gate_clear review={target_rid} reason=send_err"
                    );
                }
            }
        });
    };

    // Backends list — read from host settings if available.
    let backends_state = state.clone();
    let host_for_backends = host_id.clone();
    let backends = move || -> Vec<BackendKind> {
        backends_state
            .host_settings_by_host
            .with(|map| {
                map.get(&host_for_backends)
                    .map(|s| s.enabled_backends.clone())
            })
            .unwrap_or_default()
    };

    // Independent backend reader for the submit-target picker (the AI
    // form already consumed the `backends` closure, which isn't `Copy`).
    let target_backends_state = state.clone();
    let host_for_target_backends = host_id.clone();
    let target_backends = move || -> Vec<BackendKind> {
        target_backends_state
            .host_settings_by_host
            .with(|map| {
                map.get(&host_for_target_backends)
                    .map(|s| s.enabled_backends.clone())
            })
            .unwrap_or_default()
    };

    // Live same-project candidate agents for the picker's "Existing agents"
    // group (plain closure so its captures stay `Send`).
    let candidate_state = state.clone();
    let host_for_candidates = host_id.clone();
    let candidate_project = target_project.clone();
    let candidate_agents = move || -> Vec<(protocol::AgentId, String)> {
        live_same_project_candidates(
            &candidate_state,
            &host_for_candidates,
            &candidate_project,
            true,
        )
    };

    let ai_status_kind = {
        let live = live_for_ai;
        move || -> &'static str {
            match live.get().map(|r| r.ai_reviewer.status) {
                Some(ReviewAiReviewerStatus::Idle) => "idle",
                Some(ReviewAiReviewerStatus::Running) => "running",
                Some(ReviewAiReviewerStatus::Completed) => "completed",
                Some(ReviewAiReviewerStatus::Failed) => "failed",
                None => "unknown",
            }
        }
    };
    let ai_status_text = {
        let live = live_for_ai;
        move || -> &'static str {
            match live.get().map(|r| r.ai_reviewer.status) {
                Some(ReviewAiReviewerStatus::Idle) => "Idle",
                Some(ReviewAiReviewerStatus::Running) => "Running",
                Some(ReviewAiReviewerStatus::Completed) => "Completed",
                Some(ReviewAiReviewerStatus::Failed) => "Failed",
                None => "\u{2014}",
            }
        }
    };

    // Elapsed time tick — only meaningful while Running. setInterval
    // bumps `elapsed_tick` every second; the Effect reinstalls when
    // running state flips so we don't keep timers alive after Completed.
    let live_for_elapsed = live_for_ai;
    let agents_for_elapsed = state.clone();
    let host_for_elapsed = host_id.clone();
    let elapsed_tick = RwSignal::new(0u32);
    let live_for_tick = live_for_ai;
    let interval_slot: ReviewAiIntervalSlot = StoredValue::new_local(None);
    let interval_for_cleanup = interval_slot;
    Effect::new(move |_| {
        let running = live_for_tick
            .get()
            .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
            .unwrap_or(false);
        // Always clear any prior interval before deciding whether to
        // install a new one. setInterval ids leak otherwise.
        interval_slot.update_value(|slot| {
            if let Some((id, _cb)) = slot.take()
                && let Some(window) = web_sys::window()
            {
                window.clear_interval_with_handle(id);
            }
        });
        if !running {
            return;
        }
        let tick = elapsed_tick;
        let cb = Closure::<dyn Fn()>::new(move || {
            tick.update(|t| *t = t.wrapping_add(1));
        });
        if let Some(window) = web_sys::window()
            && let Ok(id) = window.set_interval_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                1000,
            )
        {
            interval_slot.update_value(|slot| *slot = Some((id, cb)));
        }
    });
    on_cleanup(move || {
        interval_for_cleanup.update_value(|slot| {
            if let Some((id, _cb)) = slot.take()
                && let Some(window) = web_sys::window()
            {
                window.clear_interval_with_handle(id);
            }
        });
    });
    let elapsed_label = move || -> Option<String> {
        let _ = elapsed_tick.get();
        let r = live_for_elapsed.get()?;
        if !matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running) {
            return None;
        }
        let agent_id = r.ai_reviewer.agent_id?;
        let host = host_for_elapsed.clone();
        let start_ms = agents_for_elapsed.agents.with(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == host && a.agent_id == agent_id)
                .map(|a| a.created_at_ms)
        })?;
        Some(format_elapsed(start_ms))
    };

    // AI reviewer agent link — open/focus the reviewer's chat tab so the
    // user can watch its reasoning live.
    let host_for_ai_link = host_id.clone();
    let live_for_ai_link = live_for_ai;
    let ai_state_link = state.clone();
    let ai_agent_link = move || -> Option<protocol::AgentId> {
        live_for_ai_link.get().and_then(|r| r.ai_reviewer.agent_id)
    };
    let on_open_ai_agent = {
        let host = host_for_ai_link.clone();
        let state = ai_state_link.clone();
        move |agent_id: protocol::AgentId| {
            let label = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.host_id == host && a.agent_id == agent_id)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "AI Review".to_owned())
            });
            state.open_tab(
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id: host.clone(),
                    agent_id,
                }),
                label,
                true,
            );
        }
    };

    // AI reviewer error string surfaced from the server. Local
    // `dismissed_error` holds the value of the last error the user
    // explicitly dismissed; when the server pushes a new error string
    // (different value) the dismiss is automatically cleared so the new
    // failure shows.
    let live_for_ai_err = live_for_ai;
    let dismissed_error: RwSignal<Option<String>> = RwSignal::new(None);
    let ai_error = move || -> Option<String> {
        let err = live_for_ai_err.get().and_then(|r| r.ai_reviewer.error)?;
        if dismissed_error.get().as_deref() == Some(err.as_str()) {
            return None;
        }
        Some(err)
    };

    // Disclosure state for the "Run AI reviewer" form (#6 polish).
    // Defaults closed; force-open while the reviewer is running so users
    // can see what's happening without expanding it manually.
    let ai_disclosure_open = RwSignal::new(false);
    let ai_running_for_disclosure = live_for_ai;
    let ai_open_attr = move || {
        let user_open = ai_disclosure_open.get();
        let running = ai_running_for_disclosure
            .get()
            .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
            .unwrap_or(false);
        user_open || running
    };
    let _ = status; // silence unused if banner moved out of sidebar

    view! {
        <div class="review-sidebar">
            <div class="review-sidebar-section">
                <h4 class="review-sidebar-title">"Counts"</h4>
                <div class="review-counts" data-test="review-counts">
                    <span class="review-count-line"
                          data-count-comments=move || user_comment_count.get().to_string()>
                        {move || {
                            let n = user_comment_count.get();
                            format!("{n} comment{}", if n == 1 { "" } else { "s" })
                        }}
                    </span>
                    <span class="review-count-line"
                          data-count-pending=move || pending_suggestion_count.get().to_string()>
                        {move || {
                            let n = pending_suggestion_count.get();
                            format!("{n} pending AI suggestion{}", if n == 1 { "" } else { "s" })
                        }}
                    </span>
                </div>
            </div>

            <div class="review-sidebar-section">
                <button
                    class="review-btn primary review-run-ai-btn"
                    disabled=ai_disabled
                    title=ai_reason
                    on:click=move |ev| {
                        // Opening the disclosure on Run is a UX nicety —
                        // the user should always see the reviewer's
                        // current configuration when they kick it off.
                        ai_disclosure_open.set(true);
                        on_run_ai(ev);
                    }
                >
                    "Run AI reviewer"
                </button>
                <details
                    class="review-ai-disclosure"
                    prop:open=ai_open_attr
                    on:toggle=move |ev: leptos::ev::Event| {
                        if let Some(target) = ev.target()
                            && let Ok(el) = target.dyn_into::<web_sys::HtmlDetailsElement>() {
                            ai_disclosure_open.set(el.open());
                        }
                    }
                >
                    <summary class="review-ai-disclosure-summary">
                        "Configure AI reviewer"
                    </summary>
                    <div class="review-ai-disclosure-body">
                        <select
                            class="review-backend-select"
                            on:change=move |ev| {
                                let val = event_target_value(&ev);
                                backend_pick.set(parse_backend_kind(&val));
                            }
                        >
                            <option value="" selected=move || backend_pick.get().is_none()>
                                "Choose backend"
                            </option>
                            {move || backends().into_iter().map(|kind| {
                                let label = backend_kind_label(kind);
                                view! {
                                    <option value={label}>{label}</option>
                                }
                            }).collect::<Vec<_>>()}
                        </select>
                        <select
                            class="review-cost-select"
                            on:change=move |ev| {
                                let val = event_target_value(&ev);
                                cost_pick.set(parse_cost_hint(&val));
                            }
                        >
                            <option value="">"Default cost"</option>
                            <option value="low">"Low"</option>
                            <option value="medium">"Medium"</option>
                            <option value="high">"High"</option>
                        </select>
                        {
                            let instructions_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                            autosize_textarea(instructions_ref, instructions);
                            view! {
                                <textarea
                                    node_ref=instructions_ref
                                    class="review-instructions"
                                    placeholder="Optional instructions for the AI reviewer\u{2026}"
                                    prop:value=move || instructions.get()
                                    on:input=move |ev| instructions.set(event_target_value(&ev))
                                />
                            }
                        }
                    </div>
                </details>
                <div
                    class=move || format!("review-ai-status status-{}", ai_status_kind())
                    data-status=ai_status_kind
                >
                    <span class="review-ai-status-dot"></span>
                    <span class="review-ai-status-label">
                        {move || format!("AI: {}", ai_status_text())}
                    </span>
                    {move || elapsed_label().map(|e| view! {
                        <span class="review-ai-status-elapsed">{e}</span>
                    })}
                </div>
                {
                    let agent_state = state.clone();
                    let host_for_label = host_id.clone();
                    move || ai_agent_link().map(|agent_id| {
                        let click_agent = agent_id.clone();
                        let on_open = on_open_ai_agent.clone();
                        let label = agent_state.agents.with(|agents| {
                            agents
                                .iter()
                                .find(|a| a.host_id == host_for_label && a.agent_id == agent_id)
                                .map(|a| a.name.clone())
                        });
                        let label_text = match label {
                            Some(name) if !name.is_empty() => format!("Open {name}"),
                            _ => "Open AI reviewer chat".to_owned(),
                        };
                        view! {
                            <button
                                class="review-ai-agent-link"
                                title="Open the AI reviewer agent's chat"
                                on:click=move |_| on_open(click_agent.clone())
                            >
                                {label_text}
                            </button>
                        }
                    })
                }
                {move || ai_error().map(|err| {
                    let err_for_dismiss = err.clone();
                    view! {
                        <div class="review-ai-error" data-test="review-ai-error">
                            <div class="review-ai-error-message">{err}</div>
                            <div class="review-ai-error-actions">
                                <button
                                    class="review-btn review-ai-error-dismiss"
                                    on:click=move |_| {
                                        dismissed_error.set(Some(err_for_dismiss.clone()));
                                    }
                                >
                                    "Dismiss"
                                </button>
                            </div>
                        </div>
                    }
                })}
            </div>

            <div class="review-sidebar-section">
                <label class="review-submit-target-label" for="review-submit-target">
                    "Send feedback to"
                </label>
                <select
                    id="review-submit-target"
                    class="review-submit-target-select"
                    data-test="review-submit-target"
                    disabled=move || !is_draft.get()
                    on:change=move |ev| {
                        let val = event_target_value(&ev);
                        submit_target.set(parse_submit_target(&val));
                    }
                >
                    <option value="">"Auto (single same-project agent)"</option>
                    {move || {
                        let agents = candidate_agents();
                        (!agents.is_empty()).then(|| view! {
                            <optgroup label="Existing agents">
                                {agents.into_iter().map(|(id, name)| {
                                    view! {
                                        <option value={format!("existing:{}", id.0)}>{name}</option>
                                    }
                                }).collect::<Vec<_>>()}
                            </optgroup>
                        })
                    }}
                    {move || {
                        let kinds = target_backends();
                        (!kinds.is_empty()).then(|| view! {
                            <optgroup label="Spawn new agent">
                                {kinds.into_iter().map(|kind| {
                                    let label = backend_kind_label(kind);
                                    view! {
                                        <option value={format!("new:{label}")}>
                                            {format!("New {label} agent")}
                                        </option>
                                    }
                                }).collect::<Vec<_>>()}
                            </optgroup>
                        })
                    }}
                </select>
                {move || {
                    // Optional instructions only apply when spawning a new
                    // agent — show the field only for a NewAgent selection.
                    let is_new_agent = matches!(
                        submit_target.get(),
                        Some(protocol::ReviewSubmitTarget::NewAgent { .. })
                    );
                    is_new_agent.then(|| {
                        let instructions_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                        autosize_textarea(instructions_ref, submit_new_instructions);
                        view! {
                            <textarea
                                node_ref=instructions_ref
                                class="review-submit-instructions"
                                data-test="review-submit-instructions"
                                placeholder="Optional instructions for the new agent\u{2026}"
                                prop:value=move || submit_new_instructions.get()
                                on:input=move |ev| {
                                    submit_new_instructions.set(event_target_value(&ev))
                                }
                            />
                        }
                    })
                }}
                <button
                    class="review-btn primary review-submit-btn"
                    disabled=submit_disabled
                    title=submit_reason
                    on:click=on_submit
                >
                    "Submit review"
                </button>
                <button
                    class="review-btn review-clear-btn"
                    data-test="review-clear-btn"
                    disabled=clear_disabled
                    title=clear_reason
                    on:click=on_clear
                >
                    "Clear comments"
                </button>
                <button
                    class="review-btn destructive review-cancel-btn"
                    disabled=cancel_disabled
                    title=cancel_reason
                    on:click=on_cancel
                >
                    "Cancel review"
                </button>
            </div>
        </div>
    }
}

/// Split a relative path into `(parent_dir_with_trailing_slash, basename)`
/// for the file-list rail. Concatenated text content equals the original
/// path so callers (and tests) can still find the row by full-path text.
fn split_path_for_display(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(idx) => (path[..=idx].to_owned(), path[idx + 1..].to_owned()),
        None => (String::new(), path.to_owned()),
    }
}

fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

/// Translate a submit-target picker option value into a `ReviewSubmitTarget`.
/// `None` ⇒ no concrete target selected ("Choose where to send…"). This is
/// the single place that constructs the still-settling `ReviewSubmitTarget`
/// shape, so a protocol change only has to be reconciled here.
fn parse_submit_target(value: &str) -> Option<protocol::ReviewSubmitTarget> {
    if let Some(id) = value.strip_prefix("existing:") {
        Some(protocol::ReviewSubmitTarget::ExistingAgent {
            agent_id: protocol::AgentId(id.to_owned()),
        })
    } else if let Some(label) = value.strip_prefix("new:") {
        parse_backend_kind(label).map(|backend_kind| protocol::ReviewSubmitTarget::NewAgent {
            backend_kind,
            cost_hint: None,
            custom_agent_id: None,
            name: None,
            instructions: None,
        })
    } else {
        None
    }
}

/// Live same-project agents on `host_id` for `project_id` — the deliverable
/// review submit targets. "Live" excludes agents with a fatal error
/// (terminated). The review's own origin is intentionally never consulted:
/// project-scoped reviews use a synthetic, non-deliverable origin. The
/// `tracked` flag selects reactive vs untracked signal reads so the same
/// logic serves both the reactive submit gate and the click handler.
fn live_same_project_candidates(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
    tracked: bool,
) -> Vec<(protocol::AgentId, String)> {
    let collect = |agents: &Vec<AgentInfo>| -> Vec<(protocol::AgentId, String)> {
        agents
            .iter()
            .filter(|a| {
                a.host_id == host_id
                    && a.project_id.as_ref() == Some(project_id)
                    && a.fatal_error.is_none()
            })
            .map(|a| {
                let name = if a.name.is_empty() {
                    let short: String = a.agent_id.0.chars().take(8).collect();
                    format!("agent {short}")
                } else {
                    a.name.clone()
                };
                (a.agent_id.clone(), name)
            })
            .collect()
    };
    if tracked {
        state.agents.with(collect)
    } else {
        state.agents.with_untracked(collect)
    }
}

/// The deliverable submit target: an explicit picker choice if present,
/// else the sole live same-project candidate when exactly one exists.
/// `None` ⇒ no deliverable target (Submit is gated off; the user must pick).
fn effective_submit_target(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
    submit_target: RwSignal<Option<protocol::ReviewSubmitTarget>>,
    tracked: bool,
) -> Option<protocol::ReviewSubmitTarget> {
    let explicit = if tracked {
        submit_target.get()
    } else {
        submit_target.get_untracked()
    };
    if explicit.is_some() {
        return explicit;
    }
    let candidates = live_same_project_candidates(state, host_id, project_id, tracked);
    if let [(agent_id, _)] = candidates.as_slice() {
        return Some(protocol::ReviewSubmitTarget::ExistingAgent {
            agent_id: agent_id.clone(),
        });
    }
    None
}

fn parse_backend_kind(s: &str) -> Option<BackendKind> {
    match s {
        "Tycode" => Some(BackendKind::Tycode),
        "Kiro" => Some(BackendKind::Kiro),
        "Claude" => Some(BackendKind::Claude),
        "Codex" => Some(BackendKind::Codex),
        "Gemini" => Some(BackendKind::Gemini),
        _ => None,
    }
}

fn parse_cost_hint(s: &str) -> Option<protocol::SpawnCostHint> {
    match s {
        "low" => Some(protocol::SpawnCostHint::Low),
        "medium" => Some(protocol::SpawnCostHint::Medium),
        "high" => Some(protocol::SpawnCostHint::High),
        _ => None,
    }
}

async fn send_review_action_inner(
    host_id: &str,
    review_id: protocol::ReviewId,
    payload: ReviewActionPayload,
) -> Result<(), String> {
    let stream = StreamPath(format!("/review/{}", review_id.0));
    send_frame(host_id, stream, FrameKind::ReviewAction, &payload).await
}

/// Short string describing the current connection state for diagnostics.
/// Format: `conn=<connected|connecting|disconnected|error> has_host_stream=<bool>`
fn conn_diag(state: &AppState, host_id: &str) -> String {
    let status = state
        .connection_statuses
        .with_untracked(|m| m.get(host_id).cloned());
    let label = match status {
        Some(crate::state::ConnectionStatus::Connected) => "connected",
        Some(crate::state::ConnectionStatus::Connecting) => "connecting",
        Some(crate::state::ConnectionStatus::Disconnected) => "disconnected",
        Some(crate::state::ConnectionStatus::Error(_)) => "error",
        None => "unknown",
    };
    let has_stream = state
        .host_streams
        .with_untracked(|m| m.contains_key(host_id));
    format!("conn={label} has_host_stream={has_stream}")
}

/// Fire a `ReviewAction` and clear the corresponding per-target gate on
/// local send failure (so the buttons re-enable). On success the gate
/// remains set until dispatch sees the matching server echo or error.
pub(crate) async fn send_review_action_with_failure_clear(
    state: AppState,
    host_id: &str,
    review_id: protocol::ReviewId,
    payload: ReviewActionPayload,
    target: ReviewActionTarget,
) {
    let target_label = target_label(&target);
    match send_review_action_inner(host_id, review_id.clone(), payload).await {
        Ok(()) => {
            log::info!("review.action.send_ok review={review_id} target={target_label}");
        }
        Err(e) => {
            log::error!(
                "review.action.send_err review={review_id} target={target_label} error={e}"
            );
            state.review_action_target_pending.update(|set| {
                set.remove(&(review_id.clone(), target));
            });
            log::info!(
                "review.action.local_target_gate_clear review={review_id} target={target_label} reason=send_err"
            );
        }
    }
}

fn target_label(target: &ReviewActionTarget) -> &'static str {
    match target {
        ReviewActionTarget::AddComment => "add_comment",
        ReviewActionTarget::UpdateComment(_) => "update_comment",
        ReviewActionTarget::DeleteComment(_) => "delete_comment",
        ReviewActionTarget::AcceptSuggestion(_) => "accept_suggestion",
        ReviewActionTarget::RejectSuggestion(_) => "reject_suggestion",
    }
}

/// Atomic re-entry guard for action handlers. Returns `true` if the
/// caller should proceed (gate was newly set), `false` if a request for
/// this exact `(review_id, target)` is already in flight.
///
/// Synchronous JS event dispatch can fire multiple `on:click` handlers
/// before Leptos flushes the reactive `disabled` attribute to the DOM,
/// so the visual disable alone does not prevent re-entry — handlers
/// must check this guard before sending.
pub(crate) fn try_claim_review_action(
    state: &AppState,
    review_id: &protocol::ReviewId,
    target: &ReviewActionTarget,
) -> bool {
    let mut claimed = false;
    state.review_action_target_pending.update(|set| {
        claimed = set.insert((review_id.clone(), target.clone()));
    });
    log::info!(
        "review.action.claim review={} target={} claimed={}",
        review_id,
        target_label(target),
        claimed
    );
    claimed
}

/// Public entry — used by the agent header's "Review changes" button.
///
/// At most one Draft review per project at a time. If one already
/// exists for the active agent's project we just focus its tab (and
/// open it if it isn't open yet) instead of asking the server for a
/// new one — otherwise the user ends up with a long stack of empty
/// drafts that have to be cancelled one at a time. The server enforces
/// the same invariant with a `Conflict` `CommandError` for any caller
/// that bypasses this check (MCP, an older client).
///
/// Otherwise sends `ReviewCreate` on `/project/<id>` and tags the
/// (host, project) pair as create-pending so the button disables until
/// the server's `ReviewListChanged` echoes back.
pub fn create_review_for_active_agent(state: &AppState) {
    let Some(active_agent) = state.active_agent.get_untracked() else {
        return;
    };
    let host_id = active_agent.host_id.clone();
    let agent_id = active_agent.agent_id.clone();

    let project_id = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == host_id && a.agent_id == agent_id)
            .and_then(|a| a.project_id.clone())
    });
    let Some(project_id) = project_id else {
        log::warn!("create_review_for_active_agent: agent has no project — skipping");
        return;
    };

    let existing_draft = state.review_summaries.with_untracked(|map| {
        map.get(&project_id).and_then(|summaries| {
            summaries
                .iter()
                .filter(|s| matches!(s.status, ReviewStatus::Draft))
                .max_by_key(|s| s.updated_at_ms)
                .map(|s| s.id.clone())
        })
    });
    if let Some(review_id) = existing_draft {
        let label = review_tab_label(&review_id);
        state.open_tab(
            TabContent::Review {
                host_id: host_id.clone(),
                review_id,
            },
            label,
            true,
        );
        return;
    }

    let mut claimed = false;
    state.review_create_pending.update(|map| {
        let key = (host_id.clone(), project_id.clone());
        let entry = map.entry(key).or_insert(0);
        if *entry == 0 {
            *entry = 1;
            claimed = true;
        }
    });
    if !claimed {
        return;
    }

    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = protocol::ReviewCreatePayload {
        selection: protocol::ReviewDiffSelection::AllUncommitted,
    };
    let state_for_failure = state.clone();
    let project_id_for_failure = project_id.clone();
    let host_for_failure = host_id.clone();
    spawn_local(async move {
        if let Err(e) = send_frame(&host_id, stream, FrameKind::ReviewCreate, &payload).await {
            log::error!("failed to send ReviewCreate: {e}");
            state_for_failure.review_create_pending.update(|map| {
                let key = (host_for_failure, project_id_for_failure);
                if let Some(count) = map.get_mut(&key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&key);
                    }
                }
            });
        }
    });
}

/// Returns true when the active agent's project has uncommitted changes
/// (driving the visibility of the "Review changes" header button).
pub fn active_agent_has_uncommitted_changes(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    let project_id = state.agents.with(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .and_then(|a| a.project_id.clone())
    });
    let Some(project_id) = project_id else {
        return false;
    };
    state.git_status.with(|map| {
        map.get(&project_id)
            .is_some_and(|roots| roots.iter().any(|r| !r.clean))
    })
}

/// Whether a `ReviewCreate` is currently in flight for the active agent's
/// project — used to disable the "Review changes" button until the
/// server's `ReviewListChanged` echoes back.
pub fn active_agent_review_create_pending(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    let project_id = state.agents.with(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .and_then(|a| a.project_id.clone())
    });
    let Some(project_id) = project_id else {
        return false;
    };
    state
        .review_create_pending
        .with(|map| map.contains_key(&(active_agent.host_id, project_id)))
}

/// Compute, by project, the Draft review the git panel pill links to.
/// When multiple drafts exist for the same project we surface the most
/// recently-updated one — that's the one the user is most likely to be
/// actively editing. Submitted/Consumed/Cancelled are filtered out — the
/// pill is for actively-editable reviews only.
pub fn open_review_for_active_project(state: &AppState) -> Option<(String, protocol::ReviewId)> {
    let active = state.active_project.get()?;
    let summaries = state
        .review_summaries
        .with(|map| map.get(&active.project_id).cloned())?;
    let latest = summaries
        .into_iter()
        .filter(|s| matches!(s.status, ReviewStatus::Draft))
        .max_by_key(|s| s.updated_at_ms)?;
    Some((active.host_id.clone(), latest.id))
}

/// Project-scoped entry point for the inline review flow, driven from the
/// git panel rather than an agent's chat header. Opens the active
/// project's existing Draft review if one exists; otherwise sends a
/// project-scoped `ReviewCreate` (no `origin_agent_id` — the server
/// derives the project from the `/project/<id>` stream) and tags the
/// (host, project) pair create-pending. The `ReviewListChanged` dispatch
/// handler pairs the new review with that pending tag and auto-opens its
/// tab, so this function does not open the tab itself on the create path.
pub fn create_or_open_review_for_active_project(state: &AppState) {
    let Some(active) = state.active_project.get_untracked() else {
        log::warn!("create_or_open_review_for_active_project: no active project");
        return;
    };
    let host_id = active.host_id.clone();
    let project_id = active.project_id.clone();

    let existing_draft = state.review_summaries.with_untracked(|map| {
        map.get(&project_id).and_then(|summaries| {
            summaries
                .iter()
                .filter(|s| matches!(s.status, ReviewStatus::Draft))
                .max_by_key(|s| s.updated_at_ms)
                .map(|s| s.id.clone())
        })
    });
    if let Some(review_id) = existing_draft {
        let label = review_tab_label(&review_id);
        state.open_tab(TabContent::Review { host_id, review_id }, label, true);
        return;
    }

    let mut claimed = false;
    state.review_create_pending.update(|map| {
        let key = (host_id.clone(), project_id.clone());
        let entry = map.entry(key).or_insert(0);
        if *entry == 0 {
            *entry = 1;
            claimed = true;
        }
    });
    if !claimed {
        return;
    }

    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = protocol::ReviewCreatePayload {
        selection: protocol::ReviewDiffSelection::AllUncommitted,
    };
    let state_for_failure = state.clone();
    let project_id_for_failure = project_id.clone();
    let host_for_failure = host_id.clone();
    spawn_local(async move {
        if let Err(e) = send_frame(&host_id, stream, FrameKind::ReviewCreate, &payload).await {
            log::error!("failed to send project-scoped ReviewCreate: {e}");
            state_for_failure.review_create_pending.update(|map| {
                let key = (host_for_failure, project_id_for_failure);
                if let Some(count) = map.get_mut(&key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&key);
                    }
                }
            });
        }
    });
}

// ── BTreeMap import is referenced by tests below ───────────────────────
#[allow(dead_code)]
fn _btree_marker() -> BTreeMap<u32, u32> {
    BTreeMap::new()
}

// `ReviewActionGate` is referenced via `state.review_action_pending`; touch
// the import so removal of the field fails compilation cleanly.
#[allow(dead_code)]
fn _gate_marker() -> ReviewActionGate {
    ReviewActionGate::default()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AgentInfo;
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, DiffContextMode, ProjectDiffScope, ProjectGitDiffFile,
        ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload,
        ProjectId, ProjectRootPath, Review, ReviewAiReviewerState, ReviewAiReviewerStatus,
        ReviewAnchor, ReviewAnchorStatus, ReviewComment, ReviewCommentId, ReviewCommentSource,
        ReviewDiffSelection, ReviewDiffSide, ReviewId, ReviewLocation, ReviewSeverity,
        ReviewStatus, ReviewSuggestedComment, ReviewSuggestionId, ReviewSuggestionState, SessionId,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

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
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 1100px; height: 800px;",
            )
            .unwrap();
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

    fn root_path() -> ProjectRootPath {
        ProjectRootPath("/repo".to_owned())
    }

    fn diff_payload() -> ProjectGitDiffPayload {
        ProjectGitDiffPayload {
            root: root_path(),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: vec![
                ProjectGitDiffFile {
                    relative_path: "src/foo.rs".to_owned(),
                    hunks: vec![ProjectGitDiffHunk {
                        hunk_id: "src/foo.rs:1".to_owned(),
                        old_start: 1,
                        old_count: 1,
                        new_start: 1,
                        new_count: 5,
                        lines: vec![
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Context,
                                text: "fn handle()".to_owned(),
                                old_line_number: Some(1),
                                new_line_number: Some(1),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let x = 1;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(2),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let y = 2;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(3),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let z = 3;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(4),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let w = 4;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(5),
                            },
                        ],
                    }],
                },
                ProjectGitDiffFile {
                    relative_path: "src/bar.rs".to_owned(),
                    hunks: vec![ProjectGitDiffHunk {
                        hunk_id: "src/bar.rs:1".to_owned(),
                        old_start: 1,
                        old_count: 1,
                        new_start: 1,
                        new_count: 2,
                        lines: vec![
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Context,
                                text: "fn other()".to_owned(),
                                old_line_number: Some(1),
                                new_line_number: Some(1),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    bar();".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(2),
                            },
                        ],
                    }],
                },
            ],
        }
    }

    fn make_review() -> Review {
        Review {
            id: ReviewId("rev-1".to_owned()),
            project_id: ProjectId("proj-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("sess-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![diff_payload()],
            comments: vec![],
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

    fn make_agent(name: &str, agent_id: &str, project: Option<&str>) -> AgentInfo {
        AgentInfo {
            host_id: "h1".to_owned(),
            agent_id: AgentId(agent_id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Tycode,
            workspace_roots: vec![],
            project_id: project.map(|s| ProjectId(s.to_owned())),
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath("s".to_owned()),
            started: true,
            fatal_error: None,
        }
    }

    fn comment_at_line(line: u32, body: &str) -> ReviewComment {
        ReviewComment {
            id: ReviewCommentId(format!("c-{line}-{body}")),
            location: ReviewLocation {
                root: root_path(),
                relative_path: "src/foo.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: line,
                    end_line: line,
                },
            },
            anchor_status: ReviewAnchorStatus::Current,
            body: body.to_owned(),
            source: ReviewCommentSource::User,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn comment_range(start: u32, end: u32, body: &str) -> ReviewComment {
        ReviewComment {
            id: ReviewCommentId(format!("c-{start}-{end}-{body}")),
            location: ReviewLocation {
                root: root_path(),
                relative_path: "src/foo.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: start,
                    end_line: end,
                },
            },
            anchor_status: ReviewAnchorStatus::Current,
            body: body.to_owned(),
            source: ReviewCommentSource::User,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn pending_suggestion_at_line(line: u32, body: &str) -> ReviewSuggestedComment {
        ReviewSuggestedComment {
            id: ReviewSuggestionId(format!("s-{line}")),
            location: ReviewLocation {
                root: root_path(),
                relative_path: "src/foo.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: line,
                    end_line: line,
                },
            },
            anchor_status: ReviewAnchorStatus::Current,
            body: body.to_owned(),
            rationale: None,
            severity: ReviewSeverity::Warn,
            state: ReviewSuggestionState::Pending,
            reviewer_agent_id: AgentId("ai-1".to_owned()),
            created_at_ms: 1,
        }
    }

    /// Mounts the review view with the given review pre-seeded.
    /// Returns the captured `AppState` so the test can drive signal
    /// updates that mirror dispatch events. The mount handle is leaked
    /// so the view stays alive for the duration of the test (otherwise
    /// dropping the handle unmounts the view immediately).
    fn mount_review(
        container: HtmlElement,
        review: Review,
    ) -> std::rc::Rc<std::cell::RefCell<Option<AppState>>> {
        let state_holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_holder_for_mount = state_holder.clone();
        let host_id = "h1".to_owned();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.reviews.update(|map| {
                map.insert(review.id.clone(), review.clone());
            });
            *state_holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ReviewView host_id=host_id.clone() review_id=review.id.clone() /> }
        });
        std::mem::forget(handle);
        state_holder
    }

    /// A user-anchored comment renders as visible body text under the
    /// row that displays its target line. We assert on the
    /// user-perceived effect: the comment body appears in the DOM,
    /// and the file containing it shows exactly one user-comment
    /// count badge.
    #[wasm_bindgen_test]
    async fn renders_thread_at_correct_line() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        review.comments.push(comment_at_line(2, "explain"));
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("explain"),
            "expected comment body 'explain' in DOM; got: {text}"
        );
        // One user-count badge rendered (the file has one comment).
        let badges = container.query_selector_all("[data-count-user]").unwrap();
        assert_eq!(
            badges.length(),
            1,
            "expected exactly one user count badge in file list"
        );
    }

    /// Pending AI suggestions render Accept / Edit & Accept / Reject
    /// affordances. We assert by looking up buttons by their visible
    /// text instead of class names — text is what the user reads.
    #[wasm_bindgen_test]
    async fn pending_suggestion_shows_affordances() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        review
            .suggestions
            .push(pending_suggestion_at_line(2, "consider an enum"));
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("consider an enum"),
            "suggestion body missing; got: {text}"
        );
        assert!(text.contains("Accept"), "Accept button text missing");
        assert!(
            text.contains("Edit & Accept"),
            "Edit & Accept button text missing"
        );
        assert!(text.contains("Reject"), "Reject button text missing");
    }

    /// A comment or suggestion whose anchor the server flagged `Stale`
    /// renders a visible stale marker (with the reason) so the user knows
    /// it may point at code that has since moved. Mirrors the mobile stale
    /// pill, which already renders this.
    #[wasm_bindgen_test]
    async fn stale_anchor_renders_badge() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        let mut comment = comment_at_line(2, "this moved");
        comment.anchor_status = ReviewAnchorStatus::Stale {
            reason: "line shifted".to_owned(),
        };
        review.comments.push(comment);
        let mut suggestion = pending_suggestion_at_line(3, "stale suggestion");
        suggestion.anchor_status = ReviewAnchorStatus::Stale {
            reason: "hunk changed".to_owned(),
        };
        review.suggestions.push(suggestion);
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let badges = container
            .query_selector_all("[data-test=\"review-anchor-stale\"]")
            .unwrap();
        assert_eq!(
            badges.length(),
            2,
            "expected a stale badge on both the comment and the suggestion"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("line shifted"),
            "stale comment reason must be surfaced; got: {text}"
        );
        assert!(
            text.contains("hunk changed"),
            "stale suggestion reason must be surfaced; got: {text}"
        );
    }

    /// Submit reflects whether the user can submit AND has a deliverable
    /// target. Under the explicit-target protocol a comment alone is not
    /// sufficient: there must be a chosen target, or exactly one live
    /// same-project candidate to auto-select.
    ///
    /// NOTE: this updates the pre-explicit-target assertion that submit
    /// enabled on a comment alone. That behavior relied on falling back to
    /// the review's origin agent, which is now a synthetic, non-deliverable
    /// origin for project-scoped reviews — so the old assertion tested
    /// behavior that would produce a failing submit and is obsolete.
    #[wasm_bindgen_test]
    async fn submit_button_disabled_until_draft_with_comments() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            submit.has_attribute("disabled"),
            "submit must be disabled with zero comments"
        );

        // Add a comment — necessary but NOT sufficient: with no live
        // same-project agent there is no deliverable target, so submit
        // stays disabled.
        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "fix this"));
            }
        });
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            submit.has_attribute("disabled"),
            "submit must stay disabled with a comment but no deliverable target"
        );

        // Seed exactly one live same-project agent — it auto-selects as the
        // target, so submit now enables.
        state.agents.update(|agents| {
            agents.push(make_agent("Only Candidate", "a-only", Some("proj-1")));
        });
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            !submit.has_attribute("disabled"),
            "submit must enable with a comment and exactly one live same-project agent"
        );

        // Flip the review to Submitted — submit must disable again.
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.status = ReviewStatus::Submitted {
                    submitted_at_ms: 999,
                };
            }
        });
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            submit.has_attribute("disabled"),
            "submit must disable when status leaves Draft"
        );
    }

    /// The submit-target picker offers only same-project agents as
    /// explicit destinations — an agent belonging to a different project
    /// must never appear, since feedback can only route within the
    /// review's own project.
    #[wasm_bindgen_test]
    async fn submit_target_picker_lists_only_same_project_agents() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review(); // project_id = "proj-1", host = "h1"
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // Seed one same-project agent and one from a different project.
        let state = state_holder.borrow().clone().unwrap();
        state.agents.update(|agents| {
            agents.push(make_agent("Backend Codex", "a-same", Some("proj-1")));
            agents.push(make_agent("Other Project", "a-other", Some("proj-2")));
        });
        next_tick().await;

        let select = container
            .query_selector("[data-test=\"review-submit-target\"]")
            .unwrap()
            .expect("submit target picker rendered");
        let text = select.text_content().unwrap_or_default();
        assert!(
            text.contains("Auto (single same-project agent)"),
            "default auto-target option missing; got: {text}"
        );
        assert!(
            text.contains("Backend Codex"),
            "same-project agent must be offered as a target; got: {text}"
        );
        assert!(
            !text.contains("Other Project"),
            "an agent from a different project must not be offered; got: {text}"
        );
    }

    /// The Clear control is gated on there being something to clear: it
    /// is disabled when the review has no comments or suggestions, and
    /// enables once a comment exists.
    #[wasm_bindgen_test]
    async fn clear_button_enables_with_content() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let clear =
            find_button_by_text(&container, "Clear comments").expect("clear button rendered");
        assert!(
            clear.has_attribute("disabled"),
            "clear must be disabled when there is nothing to clear"
        );

        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "please fix"));
            }
        });
        next_tick().await;

        let clear =
            find_button_by_text(&container, "Clear comments").expect("clear button rendered");
        assert!(
            !clear.has_attribute("disabled"),
            "clear must enable once the review has a comment"
        );
    }

    /// Sidebar count derives reactively from the comments/suggestions
    /// signal — the visible text reflects the live counts.
    #[wasm_bindgen_test]
    async fn sidebar_counts_derive_reactively() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let counts = container
            .query_selector("[data-test=\"review-counts\"]")
            .unwrap()
            .expect("counts panel mounted");
        let counts_el: HtmlElement = counts.dyn_into().unwrap();
        assert!(
            counts_el
                .text_content()
                .unwrap_or_default()
                .contains("0 comments"),
            "expected initial '0 comments', got: {}",
            counts_el.text_content().unwrap_or_default()
        );

        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "first"));
                r.comments.push(comment_at_line(3, "second"));
                r.suggestions.push(pending_suggestion_at_line(2, "ai"));
            }
        });
        next_tick().await;

        let counts = container
            .query_selector("[data-test=\"review-counts\"]")
            .unwrap()
            .expect("counts panel mounted");
        let counts_el: HtmlElement = counts.dyn_into().unwrap();
        let txt = counts_el.text_content().unwrap_or_default();
        assert!(
            txt.contains("2 comments"),
            "expected '2 comments' in: {txt}"
        );
        assert!(
            txt.contains("1 pending AI"),
            "expected '1 pending AI' in: {txt}"
        );
    }

    /// CRITICAL #4: Composer captures the file at click time. If the
    /// user opens the composer on src/foo.rs, then switches the visible
    /// file to src/bar.rs, the composer must NOT render under the new
    /// file (its location stays anchored to the originally clicked
    /// file). This is the regression we're guarding against — without
    /// the location-aware filter, the composer textarea would attach to
    /// whatever file the user happened to be viewing when they clicked
    /// Save, and the comment would be saved against the wrong file.
    #[wasm_bindgen_test]
    async fn composer_anchors_to_originally_clicked_file() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // Click the file-level "+" button on the visible file (src/foo.rs).
        let foo_plus = container
            .query_selector(".review-file-comment-btn")
            .unwrap()
            .expect("file-level + button rendered");
        let foo_plus: HtmlElement = foo_plus.dyn_into().unwrap();
        foo_plus.click();
        next_tick().await;

        // Composer textarea is present (currently anchored to foo.rs).
        assert!(
            container
                .query_selector(".review-composer textarea")
                .unwrap()
                .is_some(),
            "composer textarea should render after clicking + on foo.rs"
        );

        // Switch file to src/bar.rs by clicking its row in the file list.
        let bar_row =
            find_button_by_text(&container, "src/bar.rs").expect("bar.rs file row rendered");
        bar_row.click();
        next_tick().await;
        next_tick().await;

        // The composer must NOT render under the bar.rs view — its
        // location stayed anchored to foo.rs.
        let center = container
            .query_selector(".review-center")
            .unwrap()
            .expect("center pane rendered");
        let center_el: HtmlElement = center.dyn_into().unwrap();
        assert!(
            center_el
                .query_selector(".review-composer")
                .unwrap()
                .is_none(),
            "composer must not render under bar.rs after switching files"
        );
    }

    /// CRITICAL #5: Mutation controls become disabled when the review's
    /// status flips from Draft to Submitted. Drive the flip via signal
    /// mutation (mirroring what dispatch does on StatusChanged) and
    /// assert that user-perceived button states change.
    #[wasm_bindgen_test]
    async fn mutation_controls_disable_when_not_draft() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        review.comments.push(comment_at_line(2, "fix this"));
        review
            .suggestions
            .push(pending_suggestion_at_line(3, "use enum"));
        let review_id = review.id.clone();
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // While Draft: composer-trigger + button is enabled, Edit /
        // Delete / Accept / Reject are enabled.
        let plus_btn = container
            .query_selector(".review-file-comment-btn")
            .unwrap()
            .expect("file + button rendered");
        let plus_btn: HtmlElement = plus_btn.dyn_into().unwrap();
        assert!(
            !plus_btn.has_attribute("disabled"),
            "+ button should be enabled in Draft"
        );
        let edit_btn = find_button_by_text(&container, "Edit").expect("edit button rendered");
        assert!(
            !edit_btn.has_attribute("disabled"),
            "Edit must be enabled in Draft"
        );
        let delete_btn = find_button_by_text(&container, "Delete").expect("delete button rendered");
        assert!(
            !delete_btn.has_attribute("disabled"),
            "Delete must be enabled in Draft"
        );
        let accept_btn = find_button_by_text(&container, "Accept").expect("accept button rendered");
        assert!(
            !accept_btn.has_attribute("disabled"),
            "Accept must be enabled in Draft"
        );
        let reject_btn = find_button_by_text(&container, "Reject").expect("reject button rendered");
        assert!(
            !reject_btn.has_attribute("disabled"),
            "Reject must be enabled in Draft"
        );

        // Flip to Submitted via signal mutation.
        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.status = ReviewStatus::Submitted { submitted_at_ms: 1 };
            }
        });
        next_tick().await;

        // All mutation controls are now disabled.
        let plus_btn = container
            .query_selector(".review-file-comment-btn")
            .unwrap()
            .expect("file + button rendered");
        let plus_btn: HtmlElement = plus_btn.dyn_into().unwrap();
        assert!(
            plus_btn.has_attribute("disabled"),
            "+ button must disable when status leaves Draft"
        );
        let edit_btn = find_button_by_text(&container, "Edit").expect("edit button rendered");
        assert!(
            edit_btn.has_attribute("disabled"),
            "Edit must disable when status leaves Draft"
        );
        let delete_btn = find_button_by_text(&container, "Delete").expect("delete button rendered");
        assert!(
            delete_btn.has_attribute("disabled"),
            "Delete must disable when status leaves Draft"
        );
        let accept_btn = find_button_by_text(&container, "Accept").expect("accept button rendered");
        assert!(
            accept_btn.has_attribute("disabled"),
            "Accept must disable when status leaves Draft"
        );
        let reject_btn = find_button_by_text(&container, "Reject").expect("reject button rendered");
        assert!(
            reject_btn.has_attribute("disabled"),
            "Reject must disable when status leaves Draft"
        );
    }

    /// IMPORTANT #6: A multi-line LineRange comment renders exactly
    /// once — anchored at the bottom (end_line) of the range, not
    /// duplicated under every line in between.
    #[wasm_bindgen_test]
    async fn multiline_linerange_renders_once() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        // Range spans new-side lines 2..=4 inclusive.
        review.comments.push(comment_range(2, 4, "range note"));
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;
        next_tick().await;
        next_tick().await;

        // Sanity: the diff itself rendered (we should see the file path
        // in the file-list rail and the diff text in the center).
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("let z = 3"),
            "diff content not rendered yet — got: {}",
            &text[..text.len().min(400)]
        );
        let occurrences = text.matches("range note").count();
        assert_eq!(
            occurrences, 1,
            "multi-line comment 'range note' should render exactly once, found {occurrences}"
        );
    }

    /// IMPORTANT #7: Run AI button is disabled until a backend is
    /// chosen. We assert via the `disabled` attribute, which is
    /// user-perceived.
    #[wasm_bindgen_test]
    async fn run_ai_disabled_until_backend_chosen() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let run_btn =
            find_button_by_text(&container, "Run AI reviewer").expect("run AI button rendered");
        assert!(
            run_btn.has_attribute("disabled"),
            "Run AI must be disabled before any backend is chosen"
        );
    }

    /// VISUAL LAYOUT: The review workbench is a three-pane surface. This
    /// geometry guard catches regressions where the side rails stack,
    /// overlap the diff, or stop filling the available height.
    #[wasm_bindgen_test]
    async fn lays_out_file_diff_and_sidebar_as_three_panes() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let root = container.get_bounding_client_rect();
        let header = element_by_selector(&container, ".review-header").get_bounding_client_rect();
        let panes =
            element_by_selector(&container, ".review-three-pane").get_bounding_client_rect();
        let file_list =
            element_by_selector(&container, ".review-file-list").get_bounding_client_rect();
        let center = element_by_selector(&container, ".review-center").get_bounding_client_rect();
        let sidebar = element_by_selector(&container, ".review-sidebar").get_bounding_client_rect();

        assert_close(
            header.top(),
            root.top(),
            1.0,
            "review header should start at the top of the mounted surface",
        );
        assert_close(
            panes.top(),
            header.bottom(),
            1.0,
            "pane grid should sit immediately below the review header",
        );
        assert_close(
            panes.bottom(),
            root.bottom(),
            1.0,
            "pane grid should fill the remaining vertical space",
        );

        for (name, rect) in [
            ("file list", &file_list),
            ("center diff", &center),
            ("sidebar", &sidebar),
        ] {
            assert_close(
                rect.top(),
                panes.top(),
                1.0,
                &format!("{name} pane should align with the grid top"),
            );
            assert_close(
                rect.bottom(),
                panes.bottom(),
                1.0,
                &format!("{name} pane should fill the grid height"),
            );
        }

        assert_close(
            file_list.left(),
            panes.left(),
            1.0,
            "file list should be the leftmost pane",
        );
        assert_close(
            center.left(),
            file_list.right(),
            1.0,
            "center diff should start where the file list ends",
        );
        assert_close(
            sidebar.left(),
            center.right(),
            1.0,
            "sidebar should start where the center diff ends",
        );
        assert_close(
            sidebar.right(),
            panes.right(),
            1.0,
            "sidebar should end at the right edge of the pane grid",
        );

        assert!(
            (130.0..=190.0).contains(&file_list.width()),
            "file list should render as a compact left rail, got {:.2}px",
            file_list.width()
        );
        assert!(
            (170.0..=230.0).contains(&sidebar.width()),
            "sidebar should render as a compact right rail, got {:.2}px",
            sidebar.width()
        );
        assert!(
            center.width() > file_list.width() * 2.0 && center.width() > sidebar.width() * 2.0,
            "center diff pane should get the primary width; file={:.2}px center={:.2}px \
             sidebar={:.2}px",
            file_list.width(),
            center.width(),
            sidebar.width()
        );
    }

    /// A diff payload with enough rendered lines to require vertical
    /// scrolling inside `.diff-content` (container is 800px tall in
    /// `make_container`, so 100 lines × ~16px easily overflows).
    fn large_diff_payload() -> ProjectGitDiffPayload {
        let mut lines = vec![ProjectGitDiffLine {
            kind: ProjectGitDiffLineKind::Context,
            text: "fn handle()".to_owned(),
            old_line_number: Some(1),
            new_line_number: Some(1),
        }];
        for i in 0..150 {
            lines.push(ProjectGitDiffLine {
                kind: ProjectGitDiffLineKind::Added,
                text: format!("    let var_{i} = {i};"),
                old_line_number: None,
                new_line_number: Some((i as u32) + 2),
            });
        }
        let new_count = lines.len() as u32;
        ProjectGitDiffPayload {
            root: root_path(),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: vec![ProjectGitDiffFile {
                relative_path: "src/foo.rs".to_owned(),
                hunks: vec![ProjectGitDiffHunk {
                    hunk_id: "src/foo.rs:1".to_owned(),
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count,
                    lines,
                }],
            }],
        }
    }

    fn review_with_large_diff() -> Review {
        let mut review = make_review();
        review.diffs = vec![large_diff_payload()];
        review
    }

    /// A diff payload whose one Added line is wider than the visible
    /// review-center pane, so the diff forces horizontal overflow
    /// inside `.diff-content`. Used for the comment-width-bound test —
    /// without the scrollport-width clamp, the sticky thread region
    /// would inherit the wider intrinsic width and overflow the pane.
    fn wide_line_diff_payload() -> ProjectGitDiffPayload {
        let very_long = "x".repeat(800);
        ProjectGitDiffPayload {
            root: root_path(),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: vec![ProjectGitDiffFile {
                relative_path: "src/foo.rs".to_owned(),
                hunks: vec![ProjectGitDiffHunk {
                    hunk_id: "src/foo.rs:1".to_owned(),
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 2,
                    lines: vec![
                        ProjectGitDiffLine {
                            kind: ProjectGitDiffLineKind::Context,
                            text: "fn handle()".to_owned(),
                            old_line_number: Some(1),
                            new_line_number: Some(1),
                        },
                        ProjectGitDiffLine {
                            kind: ProjectGitDiffLineKind::Added,
                            text: format!("    let very_long_var = \"{very_long}\";"),
                            old_line_number: None,
                            new_line_number: Some(2),
                        },
                    ],
                }],
            }],
        }
    }

    /// BUG #1 GUARD: Adding a review comment must not remount the diff
    /// scrollport. We assert two user-perceived invariants:
    ///   * `.diff-content` keeps the same DOM node identity across the
    ///     comment add (a remount would replace the element).
    ///   * `scrollTop` is preserved.
    ///
    /// We drive the new comment by mutating the live AppState review
    /// signal — that's the same code path dispatch hits when it
    /// receives a `ReviewEvent::CommentUpsert` envelope, so this
    /// matches what production wiring does. Before the fix the
    /// top-level `match review_signal.get()` rebuilt `<ReviewBody>` on
    /// every record change, which dropped the diff scroll position.
    #[wasm_bindgen_test]
    async fn adding_comment_preserves_scroll_and_node_identity() {
        ensure_styles_loaded();
        let container = make_container();
        let review = review_with_large_diff();
        let review_id = review.id.clone();
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // Capture the diff scroll container, scroll it, and remember
        // both its DOM node identity and its scroll offset.
        let diff_before = container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff-content scroll container present");
        let diff_html_before: HtmlElement = diff_before.clone().dyn_into().unwrap();
        diff_html_before.set_scroll_top(500);
        // Sanity: container actually scrolled (the diff is tall enough
        // — if this asserts false the fixture's diff isn't large
        // enough to scroll past 500px and the test is meaningless).
        assert!(
            diff_html_before.scroll_top() > 0,
            "diff-content failed to scroll; layout did not produce a scrollable height"
        );
        let scroll_top_before = diff_html_before.scroll_top();

        // Drive a CommentUpsert through the live AppState signal —
        // mirrors what `dispatch_review_event` does on the wire.
        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "preserve scroll"));
            }
        });
        next_tick().await;
        next_tick().await;

        // Same DOM node, scroll position not lost.
        let diff_after = container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff-content still present after comment upsert");
        let diff_node_before: &web_sys::Node = diff_before.as_ref();
        let diff_node_after: &web_sys::Node = diff_after.as_ref();
        assert!(
            diff_node_before.is_same_node(Some(diff_node_after)),
            ".diff-content was remounted by the CommentUpsert — \
             scroll position would be lost on every comment add"
        );
        let diff_html_after: HtmlElement = diff_after.dyn_into().unwrap();
        let scroll_top_after = diff_html_after.scroll_top();
        // The regression we're guarding against is a remount that
        // resets scrollTop to 0. Browser scroll-anchoring may shift
        // scrollTop slightly when virtualized rows above the viewport
        // grow (e.g. a freshly rendered comment card under an
        // upstream line), but it must not lose the user's place.
        // Allow a generous downward tolerance — anything close to
        // zero (i.e., a full remount) is the bug.
        assert!(
            scroll_top_after >= scroll_top_before - 50,
            ".diff-content scroll position was lost after comment upsert; \
             expected ≥ {} (within tolerance of {scroll_top_before}), got {scroll_top_after}",
            scroll_top_before - 50
        );
        assert!(
            scroll_top_after > 0,
            ".diff-content scrollTop reset to top after comment upsert — \
             the diff scrollport was remounted, dropping the user's place"
        );
    }

    /// BUG #2 GUARD: a comment's rendered card must not require
    /// horizontal scrolling beyond the visible review center pane.
    /// Even when a diff line is wider than the pane and forces
    /// horizontal overflow inside `.diff-content`, the sticky thread
    /// region's width must be clamped to the visible scrollport.
    #[wasm_bindgen_test]
    async fn comment_width_bounded_by_visible_pane() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        review.diffs = vec![wide_line_diff_payload()];
        review.comments.push(comment_at_line(
            2,
            &format!(
                "{} long comment body that would, absent the width \
                 clamp, stretch to match the diff line below it.",
                "extremely ".repeat(40)
            ),
        ));
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;
        next_tick().await;

        let diff_content = container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff-content scroll container present");
        let diff_html: HtmlElement = diff_content.clone().dyn_into().unwrap();
        let viewport_width = diff_html.client_width() as f64;
        assert!(
            viewport_width > 0.0,
            "diff-content has zero client_width; layout did not run"
        );

        let card = container
            .query_selector(".review-comment-card")
            .unwrap()
            .expect("comment card rendered");
        let card_rect = card.get_bounding_client_rect();
        let diff_rect = diff_html.get_bounding_client_rect();

        // Card width must be inside the visible viewport (give a 1px
        // rounding tolerance — sub-pixel layout can produce a ~0.5px
        // delta on some headless renderers).
        assert!(
            card_rect.width() <= viewport_width + 1.0,
            "review comment card width {:.2}px exceeds diff viewport \
             {:.2}px — the comment will require horizontal scrolling",
            card_rect.width(),
            viewport_width
        );
        // And its right edge must not extend past the diff pane's
        // visible right edge (i.e., the comment is on-screen, not
        // hidden behind the horizontal-scroll overflow).
        assert!(
            card_rect.right() <= diff_rect.right() + 1.0,
            "comment right edge {:.2}px exceeds diff right edge {:.2}px \
             — comment is hidden off-screen until user scrolls right",
            card_rect.right(),
            diff_rect.right()
        );
    }

    /// BUG #2 GUARD (SBS): in side-by-side mode, review comment
    /// decorations render *inside* one of the `.diff-pane` columns.
    /// Without a per-pane scrollport observer, the comment card
    /// inherits `--diff-scrollport-width` from `.diff-content`, which
    /// is wider than the pane, and the comment overflows the visible
    /// pane. The fix attaches a `ResizeObserver` to each `.diff-pane`
    /// that publishes its own `client_width` into the same CSS var —
    /// the cascade then picks the pane's narrower value for elements
    /// inside the pane.
    #[wasm_bindgen_test]
    async fn sbs_comment_width_bounded_by_pane() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        review.diffs = vec![wide_line_diff_payload()];
        // New-side comment, so the thread region renders inside the
        // right pane in SBS.
        review.comments.push(comment_at_line(
            2,
            &format!(
                "{} long comment body that would, without the pane \
                 clamp, stretch to the diff-content scrollport width.",
                "extremely ".repeat(40)
            ),
        ));
        let state_holder = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // Flip to side-by-side mode through the AppState signal —
        // mirrors what the `ReviewLayoutToggle` button does on click.
        let state = state_holder.borrow().clone().unwrap();
        state
            .diff_view_mode
            .set(crate::state::DiffViewMode::SideBySide);

        next_tick().await;
        next_tick().await;
        next_tick().await;

        // Right pane is the New-side column where the comment lands.
        let right_pane = container
            .query_selector(".diff-pane-right")
            .unwrap()
            .expect("right pane present after switching to SBS");
        let right_pane_html: HtmlElement = right_pane.clone().dyn_into().unwrap();
        let pane_width = right_pane_html.client_width() as f64;
        assert!(
            pane_width > 0.0,
            "right pane has zero client width; SBS layout did not run"
        );

        // The per-pane ResizeObserver must publish the pane's width
        // — otherwise the CSS clamp falls through to whatever the
        // outer `.diff-content` published (which is too wide).
        let pane_var = right_pane_html
            .style()
            .get_property_value("--diff-scrollport-width")
            .unwrap_or_default();
        assert!(
            !pane_var.is_empty() && pane_var.ends_with("px"),
            "right pane missing --diff-scrollport-width inline style; \
             got '{pane_var}'"
        );

        // Diff content is wider than the pane (the wide diff line
        // forces it that way), so this comparison really exercises
        // the pane-vs-content distinction. If diff_width <= pane_width
        // the test is meaningless.
        let diff_content = container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff-content present");
        let diff_content_html: HtmlElement = diff_content.dyn_into().unwrap();
        let diff_width = diff_content_html.client_width() as f64;
        assert!(
            diff_width >= pane_width,
            "diff content width {diff_width:.2}px should be at least \
             the pane width {pane_width:.2}px"
        );

        let card = right_pane
            .query_selector(".review-comment-card")
            .unwrap()
            .expect("comment card rendered inside right SBS pane");
        let card_rect = card.get_bounding_client_rect();
        let pane_rect = right_pane_html.get_bounding_client_rect();

        assert!(
            card_rect.width() <= pane_width + 1.0,
            "SBS comment card width {:.2}px exceeds right-pane width \
             {:.2}px — comment overflows the visible pane",
            card_rect.width(),
            pane_width
        );
        assert!(
            card_rect.right() <= pane_rect.right() + 1.0,
            "SBS comment right edge {:.2}px exceeds right-pane right \
             edge {:.2}px — comment is hidden past the pane boundary",
            card_rect.right(),
            pane_rect.right()
        );
    }

    /// BUG #2 GUARD (wiring): the `ResizeObserver` in `DiffContent`
    /// must publish `--diff-scrollport-width` as an inline style on
    /// `.diff-content` after mount. The CSS rules for the sticky
    /// thread region depend on this var being set, so an unset var
    /// would silently regress the comment width clamp.
    #[wasm_bindgen_test]
    async fn diff_content_publishes_scrollport_width_css_var() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let _ = mount_review(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let diff_content = container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff-content present");
        let diff_html: HtmlElement = diff_content.dyn_into().unwrap();
        let var_value = diff_html
            .style()
            .get_property_value("--diff-scrollport-width")
            .unwrap_or_default();
        assert!(
            !var_value.is_empty(),
            "--diff-scrollport-width inline style is empty; ResizeObserver \
             in DiffContent did not seed the CSS var. Got style.cssText='{}'",
            diff_html.style().css_text()
        );
        // Value should be a px length matching client_width — sanity
        // check that we're publishing a width, not some other unit.
        assert!(
            var_value.ends_with("px"),
            "--diff-scrollport-width should be a px length, got '{var_value}'"
        );
    }

    /// Helper: locate a button whose visible text contains `needle`.
    /// Used so assertions live in user-perceived land (visible text)
    /// rather than internal class names.
    fn find_button_by_text(root: &HtmlElement, needle: &str) -> Option<HtmlElement> {
        let buttons = root.query_selector_all("button").ok()?;
        for i in 0..buttons.length() {
            let btn = buttons.item(i)?;
            let el: HtmlElement = btn.dyn_into().ok()?;
            if el.text_content().unwrap_or_default().contains(needle) {
                return Some(el);
            }
        }
        None
    }

    fn element_by_selector(root: &HtmlElement, selector: &str) -> HtmlElement {
        root.query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("expected selector {selector} to match"))
            .dyn_into()
            .unwrap()
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64, message: &str) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{message}: expected {expected:.2}px, got {actual:.2}px"
        );
    }

    #[allow(dead_code)]
    fn _silence_unused(_: &Element) {}
}
