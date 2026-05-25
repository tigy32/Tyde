use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, EmptyState, Pill, PillTone, Spinner,
};
use crate::state::{ActiveProjectRef, AppState, ProjectDiffRef};

/// Unified git diff viewer for an active project's root.
///
/// On mount, fires `ProjectReadDiff` with the requested scope/path.
/// Renders the diff inline below the file tree (Projects view) or
/// inside a review-detail diff tab. Pure read-only — no stage / discard
/// / commit affordances. The protocol exposes those frames, but they
/// are too destructive for a phone-sized UI in v1.
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

    let key = ProjectDiffRef {
        local_host_id: project.local_host_id.clone(),
        project_id: project.project_id.clone(),
        root: root.clone(),
        scope,
        path: path.clone(),
    };

    // Kick the request on mount and re-fire when context_mode changes.
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
    let entry = move || {
        state
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
            <div class="project-diff-viewer-body">
                {move || render_body(entry())}
            </div>
        </div>
    }
}

fn render_body(entry: Option<crate::state::ProjectDiffState>) -> AnyView {
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
                    view! { <DiffFileBlock file=file /> }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
    .into_any()
}

#[component]
fn DiffFileBlock(file: protocol::ProjectGitDiffFile) -> impl IntoView {
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
            <div class="project-diff-hunks">
                {file.hunks.into_iter().map(|hunk| {
                    view! { <DiffHunkBlock hunk=hunk /> }
                }).collect::<Vec<_>>()}
            </div>
        </details>
    }
}

#[component]
fn DiffHunkBlock(hunk: protocol::ProjectGitDiffHunk) -> impl IntoView {
    let header = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    );
    view! {
        <div class="project-diff-hunk" data-mobile-test="project-diff-hunk">
            <div class="project-diff-hunk-header">{header}</div>
            <div class="project-diff-hunk-lines">
                {hunk.lines.into_iter().map(|line| {
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
                    view! {
                        <div class=format!("project-diff-line {kind_class}") data-mobile-test=test_id>
                            <span class="project-diff-line-no" aria-hidden="true">{line_no}</span>
                            <span class="project-diff-line-marker" aria-hidden="true">{marker}</span>
                            <span class="project-diff-line-text">{line.text}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
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
        DiffContextMode, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffHunk,
        ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectId, ProjectRootPath,
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
}
