use std::cell::RefCell;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::spawn_local;

use crate::code_intel_dom::{
    TimeoutClosureSlot, caret_at_point, clear_timeout_timer, is_identifier_byte,
    line_byte_for_utf16_col, node_to_element, utf16_col_in_code,
};
use crate::components::code_intel_ui::{
    CodeIntelContextMenu, CodeIntelMenuState, CodeIntelMenuTarget,
};
use crate::components::find_bar::{FindBar, FindState, render_text_with_highlights};
use crate::components::review_layer::{build_review_decorations, install_drag_listeners};
use crate::line_source::FileLines;
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, DiffViewState, FileResourceKey, TabId, TabScrollState};
use crate::syntax_highlight::{
    LineHighlighter, LineTokens, color_to_css, compute_hunk_tokens, syntax_for_path,
};

use protocol::{
    DiffContextMode, FrameKind, ProjectDiffScope, ProjectFileVersion, ProjectGitDiffFile,
    ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload,
    ProjectId, ProjectPath, ProjectReadDiffPayload, ProjectRootPath, ProjectStageHunkPayload,
    ReviewDiffSide, StreamPath,
};

/// Map a path's extension to a syntect language token (`"rs"`, `"ts"`,
/// etc.) the highlight worker can resolve. Empty string means "let the
/// worker fall back to path-based detection".
fn syntax_token_for_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => "rs",
        "ts" | "tsx" => "ts",
        "js" | "jsx" => "js",
        "py" => "py",
        "go" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "cc" | "cxx" | "h" | "hpp" => "cpp",
        "css" => "css",
        "html" => "html",
        "json" => "json",
        "md" => "md",
        "sh" | "bash" => "sh",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        _ => return None,
    })
}

/// Main-thread fallback used when the highlight worker can't be
/// instantiated (wasm-bindgen-test, worker init failure). Tokenizes in
/// 50-line chunks, yielding to the browser between each so input
/// events / first paint don't get starved. Writes into per-line signals
/// so re-renders are bounded to the lines that actually changed.
async fn run_fallback_diff_highlight(
    syntax: &'static syntect::parsing::SyntaxReference,
    lines: Vec<String>,
    signals: Vec<ArcRwSignal<Option<LineTokens>>>,
    perf_key: String,
    task_t0: f64,
) {
    next_macrotask().await;
    let mut hl = LineHighlighter::new(syntax);
    let mut first_chunk_logged = false;
    const CHUNK: usize = 50;
    let mut i = 0usize;
    while i < lines.len() {
        let end = (i + CHUNK).min(lines.len());
        for (j, line) in lines.iter().enumerate().take(end).skip(i) {
            let toks = hl.highlight_one(line);
            if let Some(sig) = signals.get(j) {
                sig.set(Some(toks));
            }
        }
        if !first_chunk_logged {
            first_chunk_logged = true;
            let dt = crate::perf::now_ms() - task_t0;
            crate::perf::log_phase(
                "diff_open",
                "hl_first_chunk",
                &perf_key,
                &format!(" through={end} took={dt:.1}ms via=fallback"),
            );
        }
        i = end;
        next_macrotask().await;
    }
    let dt = crate::perf::now_ms() - task_t0;
    crate::perf::log_phase(
        "diff_open",
        "hl_finished",
        &perf_key,
        &format!(" lines={} took={dt:.1}ms via=fallback", lines.len()),
    );
}

/// Yield to the browser event loop. Used to chunk expensive synchronous
/// work (syntax highlighting, in particular) so the main thread doesn't
/// freeze on large diffs.
async fn next_macrotask() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Reactive view of per-hunk syntax tokens for the unified renderer.
/// One `ArcRwSignal` per line — when a chunk lands, only the affected
/// line signals fire, so re-renders are bounded to the lines that
/// actually changed instead of every visible row tracking a single
/// shared signal. With the prior shared-signal design, a 200-line chunk
/// landing while 100 rows were on screen triggered 100 row re-renders
/// per chunk, which manifested as 200–400 ms main-thread blocks.
#[derive(Clone)]
struct HunkTokensView {
    signals: Arc<Vec<Vec<ArcRwSignal<Option<LineTokens>>>>>,
}

impl HunkTokensView {
    fn new(per_line: Vec<Vec<ArcRwSignal<Option<LineTokens>>>>) -> Self {
        Self {
            signals: Arc::new(per_line),
        }
    }

    fn read(&self, hi: usize, li: usize) -> Option<LineTokens> {
        self.signals
            .get(hi)
            .and_then(|h| h.get(li))
            .map(|s| s.get())
            .unwrap_or(None)
    }
}

/// Invoked when the user presses the pointer down on a line-number gutter
/// (the canonical one for that line: `.diff-gutter-new` for Added/Context,
/// `.diff-gutter-old` for Removed). Used by review mode to start a
/// click+drag line-range selection. The diff renderer always calls
/// `prevent_default()` on the event before invoking the callback so OS
/// text-selection doesn't kick in mid-drag.
pub type GutterPointerDownFn =
    std::sync::Arc<dyn Fn(ProjectRootPath, String, ReviewDiffSide, u32) + Send + Sync + 'static>;

/// Returns an extra CSS class to attach to a rendered diff line, or `None`
/// to leave the row unchanged. The callback is read inside the line's
/// reactive `class` closure, so changes to the underlying signals (e.g. a
/// drag-selection range) flow through to the DOM without a remount.
pub type LineExtraClassFn = std::sync::Arc<
    dyn Fn(ProjectRootPath, String, ReviewDiffSide, u32) -> Option<&'static str>
        + Send
        + Sync
        + 'static,
>;

/// Renders an inline gutter button inside the file header (one per file).
/// Always-rendered — review mode uses it for the small `+ comment on file`
/// affordance next to the path label.
pub type GutterActionFileHeaderFn =
    std::sync::Arc<dyn Fn(ProjectRootPath, String) -> AnyView + Send + Sync + 'static>;

/// Renders an inline decoration block under a single line. Returning
/// `None` skips the slot for that line — this is what callers do when
/// no thread (composer/comments/suggestions) is anchored here, so the
/// row contributes zero extra DOM.
pub type DecorationLineFn = std::sync::Arc<
    dyn Fn(ProjectRootPath, String, ReviewDiffSide, u32) -> Option<AnyView> + Send + Sync + 'static,
>;

/// Renders an inline decoration block under the file header (one per file).
pub type DecorationFileHeaderFn =
    std::sync::Arc<dyn Fn(ProjectRootPath, String) -> Option<AnyView> + Send + Sync + 'static>;

/// Bundle of optional review-mode hooks passed down through the diff
/// renderer. The decoration callbacks are responsible for returning
/// `None` when they have nothing to render — virtualization stays on
/// regardless. Lines without a thread therefore contribute no extra DOM,
/// so a 1500-line file with a handful of comments stays virtualized.
#[derive(Clone, Default)]
pub struct DiffDecorations {
    pub gutter_pointer_down: Option<GutterPointerDownFn>,
    pub line_extra_class: Option<LineExtraClassFn>,
    pub gutter_action_for_file_header: Option<GutterActionFileHeaderFn>,
    pub decoration_below_line: Option<DecorationLineFn>,
    pub decoration_below_file_header: Option<DecorationFileHeaderFn>,
}

const SBS_MIN_FRACTION: f64 = 0.05;
const SBS_MAX_FRACTION: f64 = 0.95;

/// Initial estimates used by virtualization until the first measurement
/// effect refines them. Match `file_view`'s constants so behavior is
/// consistent across the two views.
const INITIAL_LINE_HEIGHT_ESTIMATE: f64 = 18.0;
const INITIAL_VIEWPORT_HEIGHT_ESTIMATE: f64 = 600.0;
/// Buffer rows rendered outside the visible viewport on each side. Larger
/// values reduce mount/unmount churn during fast scroll at the cost of a
/// slightly bigger DOM. 80 keeps ~3 viewports in DOM at once on typical
/// laptop displays — enough that wheel-scrolls don't outrun the buffer.
const OVERSCAN_LINES: f64 = 80.0;
/// Below this rendered-row total we render every row up front (no spacers,
/// no scroll math). Keeps the small-diff path identical in DOM shape and
/// preserves layout assertions for tiny test diffs.
const VIRTUALIZE_THRESHOLD: usize = 200;

fn tab_scroll_state_from_element(el: &web_sys::Element) -> TabScrollState {
    TabScrollState {
        scroll_top: el.scroll_top(),
        scroll_height: el.scroll_height(),
        client_height: el.client_height(),
        user_scrolled_up: true,
    }
}

/// Geometry signals shared between `DiffContent` and any descendant that
/// needs to virtualize its rows against the global scroll position. The
/// scroll signal is updated by `DiffContent`'s `on:scroll`; size signals
/// are refined by a measurement effect on first paint.
#[derive(Clone, Copy)]
struct DiffScroll {
    scroll_top: RwSignal<f64>,
    viewport_height: RwSignal<f64>,
    line_height: RwSignal<f64>,
}

