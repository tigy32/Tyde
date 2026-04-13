use std::collections::HashMap;

use leptos::prelude::*;

use crate::actions::open_file;
use crate::state::AppState;

use protocol::{ProjectFileEntry, ProjectFileKind};

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
            b_dir.cmp(&a_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
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

    let tree = Memo::new(move |_| {
        let pid = state.active_project_id.get()?;
        let file_map = state.file_tree.get();
        let entries = file_map.get(&pid)?;
        Some(build_tree(entries))
    });

    let project_root = Memo::new(move |_| {
        let pid = state.active_project_id.get()?;
        let projects = state.projects.get();
        let project = projects.iter().find(|p| p.id == pid)?;
        project.roots.first().cloned()
    });

    let on_filter_input = move |ev: leptos::ev::Event| {
        filter.set(event_target_value(&ev));
    };

    let toggle_hidden = move |_| {
        show_hidden.update(|v| *v = !*v);
    };

    view! {
        <div class="file-explorer">
            <div class="fe-header">
                <span class="fe-breadcrumb" title=move || project_root.get().unwrap_or_default()>
                    {move || {
                        project_root.get()
                            .map(|r| {
                                r.rsplit('/').next().unwrap_or(&r).to_owned()
                            })
                            .unwrap_or_else(|| "No project".to_owned())
                    }}
                </span>
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
                />
            </div>
            <div class="fe-tree">
                {move || {
                    match tree.get() {
                        Some(nodes) => {
                            let filter_val = filter.get().to_lowercase();
                            let hidden = show_hidden.get();
                            render_nodes(nodes, 0, &filter_val, hidden)
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

fn render_nodes(
    nodes: Vec<TreeNode>,
    depth: usize,
    filter: &str,
    show_hidden: bool,
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
                    let children = node.children.clone();
                    let expanded = RwSignal::new(depth == 0);
                    let filter_owned = filter.to_owned();

                    view! {
                        <div class="fe-dir-group">
                            <button
                                class="fe-item fe-dir"
                                style=format!("padding-left: {}px", indent + 4)
                                on:click=move |_| expanded.update(|v| *v = !*v)
                            >
                                <span class="fe-chevron">{move || if expanded.get() { "\u{25be}" } else { "\u{25b8}" }}</span>
                                <span class="fe-icon fe-folder-icon">{move || if expanded.get() { "\u{1f4c2}" } else { "\u{1f4c1}" }}</span>
                                <span class="fe-name">{name.clone()}</span>
                            </button>
                            <Show when=move || expanded.get()>
                                {
                                    let children = children.clone();
                                    let filter_owned = filter_owned.clone();
                                    move || render_nodes(children.clone(), depth + 1, &filter_owned, show_hidden)
                                }
                            </Show>
                        </div>
                    }
                    .into_any()
                }
                _ => {
                    let icon = file_type_icon(&node.name);
                    let path = node.relative_path.clone();
                    let on_click = move |_| {
                        let state = expect_context::<AppState>();
                        open_file(&state, &path);
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
