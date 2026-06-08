use leptos::prelude::*;

use crate::components::diff_viewer::DiffViewer;
use crate::components::file_viewer::FileViewer;
use crate::components::ui::{Card, EmptyState, Pill, PillTone, StatusDot, StatusTone};
use crate::state::{ActiveProjectRef, AppState};

/// Per-host project list with read-only file tree once a project is
/// selected. Git state surfaces as a `Pill` (branch name) plus a
/// `StatusDot` for clean/dirty so users get the information without
/// having to open the project. File contents and diff are intentionally
/// out of scope until the dispatch can request them.
/// Local UI state for the active project's detail pane. Either a file
/// (path + root) is pinned, a diff (root + scope + optional file
/// filter) is open, or nothing is selected.
#[derive(Clone, Debug, PartialEq)]
enum ProjectDetail {
    None,
    File {
        root: protocol::ProjectRootPath,
        relative_path: String,
    },
    Diff {
        root: protocol::ProjectRootPath,
        scope: protocol::ProjectDiffScope,
        path: Option<String>,
    },
}

#[component]
pub fn ProjectsView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    // Detail pane is local UI state — tapping a file pins the file
    // viewer, tapping a "view diff" affordance pins the diff viewer.
    // Cleared when the active project changes via the `Effect` below
    // so a stale path from a previous project doesn't linger.
    let detail: RwSignal<ProjectDetail> = RwSignal::new(ProjectDetail::None);

    {
        let state = state.clone();
        Effect::new(move |_| {
            let _ = state.active_project.get();
            detail.set(ProjectDetail::None);
        });
    }

    view! {
        <div class="view projects-view" data-mobile-test="projects-view">
            <header class="view-header">
                <h1 class="view-title">"Projects"</h1>
            </header>
            <div class="view-body">
                {move || {
                    let active_host = state.active_local_host_id.get();
                    let projects: Vec<_> = state
                        .projects
                        .get()
                        .into_iter()
                        .filter(|p| {
                            active_host
                                .as_ref()
                                .is_some_and(|h| p.local_host_id == *h)
                        })
                        .collect();
                    let active_project = state.active_project.get();

                    if projects.is_empty() {
                        return view! {
                            <EmptyState
                                title="No projects"
                                body="Projects defined on your connected host show up here. Define a project on desktop to drive a chat scoped to its workspace roots."
                                icon="\u{1F4C1}"
                                data_mobile_test="projects-empty"
                            />
                        }.into_any();
                    }

                    view! {
                        <div class="project-list" data-mobile-test="projects-list">
                            {projects.into_iter().map(|project| {
                                let project_id = project.project.id.clone();
                                let host_id = project.local_host_id.clone();
                                let name = project.project.name.clone();
                                let root_count = project.project.roots.len();
                                let roots: Vec<String> = project.project.roots.iter()
                                    .map(|r| {
                                        r.rsplit('/').find(|s| !s.is_empty()).unwrap_or(r).to_string()
                                    })
                                    .collect();
                                let is_active = active_project
                                    .as_ref()
                                    .is_some_and(|ap| ap.local_host_id == host_id && ap.project_id == project_id);

                                let key = (host_id.clone(), project_id.clone());
                                let git_info = state.git_status.with(|gs| {
                                    gs.get(&key).map(|roots| {
                                        let total_changes: usize = roots.iter().map(|r| {
                                            r.files.len()
                                        }).sum();
                                        let branch = roots.first()
                                            .and_then(|r| r.branch.clone())
                                            .unwrap_or_default();
                                        let clean = roots.iter().all(|r| r.clean);
                                        (branch, total_changes, clean)
                                    })
                                });

                                let s_click = state.clone();
                                let host_for_click = host_id.clone();
                                let pid_for_click = project_id.clone();
                                let on_select = Callback::new(move |_: ()| {
                                    s_click.active_project.set(Some(ActiveProjectRef {
                                        local_host_id: host_for_click.clone(),
                                        project_id: pid_for_click.clone(),
                                    }));
                                });

                                let test = if is_active { "project-row-active" } else { "project-row" };
                                let aria_label = format!("Open project {name}");

                                view! {
                                    <Card
                                        data_mobile_test=test
                                        dense=true
                                        interactive=true
                                        aria_label=aria_label
                                        on_click=on_select
                                    >
                                        <div class="list-row list-row-flush list-row-flush-top">
                                            <div class="list-row-primary">
                                                <div class="list-row-title">
                                                    {name.clone()}
                                                    <Show when=move || is_active>
                                                        <span style="margin-left: var(--space-2);">
                                                            <Pill
                                                                label="Active"
                                                                tone=PillTone::Accent
                                                                data_mobile_test="project-active-pill"
                                                            />
                                                        </span>
                                                    </Show>
                                                </div>
                                                <div class="list-row-subtitle">
                                                    {format!("{root_count} root{}: {}",
                                                        if root_count == 1 { "" } else { "s" },
                                                        roots.join(", ")
                                                    )}
                                                </div>
                                            </div>
                                        </div>
                                        {git_info.map(|(branch, changes, clean)| {
                                            let tone = if clean { StatusTone::Online } else { StatusTone::Active };
                                            let label = if clean { "Clean working tree".to_string() } else { format!("{changes} uncommitted change{}", if changes == 1 { "" } else { "s" }) };
                                            view! {
                                                <div style="display: flex; align-items: center; gap: var(--space-2); margin-top: var(--space-2);" data-mobile-test="project-git-row">
                                                    <StatusDot
                                                        tone=tone
                                                        label=label.clone()
                                                    />
                                                    <Pill
                                                        label=branch
                                                        tone=PillTone::Neutral
                                                        data_mobile_test="project-git-branch"
                                                    />
                                                    {if !clean {
                                                        view! {
                                                            <Pill
                                                                label=format!("{changes} change{}", if changes == 1 { "" } else { "s" })
                                                                tone=PillTone::Warning
                                                                data_mobile_test="project-git-changes"
                                                            />
                                                        }.into_any()
                                                    } else {
                                                        view! { <span></span> }.into_any()
                                                    }}
                                                </div>
                                            }
                                        })}
                                    </Card>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        {move || {
                            // Show file tree of the active project (if any) right
                            // below the project list. Read-only — the dispatcher
                            // doesn't yet handle ProjectFileContents/ProjectGitDiff.
                            let active = state.active_project.get();
                            let Some(active) = active else { return view! { <div></div> }.into_any(); };
                            let key = (active.local_host_id.clone(), active.project_id.clone());
                            let listings = state.file_tree.with(|m| m.get(&key).cloned()).unwrap_or_default();
                            if listings.is_empty() {
                                return view! {
                                    <div class="project-detail" data-mobile-test="project-file-tree-empty">
                                        <EmptyState
                                            title="No files indexed yet"
                                            body="Your host hasn't pushed a file listing for this project. The list updates automatically as changes flow in."
                                            icon="\u{1F4C4}"
                                            data_mobile_test="projects-files-empty"
                                        />
                                    </div>
                                }.into_any();
                            }
                            let active_for_rows = active.clone();
                            // Reviews are always-on and root-scoped: comment,
                            // count, and submit controls live inline on the
                            // per-root "View diff" surface (`DiffViewer`),
                            // not in a separate reviews modal.
                            view! {
                                <div class="project-detail" data-mobile-test="project-file-tree">
                                    {listings.into_iter().map(|listing| {
                                        let root_path = listing.root.clone();
                                        let root_label = listing
                                            .root
                                            .0
                                            .rsplit('/')
                                            .find(|s| !s.is_empty())
                                            .unwrap_or(&listing.root.0)
                                            .to_string();
                                        let mut entries = listing.entries.clone();
                                        entries.sort_by(|a, b| {
                                            // Directories first, then by name.
                                            let a_dir = matches!(a.kind, protocol::ProjectFileKind::Directory);
                                            let b_dir = matches!(b.kind, protocol::ProjectFileKind::Directory);
                                            b_dir.cmp(&a_dir).then_with(|| a.relative_path.cmp(&b.relative_path))
                                        });
                                        let count = entries.len();
                                        let diff_root = root_path.clone();
                                        let on_view_diff = Callback::new(move |_: ()| {
                                            detail.set(ProjectDetail::Diff {
                                                root: diff_root.clone(),
                                                scope: protocol::ProjectDiffScope::Unstaged,
                                                path: None,
                                            });
                                        });
                                        view! {
                                            <div data-mobile-test="project-file-tree-root">
                                                <div class="section-heading">
                                                    <span>{root_label.clone()}</span>
                                                    <span class="section-heading-trailing">
                                                        <Pill
                                                            label=format!("{count}")
                                                            tone=PillTone::Neutral
                                                            data_mobile_test="project-file-tree-count"
                                                        />
                                                        <crate::components::ui::Button
                                                            label="View diff"
                                                            variant=crate::components::ui::ButtonVariant::Ghost
                                                            size=crate::components::ui::ButtonSize::Compact
                                                            data_mobile_test="project-view-diff"
                                                            aria_label=format!("View unstaged diff for {root_label}")
                                                            on_click=on_view_diff
                                                        />
                                                    </span>
                                                </div>
                                                <div class="project-file-tree">
                                                    {entries.into_iter().map(|entry| {
                                                        let is_dir = matches!(entry.kind, protocol::ProjectFileKind::Directory);
                                                        let icon = if is_dir { "\u{1F4C1}" } else { "\u{1F4C4}" };
                                                        let class = if is_dir { "project-file-row is-dir" } else { "project-file-row" };
                                                        let test = if is_dir { "project-file-row-dir" } else { "project-file-row-file" };
                                                        let row_root = root_path.clone();
                                                        let rel_path = entry.relative_path.clone();
                                                        let on_click = move |_| {
                                                            if !is_dir {
                                                                detail.set(ProjectDetail::File {
                                                                    root: row_root.clone(),
                                                                    relative_path: rel_path.clone(),
                                                                });
                                                            }
                                                        };
                                                        view! {
                                                            <div
                                                                class=class
                                                                data-mobile-test=test
                                                                role=if is_dir { "group" } else { "button" }
                                                                tabindex=if is_dir { "-1" } else { "0" }
                                                                on:click=on_click
                                                            >
                                                                <span class="project-file-row-icon" aria-hidden="true">{icon}</span>
                                                                <span class="project-file-row-name">{entry.relative_path}</span>
                                                            </div>
                                                        }
                                                    }).collect::<Vec<_>>()}
                                                </div>
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                    {
                                        let active_for_detail = active_for_rows.clone();
                                        move || {
                                            let on_clear = Callback::new(move |_: ()| detail.set(ProjectDetail::None));
                                            match detail.get() {
                                                ProjectDetail::None => view! { <div></div> }.into_any(),
                                                ProjectDetail::File { root, relative_path } => {
                                                    let path = protocol::ProjectPath {
                                                        root,
                                                        relative_path,
                                                    };
                                                    view! {
                                                        <FileViewer
                                                            project=active_for_detail.clone()
                                                            path=path
                                                            on_close=on_clear
                                                        />
                                                    }.into_any()
                                                }
                                                ProjectDetail::Diff { root, scope, path } => {
                                                    view! {
                                                        <DiffViewer
                                                            project=active_for_detail.clone()
                                                            root=root
                                                            scope=scope
                                                            path=path
                                                            on_close=on_clear
                                                        />
                                                    }.into_any()
                                                }
                                            }
                                        }
                                    }
                                </div>
                            }.into_any()
                        }}
                    }.into_any()
                }}
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId, ProjectInfo};
    use leptos::mount::mount_to;
    use protocol::{
        FileEntryOp, Project, ProjectFileEntry, ProjectFileKind, ProjectGitFileStatus, ProjectId,
        ProjectRootGitStatus, ProjectRootListing, ProjectRootPath,
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

    fn make_project(host: &LocalHostId, id: &str, name: &str, roots: Vec<&str>) -> ProjectInfo {
        ProjectInfo {
            local_host_id: host.clone(),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: name.to_owned(),
                roots: roots.into_iter().map(str::to_owned).collect(),
                sort_order: 0,
            },
        }
    }

    /// Empty list shows the structured empty state.
    #[wasm_bindgen_test]
    async fn projects_empty_renders_empty_state() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <ProjectsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='projects-empty']")
                .unwrap()
                .is_some(),
            "empty state must render with semantic selector"
        );
    }

    /// Git status drives the branch pill and the change-count badge.
    /// Dirty trees get a "N change(s)" pill; clean ones get none.
    #[wasm_bindgen_test]
    async fn projects_git_status_renders_branch_and_change_pill() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.projects.set(vec![
                make_project(&host_clone, "p-dirty", "Dirty", vec!["/x/dirty"]),
                make_project(&host_clone, "p-clean", "Clean", vec!["/x/clean"]),
            ]);
            state.git_status.update(|m| {
                m.insert(
                    (host_clone.clone(), ProjectId("p-dirty".to_owned())),
                    vec![ProjectRootGitStatus {
                        root: ProjectRootPath("/x/dirty".to_owned()),
                        branch: Some("main".to_owned()),
                        ahead: 0,
                        behind: 0,
                        clean: false,
                        files: vec![ProjectGitFileStatus {
                            relative_path: "a.txt".to_owned(),
                            staged: None,
                            unstaged: Some(protocol::ProjectGitChangeKind::Modified),
                            untracked: false,
                        }],
                    }],
                );
                m.insert(
                    (host_clone.clone(), ProjectId("p-clean".to_owned())),
                    vec![ProjectRootGitStatus {
                        root: ProjectRootPath("/x/clean".to_owned()),
                        branch: Some("develop".to_owned()),
                        ahead: 0,
                        behind: 0,
                        clean: true,
                        files: Vec::new(),
                    }],
                );
            });
            provide_context(state);
            view! { <ProjectsView /> }
        });
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("main"), "dirty branch name must appear");
        assert!(text.contains("develop"), "clean branch name must appear");
        // At least one "change" pill must exist for the dirty project.
        assert!(
            container
                .query_selector("[data-mobile-test='project-git-changes']")
                .unwrap()
                .is_some(),
            "dirty project must surface a changes pill"
        );
    }

    /// Selecting a project surfaces its file tree below. Directory
    /// entries get a dir selector, file entries get a file selector.
    #[wasm_bindgen_test]
    async fn projects_active_project_shows_file_tree() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state
                .projects
                .set(vec![make_project(&host_clone, "p-1", "Active", vec!["/x"])]);
            state.active_project.set(Some(ActiveProjectRef {
                local_host_id: host_clone.clone(),
                project_id: ProjectId("p-1".to_owned()),
            }));
            state.file_tree.update(|m| {
                m.insert(
                    (host_clone.clone(), ProjectId("p-1".to_owned())),
                    vec![ProjectRootListing {
                        root: ProjectRootPath("/x".to_owned()),
                        entries: vec![
                            ProjectFileEntry {
                                relative_path: "src".to_owned(),
                                kind: ProjectFileKind::Directory,
                                op: FileEntryOp::Add,
                            },
                            ProjectFileEntry {
                                relative_path: "README.md".to_owned(),
                                kind: ProjectFileKind::File,
                                op: FileEntryOp::Add,
                            },
                        ],
                    }],
                );
            });
            provide_context(state);
            view! { <ProjectsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-tree']")
                .unwrap()
                .is_some(),
            "file tree container must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-row-dir']")
                .unwrap()
                .is_some(),
            "directory entry must use directory selector"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-row-file']")
                .unwrap()
                .is_some(),
            "file entry must use file selector"
        );
    }

    /// Tapping a file row pins the file-viewer placeholder with the
    /// selected path. Tapping a directory row must NOT open the viewer
    /// — directories are pure structure.
    #[wasm_bindgen_test]
    async fn projects_file_tap_opens_viewer_placeholder_with_path() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state
                .projects
                .set(vec![make_project(&host_clone, "p-1", "Active", vec!["/x"])]);
            state.active_project.set(Some(ActiveProjectRef {
                local_host_id: host_clone.clone(),
                project_id: ProjectId("p-1".to_owned()),
            }));
            state.file_tree.update(|m| {
                m.insert(
                    (host_clone.clone(), ProjectId("p-1".to_owned())),
                    vec![ProjectRootListing {
                        root: ProjectRootPath("/x".to_owned()),
                        entries: vec![
                            ProjectFileEntry {
                                relative_path: "src".to_owned(),
                                kind: ProjectFileKind::Directory,
                                op: FileEntryOp::Add,
                            },
                            ProjectFileEntry {
                                relative_path: "README.md".to_owned(),
                                kind: ProjectFileKind::File,
                                op: FileEntryOp::Add,
                            },
                        ],
                    }],
                );
            });
            provide_context(state);
            view! { <ProjectsView /> }
        });
        next_tick().await;

        // Tap a directory: no viewer should appear.
        let dir: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='project-file-row-dir']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        dir.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer']")
                .unwrap()
                .is_none(),
            "tapping a directory must not open the viewer"
        );

        // Tap a file: viewer appears with the file path.
        let file: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='project-file-row-file']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        file.click();
        next_tick().await;
        let viewer = container
            .query_selector("[data-mobile-test='project-file-viewer']")
            .unwrap()
            .expect("viewer placeholder must appear after tapping a file");
        let path = viewer
            .query_selector("[data-mobile-test='project-file-viewer-path']")
            .unwrap()
            .expect("viewer must expose the selected path");
        assert_eq!(
            path.text_content().unwrap_or_default().trim(),
            "README.md",
            "viewer must show the selected file's path"
        );
        // Closing the viewer clears the placeholder.
        let close: web_sys::HtmlElement = viewer
            .query_selector("[data-mobile-test='project-file-viewer-close']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        close.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer']")
                .unwrap()
                .is_none(),
            "viewer must close after tapping Close"
        );
    }
}
