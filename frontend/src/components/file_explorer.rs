use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::{delete_project_root, open_project_path_at};
use crate::components::center_zone::{announce, workspace_width};
use crate::components::command_palette::{
    ContextActionId, context_binding, open_to_side_availability,
};
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
/// the counts are only used to enrich the indexing progress text.
fn aggregate_label(summary: &CodeIntelOverviewSummary) -> (String, &'static str) {
    let class = headline_dot_class(summary.headline);
    let label = match summary.headline {
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

/// One file row: an ordinary open, plus an Open-to-the-Side action reachable
/// by pointer and by keyboard.
///
/// The `Command/Ctrl+Enter` chord is bound *here*, on the focused row, and
/// nowhere else. It is never a global binding: the chat composer needs that
/// chord for send/steer (dev-docs/32 §12), so the handler also stops the event
/// from travelling any further.
///
/// Both openers go through `open_project_path_at`, which resolves the
/// destination pane at invocation time. A cold file therefore keeps the pane
/// the user aimed at even though its tab is only created when the contents
/// arrive, and a loaded file is duplicated into the other pane synchronously.
#[component]
fn FileRow(
    root: ProjectRootPath,
    relative_path: String,
    name: String,
    indent: usize,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let width = workspace_width();

    let path = ProjectPath {
        root,
        relative_path,
    };

    // Explorer rows exist only for the active project, and opening beside is
    // offered only there — there is no cross-project split affordance.
    let availability = Memo::new(move |_| open_to_side_availability(&state, width.get()));
    let side_disabled = move || !availability.get().is_enabled();
    let side_reason = move || availability.get().reason().unwrap_or_default().to_owned();

    let open_state = expect_context::<AppState>();
    let open_path = path.clone();
    let on_click = move |_| {
        open_project_path_at(&open_state, open_path.clone(), OpenTarget::Focused);
    };

    // A refusal the user cannot perceive is the same as a dead control. Every
    // refusal — inline button, keyboard chord, menu item — sets this, which is
    // rendered as visible text on the row *and* announced politely.
    let refusal: RwSignal<Option<&'static str>> = RwSignal::new(None);

    let side_state = expect_context::<AppState>();
    let side_path = path.clone();
    let open_to_side = move || {
        if let Some(reason) = availability.get_untracked().reason() {
            refusal.set(Some(reason));
            announce(reason);
            return;
        }
        refusal.set(None);
        open_project_path_at(&side_state, side_path.clone(), OpenTarget::Beside);
    };

    // Row context menu: the plan's specified explorer affordance (§4.1), and the
    // one that can carry a full-size (>=44px) target and a visible reason
    // without turning a 26px tree row into a 44px one.
    let menu: RwSignal<Option<(f64, f64)>> = RwSignal::new(None);
    let row_ref = NodeRef::<leptos::html::Button>::new();

    // Open the menu from the row itself, so a keyboard user reaches it without
    // a pointer: Shift+F10 and the Menu key are the platform conventions.
    let open_menu_at_row = move || {
        let Some(row) = row_ref.get_untracked() else {
            return;
        };
        let rect = row.get_bounding_client_rect();
        menu.set(Some((rect.left() + 16.0, rect.bottom())));
    };

    let on_keydown = {
        let open_to_side = open_to_side.clone();
        move |ev: web_sys::KeyboardEvent| {
            let key = ev.key();
            // The Open-to-the-Side chord stays element-scoped: it is handled on
            // the focused row and stopped there, so it can never reach the
            // window, where it would collide with the composer's send/steer.
            if key == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
                ev.prevent_default();
                ev.stop_propagation();
                open_to_side();
                return;
            }
            if key == "ContextMenu" || (key == "F10" && ev.shift_key()) {
                ev.prevent_default();
                ev.stop_propagation();
                open_menu_at_row();
            }
        }
    };
    let on_contextmenu = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        menu.set(Some((ev.client_x() as f64, ev.client_y() as f64)));
    };
    let on_side_click = {
        let open_to_side = open_to_side.clone();
        move |ev: web_sys::MouseEvent| {
            // The row itself would otherwise open the file in the focused pane.
            ev.stop_propagation();
            open_to_side();
        }
    };

    // Menu actions. A disabled item still activates — and refuses out loud
    // (dev-docs/32 §12): a control that does nothing and says nothing is the
    // failure mode this exists to prevent.
    let menu_open_state = expect_context::<AppState>();
    let menu_open_path = path.clone();
    let on_menu_open = move |_: web_sys::MouseEvent| {
        menu.set(None);
        open_project_path_at(
            &menu_open_state,
            menu_open_path.clone(),
            OpenTarget::Focused,
        );
    };
    let on_menu_side = {
        let open_to_side = open_to_side.clone();
        move |_: web_sys::MouseEvent| {
            if availability.get_untracked().is_enabled() {
                menu.set(None);
            }
            // `open_to_side` refuses, notifies, and announces on its own, so the
            // menu path cannot diverge from the button and chord paths.
            open_to_side();
        }
    };

    let icon = file_type_icon(&name);
    let side_label = format!("Open {name} to the side");
    // The hint comes from the chord that actually fires, so a macOS user is told
    // ⌘Enter rather than a Ctrl chord that is not what they press.
    let side_hint = context_binding(ContextActionId::OpenToSide).chord().hint();
    let side_title = move || {
        if side_disabled() {
            side_reason()
        } else {
            format!("Open to the side ({side_hint})")
        }
    };
    // The reason id is read from three closures, one of which is `<Show>`'s
    // children — which Leptos re-runs, so it must be `Fn`. A captured `String`
    // makes that impossible: any inner closure that takes it moves it out of the
    // children's environment and collapses it to `FnOnce` (E0525). Storing it as
    // a `Copy` handle removes the hazard at the root instead of scattering
    // `.clone()`s that each have to be got right.
    let reason_id: StoredValue<String> = StoredValue::new(format!(
        "fe-side-reason-{}",
        path.relative_path.replace(['/', '.'], "-")
    ));
    let described_by = move || side_disabled().then(|| reason_id.get_value());

    view! {
        <div class="fe-row">
            <button
                class="fe-item fe-file"
                node_ref=row_ref
                style=format!("padding-left: {}px", indent + 4)
                on:click=on_click
                on:keydown=on_keydown
                on:contextmenu=on_contextmenu
            >
                <span class="fe-icon fe-file-icon">{icon}</span>
                <span class="fe-name">{name}</span>
            </button>
            <button
                class="fe-open-side"
                class:disabled=side_disabled
                aria-label=side_label
                aria-disabled=move || side_disabled().then_some("true")
                aria-describedby=described_by
                aria-keyshortcuts="Control+Enter Meta+Enter"
                title=side_title
                on:click=on_side_click
            >
                <span class="fe-open-side-icon" aria-hidden="true">"\u{29c9}"</span>
            </button>
            <Show when=move || refusal.get().is_some()>
                <div class="fe-refusal" role="status" data-testid="fe-refusal">
                    {move || refusal.get().unwrap_or_default()}
                </div>
            </Show>
            <Show when=move || menu.get().is_some()>
                <div class="fe-menu-backdrop" on:click=move |_| menu.set(None)></div>
                <div
                    class="context-menu fe-menu"
                    role="menu"
                    aria-label="File actions"
                    style=move || {
                        menu.get()
                            .map(|(x, y)| format!("left: {x}px; top: {y}px;"))
                            .unwrap_or_default()
                    }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Escape" {
                            ev.prevent_default();
                            menu.set(None);
                        }
                    }
                >
                    <button
                        class="context-menu-item fe-menu-item"
                        role="menuitem"
                        on:click=on_menu_open.clone()
                    >
                        "Open"
                    </button>
                    <button
                        class="context-menu-item fe-menu-item"
                        role="menuitem"
                        class:disabled=side_disabled
                        aria-disabled=move || side_disabled().then_some("true")
                        aria-describedby=described_by
                        on:click=on_menu_side.clone()
                    >
                        <span class="context-menu-label">"Open to the Side"</span>
                        <Show when=side_disabled>
                            <span class="context-menu-reason" id=move || reason_id.get_value()>
                                {side_reason}
                            </span>
                        </Show>
                    </button>
                </div>
            </Show>
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
    use crate::wasm_test_support::Mounted;
    use crate::state::ActiveProjectRef;
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

    /// Longest transition in `styles.css` on anything asserted here
    /// (`.fe-open-side` animates `opacity` for 100ms).
    const LONGEST_TRANSITION_MS: i32 = 100;

    /// Wait for CSS transitions to finish before reading a computed style.
    ///
    /// `getComputedStyle` mid-transition returns the *interpolated* value: an
    /// action that has just been revealed still reports `opacity: 0` a
    /// microtask later. The assertion is about the settled appearance, so the
    /// fixture waits for it — the assertion itself still demands a full
    /// `opacity: 1`.
    async fn settle_styles() {
        next_tick().await;
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    &resolve,
                    LONGEST_TRANSITION_MS + 50,
                )
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
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

    // ── Open / Open to the Side ─────────────────────────────────────────

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
    /// Pre-loading matters: it is the state in which "Open to the Side"
    /// duplicates synchronously, so the test observes the destination pane
    /// directly instead of an unresolvable cold-open round trip.
    fn mount_explorer_with_files(
        container: HtmlElement,
        files: &[&str],
    ) -> Mounted<AppState> {
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

    fn is_visible(element: &HtmlElement) -> bool {
        let rect = element.get_bounding_client_rect();
        rect.width() > 0.0 && rect.height() > 0.0
    }

    fn query_all_in(root: &HtmlElement, selector: &str) -> Vec<HtmlElement> {
        let nodes = root.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .map(|i| nodes.item(i).unwrap().dyn_into::<HtmlElement>().unwrap())
            .collect()
    }

    fn side_action(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector("button.fe-open-side")
            .unwrap()
            .expect("file rows offer an Open to the Side action")
            .dyn_into()
            .unwrap()
    }

    fn ctrl_enter() -> web_sys::KeyboardEvent {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key("Enter");
        init.set_ctrl_key(true);
        init.set_bubbles(true);
        init.set_cancelable(true);
        web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap()
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

    fn tab_count(state: &AppState) -> usize {
        state
            .center_zone
            .with_untracked(|center_zone| center_zone.all_tab_ids().len())
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

    /// The visible row action opens the file beside, creating the split, and
    /// does not also trigger the row's ordinary open.
    #[wasm_bindgen_test]
    async fn the_row_action_opens_the_file_to_the_side() {
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        let action = side_action(&container);
        assert_eq!(
            action.get_attribute("aria-label").as_deref(),
            Some("Open main.rs to the side"),
            "the action names the file it acts on"
        );
        assert_eq!(
            action.get_attribute("aria-disabled"),
            None,
            "with an active project and room to split, the action is enabled"
        );

        action.click();
        next_tick().await;

        assert!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "opening to the side creates the second pane"
        );
        assert_eq!(
            pane_of_file(&state, "main.rs"),
            Some(crate::state::PaneId::Secondary),
            "the file opens in the pane the user aimed at, resolved at invocation"
        );
        assert_eq!(
            tab_count(&state),
            2,
            "the row action must not also fire the row's ordinary open — Home \
             plus one file tab, not two file tabs"
        );
    }

    /// The Command/Ctrl+Enter chord is bound to the focused row and stops
    /// there. It must never reach a global handler, because the chat composer
    /// owns that chord for send/steer.
    #[wasm_bindgen_test]
    async fn ctrl_enter_on_a_focused_row_opens_to_the_side_and_never_escapes_the_row() {
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        // Stand in for the app's global keydown listener, which lives on the
        // window and must not see this chord.
        let seen = Rc::new(RefCell::new(0usize));
        let seen_for_cb = seen.clone();
        let callback = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::Event)>::new(
            move |_: web_sys::Event| {
                *seen_for_cb.borrow_mut() += 1;
            },
        );
        let window = web_sys::window().unwrap();
        window
            .add_event_listener_with_callback("keydown", callback.as_ref().unchecked_ref())
            .unwrap();

        let row = file_rows(&container).remove(0);
        row.dispatch_event(&ctrl_enter()).unwrap();
        next_tick().await;

        assert_eq!(
            pane_of_file(&state, "main.rs"),
            Some(crate::state::PaneId::Secondary),
            "Ctrl+Enter on the focused row opens the file to the side"
        );
        assert_eq!(
            *seen.borrow(),
            0,
            "the chord is element-scoped: it must not propagate to a global \
             window handler, where it would collide with the composer's \
             send/steer binding"
        );

        window
            .remove_event_listener_with_callback("keydown", callback.as_ref().unchecked_ref())
            .unwrap();
    }

    /// A workspace that cannot host a second pane keeps the action visible and
    /// says why, and the action refuses to act.
    #[wasm_bindgen_test]
    async fn open_to_side_is_disabled_with_a_reason_when_tabs_are_disabled() {
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        state.tabs_enabled.set(false);
        next_tick().await;

        let action = side_action(&container);
        assert_eq!(
            action.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "the action is disabled, not hidden"
        );
        assert_eq!(
            action.get_attribute("title").as_deref(),
            Some("Enable tabs to use split view."),
            "the disabled action states the specific reason"
        );

        action.click();
        next_tick().await;
        assert!(
            !state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "a disabled action performs no work"
        );

        file_rows(&container)
            .remove(0)
            .dispatch_event(&ctrl_enter())
            .unwrap();
        next_tick().await;
        assert!(
            !state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "the keyboard path honors the same availability as the visible action"
        );
    }

    fn computed(element: &HtmlElement, property: &str) -> String {
        web_sys::window()
            .unwrap()
            .get_computed_style(element)
            .unwrap()
            .unwrap()
            .get_property_value(property)
            .unwrap()
    }

    fn shift_f10() -> web_sys::KeyboardEvent {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key("F10");
        init.set_shift_key(true);
        init.set_bubbles(true);
        init.set_cancelable(true);
        web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap()
    }

    /// The side-open affordance must not be hover-only: a keyboard user who has
    /// focused the row has to be able to *see* that the action exists, and a
    /// pointer user needs a target big enough to hit.
    #[wasm_bindgen_test]
    async fn the_side_open_action_is_revealed_by_focus_and_has_a_real_target() {
        ensure_styles_loaded();
        let container = make_container();
        let _state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        settle_styles().await;
        let action = side_action(&container);
        assert_eq!(
            computed(&action, "opacity"),
            "0",
            "precondition: the action is quiet until the row is engaged"
        );

        // Focusing the row — the keyboard path — reveals it. Nothing is hovered.
        let row = file_rows(&container).remove(0);
        row.focus().unwrap();
        settle_styles().await;
        assert_eq!(
            computed(&action, "opacity"),
            "1",
            "focusing the row must reveal its action: a hover-only affordance is \
             invisible to every keyboard and touch user"
        );

        let rect = action.get_bounding_client_rect();
        assert!(
            rect.width() >= 44.0,
            "the action's hit target must be at least 44px wide, got {}px",
            rect.width()
        );
        assert!(
            rect.height() >= 24.0,
            "and at least 24px tall, got {}px",
            rect.height()
        );
    }

    /// The full-size (>=44px) target for the same action is the row's context
    /// menu, which a keyboard user opens with Shift+F10 — no pointer, no hover.
    #[wasm_bindgen_test]
    async fn the_row_context_menu_opens_from_the_keyboard_and_has_a_full_size_target() {
        ensure_styles_loaded();
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        assert!(
            container.query_selector(".fe-menu").unwrap().is_none(),
            "precondition: no menu is open"
        );

        file_rows(&container)
            .remove(0)
            .dispatch_event(&shift_f10())
            .unwrap();
        next_tick().await;

        let menu = container
            .query_selector(".fe-menu")
            .unwrap()
            .expect("Shift+F10 on the focused row opens its context menu")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(menu.get_attribute("role").as_deref(), Some("menu"));

        let side_item = query_all_in(&menu, "[role=\"menuitem\"]")
            .into_iter()
            .find(|item| {
                item.text_content()
                    .unwrap_or_default()
                    .contains("to the Side")
            })
            .expect("the menu offers Open to the Side");
        let rect = side_item.get_bounding_client_rect();
        assert!(
            rect.height() >= 44.0,
            "the menu item is the full-size target for this action: expected >=44px \
             tall, got {}px",
            rect.height()
        );
        assert!(
            rect.width() >= 44.0,
            "and >=44px wide, got {}px",
            rect.width()
        );

        side_item.click();
        next_tick().await;
        assert_eq!(
            pane_of_file(&state, "main.rs"),
            Some(crate::state::PaneId::Secondary),
            "the menu item performs the same side-open as the inline action"
        );
    }

    /// Unavailable is not invisible: the menu item stays, keeps its keyboard
    /// reachability, describes its reason, and refuses out loud.
    #[wasm_bindgen_test]
    async fn an_unavailable_side_open_menu_item_keeps_its_reason() {
        ensure_styles_loaded();
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        state.tabs_enabled.set(false);
        next_tick().await;

        file_rows(&container)
            .remove(0)
            .dispatch_event(&shift_f10())
            .unwrap();
        next_tick().await;

        let menu = container
            .query_selector(".fe-menu")
            .unwrap()
            .expect("menu opened")
            .dyn_into::<HtmlElement>()
            .unwrap();
        let side_item = query_all_in(&menu, "[role=\"menuitem\"]")
            .into_iter()
            .find(|item| {
                item.text_content()
                    .unwrap_or_default()
                    .contains("to the Side")
            })
            .expect("the item stays listed when unavailable");

        assert_eq!(
            side_item.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "unavailable items are aria-disabled, never removed"
        );
        assert!(
            !side_item.has_attribute("disabled"),
            "and never the bare disabled attribute, which would drop them out of \
             the tab order and take the reason with them"
        );
        let described_by = side_item
            .get_attribute("aria-describedby")
            .expect("the item is described by its reason");
        let description = container
            .query_selector(&format!("#{described_by}"))
            .unwrap()
            .expect("the description element exists");
        assert_eq!(
            description.text_content().unwrap_or_default().trim(),
            "Enable tabs to use split view.",
            "the shared reason vocabulary, not a bespoke message"
        );

        side_item.click();
        next_tick().await;
        assert!(
            !state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "an unavailable item performs no work"
        );
    }

    /// A refusal the user cannot perceive is a dead control. Both the inline
    /// button and the keyboard chord must say why they refused — visibly, and
    /// to a screen reader.
    #[wasm_bindgen_test]
    async fn inline_and_chord_refusals_notify_visibly() {
        ensure_styles_loaded();
        let container = make_container();
        let state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        state.tabs_enabled.set(false);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-testid=\"fe-refusal\"]")
                .unwrap()
                .is_none(),
            "precondition: nothing has been refused yet"
        );

        // The inline button.
        side_action(&container).click();
        next_tick().await;
        let notice = container
            .query_selector("[data-testid=\"fe-refusal\"]")
            .unwrap()
            .expect("the inline refusal is shown, not swallowed")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            notice.text_content().unwrap_or_default().trim(),
            "Enable tabs to use split view.",
            "the visible text is the shared reason, not a generic failure"
        );
        assert_eq!(
            notice.get_attribute("role").as_deref(),
            Some("status"),
            "and it is announced, not only drawn"
        );
        assert!(is_visible(&notice), "the refusal has a real box on screen");
        assert!(
            !state
                .center_zone
                .with_untracked(|center_zone| center_zone.is_split()),
            "a refused action performs no work"
        );

        // The keyboard chord takes the same path.
        let row = file_rows(&container).remove(0);
        row.dispatch_event(&ctrl_enter()).unwrap();
        next_tick().await;
        let notice = container
            .query_selector("[data-testid=\"fe-refusal\"]")
            .unwrap()
            .expect("the chord refusal is shown too");
        assert_eq!(
            notice.text_content().unwrap_or_default().trim(),
            "Enable tabs to use split view.",
            "the chord and the button cannot refuse differently"
        );

        // The row advertises the chord it answers to.
        assert!(
            side_action(&container)
                .get_attribute("aria-keyshortcuts")
                .is_some_and(|keys| keys.contains("Enter")),
            "the action advertises its keyboard chord"
        );
    }

    /// `.fe-open-side` is reused by the git, search and references panels. Any
    /// row that hosts it must reveal it — and while it is invisible it must not
    /// swallow clicks.
    ///
    /// The git panel's `.gp-file-row` matched none of the reveal selectors, so
    /// its side-open button sat at `opacity: 0` **and still took clicks**: an
    /// invisible hit target in the middle of a file row. This pins the CSS
    /// contract for every host row. (The markup belongs to those panels; only
    /// the stylesheet is ours.)
    #[wasm_bindgen_test]
    async fn the_reused_side_open_control_is_never_an_invisible_click_target() {
        ensure_styles_loaded();
        let container = make_container();
        let document = web_sys::window().unwrap().document().unwrap();

        // Rebuild each host row's shape, exactly as its panel renders it.
        for (row_class, wrapper) in [("fe-row", None), ("gp-file-row", Some("gp-file-actions"))] {
            let row = document.create_element("div").unwrap();
            row.set_class_name(row_class);
            let action = document.create_element("button").unwrap();
            action.set_class_name("fe-open-side");
            match wrapper {
                Some(wrapper_class) => {
                    let group = document.create_element("div").unwrap();
                    group.set_class_name(wrapper_class);
                    group.append_child(&action).unwrap();
                    row.append_child(&group).unwrap();
                }
                None => {
                    row.append_child(&action).unwrap();
                }
            }
            container.append_child(&row).unwrap();
            next_tick().await;

            let action: HtmlElement = action.dyn_into().unwrap();
            // At rest it is quiet — and, crucially, not clickable. An opacity-0
            // element still receives pointer events unless told otherwise.
            assert_eq!(
                computed(&action, "opacity"),
                "0",
                "{row_class}: the control is quiet until the row is engaged"
            );
            assert_eq!(
                computed(&action, "pointer-events"),
                "none",
                "{row_class}: an invisible control must not be clickable — that is \
                 an invisible hit target sitting in the row"
            );

            // Keyboard focus anywhere in the row reveals it, and makes it usable.
            let row: HtmlElement = row.dyn_into().unwrap();
            row.set_attribute("tabindex", "0").unwrap();
            row.focus().unwrap();
            settle_styles().await;
            assert_eq!(
                computed(&action, "opacity"),
                "1",
                "{row_class}: focus within the row reveals its action"
            );
            assert_eq!(
                computed(&action, "pointer-events"),
                "auto",
                "{row_class}: and a revealed control is clickable"
            );
            row.blur().unwrap();
            next_tick().await;

            // Disabled stays visible: it carries the reason.
            action.set_attribute("aria-disabled", "true").unwrap();
            settle_styles().await;
            assert_eq!(
                computed(&action, "opacity"),
                "1",
                "{row_class}: a disabled control stays visible, because it is the \
                 thing carrying the reason"
            );
        }
    }

    /// A file row is a single dense line, exactly like a directory row. Its
    /// trailing side-open action shares that line with the name and must never
    /// wrap onto its own line, which would double the row's height.
    ///
    /// Regression guard: the base `.fe-item` rule carries `width: 100%`, which
    /// inside the wrapping `.fe-row` resolved as a full-width flex-basis and
    /// pushed the 44px `.fe-open-side` beneath the name — every file row
    /// rendered at ~2x height while directory rows stayed dense.
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
        let name = file_rows(&container).remove(0);
        let action = side_action(&container);
        let dir = container
            .query_selector("button.fe-dir")
            .unwrap()
            .expect("the fixture directory renders a directory row")
            .dyn_into::<HtmlElement>()
            .unwrap();

        // The action shares the name's line rather than wrapping beneath it. Its
        // resting opacity is 0 but it still occupies its layout box, so its top
        // reports where it wrapped to — the wrap doubled the row height.
        let name_top = name.get_bounding_client_rect().top();
        let action_top = action.get_bounding_client_rect().top();
        assert!(
            (action_top - name_top).abs() < 3.0,
            "the side-open action must share the name's line, not wrap below it: \
             name top {name_top}, action top {action_top}"
        );

        // And so a file row is no taller than a directory row — one dense line.
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

    /// The tooltip names the chord that actually fires — rendered from the
    /// binding, not a hardcoded "Ctrl+Enter" that is wrong on macOS.
    #[wasm_bindgen_test]
    async fn the_side_open_tooltip_shows_the_platform_chord() {
        let container = make_container();
        let _state = mount_explorer_with_files(container.clone(), &["main.rs"]);
        next_tick().await;

        let expected = crate::components::command_palette::context_binding(
            crate::components::command_palette::ContextActionId::OpenToSide,
        )
        .chord()
        .hint();
        let title = side_action(&container)
            .get_attribute("title")
            .expect("the enabled action has a tooltip");
        assert!(
            title.contains(&expected),
            "the tooltip shows the chord that fires ({expected}), got {title:?}"
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