#[component]
pub fn DiffView(
    #[prop(optional)] tab_id: Option<TabId>,
    /// Explicit owning project identity for live (non-frozen) diffs. Keys
    /// `diff_contents` and addresses context-mode refetches to the tab's own
    /// project/host — not whatever project happens to be active — so a diff
    /// tab keeps showing/refetching its own project even after the active
    /// project changes, and two same-root projects never collide. Omitted
    /// only for frozen-payload (review-snapshot) diffs, which never touch
    /// `diff_contents` or refetch.
    #[prop(optional)]
    host_id: Option<String>,
    #[prop(optional)] project_id: Option<protocol::ProjectId>,
    root: ProjectRootPath,
    scope: ProjectDiffScope,
    path: String,
    /// When `Some`, drives the diff from a frozen payload Memo instead of
    /// `state.diff_contents` and skips refetches on context-mode change.
    /// The payload list is filtered by `(root, scope)` and the matching
    /// payload's files filtered to `path` if non-empty.
    #[prop(optional)]
    frozen_payload: Option<Memo<Option<Vec<ProjectGitDiffPayload>>>>,
    #[prop(optional)] on_gutter_pointer_down: Option<GutterPointerDownFn>,
    #[prop(optional)] line_extra_class: Option<LineExtraClassFn>,
    #[prop(optional)] gutter_action_for_file_header: Option<GutterActionFileHeaderFn>,
    #[prop(optional)] decoration_below_line: Option<DecorationLineFn>,
    #[prop(optional)] decoration_below_file_header: Option<DecorationFileHeaderFn>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Review mode is implied by `frozen_payload`: the caller is rendering
    // a review's frozen snapshot and wants the chrome that doesn't apply
    // (LAYOUT toggle, CONTEXT toggle, scope badge) suppressed.
    let review_mode = frozen_payload.is_some();

    let decorations = DiffDecorations {
        gutter_pointer_down: on_gutter_pointer_down,
        line_extra_class,
        gutter_action_for_file_header,
        decoration_below_line,
        decoration_below_file_header,
    };

    // Live diffs are keyed by the tab's explicit project identity (+ root,
    // scope, path). `None` only in frozen-payload mode, which reads the
    // snapshot memo instead of `diff_contents`.
    let diff_key: Option<crate::state::DiffKey> = match (host_id.clone(), project_id.clone()) {
        (Some(h), Some(p)) => Some(crate::state::DiffKey::new(
            h,
            p,
            root.clone(),
            scope,
            path.clone(),
        )),
        _ => None,
    };
    let frozen_for_diff = frozen_payload;
    let diff_root = root.clone();
    let diff_path = path.clone();
    let diff_key_for_diff = diff_key.clone();
    let diff = move || -> Option<DiffViewState> {
        if let Some(memo) = frozen_for_diff {
            let payloads = memo.get()?;
            let matched = payloads
                .into_iter()
                .find(|p| p.root == diff_root && p.scope == scope)?;
            let files: Vec<ProjectGitDiffFile> = if diff_path.is_empty() {
                matched.files
            } else {
                matched
                    .files
                    .into_iter()
                    .filter(|f| f.relative_path == diff_path)
                    .collect()
            };
            return Some(DiffViewState {
                root: matched.root,
                scope: matched.scope,
                path: if diff_path.is_empty() {
                    None
                } else {
                    Some(diff_path.clone())
                },
                context_mode: matched.context_mode,
                pending: false,
                files,
            });
        }
        let key = diff_key_for_diff.as_ref()?;
        state.diff_contents.with(|diffs| diffs.get(key).cloned())
    };

    // Reactive effect: when the context-mode signal differs from the stored
    // entry's requested mode, dispatch a fresh ProjectReadDiff to the tab's
    // OWN project/host (not the active project). Disabled in frozen-payload
    // mode because the snapshot does not refetch.
    let frozen_for_effect = frozen_payload.is_some();
    let effect_state = state.clone();
    let effect_key = diff_key.clone();
    let effect_host = host_id.clone();
    let effect_project = project_id.clone();
    Effect::new(move |_| {
        if frozen_for_effect {
            return;
        }
        let (Some(key), Some(host_id), Some(project_id)) = (
            effect_key.clone(),
            effect_host.clone(),
            effect_project.clone(),
        ) else {
            return;
        };
        let signal_mode = effect_state.diff_context_mode.get();
        let Some(current) = effect_state
            .diff_contents
            .with(|diffs| diffs.get(&key).cloned())
        else {
            return;
        };
        if current.context_mode == signal_mode {
            return;
        }
        let stream = StreamPath(format!("/project/{}", project_id.0));
        let root = current.root.clone();
        let scope = current.scope;
        let path = current.path.clone();

        let path_for_update = path.clone();
        let root_for_update = root.clone();
        effect_state.diff_contents.update(|diffs| {
            let previous = diffs.get(&key);
            let next = DiffViewState::for_request(
                previous,
                root_for_update,
                scope,
                path_for_update,
                signal_mode,
            );
            diffs.insert(key.clone(), next);
        });

        let payload = ProjectReadDiffPayload {
            root,
            scope,
            path,
            context_mode: signal_mode,
        };
        spawn_local(async move {
            if let Err(e) = send_frame(&host_id, stream, FrameKind::ProjectReadDiff, &payload).await
            {
                log::error!("failed to send ProjectReadDiff on context-mode change: {e}");
            }
        });
    });

    let content_host_id = host_id.clone();
    let content_project_id = project_id.clone();
    view! {
        <div class="diff-view">
            {(!review_mode).then(|| view! { <DiffToolbar /> })}
            {move || {
                let decorations = decorations.clone();
                match diff() {
                    Some(dv) if dv.pending && dv.files.is_empty() => view! {
                        <div class="diff-empty">
                            <p class="placeholder-text">"Loading diff…"</p>
                        </div>
                    }.into_any(),
                    Some(dv) => view! {
                        <DiffContent
                            tab_id=tab_id
                            diff=dv
                            decorations=decorations
                            host_id=content_host_id.clone()
                            project_id=content_project_id.clone()
                            review_mode=review_mode
                        />
                    }.into_any(),
                    None => view! {
                        <div class="diff-empty">
                            <p class="placeholder-text">"Select a file to view its diff"</p>
                        </div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

/// A normal git-diff tab that is also a review surface. It mounts a plain
/// `DiffView` over the *live* (non-frozen) diff payload and, when the
/// project that owns this tab's root has a Draft review, layers the same
/// review decorations the standalone workbench uses on top of it —
/// drag-to-comment gutters, inline thread regions, and a file-level comment
/// affordance. This is how reviews are reached from the normal diff
/// surfaces instead of routing the user into a separate `ReviewView`
/// workbench.
///
/// The review is bound to the tab's *explicit* `(host_id, project_id)` —
/// carried in `TabContent::Diff`, not guessed from `root` — so switching the
/// active project can't change which review decorates an already-open tab,
/// and two projects/hosts that happen to share a root path string still bind
/// to their own review.
///
/// When there is no Draft review, a thin banner offers to start one; we do
/// not fabricate an optimistic review or show comment affordances until a
/// real Draft exists (server-confirmed via `ReviewListChanged`).
#[component]
pub fn ReviewableDiffView(
    tab_id: TabId,
    host_id: String,
    project_id: protocol::ProjectId,
    root: ProjectRootPath,
    scope: ProjectDiffScope,
    path: String,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Combined HEAD-to-worktree diffs have no stable identity when staged and
    // unstaged versions of the same path coexist. The two explicit scopes are
    // independently reviewable and remain distinct in ReviewLocation.
    if scope == ProjectDiffScope::Uncommitted {
        return view! {
            <DiffView
                tab_id=tab_id
                host_id=host_id
                project_id=project_id
                root=root
                scope=scope
                path=path
            />
        }
        .into_any();
    }

    // Composer + drag-selection signals are owned here so the decorations
    // and the window-level drag listeners share them. Listeners install
    // once for the lifetime of the tab.
    let composer = RwSignal::new(None);
    let drag_selection = RwSignal::new(None);
    install_drag_listeners(drag_selection, composer);

    // Reactively resolve this project's single workspace Draft review.
    // `(host, id)` when one exists, `None` otherwise. One review spans all of
    // the project's roots, so the lookup is keyed by project alone; this diff
    // tab renders only its own root's slice of that review (its decorations
    // filter comments by `ReviewLocation.root`). Only changes when the draft
    // id changes (create / submit / cancel) — adding a comment does not flip
    // it, so the diff below does not remount on every comment.
    //
    // We start from the Draft summary (cheap, arrives first), but if we hold
    // the full record we require *its* live status to still be Draft: a live
    // `StatusChanged` updates `state.reviews` before `review_summaries`
    // refreshes, so trusting a stale Draft summary alone would keep inline
    // comment affordances enabled on an already-submitted review. When the
    // full record is absent we keep the summary's Draft id so the first
    // subscribe can fetch it.
    let draft_state = state.clone();
    let draft_host = host_id.clone();
    let draft_project = project_id.clone();
    let draft: Memo<Option<(String, protocol::ReviewId)>> = Memo::new(move |_| {
        let id = draft_state.review_summaries.with(|m| {
            m.get(&draft_project).and_then(|sums| {
                crate::components::review_view::pick_workspace_draft(sums).map(|s| s.id.clone())
            })
        })?;
        let live_non_draft = draft_state.reviews.with(|r| {
            r.get(&id)
                .map(|rev| !matches!(rev.status, protocol::ReviewStatus::Draft))
                .unwrap_or(false)
        });
        if live_non_draft {
            return None;
        }
        Some((draft_host.clone(), id))
    });
    let is_draft: Memo<bool> = Memo::new(move |_| draft.get().is_some());

    // Reactively keep the draft review subscribed so the diff can render its
    // comments and suggestions. The shared helper tracks the draft target,
    // resubscribes when the draft id changes, and retries on send failure /
    // record loss / reconnect.
    crate::components::review_view::subscribe_review_reactive(&state, draft);

    // The tab's own identity, forwarded to `DiffView` so the live diff body
    // and any context-mode refetch stay bound to this project (not the review
    // host, and not the active project).
    let dv_host = host_id.clone();
    let dv_project = project_id.clone();
    view! {
        <div class="reviewable-diff">
            {move || {
                let root = root.clone();
                let path = path.clone();
                let dv_host = dv_host.clone();
                let dv_project = dv_project.clone();
                match draft.get() {
                    // `review_host` is the *review's* host (used for comment
                    // actions); the DiffView still keys off the *tab's*
                    // `dv_host`/`dv_project` identity.
                    Some((review_host, review_id)) => {
                        let decorations = build_review_decorations(
                            composer,
                            drag_selection,
                            review_id,
                            review_host,
                            is_draft,
                            match scope {
                                ProjectDiffScope::Unstaged => protocol::ReviewTarget::UnstagedDiff,
                                ProjectDiffScope::Staged => protocol::ReviewTarget::StagedDiff,
                                ProjectDiffScope::Uncommitted => unreachable!(),
                            },
                        );
                        view! {
                            <DiffView
                                tab_id=tab_id
                                host_id=dv_host
                                project_id=dv_project
                                root=root
                                scope=scope
                                path=path
                                on_gutter_pointer_down=decorations.gutter_pointer_down
                                line_extra_class=decorations.line_extra_class
                                gutter_action_for_file_header=decorations.gutter_action_for_file_header
                                decoration_below_line=decorations.decoration_below_line
                                decoration_below_file_header=decorations.decoration_below_file_header
                            />
                        }
                        .into_any()
                    }
                    None => view! {
                        <DiffView
                            tab_id=tab_id
                            host_id=dv_host
                            project_id=dv_project
                            root=root
                            scope=scope
                            path=path
                        />
                    }
                    .into_any(),
                }
            }}
        </div>
    }
    .into_any()
}

#[component]
fn DiffToolbar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let view_mode = state.diff_view_mode;
    let context_mode = state.diff_context_mode;

    view! {
        <div class="diff-toolbar">
            <div class="diff-toolbar-group">
                <span class="diff-toolbar-label">"Layout"</span>
                <div class="settings-segmented-control">
                    <button
                        class=move || if view_mode.get() == DiffViewMode::Unified { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_view_mode(view_mode, DiffViewMode::Unified)
                    >
                        "Unified"
                    </button>
                    <button
                        class=move || if view_mode.get() == DiffViewMode::SideBySide { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_view_mode(view_mode, DiffViewMode::SideBySide)
                    >
                        "Side by Side"
                    </button>
                </div>
            </div>
            <div class="diff-toolbar-group">
                <span class="diff-toolbar-label">"Context"</span>
                <div class="settings-segmented-control">
                    <button
                        class=move || if context_mode.get() == DiffContextMode::Hunks { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_context_mode(context_mode, DiffContextMode::Hunks)
                    >
                        "Hunks"
                    </button>
                    <button
                        class=move || if context_mode.get() == DiffContextMode::FullFile { "segment active" } else { "segment" }
                        on:click=move |_| set_diff_context_mode(context_mode, DiffContextMode::FullFile)
                    >
                        "Full File"
                    </button>
                </div>
            </div>
        </div>
    }
}

fn set_diff_view_mode(signal: RwSignal<DiffViewMode>, mode: DiffViewMode) {
    signal.set(mode);
    crate::components::settings_panel::persist_diff_view_mode(mode);
}

fn set_diff_context_mode(signal: RwSignal<DiffContextMode>, mode: DiffContextMode) {
    signal.set(mode);
    crate::components::settings_panel::persist_diff_context_mode(mode);
}

/// Mirror `node_ref`'s `client_width` into the `--diff-scrollport-width`
/// CSS custom property on the same element, kept current via a
/// `ResizeObserver`. The property cascades to descendants, so
/// `.review-thread-region` / `.review-comment-card` /
/// `.review-composer` use the *closest* ancestor that sets the var:
///
/// * `.diff-content` publishes the outer scrollport width — unified
///   diffs and SBS file-header decorations bind to this.
/// * each `.diff-pane` publishes its own width — comments rendered
///   inside an SBS pane bind to the pane (narrower) value, so a long
///   comment doesn't stretch past the visible pane.
///
/// Single observer per element, eagerly seeded on mount so first
/// paint has a bounded width, and disconnected on cleanup. No
/// polling, no rAF loop, no shared signal.
fn install_scrollport_width_observer(node_ref: NodeRef<leptos::html::Div>) {
    type Slot = Option<(
        web_sys::ResizeObserver,
        Closure<dyn FnMut(JsValue, JsValue)>,
    )>;
    let slot: StoredValue<Slot, LocalStorage> = StoredValue::new_local(None);
    Effect::new(move |_| {
        let Some(el) = node_ref.get() else {
            return;
        };
        if slot.with_value(|s| s.is_some()) {
            return;
        }
        let html_el: web_sys::HtmlElement = (*el).clone();
        let write_width = move |el: &web_sys::HtmlElement| {
            let w = el.client_width();
            if w > 0 {
                let _ = el
                    .style()
                    .set_property("--diff-scrollport-width", &format!("{w}px"));
            }
        };
        // Seed once so the first paint already has a bounded width.
        write_width(&html_el);
        let el_for_cb = html_el.clone();
        let cb =
            Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_entries: JsValue, _: JsValue| {
                write_width(&el_for_cb);
            });
        if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            let element: web_sys::Element = html_el.unchecked_into();
            observer.observe(&element);
            slot.update_value(|s| *s = Some((observer, cb)));
        }
    });
    on_cleanup(move || {
        slot.update_value(|s| {
            if let Some((observer, _cb)) = s.take() {
                observer.disconnect();
            }
        });
    });
}

fn stage_hunk(root: ProjectRootPath, relative_path: String, hunk_id: String) {
    let state = expect_context::<AppState>();
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        let payload = ProjectStageHunkPayload {
            path: ProjectPath {
                root,
                relative_path,
            },
            hunk_id,
        };
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectStageHunk,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectStageHunk: {e}");
        }
    });
}

// ── Code intelligence over the diff's new side ──────────────────────────────
//
// Code intel answers questions about the file **on disk**. A diff row is only a
// legitimate place to ask one when the text in that row is the text on disk, so
// every request passes two independent gates:
//
// 1. `new_side_matches_worktree` — a per-scope precondition, checked before we
//    subscribe at all. Exact, not a guess (see the doc comment).
// 2. The row-text equality check in `diff_target_at_point` — a per-row check at
//    the moment of use, because the diff payload is a snapshot and the file may
//    have been rewritten (by an agent, a rebase, an editor) since it was
//    fetched. A row whose rendered text no longer matches the file at that line
//    number is declined outright rather than answered against shifted offsets.
//
// The old side is never eligible: removed text does not exist on disk, so there
// is no honest answer. Those rows stay inert.

/// Whether the **new side** of `scope`'s diff for `relative_path` is
/// byte-identical to the file in the working tree.
///
/// - `Unstaged` / `Uncommitted` compare against the working tree, so their new
///   side *is* the file on disk by construction. (Untracked files are read
///   straight off disk and appended — `read_diff_with_runner` in
///   `server/src/project_stream.rs` — so they hold too.)
/// - `Staged` compares HEAD to the *index*, which is a different object than
///   the working tree. It coincides exactly when the file has nothing unstaged:
///   git guarantees index == worktree then. That makes this an exact
///   precondition rather than a content heuristic — a sampled text comparison
///   could not distinguish "identical" from "shifted but coincidentally equal
///   on the lines the diff happens to show".
fn new_side_matches_worktree(
    state: &AppState,
    project_id: &ProjectId,
    root: &ProjectRootPath,
    relative_path: &str,
    scope: ProjectDiffScope,
) -> bool {
    match scope {
        ProjectDiffScope::Unstaged | ProjectDiffScope::Uncommitted => true,
        ProjectDiffScope::Staged => state.git_status.with_untracked(|status| {
            status.get(project_id).is_some_and(|roots| {
                roots
                    .iter()
                    .find(|candidate| &candidate.root == root)
                    .is_some_and(|candidate| {
                        candidate
                            .files
                            .iter()
                            .find(|file| file.relative_path == relative_path)
                            // No status entry means we cannot prove the index
                            // matches the worktree, so we decline. Failing
                            // closed keeps a wrong answer off the screen.
                            .is_some_and(|file| file.unstaged.is_none() && !file.untracked)
                    })
            })
        }),
    }
}

/// A diff row resolved to a position in the underlying file.
struct DiffCodeIntelTarget {
    key: FileResourceKey,
    version: ProjectFileVersion,
    /// 0-based index of the file line this row shows.
    line_idx: usize,
    /// Absolute byte offset into the file on disk.
    offset: u32,
    /// The file's line containing `offset`, for the identifier-char gate.
    line_text: String,
    line_byte: u32,
    /// The row's `.diff-text` element, so the Cmd-link overlay can measure a
    /// sub-range of the rendered line without re-querying the DOM.
    code: web_sys::Element,
    anchor_left: f64,
    anchor_top: f64,
    anchor_bottom: f64,
}

/// Owning identity for a diff tab's code-intel requests. Absent for review
/// snapshots, which carry no project identity and are frozen against a diff we
/// cannot prove still matches the worktree — both reasons to stay inert.
#[derive(Clone)]
struct DiffCodeIntelContext {
    tab: TabId,
    host_id: String,
    project_id: ProjectId,
    root: ProjectRootPath,
    scope: ProjectDiffScope,
}

/// Per-file `FileLines` cache for one mounted diff, so a mousemove burst does
/// not rebuild the line-offset table on every event. Keyed by file and
/// invalidated on version change.
type DiffLinesCache =
    RefCell<std::collections::HashMap<FileResourceKey, (ProjectFileVersion, FileLines)>>;

/// Why a point over the diff has no code-intel position behind it.
///
/// Every variant is a case the diff viewer must not answer, and all but
/// `NotOverCode` are worth *saying out loud* — a Cmd+click that resolves
/// nothing is otherwise indistinguishable from a broken feature.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DiffTargetMiss {
    /// The pointer is not over diff code (gutter, spacer, blank pane cell).
    /// The only silent case: there was no plausible symbol to ask about.
    NotOverCode,
    /// The old side. Removed text does not exist on disk, so there is no
    /// position to resolve.
    RemovedLine,
    /// A staged diff for a file that also has unstaged changes: the index and
    /// the working tree differ, so the row's line number does not address the
    /// file the language server has open.
    IndexDiffersFromWorktree,
    /// Contents and subscription have not arrived yet.
    NotReady,
    /// The file changed under the diff snapshot: this row's text is no longer
    /// what the file has at that line.
    RowStale,
}

impl DiffTargetMiss {
    /// User-facing explanation, or `None` when there is nothing worth saying.
    fn reason(&self) -> Option<&'static str> {
        match self {
            Self::NotOverCode => None,
            Self::RemovedLine => {
                Some("Removed lines do not exist on disk, so they have no code intelligence")
            }
            Self::IndexDiffersFromWorktree => Some(
                "This file has unstaged changes, so the staged diff does not match the file \
                 on disk",
            ),
            Self::NotReady => Some("Code intelligence is still loading for this file"),
            Self::RowStale => Some(
                "This file changed since the diff was loaded — reload it to use code \
                      intelligence here",
            ),
        }
    }
}

/// Resolve a viewport point over a diff to a position in the file on disk, or
/// the reason there is none. Never guesses.
fn diff_target_at_point(
    state: &AppState,
    context: &DiffCodeIntelContext,
    cache: &DiffLinesCache,
    client_x: f64,
    client_y: f64,
) -> Result<DiffCodeIntelTarget, DiffTargetMiss> {
    let caret = caret_at_point(client_x, client_y).ok_or(DiffTargetMiss::NotOverCode)?;
    let element = node_to_element(&caret.node).ok_or(DiffTargetMiss::NotOverCode)?;
    let row = element
        .closest(".diff-line")
        .ok()
        .flatten()
        .ok_or(DiffTargetMiss::NotOverCode)?;

    // New side only. In unified mode `data-anchor-side` is "new" for Added and
    // Context rows; in side-by-side it is "new" for the right pane. Reading
    // `data-anchor-new-line` (rather than `data-anchor-line`) matters in
    // side-by-side, where the *pair's* new line number is stamped on both
    // panes' rows — the left pane of a Removed/Added pair would otherwise
    // resolve a removed line against the added line's number.
    match row.get_attribute("data-anchor-side").as_deref() {
        Some("new") => {}
        Some("old") => return Err(DiffTargetMiss::RemovedLine),
        _ => return Err(DiffTargetMiss::NotOverCode),
    }
    let line_number: u32 = row
        .get_attribute("data-anchor-new-line")
        .and_then(|attr| attr.parse().ok())
        .ok_or(DiffTargetMiss::NotOverCode)?;
    let line_idx = line_number
        .checked_sub(1)
        .ok_or(DiffTargetMiss::NotOverCode)? as usize;

    let relative_path = row
        .closest(".diff-file")
        .ok()
        .flatten()
        .and_then(|file| file.get_attribute("data-diff-path"))
        .ok_or(DiffTargetMiss::NotOverCode)?;

    if !new_side_matches_worktree(
        state,
        &context.project_id,
        &context.root,
        &relative_path,
        context.scope,
    ) {
        return Err(DiffTargetMiss::IndexDiffersFromWorktree);
    }

    let key = FileResourceKey {
        host_id: context.host_id.clone(),
        project_id: context.project_id.clone(),
        path: ProjectPath {
            root: context.root.clone(),
            relative_path,
        },
    };

    // This diff tab must own the file's code intel. Without the hold the
    // contents in `open_files` belong to someone else (a file tab), the
    // subscription may not exist, and any result we asked for would be dropped
    // by the occurrence guard on arrival — so decline rather than round-trip.
    if !state.diff_tab_holds_code_intel(context.tab, &key) {
        return Err(DiffTargetMiss::NotReady);
    }

    let (version, lines) = held_file_lines(state, cache, &key).ok_or(DiffTargetMiss::NotReady)?;
    if line_idx >= lines.len() {
        return Err(DiffTargetMiss::RowStale);
    }
    let file_line = lines.line(line_idx).to_owned();

    // The row's rendered text must still be the file's text at that line. The
    // diff payload is a snapshot; if the file moved underneath it, answering
    // here would resolve a different symbol than the one on screen.
    let code = row
        .query_selector(".diff-text")
        .ok()
        .flatten()
        .ok_or(DiffTargetMiss::NotOverCode)?;
    if code.text_content().unwrap_or_default() != file_line {
        return Err(DiffTargetMiss::RowStale);
    }

    let code_node: &web_sys::Node = code.unchecked_ref();
    let utf16_col = utf16_col_in_code(code_node, &caret.node, caret.offset)
        .ok_or(DiffTargetMiss::NotOverCode)?;
    let line_byte = line_byte_for_utf16_col(&file_line, utf16_col);
    let (anchor_left, anchor_top, anchor_bottom) = match caret.rect {
        Some(rect) => (rect.left(), rect.top(), rect.bottom()),
        None => (client_x, client_y, client_y + 16.0),
    };
    Ok(DiffCodeIntelTarget {
        key,
        version,
        line_idx,
        offset: lines.line_start(line_idx) + line_byte,
        line_text: file_line,
        line_byte,
        code,
        anchor_left,
        anchor_top,
        anchor_bottom,
    })
}

/// The held contents of `key` as a `FileLines`, or `None` if the diff view has
/// not pulled them yet (or the read came back missing/binary).
fn held_file_lines(
    state: &AppState,
    cache: &DiffLinesCache,
    key: &FileResourceKey,
) -> Option<(ProjectFileVersion, FileLines)> {
    // Resolve the version without copying the text: mousemove fires constantly,
    // and cloning a large file's contents on every event would be the most
    // expensive thing in the handler.
    let version = state.open_files.with_untracked(|files| {
        let file = files.get(key)?;
        if file.missing || file.is_binary || file.contents.is_none() {
            return None;
        }
        Some(file.version)
    })?;
    if let Some((cached_version, lines)) = cache.borrow().get(key)
        && *cached_version == version
    {
        return Some((version, lines.clone()));
    }
    // Cold, or the file changed on disk — rebuild the line-offset table.
    let contents = state
        .open_files
        .with_untracked(|files| files.get(key).and_then(|file| file.contents.clone()))?;
    let lines = FileLines::new(&contents);
    cache
        .borrow_mut()
        .insert(key.clone(), (version, lines.clone()));
    Some((version, lines))
}

/// Ensure the file under a pointer is held (contents + subscription), so the
/// *next* interaction over it can resolve. Called on hover before any request:
/// subscribing lazily, per file the user actually points at, is what keeps a
/// whole-root diff from `didOpen`-ing hundreds of documents in the language
/// server.
fn ensure_diff_file_held(
    state: &AppState,
    context: &DiffCodeIntelContext,
    client_x: f64,
    client_y: f64,
) {
    let Some(row) = caret_at_point(client_x, client_y)
        .and_then(|caret| node_to_element(&caret.node))
        .and_then(|element| element.closest(".diff-line").ok().flatten())
    else {
        return;
    };
    if row.get_attribute("data-anchor-side").as_deref() != Some("new") {
        return;
    }
    let Some(relative_path) = row
        .closest(".diff-file")
        .ok()
        .flatten()
        .and_then(|file| file.get_attribute("data-diff-path"))
    else {
        return;
    };
    if !new_side_matches_worktree(
        state,
        &context.project_id,
        &context.root,
        &relative_path,
        context.scope,
    ) {
        return;
    }
    let key = FileResourceKey {
        host_id: context.host_id.clone(),
        project_id: context.project_id.clone(),
        path: ProjectPath {
            root: context.root.clone(),
            relative_path,
        },
    };
    crate::actions::hold_file_for_diff_code_intel(state, context.tab, &key);
}

/// Debounce before firing a hover request, matching `file_view`'s delay so the
/// two surfaces feel the same.
const DIFF_HOVER_DEBOUNCE_MS: i32 = 250;

/// Where to draw the Cmd-held "this is clickable" underline, in coordinates
/// relative to the diff's scrollport.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiffCmdLink {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

/// UTF-16 column of byte offset `byte` within `line` — the inverse of
/// `line_byte_for_utf16_col`, needed to address rendered text through a DOM
/// `Range`, which counts in UTF-16.
fn utf16_col_for_line_byte(line: &str, byte: u32) -> u32 {
    line.get(..byte as usize)
        .unwrap_or(line)
        .encode_utf16()
        .count() as u32
}

/// Locate the (text node, in-node UTF-16 offset) for column `utf16_col` within
/// `code`'s rendered text. Walks the same node order `utf16_col_in_code` sums,
/// so the two are exact inverses over the same DOM.
fn text_node_at_utf16_col(code: &web_sys::Node, utf16_col: u32) -> Option<(web_sys::Node, u32)> {
    let mut seen = 0u32;
    let nodes = crate::code_intel_dom::descendant_text_nodes(code);
    for node in &nodes {
        let len = node
            .text_content()
            .unwrap_or_default()
            .encode_utf16()
            .count() as u32;
        if utf16_col <= seen + len {
            return Some((node.clone(), utf16_col - seen));
        }
        seen += len;
    }
    // A column exactly at the end of the last node is still addressable.
    nodes.last().map(|node| {
        let len = node
            .text_content()
            .unwrap_or_default()
            .encode_utf16()
            .count() as u32;
        (node.clone(), len)
    })
}

/// Measure the on-screen box of the identifier under a resolved target, when
/// the pushed model says it is navigable.
///
/// Drawn as an overlay rather than by splitting the row's spans. The file
/// viewer splits because it also paints diagnostic squiggles, but the diff needs
/// exactly one transient range, and the per-row text path is the one a wasm test
/// guards against mangling (root `CLAUDE.md`) — an overlay cannot alter a single
/// rendered character. `None` when nothing is navigable there.
fn diff_cmd_link_box(
    state: &AppState,
    target: &DiffCodeIntelTarget,
    lines: &FileLines,
    scrollport: &web_sys::Element,
) -> Option<DiffCmdLink> {
    let code_intel_key = crate::state::CodeIntelKey::from(&target.key);
    let range = state.code_intel.with_untracked(|map| {
        map.get(&code_intel_key)?
            .navigable_range_at(target.version, target.offset)
    })?;

    // Clamp the file-absolute range to the rendered line: a definition span can
    // legitimately cover more than one line, and we only draw on this one.
    let line_start = lines.line_start(target.line_idx);
    let line_end = lines.line_content_end(target.line_idx);
    let start = range.start.max(line_start);
    let end = range.end.min(line_end);
    if end <= start {
        return None;
    }

    let start_col = utf16_col_for_line_byte(&target.line_text, start - line_start);
    let end_col = utf16_col_for_line_byte(&target.line_text, end - line_start);
    let code_node: &web_sys::Node = target.code.unchecked_ref();
    let (start_node, start_offset) = text_node_at_utf16_col(code_node, start_col)?;
    let (end_node, end_offset) = text_node_at_utf16_col(code_node, end_col)?;

    let dom_range = web_sys::Range::new().ok()?;
    dom_range.set_start(&start_node, start_offset).ok()?;
    dom_range.set_end(&end_node, end_offset).ok()?;
    let rect = dom_range.get_bounding_client_rect();
    if rect.width() <= 0.0 {
        return None;
    }
    // Convert to scrollport-relative coordinates so the overlay stays put while
    // the diff is scrolled by other means (it is cleared on scroll anyway).
    let host = scrollport.get_bounding_client_rect();
    Some(DiffCmdLink {
        left: rect.left() - host.left() + scrollport.scroll_left() as f64,
        top: rect.top() - host.top() + scrollport.scroll_top() as f64,
        width: rect.width(),
        height: rect.height(),
    })
}

/// A settled pointer over a new-side identifier fires one `code_intel_hover`.
fn maybe_request_diff_hover(
    state: &AppState,
    context: &DiffCodeIntelContext,
    cache: &DiffLinesCache,
    client_x: f64,
    client_y: f64,
) {
    let Ok(target) = diff_target_at_point(state, context, cache, client_x, client_y) else {
        crate::actions::dismiss_hover(state);
        return;
    };
    if !is_identifier_byte(&target.line_text, target.line_byte) {
        crate::actions::dismiss_hover(state);
        return;
    }
    // Already showing/awaiting a hover for this exact identifier: leave it.
    if state.code_intel_hover.with_untracked(|hover| {
        hover.as_ref().is_some_and(|popover| {
            popover.tab == context.tab
                && popover.key == target.key
                && popover.offset == target.offset
        })
    }) {
        return;
    }
    crate::actions::request_hover(
        state,
        context.tab,
        target.key,
        target.version,
        target.offset,
        target.anchor_left,
        target.anchor_top,
        target.anchor_bottom,
    );
}

#[component]
fn DiffContent(
    tab_id: Option<TabId>,
    diff: DiffViewState,
    decorations: DiffDecorations,
    #[prop(optional_no_strip)] host_id: Option<String>,
    #[prop(optional_no_strip)] project_id: Option<ProjectId>,
    #[prop(optional)] review_mode: bool,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let initial_scroll_state = tab_id.and_then(|id| state.tab_scroll_state_untracked(id));
    let scope_label = match diff.scope {
        ProjectDiffScope::Staged => "staged",
        ProjectDiffScope::Unstaged => "unstaged",
        ProjectDiffScope::Uncommitted => "uncommitted",
    };
    let perf_key = format!(
        "diff:{}:{}",
        diff.root.0,
        diff.path.clone().unwrap_or_default()
    );
    crate::perf::log_phase("diff_open", "content_mount", &perf_key, "");
    let mount_t0 = crate::perf::now_ms();

    // Flatten all diff lines into a searchable list and compute per-hunk
    // starting offsets so each rendered line knows its flat search index.
    let mut searchable_lines: Vec<String> = Vec::new();
    let mut file_hunk_offsets: Vec<Vec<usize>> = Vec::new();
    for file in &diff.files {
        let mut hunk_offsets = Vec::new();
        for hunk in &file.hunks {
            hunk_offsets.push(searchable_lines.len());
            for line in &hunk.lines {
                searchable_lines.push(line.text.clone());
            }
        }
        file_hunk_offsets.push(hunk_offsets);
    }

    let find_state = FindState::from_owned(searchable_lines);
    provide_context(find_state);

    // Compute each file's rendered-row offset (its row index in the global
    // virtual scrolling space). A "rendered row" is anything taking up a
    // line of vertical real estate: file header, hunk header (only in
    // Hunks mode), or one diff line. SBS pair rows count as one rendered
    // row even though they show two cells.
    //
    // KNOWN LIMITATION (from codex review): the row-height model treats
    // file/hunk headers as if they have the same height as a diff line.
    // They don't, so spacer math accumulates a small drift across many
    // files. Overscan hides most of it; a future fix is to count distinct
    // height classes or pin all rows to a uniform line-height.
    let mut file_rendered_offsets: Vec<usize> = Vec::with_capacity(diff.files.len());
    let mut acc: usize = 0;
    for file in &diff.files {
        file_rendered_offsets.push(acc);
        acc += rendered_rows_for_file(file, diff.context_mode);
    }
    let prep_dt = crate::perf::now_ms() - mount_t0;
    let total_lines: usize = diff
        .files
        .iter()
        .flat_map(|f| f.hunks.iter())
        .map(|h| h.lines.len())
        .sum();
    crate::perf::log_phase(
        "diff_open",
        "prep_done",
        &perf_key,
        &format!(" lines={total_lines} took={prep_dt:.1}ms"),
    );

    // Scroll geometry. Pre-seed line/viewport height estimates so the
    // visible window is bounded from the very first paint, before the
    // measurement effect runs. Mirrors `file_view` exactly.
    let scroll_top =
        RwSignal::new(initial_scroll_state.map_or(0.0_f64, |scroll| scroll.scroll_top as f64));
    let viewport_height = RwSignal::new(INITIAL_VIEWPORT_HEIGHT_ESTIMATE);
    let line_height = RwSignal::new(INITIAL_LINE_HEIGHT_ESTIMATE);
    let scroll_ctx = DiffScroll {
        scroll_top,
        viewport_height,
        line_height,
    };
    provide_context(scroll_ctx);

    let scroll_ref: NodeRef<leptos::html::Div> = NodeRef::new();

    let restored_initial_scroll = std::rc::Rc::new(std::cell::Cell::new(false));
    let restored_initial_scroll_for_effect = restored_initial_scroll.clone();
    let scroll_ref_for_restore = scroll_ref;
    let state_for_restore = state.clone();
    Effect::new(move |_| {
        if restored_initial_scroll_for_effect.get() {
            return;
        }
        let (Some(tab_id), Some(saved)) = (tab_id, initial_scroll_state) else {
            return;
        };
        let Some(el) = scroll_ref_for_restore.get() else {
            return;
        };
        restored_initial_scroll_for_effect.set(true);
        el.set_scroll_top(saved.scroll_top);
        scroll_top.set(el.scroll_top() as f64);
        let element: web_sys::Element = el.clone().unchecked_into();
        state_for_restore.save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
    });

    // Measure the geometry once after first paint. Re-runs are cheap; we
    // only update the signal if the measured value differs meaningfully.
    let perf_key_for_measure = perf_key.clone();
    let measure_logged = std::rc::Rc::new(std::cell::Cell::new(false));
    Effect::new(move |_| {
        let Some(el) = scroll_ref.get() else { return };
        let vh = el.client_height() as f64;
        if vh > 0.0 && (viewport_height.get_untracked() - vh).abs() > 0.5 {
            viewport_height.set(vh);
        }
        if let Ok(Some(line_el)) = el.query_selector(".diff-line")
            && let Some(html_el) = line_el.dyn_ref::<web_sys::HtmlElement>()
        {
            let lh = html_el.offset_height() as f64;
            if lh > 0.0 && (line_height.get_untracked() - lh).abs() > 0.5 {
                line_height.set(lh);
            }
            if !measure_logged.get() {
                measure_logged.set(true);
                crate::perf::log_phase(
                    "diff_open",
                    "first_paint_measured",
                    &perf_key_for_measure,
                    &format!(" viewport_h={vh:.0} line_h={lh:.1}"),
                );
            }
        }
    });

    // Publish the diff scrollport's visible width as a CSS custom
    // property so descendants (review thread regions, comment cards,
    // composer) can size themselves to the visible viewport instead
    // of expanding to the diff's inner scroll width. Without this, a
    // long diff line forces `.diff-content` wide enough to need
    // horizontal scrolling, and `position: sticky` review cards
    // inherit that wide intrinsic width — pushing the comment
    // off-screen and forcing the user to scroll sideways to read it.
    //
    // SBS mode additionally installs a per-pane observer (see
    // `install_scrollport_width_observer` calls below `.diff-pane`
    // node_refs in `render_sbs_*`), so a comment rendered inside an
    // SBS pane uses the pane's width, not the wider diff content
    // width. CSS custom property inheritance picks the closest
    // ancestor that sets the var, so the pane override "wins" for
    // descendants while elements outside any pane (e.g. file-header
    // decorations) still see the outer `.diff-content` value.
    install_scrollport_width_observer(scroll_ref);

    // Throttle scroll updates to one per animation frame. Native scroll
    // events fire faster than 60Hz (often hundreds/sec on a trackpad);
    // batching means the visible_window memo only invalidates once per
    // paint, so the `<For>` diffs once per frame and the main thread
    // doesn't fight itself trying to keep up. Smooths out the
    // "lines flash blank during fast scroll" pattern users report.
    //
    // ── Code intelligence (hover + go-to-definition) over the new side ──────
    // Only a live diff tab with a project identity qualifies: review snapshots
    // are frozen against a diff we cannot prove still matches the worktree.
    let code_intel_context = match (tab_id, host_id, project_id) {
        (Some(tab), Some(host_id), Some(project_id)) if !review_mode => {
            Some(DiffCodeIntelContext {
                tab,
                host_id,
                project_id,
                root: diff.root.clone(),
                scope: diff.scope,
            })
        }
        _ => None,
    };
    // `Rc`, not `Arc`: the cache never leaves this component's (single-threaded)
    // event handlers.
    let lines_cache: std::rc::Rc<DiffLinesCache> =
        std::rc::Rc::new(RefCell::new(std::collections::HashMap::new()));
    let hover_timer: TimeoutClosureSlot = StoredValue::new_local(None);
    on_cleanup(move || clear_timeout_timer(hover_timer));

    // Scroll handler: write the native scrollTop straight into the
    // signal. Leptos batches reactive updates within the same task, so
    // a burst of native scroll events still only re-renders the visible
    // window once per microtask. Earlier we throttled with
    // `request_animation_frame`, but the rAF callback never fires
    // reliably in this Tauri WKWebView — the visible window stayed at
    // its initial range and AI-suggestion rows that lived hundreds of
    // lines down were unreachable because no scroll ever propagated
    // into the virtualization math.
    let state_for_scroll = state.clone();
    let scroll_hover_state = state.clone();
    let scroll_has_code_intel = code_intel_context.is_some();
    let on_scroll = move |_: web_sys::Event| {
        if let Some(el) = scroll_ref.get_untracked() {
            scroll_top.set(el.scroll_top() as f64);
            if let Some(tab_id) = tab_id {
                let element: web_sys::Element = el.clone().unchecked_into();
                state_for_scroll
                    .save_tab_scroll_state(tab_id, tab_scroll_state_from_element(&element));
            }
        }
        // A scroll moves the hovered span out from under the popover: cancel a
        // pending request and supersede any in flight.
        if scroll_has_code_intel {
            clear_timeout_timer(hover_timer);
            crate::actions::dismiss_hover(&scroll_hover_state);
        }
    };

    // Cmd-held underline under the symbol the pointer is on, so a navigable
    // identifier is visibly clickable instead of something you have to guess at.
    // Cleared whenever the thing it points at could have moved.
    let cmd_link: RwSignal<Option<DiffCmdLink>> = RwSignal::new(None);
    let hover_point: RwSignal<Option<(f64, f64)>> = RwSignal::new(None);

    let mousemove_context = code_intel_context.clone();
    let mousemove_state = state.clone();
    let mousemove_cache = lines_cache.clone();
    let on_mousemove = move |ev: web_sys::MouseEvent| {
        let Some(context) = mousemove_context.as_ref() else {
            return;
        };
        let Some(window) = web_sys::window() else {
            return;
        };
        clear_timeout_timer(hover_timer);
        let client_x = ev.client_x() as f64;
        let client_y = ev.client_y() as f64;
        hover_point.set(Some((client_x, client_y)));
        // Pull contents + subscription for whatever file the pointer is over,
        // before the debounce, so the request that follows has something to
        // resolve against.
        ensure_diff_file_held(&mousemove_state, context, client_x, client_y);

        let state = mousemove_state.clone();
        let context = context.clone();
        let cache = mousemove_cache.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            maybe_request_diff_hover(&state, &context, &cache, client_x, client_y);
        });
        if let Ok(id) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            DIFF_HOVER_DEBOUNCE_MS,
        ) {
            hover_timer.update_value(|slot| *slot = Some((id, cb)));
        }
    };

    // Recompute the underline whenever either input changes: the pointer moving
    // while Cmd is down, or Cmd going down while the pointer is already
    // parked on a symbol. Both must light it up.
    {
        let link_state = state.clone();
        let link_context = code_intel_context.clone();
        let link_cache = lines_cache.clone();
        let cmd_held = state.cmd_held;
        Effect::new(move |_| {
            let held = cmd_held.get();
            let point = hover_point.get();
            let Some(context) = link_context.as_ref() else {
                return;
            };
            let (Some((x, y)), true) = (point, held) else {
                if cmd_link.get_untracked().is_some() {
                    cmd_link.set(None);
                }
                return;
            };
            let Some(scrollport) = scroll_ref.get_untracked() else {
                return;
            };
            let scrollport: web_sys::Element = scrollport.clone().unchecked_into();
            let next = diff_target_at_point(&link_state, context, &link_cache, x, y)
                .ok()
                .filter(|target| is_identifier_byte(&target.line_text, target.line_byte))
                .and_then(|target| {
                    let (_, lines) = held_file_lines(&link_state, &link_cache, &target.key)?;
                    diff_cmd_link_box(&link_state, &target, &lines, &scrollport)
                });
            if cmd_link.get_untracked() != next {
                cmd_link.set(next);
            }
        });
    }

    let leave_state = state.clone();
    let leave_has_code_intel = code_intel_context.is_some();
    let on_mouseleave = move |_: web_sys::MouseEvent| {
        if !leave_has_code_intel {
            return;
        }
        clear_timeout_timer(hover_timer);
        hover_point.set(None);
        cmd_link.set(None);
        crate::actions::dismiss_hover(&leave_state);
    };

    // Cmd/Ctrl+click go-to-definition. Falls through to the on-demand
    // `code_intel_navigate` when no target has been pushed yet, exactly like the
    // file viewer. When the row cannot be resolved, say why rather than doing
    // nothing — an explicit action that silently no-ops is indistinguishable
    // from a broken feature.
    let click_context = code_intel_context.clone();
    let click_state = state.clone();
    let click_cache = lines_cache.clone();
    let on_click = move |ev: web_sys::MouseEvent| {
        let Some(context) = click_context.as_ref() else {
            return;
        };
        if !(ev.ctrl_key() || ev.meta_key()) {
            return;
        }
        match diff_target_at_point(
            &click_state,
            context,
            &click_cache,
            ev.client_x() as f64,
            ev.client_y() as f64,
        ) {
            Ok(target) => {
                ev.prevent_default();
                crate::actions::navigate_to_definition(
                    &click_state,
                    context.tab,
                    target.key,
                    target.version,
                    target.offset,
                );
            }
            Err(miss) => {
                if let Some(reason) = miss.reason() {
                    ev.prevent_default();
                    click_state
                        .code_intel_notice
                        .set(Some(crate::state::CodeIntelNotice {
                            tab: context.tab,
                            message: reason.to_owned(),
                        }));
                }
            }
        }
    };

    // Right-click menu: Go to Definition / Show References, the same two actions
    // the file viewer offers, against the position under the pointer. A row that
    // cannot be resolved still opens the menu, disabled, carrying the reason.
    let context_menu: RwSignal<Option<CodeIntelMenuState>> = RwSignal::new(None);
    let menu_context = code_intel_context.clone();
    let menu_state = state.clone();
    let menu_cache = lines_cache.clone();
    let on_contextmenu = move |ev: web_sys::MouseEvent| {
        let Some(context) = menu_context.as_ref() else {
            return;
        };
        let x = ev.client_x() as f64;
        let y = ev.client_y() as f64;
        match diff_target_at_point(&menu_state, context, &menu_cache, x, y) {
            Ok(target) => {
                ev.prevent_default();
                let disabled_reason = crate::components::code_intel_ui::status_disabled_reason(
                    &menu_state,
                    &crate::state::CodeIntelKey::from(&target.key),
                    target.version,
                    Some(target.offset),
                );
                context_menu.set(Some(CodeIntelMenuState {
                    x,
                    y,
                    target: Some(CodeIntelMenuTarget {
                        tab_id: context.tab,
                        key: target.key.clone(),
                        version: target.version,
                        offset: target.offset,
                    }),
                    disabled_reason,
                }));
            }
            Err(miss) => {
                // Nothing plausible under the pointer: leave the browser menu.
                let Some(reason) = miss.reason() else { return };
                ev.prevent_default();
                context_menu.set(Some(CodeIntelMenuState {
                    x,
                    y,
                    target: None,
                    disabled_reason: Some(reason.to_owned()),
                }));
            }
        }
    };

    if let Some(context) = code_intel_context.as_ref() {
        crate::components::code_intel_ui::install_notice_auto_clear(context.tab);
    }
    let notice_signal = state.code_intel_notice;
    let notice_tab = code_intel_context.as_ref().map(|context| context.tab);

    view! {
        <>
            {move || {
                let tab = notice_tab?;
                notice_signal.with(|notice| {
                    let notice = notice.as_ref().filter(|notice| notice.tab == tab)?;
                    Some(view! {
                        <div
                            class="diff-code-intel-notice"
                            data-test="diff-code-intel-notice"
                        >
                            {notice.message.clone()}
                        </div>
                    })
                })
            }}
            <div
                // Reactive *class*, not style: `install_scrollport_width_observer`
                // owns an inline custom property on this element, and a reactive
                // style binding would clobber it.
                class=move || if cmd_link.get().is_some() {
                    "diff-content diff-content-cmd-link"
                } else {
                    "diff-content"
                }
                node_ref=scroll_ref
                on:scroll=on_scroll
                on:mousemove=on_mousemove
                on:mouseleave=on_mouseleave
                on:click=on_click
                on:contextmenu=on_contextmenu
            >
                {move || {
                    if state.find_bar_open.get() {
                        Some(view! { <FindBar /> })
                    } else {
                        None
                    }
                }}
                {move || cmd_link.get().map(|link| view! {
                    <div
                        class="diff-cmd-link"
                        data-test="diff-cmd-link"
                        style=format!(
                            "left: {}px; top: {}px; width: {}px; height: {}px;",
                            link.left, link.top, link.width, link.height,
                        )
                    ></div>
                })}
                {diff.files.into_iter().enumerate().map(|(fi, file)| {
                    let hunk_offsets = file_hunk_offsets[fi].clone();
                    let root = diff.root.clone();
                    let rendered_offset = file_rendered_offsets[fi];
                    let decorations = decorations.clone();
                    view! { <DiffFileView file=file scope_label=scope_label scope=diff.scope root=root context_mode=diff.context_mode hunk_offsets=hunk_offsets rendered_offset=rendered_offset decorations=decorations review_mode=review_mode /> }
                }).collect::<Vec<_>>()}
            </div>
            {move || {
                // A miss still opens the menu — disabled, carrying its reason —
                // so an unanswerable row explains itself instead of offering
                // actions that would do nothing.
                context_menu.get().map(|menu| view! {
                    <CodeIntelContextMenu
                        menu=context_menu
                        x=menu.x
                        y=menu.y
                        target=menu.target.clone()
                        disabled_reason=menu.disabled_reason.clone()
                    />
                })
            }}
        </>
    }
}

