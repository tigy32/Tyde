use leptos::prelude::*;

use crate::components::diff_viewer::DiffViewer;
use crate::components::ui::{Card, EmptyState, Pill, PillTone, StatusDot, StatusTone};
use crate::state::{ActiveProjectRef, AppState};

/// Per-host project list, with each root's diff reachable once a project is
/// selected. Git state surfaces as a `Pill` (branch name) plus a
/// `StatusDot` for clean/dirty so users get the information without
/// having to open the project. There is deliberately no file browser: mobile
/// never rendered one usefully, and the listing that fed it dominated the
/// connect payload.
/// Local UI state for the active project's detail pane. Either a diff
/// (root + scope + optional file filter) is open, or nothing is selected.
#[derive(Clone, Debug, PartialEq)]
enum ProjectDetail {
    None,
    Diff {
        root: protocol::ProjectRootPath,
        scope: protocol::ProjectDiffScope,
        path: Option<String>,
    },
}

#[component]
pub fn ProjectsView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    // Detail pane is local UI state — tapping a "view diff" affordance pins
    // the diff viewer. Cleared when the active project changes via the
    // `Effect` below so a stale root from a previous project doesn't linger.
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
                                // Workbenches (git worktrees) render indented
                                // beneath their parent with a branch badge.
                                // Display only — mobile cannot create or
                                // remove workbenches.
                                let workbench_branch = match &project.project.source {
                                    protocol::ProjectSource::GitWorkbench { branch, .. } => {
                                        Some(branch.0.clone())
                                    }
                                    protocol::ProjectSource::Standalone { .. } => None,
                                };
                                let is_workbench = workbench_branch.is_some();
                                let project_roots = project.project.root_paths();
                                let root_count = project_roots.len();
                                let roots: Vec<String> = project_roots.iter()
                                    .map(|r| {
                                        r.0.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&r.0).to_string()
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
                                    crate::actions::select_project(
                                        &s_click,
                                        ActiveProjectRef {
                                            local_host_id: host_for_click.clone(),
                                            project_id: pid_for_click.clone(),
                                        },
                                    );
                                });

                                let test = if is_active {
                                    "project-row-active"
                                } else if is_workbench {
                                    "project-row-workbench"
                                } else {
                                    "project-row"
                                };
                                let aria_label = if let Some(branch) = &workbench_branch {
                                    format!("Open workbench {name} on branch {branch}")
                                } else {
                                    format!("Open project {name}")
                                };
                                let item_class = if is_workbench {
                                    "project-list-item project-list-item-workbench"
                                } else {
                                    "project-list-item"
                                };

                                view! {
                                    <div class=item_class>
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
                                                    {workbench_branch.clone().map(|branch| view! {
                                                        <span style="margin-left: var(--space-2);">
                                                            <Pill
                                                                label=format!("\u{2387} {branch}")
                                                                tone=PillTone::Neutral
                                                                data_mobile_test="project-workbench-branch"
                                                            />
                                                        </span>
                                                    })}
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
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        {move || {
                            // Per-root entry points for the active project.
                            // Mobile has no file browser, so roots come from
                            // project metadata rather than a file listing.
                            let active = state.active_project.get();
                            let Some(active) = active else { return view! { <div></div> }.into_any(); };
                            let key = (active.local_host_id.clone(), active.project_id.clone());
                            let roots = state.projects.with(|projects| {
                                projects
                                    .iter()
                                    .find(|p| p.local_host_id == key.0 && p.project.id == key.1)
                                    .map(|p| p.project.root_paths())
                                    .unwrap_or_default()
                            });
                            if roots.is_empty() {
                                return view! {
                                    <div class="project-detail" data-mobile-test="project-roots-empty">
                                        <EmptyState
                                            title="No roots configured"
                                            body="This project has no roots on the host, so there is nothing to diff yet."
                                            icon="\u{1F4C1}"
                                            data_mobile_test="projects-roots-empty"
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
                                <div class="project-detail" data-mobile-test="project-roots">
                                    {roots.into_iter().map(|root_path| {
                                        let root_label = root_path
                                            .0
                                            .rsplit('/')
                                            .find(|s| !s.is_empty())
                                            .unwrap_or(&root_path.0)
                                            .to_string();
                                        let diff_root = root_path.clone();
                                        let on_view_diff = Callback::new(move |_: ()| {
                                            detail.set(ProjectDetail::Diff {
                                                root: diff_root.clone(),
                                                scope: protocol::ProjectDiffScope::Unstaged,
                                                path: None,
                                            });
                                        });
                                        view! {
                                            <div data-mobile-test="project-root-row">
                                                <div class="section-heading">
                                                    <span>{root_label.clone()}</span>
                                                    <span class="section-heading-trailing">
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
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                    {
                                        let active_for_detail = active_for_rows.clone();
                                        move || {
                                            let on_clear = Callback::new(move |_: ()| detail.set(ProjectDetail::None));
                                            match detail.get() {
                                                ProjectDetail::None => view! { <div></div> }.into_any(),
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
        Project, ProjectGitFileStatus, ProjectId, ProjectRootGitStatus, ProjectRootPath,
        ProjectSource,
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
                source: ProjectSource::Standalone {
                    roots: roots
                        .into_iter()
                        .map(|root| ProjectRootPath(root.to_owned()))
                        .collect(),
                },
                sort_order: 0,
            },
        }
    }

    /// Capture sends through the production web connection manager seam.
    fn record_bridge() -> crate::bridge::TestSendGuard {
        crate::bridge::test_capture_sends()
    }

    fn sent_lines_joined() -> String {
        crate::bridge::test_sent_lines().join("\n")
    }

    /// Tapping a project row selects it and notifies the host with a
    /// `project_accessed` frame on the project stream. Before any selection
    /// nothing is sent.
    #[wasm_bindgen_test]
    async fn selecting_project_sends_project_accessed() {
        let _send_guard = record_bridge();
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state
                .projects
                .set(vec![make_project(&host_clone, "p-1", "Proj", vec!["/x"])]);
            provide_context(state);
            view! { <ProjectsView /> }
        });
        next_tick().await;

        // No project is active yet → nothing on the wire.
        assert!(
            !sent_lines_joined().contains("project_accessed"),
            "no project_accessed should be sent before a selection; sent: {}",
            sent_lines_joined()
        );

        let row = container
            .query_selector("[data-mobile-test='project-row']")
            .unwrap()
            .expect("a project row must render");
        row.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;

        let sent = sent_lines_joined();
        assert!(
            sent.contains("project_accessed"),
            "selecting a project must send a project_accessed frame; sent: {sent}"
        );
        assert!(
            sent.contains("/project/p-1"),
            "project_accessed must target the project stream /project/p-1; sent: {sent}"
        );
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
                        head_oid: None,
                        empty_tree_oid: None,
                        ahead: 0,
                        behind: 0,
                        clean: false,
                        files: vec![ProjectGitFileStatus {
                            relative_path: "a.txt".to_owned(),
                            staged: None,
                            unstaged: Some(protocol::ProjectGitChangeKind::Modified),
                            untracked: false,
                        }],
                        recent_commits: Vec::new(),
                        history_has_more: false,
                    }],
                );
                m.insert(
                    (host_clone.clone(), ProjectId("p-clean".to_owned())),
                    vec![ProjectRootGitStatus {
                        root: ProjectRootPath("/x/clean".to_owned()),
                        branch: Some("develop".to_owned()),
                        head_oid: None,
                        empty_tree_oid: None,
                        ahead: 0,
                        behind: 0,
                        clean: true,
                        files: Vec::new(),
                        recent_commits: Vec::new(),
                        history_has_more: false,
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

    /// Selecting a project surfaces one row per configured root, each with a
    /// reachable "View diff". Mobile has no file browser, so roots come from
    /// project metadata; this pins the guarantee the old file-tree block also
    /// carried, since the diff entry point lived inside it.
    #[wasm_bindgen_test]
    async fn projects_active_project_shows_roots_with_diff_access() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_clone.clone()));
            state.projects.set(vec![make_project(
                &host_clone,
                "p-1",
                "Active",
                vec!["/x", "/y"],
            )]);
            state.active_project.set(Some(ActiveProjectRef {
                local_host_id: host_clone.clone(),
                project_id: ProjectId("p-1".to_owned()),
            }));
            provide_context(state);
            view! { <ProjectsView /> }
        });
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[data-mobile-test='project-root-row']")
                .unwrap()
                .length(),
            2,
            "one row must render per configured project root"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-row-file']")
                .unwrap()
                .is_none(),
            "mobile must not render a file browser"
        );

        // The diff viewer must still be reachable from a root row.
        let view_diff: web_sys::HtmlElement = container
            .query_selector("[data-mobile-test='project-view-diff']")
            .unwrap()
            .expect("each root must expose a View diff control")
            .dyn_into()
            .unwrap();
        view_diff.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-diff-viewer']")
                .unwrap()
                .is_some(),
            "tapping View diff must open the diff viewer"
        );
    }
}
