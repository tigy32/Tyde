use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::{delete_project_root, open_project_path_at};
use crate::components::host_browser::open_add_root_browser;
use crate::state::{ActiveProjectRef, AppState, OpenTarget, display_path_name, root_display_name};

use protocol::{
    CodeIntelOverviewHeadline, CodeIntelOverviewSummary, CodeIntelState, ProjectFileKind,
    ProjectFileView, ProjectFileViewState, ProjectFileWindow, ProjectId, ProjectPath,
    ProjectRootPath, ProjectRootStatus,
};

fn file_type_icon(name: &str) -> &'static str {
    if let Some(ext) = name.rsplit('.').next() {
        match ext {
            "rs" => "rs",
            "ts" | "tsx" => "ts",
            "js" | "jsx" => "js",
            "json" => "{}",
            "md" => "#",
            "css" | "scss" => "*",
            "toml" | "yaml" | "yml" => "\u{2699}",
            _ => "\u{25fb}",
        }
    } else {
        "\u{25fb}"
    }
}

#[component]
pub fn FileExplorer() -> impl IntoView {
    let state = expect_context::<AppState>();
    let filter = RwSignal::new(String::new());
    let show_hidden = RwSignal::new(false);
    let expanded_dirs = RwSignal::new(HashSet::<ProjectPath>::new());
    let windows = RwSignal::new(HashMap::<ProjectPath, ProjectFileWindow>::new());
    let collapsed_roots = RwSignal::new(HashSet::<String>::new());

    // Other projects' listings change the shared map; selecting the active
    // project's roots first keeps their updates from rebuilding this tree.
    let active_roots = Memo::new(move |_| {
        let pid = state.active_project.get()?.project_id;
        state.file_tree.with(|file_map| {
            file_map.get(&pid).map(|roots| {
                roots
                    .iter()
                    .map(|root| RootTree {
                        root: root.root.clone(),
                        status: root.status.clone(),
                    })
                    .collect::<Vec<_>>()
            })
        })
    });
    let tree = active_roots;
    // Declaration lifetime follows explicit UI intent, not transient row mounting.
    let declared = StoredValue::new((
        None::<ActiveProjectRef>,
        None::<u64>,
        Vec::<ProjectFileView>::new(),
    ));
    Effect::new(move |_| {
        let project = state.active_project.get();
        let previous = declared.get_value();
        if previous.0 != project {
            windows.update(HashMap::clear);
        }
        let epoch = project.as_ref().and_then(|project| {
            state
                .file_view_epochs
                .with(|epochs| epochs.get(project).copied())
        });
        let expanded = expanded_dirs.get();
        let collapsed = collapsed_roots.get();
        let mut desired = Vec::new();
        if project.is_some()
            && let Some(roots) = active_roots.get()
        {
            for root in roots {
                if collapsed.contains(&root.root.0) || root.status != ProjectRootStatus::Available {
                    continue;
                }
                let root_path = ProjectPath {
                    root: root.root.clone(),
                    relative_path: String::new(),
                };
                let paths = std::iter::once(root_path).chain(
                    expanded
                        .iter()
                        .filter(|path| {
                            if path.root != root.root {
                                return false;
                            }
                            let mut ancestor = path.relative_path.as_str();
                            while let Some((parent, _)) = ancestor.rsplit_once('/') {
                                if !expanded.contains(&ProjectPath {
                                    root: path.root.clone(),
                                    relative_path: parent.to_owned(),
                                }) {
                                    return false;
                                }
                                ancestor = parent;
                            }
                            true
                        })
                        .cloned(),
                );
                for path in paths {
                    let window = windows
                        .with(|windows| windows.get(&path).cloned())
                        .unwrap_or(ProjectFileWindow::First);
                    desired.push(ProjectFileView::Directory {
                        path,
                        window,
                        show_hidden: show_hidden.get(),
                        filter: filter.get().to_lowercase(),
                    });
                }
            }
        }
        if let Some(previous_project) = previous.0.as_ref() {
            for view in &previous.2 {
                if previous.0 != project
                    || !desired.iter().any(|current| match (view, current) {
                        (
                            ProjectFileView::Directory { path: old, .. },
                            ProjectFileView::Directory { path: new, .. },
                        ) => old == new,
                        _ => false,
                    })
                {
                    crate::send::project_file_view(previous_project.clone(), view.clone(), false);
                }
            }
        }
        if let Some(project) = project.as_ref() {
            for view in &desired {
                if previous.0.as_ref() != Some(project)
                    || previous.1 != epoch
                    || !previous.2.contains(view)
                {
                    crate::send::project_file_view(project.clone(), view.clone(), true);
                }
            }
        }
        declared.set_value((project, epoch, desired));
    });
    Effect::new(move |_| {
        let Some(project) = state.active_project.get() else {
            return;
        };
        let removed = state.file_views.with(|views| {
            views
                .get(&project)
                .into_iter()
                .flatten()
                .filter_map(|page| match (&page.view, &page.state) {
                    (
                        ProjectFileView::Directory { path, .. },
                        ProjectFileViewState::DirectoryMissing,
                    ) if !path.relative_path.is_empty() => Some(path.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        if removed.is_empty() {
            return;
        }
        let keep = |path: &ProjectPath| {
            !removed.iter().any(|removed| {
                path.root == removed.root
                    && (path == removed
                        || path
                            .relative_path
                            .starts_with(&format!("{}/", removed.relative_path)))
            })
        };
        expanded_dirs.update(|expanded| expanded.retain(keep));
        windows.update(|windows| windows.retain(|path, _| keep(path)));
    });
    on_cleanup(move || {
        let (project, _, views) = declared.get_value();
        if let Some(project) = project {
            for view in views {
                crate::send::project_file_view(project.clone(), view, false);
            }
        }
    });

    let project_header = Memo::new(move |_| {
        let active = state.active_project.get()?;
        state
            .projects
            .get()
            .into_iter()
            .find(|project| {
                project.host_id == active.host_id && project.project.id == active.project_id
            })
            .map(|project| {
                let root_paths = project.project.root_paths();
                let root_count = root_paths.len();
                let title = root_paths
                    .iter()
                    .map(|root| root.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                let label = if root_count == 1 {
                    root_paths
                        .first()
                        .map(|root| display_path_name(&root.0))
                        .unwrap_or_else(|| project.project.name.clone())
                } else {
                    format!("{} · {root_count} roots", project.project.name)
                };
                (label, title)
            })
    });

    let on_filter_input = move |ev: leptos::ev::Event| {
        batch(|| {
            windows.update(HashMap::clear);
            filter.set(event_target_value(&ev));
        });
    };

    let toggle_hidden = move |_| {
        batch(|| {
            windows.update(HashMap::clear);
            show_hidden.update(|v| *v = !*v);
        });
    };

    let state_for_add_root = state.clone();
    let on_add_root = move |_| open_add_root_browser(&state_for_add_root);
    let state_for_can_manage = state.clone();
    let can_manage_roots = move || {
        state_for_can_manage
            .active_project
            .get()
            .is_some_and(|active| {
                state_for_can_manage.can_manage_project_roots(&active.host_id, &active.project_id)
            })
    };
    let can_manage_for_button = can_manage_roots.clone();
    let add_root_disabled = move || !can_manage_for_button();
    let state_for_add_title = state.clone();
    let can_manage_for_title = can_manage_roots.clone();
    let add_root_title = move || {
        if can_manage_for_title() {
            "Add workspace root"
        } else if state_for_add_title.active_project.get().is_none() {
            "Open a project to add a workspace root"
        } else {
            "Workbench roots are managed via Create/Remove Workbench; \
             remove all child workbenches before editing the parent's roots"
        }
    };

    view! {
        <div class="file-explorer">
            <crate::notices::InlineNotices scopes=Signal::derive(move || state.active_project.get()
                .map(|project| vec![crate::notices::NoticeScope::Project(project.host_id, project.project_id)])
                .unwrap_or_default()) />
            <div class="fe-header">
                <span class="fe-breadcrumb" title=move || project_header.get().map(|(_, title)| title).unwrap_or_default()>
                    {move || {
                        project_header.get()
                            .map(|(label, _)| label)
                            .unwrap_or_else(|| "No project".to_owned())
                    }}
                </span>
                <button
                    class="fe-add-root"
                    title=add_root_title
                    on:click=on_add_root
                    disabled=add_root_disabled
                >
                    "+ root"
                </button>
                <button
                    class="fe-toggle-hidden"
                    title="Toggle hidden files"
                    on:click=toggle_hidden
                >
                    {move || if show_hidden.get() { "H" } else { "h" }}
                </button>
            </div>
            <div class="fe-search">
                <input
                    class="panel-search-input"
                    type="text"
                    placeholder="Filter files..."
                    prop:value=move || filter.get()
                    on:input=on_filter_input
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                />
            </div>
            <div class="fe-tree">
                {move || {
                    match tree.get() {
                        Some(root_trees) => {
                            let filter_val = filter.get().to_lowercase();
                            let hidden = show_hidden.get();
                            let active = state.active_project.get();
                            let project_ref = active.map(|active| (active.host_id, active.project_id));
                            root_trees
                                .into_iter()
                                .flat_map(|root_tree| {
                                    render_root_section(
                                        root_tree,
                                        &filter_val,
                                        hidden,
                                        expanded_dirs,
                                        windows,
                                        collapsed_roots,
                                        project_ref.clone(),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }
                        None => vec![
                            view! {
                                <div class="panel-empty">"No files loaded"</div>
                            }.into_any()
                        ],
                    }
                }}
            </div>
            <CodeIntelFooter />
        </div>
    }
}

/// Maps a server-authored provider state to its short display label. This is a
/// pure rendering of the enum, not an inference about provider behavior.
fn state_label(state: CodeIntelState) -> &'static str {
    match state {
        CodeIntelState::Disabled => "Off",
        CodeIntelState::Ready => "Ready",
        CodeIntelState::Indexing => "Indexing",
        CodeIntelState::Starting => "Starting",
        CodeIntelState::Unavailable => "Unavailable",
        CodeIntelState::Failed => "Failed",
        CodeIntelState::Unsupported => "Not started",
    }
}

fn state_dot_class(state: CodeIntelState) -> &'static str {
    match state {
        CodeIntelState::Disabled => "idle",
        CodeIntelState::Ready => "ready",
        CodeIntelState::Indexing => "indexing",
        CodeIntelState::Starting => "starting",
        CodeIntelState::Unavailable => "unavailable",
        CodeIntelState::Failed => "failed",
        CodeIntelState::Unsupported => "idle",
    }
}

fn headline_dot_class(headline: CodeIntelOverviewHeadline) -> &'static str {
    match headline {
        CodeIntelOverviewHeadline::Disabled | CodeIntelOverviewHeadline::NotStarted => "idle",
        CodeIntelOverviewHeadline::Starting => "starting",
        CodeIntelOverviewHeadline::Indexing => "indexing",
        CodeIntelOverviewHeadline::Ready => "ready",
        CodeIntelOverviewHeadline::Unavailable => "unavailable",
        CodeIntelOverviewHeadline::Failed => "failed",
    }
}

/// Renders the server-authored overview headline into a collapsed aggregate
/// label + dot class. The headline is authoritative (including `NotStarted`);
/// the state counts only enrich the indexing progress text. The server-owned
/// error/warning totals are appended (e.g. "Ready · 2 errors") so the footer
/// never claims a clean bill of health while the project has compile errors.
fn aggregate_label(summary: &CodeIntelOverviewSummary) -> (String, &'static str) {
    let class = headline_dot_class(summary.headline);
    let mut label = match summary.headline {
        CodeIntelOverviewHeadline::Disabled => "Off".to_owned(),
        CodeIntelOverviewHeadline::NotStarted => "Not started".to_owned(),
        CodeIntelOverviewHeadline::Starting => "Starting".to_owned(),
        CodeIntelOverviewHeadline::Indexing => {
            let total = summary.ready
                + summary.indexing
                + summary.starting
                + summary.unavailable
                + summary.failed;
            format!("Indexing · {} of {total} servers", summary.indexing)
        }
        CodeIntelOverviewHeadline::Ready => "Ready".to_owned(),
        CodeIntelOverviewHeadline::Unavailable => "Unavailable".to_owned(),
        CodeIntelOverviewHeadline::Failed => "Failed".to_owned(),
    };
    if summary.error_count > 0 {
        let noun = if summary.error_count == 1 {
            "error"
        } else {
            "errors"
        };
        label = format!("{label} · {} {noun}", summary.error_count);
    }
    if summary.warning_count > 0 {
        let noun = if summary.warning_count == 1 {
            "warning"
        } else {
            "warnings"
        };
        label = format!("{label} · {} {noun}", summary.warning_count);
    }
    (label, class)
}

/// Sticky footer at the bottom of the Files panel that renders the server's
/// code-intelligence overview for the active project. Collapsed view shows the
/// aggregate state and the server-authored message; multi-root/provider detail
/// lives behind an expander to avoid clutter.
#[component]
fn CodeIntelFooter() -> impl IntoView {
    let state = expect_context::<AppState>();
    let expanded = RwSignal::new(false);

    let overview = Memo::new(move |_| {
        // Key by the full active-project ref (host + project) so a code-intel
        // overview from one host can't render under a same-id project on another.
        let active = state.active_project.get()?;
        state.code_intel_overview.get().get(&active).cloned()
    });
    let has_project = move || state.active_project.get().is_some();

    view! {
        {move || {
            if !has_project() {
                return ().into_any();
            }
            match overview.get() {
                None => view! {
                    <div class="fe-codeintel" data-test="fe-codeintel-footer">
                        <div class="fe-ci-summary">
                            <span class="fe-ci-dot fe-ci-idle"></span>
                            <span
                                class="fe-ci-label"
                                data-test="fe-codeintel-label"
                            >
                                "Code Intel: Loading…"
                            </span>
                        </div>
                    </div>
                }
                .into_any(),
                Some(ov) => {
                    let (label, dot_class) = aggregate_label(&ov.summary);
                    let roots = ov.roots.clone();
                    let can_expand = !roots.is_empty();
                    let is_expanded = move || can_expand && expanded.get();
                    let toggle = move |_| {
                        if can_expand {
                            expanded.update(|v| *v = !*v);
                        }
                    };

                    view! {
                        <div class="fe-codeintel" data-test="fe-codeintel-footer">
                            <button
                                class="fe-ci-summary"
                                class:fe-ci-clickable=move || can_expand
                                on:click=toggle
                                disabled=move || !can_expand
                                aria-expanded=move || if is_expanded() { "true" } else { "false" }
                                title="Code Intel status"
                            >
                                <span class=format!("fe-ci-dot fe-ci-{dot_class}")></span>
                                <span class="fe-ci-label" data-test="fe-codeintel-label">
                                    {format!("Code Intel: {label}")}
                                </span>
                                {can_expand.then(|| view! {
                                    <span class="fe-ci-chevron">
                                        {move || if is_expanded() { "\u{25be}" } else { "\u{25b8}" }}
                                    </span>
                                })}
                            </button>
                            {ov.summary.message.clone().map(|msg| view! {
                                <div class="fe-ci-message" data-test="fe-codeintel-message">
                                    {msg}
                                </div>
                            })}
                            {move || is_expanded().then(|| {
                                roots
                                    .iter()
                                    .cloned()
                                    .map(render_root_overview)
                                    .collect::<Vec<_>>()
                            })}
                        </div>
                    }
                    .into_any()
                }
            }
        }}
    }
}

fn render_root_overview(root: protocol::CodeIntelRootOverview) -> impl IntoView {
    let root_label = root_display_name(&root.root);
    let root_title = root.root.0.clone();
    let providers = if root.providers.is_empty() {
        vec![
            view! {
                <div class="fe-ci-provider fe-ci-provider--idle">"Idle"</div>
            }
            .into_any(),
        ]
    } else {
        root.providers
            .iter()
            .map(|provider| {
                let dot_class = state_dot_class(provider.state);
                let name = format!("{} · {}", provider.provider, provider.language);
                let mut detail = state_label(provider.state).to_owned();
                if let (CodeIntelState::Indexing, Some(done), Some(total)) =
                    (provider.state, provider.work_done, provider.total_work)
                {
                    detail = format!("{detail} {done}/{total}");
                }
                view! {
                    <div class="fe-ci-provider" data-test="fe-codeintel-provider">
                        <span class=format!("fe-ci-dot fe-ci-{dot_class}")></span>
                        <span class="fe-ci-provider-name">{name}</span>
                        <span class="fe-ci-provider-state">{detail}</span>
                    </div>
                }
                .into_any()
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="fe-ci-root" data-test="fe-codeintel-root">
            <div class="fe-ci-root-name" title=root_title>{root_label}</div>
            {providers}
        </div>
    }
}

#[derive(Clone, PartialEq)]
struct RootTree {
    root: ProjectRootPath,
    status: ProjectRootStatus,
}

fn render_root_section(
    root_tree: RootTree,
    filter: &str,
    show_hidden: bool,
    expanded_dirs: RwSignal<HashSet<ProjectPath>>,
    windows: RwSignal<HashMap<ProjectPath, ProjectFileWindow>>,
    collapsed_roots: RwSignal<HashSet<String>>,
    project_ref: Option<(String, ProjectId)>,
) -> Vec<AnyView> {
    let RootTree { root, status } = root_tree;
    let unavailable = match status {
        ProjectRootStatus::Available => None,
        ProjectRootStatus::Unavailable { message } => Some(message),
    };
    let mut views = Vec::new();
    let root_label = root_display_name(&root);
    let root_title = root.0.clone();
    let root_key = root.0.clone();
    let is_expanded = {
        let root_key = root_key.clone();
        Signal::derive(move || !collapsed_roots.with(|roots| roots.contains(&root_key)))
    };
    let remove_button = project_ref.map(|(host_id, project_id)| {
        let state = expect_context::<AppState>();
        let host_for_manage = host_id.clone();
        let project_for_manage = project_id.clone();
        // Catalog edits must not remount directory views.
        let can_manage = Memo::new(move |_| {
            state.can_manage_project_roots(&host_for_manage, &project_for_manage)
        });
        let root_for_remove = root.clone();
        let on_remove = move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            let state = expect_context::<AppState>();
            let host_id = host_id.clone();
            let project_id = project_id.clone();
            let root_for_remove = root_for_remove.clone();
            spawn_local(async move {
                let message = format!("Remove root \"{}\" from this project?", root_for_remove.0);
                if !crate::bridge::confirm_dialog("Remove root", &message).await {
                    return;
                }
                delete_project_root(&state, host_id, project_id, root_for_remove);
            });
        };
        view! {
            <Show when=move || can_manage.get()>
                <button class="fe-root-remove" title="Remove root" on:click=on_remove.clone()>
                    "×"
                </button>
            </Show>
        }
    });
    views.push(
        view! {
            <div
                class="fe-root-header"
                class:fe-root-header-unavailable=unavailable.is_some()
                title=root_title
            >
                <button
                    class="fe-root-toggle"
                    data-test="fe-root-toggle"
                    aria-expanded=move || if is_expanded.get() { "true" } else { "false" }
                    on:click={
                        let root_key = root_key.clone();
                        move |_| {
                            collapsed_roots.update(|roots| {
                                if !roots.insert(root_key.clone()) {
                                    roots.remove(&root_key);
                                }
                            });
                        }
                    }
                >
                    <span class="fe-chevron">
                        {move || if is_expanded.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </span>
                    <span class="fe-root-name">{root_label}</span>
                </button>
                {remove_button}
            </div>
        }
        .into_any(),
    );
    let available = unavailable.is_none();
    if let Some(message) = unavailable {
        views.push(
            view! { <div class="fe-root-unavailable" role="alert">{message}</div> }.into_any(),
        );
    }
    let filter = filter.to_owned();
    views.push(view! {
        <Show when=move || available && is_expanded.get()>
            <DirectoryContents root=root.clone() path=String::new() depth=0 filter=filter.clone() show_hidden=show_hidden expanded_dirs=expanded_dirs windows=windows />
        </Show>
    }.into_any());
    views
}

#[component]
fn DirectoryContents(
    root: ProjectRootPath,
    path: String,
    depth: usize,
    filter: String,
    show_hidden: bool,
    expanded_dirs: RwSignal<HashSet<ProjectPath>>,
    windows: RwSignal<HashMap<ProjectPath, ProjectFileWindow>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let project = state.active_project.get_untracked();
    let directory = ProjectPath {
        root,
        relative_path: path,
    };
    let directory_for_view = directory.clone();
    let child_filter = filter.clone();
    let view = Memo::new(move |_| ProjectFileView::Directory {
        path: directory_for_view.clone(),
        window: windows
            .with(|windows| windows.get(&directory_for_view).cloned())
            .unwrap_or(ProjectFileWindow::First),
        show_hidden,
        filter: filter.clone(),
    });
    let page = Memo::new(move |_| {
        let project = project.as_ref()?;
        let desired = view.get();
        state.file_views.with(|map| {
            map.get(project)?
                .iter()
                .find(|page| page.view == desired)
                .map(|page| page.state.clone())
        })
    });
    view! {
        <div class="fe-directory-contents">
            <For
                each=move || match page.get() {
                    Some(ProjectFileViewState::Ready { entries, .. }) => {
                        let _diagnostic = crate::perf::UiSpan::new(crate::perf::UiOperation::FileTreeBuild, 0, entries.len());
                        entries
                    }
                    _ => Vec::new(),
                }
                key=|entry| (entry.path.clone(), entry.kind == ProjectFileKind::Directory)
                children=move |entry| view! { <DirectoryEntry entry=entry depth=depth filter=child_filter.clone() show_hidden=show_hidden expanded_dirs=expanded_dirs windows=windows /> }
            />
            {move || match page.get() {
                None | Some(ProjectFileViewState::Loading) => view! { <div role="status">"Loading…"</div> }.into_any(),
                Some(ProjectFileViewState::Closed) => ().into_any(),
                Some(ProjectFileViewState::DirectoryMissing) => view! { <div role="alert">"Directory no longer exists"</div> }.into_any(),
                Some(ProjectFileViewState::Unavailable { message }) => view! { <div class="fe-root-unavailable" role="alert">{message}</div> }.into_any(),
                Some(ProjectFileViewState::Ready { entries, previous, next }) => view! {
                    {entries.is_empty().then(|| view! { <div class="panel-empty">"No entries"</div> })}
                    <div class="fe-pages">
                        {previous.map(|cursor| { let directory = directory.clone(); view! { <button on:click=move |_| windows.update(|windows| { windows.insert(directory.clone(), ProjectFileWindow::Before(cursor.clone())); })>"Previous"</button> } })}
                        {next.map(|cursor| { let directory = directory.clone(); view! { <button on:click=move |_| windows.update(|windows| { windows.insert(directory.clone(), ProjectFileWindow::After(cursor.clone())); })>"Next"</button> } })}
                    </div>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn DirectoryEntry(
    entry: protocol::ProjectFileCursor,
    depth: usize,
    filter: String,
    show_hidden: bool,
    expanded_dirs: RwSignal<HashSet<ProjectPath>>,
    windows: RwSignal<HashMap<ProjectPath, ProjectFileWindow>>,
) -> AnyView {
    let root = entry.path.root;
    let dir_path = entry.path.relative_path;
    let name = dir_path.rsplit('/').next().unwrap_or(&dir_path).to_owned();
    if entry.kind != ProjectFileKind::Directory {
        return view! { <FileRow root=root relative_path=dir_path name=name indent=depth*16 /> }
            .into_any();
    }
    let state = expect_context::<AppState>();
    let key = ProjectPath {
        root: root.clone(),
        relative_path: dir_path.clone(),
    };
    let key_for_click = key.clone();
    let expanded = Signal::derive(move || expanded_dirs.with(|set| set.contains(&key)));
    let root_for_context = root.clone();
    let path_for_context = dir_path.clone();
    view! {
        <div class="fe-dir-group">
            <button class="fe-item fe-dir" style=format!("padding-left: {}px", depth * 16 + 4)
                aria-expanded=move || expanded.get().to_string()
                on:contextmenu=move |ev: web_sys::MouseEvent| {
                    ev.prevent_default();
                    crate::actions::search_in_folder(&state, root_for_context.clone(), path_for_context.clone());
                }
                on:click=move |_| {
                    expanded_dirs.update(|set| {
                        if !set.insert(key_for_click.clone()) {
                            let prefix = format!("{}/", key_for_click.relative_path);
                            set.retain(|path| path.root != key_for_click.root || (path != &key_for_click && !path.relative_path.starts_with(&prefix)));
                        }
                    });
                    windows.update(|windows| windows.retain(|path, _| expanded_dirs.with_untracked(|expanded| expanded.contains(path)) || path.relative_path.is_empty()));
                }>
                <span class="fe-chevron">{move || if expanded.get() { "▾" } else { "▸" }}</span>
                <span class="fe-name">{name}</span>
            </button>
            <Show when=move || expanded.get()>
                <DirectoryContents root=root.clone() path=dir_path.clone() depth=depth+1 filter=filter.clone() show_hidden=show_hidden expanded_dirs=expanded_dirs windows=windows />
            </Show>
        </div>
    }.into_any()
}

/// One file row: clicking it opens the file in the focused pane. Splits are
/// created by dragging tabs, so the row offers no open-to-the-side affordance.
#[component]
fn FileRow(
    root: ProjectRootPath,
    relative_path: String,
    name: String,
    indent: usize,
) -> impl IntoView {
    let path = ProjectPath {
        root,
        relative_path,
    };

    let open_state = expect_context::<AppState>();
    let on_click = move |_| {
        open_project_path_at(&open_state, path.clone(), OpenTarget::Focused);
    };

    let icon = file_type_icon(&name);

    view! {
        <div class="fe-row">
            <button
                class="fe-item fe-file"
                style=format!("padding-left: {}px", indent + 4)
                on:click=on_click
            >
                <span class="fe-icon fe-file-icon">{icon}</span>
                <span class="fe-name">{name}</span>
            </button>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::ActiveProjectRef;
    use crate::wasm_test_support::{Mounted, ScopedRoot};
    use leptos::mount::mount_to;
    use protocol::{
        CodeIntelLanguageId, CodeIntelOverviewHeadline, CodeIntelOverviewPayload,
        CodeIntelProviderId, CodeIntelProviderStatus, CodeIntelResourceMode, CodeIntelRootOverview,
        CodeIntelState, FrameKind, ProjectFileEntry, StreamPath,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    struct FileViewBridge(wasm_bindgen::JsValue);
    impl FileViewBridge {
        fn install() -> Self {
            let previous =
                js_sys::Reflect::get(&web_sys::window().unwrap(), &"__TAURI__".into()).unwrap();
            js_sys::eval("window.__fileViewCommands = []; window.__TAURI__ = {core: {invoke: (cmd, args) => { if (cmd === 'send_host_line') window.__fileViewCommands.push(JSON.parse(args.line)); return Promise.resolve(null); }}};").unwrap();
            Self(previous)
        }
    }
    impl Drop for FileViewBridge {
        fn drop(&mut self) {
            js_sys::Reflect::set(&web_sys::window().unwrap(), &"__TAURI__".into(), &self.0)
                .unwrap();
        }
    }

    fn make_container() -> ScopedRoot {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container.set_attribute("data-test-container", "1").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        ScopedRoot::new(
            container.dyn_into::<HtmlElement>().unwrap(),
            "file-explorer",
        )
    }

    fn directory_page(
        path: &str,
        entries: Vec<ProjectFileEntry>,
    ) -> protocol::ProjectFileViewStatePayload {
        protocol::ProjectFileViewStatePayload {
            view: ProjectFileView::Directory {
                path: ProjectPath {
                    root: ProjectRootPath(ROOT.to_owned()),
                    relative_path: path.to_owned(),
                },
                window: ProjectFileWindow::First,
                show_hidden: false,
                filter: String::new(),
            },
            state: ProjectFileViewState::Ready {
                entries: entries
                    .into_iter()
                    .map(|entry| protocol::ProjectFileCursor {
                        path: ProjectPath {
                            root: ProjectRootPath(ROOT.to_owned()),
                            relative_path: entry.relative_path,
                        },
                        kind: entry.kind,
                        score: 0,
                    })
                    .collect(),
                previous: None,
                next: None,
            },
        }
    }

    fn publish_page(
        state: &AppState,
        host: &str,
        seq: u64,
        page: &protocol::ProjectFileViewStatePayload,
    ) {
        crate::dispatch::dispatch_envelope(
            state,
            host,
            protocol::Envelope::from_payload(
                StreamPath(format!("/project/{PROJECT}")),
                FrameKind::ProjectFileViewState,
                seq,
                page,
            )
            .unwrap(),
        );
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

    fn provider(
        id: &str,
        language: &str,
        state: CodeIntelState,
        progress: Option<(u32, u32)>,
    ) -> CodeIntelProviderStatus {
        CodeIntelProviderStatus {
            provider: CodeIntelProviderId(id.to_owned()),
            language: CodeIntelLanguageId(language.to_owned()),
            state,
            resource_mode: CodeIntelResourceMode::Full,
            work_done: progress.map(|(d, _)| d),
            total_work: progress.map(|(_, t)| t),
            message: None,
            error_count: 0,
            warning_count: 0,
        }
    }

    fn root(path: &str, providers: Vec<CodeIntelProviderStatus>) -> CodeIntelRootOverview {
        CodeIntelRootOverview {
            root: ProjectRootPath(path.to_owned()),
            providers,
        }
    }

    fn summary(
        headline: CodeIntelOverviewHeadline,
        counts: [u32; 5],
        message: Option<&str>,
    ) -> protocol::CodeIntelOverviewSummary {
        protocol::CodeIntelOverviewSummary {
            headline,
            ready: counts[0],
            indexing: counts[1],
            starting: counts[2],
            unavailable: counts[3],
            failed: counts[4],
            message: message.map(|m| m.to_owned()),
            error_count: 0,
            warning_count: 0,
        }
    }

    fn mount_footer(
        container: HtmlElement,
        overview: Option<CodeIntelOverviewPayload>,
    ) -> Mounted<Rc<RefCell<Option<AppState>>>> {
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "h1".to_owned(),
                project_id: ProjectId("proj-1".to_owned()),
            }));
            if let Some(overview) = overview.clone() {
                state.code_intel_overview.update(|m| {
                    m.insert(
                        ActiveProjectRef {
                            host_id: "h1".to_owned(),
                            project_id: ProjectId("proj-1".to_owned()),
                        },
                        overview,
                    );
                });
            }
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <FileExplorer /> }
        });
        Mounted::new(handle, holder)
    }

    // ── Open ────────────────────────────────────────────────────────────

    const HOST: &str = "h1";
    const PROJECT: &str = "proj-1";
    const ROOT: &str = "/repo";
    /// A Directory-kind entry every fixture listing carries, so "is this a file
    /// row?" is answered from the protocol's `kind` and not from the name.
    const FIXTURE_DIR: &str = "src";

    fn active_project() -> ActiveProjectRef {
        ActiveProjectRef {
            host_id: HOST.to_owned(),
            project_id: ProjectId(PROJECT.to_owned()),
        }
    }

    fn file_key(name: &str) -> crate::state::FileResourceKey {
        crate::state::FileResourceKey {
            host_id: HOST.to_owned(),
            project_id: ProjectId(PROJECT.to_owned()),
            path: ProjectPath {
                root: ProjectRootPath(ROOT.to_owned()),
                relative_path: name.to_owned(),
            },
        }
    }

    /// An explorer whose active project lists `files`, each already loaded.
    ///
    /// Pre-loading matters: it is the state in which opening a file resolves
    /// synchronously, so the test observes the destination pane directly instead
    /// of an unresolvable cold-open round trip.
    fn mount_explorer_with_files(container: HtmlElement, files: &[&str]) -> Mounted<AppState> {
        // The shape the server actually emits: one entry per path, `Add` for a
        // path present in the listing (server/src/project_stream.rs). The
        // directory rides along so the tree is exercised against the protocol's
        // typed `kind` rather than a listing of files only.
        let mut entries: Vec<ProjectFileEntry> = files
            .iter()
            .map(|name| ProjectFileEntry {
                relative_path: (*name).to_owned(),
                kind: ProjectFileKind::File,
                op: protocol::FileEntryOp::Add,
            })
            .collect();
        entries.push(ProjectFileEntry {
            relative_path: FIXTURE_DIR.to_owned(),
            kind: ProjectFileKind::Directory,
            op: protocol::FileEntryOp::Add,
        });
        // Owned before the boundary: `mount_to`'s closure is `'static`, so nothing
        // borrowed from the caller's slice may cross into it. `entries` above is
        // already owned; this was the one value that still pointed back at
        // `files` (E0521).
        let loaded: Vec<String> = files.iter().map(|name| (*name).to_owned()).collect();

        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.active_project.set(Some(active_project()));
            state.host_streams.update(|streams| {
                streams.insert(HOST.to_owned(), StreamPath("/host/1".to_owned()));
            });
            state.file_tree.update(|tree| {
                tree.insert(
                    ProjectId(PROJECT.to_owned()),
                    vec![protocol::ProjectRootListing {
                        root: ProjectRootPath(ROOT.to_owned()),
                        status: protocol::ProjectRootStatus::Available,
                        entries: entries.clone(),
                        partial_dirs: Vec::new(),
                    }],
                );
            });
            state.file_views.update(|views| {
                views.insert(active_project(), vec![directory_page("", entries.clone())]);
            });
            for name in &loaded {
                let key = file_key(name);
                state.open_files.update(|files| {
                    files.insert(
                        key.clone(),
                        crate::state::OpenFile {
                            path: key.path.clone(),
                            version: protocol::ProjectFileVersion(1),
                            contents: Some("contents".to_owned()),
                            is_binary: false,
                            missing: false,
                        },
                    );
                });
            }
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <FileExplorer /> }
        });
        let state = holder.borrow().clone().expect("state provided");
        Mounted::new(handle, state)
    }

    fn file_rows(container: &HtmlElement) -> Vec<HtmlElement> {
        let nodes = container.query_selector_all("button.fe-file").unwrap();
        (0..nodes.length())
            .map(|i| nodes.item(i).unwrap().dyn_into::<HtmlElement>().unwrap())
            .collect()
    }

    /// Inject the production stylesheet once, so geometry and visibility
    /// assertions reflect real styling rather than an unstyled DOM (where every
    /// element is visible and every box is zero).
    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-explorer")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-explorer");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    /// Where a file ended up: which pane holds a tab for it.
    fn pane_of_file(state: &AppState, name: &str) -> Option<crate::state::PaneId> {
        let content = crate::state::TabContent::File {
            key: file_key(name),
        };
        state.center_zone.with_untracked(|center_zone| {
            center_zone
                .occurrences(&content)
                .first()
                .map(|(pane, _)| *pane)
        })
    }

    /// An ordinary click opens in the focused pane and never splits.
    #[wasm_bindgen_test]
    async fn clicking_a_file_row_opens_it_in_the_focused_pane() {
        let document = web_sys::window().unwrap().document().unwrap();
        let peer = document.create_element("div").unwrap();
        peer.set_attribute("data-test-container", "1").unwrap();
        document.body().unwrap().append_child(&peer).unwrap();
        let peer = ScopedRoot::new(peer.dyn_into::<HtmlElement>().unwrap(), "explorer-peer");
        let container = make_container();
        console_log!(
            "Explorer fixture creation: peer_connected={}, current_connected={}",
            peer.is_connected(),
            container.is_connected()
        );
        assert!(
            peer.is_connected(),
            "a fixture must not delete another owner's root"
        );
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        let rows = file_rows(&container);
        assert_eq!(rows.len(), 1, "the active project's file is listed");
        assert_eq!(
            container
                .query_selector_all("button.fe-dir")
                .unwrap()
                .length(),
            1,
            "and the Directory-kind entry renders as a directory, not a file row: \
             the protocol's `kind` decides, not the path"
        );
        assert!(
            rows[0].get_attribute("draggable").is_none(),
            "explorer rows are not drag sources — no drag-copy affordance exists"
        );

        rows[0].click();
        next_tick().await;

        assert_eq!(
            pane_of_file(&state, "main.rs"),
            Some(crate::state::PaneId::Primary),
            "an ordinary open lands in the focused pane"
        );
        assert!(
            !state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "an ordinary open never creates a split"
        );

        // The retained gate counted this abandoned parent in CenterZone, whose
        // panic then left a fixed overlay covering every diff pointer fixture.
        let previous_root = (*container).clone();
        drop(state);
        drop(container);
        console_log!(
            "Explorer fixture teardown: previous_connected={}, previous_children={}, peer_connected={}",
            previous_root.is_connected(),
            previous_root.has_child_nodes(),
            peer.is_connected()
        );
        assert!(
            !previous_root.is_connected(),
            "the fixture removes its own parent"
        );
        assert!(
            !previous_root.has_child_nodes(),
            "the mounted view is disposed"
        );
        assert!(
            peer.is_connected(),
            "teardown must preserve another owner's root"
        );
    }

    #[wasm_bindgen_test]
    async fn project_roots_collapse_and_expand_their_files() {
        let container = make_container();
        let _state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        let root = container
            .query_selector("button[data-test='fe-root-toggle']")
            .unwrap()
            .expect("the project root renders as a tree toggle")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(root.get_attribute("aria-expanded").as_deref(), Some("true"));
        assert_eq!(
            file_rows(&container).len(),
            1,
            "expanded roots show their files"
        );

        root.click();
        next_tick().await;
        assert_eq!(
            root.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert!(
            file_rows(&container).is_empty(),
            "collapsing a root hides only the tree contained by that root"
        );

        root.click();
        next_tick().await;
        assert_eq!(root.get_attribute("aria-expanded").as_deref(), Some("true"));
        assert_eq!(
            file_rows(&container).len(),
            1,
            "expanding restores its files"
        );
    }

    /// A file row is a single dense line, exactly like a directory row. It is a
    /// plain open-on-click row: splits are created by dragging tabs, so the row
    /// carries no side-open action, and it must not render any taller than a
    /// directory row.
    ///
    /// Regression guard: the base `.fe-item` rule carries `width: 100%`, which
    /// inside the wrapping `.fe-row` resolved as a full-width flex-basis. When
    /// the row still carried a trailing action that pushed the action onto its
    /// own line and doubled the row height; the row now has no such action, and
    /// this keeps it a single dense line.
    #[wasm_bindgen_test]
    async fn file_rows_are_single_line_and_match_directory_density() {
        ensure_styles_loaded();
        let container = make_container();
        let _state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        let row = container
            .query_selector(".fe-row")
            .unwrap()
            .expect("the file renders as a row")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let dir = container
            .query_selector("button.fe-dir")
            .unwrap()
            .expect("the fixture directory renders a directory row")
            .dyn_into::<HtmlElement>()
            .unwrap();

        // Splits are drag-only now: the row carries no side-open control.
        assert!(
            container
                .query_selector("button.fe-open-side")
                .unwrap()
                .is_none(),
            "the file row must not render an open-to-the-side action"
        );

        // A file row is no taller than a directory row — one dense line.
        let row_h = row.get_bounding_client_rect().height();
        let dir_h = dir.get_bounding_client_rect().height();
        assert!(
            row_h <= dir_h + 1.0,
            "a file row must be a single dense line like a directory row: \
             file row {row_h}px vs directory row {dir_h}px"
        );
    }

    /// The context menus animate via `menu-in` on `.context-menu`. Under
    /// reduced motion that animation must be off — and the rule that turns it
    /// off must actually win, which an earlier block in the file does not.
    #[wasm_bindgen_test]
    async fn the_reduced_motion_rule_for_menus_is_not_overridden() {
        // The later swarm media block correctly affects only its spinner, not
        // menus. Selecting the last media block rejected valid CSS. Activate
        // the browser-parsed production media rules in an isolated document so
        // the computed cascade, including source order, is the assertion.
        let container = make_container();
        let mount_root = js_sys::Function::new_with_args(
            "container",
            r#"
                const frame = document.createElement('iframe');
                container.appendChild(frame);
                // Adoption retains the outer realm's HTMLElement prototype.
                const root = document.createElement('div');
                frame.contentDocument.body.appendChild(root);
                return root;
            "#,
        )
        .call1(&wasm_bindgen::JsValue::NULL, container.as_ref())
        .expect("create an isolated menu document")
        .dyn_into::<HtmlElement>()
        .expect("menu mount root");
        let _handle = mount_to(mount_root.clone(), move || {
            use crate::components::card_menu::CardContextMenu;
            view! {
                <CardContextMenu
                    menu=RwSignal::new(Some((8.0, 8.0)))
                    label="File actions"
                    anchor=NodeRef::new()
                >
                    <button class="context-menu-item" role="menuitem">"Open file"</button>
                </CardContextMenu>
            }
        });
        next_tick().await;
        let probe = js_sys::Function::new_with_args(
            "root, css",
            r#"
                const doc = root.ownerDocument;
                const frame = doc.defaultView.frameElement;
                try {
                    const style = doc.createElement('style');
                    style.textContent = css;
                    doc.head.appendChild(style);
                    const menu = root.querySelector('[role="menu"]');
                    if (!menu) throw new Error('Production menu did not render');
                    const reducedRules = Array.from(style.sheet.cssRules).filter(
                        rule => rule.conditionText === '(prefers-reduced-motion: reduce)'
                    );
                    const setReduced = enabled => {
                        for (const rule of reducedRules) {
                            rule.media.mediaText = enabled ? 'all' : 'not all';
                        }
                        return frame.contentWindow.getComputedStyle(menu).animationName;
                    };
                    return [setReduced(false), setReduced(true), setReduced(false), reducedRules.length];
                } finally {
                    frame.remove();
                }
            "#,
        );
        let observed = probe
            .call2(
                &wasm_bindgen::JsValue::NULL,
                mount_root.as_ref(),
                &wasm_bindgen::JsValue::from_str(PROD_STYLES),
            )
            .expect("render the production menu stylesheet")
            .dyn_into::<js_sys::Array>()
            .expect("computed menu animation states");
        let normal = observed.get(0).as_string().unwrap();
        let reduced = observed.get(1).as_string().unwrap();
        let restored = observed.get(2).as_string().unwrap();
        let media_rules = observed.get(3).as_f64().unwrap();
        console_log!(
            "menu motion cascade: media_rules={}, normal={}, reduced={}, restored={}",
            media_rules,
            normal,
            reduced,
            restored
        );
        assert!(media_rules > 0.0, "production honors reduced motion");
        assert_eq!(normal, "menu-in", "ordinary menus retain their animation");
        assert_eq!(
            reduced, "none",
            "the production reduced-motion rule must win the actual menu cascade"
        );
        assert_eq!(
            restored, "menu-in",
            "only reduced motion disables animation"
        );
    }

    /// The explorer lists — and opens — the active project only.
    #[wasm_bindgen_test]
    async fn without_an_active_project_there_are_no_rows_to_open() {
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        state.active_project.set(None);
        next_tick().await;

        assert!(
            file_rows(&container).is_empty(),
            "no active project means no file rows, so there is nothing to open \
             to the side from another project"
        );
    }

    fn label_text(container: &HtmlElement) -> String {
        container
            .query_selector("[data-test=\"fe-codeintel-label\"]")
            .unwrap()
            .expect("code-intel label present")
            .text_content()
            .unwrap_or_default()
    }

    /// No overview received yet ⇒ neutral "Loading…" rather than a guessed state.
    #[wasm_bindgen_test]
    async fn footer_shows_loading_without_overview() {
        let container = make_container();
        let _mounted = mount_footer(container.clone(), None);
        next_tick().await;
        assert_eq!(label_text(&container), "Code Intel: Loading…");
    }

    /// Lazy-v1 idle: server-authored `NotStarted` headline renders as
    /// "Not started" with the server message shown verbatim.
    #[wasm_bindgen_test]
    async fn footer_shows_not_started_with_server_message() {
        let container = make_container();
        let overview = CodeIntelOverviewPayload {
            roots: vec![root("/repo", vec![])],
            summary: summary(
                CodeIntelOverviewHeadline::NotStarted,
                [0, 0, 0, 0, 0],
                Some("No language server running — open a file to index"),
            ),
        };
        let _mounted = mount_footer(container.clone(), Some(overview));
        next_tick().await;

        assert_eq!(label_text(&container), "Code Intel: Not started");
        let message = container
            .query_selector("[data-test=\"fe-codeintel-message\"]")
            .unwrap()
            .expect("server message present")
            .text_content()
            .unwrap_or_default();
        assert!(
            message.contains("open a file to index"),
            "server message must render verbatim; got: {message}"
        );
    }

    /// Multi-root indexing collapses to an "Indexing · N of M servers" aggregate,
    /// and the collapsed view does not list per-root provider rows.
    #[wasm_bindgen_test]
    async fn footer_indexing_aggregate_collapsed() {
        let container = make_container();
        let overview = CodeIntelOverviewPayload {
            roots: vec![
                root(
                    "/repo/api",
                    vec![provider(
                        "rust-analyzer",
                        "rust",
                        CodeIntelState::Indexing,
                        Some((30, 100)),
                    )],
                ),
                root(
                    "/repo/web",
                    vec![provider("pyright", "python", CodeIntelState::Ready, None)],
                ),
                root(
                    "/repo/tools",
                    vec![provider(
                        "tsserver",
                        "typescript",
                        CodeIntelState::Starting,
                        None,
                    )],
                ),
            ],
            summary: summary(
                CodeIntelOverviewHeadline::Indexing,
                [1, 1, 1, 0, 0],
                Some("Indexing code intelligence"),
            ),
        };
        let _mounted = mount_footer(container.clone(), Some(overview));
        next_tick().await;

        assert_eq!(
            label_text(&container),
            "Code Intel: Indexing · 1 of 3 servers"
        );
        // Collapsed: no per-root or provider detail visible yet.
        assert_eq!(
            container
                .query_selector_all("[data-test=\"fe-codeintel-root\"]")
                .unwrap()
                .length(),
            0,
            "collapsed footer must not render per-root detail"
        );
    }

    /// B2: the footer surfaces the server-owned project-wide error/warning
    /// totals next to the state, so "Ready" with compile errors reads as
    /// "Ready · 2 errors · 1 warning" instead of a clean bill of health.
    #[wasm_bindgen_test]
    async fn footer_shows_project_error_and_warning_counts() {
        let container = make_container();
        let mut with_counts = summary(CodeIntelOverviewHeadline::Ready, [1, 0, 0, 0, 0], None);
        with_counts.error_count = 2;
        with_counts.warning_count = 1;
        let overview = CodeIntelOverviewPayload {
            roots: vec![root(
                "/repo",
                vec![provider(
                    "rust-analyzer",
                    "rust",
                    CodeIntelState::Ready,
                    None,
                )],
            )],
            summary: with_counts,
        };
        let _mounted = mount_footer(container.clone(), Some(overview));
        next_tick().await;

        assert_eq!(
            label_text(&container),
            "Code Intel: Ready · 2 errors · 1 warning"
        );
    }

    /// Expanding the footer reveals one section per root and a row per provider,
    /// including indexing progress.
    #[wasm_bindgen_test]
    async fn footer_expands_to_per_root_providers() {
        let container = make_container();
        let overview = CodeIntelOverviewPayload {
            roots: vec![
                root(
                    "/repo/api",
                    vec![provider(
                        "rust-analyzer",
                        "rust",
                        CodeIntelState::Indexing,
                        Some((30, 100)),
                    )],
                ),
                root(
                    "/repo/web",
                    vec![provider("pyright", "python", CodeIntelState::Ready, None)],
                ),
            ],
            summary: summary(CodeIntelOverviewHeadline::Indexing, [1, 1, 0, 0, 0], None),
        };
        let _mounted = mount_footer(container.clone(), Some(overview));
        next_tick().await;

        let toggle = container
            .query_selector("[data-test=\"fe-codeintel-footer\"] button")
            .unwrap()
            .expect("summary toggle present")
            .dyn_into::<HtmlElement>()
            .unwrap();
        toggle.click();
        next_tick().await;

        assert_eq!(
            container
                .query_selector_all("[data-test=\"fe-codeintel-root\"]")
                .unwrap()
                .length(),
            2,
            "expanded footer must render one section per root"
        );
        let providers = container
            .query_selector_all("[data-test=\"fe-codeintel-provider\"]")
            .unwrap();
        assert_eq!(providers.length(), 2, "one row per provider");
        let mut found_progress = false;
        for i in 0..providers.length() {
            let text = providers
                .item(i)
                .unwrap()
                .text_content()
                .unwrap_or_default();
            if text.contains("30/100") {
                found_progress = true;
            }
        }
        assert!(
            found_progress,
            "indexing provider row must show work progress"
        );
    }

    /// The footer is a pure projection of the `code_intel_overview` signal:
    /// mutating it after mount must rerender live, with no remount and without
    /// reopening the panel.
    #[wasm_bindgen_test]
    async fn footer_rerenders_live_on_overview_update() {
        let container = make_container();
        let initial = CodeIntelOverviewPayload {
            roots: vec![root("/repo", vec![])],
            summary: summary(
                CodeIntelOverviewHeadline::NotStarted,
                [0, 0, 0, 0, 0],
                Some("No language server running — open a file to index"),
            ),
        };
        let holder = mount_footer(container.clone(), Some(initial));
        next_tick().await;
        assert_eq!(label_text(&container), "Code Intel: Not started");

        // Drive a new server-authored overview through the signal, exactly as the
        // dispatcher would on a fresh `code_intel_overview` frame.
        let state = holder.borrow().clone().expect("state captured at mount");
        state.code_intel_overview.update(|m| {
            m.insert(
                ActiveProjectRef {
                    host_id: "h1".to_owned(),
                    project_id: ProjectId("proj-1".to_owned()),
                },
                CodeIntelOverviewPayload {
                    roots: vec![root(
                        "/repo",
                        vec![provider(
                            "rust-analyzer",
                            "rust",
                            CodeIntelState::Ready,
                            None,
                        )],
                    )],
                    summary: summary(
                        CodeIntelOverviewHeadline::Ready,
                        [1, 0, 0, 0, 0],
                        Some("Code intelligence ready"),
                    ),
                },
            );
        });
        next_tick().await;

        assert_eq!(label_text(&container), "Code Intel: Ready");
        let message = container
            .query_selector("[data-test=\"fe-codeintel-message\"]")
            .unwrap()
            .expect("updated server message present")
            .text_content()
            .unwrap_or_default();
        assert!(
            message.contains("Code intelligence ready"),
            "footer message must reflect the updated overview; got: {message}"
        );
    }

    /// The overview is keyed by (host_id, project_id): an overview for the same
    /// project id on a *different* host must not render under the active project.
    /// Only the owning host's overview shows.
    #[wasm_bindgen_test]
    async fn footer_overview_is_scoped_to_owning_host() {
        let container = make_container();
        // Active project is (h1, proj-1); mount with no overview yet.
        let holder = mount_footer(container.clone(), None);
        next_tick().await;
        assert_eq!(label_text(&container), "Code Intel: Loading…");

        let state = holder.borrow().clone().expect("state captured at mount");
        // Wrong-host overview for the same project id must be ignored.
        state.code_intel_overview.update(|m| {
            m.insert(
                ActiveProjectRef {
                    host_id: "other-host".to_owned(),
                    project_id: ProjectId("proj-1".to_owned()),
                },
                CodeIntelOverviewPayload {
                    roots: vec![root(
                        "/repo",
                        vec![provider(
                            "rust-analyzer",
                            "rust",
                            CodeIntelState::Ready,
                            None,
                        )],
                    )],
                    summary: summary(CodeIntelOverviewHeadline::Ready, [1, 0, 0, 0, 0], None),
                },
            );
        });
        next_tick().await;
        assert_eq!(
            label_text(&container),
            "Code Intel: Loading…",
            "an overview from another host must not render under the active project"
        );

        // Owning-host overview renders.
        state.code_intel_overview.update(|m| {
            m.insert(
                ActiveProjectRef {
                    host_id: "h1".to_owned(),
                    project_id: ProjectId("proj-1".to_owned()),
                },
                CodeIntelOverviewPayload {
                    roots: vec![root("/repo", vec![])],
                    summary: summary(
                        CodeIntelOverviewHeadline::NotStarted,
                        [0, 0, 0, 0, 0],
                        Some("No language server running — open a file to index"),
                    ),
                },
            );
        });
        next_tick().await;
        assert_eq!(label_text(&container), "Code Intel: Not started");
    }

    /// A root the server cannot read renders its reason and a visible remove
    /// control, while the project's readable roots still list their files.
    #[wasm_bindgen_test]
    async fn unavailable_root_shows_reason_and_remove_control_beside_live_roots() {
        // Priming leaves this host connected; keep it off the shared test host.
        let _bridge = FileViewBridge::install();
        const HOST: &str = "fe-unavailable-root";
        const MISSING: &str = "/work/moved-away";
        const MESSAGE: &str = "Failed to stat project root '/work/moved-away': No such file or directory (os error 2)";
        ensure_styles_loaded();
        let container = make_container();
        let holder: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let holder_for_mount = holder.clone();
        let _mounted = Mounted::new(
            mount_to(container.clone(), move || {
                let state = AppState::new();
                crate::dispatch::prime_host_for_tests(&state, HOST);
                state.active_project.set(Some(ActiveProjectRef {
                    host_id: HOST.to_owned(),
                    project_id: ProjectId(PROJECT.to_owned()),
                }));
                *holder_for_mount.borrow_mut() = Some(state.clone());
                provide_context(state);
                view! { <FileExplorer /> }
            }),
            (),
        );
        let state = holder.borrow().clone().expect("state provided");
        let bootstrap = protocol::ProjectBootstrapPayload {
            project: protocol::Project {
                id: ProjectId(PROJECT.to_owned()),
                name: "Two roots".to_owned(),
                sort_order: 0,
                source: protocol::ProjectSource::Standalone {
                    roots: vec![
                        ProjectRootPath(ROOT.to_owned()),
                        ProjectRootPath(MISSING.to_owned()),
                    ],
                },
            },
            file_list: protocol::ProjectFileListPayload {
                incremental: false,
                roots: vec![
                    protocol::ProjectRootListing {
                        root: ProjectRootPath(ROOT.to_owned()),
                        status: ProjectRootStatus::Available,
                        entries: [
                            ("main.rs", ProjectFileKind::File),
                            ("big", ProjectFileKind::Directory),
                            ("big/a.rs", ProjectFileKind::File),
                        ]
                        .into_iter()
                        .map(|(path, kind)| ProjectFileEntry {
                            relative_path: path.to_owned(),
                            kind,
                            op: protocol::FileEntryOp::Add,
                        })
                        .collect(),
                        partial_dirs: Vec::new(),
                    },
                    protocol::ProjectRootListing {
                        root: ProjectRootPath(MISSING.to_owned()),
                        status: ProjectRootStatus::Unavailable {
                            message: MESSAGE.to_owned(),
                        },
                        entries: Vec::new(),
                        partial_dirs: Vec::new(),
                    },
                ],
            },
            git_status: protocol::ProjectGitStatusPayload { roots: Vec::new() },
            review_summaries: Vec::new(),
        };
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                StreamPath(format!("/project/{PROJECT}")),
                FrameKind::ProjectBootstrap,
                0,
                &bootstrap,
            )
            .unwrap(),
        );
        next_tick().await;

        let root_page = directory_page(
            "",
            bootstrap.file_list.roots[0]
                .entries
                .iter()
                .filter(|entry| !entry.relative_path.contains('/'))
                .cloned()
                .collect(),
        );
        publish_page(&state, HOST, 1, &root_page);
        next_tick().await;
        let rows = file_rows(&container);
        assert_eq!(rows.len(), 1, "the readable root still lists its file");
        let row_text = rows[0].text_content().unwrap_or_default();
        assert!(
            row_text.trim().ends_with("main.rs"),
            "the readable root's row names its file: {row_text:?}"
        );
        let alerts = container.query_selector_all("[role='alert']").unwrap();
        assert_eq!(
            alerts.length(),
            1,
            "only the unreadable root reports a problem"
        );
        assert_eq!(
            alerts.item(0).unwrap().text_content().as_deref(),
            Some(MESSAGE)
        );

        let missing_remove = container
            .query_selector(&format!("[title='{MISSING}'] button[title='Remove root']"))
            .unwrap()
            .expect("the unreadable root keeps its remove control")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let opacity = web_sys::window()
            .unwrap()
            .get_computed_style(&missing_remove)
            .unwrap()
            .unwrap()
            .get_property_value("opacity")
            .unwrap();
        assert_eq!(
            opacity, "1",
            "the remove control is visible without hovering"
        );
        assert!(missing_remove.get_bounding_client_rect().width() > 0.0);

        let explorer_text = || container.text_content().unwrap_or_default();
        container
            .query_selector("button.fe-dir")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            explorer_text().contains("Loading…"),
            "an expanded folder is loading, not silently empty"
        );
        let mut page = directory_page(
            "big",
            vec![ProjectFileEntry {
                relative_path: "big/a.rs".to_owned(),
                kind: ProjectFileKind::File,
                op: protocol::FileEntryOp::Add,
            }],
        );
        let cursor = protocol::ProjectFileCursor {
            path: ProjectPath {
                root: ProjectRootPath(ROOT.to_owned()),
                relative_path: "big/a.rs".to_owned(),
            },
            kind: ProjectFileKind::File,
            score: 0,
        };
        if let ProjectFileViewState::Ready { next, .. } = &mut page.state {
            *next = Some(cursor.clone());
        }
        publish_page(&state, HOST, 2, &page);
        next_tick().await;
        assert_eq!(file_rows(&container).len(), 2);
        let before_unrelated = js_sys::eval("window.__fileViewCommands.length")
            .unwrap()
            .as_f64()
            .unwrap();
        let mut unrelated = bootstrap.clone();
        unrelated.project.id = ProjectId("unrelated-file-view".to_owned());
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                StreamPath("/project/unrelated-file-view".to_owned()),
                FrameKind::ProjectBootstrap,
                0,
                &unrelated,
            )
            .unwrap(),
        );
        next_tick().await;
        assert_eq!(
            file_rows(&container).len(),
            2,
            "other projects cannot clear visible expanded children"
        );
        assert_eq!(
            js_sys::eval("window.__fileViewCommands.length")
                .unwrap()
                .as_f64()
                .unwrap(),
            before_unrelated,
            "another project's bootstrap must not re-declare active views"
        );
        let next = (0..container.query_selector_all("button").unwrap().length())
            .filter_map(|index| container.query_selector_all("button").unwrap().item(index))
            .find(|button| button.text_content().as_deref() == Some("Next"))
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        next.click();
        next_tick().await;
        assert!(explorer_text().contains("Loading…"));
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        assert!(
            commands
                .iter()
                .filter(|command| command.kind == FrameKind::ProjectFileView)
                .any(|command| {
                    command
                        .parse_payload::<protocol::ProjectFileViewPayload>()
                        .is_ok_and(|command| {
                            command.active
                                && matches!(
                                    command.view,
                                    ProjectFileView::Directory {
                                        window: ProjectFileWindow::After(_),
                                        ..
                                    }
                                )
                        })
                }),
            "Next declares a real server page, rather than hiding inaccessible files"
        );
        if let ProjectFileView::Directory { window, .. } = &mut page.view {
            *window = ProjectFileWindow::After(cursor.clone());
        }
        page.state = ProjectFileViewState::Unavailable {
            message: "Directory read failed".to_owned(),
        };
        publish_page(&state, HOST, 3, &page);
        next_tick().await;
        assert!(explorer_text().contains("Directory read failed"));
        assert!(!explorer_text().contains("No entries"));

        // Replaying bootstrap must re-declare both the root and the still-open
        // child, even though the server forgot all connection-owned views.
        let before = js_sys::eval("window.__fileViewCommands.length")
            .unwrap()
            .as_f64()
            .unwrap() as usize;
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                StreamPath(format!("/project/{PROJECT}")),
                FrameKind::ProjectBootstrap,
                4,
                &bootstrap,
            )
            .unwrap(),
        );
        next_tick().await;
        assert!(explorer_text().contains("Loading…"));
        publish_page(&state, HOST, 5, &root_page);
        next_tick().await;
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        assert!(commands[before..].iter().any(|command| command.kind == FrameKind::ProjectFileView && command.parse_payload::<protocol::ProjectFileViewPayload>().is_ok_and(|command| command.active && matches!(command.view, ProjectFileView::Directory { path, .. } if path.relative_path == "big"))), "expanded directory is re-declared on replay");
        let mut page = directory_page(
            "big",
            vec![ProjectFileEntry {
                relative_path: "big/restored.rs".to_owned(),
                kind: ProjectFileKind::File,
                op: protocol::FileEntryOp::Add,
            }],
        );
        if let ProjectFileView::Directory { window, .. } = &mut page.view {
            *window = ProjectFileWindow::After(cursor);
        }
        publish_page(&state, HOST, 6, &page);
        next_tick().await;
        assert!(explorer_text().contains("restored.rs"));
        let mut edge_entries: Vec<_> = (0..128)
            .map(|index| ProjectFileEntry {
                relative_path: format!("edge{index:03}"),
                kind: ProjectFileKind::Directory,
                op: protocol::FileEntryOp::Add,
            })
            .collect();
        publish_page(&state, HOST, 7, &directory_page("", edge_entries.clone()));
        next_tick().await;
        let edge_row = container
            .query_selector_all(".fe-dir")
            .unwrap()
            .item(127)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        edge_row.click();
        next_tick().await;
        publish_page(
            &state,
            HOST,
            8,
            &directory_page(
                "edge127",
                vec![ProjectFileEntry {
                    relative_path: "edge127/edge-child.rs".into(),
                    kind: ProjectFileKind::File,
                    op: protocol::FileEntryOp::Add,
                }],
            ),
        );
        next_tick().await;
        assert!(explorer_text().contains("edge-child.rs"));
        // The server's page includes its 128 selected rows plus the declared child.
        edge_entries.insert(
            0,
            ProjectFileEntry {
                relative_path: "a-new".into(),
                kind: ProjectFileKind::Directory,
                op: protocol::FileEntryOp::Add,
            },
        );
        publish_page(&state, HOST, 9, &directory_page("", edge_entries.clone()));
        next_tick().await;
        assert!(
            edge_row.is_connected(),
            "live page insertion must not replace the expanded row"
        );
        assert_eq!(
            edge_row.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        assert!(
            explorer_text().contains("edge-child.rs"),
            "expanded children remain visible at the page boundary"
        );
        let commands_before = js_sys::eval("window.__fileViewCommands.length")
            .unwrap()
            .as_f64()
            .unwrap() as usize;
        let mut failed_parent = directory_page("", Vec::new());
        failed_parent.state = ProjectFileViewState::Unavailable {
            message: "Temporary parent read failure".into(),
        };
        publish_page(&state, HOST, 10, &failed_parent);
        next_tick().await;
        assert!(explorer_text().contains("Temporary parent read failure"));
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        assert!(!commands[commands_before..].iter().any(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().is_ok_and(|command| !command.active && matches!(command.view, ProjectFileView::Directory {path, ..} if path.relative_path == "edge127"))), "a temporary parent error must not release its expanded child");
        publish_page(&state, HOST, 11, &directory_page("", edge_entries.clone()));
        next_tick().await;
        assert!(
            explorer_text().contains("edge-child.rs"),
            "parent recovery restores the expanded child without user action"
        );
        let before_replay = js_sys::eval("window.__fileViewCommands.length")
            .unwrap()
            .as_f64()
            .unwrap() as usize;
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                StreamPath(format!("/project/{PROJECT}")),
                FrameKind::ProjectBootstrap,
                12,
                &bootstrap,
            )
            .unwrap(),
        );
        next_tick().await;
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        assert!(commands[before_replay..].iter().any(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().is_ok_and(|command| command.active && matches!(command.view, ProjectFileView::Directory {path, ..} if path.relative_path == "edge127"))), "replay re-declares off-page expansion before any parent rows exist");
        assert!(!commands[before_replay..].iter().any(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().is_ok_and(|command| !command.active && matches!(command.view, ProjectFileView::Directory {path, ..} if path.relative_path == "edge127"))), "transient row teardown must not withdraw replayed intent");
        publish_page(&state, HOST, 13, &directory_page("", edge_entries));
        publish_page(
            &state,
            HOST,
            14,
            &directory_page(
                "edge127",
                vec![ProjectFileEntry {
                    relative_path: "edge127/after-replay.rs".into(),
                    kind: ProjectFileKind::File,
                    op: protocol::FileEntryOp::Add,
                }],
            ),
        );
        next_tick().await;
        assert!(explorer_text().contains("after-replay.rs"));
        let mut missing = directory_page("edge127", Vec::new());
        missing.state = ProjectFileViewState::DirectoryMissing;
        publish_page(&state, HOST, 15, &missing);
        next_tick().await;
        let before_deleted_replay = js_sys::eval("window.__fileViewCommands.length")
            .unwrap()
            .as_f64()
            .unwrap() as usize;
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                StreamPath(format!("/project/{PROJECT}")),
                FrameKind::ProjectBootstrap,
                16,
                &bootstrap,
            )
            .unwrap(),
        );
        next_tick().await;
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        assert!(!commands[before_deleted_replay..].iter().any(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().is_ok_and(|command| command.active && matches!(command.view, ProjectFileView::Directory {path, ..} if path.relative_path == "edge127"))), "server-authored deletion retires expansion instead of accumulating history");
        let mut first = directory_page(
            "",
            vec![ProjectFileEntry {
                relative_path: "a-first.rs".into(),
                kind: ProjectFileKind::File,
                op: protocol::FileEntryOp::Add,
            }],
        );
        let boundary = protocol::ProjectFileCursor {
            path: ProjectPath {
                root: ProjectRootPath(ROOT.into()),
                relative_path: "z-last.rs".into(),
            },
            kind: ProjectFileKind::File,
            score: 0,
        };
        if let ProjectFileViewState::Ready { next, .. } = &mut first.state {
            *next = Some(boundary.clone());
        }
        publish_page(&state, HOST, 17, &first);
        next_tick().await;
        let next_button = || {
            (0..container.query_selector_all("button").unwrap().length())
                .filter_map(|index| container.query_selector_all("button").unwrap().item(index))
                .find(|button| button.text_content().as_deref() == Some("Next"))
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
        };
        next_button().click();
        next_tick().await;
        let input = container
            .query_selector(".panel-search-input")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        input.set_value("a-first");
        input
            .dispatch_event(
                &web_sys::Event::new_with_event_init_dict("input", &{
                    let init = web_sys::EventInit::new();
                    init.set_bubbles(true);
                    init
                })
                .unwrap(),
            )
            .unwrap();
        next_tick().await;
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        let last = commands
            .iter()
            .rev()
            .filter(|command| command.kind == FrameKind::ProjectFileView)
            .map(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().unwrap())
            .find(|command| matches!(&command.view, ProjectFileView::Directory {path, ..} if path.relative_path.is_empty()))
            .unwrap();
        assert!(
            last.active
                && matches!(last.view, ProjectFileView::Directory {path, window: ProjectFileWindow::First, filter, ..} if path.relative_path.is_empty() && filter == "a-first"),
            "filtering after Next starts at First, so earlier matches stay reachable"
        );
        if let ProjectFileView::Directory { filter, .. } = &mut first.view {
            *filter = "a-first".into();
        }
        publish_page(&state, HOST, 18, &first);
        next_tick().await;
        assert!(explorer_text().contains("a-first.rs"));
        next_button().click();
        next_tick().await;
        container
            .query_selector(".fe-toggle-hidden")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        let last = commands
            .iter()
            .rev()
            .filter(|command| command.kind == FrameKind::ProjectFileView)
            .map(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().unwrap())
            .find(|command| matches!(&command.view, ProjectFileView::Directory {path, ..} if path.relative_path.is_empty()))
            .unwrap();
        assert!(
            last.active
                && matches!(last.view, ProjectFileView::Directory {path, window: ProjectFileWindow::First, show_hidden: true, ..} if path.relative_path.is_empty()),
            "revealing hidden files after Next starts at First"
        );
        if let ProjectFileView::Directory { show_hidden, .. } = &mut first.view {
            *show_hidden = true;
        }
        if let ProjectFileViewState::Ready { entries, .. } = &mut first.state {
            entries[0].path.relative_path = ".a-first.rs".into();
        }
        publish_page(&state, HOST, 19, &first);
        next_tick().await;
        assert!(explorer_text().contains(".a-first.rs"));
        next_button().click();
        next_tick().await;
        let mut other = bootstrap.clone();
        other.project.id = ProjectId("same-path-other-project".into());
        let other_stream = StreamPath("/project/same-path-other-project".into());
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                other_stream.clone(),
                FrameKind::ProjectBootstrap,
                0,
                &other,
            )
            .unwrap(),
        );
        state.active_project.set(Some(ActiveProjectRef {
            host_id: HOST.into(),
            project_id: other.project.id,
        }));
        next_tick().await;
        let commands: Vec<protocol::Envelope> =
            serde_wasm_bindgen::from_value(js_sys::eval("window.__fileViewCommands").unwrap())
                .unwrap();
        let declared_root = commands.iter().rev().filter(|command| command.stream == other_stream && command.kind == FrameKind::ProjectFileView).map(|command| command.parse_payload::<protocol::ProjectFileViewPayload>().unwrap()).find(|command| matches!(&command.view, ProjectFileView::Directory {path, ..} if path.relative_path.is_empty())).unwrap();
        assert!(
            declared_root.active
                && matches!(
                    declared_root.view,
                    ProjectFileView::Directory {
                        window: ProjectFileWindow::First,
                        ..
                    }
                ),
            "a different project starts at First even when its root spelling matches"
        );
        if let ProjectFileViewState::Ready { entries, .. } = &mut first.state {
            entries[0].path.relative_path = ".a-first-other.rs".into();
        }
        crate::dispatch::dispatch_envelope(
            &state,
            HOST,
            protocol::Envelope::from_payload(
                other_stream,
                FrameKind::ProjectFileViewState,
                1,
                &first,
            )
            .unwrap(),
        );
        next_tick().await;
        assert!(explorer_text().contains(".a-first-other.rs"));
        drop(_mounted);
        next_tick().await;
    }
}