/// Count how many vertical rows a single file's rendering takes: the file
/// header + (per hunk: optional hunk header + every line). Used to lay out
/// each file at a known position in the global virtual scroll space.
///
/// The line count is the unified-mode row count. SBS mode collapses paired
/// Removed/Added lines into a single paired row, so the SBS row count is
/// `<=` the unified count. This function returns the unified count, which
/// is correct for unified mode and an upper bound for SBS. For single-file
/// diffs (the typical case in Tyde) `rendered_offset` is always 0 so the
/// over-estimate is moot. For multi-file SBS diffs the offsets shift
/// downward by up to ~half the diff size; the visible window may briefly
/// fall on the wrong file boundary until scrolling stabilizes. Acceptable
/// for now; fix is to make this mode-aware (run `pair_lines_side_by_side`
/// per hunk to count) and recompute when view_mode toggles.
fn rendered_rows_for_file(file: &ProjectGitDiffFile, context_mode: DiffContextMode) -> usize {
    // Unmerged, binary, and no-hunk files render the file header plus a
    // single placeholder row instead of hunks. Counting it keeps the per-file
    // virtual-scroll offsets aligned in multi-file diffs.
    if file.unmerged || file.is_binary || file.hunks.is_empty() {
        return 2;
    }
    let mut total = 1; // file header
    for hunk in &file.hunks {
        if context_mode == DiffContextMode::Hunks {
            total += 1;
        }
        total += hunk.lines.len();
    }
    total
}

#[component]
fn DiffFileView(
    file: ProjectGitDiffFile,
    scope_label: &'static str,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    context_mode: DiffContextMode,
    hunk_offsets: Vec<usize>,
    rendered_offset: usize,
    #[prop(optional)] decorations: DiffDecorations,
    #[prop(optional)] review_mode: bool,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let view_mode_sig = state.diff_view_mode;
    let relative_path = file.relative_path.clone();

    let initial_split = match classify_diff_file(&file) {
        DiffFileKind::PureAdd => 0.0,
        DiffFileKind::PureDelete => 1.0,
        DiffFileKind::Mixed => 0.5,
    };
    let split = RwSignal::new(initial_split);

    on_cleanup(clear_sbs_drag_listeners);

    let file_for_view = file.clone();
    let offsets_for_view = hunk_offsets.clone();
    let root_for_view = root.clone();
    let path_for_view = relative_path.clone();

    let syntax_theme = state.syntax_theme;

    let decorations_for_view = decorations.clone();
    let header_decoration_root = root.clone();
    let header_decoration_path = relative_path.clone();
    let header_decoration_cb = decorations.decoration_below_file_header.clone();

    let header_action_root = root.clone();
    let header_action_path = relative_path.clone();
    let header_action_cb = decorations.gutter_action_for_file_header.clone();

    // Binary files (and files git reports with no textual hunks, e.g. a
    // pure mode change) have no lines to render line-anchored comments on.
    // We render a clear placeholder instead of an empty diff body and skip
    // the hunk renderer entirely. The file header still carries the
    // file-level comment affordance + thread region, so the user can leave
    // a file-scoped comment on a binary change.
    let is_unmerged = file.unmerged;
    let is_binary = file.is_binary;
    let show_placeholder = is_unmerged || is_binary || file.hunks.is_empty();
    let placeholder_text = if is_unmerged {
        "Unmerged file — resolve conflicts in the file, then stage it"
    } else if is_binary {
        "Binary file changed"
    } else {
        "No textual changes"
    };
    let placeholder_test = if is_unmerged {
        "diff-unmerged-placeholder"
    } else {
        "diff-binary-placeholder"
    };

    // `data-diff-path` lets a pointer hit-test resolve which file a row belongs
    // to in an all-files diff, where many `.diff-file` blocks share one
    // scrollport. Code intel needs the path to address the project stream.
    let data_path = relative_path.clone();
    view! {
        <div class="diff-file" data-diff-path=data_path>
            <div class="diff-file-header">
                <span class="diff-file-path">{file.relative_path}</span>
                {(!review_mode).then(|| view! { <span class="diff-scope-badge">{scope_label}</span> })}
                {header_action_cb.as_ref().map(|cb| {
                    cb(header_action_root.clone(), header_action_path.clone())
                })}
            </div>
            {move || header_decoration_cb.as_ref().and_then(|cb| {
                cb(header_decoration_root.clone(), header_decoration_path.clone())
            })}
            {if show_placeholder {
                view! {
                    <div class="diff-binary-placeholder" data-test=placeholder_test>
                        {placeholder_text}
                    </div>
                }.into_any()
            } else {
                let file_for_view = file_for_view.clone();
                let offsets_for_view = offsets_for_view.clone();
                let root_for_view = root_for_view.clone();
                let path_for_view = path_for_view.clone();
                let decorations_for_view = decorations_for_view.clone();
                view! {
                    {move || {
                        // Subscribe to syntax_theme so changing the active theme
                        // re-renders the diff with re-tokenized colors. Tokens are
                        // computed eagerly inside render_*_virtualized; reading
                        // the signal here causes that work to re-run on change.
                        let _ = syntax_theme.get();
                        match view_mode_sig.get() {
                            DiffViewMode::Unified => render_unified_virtualized(UnifiedVirtualizedArgs {
                                file: file_for_view.clone(),
                                hunk_offsets: offsets_for_view.clone(),
                                context_mode,
                                scope,
                                root: root_for_view.clone(),
                                relative_path: path_for_view.clone(),
                                rendered_offset,
                                decorations: decorations_for_view.clone(),
                            }),
                            DiffViewMode::SideBySide => render_sbs_virtualized(
                                file_for_view.clone(),
                                offsets_for_view.clone(),
                                context_mode,
                                scope,
                                root_for_view.clone(),
                                path_for_view.clone(),
                                split,
                                rendered_offset,
                                decorations_for_view.clone(),
                            ),
                        }
                    }}
                }.into_any()
            }}
        </div>
    }
}

fn render_unified_hunks(
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    decorations: DiffDecorations,
) -> AnyView {
    view! {
        <div class="diff-hunks">
            {file.hunks.into_iter().enumerate().map(|(hi, hunk)| {
                let offset = hunk_offsets[hi];
                let decorations = decorations.clone();
                view! {
                    <UnifiedHunk hunk=hunk context_mode=context_mode line_offset=offset scope=scope root=root.clone() relative_path=relative_path.clone() decorations=decorations />
                }
            }).collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

/// One vertical row inside a virtualized unified-diff file. The file
/// header sits at index 0 in `rendered_offset` space (one above this list,
/// rendered by `DiffFileView` itself), so the row indices here begin at 1
/// for the first hunk header (or first line in FullFile mode). Each
/// variant carries the indices it needs to render on demand.
#[derive(Clone)]
enum UnifiedFileItem {
    HunkHeader { hi: usize },
    Line { hi: usize, li: usize },
}

/// Build the per-file `Vec<UnifiedFileItem>` once. Item index i corresponds
/// to rendered row `rendered_offset + 1 + i` in the global virtual scroll
/// space (the `+1` is for the file header rendered separately above).
fn build_unified_file_items(
    file: &ProjectGitDiffFile,
    context_mode: DiffContextMode,
) -> Vec<UnifiedFileItem> {
    let mut items = Vec::new();
    for (hi, hunk) in file.hunks.iter().enumerate() {
        if context_mode == DiffContextMode::Hunks {
            items.push(UnifiedFileItem::HunkHeader { hi });
        }
        for li in 0..hunk.lines.len() {
            items.push(UnifiedFileItem::Line { hi, li });
        }
    }
    items
}

/// Per-file unified virtualized renderer. Reads the global `DiffScroll`
/// context, computes its own visible window clipped to its row range, and
/// emits top/bottom spacers + a `<For>` over visible items so off-screen
/// rows incur zero DOM/paint cost. Falls through to the non-virtualized
/// path for tiny files (preserves DOM shape for layout assertions).
struct UnifiedVirtualizedArgs {
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    rendered_offset: usize,
    decorations: DiffDecorations,
}

fn render_unified_virtualized(args: UnifiedVirtualizedArgs) -> AnyView {
    let UnifiedVirtualizedArgs {
        file,
        hunk_offsets,
        context_mode,
        scope,
        root,
        relative_path,
        rendered_offset,
        decorations,
    } = args;
    let _ruv_t0 = crate::perf::now_ms();
    let _ruv_key = format!("diff:{}:{relative_path}", root.0);
    let items = Arc::new(build_unified_file_items(&file, context_mode));
    let total_items = items.len();
    crate::perf::log_phase(
        "diff_open",
        "ruv_build_items",
        &_ruv_key,
        &format!(
            " items={total_items} took={:.1}ms",
            crate::perf::now_ms() - _ruv_t0
        ),
    );

    if total_items < VIRTUALIZE_THRESHOLD {
        // Small file: render every row up front. Keeps DOM shape identical
        // to the pre-virtualization path for layout-assertion tests.
        return render_unified_hunks(
            file,
            hunk_offsets,
            context_mode,
            scope,
            root,
            relative_path,
            decorations,
        );
    }

    let scroll = expect_context::<DiffScroll>();
    let file_arc = Arc::new(file);
    let hunk_offsets = Arc::new(hunk_offsets);

    // Tokenization runs in the highlight worker (separate JS thread) so
    // the main thread isn't blocked. Each line gets its OWN reactive
    // signal — when the worker streams a chunk back, only the lines
    // covered by that chunk re-render, instead of every visible row
    // being invalidated by a single shared signal.
    //
    // Without these two together, opening a 3000-line Rust diff caused
    // 200–470 ms main-thread freezes recurring for ~3.5 s (every
    // 50-line worker chunk re-rendered ~100 visible rows; plus the
    // tokenization itself ran on the main thread).
    let syntax = syntax_for_path(&relative_path);
    let perf_key_u = format!("diff:{}:{relative_path}", root.0);
    let total_lines: usize = file_arc.hunks.iter().map(|h| h.lines.len()).sum();
    const HIGHLIGHT_LINE_CAP: usize = 50_000;

    let _signals_t0 = crate::perf::now_ms();
    let per_line_signals: Vec<Vec<ArcRwSignal<Option<LineTokens>>>> = file_arc
        .hunks
        .iter()
        .map(|h| (0..h.lines.len()).map(|_| ArcRwSignal::new(None)).collect())
        .collect();
    crate::perf::log_phase(
        "diff_open",
        "ruv_alloc_signals",
        &perf_key_u,
        &format!(
            " lines={total_lines} took={:.1}ms",
            crate::perf::now_ms() - _signals_t0
        ),
    );

    if let Some(syn) = syntax
        && total_lines <= HIGHLIGHT_LINE_CAP
    {
        // Build the flat (hi, li) → flat_index mapping the worker's
        // start-index responses get translated through.
        let flat_to_pos: Vec<(usize, usize)> = file_arc
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(hi, h)| (0..h.lines.len()).map(move |li| (hi, li)))
            .collect();

        // Snapshot lines for the worker. One String alloc per line — for
        // a 3000-line diff that's ~3 ms of work, well below the
        // perceptual threshold and amortized by getting the work off
        // the main thread.
        let lines_owned: Vec<String> = file_arc
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.text.clone()))
            .collect();

        // Per-line signal handles for the worker callback.
        let signals_for_cb: Vec<ArcRwSignal<Option<LineTokens>>> = file_arc
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(hi, h)| {
                let row = &per_line_signals[hi];
                (0..h.lines.len()).map(move |li| row[li].clone())
            })
            .collect();

        let syntax_name = syntax_token_for_path(&relative_path)
            .unwrap_or("")
            .to_owned();
        let theme_name = expect_context::<AppState>().syntax_theme.get_untracked();
        let key_for_done = perf_key_u.clone();
        let key_for_first = perf_key_u.clone();
        let total_for_done = total_lines;
        let task_t0 = crate::perf::now_ms();
        let first_chunk_logged = std::rc::Rc::new(std::cell::Cell::new(false));
        let first_logged_for_cb = first_chunk_logged.clone();

        // Prefer the worker. Fall back to the main-thread chunked path
        // if the worker can't be spawned (wasm-bindgen-test environment,
        // or worker init failure). The fallback also yields between
        // chunks and uses per-line signals so a fallback file open
        // still doesn't lock up the main thread the way the prior
        // shared-signal design did.
        if let Some(client) = crate::highlight_worker::shared() {
            let on_chunk = Box::new(move |start: usize, tokens: Vec<LineTokens>| {
                if !first_logged_for_cb.get() {
                    first_logged_for_cb.set(true);
                    let dt = crate::perf::now_ms() - task_t0;
                    crate::perf::log_phase(
                        "diff_open",
                        "hl_first_chunk",
                        &key_for_first,
                        &format!(" through={} took={dt:.1}ms", start + tokens.len()),
                    );
                }
                for (offset, toks) in tokens.into_iter().enumerate() {
                    let idx = start + offset;
                    if let Some(sig) = signals_for_cb.get(idx) {
                        sig.set(Some(toks));
                    }
                }
            });
            let on_done = Box::new(move || {
                let dt = crate::perf::now_ms() - task_t0;
                crate::perf::log_phase(
                    "diff_open",
                    "hl_finished",
                    &key_for_done,
                    &format!(" lines={total_for_done} took={dt:.1}ms via=worker"),
                );
            });
            let _ = syn;
            let _ = flat_to_pos;
            let task_id = client.highlight_file_concurrent(
                if syntax_name.is_empty() {
                    relative_path.clone()
                } else {
                    syntax_name
                },
                theme_name,
                lines_owned,
                on_chunk,
                on_done,
            );
            on_cleanup(move || {
                if let Some(client) = crate::highlight_worker::shared() {
                    client.cancel_task(task_id);
                }
            });
        } else {
            spawn_local(run_fallback_diff_highlight(
                syn,
                lines_owned,
                signals_for_cb,
                perf_key_u.clone(),
                task_t0,
            ));
        }
    }

    let hunk_tokens = HunkTokensView::new(per_line_signals);

    let visible_window: Memo<(usize, usize)> = Memo::new(move |_| {
        let lh = scroll.line_height.get().max(1.0);
        let st = scroll.scroll_top.get();
        let vh = scroll.viewport_height.get();
        // File header sits at row `rendered_offset`; items occupy
        // `rendered_offset + 1 ..= rendered_offset + total_items`. Map
        // global scroll into local item indices.
        let file_first_item_row = rendered_offset + 1;
        let global_visible_first = ((st - OVERSCAN_LINES * lh) / lh).floor().max(0.0) as i64;
        let global_visible_last = ((st + vh + OVERSCAN_LINES * lh) / lh).ceil() as i64;
        let local_start = (global_visible_first - file_first_item_row as i64).max(0) as usize;
        let local_end = (global_visible_last - file_first_item_row as i64).max(0) as usize;
        let local_start = local_start.min(total_items);
        let local_end = local_end.min(total_items);
        (local_start, local_end)
    });

    let items_for_each = items.clone();
    let lh_for_top = scroll.line_height;
    let lh_for_bottom = scroll.line_height;
    let window_for_top = visible_window;
    let window_for_bottom = visible_window;

    view! {
        <div class="diff-hunks">
            {move || {
                let (start, _) = window_for_top.get();
                let h = start as f64 * lh_for_top.get();
                (h > 0.0).then(|| view! {
                    <div class="diff-virt-spacer" style=format!("height: {h}px;")></div>
                })
            }}
            <For
                each=move || {
                    let (s, e) = visible_window.get();
                    (s..e).collect::<Vec<usize>>()
                }
                key=|i| *i
                let:i
            >
                {
                    let item = items_for_each[i].clone();
                    let file = file_arc.clone();
                    let hunk_offsets = hunk_offsets.clone();
                    let hunk_tokens = hunk_tokens.clone();
                    let root = root.clone();
                    let path = relative_path.clone();
                    let decorations = decorations.clone();
                    render_unified_item(
                        &item,
                        file.as_ref(),
                        hunk_offsets.as_ref(),
                        &hunk_tokens,
                        context_mode,
                        scope,
                        &root,
                        &path,
                        &decorations,
                    )
                }
            </For>
            {move || {
                let (_, end) = window_for_bottom.get();
                let h = total_items.saturating_sub(end) as f64 * lh_for_bottom.get();
                (h > 0.0).then(|| view! {
                    <div class="diff-virt-spacer" style=format!("height: {h}px;")></div>
                })
            }}
        </div>
    }
    .into_any()
}

