use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::open_file;
use crate::components::host_browser::open_add_root_browser;
use crate::send::send_frame;
use crate::state::{AppState, display_path_name, root_display_name};

use protocol::{
    FrameKind, ProjectFileEntry, ProjectFileKind, ProjectListDirPayload, ProjectPath,
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
                let root_count = project.project.roots.len();
                let title = project.project.roots.join("\n");
                let label = if root_count == 1 {
                    project
                        .project
                        .roots
                        .first()
                        .map(|root| display_path_name(root))
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
    let add_root_disabled = move || state.active_project.get().is_none();

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
                    title="Add workspace root"
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
                            root_trees
                                .into_iter()
                                .flat_map(|(root, nodes)| {
                                    render_root_section(root, nodes, &filter_val, hidden, expanded_dirs)
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
) -> Vec<AnyView> {
    let mut views = Vec::new();
    let root_label = root_display_name(&root);
    let root_title = root.0.clone();
    views.push(
        view! {
            <div class="fe-root-header" title=root_title>
                <span class="fe-root-name">{root_label}</span>
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

                    view! {
                        <div class="fe-dir-group">
                            <button
                                class="fe-item fe-dir"
                                style=format!("padding-left: {}px", indent + 4)
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
                _ => {
                    let icon = file_type_icon(&node.name);
                    let path = node.relative_path.clone();
                    let root_for_click = root.clone();
                    let on_click = move |_| {
                        let state = expect_context::<AppState>();
                        open_file(
                            &state,
                            ProjectPath {
                                root: root_for_click.clone(),
                                relative_path: path.clone(),
                            },
                        );
                    };
                    view! {
                        <button
                            class="fe-item fe-file"
                            style=format!("padding-left: {}px", indent + 4)
                            on:click=on_click
                        >
                            <span class="fe-icon fe-file-icon">{icon}</span>
                            <span class="fe-name">{node.name}</span>
                        </button>
                    }
                    .into_any()
                }
            }
        })
        .collect()
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
