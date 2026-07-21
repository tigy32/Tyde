use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::{delete_project_root, open_project_path_at};
use crate::components::host_browser::open_add_root_browser;
use crate::send::send_frame;
use crate::state::{AppState, OpenTarget, display_path_name, root_display_name};

use protocol::{
    CodeIntelOverviewHeadline, CodeIntelOverviewSummary, CodeIntelState, FrameKind,
    ProjectFileEntry, ProjectFileKind, ProjectId, ProjectListDirPayload, ProjectPath,
    ProjectRootPath, StreamPath,
};

/// A node in the file tree built from the flat entry list.
#[derive(Clone, Debug, PartialEq)]
struct TreeNode {
    name: String,
    kind: ProjectFileKind,
    relative_path: String,
    children: Vec<TreeNode>,
}

fn build_tree(entries: &[ProjectFileEntry]) -> Vec<TreeNode> {
    // Intermediate map: path segment -> (kind, children map)
    // We'll build a nested HashMap and then flatten to Vec<TreeNode>.
    struct DirEntry {
        kind: ProjectFileKind,
        relative_path: String,
        children: HashMap<String, DirEntry>,
    }

    let mut root_children: HashMap<String, DirEntry> = HashMap::new();

    for entry in entries {
        let parts: Vec<&str> = entry.relative_path.split('/').collect();
        let mut current = &mut root_children;

        for (i, part) in parts.iter().enumerate() {
            let is_last = i == parts.len() - 1;
            let dir_entry = current.entry(part.to_string()).or_insert_with(|| DirEntry {
                kind: if is_last {
                    entry.kind
                } else {
                    ProjectFileKind::Directory
                },
                relative_path: parts[..=i].join("/"),
                children: HashMap::new(),
            });
            if is_last {
                dir_entry.kind = entry.kind;
                dir_entry.relative_path = entry.relative_path.clone();
            }
            current = &mut dir_entry.children;
        }
    }

    fn to_tree_nodes(map: HashMap<String, DirEntry>) -> Vec<TreeNode> {
        let mut nodes: Vec<TreeNode> = map
            .into_iter()
            .map(|(name, entry)| TreeNode {
                name,
                kind: entry.kind,
                relative_path: entry.relative_path,
                children: to_tree_nodes(entry.children),
            })
            .collect();
        // Sort: directories first, then alphabetically
        nodes.sort_by(|a, b| {
            let a_dir = matches!(a.kind, ProjectFileKind::Directory);
            let b_dir = matches!(b.kind, ProjectFileKind::Directory);
            b_dir
                .cmp(&a_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        nodes
    }

    to_tree_nodes(root_children)
}

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
    let expanded_dirs = RwSignal::new(HashSet::<String>::new());

    let tree = Memo::new(move |_| {
        let pid = state.active_project.get()?.project_id;
        let file_map = state.file_tree.get();
        let roots = file_map.get(&pid)?;
        Some(
            roots
                .iter()
                .map(|root| (root.root.clone(), build_tree(&root.entries)))
                .collect::<Vec<_>>(),
        )
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
        filter.set(event_target_value(&ev));
    };

    let toggle_hidden = move |_| {
        show_hidden.update(|v| *v = !*v);
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
                            // Only thread an (host_id, project_id) into
                            // render_root_section when root edits are valid
                            // for this project — that tuple is what gates the
                            // per-root remove button. The check tracks the
                            // projects signal so adding a workbench child
                            // immediately removes the affordance from the
                            // parent's roots.
                            let project_ref = active.as_ref().and_then(|a| {
                                state
                                    .can_manage_project_roots(&a.host_id, &a.project_id)
                                    .then(|| (a.host_id.clone(), a.project_id.clone()))
                            });
                            root_trees
                                .into_iter()
                                .flat_map(|(root, nodes)| {
                                    render_root_section(
                                        root,
                                        nodes,
                                        &filter_val,
                                        hidden,
                                        expanded_dirs,
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
        CodeIntelOverviewHeadline::NotStarted => "idle",
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

/// Send a ProjectListDir request so the server returns 2 levels of entries under `dir_path`.
fn request_dir_listing(state: &AppState, root: ProjectRootPath, dir_relative_path: &str) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let project_id = active_project.project_id.clone();
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = ProjectListDirPayload {
        root,
        path: dir_relative_path.to_owned(),
    };
    spawn_local(async move {
        if let Err(e) = send_frame(
            &active_project.host_id,
            stream,
            FrameKind::ProjectListDir,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectListDir: {e}");
        }
    });
}

fn render_root_section(
    root: ProjectRootPath,
    nodes: Vec<TreeNode>,
    filter: &str,
    show_hidden: bool,
    expanded_dirs: RwSignal<HashSet<String>>,
    project_ref: Option<(String, ProjectId)>,
) -> Vec<AnyView> {
    let mut views = Vec::new();
    let root_label = root_display_name(&root);
    let root_title = root.0.clone();
    let remove_button = project_ref.map(|(host_id, project_id)| {
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
            <button class="fe-root-remove" title="Remove root" on:click=on_remove>
                "×"
            </button>
        }
    });
    views.push(
        view! {
            <div class="fe-root-header" title=root_title>
                <span class="fe-root-name">{root_label}</span>
                {remove_button}
            </div>
        }
        .into_any(),
    );
    views.extend(render_nodes(
        root,
        nodes,
        0,
        filter,
        show_hidden,
        expanded_dirs,
    ));
    views
}

fn expanded_key(root: &ProjectRootPath, relative_path: &str) -> String {
    format!("{}\u{0}{relative_path}", root.0)
}

fn render_nodes(
    root: ProjectRootPath,
    nodes: Vec<TreeNode>,
    depth: usize,
    filter: &str,
    show_hidden: bool,
    expanded_dirs: RwSignal<HashSet<String>>,
) -> Vec<AnyView> {
    nodes
        .into_iter()
        .filter(|node| {
            if !show_hidden && node.name.starts_with('.') {
                return false;
            }
            if filter.is_empty() {
                return true;
            }
            // For directories, show if any child matches
            if matches!(node.kind, ProjectFileKind::Directory) {
                return node_matches_filter(node, filter, show_hidden);
            }
            node.name.to_lowercase().contains(filter)
        })
        .map(|node| {
            let indent = depth * 16;
            match node.kind {
                ProjectFileKind::Directory => {
                    let name = node.name.clone();
                    let dir_path = node.relative_path.clone();
                    let children = node.children.clone();
                    let filter_owned = filter.to_owned();
                    let root_for_expand = root.clone();
                    let key = expanded_key(&root_for_expand, &dir_path);
                    let is_expanded = {
                        let key = key.clone();
                        Signal::derive(move || expanded_dirs.with(|set| set.contains(&key)))
                    };
                    let root_for_click = root.clone();
                    let root_for_children = root.clone();
                    let root_for_ctx = root.clone();
                    let dir_path_for_ctx = dir_path.clone();
                    // Resolve the context once, in render scope, rather than on
                    // every right-click inside the event callback.
                    let state_for_ctx = expect_context::<AppState>();

                    view! {
                        <div class="fe-dir-group">
                            <button
                                class="fe-item fe-dir"
                                style=format!("padding-left: {}px", indent + 4)
                                title="Right-click to search in this folder"
                                on:contextmenu={
                                    let root = root_for_ctx.clone();
                                    let dir_path = dir_path_for_ctx.clone();
                                    let state = state_for_ctx.clone();
                                    move |ev: web_sys::MouseEvent| {
                                        ev.prevent_default();
                                        crate::actions::search_in_folder(
                                            &state,
                                            root.clone(),
                                            dir_path.clone(),
                                        );
                                    }
                                }
                                on:click={
                                    let dir_path = dir_path.clone();
                                    let root = root_for_click.clone();
                                    let key = key.clone();
                                    move |_| {
                                        let opening = !expanded_dirs
                                            .with_untracked(|set| set.contains(&key));
                                        expanded_dirs.update(|set| {
                                            if opening {
                                                set.insert(key.clone());
                                            } else {
                                                set.remove(&key);
                                            }
                                        });
                                        if opening {
                                            let state = expect_context::<AppState>();
                                            request_dir_listing(&state, root.clone(), &dir_path);
                                        }
                                    }
                                }
                            >
                                <span class="fe-chevron">{move || if is_expanded.get() { "\u{25be}" } else { "\u{25b8}" }}</span>
                                <span class="fe-icon fe-folder-icon">{move || if is_expanded.get() { "\u{1f4c2}" } else { "\u{1f4c1}" }}</span>
                                <span class="fe-name">{name.clone()}</span>
                            </button>
                            <Show when=move || is_expanded.get()>
                                {
                                    let children = children.clone();
                                    let filter_owned = filter_owned.clone();
                                    let root = root_for_children.clone();
                                    move || render_nodes(root.clone(), children.clone(), depth + 1, &filter_owned, show_hidden, expanded_dirs)
                                }
                            </Show>
                        </div>
                    }
                    .into_any()
                }
                _ => view! {
                    <FileRow
                        root=root.clone()
                        relative_path=node.relative_path.clone()
                        name=node.name.clone()
                        indent=indent
                    />
                }
                .into_any(),
            }
        })
        .collect()
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

fn node_matches_filter(node: &TreeNode, filter: &str, show_hidden: bool) -> bool {
    if !show_hidden && node.name.starts_with('.') {
        return false;
    }
    if node.name.to_lowercase().contains(filter) {
        return true;
    }
    node.children
        .iter()
        .any(|child| node_matches_filter(child, filter, show_hidden))
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::ActiveProjectRef;
    use crate::wasm_test_support::Mounted;
    use leptos::mount::mount_to;
    use protocol::{
        CodeIntelLanguageId, CodeIntelOverviewHeadline, CodeIntelOverviewPayload,
        CodeIntelProviderId, CodeIntelProviderStatus, CodeIntelResourceMode, CodeIntelRootOverview,
        CodeIntelState,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        // One document is shared by every test in the binary. A container left
        // behind keeps its rows — and their hover/focus state — in the page for
        // whatever runs next, so each fixture disposes the last one and owns the
        // page it asserts about.
        let stale = document
            .query_selector_all("[data-test-container]")
            .unwrap();
        for index in 0..stale.length() {
            if let Some(node) = stale.item(index)
                && let Some(parent) = node.parent_node()
            {
                let _ = parent.remove_child(&node);
            }
        }
        let container = document.create_element("div").unwrap();
        container.set_attribute("data-test-container", "1").unwrap();
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
                        entries: entries.clone(),
                    }],
                );
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
        let container = make_container();
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
        // A media query cannot be forced on in a headless browser, so this
        // asserts the property that made the first attempt useless: the rule
        // that disables the animation has the same specificity as the rule that
        // sets it, so it only wins if it comes *later* in the sheet.
        let source = PROD_STYLES;
        let animates = source
            .rfind(".context-menu {")
            .expect("the menu carries the animation");
        let reduced = source
            .rfind("@media (prefers-reduced-motion: reduce)")
            .expect("reduced motion is honored");
        assert!(
            reduced > animates,
            "the reduced-motion block must come after the rule it overrides, or \
             source order silently beats it"
        );
        let block = &source[reduced..];
        assert!(
            block.contains(".context-menu"),
            "it must name the element that actually animates — the menus inherit \
             `menu-in` from .context-menu, not from their own classes"
        );
        assert!(
            block.contains("animation: none"),
            "and it must actually turn the animation off"
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
}