/// Render a single item from the per-file unified items list.
#[allow(clippy::too_many_arguments)]
fn render_unified_item(
    item: &UnifiedFileItem,
    file: &ProjectGitDiffFile,
    hunk_offsets: &[usize],
    hunk_tokens: &HunkTokensView,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: &ProjectRootPath,
    relative_path: &str,
    decorations: &DiffDecorations,
) -> AnyView {
    match *item {
        UnifiedFileItem::HunkHeader { hi } => {
            let hunk = &file.hunks[hi];
            let header = hunk_header_label(hunk);
            let show_stage =
                scope == ProjectDiffScope::Unstaged && context_mode == DiffContextMode::Hunks;
            let stage_btn = if show_stage {
                let r = root.clone();
                let p = relative_path.to_owned();
                let h = hunk.hunk_id.clone();
                Some(view! {
                    <button
                        class="diff-hunk-stage-btn"
                        title="Stage hunk"
                        on:click=move |_| stage_hunk(r.clone(), p.clone(), h.clone())
                    >
                        "+"
                    </button>
                })
            } else {
                None
            };
            view! {
                <div class="diff-hunk-header">
                    {header}
                    {stage_btn}
                </div>
            }
            .into_any()
        }
        UnifiedFileItem::Line { hi, li } => {
            let hunk = &file.hunks[hi];
            let line = &hunk.lines[li];
            let search_idx = hunk_offsets[hi] + li;
            let base_class = line_class(line.kind);
            let prefix = line_prefix(line.kind);
            let old_str = line
                .old_line_number
                .map(|n| n.to_string())
                .unwrap_or_default();
            let new_str = line
                .new_line_number
                .map(|n| n.to_string())
                .unwrap_or_default();
            let text = line.text.clone();
            let hunk_tokens_clone = hunk_tokens.clone();
            let find = use_context::<FindState>();
            let find_for_class = find.clone();
            let find_for_text = find;
            let kind = line.kind;
            let (anchor_side, anchor_line_no) = match kind {
                ProjectGitDiffLineKind::Removed => (ReviewDiffSide::Old, line.old_line_number),
                ProjectGitDiffLineKind::Added | ProjectGitDiffLineKind::Context => (
                    ReviewDiffSide::New,
                    line.new_line_number.or(line.old_line_number),
                ),
            };
            let pointer_down_cb = decorations.gutter_pointer_down.clone();
            let extra_class_cb = decorations.line_extra_class.clone();
            let line_decoration_cb = decorations.decoration_below_line.clone();
            let class_root = root.clone();
            let class_path = relative_path.to_owned();
            let decoration_root = root.clone();
            let decoration_path = relative_path.to_owned();
            let pdown_root = root.clone();
            let pdown_path = relative_path.to_owned();
            let line_class_closure = move || {
                let mut s = diff_line_class(base_class, search_idx, &find_for_class);
                if let (Some(cb), Some(n)) = (extra_class_cb.as_ref(), anchor_line_no)
                    && let Some(extra) = cb(class_root.clone(), class_path.clone(), anchor_side, n)
                {
                    s.push(' ');
                    s.push_str(extra);
                }
                s
            };
            let anchor_side_str = match anchor_side {
                ReviewDiffSide::Old => "old",
                ReviewDiffSide::New => "new",
            };
            let anchor_line_attr = anchor_line_no.map(|n| n.to_string()).unwrap_or_default();
            // Per-side line numbers for drag-selection: lets the
            // window-level pointermove listener look up "what line is the
            // cursor over on the side the drag started on?" even when the
            // cursor is over a row whose primary anchor side doesn't
            // match (e.g. drag started on +new, cursor over a Removed
            // line). Empty string means the row has no counterpart on
            // that side.
            let anchor_old_line_attr = line
                .old_line_number
                .map(|n| n.to_string())
                .unwrap_or_default();
            let anchor_new_line_attr = line
                .new_line_number
                .map(|n| n.to_string())
                .unwrap_or_default();
            let pointer_active = pointer_down_cb.is_some() && anchor_line_no.is_some();
            let old_clickable = pointer_active && anchor_side == ReviewDiffSide::Old;
            let new_clickable = pointer_active && anchor_side == ReviewDiffSide::New;
            let old_class = if old_clickable {
                "diff-gutter diff-gutter-old diff-gutter-clickable"
            } else {
                "diff-gutter diff-gutter-old"
            };
            let new_class = if new_clickable {
                "diff-gutter diff-gutter-new diff-gutter-clickable"
            } else {
                "diff-gutter diff-gutter-new"
            };
            let cb_for_old = pointer_down_cb.clone();
            let cb_for_new = pointer_down_cb.clone();
            let pdown_root_old = pdown_root.clone();
            let pdown_path_old = pdown_path.clone();
            let pdown_root_new = pdown_root;
            let pdown_path_new = pdown_path;
            view! {
                <>
                    <div
                        class=line_class_closure
                        data-find-idx=search_idx
                        data-anchor-side=anchor_side_str
                        data-anchor-line=anchor_line_attr
                        data-anchor-old-line=anchor_old_line_attr
                        data-anchor-new-line=anchor_new_line_attr
                    >
                        <span
                            class=old_class
                            data-line-num=old_str
                            title=if old_clickable { "Click or drag to comment" } else { "" }
                            on:pointerdown=move |ev: web_sys::PointerEvent| {
                                if !old_clickable { return; }
                                let Some(cb) = cb_for_old.as_ref() else { return };
                                let Some(n) = anchor_line_no else { return };
                                ev.prevent_default();
                                cb(pdown_root_old.clone(), pdown_path_old.clone(), ReviewDiffSide::Old, n);
                            }
                        ></span>
                        <span
                            class=new_class
                            data-line-num=new_str
                            title=if new_clickable { "Click or drag to comment" } else { "" }
                            on:pointerdown=move |ev: web_sys::PointerEvent| {
                                if !new_clickable { return; }
                                let Some(cb) = cb_for_new.as_ref() else { return };
                                let Some(n) = anchor_line_no else { return };
                                ev.prevent_default();
                                cb(pdown_root_new.clone(), pdown_path_new.clone(), ReviewDiffSide::New, n);
                            }
                        ></span>
                        <span class="diff-prefix">{prefix}</span>
                        {move || {
                            // Reactive read so the row re-renders as
                            // chunked syntax tokens land via the lazy
                            // ArcRwSignal. For Eager paths this is a
                            // plain Arc lookup with no signal track.
                            let tokens = hunk_tokens_clone.read(hi, li);
                            render_diff_text(&text, tokens.as_ref(), search_idx, &find_for_text)
                        }}
                    </div>
                    {move || line_decoration_cb.as_ref().and_then(|cb| {
                        let n = anchor_line_no?;
                        cb(decoration_root.clone(), decoration_path.clone(), anchor_side, n)
                    })}
                </>
            }
            .into_any()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SbsSide {
    Left,
    Right,
}

/// Per-hunk dual highlight: `(old_per_line, new_per_line)`.
/// Per-hunk pair of per-line reactive token signals — one entry per
/// hunk line on each side. `None` at an index means that line never
/// appears on that side (e.g. an Added line on the Old side).
type DualHunkTokenSignals = (
    Vec<Option<ArcRwSignal<Option<LineTokens>>>>,
    Vec<Option<ArcRwSignal<Option<LineTokens>>>>,
);

#[allow(clippy::too_many_arguments)]
fn render_sbs_panes(
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    split: RwSignal<f64>,
    decorations: DiffDecorations,
) -> AnyView {
    let pair_ref = NodeRef::<leptos::html::Div>::new();
    let left_pane_ref = NodeRef::<leptos::html::Div>::new();
    let right_pane_ref = NodeRef::<leptos::html::Div>::new();

    // Per-pane scrollport observers: review comment decorations
    // render inside whichever pane matches their anchor side, so
    // they need the pane's own width (not the wider diff-content
    // width) to stay inside the visible viewport in SBS mode.
    install_scrollport_width_observer(left_pane_ref);
    install_scrollport_width_observer(right_pane_ref);

    // No synchronous tokenization on the main thread — same architecture
    // as `render_sbs_virtualized`. Per-line signals are allocated up
    // front; the worker streams tokens into them. The eager path used
    // to call `compute_hunk_tokens_dual` here, blocking the main
    // thread for ~3 s on a 2000-line file in SBS mode.
    let syntax = syntax_for_path(&relative_path);
    let perf_key = format!("diff:{}:{relative_path}", root.0);
    let total_lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();

    let mut hunk_old_signals: Vec<Vec<Option<ArcRwSignal<Option<LineTokens>>>>> =
        Vec::with_capacity(file.hunks.len());
    let mut hunk_new_signals: Vec<Vec<Option<ArcRwSignal<Option<LineTokens>>>>> =
        Vec::with_capacity(file.hunks.len());
    let mut hunk_tokens: Vec<DualHunkTokenSignals> = Vec::with_capacity(file.hunks.len());
    for hunk in file.hunks.iter() {
        let mut old_sigs: Vec<Option<ArcRwSignal<Option<LineTokens>>>> =
            Vec::with_capacity(hunk.lines.len());
        let mut new_sigs: Vec<Option<ArcRwSignal<Option<LineTokens>>>> =
            Vec::with_capacity(hunk.lines.len());
        for line in &hunk.lines {
            old_sigs.push(match line.kind {
                ProjectGitDiffLineKind::Removed | ProjectGitDiffLineKind::Context => {
                    Some(ArcRwSignal::new(None))
                }
                ProjectGitDiffLineKind::Added => None,
            });
            new_sigs.push(match line.kind {
                ProjectGitDiffLineKind::Added | ProjectGitDiffLineKind::Context => {
                    Some(ArcRwSignal::new(None))
                }
                ProjectGitDiffLineKind::Removed => None,
            });
        }
        hunk_old_signals.push(old_sigs.clone());
        hunk_new_signals.push(new_sigs.clone());
        hunk_tokens.push((old_sigs, new_sigs));
    }
    crate::perf::log_phase(
        "diff_open",
        "sbs_pairs_built",
        &perf_key,
        &format!(" hunks={} lines={total_lines}", file.hunks.len()),
    );

    if syntax.is_some() {
        // Spawn both sides independently. Each returned task is cancelled
        // by its view cleanup, so another mounted file does not clobber
        // this file's highlighting.
        spawn_sbs_side_highlight(
            relative_path.clone(),
            &file.hunks,
            ProjectGitDiffSide::Old,
            &hunk_old_signals,
            perf_key.clone(),
        );
        spawn_sbs_side_highlight(
            relative_path.clone(),
            &file.hunks,
            ProjectGitDiffSide::New,
            &hunk_new_signals,
            perf_key.clone(),
        );
    }

    let file_left = file.clone();
    let file_right = file.clone();
    let offsets_left = hunk_offsets.clone();
    let offsets_right = hunk_offsets.clone();
    let root_left = root.clone();
    let root_right = root.clone();
    let path_left = relative_path.clone();
    let path_right = relative_path.clone();
    let tokens_left = hunk_tokens.clone();
    let tokens_right = hunk_tokens;

    let left_style = move || {
        let pct = (split.get() * 100.0).clamp(0.0, 100.0);
        format!("flex: 0 0 {pct:.4}%")
    };

    let on_divider_mousedown = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        start_divider_drag(pair_ref, split);
    };

    // Same horizontal scroll-sync handlers as in `render_sbs_virtualized`.
    // Both code paths use the same `.diff-pane` selector so the CSS
    // `scrollbar-width: none` applies to both; the JS sync below makes
    // the panes scroll together horizontally so corresponding columns
    // stay aligned for visual diff comparison.
    let on_left_scroll = move |_: web_sys::Event| {
        let Some(left) = left_pane_ref.get_untracked() else {
            return;
        };
        let Some(right) = right_pane_ref.get_untracked() else {
            return;
        };
        let lsl = left.scroll_left();
        if right.scroll_left() != lsl {
            right.set_scroll_left(lsl);
        }
    };
    let on_right_scroll = move |_: web_sys::Event| {
        let Some(left) = left_pane_ref.get_untracked() else {
            return;
        };
        let Some(right) = right_pane_ref.get_untracked() else {
            return;
        };
        let rsl = right.scroll_left();
        if left.scroll_left() != rsl {
            left.set_scroll_left(rsl);
        }
    };

    view! {
        <div class="diff-pair" node_ref=pair_ref>
            <div
                class="diff-pane diff-pane-left"
                style=left_style
                node_ref=left_pane_ref
                on:scroll=on_left_scroll
            >
                {render_sbs_pane_content(
                    SbsSide::Left,
                    file_left,
                    offsets_left,
                    context_mode,
                    scope,
                    root_left,
                    path_left,
                    tokens_left,
                    decorations.clone(),
                )}
            </div>
            <div
                class="diff-divider"
                title="Drag to resize"
                on:mousedown=on_divider_mousedown
            ></div>
            <div
                class="diff-pane diff-pane-right"
                node_ref=right_pane_ref
                on:scroll=on_right_scroll
            >
                {render_sbs_pane_content(
                    SbsSide::Right,
                    file_right,
                    offsets_right,
                    context_mode,
                    scope,
                    root_right,
                    path_right,
                    tokens_right,
                    decorations,
                )}
            </div>
        </div>
    }
    .into_any()
}

#[allow(clippy::too_many_arguments)]
fn render_sbs_pane_content(
    side: SbsSide,
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    hunk_tokens: Vec<DualHunkTokenSignals>,
    decorations: DiffDecorations,
) -> AnyView {
    let find = use_context::<FindState>();
    let show_stage_btn = scope == ProjectDiffScope::Unstaged && side == SbsSide::Right;
    let show_header = context_mode == DiffContextMode::Hunks;

    let mut hunk_views: Vec<AnyView> = Vec::with_capacity(file.hunks.len());
    for (hi, hunk) in file.hunks.into_iter().enumerate() {
        let line_offset = hunk_offsets[hi];
        let header = hunk_header_label(&hunk);
        let hunk_id = hunk.hunk_id.clone();
        let lines = hunk.lines.clone();
        let indices = sbs_search_indices(&lines, line_offset);
        let (old_sigs, new_sigs) = hunk_tokens[hi].clone();
        let rows = pair_lines_side_by_side_with_token_signals(lines, &old_sigs, &new_sigs);

        let cell_views: Vec<AnyView> = rows
            .into_iter()
            .zip(indices)
            .map(|(row, (left_idx, right_idx))| {
                render_sbs_paired_row(
                    row,
                    side,
                    left_idx,
                    right_idx,
                    find.clone(),
                    &root,
                    &relative_path,
                    &decorations,
                )
            })
            .collect();

        let header_view = if show_header {
            let stage_btn = if show_stage_btn {
                let r = root.clone();
                let p = relative_path.clone();
                let h = hunk_id.clone();
                Some(view! {
                    <button
                        class="diff-hunk-stage-btn"
                        title="Stage hunk"
                        on:click=move |_| stage_hunk(r.clone(), p.clone(), h.clone())
                    >
                        "+"
                    </button>
                })
            } else {
                None
            };
            Some(view! {
                <div class="diff-hunk-header">
                    {header}
                    {stage_btn}
                </div>
            })
        } else {
            None
        };

        hunk_views.push(
            view! {
                <div class="diff-hunk diff-hunk-side-by-side">
                    {header_view}
                    {cell_views}
                </div>
            }
            .into_any(),
        );
    }

    hunk_views.into_any()
}

/// Per-hunk pre-pairing for SBS virtualization: the paired rows
/// (`pair_lines_side_by_side_with_tokens` output) plus the per-row
/// (left_idx, right_idx) search indices used by find highlighting.
type HunkPairs = (Vec<SideBySideRow>, Vec<(Option<usize>, Option<usize>)>);

/// One vertical row inside a virtualized SBS file (after the file header,
/// which is rendered separately above the `.diff-pair`). PairedRow indices
/// are into the corresponding hunk's pre-pairing.
#[derive(Clone)]
enum SbsItem {
    HunkHeader { hi: usize },
    PairedRow { hi: usize, ri: usize },
}

fn build_sbs_file_items(
    _file: &ProjectGitDiffFile,
    hunk_pairs: &[HunkPairs],
    context_mode: DiffContextMode,
) -> Vec<SbsItem> {
    let mut items = Vec::new();
    for (hi, (rows, _)) in hunk_pairs.iter().enumerate() {
        if context_mode == DiffContextMode::Hunks {
            items.push(SbsItem::HunkHeader { hi });
        }
        for ri in 0..rows.len() {
            items.push(SbsItem::PairedRow { hi, ri });
        }
    }
    items
}

/// Per-file SBS virtualized renderer. Pre-pairs every hunk, builds an
/// item list, and emits a `.diff-pair` whose left and right panes both
/// render top spacer + a `<For>` over the same visible-window slice +
/// bottom spacer. Both panes share the visible_window memo so they stay
/// row-aligned even as scroll progresses. Falls through to the eager
/// `render_sbs_panes` path for tiny diffs.
#[allow(clippy::too_many_arguments)]
fn render_sbs_virtualized(
    file: ProjectGitDiffFile,
    hunk_offsets: Vec<usize>,
    context_mode: DiffContextMode,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    split: RwSignal<f64>,
    rendered_offset: usize,
    decorations: DiffDecorations,
) -> AnyView {
    // Pair rows + search indices synchronously (cheap), but do ZERO syntect
    // work on the main thread. Each line gets its own old-side and
    // new-side `ArcRwSignal<Option<LineTokens>>`; the worker streams
    // tokens in via per-line signal updates so only the rows that
    // actually changed re-render. This mirrors the unified path's
    // architecture — without it, opening a 2000-line file in SBS mode
    // froze the main thread for ~3.4 s while compute_hunk_tokens_dual
    // ran inline.
    let sbs_t0 = crate::perf::now_ms();
    let syntax = syntax_for_path(&relative_path);
    let perf_key = format!("diff:{}:{relative_path}", root.0);
    let mut hunk_pairs: Vec<HunkPairs> = Vec::with_capacity(file.hunks.len());
    let mut total_lines = 0usize;
    // For each hunk, allocate per-line signals AND build the pair list
    // referencing those signals. The pairing function fills `tokens` on
    // each cell with a clone of the signal handle for that line/side.
    let mut hunk_old_signals: Vec<Vec<Option<ArcRwSignal<Option<LineTokens>>>>> =
        Vec::with_capacity(file.hunks.len());
    let mut hunk_new_signals: Vec<Vec<Option<ArcRwSignal<Option<LineTokens>>>>> =
        Vec::with_capacity(file.hunks.len());
    for (hi, hunk) in file.hunks.iter().enumerate() {
        let line_offset = hunk_offsets[hi];
        let lines = hunk.lines.clone();
        let indices = sbs_search_indices(&lines, line_offset);
        let n = hunk.lines.len();
        // Build per-line signal slots. Old slot is Some only for
        // Removed/Context (the lines that appear on the left); New slot
        // is Some only for Added/Context. This matches what
        // compute_hunk_tokens_dual returns and avoids allocating
        // unreachable signals.
        let mut old_sigs: Vec<Option<ArcRwSignal<Option<LineTokens>>>> = Vec::with_capacity(n);
        let mut new_sigs: Vec<Option<ArcRwSignal<Option<LineTokens>>>> = Vec::with_capacity(n);
        for line in &hunk.lines {
            old_sigs.push(match line.kind {
                ProjectGitDiffLineKind::Removed | ProjectGitDiffLineKind::Context => {
                    Some(ArcRwSignal::new(None))
                }
                ProjectGitDiffLineKind::Added => None,
            });
            new_sigs.push(match line.kind {
                ProjectGitDiffLineKind::Added | ProjectGitDiffLineKind::Context => {
                    Some(ArcRwSignal::new(None))
                }
                ProjectGitDiffLineKind::Removed => None,
            });
        }
        total_lines += n;
        let pairs = pair_lines_side_by_side_with_token_signals(lines, &old_sigs, &new_sigs);
        hunk_pairs.push((pairs, indices));
        hunk_old_signals.push(old_sigs);
        hunk_new_signals.push(new_sigs);
    }
    let hunk_pairs = Arc::new(hunk_pairs);
    let sbs_dt = crate::perf::now_ms() - sbs_t0;
    crate::perf::log_phase(
        "diff_open",
        "sbs_pairs_built",
        &perf_key,
        &format!(
            " hunks={} lines={total_lines} took={sbs_dt:.1}ms",
            file.hunks.len(),
        ),
    );

    // Spawn ONE worker request per side per file (not per hunk). The
    // worker's `active_task` is single-slot, so per-hunk requests would
    // cancel earlier hunks before they emitted any chunks — leaving
    // most of the file uncolored. One request per side ships every
    // hunk's lines for that side as a single stream; the callback maps
    // chunks back to (hunk_idx, line_idx) for the per-line signals.
    if syntax.is_some() {
        // Spawn both sides independently. Each returned task is cancelled
        // by its view cleanup, so another mounted file does not clobber
        // this file's highlighting.
        spawn_sbs_side_highlight(
            relative_path.clone(),
            &file.hunks,
            ProjectGitDiffSide::Old,
            &hunk_old_signals,
            perf_key.clone(),
        );
        spawn_sbs_side_highlight(
            relative_path.clone(),
            &file.hunks,
            ProjectGitDiffSide::New,
            &hunk_new_signals,
            perf_key.clone(),
        );
    }

    let items = Arc::new(build_sbs_file_items(&file, &hunk_pairs, context_mode));
    let total_items = items.len();

    if total_items < VIRTUALIZE_THRESHOLD {
        // Small file: keep DOM shape identical to pre-virtualization.
        return render_sbs_panes(
            file,
            hunk_offsets,
            context_mode,
            scope,
            root,
            relative_path,
            split,
            decorations,
        );
    }

    let scroll = expect_context::<DiffScroll>();
    let pair_ref = NodeRef::<leptos::html::Div>::new();
    let left_pane_ref = NodeRef::<leptos::html::Div>::new();
    let right_pane_ref = NodeRef::<leptos::html::Div>::new();

    // Per-pane scrollport observers: see the eager `render_sbs_panes`
    // path for the rationale. SBS comment decorations render inside
    // their anchor-side pane and must clamp to the pane width.
    install_scrollport_width_observer(left_pane_ref);
    install_scrollport_width_observer(right_pane_ref);

    // Horizontal scroll-sync handlers wired directly via `on:scroll`
    // below. When either pane scrolls, copy scrollLeft to the other
    // pane. The "set only if different" check breaks the feedback loop
    // — once both panes match, the other pane's scroll-event handler
    // sees equal values and exits.
    //
    // Goal: make SBS panes feel like a single horizontally-scrollable
    // surface (combined with the hidden CSS scrollbars on `.diff-pane`)
    // so corresponding columns of code stay visually aligned even on
    // wide-line diffs.
    let on_left_scroll = move |_: web_sys::Event| {
        let Some(left) = left_pane_ref.get_untracked() else {
            return;
        };
        let Some(right) = right_pane_ref.get_untracked() else {
            return;
        };
        let lsl = left.scroll_left();
        if right.scroll_left() != lsl {
            right.set_scroll_left(lsl);
        }
    };
    let on_right_scroll = move |_: web_sys::Event| {
        let Some(left) = left_pane_ref.get_untracked() else {
            return;
        };
        let Some(right) = right_pane_ref.get_untracked() else {
            return;
        };
        let rsl = right.scroll_left();
        if left.scroll_left() != rsl {
            left.set_scroll_left(rsl);
        }
    };

    let visible_window: Memo<(usize, usize)> = Memo::new(move |_| {
        let lh = scroll.line_height.get().max(1.0);
        let st = scroll.scroll_top.get();
        let vh = scroll.viewport_height.get();
        let file_first_item_row = rendered_offset + 1;
        let global_visible_first = ((st - OVERSCAN_LINES * lh) / lh).floor().max(0.0) as i64;
        let global_visible_last = ((st + vh + OVERSCAN_LINES * lh) / lh).ceil() as i64;
        let local_start = (global_visible_first - file_first_item_row as i64).max(0) as usize;
        let local_end = (global_visible_last - file_first_item_row as i64).max(0) as usize;
        let local_start = local_start.min(total_items);
        let local_end = local_end.min(total_items);
        (local_start, local_end)
    });

    let left_style = move || {
        let pct = (split.get() * 100.0).clamp(0.0, 100.0);
        format!("flex: 0 0 {pct:.4}%")
    };

    let on_divider_mousedown = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        start_divider_drag(pair_ref, split);
    };

    let pane_left = render_sbs_pane_virtualized(
        SbsSide::Left,
        file.clone(),
        hunk_pairs.clone(),
        items.clone(),
        visible_window,
        scroll,
        scope,
        context_mode,
        root.clone(),
        relative_path.clone(),
        decorations.clone(),
    );
    let pane_right = render_sbs_pane_virtualized(
        SbsSide::Right,
        file,
        hunk_pairs,
        items,
        visible_window,
        scroll,
        scope,
        context_mode,
        root,
        relative_path,
        decorations,
    );

    view! {
        <div class="diff-pair" node_ref=pair_ref>
            <div
                class="diff-pane diff-pane-left"
                style=left_style
                node_ref=left_pane_ref
                on:scroll=on_left_scroll
            >
                {pane_left}
            </div>
            <div
                class="diff-divider"
                title="Drag to resize"
                on:mousedown=on_divider_mousedown
            ></div>
            <div
                class="diff-pane diff-pane-right"
                node_ref=right_pane_ref
                on:scroll=on_right_scroll
            >
                {pane_right}
            </div>
        </div>
    }
    .into_any()
}

/// Render one virtualized SBS pane. Spacers + `<For>` over the visible
/// item slice; per-item rendering picks the cell for `side` and reuses
/// the same gutter / search-aware logic as the eager pane.
#[allow(clippy::too_many_arguments)]
fn render_sbs_pane_virtualized(
    side: SbsSide,
    file: ProjectGitDiffFile,
    hunk_pairs: Arc<Vec<HunkPairs>>,
    items: Arc<Vec<SbsItem>>,
    visible_window: Memo<(usize, usize)>,
    scroll: DiffScroll,
    scope: ProjectDiffScope,
    context_mode: DiffContextMode,
    root: ProjectRootPath,
    relative_path: String,
    decorations: DiffDecorations,
) -> AnyView {
    let total_items = items.len();
    let lh_for_top = scroll.line_height;
    let lh_for_bottom = scroll.line_height;
    let window_top = visible_window;
    let window_bottom = visible_window;

    view! {
        <>
            {move || {
                let (start, _) = window_top.get();
                let h = start as f64 * lh_for_top.get();
                (h > 0.0).then(|| view! {
                    <div class="diff-virt-spacer" style=format!("height: {h}px;")></div>
                })
            }}
            <For
                each=move || {
                    let (s, e) = visible_window.get();
                    (s..e).collect::<Vec<usize>>()
                }
                key=|i| *i
                let:i
            >
                {
                    let item = items[i].clone();
                    let file = file.clone();
                    let hunk_pairs = hunk_pairs.clone();
                    let root = root.clone();
                    let path = relative_path.clone();
                    let decorations = decorations.clone();
                    render_sbs_item(
                        side,
                        &item,
                        &file,
                        hunk_pairs.as_ref(),
                        scope,
                        context_mode,
                        &root,
                        &path,
                        &decorations,
                    )
                }
            </For>
            {move || {
                let (_, end) = window_bottom.get();
                let h = total_items.saturating_sub(end) as f64 * lh_for_bottom.get();
                (h > 0.0).then(|| view! {
                    <div class="diff-virt-spacer" style=format!("height: {h}px;")></div>
                })
            }}
        </>
    }
    .into_any()
}

#[allow(clippy::too_many_arguments)]
fn render_sbs_item(
    side: SbsSide,
    item: &SbsItem,
    file: &ProjectGitDiffFile,
    hunk_pairs: &[HunkPairs],
    scope: ProjectDiffScope,
    context_mode: DiffContextMode,
    root: &ProjectRootPath,
    relative_path: &str,
    decorations: &DiffDecorations,
) -> AnyView {
    match *item {
        SbsItem::HunkHeader { hi } => {
            // Header is identical-ish on both sides; show the stage
            // button only on the right pane (matching the eager path).
            let hunk = &file.hunks[hi];
            let header = hunk_header_label(hunk);
            let show_stage_btn = scope == ProjectDiffScope::Unstaged
                && side == SbsSide::Right
                && context_mode == DiffContextMode::Hunks;
            let stage_btn = if show_stage_btn {
                let r = root.clone();
                let p = relative_path.to_owned();
                let h = hunk.hunk_id.clone();
                Some(view! {
                    <button
                        class="diff-hunk-stage-btn"
                        title="Stage hunk"
                        on:click=move |_| stage_hunk(r.clone(), p.clone(), h.clone())
                    >
                        "+"
                    </button>
                })
            } else {
                None
            };
            view! {
                <div class="diff-hunk-header">
                    {header}
                    {stage_btn}
                </div>
            }
            .into_any()
        }
        SbsItem::PairedRow { hi, ri } => {
            let (rows, indices) = &hunk_pairs[hi];
            let row = rows[ri].clone();
            let (left_idx, right_idx) = indices[ri];
            let find = use_context::<FindState>();
            render_sbs_paired_row(
                row,
                side,
                left_idx,
                right_idx,
                find,
                root,
                relative_path,
                decorations,
            )
        }
    }
}

thread_local! {
    static SBS_DRAG_LISTENERS: RefCell<Option<SbsDragListeners>> = const { RefCell::new(None) };
}

struct SbsDragListeners {
    window: web_sys::Window,
    mousemove: Closure<dyn Fn(web_sys::MouseEvent)>,
    mouseup: Closure<dyn Fn(web_sys::MouseEvent)>,
}

impl SbsDragListeners {
    fn remove(self) {
        let _ = self.window.remove_event_listener_with_callback(
            "mousemove",
            self.mousemove.as_ref().unchecked_ref(),
        );
        let _ = self
            .window
            .remove_event_listener_with_callback("mouseup", self.mouseup.as_ref().unchecked_ref());
    }
}

fn clear_sbs_drag_listeners() {
    SBS_DRAG_LISTENERS.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            handle.remove();
        }
    });
}

