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
use crate::state::{
    ActiveAgentRef, AppState, ReviewActionGate, ReviewActionTarget, TabContent, root_display_name,
};

use protocol::{
    BackendKind, FrameKind, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffPayload,
    ProjectRootPath, Review, ReviewActionPayload, ReviewAiReviewerStatus, ReviewAnchor,
    ReviewComment, ReviewCommentSource, ReviewDiffSide, ReviewLocation, ReviewStatus,
    ReviewSuggestedComment, ReviewSuggestionState, StreamPath,
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
struct ComposerState {
    location: ReviewLocation,
    body: RwSignal<String>,
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
        if !already_present {
            spawn_local(async move {
                let stream = StreamPath(format!("/review/{}", review_for_sub.0));
                if let Err(e) = send_frame(
                    &host_for_sub,
                    stream,
                    FrameKind::ReviewSubscribe,
                    &serde_json::json!({}),
                )
                .await
                {
                    log::error!("failed to send ReviewSubscribe for review {review_for_sub}: {e}");
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

    view! {
        <div class="review-view" data-review-id={review_id.0.clone()}>
            {move || {
                match review_signal.get() {
                    None => view! {
                        <div class="review-loading">
                            <p class="placeholder-text">"Loading review\u{2026}"</p>
                        </div>
                    }.into_any(),
                    Some(review) => view! {
                        <ReviewBody
                            review=review
                            selected=selected
                            host_id=host_id_for_view.clone()
                            review_id=review_id_for_view.clone()
                        />
                    }.into_any(),
                }
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
        move || -> String {
            let agent_id = live
                .get()
                .map(|r| r.origin_agent_id)
                .unwrap_or_else(|| initial.clone());
            let name = agent_state.agents.with(|agents| {
                agents
                    .iter()
                    .find(|a| a.agent_id == agent_id)
                    .map(|a| a.name.clone())
            });
            match name {
                Some(name) if !name.is_empty() => format!("Origin: {name}"),
                _ => format!("Origin: {}", short_agent_id(&agent_id.0)),
            }
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
                <span class="review-origin">{move || header_origin()}</span>
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
fn format_relative_time(timestamp_ms: u64) -> String {
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
fn autosize_textarea(node_ref: NodeRef<leptos::html::Textarea>, body: RwSignal<String>) {
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
                let exists = state.reviews.with(|map| {
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
fn thread_region_has_content(
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

/// Generic thread region. Renders all comments + pending suggestions (and
/// the inline composer when its location matches) whose `(root,
/// relative_path)` matches and whose anchor satisfies the matcher.
#[component]
fn ThreadRegionFiltered(
    review_id: protocol::ReviewId,
    root: ProjectRootPath,
    relative_path: String,
    host_id: String,
    composer: RwSignal<Option<ComposerState>>,
    matcher: std::sync::Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync>,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let comments_review_id = review_id.clone();
    let cm_root = root.clone();
    let cm_path = relative_path.clone();
    let cm_matcher = matcher.clone();
    let comments: Memo<Vec<ReviewComment>> = Memo::new(move |_| {
        state.reviews.with(|map| {
            let Some(review) = map.get(&comments_review_id) else {
                return Vec::new();
            };
            review
                .comments
                .iter()
                .filter(|c| {
                    c.location.root == cm_root
                        && c.location.relative_path == cm_path
                        && cm_matcher(&c.location.anchor)
                })
                .cloned()
                .collect()
        })
    });

    let suggestions_review_id = review_id.clone();
    let sg_root = root.clone();
    let sg_path = relative_path.clone();
    let sg_matcher = matcher.clone();
    let suggestions: Memo<Vec<ReviewSuggestedComment>> = Memo::new(move |_| {
        state.reviews.with(|map| {
            let Some(review) = map.get(&suggestions_review_id) else {
                return Vec::new();
            };
            review
                .suggestions
                .iter()
                .filter(|s| {
                    s.location.root == sg_root
                        && s.location.relative_path == sg_path
                        && sg_matcher(&s.location.anchor)
                })
                .cloned()
                .collect()
        })
    });

    // Composer matcher: open ⇒ render here if the composer's location
    // (root + relative_path + anchor) matches this thread region.
    let composer_matcher = matcher.clone();
    let composer_root = root.clone();
    let composer_path = relative_path.clone();
    let composer_visible = move || -> bool {
        let Some(c) = composer.get() else {
            return false;
        };
        c.location.root == composer_root
            && c.location.relative_path == composer_path
            && composer_matcher(&c.location.anchor)
    };

    let composer_host = host_id.clone();
    let composer_review = review_id.clone();

    let comment_card_review_id = review_id.clone();
    let comment_card_host = host_id.clone();
    let suggestion_card_review_id = review_id.clone();
    let suggestion_card_host = host_id.clone();

    // Rejected-suggestions toggle for this region.
    let rejected_open: RwSignal<bool> = RwSignal::new(false);
    let sg_rejected_review_id = review_id.clone();
    let sg_rejected_root = root.clone();
    let sg_rejected_path = relative_path.clone();
    let sg_rejected_matcher = matcher.clone();
    let rejected_suggestions: Memo<Vec<ReviewSuggestedComment>> = Memo::new(move |_| {
        state.reviews.with(|map| {
            let Some(review) = map.get(&sg_rejected_review_id) else {
                return Vec::new();
            };
            review
                .suggestions
                .iter()
                .filter(|s| {
                    matches!(s.state, ReviewSuggestionState::Rejected)
                        && s.location.root == sg_rejected_root
                        && s.location.relative_path == sg_rejected_path
                        && sg_rejected_matcher(&s.location.anchor)
                })
                .cloned()
                .collect()
        })
    });

    view! {
        <div class="review-thread-region" data-rel-path={relative_path.clone()}>
            {move || {
                let card_review = comment_card_review_id.clone();
                let card_host = comment_card_host.clone();
                comments.get().into_iter().map(|c| {
                    view! {
                        <CommentCard
                            comment=c
                            host_id=card_host.clone()
                            review_id=card_review.clone()
                            is_draft=is_draft
                        />
                    }
                }).collect::<Vec<_>>()
            }}
            {move || {
                let sg_review = suggestion_card_review_id.clone();
                let sg_host = suggestion_card_host.clone();
                suggestions.get().into_iter()
                    .filter(|s| matches!(s.state, ReviewSuggestionState::Pending))
                    .map(|s| {
                        view! {
                            <SuggestionCard
                                suggestion=s
                                host_id=sg_host.clone()
                                review_id=sg_review.clone()
                                is_draft=is_draft
                            />
                        }
                    }).collect::<Vec<_>>()
            }}
            {move || {
                let rejected = rejected_suggestions.get();
                if rejected.is_empty() {
                    return None;
                }
                let count = rejected.len();
                let open = rejected_open.get();
                let toggle_label = if open {
                    format!("Hide {count} rejected")
                } else {
                    format!("{count} rejected")
                };
                let rejected_clone = rejected.clone();
                Some(view! {
                    <div class="review-rejected-region">
                        <button
                            class="review-rejected-toggle"
                            on:click=move |_| rejected_open.update(|v| *v = !*v)
                        >
                            {toggle_label}
                        </button>
                        {open.then(|| view! {
                            <div class="review-rejected-list">
                                {rejected_clone.into_iter().map(|s| view! {
                                    <RejectedSuggestionCard suggestion=s />
                                }).collect::<Vec<_>>()}
                            </div>
                        })}
                    </div>
                })
            }}
            {move || {
                if !composer_visible() {
                    return None;
                }
                let composer_state = composer.get()?;
                let composer_host = composer_host.clone();
                let composer_review = composer_review.clone();
                Some(view! {
                    <Composer
                        composer=composer
                        composer_state=composer_state
                        host_id=composer_host
                        review_id=composer_review
                        is_draft=is_draft
                    />
                })
            }}
        </div>
    }
}

#[component]
fn CommentCard(
    comment: ReviewComment,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let pill = match &comment.source {
        ReviewCommentSource::User => view! {
            <span class="review-source-pill user">"You"</span>
        }
        .into_any(),
        ReviewCommentSource::AiSuggestion { edited, .. } => {
            let label = if *edited { "AI (edited)" } else { "AI" };
            view! { <span class="review-source-pill ai">{label}</span> }.into_any()
        }
    };
    let body_for_view = comment.body.clone();
    let comment_id = comment.id.clone();
    let comment_created_at = comment.created_at_ms;
    let comment_updated_at = comment.updated_at_ms;

    let is_user = matches!(comment.source, ReviewCommentSource::User);

    let edit_open = RwSignal::new(false);
    let edit_value = RwSignal::new(comment.body.clone());

    // Reactive pending flags for the per-row gates.
    let update_pending = {
        let rid = review_id.clone();
        let cid = comment_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(rid.clone(), ReviewActionTarget::UpdateComment(cid.clone())))
            })
        }
    };
    let delete_pending = {
        let rid = review_id.clone();
        let cid = comment_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(rid.clone(), ReviewActionTarget::DeleteComment(cid.clone())))
            })
        }
    };

    // Effect: close the edit form when an UpdateComment echo lands —
    // i.e. when the comment's body matches the edited value AND the
    // gate has dropped (was set, now not). Simplest reliable detection:
    // when the gate transitions from set → unset, snap the form shut
    // unless body still doesn't match (Error path keeps form open).
    {
        let rid = review_id.clone();
        let cid = comment_id.clone();
        let s = state.clone();
        let was_pending = RwSignal::new(false);
        Effect::new(move |_| {
            let pending_now = s.review_action_target_pending.with(|set| {
                set.contains(&(rid.clone(), ReviewActionTarget::UpdateComment(cid.clone())))
            });
            let prev = was_pending.get_untracked();
            was_pending.set(pending_now);
            if prev && !pending_now {
                // Gate cleared. Determine whether the server accepted
                // the edit by comparing the typed value to the live
                // comment body. On accept they match; on error the
                // live body is unchanged and the typed value still
                // differs.
                let typed = edit_value.get_untracked();
                let live_body = s.reviews.with_untracked(|map| {
                    map.get(&rid).and_then(|r| {
                        r.comments
                            .iter()
                            .find(|c| c.id == cid)
                            .map(|c| c.body.clone())
                    })
                });
                if live_body.as_deref() == Some(typed.as_str()) {
                    edit_open.set(false);
                }
            }
        });
    }

    let comment_id_for_save = comment_id.clone();
    let host_id_for_save = host_id.clone();
    let review_id_for_save = review_id.clone();
    let state_for_save = state.clone();
    let do_save_edit: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        let body = edit_value.get_untracked().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let rid = review_id_for_save.clone();
        let cid = comment_id_for_save.clone();
        if !try_claim_review_action(
            &state_for_save,
            &rid,
            &ReviewActionTarget::UpdateComment(cid.clone()),
        ) {
            return;
        }
        let host = host_id_for_save.clone();
        let target_state = state_for_save.clone();
        let target_rid = rid.clone();
        let target_cid = cid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::UpdateComment {
                    comment_id: target_cid.clone(),
                    body,
                },
                ReviewActionTarget::UpdateComment(target_cid),
            )
            .await;
        });
    });

    let host_id_for_delete = host_id.clone();
    let review_id_for_delete = review_id.clone();
    let state_for_delete = state.clone();
    let comment_id_for_del = comment_id.clone();
    let comment_body_for_del = comment.body.clone();
    let on_delete = move |_| {
        let rid = review_id_for_delete.clone();
        let cid = comment_id_for_del.clone();
        let host = host_id_for_delete.clone();
        let target_state = state_for_delete.clone();
        let body_preview = {
            let trimmed = comment_body_for_del.trim();
            if trimmed.len() > 80 {
                format!("{}\u{2026}", &trimmed[..80])
            } else {
                trimmed.to_owned()
            }
        };
        spawn_local(async move {
            let message = if body_preview.is_empty() {
                "Delete this comment?".to_owned()
            } else {
                format!("Delete this comment?\n\n\u{201c}{body_preview}\u{201d}")
            };
            if !crate::bridge::confirm_dialog("Delete comment", &message).await {
                return;
            }
            if !try_claim_review_action(
                &target_state,
                &rid,
                &ReviewActionTarget::DeleteComment(cid.clone()),
            ) {
                return;
            }
            let target_rid = rid.clone();
            let target_cid = cid.clone();
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::DeleteComment {
                    comment_id: target_cid.clone(),
                },
                ReviewActionTarget::DeleteComment(target_cid),
            )
            .await;
        });
    };

    let do_save_edit_for_btn = do_save_edit.clone();
    let on_delete_for_btn = on_delete.clone();
    let comment_body_for_edit_open = comment.body.clone();
    // Wrap in Rc so independent view closures can each hold their own
    // cloneable handle without moving ownership.
    let edit_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let update_pending = update_pending.clone();
        move || !is_draft.get() || update_pending()
    });
    let delete_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let delete_pending = delete_pending.clone();
        move || !is_draft.get() || delete_pending()
    });

    let edit_disabled_for_edit_block = edit_disabled.clone();
    let edit_disabled_for_actions = edit_disabled.clone();
    let delete_disabled_for_actions = delete_disabled.clone();

    view! {
        <div class="review-comment-card" data-comment-id={comment.id.0.clone()}
             data-source={match &comment.source {
                 ReviewCommentSource::User => "user",
                 ReviewCommentSource::AiSuggestion { .. } => "ai",
             }}>
            <div class="review-comment-header">
                {pill}
                {(comment_created_at > 0).then(|| {
                    let edited = comment_updated_at > comment_created_at;
                    let title = if edited {
                        format!(
                            "Created {}, edited {}",
                            format_relative_time(comment_created_at),
                            format_relative_time(comment_updated_at)
                        )
                    } else {
                        format!("Created {}", format_relative_time(comment_created_at))
                    };
                    let stamp = format_relative_time(
                        if edited { comment_updated_at } else { comment_created_at }
                    );
                    let label = if edited { format!("{stamp} (edited)") } else { stamp };
                    view! {
                        <span class="review-comment-time" title={title}>{label}</span>
                    }
                })}
            </div>
            {move || {
                if edit_open.get() {
                    let save_disabled = edit_disabled_for_edit_block.clone();
                    let save_disabled_for_keydown = save_disabled.clone();
                    let do_save = do_save_edit_for_btn.clone();
                    let do_save_for_keydown = do_save.clone();
                    let edit_textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                    Effect::new(move |_| {
                        if let Some(el) = edit_textarea_ref.get() {
                            let _ = el.focus();
                            // Caret to end so the user can keep typing
                            // without having to navigate manually.
                            let len = el.value().len() as u32;
                            let _ = el.set_selection_range(len, len);
                        }
                    });
                    autosize_textarea(edit_textarea_ref, edit_value);
                    view! {
                        <div class="review-comment-edit">
                            <textarea
                                node_ref=edit_textarea_ref
                                class="review-textarea"
                                prop:value=move || edit_value.get()
                                on:input=move |ev| edit_value.set(event_target_value(&ev))
                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                    if ev.key() == "Escape" {
                                        ev.prevent_default();
                                        edit_open.set(false);
                                        return;
                                    }
                                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key())
                                        && !save_disabled_for_keydown()
                                    {
                                        ev.prevent_default();
                                        do_save_for_keydown();
                                    }
                                }
                            />
                            <div class="review-comment-actions">
                                <button class="review-btn primary review-comment-edit-save"
                                        disabled=move || save_disabled()
                                        on:click=move |_| do_save()>
                                    "Save"
                                </button>
                                <button
                                    class="review-btn"
                                    on:click={
                                        let original = body_for_view.clone();
                                        move |_| {
                                            let typed = edit_value.get_untracked();
                                            if typed == original {
                                                edit_open.set(false);
                                                return;
                                            }
                                            spawn_local(async move {
                                                if crate::bridge::confirm_dialog(
                                                    "Discard edit",
                                                    "You have unsaved changes. Discard them?",
                                                ).await {
                                                    edit_open.set(false);
                                                }
                                            });
                                        }
                                    }
                                >
                                    "Cancel"
                                </button>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    let body_html = body_for_view.clone();
                    view! {
                        <div class="review-comment-body"
                             inner_html=crate::markdown::render_markdown(&body_html)>
                        </div>
                    }.into_any()
                }
            }}
            {is_user.then(|| {
                let on_delete = on_delete_for_btn.clone();
                let edit_disabled = edit_disabled_for_actions.clone();
                let delete_disabled = delete_disabled_for_actions.clone();
                view! {
                    <div class="review-comment-actions">
                        <button class="review-btn review-edit-btn"
                                disabled=move || edit_disabled()
                                on:click=move |_| {
                                    edit_value.set(comment_body_for_edit_open.clone());
                                    edit_open.set(true);
                                }>"Edit"</button>
                        <button class="review-btn destructive review-delete-btn"
                                disabled=move || delete_disabled()
                                on:click=on_delete>"Delete"</button>
                    </div>
                }
            })}
        </div>
    }
}

