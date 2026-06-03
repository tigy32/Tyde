//! Reusable desktop review-decoration layer.
//!
//! These building blocks turn any `DiffView` into a review surface: a
//! click/drag line-range selection that opens an inline composer, inline
//! comment/suggestion thread regions under the lines they anchor to, and a
//! file-level comment affordance in the file header. They were originally
//! defined inline in `review_view` (the standalone three-pane workbench);
//! they live here now so the *normal* git-diff tabs can reuse the exact
//! same decorations against the project's draft review instead of routing
//! the user into a separate workbench.
//!
//! `review_view` re-exports `ComposerState` for back-compat and calls
//! `build_review_decorations` to wire its center pane; the normal diff tab
//! (`diff_view::ReviewableDiffView`) calls the same helper.

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::components::diff_view::{
    DecorationFileHeaderFn, DecorationLineFn, GutterActionFileHeaderFn, GutterPointerDownFn,
    LineExtraClassFn,
};
use crate::components::inline_review::ThreadRegionFiltered;
use crate::state::AppState;

use protocol::{ProjectRootPath, ReviewAnchor, ReviewDiffSide, ReviewLocation};

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

/// Live state of an in-progress click+drag line-range selection on the
/// diff gutter. Pure UI state — never sent on the wire. Anchored to a
/// specific (root, file, side) so a drag that wanders onto another file
/// or the opposite side stays clamped to the start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DragSelection {
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

/// The five review-mode decoration callbacks `DiffView` accepts. Bundled
/// so both the standalone workbench and the normal diff tab build them the
/// same way and spread them into `DiffView`'s props.
#[derive(Clone)]
pub(crate) struct ReviewDecorationFns {
    pub(crate) gutter_pointer_down: GutterPointerDownFn,
    pub(crate) line_extra_class: LineExtraClassFn,
    pub(crate) gutter_action_for_file_header: GutterActionFileHeaderFn,
    pub(crate) decoration_below_line: DecorationLineFn,
    pub(crate) decoration_below_file_header: DecorationFileHeaderFn,
}

/// Build all five decoration callbacks for a given draft review. The
/// caller owns the `composer` / `drag_selection` signals (so the workbench
/// can decorate its file-list rows from the same composer signal) and is
/// responsible for calling [`install_drag_listeners`] once on mount.
pub(crate) fn build_review_decorations(
    composer: RwSignal<Option<ComposerState>>,
    drag_selection: RwSignal<Option<DragSelection>>,
    review_id: protocol::ReviewId,
    host_id: String,
    is_draft: Memo<bool>,
) -> ReviewDecorationFns {
    ReviewDecorationFns {
        gutter_pointer_down: make_gutter_pointer_down(drag_selection, is_draft),
        line_extra_class: make_line_extra_class(drag_selection),
        gutter_action_for_file_header: make_gutter_action_for_file_header(composer, is_draft),
        decoration_below_line: make_line_decoration(
            composer,
            review_id.clone(),
            host_id.clone(),
            is_draft,
        ),
        decoration_below_file_header: make_file_header_decoration(
            composer, review_id, host_id, is_draft,
        ),
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
pub(crate) fn make_gutter_pointer_down(
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
pub(crate) fn make_line_extra_class(
    drag_selection: RwSignal<Option<DragSelection>>,
) -> LineExtraClassFn {
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
/// active drag selection. Attaches once when the review surface mounts and
/// removes them on cleanup so the listeners don't outlive it.
///
/// Why window-level (not on `.diff-content`): rows mount/unmount under
/// virtualization while the user is mid-drag. Listening on the diff
/// container would lose events when the originating row scrolls out of
/// view. Window listeners survive that.
pub(crate) fn install_drag_listeners(
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
pub(crate) fn make_line_decoration(
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
pub(crate) fn make_gutter_action_for_file_header(
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
pub(crate) fn make_file_header_decoration(
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