fn start_divider_drag(pair_ref: NodeRef<leptos::html::Div>, split: RwSignal<f64>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    clear_sbs_drag_listeners();

    if let Some(body) = window.document().and_then(|d| d.body()) {
        let _ = body.style().set_property("cursor", "col-resize");
        let _ = body.style().set_property("user-select", "none");
    }

    let mousemove = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        let Some(el) = pair_ref.get_untracked() else {
            return;
        };
        let rect = el.get_bounding_client_rect();
        let width = rect.width();
        if width <= 0.0 {
            return;
        }
        let x = ev.client_x() as f64 - rect.left();
        let f = (x / width).clamp(SBS_MIN_FRACTION, SBS_MAX_FRACTION);
        split.set(f);
    });

    let mouseup = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |_: web_sys::MouseEvent| {
        clear_sbs_drag_listeners();
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            let _ = body.style().remove_property("cursor");
            let _ = body.style().remove_property("user-select");
        }
    });

    let _ =
        window.add_event_listener_with_callback("mousemove", mousemove.as_ref().unchecked_ref());
    let _ = window.add_event_listener_with_callback("mouseup", mouseup.as_ref().unchecked_ref());

    SBS_DRAG_LISTENERS.with(|slot| {
        slot.borrow_mut().replace(SbsDragListeners {
            window,
            mousemove,
            mouseup,
        });
    });
}

fn fmt_line_range(start: u32, count: u32) -> String {
    if count == 0 {
        "—".to_string()
    } else if count == 1 {
        start.to_string()
    } else {
        format!("{}-{}", start, start + count - 1)
    }
}

fn hunk_header_label(hunk: &ProjectGitDiffHunk) -> String {
    let old = fmt_line_range(hunk.old_start, hunk.old_count);
    let new = fmt_line_range(hunk.new_start, hunk.new_count);
    format!("Lines {old} → {new}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffFileKind {
    PureAdd,
    PureDelete,
    Mixed,
}

fn classify_diff_file(file: &ProjectGitDiffFile) -> DiffFileKind {
    let mut has_added = false;
    let mut has_removed = false;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                ProjectGitDiffLineKind::Added => has_added = true,
                ProjectGitDiffLineKind::Removed => has_removed = true,
                ProjectGitDiffLineKind::Context => {}
            }
        }
    }
    match (has_added, has_removed) {
        (true, false) => DiffFileKind::PureAdd,
        (false, true) => DiffFileKind::PureDelete,
        _ => DiffFileKind::Mixed,
    }
}

fn line_class(kind: ProjectGitDiffLineKind) -> &'static str {
    match kind {
        ProjectGitDiffLineKind::Context => "diff-line diff-line-context",
        ProjectGitDiffLineKind::Added => "diff-line diff-line-added",
        ProjectGitDiffLineKind::Removed => "diff-line diff-line-removed",
    }
}

fn line_prefix(kind: ProjectGitDiffLineKind) -> &'static str {
    match kind {
        ProjectGitDiffLineKind::Context => " ",
        ProjectGitDiffLineKind::Added => "+",
        ProjectGitDiffLineKind::Removed => "-",
    }
}

#[component]
fn UnifiedHunk(
    hunk: ProjectGitDiffHunk,
    context_mode: DiffContextMode,
    line_offset: usize,
    scope: ProjectDiffScope,
    root: ProjectRootPath,
    relative_path: String,
    #[prop(optional)] decorations: DiffDecorations,
) -> impl IntoView {
    let find = use_context::<FindState>();
    let header = hunk_header_label(&hunk);
    let show_header = context_mode == DiffContextMode::Hunks;
    let show_stage = scope == ProjectDiffScope::Unstaged;
    let hunk_id = hunk.hunk_id.clone();
    let token_lines: Vec<Option<LineTokens>> = match syntax_for_path(&relative_path) {
        Some(syn) => compute_hunk_tokens(&hunk, syn),
        None => vec![None; hunk.lines.len()],
    };

    view! {
        <div class="diff-hunk">
            {show_header.then(|| {
                let stage_root = root.clone();
                let stage_path = relative_path.clone();
                let stage_hunk_id = hunk_id.clone();
                view! {
                    <div class="diff-hunk-header">
                        {header}
                        {show_stage.then(move || {
                            let r = stage_root.clone();
                            let p = stage_path.clone();
                            let h = stage_hunk_id.clone();
                            view! {
                                <button
                                    class="diff-hunk-stage-btn"
                                    title="Stage hunk"
                                    on:click=move |_| {
                                        stage_hunk(r.clone(), p.clone(), h.clone());
                                    }
                                >
                                    "+"
                                </button>
                            }
                        })}
                    </div>
                }
            })}
            {hunk.lines.into_iter().zip(token_lines).enumerate().map(|(i, (line, tokens))| {
                let search_idx = line_offset + i;
                let base_class = line_class(line.kind);
                let prefix = line_prefix(line.kind);
                let old_str = line.old_line_number.map(|n| n.to_string()).unwrap_or_default();
                let new_str = line.new_line_number.map(|n| n.to_string()).unwrap_or_default();
                let text = line.text;
                let find_for_class = find.clone();
                let find_for_text = find.clone();
                let kind = line.kind;
                let (anchor_side, anchor_line_no) = match kind {
                    ProjectGitDiffLineKind::Removed => (ReviewDiffSide::Old, line.old_line_number),
                    ProjectGitDiffLineKind::Added | ProjectGitDiffLineKind::Context => (
                        ReviewDiffSide::New,
                        line.new_line_number.or(line.old_line_number),
                    ),
                };
                let pointer_down_cb = decorations.gutter_pointer_down.clone();
                let extra_class_cb = decorations.line_extra_class.clone();
                let line_decoration_cb = decorations.decoration_below_line.clone();
                let class_root = root.clone();
                let class_path = relative_path.clone();
                let decoration_root = root.clone();
                let decoration_path = relative_path.clone();
                let pdown_root_old = root.clone();
                let pdown_path_old = relative_path.clone();
                let pdown_root_new = root.clone();
                let pdown_path_new = relative_path.clone();
                let line_class_closure = move || {
                    let mut s = diff_line_class(base_class, search_idx, &find_for_class);
                    if let (Some(cb), Some(n)) = (extra_class_cb.as_ref(), anchor_line_no)
                        && let Some(extra) =
                            cb(class_root.clone(), class_path.clone(), anchor_side, n)
                    {
                        s.push(' ');
                        s.push_str(extra);
                    }
                    s
                };
                let anchor_side_str = match anchor_side {
                    ReviewDiffSide::Old => "old",
                    ReviewDiffSide::New => "new",
                };
                let anchor_line_attr = anchor_line_no.map(|n| n.to_string()).unwrap_or_default();
                let anchor_old_line_attr = line
                    .old_line_number
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let anchor_new_line_attr = line
                    .new_line_number
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let pointer_active = pointer_down_cb.is_some() && anchor_line_no.is_some();
                let old_clickable = pointer_active && anchor_side == ReviewDiffSide::Old;
                let new_clickable = pointer_active && anchor_side == ReviewDiffSide::New;
                let old_class = if old_clickable {
                    "diff-gutter diff-gutter-old diff-gutter-clickable"
                } else {
                    "diff-gutter diff-gutter-old"
                };
                let new_class = if new_clickable {
                    "diff-gutter diff-gutter-new diff-gutter-clickable"
                } else {
                    "diff-gutter diff-gutter-new"
                };
                let cb_for_old = pointer_down_cb.clone();
                let cb_for_new = pointer_down_cb;
                view! {
                    <div
                        class=line_class_closure
                        data-find-idx=search_idx
                        data-anchor-side=anchor_side_str
                        data-anchor-line=anchor_line_attr
                        data-anchor-old-line=anchor_old_line_attr
                        data-anchor-new-line=anchor_new_line_attr
                    >
                        <span
                            class=old_class
                            data-line-num=old_str
                            on:pointerdown=move |ev: web_sys::PointerEvent| {
                                if !old_clickable { return; }
                                let Some(cb) = cb_for_old.as_ref() else { return };
                                let Some(n) = anchor_line_no else { return };
                                ev.prevent_default();
                                cb(pdown_root_old.clone(), pdown_path_old.clone(), ReviewDiffSide::Old, n);
                            }
                        ></span>
                        <span
                            class=new_class
                            data-line-num=new_str
                            on:pointerdown=move |ev: web_sys::PointerEvent| {
                                if !new_clickable { return; }
                                let Some(cb) = cb_for_new.as_ref() else { return };
                                let Some(n) = anchor_line_no else { return };
                                ev.prevent_default();
                                cb(pdown_root_new.clone(), pdown_path_new.clone(), ReviewDiffSide::New, n);
                            }
                        ></span>
                        <span class="diff-prefix">{prefix}</span>
                        {move || {
                            let result: AnyView = render_diff_text(&text, tokens.as_ref(), search_idx, &find_for_text);
                            result
                        }}
                    </div>
                    {move || line_decoration_cb.as_ref().and_then(|cb| {
                        let n = anchor_line_no?;
                        cb(decoration_root.clone(), decoration_path.clone(), anchor_side, n)
                    })}
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

/// A single paired row in the side-by-side layout. Either side may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideRow {
    pub left: Option<SideBySideCell>,
    pub right: Option<SideBySideCell>,
}

#[derive(Clone, Debug)]
pub struct SideBySideCell {
    pub kind: ProjectGitDiffLineKind,
    pub line_number: Option<u32>,
    pub text: String,
    /// Static snapshot of the cell's syntax tokens. Populated by the
    /// test-facing pairing helpers so existing tests keep working with
    /// plain values. The runtime renderer prefers `tokens_signal` if
    /// present so it can react to async tokenization landing.
    pub tokens: Option<LineTokens>,
    /// Reactive token slot used by the live renderer. `None` means
    /// "use `tokens` directly". `Some(signal)` means the row should
    /// read from the signal each render so worker-streamed chunks
    /// update only the affected rows instead of forcing a re-render
    /// of the whole window via a shared signal.
    pub tokens_signal: Option<ArcRwSignal<Option<LineTokens>>>,
}

impl PartialEq for SideBySideCell {
    fn eq(&self, other: &Self) -> bool {
        // Compare on user-visible content. `tokens_signal` carries
        // identity, not value, and `tokens` is a render-time cache —
        // either may legitimately differ between two cells that the
        // pairing layer treats as equivalent.
        self.kind == other.kind
            && self.line_number == other.line_number
            && self.text == other.text
            && self.tokens == other.tokens
    }
}

impl Eq for SideBySideCell {}

/// Convenience wrapper used by tests; pairs lines with no syntax tokens.
pub fn pair_lines_side_by_side(lines: Vec<ProjectGitDiffLine>) -> Vec<SideBySideRow> {
    let n = lines.len();
    pair_lines_side_by_side_with_tokens(lines, vec![None; n], vec![None; n])
}

/// Pair the lines of a single hunk into side-by-side rows, attaching the
/// matching old-side / new-side syntax tokens to each cell.
///
/// `old_tokens[i]` and `new_tokens[i]` correspond to `lines[i]`. For Removed
/// lines only `old_tokens[i]` should be `Some`; for Added only `new_tokens[i]`;
/// Context lines may have both populated (with potentially different state).
///
/// Pairing algorithm: walk lines in order; collect consecutive Removed into a
/// left-run and Added into a right-run. On a Context line (or end of hunk),
/// flush the runs: zip their overlap into paired rows and emit the remainder
/// as half-empty rows. Context lines become rows with text on both sides.
pub fn pair_lines_side_by_side_with_tokens(
    lines: Vec<ProjectGitDiffLine>,
    old_tokens: Vec<Option<LineTokens>>,
    new_tokens: Vec<Option<LineTokens>>,
) -> Vec<SideBySideRow> {
    debug_assert_eq!(lines.len(), old_tokens.len());
    debug_assert_eq!(lines.len(), new_tokens.len());

    type Entry = (ProjectGitDiffLine, Option<LineTokens>, Option<LineTokens>);

    let mut rows: Vec<SideBySideRow> = Vec::new();
    let mut removed: Vec<Entry> = Vec::new();
    let mut added: Vec<Entry> = Vec::new();

    let flush =
        |removed: &mut Vec<Entry>, added: &mut Vec<Entry>, rows: &mut Vec<SideBySideRow>| {
            let pair_count = removed.len().min(added.len());
            let rem_iter = std::mem::take(removed);
            let add_iter = std::mem::take(added);
            let mut rem_it = rem_iter.into_iter();
            let mut add_it = add_iter.into_iter();
            for _ in 0..pair_count {
                let (r, r_old, _r_new) = rem_it.next().expect("removed run underflow");
                let (a, _a_old, a_new) = add_it.next().expect("added run underflow");
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: r.kind,
                        line_number: r.old_line_number,
                        text: r.text,
                        tokens: r_old,
                        tokens_signal: None,
                    }),
                    right: Some(SideBySideCell {
                        kind: a.kind,
                        line_number: a.new_line_number,
                        text: a.text,
                        tokens: a_new,
                        tokens_signal: None,
                    }),
                });
            }
            for (r, r_old, _) in rem_it {
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: r.kind,
                        line_number: r.old_line_number,
                        text: r.text,
                        tokens: r_old,
                        tokens_signal: None,
                    }),
                    right: None,
                });
            }
            for (a, _, a_new) in add_it {
                rows.push(SideBySideRow {
                    left: None,
                    right: Some(SideBySideCell {
                        kind: a.kind,
                        line_number: a.new_line_number,
                        text: a.text,
                        tokens: a_new,
                        tokens_signal: None,
                    }),
                });
            }
        };

    for ((line, old_tok), new_tok) in lines.into_iter().zip(old_tokens).zip(new_tokens) {
        match line.kind {
            ProjectGitDiffLineKind::Removed => removed.push((line, old_tok, new_tok)),
            ProjectGitDiffLineKind::Added => added.push((line, old_tok, new_tok)),
            ProjectGitDiffLineKind::Context => {
                flush(&mut removed, &mut added, &mut rows);
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.old_line_number,
                        text: line.text.clone(),
                        tokens: old_tok,
                        tokens_signal: None,
                    }),
                    right: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.new_line_number,
                        text: line.text,
                        tokens: new_tok,
                        tokens_signal: None,
                    }),
                });
            }
        }
    }
    flush(&mut removed, &mut added, &mut rows);
    rows
}

/// Side selector used when shipping a hunk to the worker. Each side is
/// a separate `HighlightFile` request because the parser state across
/// removed/added boundaries diverges between the two streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectGitDiffSide {
    Old,
    New,
}

/// Like `pair_lines_side_by_side_with_tokens` but the per-cell `tokens`
/// field stays None and `tokens_signal` carries the per-line reactive
/// handle. The render layer reads the signal at row-render time so
/// worker-streamed chunks update only the affected rows.
fn pair_lines_side_by_side_with_token_signals(
    lines: Vec<ProjectGitDiffLine>,
    old_signals: &[Option<ArcRwSignal<Option<LineTokens>>>],
    new_signals: &[Option<ArcRwSignal<Option<LineTokens>>>],
) -> Vec<SideBySideRow> {
    debug_assert_eq!(lines.len(), old_signals.len());
    debug_assert_eq!(lines.len(), new_signals.len());

    type Entry = (
        ProjectGitDiffLine,
        Option<ArcRwSignal<Option<LineTokens>>>,
        Option<ArcRwSignal<Option<LineTokens>>>,
    );

    let mut rows: Vec<SideBySideRow> = Vec::new();
    let mut removed: Vec<Entry> = Vec::new();
    let mut added: Vec<Entry> = Vec::new();

    let flush =
        |removed: &mut Vec<Entry>, added: &mut Vec<Entry>, rows: &mut Vec<SideBySideRow>| {
            let pair_count = removed.len().min(added.len());
            let rem_iter = std::mem::take(removed);
            let add_iter = std::mem::take(added);
            let mut rem_it = rem_iter.into_iter();
            let mut add_it = add_iter.into_iter();
            for _ in 0..pair_count {
                let (r, r_old, _r_new) = rem_it.next().expect("removed run underflow");
                let (a, _a_old, a_new) = add_it.next().expect("added run underflow");
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: r.kind,
                        line_number: r.old_line_number,
                        text: r.text,
                        tokens: None,
                        tokens_signal: r_old,
                    }),
                    right: Some(SideBySideCell {
                        kind: a.kind,
                        line_number: a.new_line_number,
                        text: a.text,
                        tokens: None,
                        tokens_signal: a_new,
                    }),
                });
            }
            for (r, r_old, _) in rem_it {
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: r.kind,
                        line_number: r.old_line_number,
                        text: r.text,
                        tokens: None,
                        tokens_signal: r_old,
                    }),
                    right: None,
                });
            }
            for (a, _, a_new) in add_it {
                rows.push(SideBySideRow {
                    left: None,
                    right: Some(SideBySideCell {
                        kind: a.kind,
                        line_number: a.new_line_number,
                        text: a.text,
                        tokens: None,
                        tokens_signal: a_new,
                    }),
                });
            }
        };

    for ((line, old_sig), new_sig) in lines
        .into_iter()
        .zip(old_signals.iter().cloned())
        .zip(new_signals.iter().cloned())
    {
        match line.kind {
            ProjectGitDiffLineKind::Removed => removed.push((line, old_sig, new_sig)),
            ProjectGitDiffLineKind::Added => added.push((line, old_sig, new_sig)),
            ProjectGitDiffLineKind::Context => {
                flush(&mut removed, &mut added, &mut rows);
                rows.push(SideBySideRow {
                    left: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.old_line_number,
                        text: line.text.clone(),
                        tokens: None,
                        tokens_signal: old_sig,
                    }),
                    right: Some(SideBySideCell {
                        kind: ProjectGitDiffLineKind::Context,
                        line_number: line.new_line_number,
                        text: line.text,
                        tokens: None,
                        tokens_signal: new_sig,
                    }),
                });
            }
        }
    }
    flush(&mut removed, &mut added, &mut rows);
    rows
}

/// Ship one side of an ENTIRE FILE (all hunks at once) to the highlight
/// worker. The worker only tracks one active task — calling it per-hunk
/// would cancel earlier hunks before they emit chunks, leaving most of
/// the file uncolored. So we concatenate every included line into one
/// stream and ship a single request per side.
///
/// `hunk_signals[hi]` is the per-line signal slice for hunk `hi`; the
/// worker returns chunks indexed by stream-position, which we map back
/// to (hunk_idx, line_idx) via the parallel `stream_to_pos` table.
fn spawn_sbs_side_highlight(
    relative_path: String,
    hunks: &[ProjectGitDiffHunk],
    side: ProjectGitDiffSide,
    hunk_signals: &[Vec<Option<ArcRwSignal<Option<LineTokens>>>>],
    perf_key: String,
) {
    debug_assert_eq!(hunks.len(), hunk_signals.len());
    let mut stream_lines: Vec<String> = Vec::new();
    // Each entry: (hunk_idx, line_idx_within_hunk).
    let mut stream_to_pos: Vec<(usize, usize)> = Vec::new();
    for (hi, hunk) in hunks.iter().enumerate() {
        for (li, line) in hunk.lines.iter().enumerate() {
            let included = matches!(
                (side, line.kind),
                (ProjectGitDiffSide::Old, ProjectGitDiffLineKind::Removed)
                    | (ProjectGitDiffSide::Old, ProjectGitDiffLineKind::Context)
                    | (ProjectGitDiffSide::New, ProjectGitDiffLineKind::Added)
                    | (ProjectGitDiffSide::New, ProjectGitDiffLineKind::Context)
            );
            if included {
                stream_lines.push(line.text.clone());
                stream_to_pos.push((hi, li));
            }
        }
    }
    if stream_lines.is_empty() {
        return;
    }

    let key = perf_key;
    let task_t0 = crate::perf::now_ms();
    let side_tag = match side {
        ProjectGitDiffSide::Old => "old",
        ProjectGitDiffSide::New => "new",
    };
    let key_for_first = key.clone();
    let key_for_done = key.clone();
    let first_logged = std::rc::Rc::new(std::cell::Cell::new(false));
    let first_for_cb = first_logged.clone();
    let total_for_done = stream_lines.len();

    // Clone the nested signal vec once so the callback owns it.
    let signals_for_cb: Vec<Vec<Option<ArcRwSignal<Option<LineTokens>>>>> = hunk_signals.to_vec();
    let stream_to_pos_for_cb = stream_to_pos;

    if let Some(client) = crate::highlight_worker::shared() {
        let on_chunk = Box::new(move |start: usize, tokens: Vec<LineTokens>| {
            if !first_for_cb.get() {
                first_for_cb.set(true);
                let dt = crate::perf::now_ms() - task_t0;
                crate::perf::log_phase(
                    "diff_open",
                    "sbs_hl_first_chunk",
                    &key_for_first,
                    &format!(
                        " side={side_tag} through={} took={dt:.1}ms",
                        start + tokens.len()
                    ),
                );
            }
            for (offset, toks) in tokens.into_iter().enumerate() {
                let stream_idx = start + offset;
                let Some(&(hi, li)) = stream_to_pos_for_cb.get(stream_idx) else {
                    continue;
                };
                if let Some(Some(sig)) = signals_for_cb.get(hi).and_then(|h| h.get(li)) {
                    sig.set(Some(toks));
                }
            }
        });
        let on_done = Box::new(move || {
            let dt = crate::perf::now_ms() - task_t0;
            crate::perf::log_phase(
                "diff_open",
                "sbs_hl_finished",
                &key_for_done,
                &format!(" side={side_tag} lines={total_for_done} took={dt:.1}ms via=worker"),
            );
        });
        let syntax_name = syntax_token_for_path(&relative_path)
            .unwrap_or("")
            .to_owned();
        let theme_name = expect_context::<AppState>().syntax_theme.get_untracked();
        // Old and New are independent tasks for this file. The cleanup
        // below cancels only this side's task, so other mounted diff
        // files keep their highlighting work.
        let task_id = client.highlight_file_concurrent(
            if syntax_name.is_empty() {
                relative_path
            } else {
                syntax_name
            },
            theme_name,
            stream_lines,
            on_chunk,
            on_done,
        );
        on_cleanup(move || {
            if let Some(client) = crate::highlight_worker::shared() {
                client.cancel_task(task_id);
            }
        });
    } else {
        // No-worker fallback: tokenize on the main thread but yielding
        // between 50-line chunks so the UI doesn't lock up.
        let Some(syn) = syntax_for_path(&relative_path) else {
            return;
        };
        spawn_local(async move {
            next_macrotask().await;
            let mut hl = LineHighlighter::new(syn);
            const CHUNK: usize = 50;
            let mut i = 0usize;
            while i < stream_lines.len() {
                let end = (i + CHUNK).min(stream_lines.len());
                for (j, line) in stream_lines.iter().enumerate().take(end).skip(i) {
                    let toks = hl.highlight_one(line);
                    if let Some(&(hi, li)) = stream_to_pos_for_cb.get(j)
                        && let Some(Some(sig)) = signals_for_cb.get(hi).and_then(|h| h.get(li))
                    {
                        sig.set(Some(toks));
                    }
                }
                if !first_for_cb.get() {
                    first_for_cb.set(true);
                    let dt = crate::perf::now_ms() - task_t0;
                    crate::perf::log_phase(
                        "diff_open",
                        "sbs_hl_first_chunk",
                        &key_for_first,
                        &format!(" side={side_tag} through={end} took={dt:.1}ms via=fallback"),
                    );
                }
                i = end;
                next_macrotask().await;
            }
            let dt = crate::perf::now_ms() - task_t0;
            crate::perf::log_phase(
                "diff_open",
                "sbs_hl_finished",
                &key_for_done,
                &format!(" side={side_tag} lines={total_for_done} took={dt:.1}ms via=fallback"),
            );
        });
    }
}

/// Compute search indices for each cell in the paired side-by-side rows.
///
/// Returns `Vec<(Option<usize>, Option<usize>)>` — one entry per row,
/// giving the flat search index of the left and right cell respectively.
fn sbs_search_indices(
    lines: &[ProjectGitDiffLine],
    line_offset: usize,
) -> Vec<(Option<usize>, Option<usize>)> {
    // Partition original line indices by kind.
    let mut removed_idx: Vec<usize> = Vec::new();
    let mut added_idx: Vec<usize> = Vec::new();
    let mut context_idx: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match line.kind {
            ProjectGitDiffLineKind::Removed => removed_idx.push(line_offset + i),
            ProjectGitDiffLineKind::Added => added_idx.push(line_offset + i),
            ProjectGitDiffLineKind::Context => context_idx.push(line_offset + i),
        }
    }

    // Replay the same pairing logic to assign indices to rows.
    let rows = pair_lines_side_by_side(lines.to_vec());
    let mut ri = 0usize;
    let mut ai = 0usize;
    let mut ci = 0usize;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let is_context = row
            .left
            .as_ref()
            .is_some_and(|c| c.kind == ProjectGitDiffLineKind::Context);
        if is_context {
            let idx = context_idx.get(ci).copied();
            ci += 1;
            out.push((idx, idx));
        } else {
            let li = row.left.as_ref().and_then(|c| {
                if c.kind == ProjectGitDiffLineKind::Removed {
                    let idx = removed_idx.get(ri).copied();
                    ri += 1;
                    idx
                } else {
                    None
                }
            });
            let r_idx = row.right.as_ref().and_then(|c| {
                if c.kind == ProjectGitDiffLineKind::Added {
                    let idx = added_idx.get(ai).copied();
                    ai += 1;
                    idx
                } else {
                    None
                }
            });
            out.push((li, r_idx));
        }
    }
    out
}