#[component]
fn SuggestionCard(
    suggestion: ReviewSuggestedComment,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let edit_open = RwSignal::new(false);
    let edit_value = RwSignal::new(suggestion.body.clone());
    let body_for_view = suggestion.body.clone();
    let suggestion_id = suggestion.id.clone();

    let accept_pending = {
        let rid = review_id.clone();
        let sid = suggestion_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(
                    rid.clone(),
                    ReviewActionTarget::AcceptSuggestion(sid.clone()),
                ))
            })
        }
    };
    let reject_pending = {
        let rid = review_id.clone();
        let sid = suggestion_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(
                    rid.clone(),
                    ReviewActionTarget::RejectSuggestion(sid.clone()),
                ))
            })
        }
    };

    // Effect: close the edit form once AcceptSuggestion gate clears AND
    // the suggestion has transitioned to Accepted (success path). On
    // error the suggestion stays Pending, leaving the form open.
    {
        let rid = review_id.clone();
        let sid = suggestion_id.clone();
        let s = state.clone();
        let was_pending = RwSignal::new(false);
        Effect::new(move |_| {
            let pending_now = s.review_action_target_pending.with(|set| {
                set.contains(&(
                    rid.clone(),
                    ReviewActionTarget::AcceptSuggestion(sid.clone()),
                ))
            });
            let prev = was_pending.get_untracked();
            was_pending.set(pending_now);
            if prev && !pending_now {
                let accepted = s.reviews.with_untracked(|map| {
                    map.get(&rid)
                        .and_then(|r| r.suggestions.iter().find(|sg| sg.id == sid))
                        .map(|sg| matches!(sg.state, ReviewSuggestionState::Accepted { .. }))
                        .unwrap_or(false)
                });
                if accepted {
                    edit_open.set(false);
                }
            }
        });
    }

    let host_for_accept = host_id.clone();
    let review_for_accept = review_id.clone();
    let suggestion_for_accept = suggestion_id.clone();
    let state_for_accept = state.clone();
    let on_accept = move |_| {
        let rid = review_for_accept.clone();
        let sid = suggestion_for_accept.clone();
        if !try_claim_review_action(
            &state_for_accept,
            &rid,
            &ReviewActionTarget::AcceptSuggestion(sid.clone()),
        ) {
            return;
        }
        let host = host_for_accept.clone();
        let target_state = state_for_accept.clone();
        let target_rid = rid.clone();
        let target_sid = sid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::AcceptSuggestion {
                    suggestion_id: target_sid.clone(),
                    edit: None,
                },
                ReviewActionTarget::AcceptSuggestion(target_sid),
            )
            .await;
        });
    };

    let host_for_edit_accept = host_id.clone();
    let review_for_edit_accept = review_id.clone();
    let suggestion_for_edit_accept = suggestion_id.clone();
    let state_for_edit_accept = state.clone();
    let do_edit_accept: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        let body = edit_value.get_untracked().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let rid = review_for_edit_accept.clone();
        let sid = suggestion_for_edit_accept.clone();
        if !try_claim_review_action(
            &state_for_edit_accept,
            &rid,
            &ReviewActionTarget::AcceptSuggestion(sid.clone()),
        ) {
            return;
        }
        let host = host_for_edit_accept.clone();
        let target_state = state_for_edit_accept.clone();
        let target_rid = rid.clone();
        let target_sid = sid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::AcceptSuggestion {
                    suggestion_id: target_sid.clone(),
                    edit: Some(body),
                },
                ReviewActionTarget::AcceptSuggestion(target_sid),
            )
            .await;
        });
    });

    let host_for_reject = host_id.clone();
    let review_for_reject = review_id.clone();
    let suggestion_for_reject = suggestion_id.clone();
    let state_for_reject = state.clone();
    let on_reject = move |_| {
        let rid = review_for_reject.clone();
        let sid = suggestion_for_reject.clone();
        if !try_claim_review_action(
            &state_for_reject,
            &rid,
            &ReviewActionTarget::RejectSuggestion(sid.clone()),
        ) {
            return;
        }
        let host = host_for_reject.clone();
        let target_state = state_for_reject.clone();
        let target_rid = rid.clone();
        let target_sid = sid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::RejectSuggestion {
                    suggestion_id: target_sid.clone(),
                },
                ReviewActionTarget::RejectSuggestion(target_sid),
            )
            .await;
        });
    };

    let severity_class = match suggestion.severity {
        protocol::ReviewSeverity::Info => "review-severity info",
        protocol::ReviewSeverity::Warn => "review-severity warn",
        protocol::ReviewSeverity::Bug => "review-severity bug",
    };

    let accept_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let accept_pending = accept_pending.clone();
        move || !is_draft.get() || accept_pending()
    });
    let reject_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let reject_pending = reject_pending.clone();
        move || !is_draft.get() || reject_pending()
    });
    let edit_accept_save_disabled = accept_disabled.clone();

    view! {
        <div class="review-suggestion-card pending"
             data-suggestion-id={suggestion.id.0.clone()}
             data-state="pending">
            <div class="review-comment-header">
                <span class="review-source-pill ai-pending">"AI suggestion"</span>
                <span class={severity_class}>{format!("{:?}", suggestion.severity)}</span>
            </div>
            {move || {
                if edit_open.get() {
                    let do_save = do_edit_accept.clone();
                    let do_save_for_keydown = do_save.clone();
                    let save_disabled = edit_accept_save_disabled.clone();
                    let save_disabled_for_keydown = save_disabled.clone();
                    let edit_textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                    Effect::new(move |_| {
                        if let Some(el) = edit_textarea_ref.get() {
                            let _ = el.focus();
                            let len = el.value().len() as u32;
                            let _ = el.set_selection_range(len, len);
                        }
                    });
                    autosize_textarea(edit_textarea_ref, edit_value);
                    view! {
                        <div class="review-comment-edit">
                            <textarea
                                node_ref=edit_textarea_ref
                                class="review-textarea"
                                prop:value=move || edit_value.get()
                                on:input=move |ev| edit_value.set(event_target_value(&ev))
                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                    if ev.key() == "Escape" {
                                        ev.prevent_default();
                                        edit_open.set(false);
                                        return;
                                    }
                                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key())
                                        && !save_disabled_for_keydown()
                                    {
                                        ev.prevent_default();
                                        do_save_for_keydown();
                                    }
                                }
                            />
                            <div class="review-comment-actions">
                                <button class="review-btn primary review-edit-accept-save"
                                        disabled=move || save_disabled()
                                        on:click=move |_| do_save()>
                                    "Save & Accept"
                                </button>
                                <button class="review-btn" on:click={
                                    let original = body_for_view.clone();
                                    move |_| {
                                        let typed = edit_value.get_untracked();
                                        if typed == original {
                                            edit_open.set(false);
                                            return;
                                        }
                                        spawn_local(async move {
                                            if crate::bridge::confirm_dialog(
                                                "Discard edit",
                                                "You have unsaved changes. Discard them?",
                                            ).await {
                                                edit_open.set(false);
                                            }
                                        });
                                    }
                                }>
                                    "Cancel"
                                </button>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    let body_html = body_for_view.clone();
                    view! {
                        <div class="review-comment-body"
                             inner_html=crate::markdown::render_markdown(&body_html)>
                        </div>
                    }.into_any()
                }
            }}
            {move || {
                if edit_open.get() {
                    view! { <span></span> }.into_any()
                } else {
                    let on_accept = on_accept.clone();
                    let on_reject = on_reject.clone();
                    let accept_disabled = accept_disabled.clone();
                    let reject_disabled = reject_disabled.clone();
                    view! {
                        <div class="review-comment-actions">
                            <button class="review-btn primary review-accept-btn"
                                    disabled=move || accept_disabled()
                                    on:click=on_accept>
                                "Accept"
                            </button>
                            <button class="review-btn review-edit-accept-btn"
                                    disabled=move || !is_draft.get()
                                    on:click=move |_| edit_open.set(true)>
                                "Edit & Accept"
                            </button>
                            <button class="review-btn destructive review-reject-btn"
                                    disabled=move || reject_disabled()
                                    on:click=on_reject>
                                "Reject"
                            </button>
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

/// Read-only card for rejected AI suggestions, displayed under the
/// "show N rejected" toggle. No accept/reject buttons — the suggestion
/// is already terminal.
#[component]
fn RejectedSuggestionCard(suggestion: ReviewSuggestedComment) -> impl IntoView {
    let body = suggestion.body.clone();
    let severity_class = match suggestion.severity {
        protocol::ReviewSeverity::Info => "review-severity info",
        protocol::ReviewSeverity::Warn => "review-severity warn",
        protocol::ReviewSeverity::Bug => "review-severity bug",
    };
    view! {
        <div class="review-suggestion-card rejected"
             data-suggestion-id={suggestion.id.0.clone()}
             data-state="rejected">
            <div class="review-comment-header">
                <span class="review-source-pill ai-rejected">"AI (rejected)"</span>
                <span class={severity_class}>{format!("{:?}", suggestion.severity)}</span>
            </div>
            <div class="review-comment-body"
                 inner_html=crate::markdown::render_markdown(&body)>
            </div>
        </div>
    }
}

#[component]
fn Composer(
    composer: RwSignal<Option<ComposerState>>,
    composer_state: ComposerState,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let body = composer_state.body;
    let location = composer_state.location.clone();
    let host_for_send = host_id.clone();
    let review_for_send = review_id.clone();
    let location_for_send = location.clone();
    let state_for_send = state.clone();

    // Reactive pending flag. While set, Save button is disabled and
    // composer stays open.
    let rid_for_pending = review_id.clone();
    let pending_state = state.clone();
    let pending = move || {
        pending_state
            .review_action_target_pending
            .with(|set| set.contains(&(rid_for_pending.clone(), ReviewActionTarget::AddComment)))
    };

    // Effect: when an AddComment echo lands, the dispatch handler
    // clears the gate AND the new User comment with our location is
    // present in the live review record. Detect that exact transition
    // and close the composer. On error the gate clears without a
    // matching new comment ⇒ keep composer open with body intact.
    {
        let rid = review_id.clone();
        let s = state.clone();
        let target_location = location.clone();
        let was_pending = RwSignal::new(false);
        Effect::new(move |_| {
            let pending_now = s
                .review_action_target_pending
                .with(|set| set.contains(&(rid.clone(), ReviewActionTarget::AddComment)));
            let prev = was_pending.get_untracked();
            was_pending.set(pending_now);
            if prev && !pending_now {
                let echoed = s.reviews.with_untracked(|map| {
                    map.get(&rid)
                        .map(|r| {
                            r.comments.iter().any(|c| {
                                matches!(c.source, ReviewCommentSource::User)
                                    && c.location == target_location
                            })
                        })
                        .unwrap_or(false)
                });
                if echoed {
                    composer.set(None);
                }
            }
        });
    }

    let do_save: std::sync::Arc<dyn Fn() + Send + Sync> = {
        let host_for_send = host_for_send.clone();
        let review_for_send = review_for_send.clone();
        let location_for_send = location_for_send.clone();
        let state_for_send = state_for_send.clone();
        let body_for_save = body;
        std::sync::Arc::new(move || {
            let body_text = body_for_save.get_untracked().trim().to_owned();
            if body_text.is_empty() {
                return;
            }
            let rid = review_for_send.clone();
            if !try_claim_review_action(&state_for_send, &rid, &ReviewActionTarget::AddComment) {
                return;
            }
            let host = host_for_send.clone();
            let location = location_for_send.clone();
            let target_state = state_for_send.clone();
            let target_rid = rid.clone();
            spawn_local(async move {
                send_review_action_with_failure_clear(
                    target_state,
                    &host,
                    target_rid,
                    ReviewActionPayload::AddComment {
                        location,
                        body: body_text,
                    },
                    ReviewActionTarget::AddComment,
                )
                .await;
            });
        })
    };
    let do_save_for_btn = do_save.clone();
    let do_save_for_keydown = do_save.clone();

    let save_disabled = {
        let pending = pending.clone();
        move || !is_draft.get() || pending()
    };
    let save_disabled_for_keydown = save_disabled.clone();

    let textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(el) = textarea_ref.get() {
            let _ = el.focus();
        }
    });
    autosize_textarea(textarea_ref, body);

    view! {
        <div class="review-composer">
            <textarea
                node_ref=textarea_ref
                class="review-textarea"
                placeholder="Comment\u{2026} (\u{2318}/Ctrl+Enter to save, Esc to cancel)"
                prop:value=move || body.get()
                on:input=move |ev| body.set(event_target_value(&ev))
                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                    if ev.key() == "Escape" {
                        ev.prevent_default();
                        composer.set(None);
                        return;
                    }
                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) {
                        ev.prevent_default();
                        if !save_disabled_for_keydown() {
                            do_save_for_keydown();
                        }
                    }
                }
            />
            <div class="review-composer-actions">
                <button class="review-btn primary review-composer-save"
                        disabled=save_disabled
                        on:click=move |_| do_save_for_btn()>
                    "Save"
                </button>
                <button class="review-btn review-composer-cancel"
                        on:click=move |_| {
                            let body_now = body.get_untracked();
                            if body_now.trim().is_empty() {
                                composer.set(None);
                                return;
                            }
                            spawn_local(async move {
                                if crate::bridge::confirm_dialog(
                                    "Discard comment",
                                    "You have unsaved text. Discard it?",
                                ).await {
                                    composer.set(None);
                                }
                            });
                        }>
                    "Cancel"
                </button>
            </div>
        </div>
    }
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

    // Returns either an empty string (button enabled) or a short reason
    // string suitable for binding to `title` so hovering a disabled
    // button explains why.
    let submit_reason = {
        let action_pending = action_pending.clone();
        let live = live_for_ai;
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
    let on_submit = move |_| {
        let host = host_for_submit.clone();
        let rid = review_for_submit.clone();
        let mut claimed = false;
        state_for_submit.review_action_pending.update(|map| {
            let gate = map.entry(rid.clone()).or_default();
            if !gate.submit {
                gate.submit = true;
                claimed = true;
            }
        });
        if !claimed {
            return;
        }
        let target_state = state_for_submit.clone();
        let target_rid = rid.clone();
        spawn_local(async move {
            if let Err(e) =
                send_review_action_inner(&host, target_rid.clone(), ReviewActionPayload::Submit)
                    .await
            {
                log::error!("ReviewAction Submit local send failed for {target_rid}: {e}");
                target_state.review_action_pending.update(|map| {
                    if let Some(gate) = map.get_mut(&target_rid) {
                        gate.submit = false;
                        if gate.is_idle() {
                            map.remove(&target_rid);
                        }
                    }
                });
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
                return;
            }
            let mut claimed = false;
            target_state.review_action_pending.update(|map| {
                let gate = map.entry(rid.clone()).or_default();
                if !gate.cancel {
                    gate.cancel = true;
                    claimed = true;
                }
            });
            if !claimed {
                return;
            }
            let target_rid = rid.clone();
            if let Err(e) =
                send_review_action_inner(&host, target_rid.clone(), ReviewActionPayload::Cancel)
                    .await
            {
                log::error!("ReviewAction Cancel local send failed for {target_rid}: {e}");
                target_state.review_action_pending.update(|map| {
                    if let Some(gate) = map.get_mut(&target_rid) {
                        gate.cancel = false;
                        if gate.is_idle() {
                            map.remove(&target_rid);
                        }
                    }
                });
            }
        });
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
    let on_run_ai = move |_| {
        let Some(backend) = backend_pick.get_untracked() else {
            return;
        };
        let host = host_for_ai.clone();
        let rid = review_for_ai.clone();
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
        let mut claimed = false;
        state_for_ai.review_action_pending.update(|map| {
            let gate = map.entry(rid.clone()).or_default();
            if !gate.start_ai {
                gate.start_ai = true;
                claimed = true;
            }
        });
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
            if let Err(e) = send_review_action_inner(&host, target_rid.clone(), payload).await {
                log::error!("ReviewAction StartAiReview local send failed for {target_rid}: {e}");
                target_state.review_action_pending.update(|map| {
                    if let Some(gate) = map.get_mut(&target_rid) {
                        gate.start_ai = false;
                        if gate.is_idle() {
                            map.remove(&target_rid);
                        }
                    }
                });
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
                TabContent::Chat {
                    agent_ref: Some(ActiveAgentRef {
                        host_id: host.clone(),
                        agent_id,
                    }),
                },
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
                <button
                    class="review-btn primary review-submit-btn"
                    disabled=submit_disabled
                    title=submit_reason
                    on:click=on_submit
                >
                    "Submit review"
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

/// Fire a `ReviewAction` and clear the corresponding per-target gate on
/// local send failure (so the buttons re-enable). On success the gate
/// remains set until dispatch sees the matching server echo or error.
async fn send_review_action_with_failure_clear(
    state: AppState,
    host_id: &str,
    review_id: protocol::ReviewId,
    payload: ReviewActionPayload,
    target: ReviewActionTarget,
) {
    if let Err(e) = send_review_action_inner(host_id, review_id.clone(), payload).await {
        log::error!("ReviewAction local send failed for {review_id}: {e}");
        state.review_action_target_pending.update(|set| {
            set.remove(&(review_id, target));
        });
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
fn try_claim_review_action(
    state: &AppState,
    review_id: &protocol::ReviewId,
    target: &ReviewActionTarget,
) -> bool {
    let mut claimed = false;
    state.review_action_target_pending.update(|set| {
        claimed = set.insert((review_id.clone(), target.clone()));
    });
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
        origin_agent_id: agent_id,
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
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, DiffContextMode, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk,
        ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload, ProjectId,
        ProjectRootPath, Review, ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewAnchor,
        ReviewComment, ReviewCommentId, ReviewCommentSource, ReviewDiffSelection, ReviewDiffSide,
        ReviewId, ReviewLocation, ReviewSeverity, ReviewStatus, ReviewSuggestedComment,
        ReviewSuggestionId, ReviewSuggestionState, SessionId,
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

    /// Submit button reflects whether the user can submit. Assert on
    /// the `disabled` attribute (user-perceived state) by finding the
    /// button by its visible label.
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

        // Add a comment via signal mutation — mirrors what dispatch does
        // on `CommentUpsert`. The submit button must re-enable.
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
            !submit.has_attribute("disabled"),
            "submit must enable once Draft has at least one comment"
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