/// Render one SBS row (a paired left+right line) for one pane.
///
/// Why this is a single helper instead of "render the cell, then attach
/// decorations later":
/// - Drag-to-select needs `data-anchor-side` and per-side line numbers
///   on the same `.diff-line` the user's pointer can land on, so the
///   window-level `pointermove` listener can identify which side+line
///   the cursor is over even when it's on a Removed-only or Added-only
///   row.
/// - Inline review threads (composer / comment cards / suggestion
///   cards) render *below* the line they're anchored to. The decoration
///   slot has to sit next to the row that just rendered so virtualized
///   mount/unmount keeps it co-located with its anchor row.
///
/// The `pane_side` argument disambiguates the visible cell from the
/// invisible counterpart: for the left pane we read `row.left` for
/// gutter/text and use `Old` as the anchor side; for the right pane
/// we read `row.right` and use `New`. Per-side line attrs always come
/// from the *full* paired row so a drag started on the right pane that
/// passes over a Removed-only row (whose right cell is empty) still
/// degrades cleanly — the listener sees no `data-anchor-new-line` and
/// just leaves the selection range unchanged for that row.
#[allow(clippy::too_many_arguments)]
fn render_sbs_paired_row(
    row: SideBySideRow,
    pane_side: SbsSide,
    left_idx: Option<usize>,
    right_idx: Option<usize>,
    find: Option<FindState>,
    root: &ProjectRootPath,
    relative_path: &str,
    decorations: &DiffDecorations,
) -> AnyView {
    let (cell, search_idx) = match pane_side {
        SbsSide::Left => (row.left.clone(), left_idx),
        SbsSide::Right => (row.right.clone(), right_idx),
    };

    let Some(c) = cell else {
        return view! { <div class="diff-line diff-line-empty"></div> }.into_any();
    };

    let base_class = line_class(c.kind);
    let prefix = line_prefix(c.kind);
    let num = c.line_number.map(|n| n.to_string()).unwrap_or_default();
    let text = c.text;
    let static_tokens = c.tokens;
    let token_signal = c.tokens_signal;
    let find_for_class = find.clone();
    let find_for_text = find;
    let idx_str = search_idx.map(|i| i.to_string()).unwrap_or_default();

    let anchor_side = match pane_side {
        SbsSide::Left => ReviewDiffSide::Old,
        SbsSide::Right => ReviewDiffSide::New,
    };
    let anchor_side_str = match anchor_side {
        ReviewDiffSide::Old => "old",
        ReviewDiffSide::New => "new",
    };
    let anchor_line_no: Option<u32> = c.line_number;
    let anchor_line_attr = anchor_line_no.map(|n| n.to_string()).unwrap_or_default();
    let paired_old_line = row
        .left
        .as_ref()
        .and_then(|c| c.line_number)
        .map(|n| n.to_string())
        .unwrap_or_default();
    let paired_new_line = row
        .right
        .as_ref()
        .and_then(|c| c.line_number)
        .map(|n| n.to_string())
        .unwrap_or_default();

    let pointer_down_cb = decorations.gutter_pointer_down.clone();
    let extra_class_cb = decorations.line_extra_class.clone();
    let line_decoration_cb = decorations.decoration_below_line.clone();
    let pointer_active = pointer_down_cb.is_some() && anchor_line_no.is_some();

    let class_root = root.clone();
    let class_path = relative_path.to_owned();
    let line_class_closure = move || {
        let mut s = if let (Some(idx), Some(find)) = (search_idx, &find_for_class) {
            diff_line_class(base_class, idx, &Some(find.clone()))
        } else {
            base_class.to_string()
        };
        if let (Some(cb), Some(n)) = (extra_class_cb.as_ref(), anchor_line_no)
            && let Some(extra) = cb(class_root.clone(), class_path.clone(), anchor_side, n)
        {
            s.push(' ');
            s.push_str(extra);
        }
        s
    };

    let gutter_class = if pointer_active {
        "diff-gutter diff-gutter-clickable"
    } else {
        "diff-gutter"
    };
    let pdown_root = root.clone();
    let pdown_path = relative_path.to_owned();
    let pdown_cb = pointer_down_cb;
    let on_pointer_down = move |ev: web_sys::PointerEvent| {
        if !pointer_active {
            return;
        }
        let Some(cb) = pdown_cb.as_ref() else { return };
        let Some(n) = anchor_line_no else { return };
        ev.prevent_default();
        cb(pdown_root.clone(), pdown_path.clone(), anchor_side, n);
    };

    let decoration_root = root.clone();
    let decoration_path = relative_path.to_owned();

    view! {
        <>
            <div
                class=line_class_closure
                data-find-idx=idx_str
                data-anchor-side=anchor_side_str
                data-anchor-line=anchor_line_attr
                data-anchor-old-line=paired_old_line
                data-anchor-new-line=paired_new_line
            >
                <span
                    class=gutter_class
                    data-line-num=num
                    title=if pointer_active { "Click or drag to comment" } else { "" }
                    on:pointerdown=on_pointer_down
                ></span>
                <span class="diff-prefix">{prefix}</span>
                {move || {
                    // Reactive read: if the cell carries a token signal
                    // (live SBS path), pull the current value each render
                    // so chunks landing from the worker fill in colour
                    // without remounting the row. Otherwise fall back to
                    // the static snapshot the test API attaches.
                    let live_tokens = token_signal
                        .as_ref()
                        .map(|s| s.get())
                        .unwrap_or_else(|| static_tokens.clone());
                    let result: AnyView = match search_idx {
                        Some(idx) => render_diff_text(&text, live_tokens.as_ref(), idx, &find_for_text),
                        None => render_diff_text_plain(&text, live_tokens.as_ref()),
                    };
                    result
                }}
            </div>
            {move || line_decoration_cb.as_ref().and_then(|cb| {
                let n = anchor_line_no?;
                cb(decoration_root.clone(), decoration_path.clone(), anchor_side, n)
            })}
        </>
    }
    .into_any()
}

// ── Search-aware rendering helpers ──────────────────────────────────────

fn diff_line_class(base: &'static str, search_idx: usize, find: &Option<FindState>) -> String {
    let Some(find) = find else {
        return base.to_string();
    };
    let results = find.results.get();
    if !results.match_set.contains(&search_idx) {
        return base.to_string();
    }
    let active = find.active_index.get();
    if active >= 0 && results.match_lines.get(active as usize) == Some(&search_idx) {
        format!("{base} find-hit-active")
    } else {
        format!("{base} find-hit")
    }
}

fn render_diff_text(
    text: &str,
    tokens: Option<&LineTokens>,
    search_idx: usize,
    find: &Option<FindState>,
) -> AnyView {
    let find_ranges: Option<Vec<(usize, usize)>> = find.as_ref().and_then(|f| {
        let results = f.results.get();
        results.ranges_by_line.get(&search_idx).cloned()
    });
    match (tokens, find_ranges) {
        (None, None) => view! { <span class="diff-text">{text.to_owned()}</span> }.into_any(),
        (None, Some(ranges)) => render_text_with_highlights(text, &ranges).into_any(),
        (Some(toks), None) => render_tokens(toks).into_any(),
        (Some(toks), Some(ranges)) => render_tokens_with_find(text, toks, &ranges).into_any(),
    }
}

fn render_diff_text_plain(text: &str, tokens: Option<&LineTokens>) -> AnyView {
    match tokens {
        Some(toks) => render_tokens(toks).into_any(),
        None => view! { <span class="diff-text">{text.to_owned()}</span> }.into_any(),
    }
}

/// Render syntax tokens as colored `<span>` children inside a `.diff-text`
/// wrapper. Uses inline `style="color:#…"` so we don't have to ship a syntect
/// theme stylesheet — we keep one bundled theme in `syntax_highlight::THEME`.
fn render_tokens(tokens: &LineTokens) -> impl IntoView + use<> {
    let spans: Vec<AnyView> = tokens
        .iter()
        .map(|t| {
            let style = format!("color:{}", color_to_css(t.fg));
            let txt = t.text.clone();
            view! { <span style=style>{txt}</span> }.into_any()
        })
        .collect();
    view! { <span class="diff-text">{spans}</span> }
}

/// Render syntax tokens AND inline find-bar match highlighting on the same
/// line. Walks tokens by byte offset; for each token, splits any overlapping
/// find ranges into nested `<span class="find-inline-match">` siblings while
/// keeping the token's color. Find ranges are pre-clipped to the line in
/// `find_bar`; byte offsets correspond to the concatenated text of all
/// `tokens[i].text` (which equals `text`).
///
/// Safety/robustness:
/// - Each slice index is snapped to the nearest UTF-8 char boundary at-or-before,
///   guarding against `find_bar` ranges that may currently come from JS UTF-16
///   `lastIndex` offsets (TODO upstream: convert JS offsets to Rust byte
///   offsets at source).
/// - Overlapping find ranges are tolerated: the running cursor `p` keeps each
///   byte appearing at most once.
fn render_tokens_with_find(
    text: &str,
    tokens: &LineTokens,
    find_ranges: &[(usize, usize)],
) -> impl IntoView + use<> {
    let mut fragments: Vec<AnyView> = Vec::new();
    let mut byte_pos: usize = 0;
    for tok in tokens {
        let tok_start = byte_pos;
        let tok_end = byte_pos + tok.text.len();
        let style = format!("color:{}", color_to_css(tok.fg));

        let mut sub: Vec<AnyView> = Vec::new();
        let mut p = tok_start;
        for &(rs, re) in find_ranges {
            // Clamp to current cursor (`p`) so overlapping ranges don't
            // duplicate bytes, and to the token bounds.
            let s = snap_char_boundary(text, rs.max(p).max(tok_start));
            let e = snap_char_boundary(text, re.min(tok_end));
            if s >= e {
                continue;
            }
            if s > p {
                let slice = text[p..s].to_owned();
                sub.push(view! { <>{slice}</> }.into_any());
            }
            let slice = text[s..e].to_owned();
            sub.push(view! { <span class="find-inline-match">{slice}</span> }.into_any());
            p = e;
        }
        if p < tok_end {
            let slice = text[p..tok_end].to_owned();
            sub.push(view! { <>{slice}</> }.into_any());
        }
        fragments.push(view! { <span style=style>{sub}</span> }.into_any());
        byte_pos = tok_end;
    }
    view! { <span class="diff-text">{fragments}</span> }
}

/// Round `idx` down to the nearest valid UTF-8 char boundary in `text`.
/// Idempotent on already-aligned indices and on `text.len()`.
fn snap_char_boundary(text: &str, mut idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::DiffViewState;
    use crate::syntax_highlight::{compute_hunk_tokens, syntax_for_path};
    use crate::wasm_test_support::Mounted;
    use leptos::mount::mount_to;
    use protocol::ProjectGitDiffHunk;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Production stylesheet — needed so the scroll container has a real
    /// computed height, otherwise virtualization's viewport math collapses
    /// to zero and every row would render.
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
                "position: absolute; top: 0; left: 0; width: 800px; height: 600px; \
                 display: flex; flex-direction: column;",
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

    /// Build a synthetic diff with one file containing a single hunk of
    /// `n` Added lines. Used by the virtualization regression test.
    fn synth_added_diff(n: usize, root: ProjectRootPath) -> DiffViewState {
        let lines: Vec<ProjectGitDiffLine> = (0..n)
            .map(|i| ProjectGitDiffLine {
                kind: ProjectGitDiffLineKind::Added,
                text: format!("line {i}"),
                old_line_number: None,
                new_line_number: Some((i + 1) as u32),
            })
            .collect();
        let hunk = ProjectGitDiffHunk {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: n as u32,
            hunk_id: "h1".to_owned(),
            lines,
        };
        let file = ProjectGitDiffFile {
            relative_path: "big.rs".to_owned(),
            is_binary: false,
            unmerged: false,
            hunks: vec![hunk],
        };
        DiffViewState {
            root,
            scope: ProjectDiffScope::Unstaged,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![file],
        }
    }

    /// A line of Rust source rendered by the diff view's per-line render path
    /// must produce DOM children with inline `style="color:#…"` so users
    /// actually see syntax colors. Asserting at the rendered-output level
    /// (rather than on internal class names) keeps the test resilient to
    /// future refactors of `render_tokens`.
    #[wasm_bindgen_test]
    async fn rust_line_renders_colored_spans() {
        let syntax = syntax_for_path("foo.rs").expect("rust syntax bundled");
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            hunk_id: "h1".to_owned(),
            lines: vec![ProjectGitDiffLine {
                kind: ProjectGitDiffLineKind::Added,
                text: "fn main() {}".to_owned(),
                old_line_number: None,
                new_line_number: Some(1),
            }],
        };
        let tokens = compute_hunk_tokens(&hunk, syntax);
        let toks = tokens[0].clone().expect("rust line tokenizes");

        let container = make_container();
        let container_for_mount = container.clone();
        let _handle = mount_to(container_for_mount, move || render_tokens(&toks));
        next_tick().await;

        let nodes = container.query_selector_all("span[style]").unwrap();
        assert!(
            nodes.length() > 0,
            "expected at least one styled span in rendered output"
        );
        let mut found_color = false;
        for i in 0..nodes.length() {
            if let Some(node) = nodes.item(i) {
                let el: web_sys::Element = node.dyn_into().unwrap();
                let style = el.get_attribute("style").unwrap_or_default();
                if style.contains("color:") {
                    found_color = true;
                    break;
                }
            }
        }
        assert!(
            found_color,
            "expected at least one span to have a color: in its style attribute"
        );

        // Concatenated text content of the rendered output must match the
        // original line — rendering must not corrupt or duplicate source.
        let rendered_text = container.text_content().unwrap_or_default();
        assert_eq!(rendered_text, "fn main() {}");
    }

    // ── Code intelligence over the diff ────────────────────────────────────

    const CODE_INTEL_HOST: &str = "h";
    const CODE_INTEL_PROJECT: &str = "p";

    fn code_intel_root() -> ProjectRootPath {
        ProjectRootPath("test-root".to_owned())
    }

    fn code_intel_key(relative_path: &str) -> FileResourceKey {
        FileResourceKey {
            host_id: CODE_INTEL_HOST.to_owned(),
            project_id: ProjectId(CODE_INTEL_PROJECT.to_owned()),
            path: ProjectPath {
                root: code_intel_root(),
                relative_path: relative_path.to_owned(),
            },
        }
    }

    /// Install a JS stub capturing outbound `send_host_line` invokes so a test
    /// can inspect the frames actually put on the wire.
    fn install_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
            "#,
        )
        .expect("install send stub");
    }

    /// Every captured frame of `kind`, as JSON strings of its payload.
    fn captured_frames(kind: &str) -> Vec<String> {
        let script = format!(
            r#"
            (function() {{
                const out = [];
                for (const [cmd, args] of (window.__test_send_calls || [])) {{
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "{kind}") out.push(JSON.stringify(env.payload));
                }}
                return out.join("\n");
            }})()
            "#
        );
        let joined = js_sys::eval(&script)
            .expect("probe send calls")
            .as_string()
            .unwrap_or_default();
        if joined.is_empty() {
            Vec::new()
        } else {
            joined.lines().map(|line| line.to_owned()).collect()
        }
    }

    /// A one-file unstaged diff: one Context row then one Added row, plus a
    /// Removed row so the old side is represented too.
    fn code_intel_diff(relative_path: &str, added_line: &str) -> DiffViewState {
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
            hunk_id: "h1".to_owned(),
            lines: vec![
                ProjectGitDiffLine {
                    kind: ProjectGitDiffLineKind::Context,
                    text: "fn main() {".to_owned(),
                    old_line_number: Some(1),
                    new_line_number: Some(1),
                },
                ProjectGitDiffLine {
                    kind: ProjectGitDiffLineKind::Removed,
                    text: "    let removed = 1;".to_owned(),
                    old_line_number: Some(2),
                    new_line_number: None,
                },
                ProjectGitDiffLine {
                    kind: ProjectGitDiffLineKind::Added,
                    text: added_line.to_owned(),
                    old_line_number: None,
                    new_line_number: Some(2),
                },
            ],
        };
        DiffViewState {
            root: code_intel_root(),
            scope: ProjectDiffScope::Unstaged,
            path: Some(relative_path.to_owned()),
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: relative_path.to_owned(),
                is_binary: false,
                unmerged: false,
                hunks: vec![hunk],
            }],
        }
    }

    /// Mount a live unstaged diff tab with `on_disk` cached as the file's
    /// contents and the diff tab already holding its code intel.
    ///
    /// `on_disk` is passed separately from the diff on purpose: a test can make
    /// them disagree to exercise the staleness guard.
    /// What to seed alongside a mounted diff. Defaults are "nothing held, no
    /// pushed model, no modifier" — the cold state a diff tab opens in.
    #[derive(Clone, Copy, Default)]
    struct MountOpts {
        /// The diff tab already holds the file's contents + subscription, as it
        /// would after the pointer has settled over one of its rows.
        held: bool,
        /// Seed a resolved definition for the identifier at the start of the
        /// added row, so `navigable_range_at` answers.
        with_model: bool,
        /// The Cmd/Ctrl modifier is down.
        cmd_held: bool,
    }

    fn mount_code_intel_diff(
        container: HtmlElement,
        relative_path: &'static str,
        on_disk: &'static str,
        held: bool,
    ) -> MountedDiff {
        mount_code_intel_diff_with(
            container,
            relative_path,
            on_disk,
            "    let 名前 = 2;",
            MountOpts {
                held,
                ..Default::default()
            },
        )
    }

    /// A mounted diff plus the handles a test needs to drive it: the app state
    /// it was given, and the id of the real diff tab it renders into.
    struct MountedDiff {
        _handle: Box<dyn std::any::Any>,
        state: AppState,
        tab: TabId,
    }

    /// Mount a live unstaged diff of `relative_path` whose added row shows
    /// `added_line`, over a file whose on-disk contents are `on_disk`.
    ///
    /// `on_disk` is passed separately from the diff on purpose: a test can make
    /// them disagree to exercise the staleness guard.
    ///
    /// A real `TabContent::Diff` tab is opened in the center zone rather than a
    /// bare id being invented, because every code-intel request is gated on
    /// `file_occurrence_is_current`, which looks the tab up there. Inventing an
    /// id would make requests silently drop for a reason unrelated to what a
    /// test is asserting.
    fn mount_code_intel_diff_with(
        container: HtmlElement,
        relative_path: &'static str,
        on_disk: &'static str,
        added_line: &'static str,
        opts: MountOpts,
    ) -> MountedDiff {
        let shared: std::rc::Rc<RefCell<Option<(AppState, TabId)>>> =
            std::rc::Rc::new(RefCell::new(None));
        let shared_for_mount = shared.clone();
        let diff = code_intel_diff(relative_path, added_line);
        let handle = mount_to(container, move || {
            let state = AppState::new();
            let key = code_intel_key(relative_path);
            state.diff_contents.update(|diffs| {
                diffs.insert(
                    crate::state::DiffKey::new(
                        CODE_INTEL_HOST,
                        ProjectId(CODE_INTEL_PROJECT.to_owned()),
                        code_intel_root(),
                        ProjectDiffScope::Unstaged,
                        relative_path.to_owned(),
                    ),
                    diff.clone(),
                );
            });
            state.open_files.update(|files| {
                files.insert(
                    key.clone(),
                    crate::state::OpenFile {
                        path: key.path.clone(),
                        version: ProjectFileVersion(1),
                        contents: Some(on_disk.to_owned()),
                        is_binary: false,
                        missing: false,
                    },
                );
            });
            let tab = state
                .open_tab_in(
                    crate::state::PaneId::Primary,
                    crate::state::TabContent::Diff {
                        host_id: CODE_INTEL_HOST.to_owned(),
                        project_id: ProjectId(CODE_INTEL_PROJECT.to_owned()),
                        root: code_intel_root(),
                        scope: ProjectDiffScope::Unstaged,
                        path: relative_path.to_owned(),
                    },
                    "diff".to_owned(),
                    true,
                )
                .expect("diff tab opens");
            if opts.held {
                state.hold_diff_code_intel(tab, &key);
            }
            if opts.with_model {
                // The identifier occupying the first three bytes of file line 2
                // (which starts at byte 12), with a definition elsewhere.
                let model = protocol::CodeIntelFileModelPayload {
                    path: key.path.clone(),
                    version: ProjectFileVersion(1),
                    provider: protocol::CodeIntelProviderId("rust-analyzer".to_owned()),
                    language: protocol::CodeIntelLanguageId("rust".to_owned()),
                    model_range: protocol::CodeIntelModelRange::FullFile,
                    completeness: protocol::CodeIntelCompleteness::Complete,
                    occurrences: vec![protocol::CodeIntelOccurrence {
                        range: protocol::ByteRange { start: 12, end: 15 },
                        role: protocol::CodeIntelRole::Reference,
                        display: "let".to_owned(),
                        definition: vec![protocol::CodeIntelLocation {
                            path: key.path.clone(),
                            range: protocol::ByteRange { start: 0, end: 2 },
                        }],
                    }],
                };
                state.code_intel.update(|map| {
                    let file = map
                        .entry(crate::state::CodeIntelKey::from(&key))
                        .or_default();
                    file.set_rendered_version(ProjectFileVersion(1));
                    file.merge_versioned(ProjectFileVersion(1), |data| data.merge_model(model));
                });
            }
            state.cmd_held.set(opts.cmd_held);
            state
                .active_project
                .set(Some(crate::state::ActiveProjectRef {
                    host_id: CODE_INTEL_HOST.to_owned(),
                    project_id: ProjectId(CODE_INTEL_PROJECT.to_owned()),
                }));
            shared_for_mount.borrow_mut().replace((state.clone(), tab));
            provide_context(state);
            view! {
                <DiffView
                    tab_id=tab
                    host_id=CODE_INTEL_HOST.to_owned()
                    project_id=ProjectId(CODE_INTEL_PROJECT.to_owned())
                    root=code_intel_root()
                    scope=ProjectDiffScope::Unstaged
                    path=relative_path.to_owned()
                />
            }
        });
        let (state, tab) = shared.borrow().clone().expect("mount body ran");
        MountedDiff {
            _handle: Box::new(handle),
            state,
            tab,
        }
    }

    /// The `.diff-text` element of the row whose `data-anchor-side` is `side`
    /// and whose own line number is `line_number`.
    fn row_code_element(
        container: &HtmlElement,
        side: &str,
        line_number: u32,
    ) -> Option<web_sys::Element> {
        let selector = format!(
            ".diff-line[data-anchor-side=\"{side}\"][data-anchor-{side}-line=\"{line_number}\"] \
             .diff-text"
        );
        container.query_selector(&selector).unwrap()
    }

    /// A container for the pointer-driven code-intel tests.
    ///
    /// These tests resolve a position with `caretPositionFromPoint`, which
    /// answers by *viewport coordinate*, so unlike the render-only tests they
    /// are sensitive to where the container actually sits and to what else is
    /// painted over it. Two things had to be dealt with:
    ///
    /// - `make_container`'s divs are never removed, so they pile up on the page
    ///   and one of them wins hit-testing at our coordinates — the caret then
    ///   resolves inside *that* div, the handler correctly finds no diff row,
    ///   and every "did we send a request?" assertion fails as though the
    ///   feature were broken. They are cleared here (tests run sequentially, so
    ///   by now they are all finished).
    /// - Absolutely-positioned containers move with page scroll; a mounted row
    ///   was landing at `top: -93px`, off-screen, where the caret API returns
    ///   `None`. `position: fixed` pins this one to the viewport instead.
    fn make_code_intel_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        if let Some(previous) = document.get_element_by_id("code-intel-test-container") {
            previous.remove();
        }
        // Leftover mount containers from earlier tests. Every component's
        // `make_container` pins one at the top-left corner — some absolute,
        // some fixed — which is exactly where ours goes, and a `position:
        // fixed` leftover sits in the same stacking layer, so `z-index` alone
        // does not reliably keep ours on top. Matching on the shared
        // "top: 0; left: 0" inline style catches them whichever they use.
        let stale = document
            .query_selector_all("div[style*=\"top: 0\"][style*=\"left: 0\"]")
            .unwrap();
        for i in 0..stale.length() {
            if let Some(node) = stale.item(i)
                && let Ok(element) = node.dyn_into::<web_sys::Element>()
                && element.id() != "code-intel-test-container"
            {
                element.remove();
            }
        }
        let container = document.create_element("div").unwrap();
        container.set_id("code-intel-test-container");
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 800px; height: 600px; \
                 z-index: 99999; background: #fff; display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    /// Viewport coordinates just inside `element`'s left edge.
    ///
    /// Asserts the point is on-screen first. An off-screen point makes
    /// `caretPositionFromPoint` return `None`, which would turn every "did this
    /// send a request?" assertion into a false negative that passes for the
    /// wrong reason.
    fn point_in(element: &web_sys::Element) -> (f64, f64) {
        let rect = element.get_bounding_client_rect();
        assert!(
            rect.width() > 0.0 && rect.height() > 0.0,
            "row has no layout box ({}x{}); it cannot be pointed at",
            rect.width(),
            rect.height()
        );
        let x = rect.left() + 1.0;
        let y = rect.top() + rect.height() / 2.0;
        assert!(
            x >= 0.0 && y >= 0.0,
            "row is off-screen at ({x}, {y}); caretPositionFromPoint would answer None \
             and the test would pass for the wrong reason"
        );
        // The point must actually hit-test into this row. Containers from
        // earlier tests share the page, and if one covers these coordinates the
        // caret API resolves inside *it* instead — the handler then correctly
        // declines, and the assertion downstream fails as "no frames sent",
        // which reads like a product bug rather than a test-environment one.
        // Checking here turns that into a failure that names the real cause.
        let document = web_sys::window().unwrap().document().unwrap();
        let hit = document.element_from_point(x as f32, y as f32);
        let landed_in_row = hit.as_ref().is_some_and(|hit| {
            element.contains(Some(hit.unchecked_ref()))
                || hit.contains(Some(element.unchecked_ref()))
        });
        assert!(
            landed_in_row,
            "point ({x}, {y}) hit-tests to {} which is outside the intended row; \
             something is covering the diff",
            hit.map(|hit| {
                let rect = hit.get_bounding_client_rect();
                format!(
                    "<{} id={:?} class={:?} at ({:.0},{:.0}) {:.0}x{:.0}>",
                    hit.tag_name(),
                    hit.id(),
                    hit.class_name(),
                    rect.left(),
                    rect.top(),
                    rect.width(),
                    rect.height(),
                )
            })
            .unwrap_or_else(|| "nothing".into())
        );
        (x, y)
    }

    /// Dispatch a synthesized mouse event positioned over `element`.
    ///
    /// The event is dispatched on the **scroll container**, not on `element`
    /// itself. Syntax tokens arrive asynchronously from the highlight worker and
    /// re-render each row's `.diff-text`, so a row's code node can be replaced
    /// between being looked up and being clicked — and an event dispatched on a
    /// detached node never reaches the handler, which listens on the container.
    /// The handler resolves the position by viewport coordinate, so dispatching
    /// on the (stable) container is equivalent and not timing-dependent.
    fn dispatch_over(container: &HtmlElement, element: &web_sys::Element, kind: &str, cmd: bool) {
        let (x, y) = point_in(element);
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_client_x(x as i32);
        init.set_client_y(y as i32);
        if cmd {
            init.set_ctrl_key(true);
            init.set_meta_key(true);
        }
        let event = web_sys::MouseEvent::new_with_mouse_event_init_dict(kind, &init).unwrap();
        container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff scroll container present")
            .dispatch_event(&event)
            .unwrap();
    }

    fn cmd_click(container: &HtmlElement, element: &web_sys::Element) {
        dispatch_over(container, element, "click", true);
    }

    fn mouse_move(container: &HtmlElement, element: &web_sys::Element) {
        dispatch_over(container, element, "mousemove", false);
    }

    /// Cmd/Ctrl+click on an **added** row resolves the position to an absolute
    /// byte offset **into the file on disk** — not into the diff — and asks the
    /// server about it.
    ///
    /// The file's first line is 12 bytes (`fn main() {` + `\n`), so the added
    /// row's line 2 starts at byte 12; four spaces of indent put `let` at byte
    /// 16. Getting this wrong by even one line is the whole risk of hanging code
    /// intel off a diff, so the test pins the exact number.
    #[wasm_bindgen_test]
    async fn cmd_click_on_added_row_navigates_at_the_file_byte_offset() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, true);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "new", 2).expect("added row rendered");
        // 4 px in from the left edge of the code lands inside the leading
        // indent; +1 char widths are unreliable, so click the row start and
        // assert the offset is the line start.
        cmd_click(&container, &code);
        for _ in 0..10 {
            next_tick().await;
        }

        let frames = captured_frames("code_intel_navigate");
        assert_eq!(
            frames.len(),
            1,
            "expected exactly one code_intel_navigate, got {frames:?}"
        );
        let payload: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(
            payload["path"]["relative_path"], "main.rs",
            "navigate must address the file the row belongs to"
        );
        assert_eq!(
            payload["offset"], 12,
            "line 2 of the file starts at byte 12 (line 1 is `fn main() {{` + newline); \
             got {payload:?}"
        );
    }

    /// A **removed** row has no counterpart on disk, so there is no honest
    /// position to resolve. Cmd/Ctrl+click over it must stay inert rather than
    /// answer against the line number of some other row.
    #[wasm_bindgen_test]
    async fn cmd_click_on_removed_row_sends_nothing() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, true);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "old", 2).expect("removed row rendered");
        cmd_click(&container, &code);
        for _ in 0..10 {
            next_tick().await;
        }

        assert!(
            captured_frames("code_intel_navigate").is_empty(),
            "the old side of a diff does not exist on disk; it must not be queried"
        );
    }

    /// The diff payload is a snapshot. If the file was rewritten since it was
    /// fetched, the row's line number now addresses different text — answering
    /// would silently resolve a symbol other than the one under the cursor. The
    /// row-text equality check must decline instead.
    #[wasm_bindgen_test]
    async fn cmd_click_declines_when_the_file_no_longer_matches_the_row() {
        ensure_styles_loaded();
        install_send_stub();

        // Same line 1, but line 2 has been rewritten out from under the diff.
        let on_disk = "fn main() {\n    let something_else = 9;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, true);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "new", 2).expect("added row rendered");
        cmd_click(&container, &code);
        for _ in 0..10 {
            next_tick().await;
        }

        assert!(
            captured_frames("code_intel_navigate").is_empty(),
            "a row whose text no longer matches the file must not be resolved against it"
        );

        // The unchanged context row still matches the file, so it stays usable —
        // staleness is judged per row, not by disabling the whole file.
        let context_row = row_code_element(&container, "new", 1).expect("context row rendered");
        cmd_click(&container, &context_row);
        for _ in 0..10 {
            next_tick().await;
        }
        let frames = captured_frames("code_intel_navigate");
        assert_eq!(
            frames.len(),
            1,
            "the still-matching context row should resolve, got {frames:?}"
        );
        let payload: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(payload["offset"], 0, "line 1 of the file starts at byte 0");
    }

    /// Without the diff tab's hold, the file's contents in `open_files` belong
    /// to someone else and there may be no subscription — a request would be
    /// dropped on arrival. Declining keeps a pointless round trip off the wire.
    #[wasm_bindgen_test]
    async fn cmd_click_sends_nothing_before_the_file_is_held() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, false);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "new", 2).expect("added row rendered");
        cmd_click(&container, &code);
        for _ in 0..10 {
            next_tick().await;
        }

        assert!(captured_frames("code_intel_navigate").is_empty());
    }

    /// Hovering the new side pulls the file's contents + subscription exactly
    /// once, however many events the pointer generates. Lazy, per-file
    /// subscription is what keeps a whole-root diff from `didOpen`-ing every
    /// file it lists in the language server.
    #[wasm_bindgen_test]
    async fn hovering_subscribes_the_file_once() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        // Not pre-held: the hover itself is what must acquire the hold.
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, false);
        next_tick().await;
        next_tick().await;

        // Re-query each pass: the row's code node is replaced when syntax
        // tokens land, and a stale handle would be measuring a detached node.
        for _ in 0..3 {
            let code = row_code_element(&container, "new", 2).expect("added row rendered");
            mouse_move(&container, &code);
            next_tick().await;
        }
        for _ in 0..10 {
            next_tick().await;
        }

        let subscribes = captured_frames("code_intel_subscribe_file");
        assert_eq!(
            subscribes.len(),
            1,
            "three mousemoves over one file must produce one subscription, got {subscribes:?}"
        );
        let payload: serde_json::Value = serde_json::from_str(&subscribes[0]).unwrap();
        assert_eq!(payload["path"]["relative_path"], "main.rs");
        assert_eq!(
            captured_frames("project_read_file").len(),
            1,
            "the contents read is paired with the subscription"
        );
    }

    /// Hovering the **old** side subscribes nothing: there is no on-disk file
    /// position behind a removed line to ask about.
    #[wasm_bindgen_test]
    async fn hovering_the_old_side_subscribes_nothing() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, false);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "old", 2).expect("removed row rendered");
        mouse_move(&container, &code);
        for _ in 0..10 {
            next_tick().await;
        }

        assert!(captured_frames("code_intel_subscribe_file").is_empty());
    }

    // ── Code-intel affordances: link cue, explanations, menu ───────────────

    /// A file whose second line starts with an identifier at byte 12, matching
    /// the occurrence `MountOpts::with_model` seeds.
    const LINK_ON_DISK: &str = "fn main() {\nlet total = 2;\n}\n";
    const LINK_ADDED_LINE: &str = "let total = 2;";

    fn query_test(container: &HtmlElement, id: &str) -> Option<web_sys::Element> {
        container
            .query_selector(&format!("[data-test=\"{id}\"]"))
            .unwrap()
    }

    /// Menus render at the document root (they are fixed-position overlays), so
    /// they are not inside the mount container.
    fn query_document(selector: &str) -> Option<web_sys::Element> {
        web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .query_selector(selector)
            .unwrap()
    }

    fn context_click(container: &HtmlElement, element: &web_sys::Element) {
        let (x, y) = point_in(element);
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        init.set_client_x(x as i32);
        init.set_client_y(y as i32);
        let event =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &init).unwrap();
        container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff scroll container present")
            .dispatch_event(&event)
            .unwrap();
    }

    /// Holding Cmd/Ctrl over a navigable identifier marks it as clickable, and
    /// the mark covers *the identifier*, not the whole line — otherwise it would
    /// say nothing about where to click.
    #[wasm_bindgen_test]
    async fn cmd_held_marks_the_navigable_symbol_as_clickable() {
        ensure_styles_loaded();
        install_send_stub();

        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff_with(
            container.clone(),
            "main.rs",
            LINK_ON_DISK,
            LINK_ADDED_LINE,
            MountOpts {
                held: true,
                with_model: true,
                cmd_held: true,
            },
        );
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "new", 2).expect("added row rendered");
        mouse_move(&container, &code);
        for _ in 0..4 {
            next_tick().await;
        }

        let link = query_test(&container, "diff-cmd-link")
            .expect("holding the modifier over a navigable identifier should mark it clickable");
        let link_rect = link.get_bounding_client_rect();
        let code_rect = code.get_bounding_client_rect();
        assert!(
            link_rect.width() > 0.0,
            "the mark must have width to be visible"
        );
        assert!(
            link_rect.width() < code_rect.width(),
            "the mark covers the identifier ({}px), so it must be narrower than the whole \
             rendered line ({}px)",
            link_rect.width(),
            code_rect.width()
        );
        assert!(
            link_rect.left() >= code_rect.left() - 1.0,
            "the mark must sit within the rendered line, not left of it"
        );
    }

    /// The mark is an affordance for a held modifier: releasing it takes the
    /// mark away, so it never claims a plain click will navigate.
    #[wasm_bindgen_test]
    async fn releasing_the_modifier_removes_the_clickable_mark() {
        ensure_styles_loaded();
        install_send_stub();

        let container = make_code_intel_container();
        let mounted = mount_code_intel_diff_with(
            container.clone(),
            "main.rs",
            LINK_ON_DISK,
            LINK_ADDED_LINE,
            MountOpts {
                held: true,
                with_model: true,
                cmd_held: true,
            },
        );
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "new", 2).expect("added row rendered");
        mouse_move(&container, &code);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(query_test(&container, "diff-cmd-link").is_some());

        // Release the modifier the way the app-level key handler does.
        mounted.state.cmd_held.set(false);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            query_test(&container, "diff-cmd-link").is_none(),
            "the clickable mark must not outlive the modifier"
        );
    }

    /// Cmd/Ctrl+clicking a removed line cannot resolve anything, and saying so
    /// is the difference between "not supported here" and "this feature is
    /// broken".
    #[wasm_bindgen_test]
    async fn cmd_click_on_a_removed_row_explains_itself() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, true);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "old", 2).expect("removed row rendered");
        cmd_click(&container, &code);
        for _ in 0..4 {
            next_tick().await;
        }

        let notice = query_test(&container, "diff-code-intel-notice")
            .expect("an unanswerable explicit action must explain itself");
        let text = notice.text_content().unwrap_or_default();
        assert!(
            text.contains("Removed lines"),
            "notice should name the reason, got {text:?}"
        );
        assert!(
            captured_frames("code_intel_navigate").is_empty(),
            "explaining must not also send a request"
        );
    }

    /// Right-clicking a resolvable row offers Show References, and choosing it
    /// asks the server — find-references works from a diff, not just a file.
    #[wasm_bindgen_test]
    async fn show_references_from_a_diff_row_queries_the_server() {
        ensure_styles_loaded();
        install_send_stub();

        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff_with(
            container.clone(),
            "main.rs",
            LINK_ON_DISK,
            LINK_ADDED_LINE,
            MountOpts {
                held: true,
                with_model: true,
                cmd_held: false,
            },
        );
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "new", 2).expect("added row rendered");
        context_click(&container, &code);
        for _ in 0..4 {
            next_tick().await;
        }

        let menu = query_document(".context-menu").expect("right-click opens the code-intel menu");
        let buttons = menu.query_selector_all("button").unwrap();
        assert_eq!(buttons.length(), 2, "Go to Definition and Show References");
        let references: web_sys::HtmlElement = buttons
            .item(1)
            .unwrap()
            .dyn_into()
            .expect("second action is an element");
        assert!(
            references
                .text_content()
                .unwrap_or_default()
                .contains("References"),
            "second action should be Show References"
        );
        references.click();
        for _ in 0..10 {
            next_tick().await;
        }

        let frames = captured_frames("code_intel_find_references");
        assert_eq!(
            frames.len(),
            1,
            "Show References should query the server once, got {frames:?}"
        );
        let payload: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(payload["path"]["relative_path"], "main.rs");
        assert_eq!(
            payload["offset"], 12,
            "the query addresses the identifier at the start of file line 2"
        );
    }

    /// Right-clicking a row that cannot be resolved still opens the menu, with
    /// the actions disabled and the reason shown — the same contract the file
    /// viewer has, so a diff never presents an action that would do nothing.
    #[wasm_bindgen_test]
    async fn right_click_on_a_removed_row_disables_the_actions_and_says_why() {
        ensure_styles_loaded();
        install_send_stub();

        let on_disk = "fn main() {\n    let 名前 = 2;\n}\n";
        let container = make_code_intel_container();
        let _mounted = mount_code_intel_diff(container.clone(), "main.rs", on_disk, true);
        next_tick().await;
        next_tick().await;

        let code = row_code_element(&container, "old", 2).expect("removed row rendered");
        context_click(&container, &code);
        for _ in 0..4 {
            next_tick().await;
        }

        let menu = query_document(".context-menu").expect("right-click opens the menu");
        let buttons = menu.query_selector_all("button").unwrap();
        assert_eq!(buttons.length(), 2);
        for i in 0..buttons.length() {
            let button: web_sys::HtmlButtonElement =
                buttons.item(i).unwrap().dyn_into().expect("action button");
            assert!(
                button.disabled(),
                "a removed line has no position, so its actions must be disabled"
            );
        }
        let hint = menu
            .query_selector(".context-menu-hint")
            .unwrap()
            .expect("a disabled menu must carry its reason");
        let text = hint.text_content().unwrap_or_default();
        assert!(
            text.contains("Removed lines"),
            "hint should name the reason, got {text:?}"
        );
    }

    /// A diff with 1000 Added lines must NOT put all 1000 rows in the DOM.
    /// Virtualization (commits c55a2c4 / 93e0343) clips rendering to the
    /// visible window plus an overscan buffer; spacers above and below
    /// preserve the scroll geometry so the scrollbar is the right size.
    ///
    /// If this regresses, the diff viewer's gutter paint flash on scroll
    /// returns and the FullFile mode becomes unusable on big files.
    /// Asserts on user-visible behaviour: row counts and scroll height,
    /// not on internal class names beyond the ones needed to find rows.
    #[wasm_bindgen_test]
    async fn large_diff_only_renders_visible_window() {
        ensure_styles_loaded();
        let root = ProjectRootPath("test-root".to_owned());
        let scope = ProjectDiffScope::Unstaged;
        let total_lines = 1000usize;
        let diff = synth_added_diff(total_lines, root.clone());

        let container = make_container();
        let mount_root = root.clone();
        let mount_path = "big.rs".to_owned();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h",
                        protocol::ProjectId("p".to_owned()),
                        mount_root.clone(),
                        scope,
                        mount_path.clone(),
                    ),
                    diff.clone(),
                );
            });
            provide_context(state);
            view! {
                <DiffView
                    host_id="h".to_owned()
                    project_id=protocol::ProjectId("p".to_owned())
                    root=mount_root.clone()
                    scope=scope
                    path=mount_path.clone()
                />
            }
        });
        // First tick mounts; a second lets the measurement Effect refine
        // viewport_height/line_height from real DOM measurements.
        next_tick().await;
        next_tick().await;

        let rendered = container.query_selector_all(".diff-line").unwrap().length() as usize;
        assert!(
            rendered > 0,
            "expected some diff lines rendered, got {rendered}"
        );
        assert!(
            rendered < total_lines / 2,
            "virtualization not engaging: rendered {rendered} of {total_lines} lines \
             (expected fewer than {})",
            total_lines / 2
        );

        // Scroll height should reflect the full file (visible rows + spacers
        // standing in for off-screen rows). If virtualization regressed to
        // rendering everything, scroll_height would still match — but we'd
        // also see all 1000 rows in DOM. The row-count guard above catches
        // that case; this guard catches the inverse regression where
        // someone "fixes" the row count by silently dropping spacers.
        let scroll_el = container
            .query_selector(".diff-content")
            .unwrap()
            .expect("diff-content scroll container present")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let scroll_height = scroll_el.scroll_height();
        assert!(
            scroll_height > 5000,
            "expected scroll_height > 5000px for 1000-line file, got {scroll_height}; \
             spacers may have been dropped"
        );
    }

    /// CRITICAL: Virtualization must stay engaged when the host passes
    /// review-mode decoration callbacks. Regression we're guarding against:
    /// a previous refactor disabled virt whenever any decoration was set,
    /// blowing up the DOM for 1500-line files in review mode and making
    /// them feel sluggish to open. The decoration callback is responsible
    /// for returning `None` when there's nothing to show — that keeps lines
    /// uniform-height and the row-count math correct.
    #[wasm_bindgen_test]
    async fn large_diff_with_decorations_still_virtualizes() {
        ensure_styles_loaded();
        let root = ProjectRootPath("test-root".to_owned());
        let scope = ProjectDiffScope::Unstaged;
        let total_lines = 1500usize;
        let diff = synth_added_diff(total_lines, root.clone());

        // A line-decoration callback that returns None for every row —
        // mimics the review-mode case where most rows have no thread.
        let line_decoration: super::DecorationLineFn = std::sync::Arc::new(|_, _, _, _| None);
        // Pointer-down callback stand-in: the real review surface uses
        // this to start a drag selection. For virtualization purposes
        // any non-None callback exercises the same code path.
        let pointer_down: super::GutterPointerDownFn = std::sync::Arc::new(|_, _, _, _| {});

        let container = make_container();
        let mount_root = root.clone();
        let mount_path = "big.rs".to_owned();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h",
                        protocol::ProjectId("p".to_owned()),
                        mount_root.clone(),
                        scope,
                        mount_path.clone(),
                    ),
                    diff.clone(),
                );
            });
            provide_context(state);
            view! {
                <DiffView
                    host_id="h".to_owned()
                    project_id=protocol::ProjectId("p".to_owned())
                    root=mount_root.clone()
                    scope=scope
                    path=mount_path.clone()
                    on_gutter_pointer_down=pointer_down.clone()
                    decoration_below_line=line_decoration.clone()
                />
            }
        });
        next_tick().await;
        next_tick().await;

        let rendered = container.query_selector_all(".diff-line").unwrap().length() as usize;
        assert!(
            rendered > 0 && rendered < 250,
            "decorations must not disable virtualization: rendered {rendered} of \
             {total_lines} lines (expected < 250)"
        );

        // None-returning decoration callbacks must not leave empty
        // `.review-thread-region` boxes in the DOM (looks like a
        // dropzone, confuses users).
        let empty_threads = container
            .query_selector_all(".review-thread-region:empty")
            .unwrap()
            .length();
        assert_eq!(
            empty_threads, 0,
            "empty thread-region elements should not be emitted"
        );

        // Each rendered diff line must expose its anchor side+line as
        // data attributes — the drag-selection hit-test depends on this.
        let line: HtmlElement = container
            .query_selector(".diff-line")
            .unwrap()
            .expect("at least one diff-line rendered")
            .dyn_into()
            .unwrap();
        assert!(
            line.get_attribute("data-anchor-side").is_some(),
            "diff-line must expose data-anchor-side for drag hit-testing"
        );
        assert!(
            line.get_attribute("data-anchor-line").is_some(),
            "diff-line must expose data-anchor-line for drag hit-testing"
        );

        // The canonical gutter must carry the clickable class so its
        // cursor reads as :pointer for the click+drag affordance.
        let canonical_gutter = line
            .query_selector(".diff-gutter-clickable")
            .unwrap()
            .expect("canonical gutter should be clickable when callback set");
        let _ = canonical_gutter;
    }

    /// In SBS mode, consecutive `.diff-hunk` siblings inside a `.diff-pane`
    /// (a `display: grid` container) must pack tightly with no extra space
    /// between them. Regression: without `align-content: start` Chrome
    /// stretches auto-sized grid rows to fill the pane's flex height,
    /// inflating each `.diff-hunk` track and leaving a visible empty
    /// region inside each hunk that reads as a gap before the next hunk
    /// header.
    #[wasm_bindgen_test]
    async fn sbs_hunks_pack_with_no_inter_hunk_gap() {
        ensure_styles_loaded();
        let root = ProjectRootPath("test-root".to_owned());
        let scope = ProjectDiffScope::Unstaged;
        let path = "multi.rs".to_owned();

        let mk_hunk = |old_start: u32, new_start: u32, hunk_id: &str| ProjectGitDiffHunk {
            old_start,
            old_count: 2,
            new_start,
            new_count: 2,
            hunk_id: hunk_id.to_owned(),
            lines: vec![
                ProjectGitDiffLine {
                    kind: ProjectGitDiffLineKind::Context,
                    text: format!("ctx line {old_start}"),
                    old_line_number: Some(old_start),
                    new_line_number: Some(new_start),
                },
                ProjectGitDiffLine {
                    kind: ProjectGitDiffLineKind::Context,
                    text: format!("ctx line {}", old_start + 1),
                    old_line_number: Some(old_start + 1),
                    new_line_number: Some(new_start + 1),
                },
            ],
        };
        let file = ProjectGitDiffFile {
            relative_path: path.clone(),
            is_binary: false,
            unmerged: false,
            hunks: vec![mk_hunk(431, 431, "h1"), mk_hunk(459, 458, "h2")],
        };
        let diff = DiffViewState {
            root: root.clone(),
            scope,
            path: Some(path.clone()),
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![file],
        };

        let container = make_container();
        let mount_root = root.clone();
        let mount_path = path.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.diff_view_mode.set(DiffViewMode::SideBySide);
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h",
                        protocol::ProjectId("p".to_owned()),
                        mount_root.clone(),
                        scope,
                        mount_path.clone(),
                    ),
                    diff.clone(),
                );
            });
            provide_context(state);
            view! {
                <DiffView
                    host_id="h".to_owned()
                    project_id=protocol::ProjectId("p".to_owned())
                    root=mount_root.clone()
                    scope=scope
                    path=mount_path.clone()
                />
            }
        });
        next_tick().await;
        next_tick().await;

        let left_pane = container
            .query_selector(".diff-pane-left")
            .unwrap()
            .expect("left SBS pane present")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let hunks = left_pane.query_selector_all(".diff-hunk").unwrap();
        assert_eq!(hunks.length(), 2, "expected two hunks in left pane");

        let h0 = hunks
            .item(0)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .get_bounding_client_rect();
        let h1 = hunks
            .item(1)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .get_bounding_client_rect();
        let gap = h1.top() - h0.bottom();
        assert!(
            gap.abs() < 1.0,
            "expected hunks to pack tightly, got {gap}px between them \
             (hunk0 bottom={}, hunk1 top={})",
            h0.bottom(),
            h1.top(),
        );

        // Each hunk should hug its content (header ~22px + 2 ctx lines × 20px
        // = ~62px). Without `align-content: start` Chrome inflates each hunk
        // to roughly half the pane height, which would be > 200px each.
        assert!(
            h0.height() < 120.0,
            "first hunk inflated to {}px (content should be ~62px); \
             grid rows are stretching when they shouldn't",
            h0.height(),
        );
    }

    // ── Review-integrated normal diff tab + binary placeholder ──────────

    fn review_root() -> ProjectRootPath {
        ProjectRootPath("/repo".to_owned())
    }

    fn small_foo_diff() -> DiffViewState {
        let hunk = ProjectGitDiffHunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 3,
            hunk_id: "src/foo.rs:1".to_owned(),
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
            ],
        };
        DiffViewState {
            root: review_root(),
            // The canonical review surface is the whole-root unstaged diff;
            // review affordances only bind on this scope.
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/foo.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: "src/foo.rs".to_owned(),
                is_binary: false,
                unmerged: false,
                hunks: vec![hunk],
            }],
        }
    }

    fn binary_diff(path: &str) -> DiffViewState {
        DiffViewState {
            root: review_root(),
            scope: ProjectDiffScope::Unstaged,
            path: Some(path.to_owned()),
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: path.to_owned(),
                is_binary: true,
                unmerged: false,
                hunks: vec![],
            }],
        }
    }

    fn unmerged_diff(path: &str) -> DiffViewState {
        DiffViewState {
            root: review_root(),
            scope: ProjectDiffScope::Unstaged,
            path: Some(path.to_owned()),
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![ProjectGitDiffFile {
                relative_path: path.to_owned(),
                is_binary: false,
                unmerged: true,
                hunks: vec![],
            }],
        }
    }

    fn one_added_line_file(path: &str, text: &str) -> ProjectGitDiffFile {
        ProjectGitDiffFile {
            relative_path: path.to_owned(),
            is_binary: false,
            unmerged: false,
            hunks: vec![ProjectGitDiffHunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                hunk_id: format!("{path}:1"),
                lines: vec![
                    ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Context,
                        text: "fn x()".to_owned(),
                        old_line_number: Some(1),
                        new_line_number: Some(1),
                    },
                    ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Added,
                        text: text.to_owned(),
                        old_line_number: None,
                        new_line_number: Some(2),
                    },
                ],
            }],
        }
    }

    /// The whole-root review surface: empty `path` (all unstaged files in
    /// the root) with two changed files.
    fn all_files_diff() -> DiffViewState {
        DiffViewState {
            root: review_root(),
            scope: ProjectDiffScope::Unstaged,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![
                one_added_line_file("src/foo.rs", "    let a = 1;"),
                one_added_line_file("src/bar.rs", "    let b = 2;"),
            ],
        }
    }

    fn draft_review(path: &str, line: u32, body: &str) -> protocol::Review {
        use protocol::*;
        Review {
            id: ReviewId("rev-1".to_owned()),
            project_id: ProjectId("proj-1".to_owned()),
            origin_agent_id: AgentId("project-review:rev-1".to_owned()),
            origin_session_id: SessionId("s".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![],
            file_snapshots: Vec::new(),
            comments: vec![ReviewComment {
                id: ReviewCommentId("c1".to_owned()),
                location: ReviewLocation {
                    root: review_root(),
                    relative_path: path.to_owned(),
                    target: protocol::ReviewTarget::UnstagedDiff,
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

    fn draft_review_for(
        id: &str,
        project: &str,
        path: &str,
        line: u32,
        body: &str,
    ) -> protocol::Review {
        use protocol::*;
        let mut r = draft_review(path, line, body);
        r.id = ReviewId(id.to_owned());
        r.project_id = ProjectId(project.to_owned());
        if let Some(c) = r.comments.first_mut() {
            c.id = ReviewCommentId(format!("c-{id}"));
        }
        r
    }

    fn summary_for(review: &protocol::Review) -> protocol::ReviewSummary {
        protocol::ReviewSummary {
            id: review.id.clone(),
            scope: protocol::ReviewSummaryScope::Workspace,
            status: review.status.clone(),
            origin_session_id: review.origin_session_id.clone(),
            origin_agent_id: review.origin_agent_id.clone(),
            created_at_ms: 0,
            updated_at_ms: 1,
            user_comment_count: review.comments.len() as u32,
            pending_suggestion_count: 0,
            file_comment_counts: vec![],
        }
    }

    fn project_info(host: &str, project: &str, root: &str) -> crate::state::ProjectInfo {
        crate::state::ProjectInfo {
            host_id: host.to_owned(),
            project: protocol::Project {
                id: protocol::ProjectId(project.to_owned()),
                name: project.to_owned(),
                source: protocol::ProjectSource::Standalone {
                    roots: vec![protocol::ProjectRootPath(root.to_owned())],
                },
                sort_order: 0,
            },
        }
    }

    fn mount_reviewable(
        container: HtmlElement,
        diff: DiffViewState,
        review: Option<protocol::Review>,
    ) -> Mounted<()> {
        mount_reviewable_with_mode(container, diff, review, DiffViewMode::Unified)
    }

    fn mount_reviewable_with_mode(
        container: HtmlElement,
        diff: DiffViewState,
        review: Option<protocol::Review>,
        view_mode: DiffViewMode,
    ) -> Mounted<()> {
        let scope = diff.scope;
        let path = diff.path.clone().unwrap_or_default();
        let root = diff.root.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.diff_view_mode.set(view_mode);
            state
                .active_project
                .set(Some(crate::state::ActiveProjectRef {
                    host_id: "h1".to_owned(),
                    project_id: protocol::ProjectId("proj-1".to_owned()),
                }));
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h1",
                        protocol::ProjectId("proj-1".to_owned()),
                        root.clone(),
                        scope,
                        path.clone(),
                    ),
                    diff.clone(),
                );
            });
            if let Some(review) = review.clone() {
                state.review_summaries.update(|m| {
                    m.insert(
                        protocol::ProjectId("proj-1".to_owned()),
                        vec![protocol::ReviewSummary {
                            id: review.id.clone(),
                            scope: protocol::ReviewSummaryScope::Workspace,
                            status: review.status.clone(),
                            origin_session_id: review.origin_session_id.clone(),
                            origin_agent_id: review.origin_agent_id.clone(),
                            created_at_ms: 0,
                            updated_at_ms: 0,
                            user_comment_count: review.comments.len() as u32,
                            pending_suggestion_count: 0,
                            file_comment_counts: vec![],
                        }],
                    );
                });
                state.reviews.update(|m| {
                    m.insert(review.id.clone(), review.clone());
                });
            }
            provide_context(state);
            view! {
                <ReviewableDiffView
                    tab_id=crate::state::TabId(1)
                    host_id="h1".to_owned()
                    project_id=protocol::ProjectId("proj-1".to_owned())
                    root=root.clone()
                    scope=scope
                    path=path.clone()
                />
            }
        });
        Mounted::new(handle, ())
    }

    /// A normal diff tab whose project has a Draft review renders that
    /// review's comments inline — without ever mounting the standalone
    /// `ReviewView` workbench. This is the core of the integration: review
    /// happens on the ordinary diff surface.
    #[wasm_bindgen_test]
    async fn reviewable_diff_renders_draft_comment_without_review_view() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted = mount_reviewable(
            container.clone(),
            small_foo_diff(),
            Some(draft_review("src/foo.rs", 2, "please fix this")),
        );
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("please fix this"),
            "expected the draft review comment to render on the normal diff tab; got: {text}"
        );
        // No standalone review workbench is mounted on this surface.
        assert!(
            container.query_selector(".review-view").unwrap().is_none(),
            "ReviewView workbench must not be mounted by the normal diff tab"
        );
        // The create-review banner is hidden once a draft exists.
        assert!(
            container
                .query_selector("[data-test=\"reviewable-diff-banner\"]")
                .unwrap()
                .is_none(),
            "the start-a-review banner must be hidden when a draft already exists"
        );
        // The line-level comment affordance exists (gutter is clickable).
        assert!(
            container
                .query_selector(".diff-gutter-clickable")
                .unwrap()
                .is_some(),
            "expected clickable comment gutters on a reviewable diff"
        );
    }

    #[wasm_bindgen_test]
    async fn reviewable_diff_clicking_added_line_opens_composer() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted = mount_reviewable(
            container.clone(),
            small_foo_diff(),
            Some(draft_review("src/foo.rs", 2, "please fix this")),
        );
        next_tick().await;
        next_tick().await;

        let gutter = container
            .query_selector(".diff-gutter-new.diff-gutter-clickable[data-line-num=\"2\"]")
            .unwrap()
            .expect("new-side added-line gutter");
        gutter
            .dispatch_event(&web_sys::PointerEvent::new("pointerdown").unwrap())
            .unwrap();
        web_sys::window()
            .unwrap()
            .dispatch_event(&web_sys::PointerEvent::new("pointerup").unwrap())
            .unwrap();
        next_tick().await;

        assert!(
            container
                .query_selector(".review-composer")
                .unwrap()
                .is_some(),
            "clicking an added-line gutter should open an inline composer"
        );
    }

    #[wasm_bindgen_test]
    async fn reviewable_sbs_pure_add_clicking_added_line_opens_composer() {
        ensure_styles_loaded();
        let container = make_container();
        let mut diff = small_foo_diff();
        diff.files[0].hunks[0].lines = diff.files[0].hunks[0].lines[1..].to_vec();
        diff.files[0].hunks[0].old_count = 0;
        let _mounted = mount_reviewable_with_mode(
            container.clone(),
            diff,
            Some(draft_review("src/foo.rs", 2, "please fix this")),
            DiffViewMode::SideBySide,
        );
        next_tick().await;
        next_tick().await;

        let gutter = container
            .query_selector(
                ".diff-pane-right .diff-gutter.diff-gutter-clickable[data-line-num=\"2\"]",
            )
            .unwrap()
            .expect("right-pane added-line gutter");
        gutter
            .dispatch_event(&web_sys::PointerEvent::new("pointerdown").unwrap())
            .unwrap();
        web_sys::window()
            .unwrap()
            .dispatch_event(&web_sys::PointerEvent::new("pointerup").unwrap())
            .unwrap();
        next_tick().await;

        assert!(
            container
                .query_selector(".review-composer")
                .unwrap()
                .is_some(),
            "clicking a pure-add side-by-side gutter should open an inline composer"
        );
    }

    /// With no Draft review the diff renders as a plain diff — no start-review
    /// banner (reviews are always-on server-side), no comment gutters.
    #[wasm_bindgen_test]
    async fn reviewable_diff_no_banner_without_draft() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted = mount_reviewable(container.clone(), small_foo_diff(), None);
        next_tick().await;
        next_tick().await;

        // No start-a-review banner in the always-on model.
        assert!(
            container
                .query_selector("[data-test=\"reviewable-diff-banner\"]")
                .unwrap()
                .is_none(),
            "no start-a-review banner must show (reviews are always-on)"
        );
        // No comment composer gutters when there's no review to comment on.
        assert!(
            container
                .query_selector(".diff-gutter-clickable")
                .unwrap()
                .is_none(),
            "comment gutters must not appear without a draft review"
        );
    }

    /// An `Unstaged` diff tab now IS a valid review surface (reviews track
    /// unstaged diffs). Verify comments and gutters appear.
    #[wasm_bindgen_test]
    async fn unstaged_scope_shows_review_overlay() {
        ensure_styles_loaded();
        let container = make_container();
        let mut diff = small_foo_diff();
        diff.scope = ProjectDiffScope::Unstaged;
        let _mounted = mount_reviewable(
            container.clone(),
            diff,
            Some(draft_review("src/foo.rs", 2, "please fix this")),
        );
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("let x = 1;"),
            "the diff itself should still render; got: {text}"
        );
        // Unstaged scope gets review overlay (reviews track unstaged).
        assert!(
            container
                .query_selector(".diff-gutter-clickable")
                .unwrap()
                .is_some(),
            "Unstaged scope must get comment gutters (reviews track unstaged diffs)"
        );
        assert!(
            text.contains("please fix this"),
            "review comments must overlay Unstaged diff; got: {text}"
        );
        // Never a start-a-review banner in the always-on model.
        assert!(
            container
                .query_selector("[data-test=\"reviewable-diff-banner\"]")
                .unwrap()
                .is_none(),
            "no start-a-review banner (always-on model)"
        );
    }

    /// Staged tabs are first-class review surfaces, but only staged-targeted
    /// comments render on them.
    #[wasm_bindgen_test]
    async fn staged_scope_shows_only_staged_review_overlay() {
        ensure_styles_loaded();
        let container = make_container();
        let mut diff = small_foo_diff();
        diff.scope = ProjectDiffScope::Staged;
        let mut review = draft_review("src/foo.rs", 2, "please fix staged");
        review.comments[0].location.target = protocol::ReviewTarget::StagedDiff;
        let mut unstaged_comment = review.comments[0].clone();
        unstaged_comment.id = protocol::ReviewCommentId("unstaged-comment".to_owned());
        unstaged_comment.body = "unstaged must stay hidden".to_owned();
        unstaged_comment.location.target = protocol::ReviewTarget::UnstagedDiff;
        review.comments.push(unstaged_comment);
        let _mounted = mount_reviewable(container.clone(), diff, Some(review));
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("let x = 1;"),
            "the diff itself should still render; got: {text}"
        );
        // Staged scope gets its own overlay and comment gutters.
        assert!(
            container
                .query_selector("[data-test=\"reviewable-diff-banner\"]")
                .unwrap()
                .is_none(),
            "no banner on a staged diff tab"
        );
        assert!(
            container
                .query_selector(".diff-gutter-clickable")
                .unwrap()
                .is_some(),
            "staged diff tab must expose comment gutters"
        );
        assert!(
            text.contains("please fix staged"),
            "staged comments must overlay a staged diff; got: {text}"
        );
        assert!(
            !text.contains("unstaged must stay hidden"),
            "unstaged comments must not leak onto staged diffs; got: {text}"
        );
    }

    /// An `Uncommitted` (HEAD↔worktree) diff tab gets NO review overlay even
    /// when a draft exists. Active reviews are anchored to `Unstaged`
    /// (index↔worktree); once staged changes exist the two scopes' line
    /// numbers diverge, so overlaying a draft's comments here would
    /// mis-anchor them. Regression for the old model that decorated
    /// `Uncommitted` surfaces.
    #[wasm_bindgen_test]
    async fn uncommitted_scope_has_no_review_overlay() {
        ensure_styles_loaded();
        let container = make_container();
        let mut diff = small_foo_diff();
        diff.scope = ProjectDiffScope::Uncommitted;
        let _mounted = mount_reviewable(
            container.clone(),
            diff,
            Some(draft_review("src/foo.rs", 2, "please fix this")),
        );
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("let x = 1;"),
            "the diff itself should still render; got: {text}"
        );
        // Uncommitted scope: no overlay, no banner, no comment gutters.
        assert!(
            container
                .query_selector("[data-test=\"reviewable-diff-banner\"]")
                .unwrap()
                .is_none(),
            "no banner on an uncommitted diff tab"
        );
        assert!(
            container
                .query_selector(".diff-gutter-clickable")
                .unwrap()
                .is_none(),
            "no comment gutters on an uncommitted diff tab"
        );
        assert!(
            !text.contains("please fix this"),
            "review comments must not overlay an uncommitted diff; got: {text}"
        );
    }

    /// The review overlay binds to the project that owns the tab's root, not
    /// the globally active project. With proj-2 active but the tab rooted in
    /// proj-1's `/repo`, proj-1's review must decorate the diff — proj-2's
    /// review must never leak onto it (or comments would route to the wrong
    /// review).
    #[wasm_bindgen_test]
    async fn overlay_binds_to_root_project_not_active_project() {
        ensure_styles_loaded();
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Active project is the OTHER project on purpose.
            state
                .active_project
                .set(Some(crate::state::ActiveProjectRef {
                    host_id: "h1".to_owned(),
                    project_id: protocol::ProjectId("proj-2".to_owned()),
                }));
            state.projects.update(|p| {
                p.push(project_info("h1", "proj-1", "/repo"));
                p.push(project_info("h1", "proj-2", "/other"));
            });
            let diff = small_foo_diff();
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h1",
                        protocol::ProjectId("proj-1".to_owned()),
                        diff.root.clone(),
                        diff.scope,
                        "src/foo.rs",
                    ),
                    diff,
                );
            });
            let r1 = draft_review_for("rev-1", "proj-1", "src/foo.rs", 2, "from project one");
            let r2 = draft_review_for("rev-2", "proj-2", "src/foo.rs", 2, "from project two");
            state.review_summaries.update(|m| {
                m.insert(
                    protocol::ProjectId("proj-1".to_owned()),
                    vec![summary_for(&r1)],
                );
                m.insert(
                    protocol::ProjectId("proj-2".to_owned()),
                    vec![summary_for(&r2)],
                );
            });
            state.reviews.update(|m| {
                m.insert(r1.id.clone(), r1.clone());
                m.insert(r2.id.clone(), r2.clone());
            });
            provide_context(state);
            // Tab is explicitly for proj-1, even though proj-2 is active.
            view! {
                <ReviewableDiffView
                    tab_id=crate::state::TabId(1)
                    host_id="h1".to_owned()
                    project_id=protocol::ProjectId("proj-1".to_owned())
                    root=review_root()
                    scope=ProjectDiffScope::Unstaged
                    path="src/foo.rs".to_owned()
                />
            }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("from project one"),
            "overlay must show the tab's project review; got: {text}"
        );
        assert!(
            !text.contains("from project two"),
            "overlay must NOT show the active (different) project's review; got: {text}"
        );
    }

    /// Two projects that share the same root path string must each bind to
    /// their OWN review — the diff tab carries explicit project identity, so
    /// the overlay can't be confused by a duplicate root.
    #[wasm_bindgen_test]
    async fn duplicate_root_binds_to_explicit_project() {
        ensure_styles_loaded();
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Both projects use the SAME root string "/repo".
            state.projects.update(|p| {
                p.push(project_info("h1", "proj-1", "/repo"));
                p.push(project_info("h2", "proj-2", "/repo"));
            });
            let diff = small_foo_diff();
            // The diff body belongs to proj-2 (the tab's project), keyed by
            // its explicit identity even though proj-1 shares the same root.
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h2",
                        protocol::ProjectId("proj-2".to_owned()),
                        diff.root.clone(),
                        diff.scope,
                        "src/foo.rs",
                    ),
                    diff,
                );
            });
            let r1 = draft_review_for("rev-1", "proj-1", "src/foo.rs", 2, "review for project one");
            let r2 = draft_review_for("rev-2", "proj-2", "src/foo.rs", 2, "review for project two");
            state.review_summaries.update(|m| {
                m.insert(
                    protocol::ProjectId("proj-1".to_owned()),
                    vec![summary_for(&r1)],
                );
                m.insert(
                    protocol::ProjectId("proj-2".to_owned()),
                    vec![summary_for(&r2)],
                );
            });
            state.reviews.update(|m| {
                m.insert(r1.id.clone(), r1.clone());
                m.insert(r2.id.clone(), r2.clone());
            });
            provide_context(state);
            // This tab is for proj-2 (host h2), same root as proj-1.
            view! {
                <ReviewableDiffView
                    tab_id=crate::state::TabId(1)
                    host_id="h2".to_owned()
                    project_id=protocol::ProjectId("proj-2".to_owned())
                    root=review_root()
                    scope=ProjectDiffScope::Unstaged
                    path="src/foo.rs".to_owned()
                />
            }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("review for project two"),
            "overlay must bind to the tab's explicit project (proj-2); got: {text}"
        );
        assert!(
            !text.contains("review for project one"),
            "overlay must NOT bind to the other same-root project (proj-1); got: {text}"
        );
    }

    /// The integrated review surface is the whole-root unstaged diff (empty
    /// path), so every changed file renders and a comment on a non-first
    /// file shows — not just the first changed file.
    #[wasm_bindgen_test]
    async fn all_files_surface_shows_comment_on_non_first_file() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted = mount_reviewable(
            container.clone(),
            all_files_diff(),
            // Comment anchored to the SECOND file (src/bar.rs), new line 2.
            Some(draft_review("src/bar.rs", 2, "comment on the second file")),
        );
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        // Both files render on the all-files surface...
        assert!(
            text.contains("src/foo.rs") && text.contains("src/bar.rs"),
            "both changed files must render on the whole-root unstaged surface; got: {text}"
        );
        // ...and the comment on the non-first file is visible.
        assert!(
            text.contains("comment on the second file"),
            "a comment on a non-first changed file must render; got: {text}"
        );
        // The comment thread renders under bar.rs's region, not foo.rs's.
        let bar_region = container
            .query_selector("[data-rel-path=\"src/bar.rs\"]")
            .unwrap();
        assert!(
            bar_region.is_some(),
            "the second file's thread region must be present"
        );
    }

    /// A stale Draft *summary* must not keep the overlay alive once the full
    /// record has gone non-Draft (a live `StatusChanged` updates `reviews`
    /// before `review_summaries` refreshes). With a Draft summary but a
    /// Submitted full record, the integrated diff drops its comments/affordances
    /// and shows the start-a-review banner.
    #[wasm_bindgen_test]
    async fn submitted_record_with_stale_draft_summary_drops_overlay() {
        ensure_styles_loaded();
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .active_project
                .set(Some(crate::state::ActiveProjectRef {
                    host_id: "h1".to_owned(),
                    project_id: protocol::ProjectId("proj-1".to_owned()),
                }));
            let diff = small_foo_diff();
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "h1",
                        protocol::ProjectId("proj-1".to_owned()),
                        diff.root.clone(),
                        diff.scope,
                        "src/foo.rs",
                    ),
                    diff,
                );
            });
            // Stale Draft summary built while the review is still Draft...
            let mut review = draft_review("src/foo.rs", 2, "should be hidden");
            state.review_summaries.update(|m| {
                m.insert(
                    protocol::ProjectId("proj-1".to_owned()),
                    vec![summary_for(&review)],
                );
            });
            // ...but the live full record has already been Submitted.
            review.status = protocol::ReviewStatus::Submitted { submitted_at_ms: 1 };
            state.reviews.update(|m| {
                m.insert(review.id.clone(), review);
            });
            provide_context(state);
            view! {
                <ReviewableDiffView
                    tab_id=crate::state::TabId(1)
                    host_id="h1".to_owned()
                    project_id=protocol::ProjectId("proj-1".to_owned())
                    root=review_root()
                    scope=ProjectDiffScope::Unstaged
                    path="src/foo.rs".to_owned()
                />
            }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("should be hidden"),
            "a submitted (live) review must not overlay comments; got: {text}"
        );
        assert!(
            container
                .query_selector(".diff-gutter-clickable")
                .unwrap()
                .is_none(),
            "no comment gutters once the live review is non-draft"
        );
        // No start-a-review banner in the always-on model, even when non-draft.
        assert!(
            container
                .query_selector("[data-test=\"reviewable-diff-banner\"]")
                .unwrap()
                .is_none(),
            "no start-a-review banner (always-on model, even after non-draft transition)"
        );
    }

    /// Binary (and no-hunk) files render a clear placeholder, no diff lines,
    /// and — when a draft review exists — still expose the file-level
    /// comment affordance so a file-scoped comment can be left.
    #[wasm_bindgen_test]
    async fn binary_file_renders_placeholder_with_file_comment_affordance() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted = mount_reviewable(
            container.clone(),
            binary_diff("assets/logo.png"),
            Some(draft_review("assets/logo.png", 1, "file note")),
        );
        next_tick().await;
        next_tick().await;

        let placeholder = container
            .query_selector("[data-test=\"diff-binary-placeholder\"]")
            .unwrap();
        assert!(placeholder.is_some(), "expected a binary-file placeholder");
        assert_eq!(
            placeholder.unwrap().text_content().unwrap_or_default(),
            "Binary file changed",
            "placeholder should name the binary change"
        );
        // No line rows for a binary file.
        assert_eq!(
            container.query_selector_all(".diff-line").unwrap().length(),
            0,
            "binary files must not render diff lines"
        );
        // File-level comment affordance is still present.
        assert!(
            container
                .query_selector(".review-file-comment-btn")
                .unwrap()
                .is_some(),
            "expected a file-level comment affordance on the binary file header"
        );
    }

    #[wasm_bindgen_test]
    async fn unmerged_file_renders_conflict_placeholder_without_hunk_actions() {
        ensure_styles_loaded();
        let container = make_container();
        let _mounted =
            mount_reviewable(container.clone(), unmerged_diff("src/conflicted.rs"), None);
        next_tick().await;
        next_tick().await;

        let placeholder = container
            .query_selector("[data-test=\"diff-unmerged-placeholder\"]")
            .unwrap()
            .expect("expected an unmerged-file placeholder");
        assert_eq!(
            placeholder.text_content().unwrap_or_default(),
            "Unmerged file — resolve conflicts in the file, then stage it"
        );
        assert!(
            container
                .query_selector("[data-test=\"diff-binary-placeholder\"]")
                .unwrap()
                .is_none(),
            "unmerged state must take precedence over the no-hunk placeholder"
        );
        assert_eq!(
            container.query_selector_all(".diff-line").unwrap().length(),
            0
        );
        assert!(
            container
                .query_selector(".diff-hunk-stage-btn")
                .unwrap()
                .is_none(),
            "unmerged files must not expose ordinary hunk staging"
        );
    }

    fn diff_state_with_file(file_name: &str) -> DiffViewState {
        DiffViewState {
            root: review_root(),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::Hunks,
            pending: false,
            files: vec![one_added_line_file(file_name, "    let z = 0;")],
        }
    }

    /// Two projects/hosts that share the same root path string must keep
    /// separate diff bodies — keying `diff_contents` by explicit identity
    /// means one's response can't overwrite the other's tab.
    #[wasm_bindgen_test]
    async fn same_root_two_projects_render_distinct_diffs() {
        ensure_styles_loaded();
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Same root "/repo", different (host, project), different files.
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "hostA",
                        protocol::ProjectId("projA".to_owned()),
                        review_root(),
                        ProjectDiffScope::Uncommitted,
                        "",
                    ),
                    diff_state_with_file("alpha.rs"),
                );
                d.insert(
                    crate::state::DiffKey::new(
                        "hostB",
                        protocol::ProjectId("projB".to_owned()),
                        review_root(),
                        ProjectDiffScope::Uncommitted,
                        "",
                    ),
                    diff_state_with_file("beta.rs"),
                );
            });
            provide_context(state);
            view! {
                <div>
                    <DiffView
                        host_id="hostA".to_owned()
                        project_id=protocol::ProjectId("projA".to_owned())
                        root=review_root()
                        scope=ProjectDiffScope::Uncommitted
                        path=String::new()
                    />
                    <DiffView
                        host_id="hostB".to_owned()
                        project_id=protocol::ProjectId("projB".to_owned())
                        root=review_root()
                        scope=ProjectDiffScope::Uncommitted
                        path=String::new()
                    />
                </div>
            }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("alpha.rs"),
            "project A's diff must render its own file; got: {text}"
        );
        assert!(
            text.contains("beta.rs"),
            "project B's diff must render its own file (not overwritten); got: {text}"
        );
    }

    fn recorded_sends() -> String {
        js_sys::eval("(window.__sends || []).join('\\n')")
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default()
    }

    /// A context-mode refetch must address the *tab's* project/host, not the
    /// globally active project — even after the user switches the active
    /// project to a different one.
    #[wasm_bindgen_test]
    async fn context_mode_refetch_uses_tabs_project_not_active() {
        // Recording bridge: capture every send's args.
        let _ = js_sys::eval(
            "(function(){ \
               window.__sends = []; \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(cmd, args){ \
                 window.__sends.push(JSON.stringify(args || {})); return Promise.resolve(); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Tab belongs to projA on hostA.
            state.diff_context_mode.set(DiffContextMode::Hunks);
            state.diff_contents.update(|d| {
                d.insert(
                    crate::state::DiffKey::new(
                        "hostA",
                        protocol::ProjectId("projA".to_owned()),
                        review_root(),
                        ProjectDiffScope::Unstaged,
                        "foo.rs",
                    ),
                    DiffViewState {
                        root: review_root(),
                        scope: ProjectDiffScope::Unstaged,
                        path: Some("foo.rs".to_owned()),
                        context_mode: DiffContextMode::Hunks,
                        pending: false,
                        files: vec![one_added_line_file("foo.rs", "    let q = 1;")],
                    },
                );
            });
            // ...but the ACTIVE project is a different one (projB on hostB).
            state
                .active_project
                .set(Some(crate::state::ActiveProjectRef {
                    host_id: "hostB".to_owned(),
                    project_id: protocol::ProjectId("projB".to_owned()),
                }));
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <DiffView
                    host_id="hostA".to_owned()
                    project_id=protocol::ProjectId("projA".to_owned())
                    root=review_root()
                    scope=ProjectDiffScope::Unstaged
                    path="foo.rs".to_owned()
                />
            }
        });
        let _mounted = Mounted::new(handle, ());
        next_tick().await;

        // Toggle the context mode ⇒ refetch.
        let state = holder.borrow().clone().unwrap();
        state.diff_context_mode.set(DiffContextMode::FullFile);
        next_tick().await;
        next_tick().await;

        let sends = recorded_sends();
        assert!(
            sends.contains("/project/projA"),
            "refetch must target the tab's project (projA); sends: {sends}"
        );
        assert!(
            !sends.contains("/project/projB"),
            "refetch must NOT target the active project (projB); sends: {sends}"
        );
        assert!(
            sends.contains("hostA"),
            "refetch must go to the tab's host (hostA); sends: {sends}"
        );
    }
}
