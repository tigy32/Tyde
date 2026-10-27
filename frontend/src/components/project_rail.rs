use leptos::prelude::*;
use leptos::tachys::dom::event_target_value;
use protocol::{Project, ProjectSource};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::actions::{
    begin_new_chat_default, create_workbench, delete_project, remove_workbench, rename_project,
    reorder_projects,
};
use crate::components::host_browser::open_project_browser;
use crate::state::{ActiveProjectRef, AppState, TabContent, WorkbenchRemovePrompt};

#[derive(Clone, Debug, PartialEq, Eq)]
struct DraggedProject {
    host_id: String,
    project_id: protocol::ProjectId,
    /// `Some` when the dragged row is a workbench (its parent project id),
    /// `None` for a top-level standalone project. Captured at drag start so
    /// drop targets can apply the same scope rule as `reorder_projects`:
    /// top-level rows reorder among top-level rows, workbenches among
    /// siblings under the same parent. Cross-scope targets show no drop
    /// indicator and reject the drop.
    parent_project_id: Option<protocol::ProjectId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropPlacement {
    Before,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectDropTarget {
    host_id: String,
    project_id: protocol::ProjectId,
    placement: DropPlacement,
}

#[derive(Clone, Debug, PartialEq)]
struct RailContextMenu {
    project: Option<ActiveProjectRef>,
    x: f64,
    y: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkbenchCreatePrompt {
    host_id: String,
    parent_project_id: protocol::ProjectId,
    parent_name: String,
}

type EditingKey = (String, protocol::ProjectId);

#[component]
pub fn ProjectRail() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_connected = state.clone();
    let state_for_add = state.clone();
    let state_for_hosts = state.clone();
    let dragged_project = RwSignal::new(None::<DraggedProject>);
    let drop_target = RwSignal::new(None::<ProjectDropTarget>);
    let editing_project = RwSignal::new(None::<EditingKey>);
    let context_menu = RwSignal::new(None::<RailContextMenu>);
    let workbench_prompt = RwSignal::new(None::<WorkbenchCreatePrompt>);

    let state_for_home = state.clone();
    let go_home = move |_| {
        state_for_home.switch_active_project(None);
    };

    let on_home_contextmenu = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        context_menu.set(Some(RailContextMenu {
            project: None,
            x: ev.client_x() as f64,
            y: ev.client_y() as f64,
        }));
    };

    let home_class = move || {
        let is_home = state
            .center_zone
            .with(|cz| matches!(cz.active_content(), Some(TabContent::Home)));
        if state.active_project.get().is_none() && is_home {
            "rail-item rail-home active"
        } else {
            "rail-item rail-home"
        }
    };

    let connected = Memo::new(move |_| state_for_connected.active_connection_count() > 0);

    let expanded = RwSignal::new(false);
    let toggle_expanded = move |_| expanded.update(|value| *value = !*value);
    let layouts = state.sidebar_layout;
    let nav_class = move || {
        if expanded.get() {
            "project-rail expanded"
        } else {
            "project-rail"
        }
    };
    let toggle_title = move || {
        if expanded.get() { "Collapse" } else { "Expand" }
    };

    let on_add_click = move |_| open_project_browser(&state_for_add);

    view! {
        <nav class=nav_class aria-label="Projects" style=move || if layouts.get().mode == crate::sidebar_layout::InterfaceMode::Vibe { "display:none" } else { "" }>
            <div class="rail-items">
                <button class="rail-toggle" on:click=toggle_expanded title=toggle_title>
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                        <polyline points="9 18 15 12 9 6"/>
                    </svg>
                </button>

                <button class=home_class on:click=go_home on:contextmenu=on_home_contextmenu title="Home">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>
                        <polyline points="9 22 9 12 15 12 15 22"/>
                    </svg>
                    <span class="rail-label">"Home"</span>
                </button>

                <div class="rail-divider"></div>

                <For
                    each=move || state_for_hosts.configured_hosts.get()
                    key=|host| host.id.clone()
                    let:host
                >
                    {
                        let state_for_status = state.clone();
                        let host_id_for_status = host.id.clone();
                        let status = move || {
                            state_for_status.connection_statuses
                                .get()
                                .get(&host_id_for_status)
                                .cloned()
                                .unwrap_or(crate::state::ConnectionStatus::Disconnected)
                        };

                        let host_id_for_top = host.id.clone();
                        let host_id_for_top_filter = host.id.clone();
                        let top_level = {
                            let host_id = host_id_for_top_filter.clone();
                            move || {
                                state.projects.with(|projects| {
                                    projects
                                        .iter()
                                        .filter(|project| {
                                            project.host_id == host_id
                                                && matches!(
                                                    project.project.source,
                                                    ProjectSource::Standalone { .. }
                                                )
                                        })
                                        .map(|info| info.project.id.clone())
                                        .collect::<Vec<_>>()
                                })
                            }
                        };

                        view! {
                            <div class="rail-host-group">
                                <div class="rail-host-label" title=host.label.clone()>
                                    {host.label.clone()}
                                    <span class="rail-host-state">
                                        {move || match status() {
                                            crate::state::ConnectionStatus::Connected => "●",
                                            crate::state::ConnectionStatus::Connecting => "◐",
                                            crate::state::ConnectionStatus::Reconnecting { .. } => "Reconnecting…",
                                            crate::state::ConnectionStatus::Disconnected => "○",
                                            crate::state::ConnectionStatus::Error(_) => "!",
                                        }}
                                    </span>
                                </div>
                                <For
                                    each=top_level
                                    key=|id| id.clone()
                                    let:project_id
                                >
                                    {
                                        let host_id = host_id_for_top.clone();
                                        view! {
                                            <ProjectGroup
                                                host_id=host_id
                                                project_id=project_id
                                                dragged_project=dragged_project
                                                drop_target=drop_target
                                                editing_project=editing_project
                                                context_menu=context_menu
                                            />
                                        }
                                    }
                                </For>
                            </div>
                        }
                    }
                </For>
            </div>

            <div class="rail-bottom">
                <button
                    class="rail-item rail-add"
                    title=move || if connected.get() {
                        "New project on the selected host"
                    } else {
                        "Connect to a host first to create a project"
                    }
                    on:click=on_add_click
                    disabled=move || !connected.get()
                >
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="12" y1="5" x2="12" y2="19"/>
                        <line x1="5" y1="12" x2="19" y2="12"/>
                    </svg>
                </button>
            </div>

            {move || context_menu.get().map(|menu| view! {
                <RailContextMenuView
                    menu=menu
                    context_menu=context_menu
                    editing_project=editing_project
                    workbench_prompt=workbench_prompt
                />
            })}
            {move || workbench_prompt.get().map(|prompt| view! {
                <WorkbenchCreateModal prompt=prompt workbench_prompt=workbench_prompt />
            })}

        </nav>
    }
}

/// A top-level standalone project plus its workbench children. The parent row
/// and the children are rendered as a single visual unit so children disappear
/// when the parent is removed.
#[component]
fn ProjectGroup(
    host_id: String,
    project_id: protocol::ProjectId,
    dragged_project: RwSignal<Option<DraggedProject>>,
    drop_target: RwSignal<Option<ProjectDropTarget>>,
    editing_project: RwSignal<Option<EditingKey>>,
    context_menu: RwSignal<Option<RailContextMenu>>,
) -> impl IntoView {
    let workbenches_host = host_id.clone();
    let workbenches_parent = project_id.clone();
    // Per §7.1 of the spec, the children list is a derivation off the same
    // signal; both filter closures live inside `move ||` blocks so adding /
    // removing a workbench reactively re-renders.
    let workbenches_for = {
        let host_id = workbenches_host.clone();
        let parent_id = workbenches_parent.clone();
        move || {
            let state = expect_context::<AppState>();
            state.projects.with(|projects| {
                projects
                    .iter()
                    .filter(|info| {
                        info.host_id == host_id
                            && info.project.parent_project_id() == Some(&parent_id)
                    })
                    .map(|info| info.project.id.clone())
                    .collect::<Vec<_>>()
            })
        }
    };

    let host_id_for_parent = host_id.clone();
    let project_id_for_parent = project_id.clone();
    let host_id_for_children = host_id.clone();

    view! {
        <div class="rail-project-group">
            <ProjectRow
                host_id=host_id_for_parent
                project_id=project_id_for_parent
                dragged_project=dragged_project
                drop_target=drop_target
                editing_project=editing_project
                context_menu=context_menu
            />
            <div class="rail-workbench-children">
                <For
                    each=workbenches_for
                    key=|id| id.clone()
                    let:workbench_id
                >
                    {
                        let host_id = host_id_for_children.clone();
                        view! {
                            <ProjectRow
                                host_id=host_id
                                project_id=workbench_id
                                dragged_project=dragged_project
                                drop_target=drop_target
                                editing_project=editing_project
                                context_menu=context_menu
                            />
                        }
                    }
                </For>
            </div>
        </div>
    }
}

#[component]
fn ProjectRow(
    host_id: String,
    project_id: protocol::ProjectId,
    dragged_project: RwSignal<Option<DraggedProject>>,
    drop_target: RwSignal<Option<ProjectDropTarget>>,
    editing_project: RwSignal<Option<EditingKey>>,
    context_menu: RwSignal<Option<RailContextMenu>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Look up the project reactively so live changes (rename, reorder) flow
    // through without recreating the row.
    let host_id_for_lookup = host_id.clone();
    let project_id_for_lookup = project_id.clone();
    let project_signal = {
        let state = state.clone();
        move || {
            state.projects.with(|projects| {
                projects
                    .iter()
                    .find(|info| {
                        info.host_id == host_id_for_lookup
                            && info.project.id == project_id_for_lookup
                    })
                    .map(|info| info.project.clone())
            })
        }
    };

    let project_name = {
        let project_signal = project_signal.clone();
        move || {
            project_signal()
                .map(|project| project.name)
                .unwrap_or_default()
        }
    };
    let is_workbench_signal = {
        let project_signal = project_signal.clone();
        move || project_signal().is_some_and(|p| p.is_workbench())
    };

    let host_id_for_class = host_id.clone();
    let project_id_for_class = project_id.clone();
    let state_for_class = state.clone();
    let is_workbench_for_class = is_workbench_signal.clone();
    let item_class = move || {
        let is_active = state_for_class
            .active_project
            .get()
            .as_ref()
            .is_some_and(|active| {
                active.host_id == host_id_for_class && active.project_id == project_id_for_class
            });
        let mut class = String::from("rail-item rail-project");
        if is_workbench_for_class() {
            class.push_str(" rail-workbench");
        }
        if is_active {
            class.push_str(" active");
        }
        class
    };

    let host_id_for_row_class = host_id.clone();
    let project_id_for_row_class = project_id.clone();
    let is_workbench_for_row = is_workbench_signal.clone();
    let row_class = move || {
        let mut class = String::from("rail-project-row");
        if is_workbench_for_row() {
            class.push_str(" rail-project-row-workbench");
        }
        if dragged_project.get().as_ref().is_some_and(|dragged| {
            dragged.host_id == host_id_for_row_class
                && dragged.project_id == project_id_for_row_class
        }) {
            class.push_str(" dragging");
        }
        if let Some(target) = drop_target.get()
            && target.host_id == host_id_for_row_class
            && target.project_id == project_id_for_row_class
        {
            class.push_str(match target.placement {
                DropPlacement::Before => " drop-before",
                DropPlacement::After => " drop-after",
            });
        }
        class
    };

    let state_for_click = state.clone();
    let host_id_for_click = host_id.clone();
    let project_id_for_click = project_id.clone();
    let on_click = move |_| {
        state_for_click.switch_active_project(Some(ActiveProjectRef {
            host_id: host_id_for_click.clone(),
            project_id: project_id_for_click.clone(),
        }));
    };

    let host_id_for_drag = host_id.clone();
    let project_id_for_drag = project_id.clone();
    let project_signal_for_drag = project_signal.clone();
    let on_drag_start = move |ev: web_sys::DragEvent| {
        if let Some(data_transfer) = ev.data_transfer() {
            data_transfer.set_effect_allowed("move");
            let _ = data_transfer.set_data("text/plain", &project_id_for_drag.0);
        }
        drop_target.set(None);
        dragged_project.set(Some(DraggedProject {
            host_id: host_id_for_drag.clone(),
            project_id: project_id_for_drag.clone(),
            parent_project_id: project_signal_for_drag()
                .and_then(|project| project.parent_project_id().cloned()),
        }));
    };

    let host_id_for_dragover = host_id.clone();
    let project_id_for_dragover = project_id.clone();
    let project_signal_for_dragover = project_signal.clone();
    let on_drag_over = move |ev: web_sys::DragEvent| {
        let Some(active_drag) = dragged_project.get() else {
            return;
        };
        if active_drag.host_id != host_id_for_dragover
            || active_drag.project_id == project_id_for_dragover
        {
            return;
        }
        // Reorder is scoped (same parent, or both top-level) — see
        // `reorder_projects`. Don't advertise a drop the action would
        // reject: no prevent_default (so the drop never fires here) and no
        // indicator.
        let target_parent =
            project_signal_for_dragover().and_then(|project| project.parent_project_id().cloned());
        if active_drag.parent_project_id != target_parent {
            if drop_target.get_untracked().is_some() {
                drop_target.set(None);
            }
            return;
        }
        ev.prevent_default();
        if let Some(data_transfer) = ev.data_transfer() {
            data_transfer.set_drop_effect("move");
        }
        drop_target.set(Some(ProjectDropTarget {
            host_id: host_id_for_dragover.clone(),
            project_id: project_id_for_dragover.clone(),
            placement: drag_drop_placement(&ev),
        }));
    };

    let state_for_reorder = state.clone();
    let host_id_for_drop = host_id.clone();
    let project_id_for_drop = project_id.clone();
    let project_signal_for_drop = project_signal.clone();
    let on_drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        let Some(active_drag) = dragged_project.get() else {
            return;
        };
        drop_target.set(None);
        dragged_project.set(None);
        if active_drag.host_id != host_id_for_drop || active_drag.project_id == project_id_for_drop
        {
            return;
        }
        // Same scope rule as on_drag_over / reorder_projects; defensive in
        // case a drop sneaks through without a matching dragover.
        let target_parent =
            project_signal_for_drop().and_then(|project| project.parent_project_id().cloned());
        if active_drag.parent_project_id != target_parent {
            return;
        }
        reorder_projects(
            &state_for_reorder,
            host_id_for_drop.clone(),
            active_drag.project_id,
            project_id_for_drop.clone(),
            matches!(drag_drop_placement(&ev), DropPlacement::After),
        );
    };

    let on_drag_end = move |_| {
        drop_target.set(None);
        dragged_project.set(None);
    };

    let editing_key: EditingKey = (host_id.clone(), project_id.clone());
    let editing_key_for_check = editing_key.clone();
    let editing_key_for_dbl = editing_key.clone();
    let is_editing = move || editing_project.get().as_ref() == Some(&editing_key_for_check);
    let on_dblclick = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        editing_project.set(Some(editing_key_for_dbl.clone()));
    };

    let host_id_for_ctx = host_id.clone();
    let project_id_for_ctx = project_id.clone();
    let on_contextmenu = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        context_menu.set(Some(RailContextMenu {
            project: Some(ActiveProjectRef {
                host_id: host_id_for_ctx.clone(),
                project_id: project_id_for_ctx.clone(),
            }),
            x: ev.client_x() as f64,
            y: ev.client_y() as f64,
        }));
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();
    let edit_value: RwSignal<String> = RwSignal::new(String::new());
    {
        let editing_key_for_seed = editing_key.clone();
        let project_name_for_seed = project_name.clone();
        let mut last_editing = false;
        Effect::new(move |_| {
            let editing_now = editing_project.get().as_ref() == Some(&editing_key_for_seed);
            if editing_now && !last_editing {
                edit_value.set(project_name_for_seed());
            }
            last_editing = editing_now;
        });
    }
    {
        let editing_key_for_focus = editing_key.clone();
        Effect::new(move |_| {
            let editing_now = editing_project.get().as_ref() == Some(&editing_key_for_focus);
            if editing_now && let Some(el) = input_ref.get() {
                let _ = el.focus();
                el.select();
            }
        });
    }

    let state_for_kd = state.clone();
    let host_id_for_kd = host_id.clone();
    let project_id_for_kd = project_id.clone();
    let project_name_for_kd = project_name.clone();
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        ev.stop_propagation();
        match ev.key().as_str() {
            "Enter" => {
                let new_name = edit_value.get_untracked().trim().to_string();
                let original = project_name_for_kd();
                editing_project.set(None);
                if !new_name.is_empty() && new_name != original {
                    rename_project(
                        &state_for_kd,
                        host_id_for_kd.clone(),
                        project_id_for_kd.clone(),
                        new_name,
                    );
                }
            }
            "Escape" => editing_project.set(None),
            _ => {}
        }
    };

    let state_for_bl = state.clone();
    let host_id_for_bl = host_id.clone();
    let project_id_for_bl = project_id.clone();
    let project_name_for_bl = project_name.clone();
    let editing_key_for_bl = editing_key.clone();
    let on_blur = move |_: web_sys::FocusEvent| {
        if editing_project.with_untracked(|e| e.as_ref() != Some(&editing_key_for_bl)) {
            return;
        }
        let new_name = edit_value.get_untracked().trim().to_string();
        let original = project_name_for_bl();
        editing_project.set(None);
        if !new_name.is_empty() && new_name != original {
            rename_project(
                &state_for_bl,
                host_id_for_bl.clone(),
                project_id_for_bl.clone(),
                new_name,
            );
        }
    };

    let project_name_for_label = project_name.clone();
    let label_view = move || {
        if is_editing() {
            view! {
                <input
                    type="text"
                    class="rail-label rail-label-input"
                    node_ref=input_ref
                    spellcheck="false"
                    autocapitalize="none"
                    autocomplete="off"
                    prop:value=move || edit_value.get()
                    on:input=move |ev| edit_value.set(event_target_value(&ev))
                    on:keydown=on_keydown.clone()
                    on:blur=on_blur.clone()
                    on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                    on:dblclick=|ev: web_sys::MouseEvent| ev.stop_propagation()
                />
            }
            .into_any()
        } else {
            let name = project_name_for_label();
            view! { <span class="rail-label">{name}</span> }.into_any()
        }
    };

    let abbrev = {
        let project_name = project_name.clone();
        move || abbreviate(&project_name())
    };
    let title_attr = project_name.clone();
    let engine_title = move || {
        project_signal().and_then(|project| match project.source {
            ProjectSource::Standalone { .. } => None,
            ProjectSource::GitWorkbench { environment, .. } => Some(match environment {
                protocol::WorkbenchEnvironment::Native => "Native workbench".to_owned(),
                protocol::WorkbenchEnvironment::Bubblewrap {
                    initialization,
                    build_blockers,
                    ..
                } => {
                    let title = match &initialization {
                        protocol::WorkbenchInitialization::Cold => {
                            "Bubblewrap · Ready (cold)".to_owned()
                        }
                        protocol::WorkbenchInitialization::Prepared { .. } => {
                            "Bubblewrap · Baseline prepared (initial build)".to_owned()
                        }
                        protocol::WorkbenchInitialization::Blocked { message } => {
                            format!("Bubblewrap · Warm builds blocked: {message}")
                        }
                        protocol::WorkbenchInitialization::Snapshot { .. } => {
                            "Bubblewrap · Snapshot seeded".to_owned()
                        }
                    };
                    if build_blockers.is_empty()
                        || matches!(
                            initialization,
                            protocol::WorkbenchInitialization::Blocked { .. }
                        )
                    {
                        title
                    } else {
                        format!(
                            "{title} · {}",
                            build_blockers
                                .iter()
                                .map(|blocker| format!(
                                    "{}: {}",
                                    blocker.logical_root, blocker.message
                                ))
                                .collect::<Vec<_>>()
                                .join("; ")
                        )
                    }
                }
            }),
        })
    };

    view! {
        <div
            class=row_class
            draggable="true"
            on:dragstart=on_drag_start
            on:dragover=on_drag_over
            on:drop=on_drop
            on:dragend=on_drag_end
        >
            <button
                class=item_class
                title=title_attr
                on:click=on_click
                on:dblclick=on_dblclick
                on:contextmenu=on_contextmenu
            >
                <span class="rail-tag" title=engine_title>{move || abbrev()}</span>
                {label_view}
            </button>
        </div>
    }
}

#[component]
fn RailContextMenuView(
    menu: RailContextMenu,
    context_menu: RwSignal<Option<RailContextMenu>>,
    editing_project: RwSignal<Option<EditingKey>>,
    workbench_prompt: RwSignal<Option<WorkbenchCreatePrompt>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let project_for_new_agent = menu.project.clone();
    let on_new_agent = move |_| {
        context_menu.set(None);
        state.switch_active_project(project_for_new_agent.clone());
        begin_new_chat_default(&state);
    };

    let close_via_backdrop = move |_| context_menu.set(None);
    let stop_in_menu = |ev: web_sys::MouseEvent| ev.stop_propagation();

    view! {
        <>
            <div
                class="rail-context-backdrop"
                style="position: fixed; inset: 0; z-index: 1000;"
                on:click=close_via_backdrop
                on:contextmenu=move |ev: web_sys::MouseEvent| {
                    ev.prevent_default();
                    context_menu.set(None);
                }
            />
            <div
                class="context-menu rail-context-menu"
                style=format!("left: {}px; top: {}px;", menu.x, menu.y)
                on:click=stop_in_menu
            >
                <button class="context-menu-item" on:click=on_new_agent>"New Agent"</button>
                {menu.project.map(|project| view! {
                    <ProjectContextMenuItems
                        project=project
                        context_menu=context_menu
                        editing_project=editing_project
                        workbench_prompt=workbench_prompt
                    />
                })}
            </div>
        </>
    }
}

fn failed_workbench_removal(
    state: &AppState,
    host_id: &str,
    workbench_id: &protocol::ProjectId,
) -> Option<(protocol::StreamPath, String)> {
    state.project_operations.with(|operations| {
        let mut failed = operations
            .iter()
            .filter_map(|((host, stream), operation)| {
                if host != host_id {
                    return None;
                }
                let protocol::ProjectMutation::RemoveWorkbench(payload) = &operation.mutation
                else {
                    return None;
                };
                if &payload.id != workbench_id {
                    return None;
                }
                let protocol::ProjectOperationState::Failed {
                    message,
                    cleanup_may_be_partial,
                    ..
                } = &operation.state
                else {
                    return None;
                };
                let message = if *cleanup_may_be_partial {
                    format!(
                        "{message} Cleanup may be incomplete; inspect the workbench before trying again."
                    )
                } else {
                    message.clone()
                };
                Some((stream.clone(), message))
            })
            .collect::<Vec<_>>();
        failed.sort_by(|left, right| left.0 .0.cmp(&right.0 .0));
        failed.into_iter().next()
    })
}

#[component]
fn ProjectContextMenuItems(
    project: ActiveProjectRef,
    context_menu: RwSignal<Option<RailContextMenu>>,
    editing_project: RwSignal<Option<EditingKey>>,
    workbench_prompt: RwSignal<Option<WorkbenchCreatePrompt>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let host_id_for_lookup = project.host_id.clone();
    let project_id_for_lookup = project.project_id.clone();
    let project_signal: Memo<Option<Project>> = {
        let state = state.clone();
        Memo::new(move |_| {
            state.projects.with(|projects| {
                projects
                    .iter()
                    .find(|info| {
                        info.host_id == host_id_for_lookup
                            && info.project.id == project_id_for_lookup
                    })
                    .map(|info| info.project.clone())
            })
        })
    };

    let busy = {
        let state = state.clone();
        let project = project.clone();
        Memo::new(move |_| {
            crate::notices::project_mutation_pending(&state, &project.host_id, &project.project_id)
        })
    };

    let recoverable_removal = {
        let state = state.clone();
        let project = project.clone();
        Memo::new(move |_| {
            failed_workbench_removal(&state, &project.host_id, &project.project_id).is_some()
        })
    };

    let is_workbench = move || {
        project_signal
            .get()
            .is_some_and(|project| project.is_workbench())
    };

    // The server rejects deleting a standalone project while a workbench
    // still references it ("cannot delete project … while referenced by
    // workbench …"), so offering an enabled Delete there is a guaranteed
    // failure. Derive child presence from the same projects signal the rail
    // uses for nesting.
    let has_workbench_children: Memo<bool> = {
        let state = state.clone();
        let host_id = project.host_id.clone();
        let project_id = project.project_id.clone();
        Memo::new(move |_| {
            state.projects.with(|projects| {
                projects.iter().any(|info| {
                    info.host_id == host_id && info.project.parent_project_id() == Some(&project_id)
                })
            })
        })
    };

    let editor_target = project.clone();
    let editor = state.project_editor;
    let on_edit = move |_| {
        context_menu.set(None);
        editor.set(Some(editor_target.clone()));
    };
    let host_id_for_rename = project.host_id.clone();
    let project_id_for_rename = project.project_id.clone();
    let on_rename = move |_| {
        context_menu.set(None);
        editing_project.set(Some((
            host_id_for_rename.clone(),
            project_id_for_rename.clone(),
        )));
    };

    let host_id_for_new = project.host_id.clone();
    let project_id_for_new = project.project_id.clone();
    let on_new_workbench = move |_| {
        context_menu.set(None);
        let state = expect_context::<AppState>();
        let parent_name = state
            .projects
            .get_untracked()
            .into_iter()
            .find_map(|info| {
                if info.host_id == host_id_for_new && info.project.id == project_id_for_new {
                    Some(info.project.name)
                } else {
                    None
                }
            })
            .unwrap_or_default();
        workbench_prompt.set(Some(WorkbenchCreatePrompt {
            host_id: host_id_for_new.clone(),
            parent_project_id: project_id_for_new.clone(),
            parent_name,
        }));
    };

    let host_id_for_delete = project.host_id.clone();
    let project_id_for_delete = project.project_id.clone();
    let on_delete = move |_| {
        let host_id = host_id_for_delete.clone();
        let project_id = project_id_for_delete.clone();
        let state = expect_context::<AppState>();
        context_menu.set(None);
        let project_name = state
            .projects
            .get_untracked()
            .into_iter()
            .find_map(|info| {
                if info.host_id == host_id && info.project.id == project_id {
                    Some(info.project.name)
                } else {
                    None
                }
            })
            .unwrap_or_default();
        spawn_local(async move {
            let message = format!(
                "Delete project \"{project_name}\"? Sessions and history will be preserved without this project."
            );
            if !crate::bridge::confirm_dialog("Delete project", &message).await {
                return;
            }
            delete_project(&state, host_id, project_id);
        });
    };

    let host_id_for_remove_wb = project.host_id.clone();
    let project_id_for_remove_wb = project.project_id.clone();
    let on_remove_workbench = move |_| {
        let host_id = host_id_for_remove_wb.clone();
        let workbench_id = project_id_for_remove_wb.clone();
        let state = expect_context::<AppState>();
        context_menu.set(None);
        let workbench_name = state
            .projects
            .get_untracked()
            .into_iter()
            .find_map(|info| {
                if info.host_id == host_id && info.project.id == workbench_id {
                    Some(info.project.name)
                } else {
                    None
                }
            })
            .unwrap_or_default();
        if let Some((operation_stream, message)) =
            failed_workbench_removal(&state, &host_id, &workbench_id)
        {
            state
                .workbench_remove_prompt
                .set(Some(WorkbenchRemovePrompt {
                    host_id,
                    operation_stream,
                    project_id: workbench_id,
                    project_name: workbench_name,
                    message,
                }));
            return;
        }
        spawn_local(async move {
            let message = format!(
                "Remove workbench '{workbench_name}'? This will remove its git worktree and all associated agents, terminals, sessions, steering, team references, reviews, and workflow runs."
            );
            if !crate::bridge::confirm_dialog("Remove workbench", &message).await {
                return;
            }
            remove_workbench(&state, host_id, workbench_id, false);
        });
    };

    view! {
        <>
            <button class="context-menu-item" on:click=on_edit>"Edit project…"</button>
            <button class="context-menu-item" disabled=move || busy.get() on:click=on_rename>"Rename"</button>
            {move || (!is_workbench()).then(|| view! {
                <button class="context-menu-item" disabled=move || busy.get() on:click=on_new_workbench.clone()>
                    "New Workbench"
                </button>
                <button
                    class="context-menu-item"
                    on:click=on_delete.clone()
                    disabled=move || has_workbench_children.get() || busy.get()
                    title=move || if has_workbench_children.get() {
                        "Remove its workbenches first"
                    } else {
                        ""
                    }
                >
                    "Delete Project"
                </button>
                {move || has_workbench_children.get().then(|| view! {
                    <div class="context-menu-hint">"Remove its workbenches first"</div>
                })}
            })}
            {move || is_workbench().then(|| view! {
                <button
                    class="context-menu-item"
                    disabled=move || busy.get() && !recoverable_removal.get()
                    on:click=on_remove_workbench.clone()
                >
                    "Remove Workbench"
                </button>
            })}
        </>
    }
}

#[component]
fn WorkbenchCreateModal(
    prompt: WorkbenchCreatePrompt,
    workbench_prompt: RwSignal<Option<WorkbenchCreatePrompt>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let branch = RwSignal::new(String::new());
    let input_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if let Some(el) = input_ref.get() {
            let _ = el.focus();
        }
    });

    let close = move || workbench_prompt.set(None);

    let close_for_backdrop = close;
    let on_backdrop_click = move |_| close_for_backdrop();

    let close_for_cancel = close;
    let on_cancel = move |_| close_for_cancel();

    let state_for_submit = state.clone();
    let prompt_for_submit = prompt.clone();
    let submit = move || {
        let value = branch.get_untracked().trim().to_owned();
        if value.is_empty() {
            return;
        }
        create_workbench(
            &state_for_submit,
            prompt_for_submit.host_id.clone(),
            prompt_for_submit.parent_project_id.clone(),
            value,
        );
        workbench_prompt.set(None);
    };
    let submit_for_keydown = submit.clone();
    let submit_for_button = submit;

    let close_for_keydown = close;
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        ev.stop_propagation();
        match ev.key().as_str() {
            "Enter" => {
                ev.prevent_default();
                submit_for_keydown();
            }
            "Escape" => {
                ev.prevent_default();
                close_for_keydown();
            }
            _ => {}
        }
    };

    let title = format!("New workbench in {}", prompt.parent_name);

    view! {
        <>
            <div
                class="modal-backdrop"
                style="position: fixed; inset: 0; z-index: 1100; background: rgba(0,0,0,0.4);"
                on:click=on_backdrop_click
            />
            <div
                class="modal workbench-create-modal"
                style="position: fixed; left: 50%; top: 30%; transform: translateX(-50%); z-index: 1101;"
                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
            >
                <div class="modal-title">{title}</div>
                <div class="modal-body">
                    <crate::notices::ProjectOperationDetails host=prompt.host_id.clone() project=prompt.parent_project_id.clone() />
                    <label class="modal-label">"Branch name (a new git branch will be created):"</label>
                    <input
                        type="text"
                        class="modal-input"
                        node_ref=input_ref
                        spellcheck="false"
                        autocapitalize="none"
                        autocomplete="off"
                        prop:value=move || branch.get()
                        on:input=move |ev| branch.set(event_target_value(&ev))
                        on:keydown=on_keydown
                    />
                </div>
                <div class="modal-actions">
                    <button class="modal-button" on:click=on_cancel>"Cancel"</button>
                    <button
                        class="modal-button primary"
                        on:click=move |_| submit_for_button()
                        disabled=move || branch.get().trim().is_empty()
                    >
                        "Create Workbench"
                    </button>
                </div>
            </div>
        </>
    }
}

#[component]
pub(crate) fn WorkbenchRemoveModal(prompt: WorkbenchRemovePrompt) -> impl IntoView {
    let state = expect_context::<AppState>();
    let busy = {
        let state = state.clone();
        let prompt = prompt.clone();
        Memo::new(move |_| {
            crate::notices::project_mutation_pending(&state, &prompt.host_id, &prompt.project_id)
        })
    };
    let close = {
        let state = state.clone();
        move || state.workbench_remove_prompt.set(None)
    };
    let close_for_backdrop = close;
    let close_for_cancel = close;
    let close_for_keydown = close;

    let state_for_open = state.clone();
    let open_prompt = prompt.clone();
    let on_open = move |_| {
        state_for_open.switch_active_project(Some(ActiveProjectRef {
            host_id: open_prompt.host_id.clone(),
            project_id: open_prompt.project_id.clone(),
        }));
        state_for_open.workbench_remove_prompt.set(None);
    };

    let state_for_force = state.clone();
    let force_prompt = prompt.clone();
    let on_force = move |_| {
        if busy.get_untracked() {
            return;
        }
        state_for_force.workbench_remove_prompt.set(None);
        state_for_force.project_operations.update(|operations| {
            operations.remove(&(
                force_prompt.host_id.clone(),
                force_prompt.operation_stream.clone(),
            ));
        });
        remove_workbench(
            &state_for_force,
            force_prompt.host_id.clone(),
            force_prompt.project_id.clone(),
            true,
        );
    };

    let on_keydown = move |event: web_sys::KeyboardEvent| {
        if event.key() == "Escape" {
            event.prevent_default();
            close_for_keydown();
        }
    };

    view! {
        <>
            <div
                class="modal-backdrop"
                style="position: fixed; inset: 0; z-index: 1100; background: rgba(0,0,0,0.4);"
                on:click=move |_| close_for_backdrop()
            />
            <div
                class="modal workbench-create-modal workbench-remove-modal"
                role="alertdialog"
                aria-modal="true"
                aria-labelledby="workbench-remove-title"
                style="position: fixed; left: 50%; top: 24%; transform: translateX(-50%); z-index: 1101;"
                tabindex="-1"
                on:keydown=on_keydown
                on:click=|event: web_sys::MouseEvent| event.stop_propagation()
            >
                <div id="workbench-remove-title" class="modal-title">
                    "Review workbench removal"
                </div>
                <div class="modal-body">
                    <div class="modal-error">{prompt.message}</div>
                    <div class="modal-warning">
                        {format!(
                            "Deleting '{}' anyway permanently discards every uncommitted and untracked file in this workbench.",
                            prompt.project_name
                        )}
                    </div>
                </div>
                <div class="modal-actions">
                    <button class="modal-button" on:click=on_open>"Open Workbench"</button>
                    <button class="modal-button" on:click=move |_| close_for_cancel()>
                        "Cancel"
                    </button>
                    <button class="modal-button danger" disabled=move || busy.get() on:click=on_force>
                        "Delete Anyway"
                    </button>
                </div>
            </div>
        </>
    }
}

/// Produce a short lowercase tag for a project name.
///
/// - Multi-token names (split on non-alphanumerics) take the first char of each
///   token, up to 4.
/// - Single CamelCase tokens take the uppercase letters (`ThingDoStuff` -> `tds`).
/// - Otherwise fall back to the first 4 characters.
fn abbreviate(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "?".into();
    }

    let tokens: Vec<&str> = trimmed
        .split(|c: char| !c.is_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .collect();

    let raw: String = if tokens.len() > 1 {
        tokens
            .iter()
            .take(4)
            .filter_map(|token| token.chars().next())
            .collect()
    } else if let Some(token) = tokens.first() {
        let caps: String = token.chars().filter(|c| c.is_uppercase()).collect();
        if caps.chars().count() >= 2 {
            caps.chars().take(4).collect()
        } else {
            token.chars().take(4).collect()
        }
    } else {
        trimmed.chars().take(4).collect()
    };

    raw.to_lowercase()
}

fn drag_drop_placement(ev: &web_sys::DragEvent) -> DropPlacement {
    let Some(current_target) = ev.current_target() else {
        return DropPlacement::Before;
    };
    let Ok(element) = current_target.dyn_into::<web_sys::HtmlElement>() else {
        return DropPlacement::Before;
    };
    let midpoint = element.offset_height() / 2;
    if ev.offset_y() >= midpoint {
        DropPlacement::After
    } else {
        DropPlacement::Before
    }
}

#[component]
pub fn ProjectEditor() -> impl IntoView {
    let state = expect_context::<AppState>();
    let target = state.project_editor;
    view! {
        {move || target.get().map(|project| view! { <ProjectEditorDialog project /> })}
    }
}

#[component]
fn ProjectEditorDialog(project: ActiveProjectRef) -> impl IntoView {
    let state = expect_context::<AppState>();
    let info = {
        let state = state.clone();
        let target = project.clone();
        Memo::new(move |_| {
            state.projects.with(|projects| {
                projects
                    .iter()
                    .find(|info| {
                        info.host_id == target.host_id && info.project.id == target.project_id
                    })
                    .cloned()
            })
        })
    };
    let name = RwSignal::new(match info.get_untracked() {
        Some(info) => info.project.name,
        None => String::new(),
    });
    let can_edit_roots = {
        let state = state.clone();
        let project = project.clone();
        Memo::new(move |_| state.can_manage_project_roots(&project.host_id, &project.project_id))
    };
    let busy = {
        let state = state.clone();
        let project = project.clone();
        Memo::new(move |_| {
            crate::notices::project_mutation_pending(&state, &project.host_id, &project.project_id)
        })
    };
    let delete_blocked = {
        let state = state.clone();
        let project = project.clone();
        Memo::new(move |_| {
            if info.with(|info| {
                info.as_ref()
                    .is_some_and(|info| info.project.is_workbench())
            }) {
                Some("Use Remove Workbench in the project menu to remove this workbench.")
            } else if state.projects.with(|projects| {
                projects.iter().any(|info| {
                    info.host_id == project.host_id
                        && info.project.parent_project_id() == Some(&project.project_id)
                })
            }) {
                Some("Remove this project's workbenches before deleting the project.")
            } else {
                None
            }
        })
    };
    let confirming = RwSignal::new(false);
    let delete_state = state.clone();
    let delete_target = project.clone();
    let on_delete = move |_| {
        if confirming.get_untracked()
            || busy.get_untracked()
            || delete_blocked.get_untracked().is_some()
        {
            return;
        }
        let Some(info) = info.get_untracked() else {
            return;
        };
        confirming.set(true);
        let state = delete_state.clone();
        let target = delete_target.clone();
        spawn_local(async move {
            let message = format!(
                "Delete project \"{}\"? Source files stay on disk. Agent sessions and history are preserved and detached from the project. Project-scoped steering and reviews are removed, and team project references are cleared.",
                info.project.name
            );
            let confirmed = crate::bridge::try_confirm_dialog("Delete project", &message).await;
            confirming.try_set(false);
            match confirmed {
                Ok(true) => {
                    state.project_operations.update(|operations| {
                        operations.retain(|(host, _), operation| {
                            !(host == &target.host_id
                                && matches!(&operation.mutation, protocol::ProjectMutation::Delete(payload) if payload.id == target.project_id)
                                && matches!(operation.state, protocol::ProjectOperationState::Failed { .. }))
                        });
                    });
                    delete_project(&state, target.host_id, target.project_id);
                }
                Ok(false) => {}
                Err(error) => crate::notices::report_error(
                    crate::notices::NoticeScope::Project(target.host_id, target.project_id),
                    error,
                ),
            }
        });
    };
    let save_state = state.clone();
    let save_project = project.clone();
    let save = move |_| {
        rename_project(
            &save_state,
            save_project.host_id.clone(),
            save_project.project_id.clone(),
            name.get_untracked(),
        )
    };
    let add_state = state.clone();
    let add_project = project.clone();
    let close = state.project_editor;
    let roots_state = state.clone();
    let roots_project = project.clone();
    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    let input = NodeRef::<leptos::html::Input>::new();
    let previous_focus = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
    on_cleanup(move || {
        if let Some(element) = previous_focus {
            let _ = element.focus();
        }
    });
    let browser_dialog = state.browse_dialog;
    Effect::new(move |_| {
        if browser_dialog.with(|dialog| dialog.is_none())
            && let Some(input) = input.get()
        {
            let _ = input.focus();
        }
    });
    view! {
        <div class="modal-overlay project-editor-overlay" on:click=move |_| close.set(None)>
            <div class="workbench-create-modal project-editor" node_ref=dialog_ref role="dialog" aria-modal="true" aria-label="Edit project"
                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    ev.stop_propagation();
                    if ev.key() == "Escape" { close.set(None); }
                    if ev.key() == "Tab" {
                        let Some(dialog) = dialog_ref.get_untracked() else { return; };
                        let Ok(nodes) = dialog.query_selector_all("button:not([disabled]), input:not([disabled])") else { return; };
                        let Some(first) = nodes.item(0).and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok()) else { return; };
                        let Some(last) = nodes.item(nodes.length().saturating_sub(1)).and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok()) else { return; };
                        let active = web_sys::window().and_then(|w| w.document()).and_then(|d| d.active_element());
                        if ev.shift_key() && active.as_ref().is_some_and(|e| e.is_same_node(Some(&first))) { ev.prevent_default(); let _ = last.focus(); }
                        else if !ev.shift_key() && active.as_ref().is_some_and(|e| e.is_same_node(Some(&last))) { ev.prevent_default(); let _ = first.focus(); }
                    }
                }
            >
                <h2 class="modal-title">"Edit project"</h2>
                <div class="modal-body">
                    <crate::notices::InlineNotices scopes=vec![crate::notices::NoticeScope::Browser] />
                    <Show when=move || info.get().is_none()><p role="status">"Project is no longer available"</p></Show>
                    <label class="modal-label" for="project-editor-name">"Project name"</label>
                    <div class="project-editor-name-row">
                        <input id="project-editor-name" class="modal-input" node_ref=input prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev)) />
                        <button class="modal-button" disabled=move || busy.get() || name.get().trim().is_empty() || info.with(|info| info.as_ref().is_none_or(|info| info.project.name == name.get().trim())) on:click=save>"Save name"</button>
                    </div>
                    <h3>"Source roots"</h3>
                    <p class="project-roots-help">"Removing a source root disconnects it from this project. Files stay on disk."</p>
                    {move || info.get().map(|info| {
                        let roots = info.project.root_paths();
                        let count = roots.len();
                        roots.into_iter().map(|root| {
                            let remove_state = roots_state.clone();
                            let remove_project = roots_project.clone();
                            let root_label = root.0.clone();
                            let status_state = roots_state.clone();
                            let status_project = roots_project.project_id.clone();
                            let status_root = root.clone();
                            let status = move || status_state.file_tree.with(|trees| trees.get(&status_project).and_then(|roots| roots.iter().find(|entry| entry.root == status_root)).and_then(|entry| match &entry.status {
                                protocol::ProjectRootStatus::Available => None,
                                protocol::ProjectRootStatus::Unavailable { message } => Some(message.clone()),
                            }));
                            view! {
                                <div class="project-editor-root">
                                    <span>{root_label.clone()}<span class="project-root-error" role="status">{status}</span></span>
                                    <button class="modal-button" aria-label=format!("Remove source root {root_label}")
                                        disabled=move || busy.get() || !can_edit_roots.get() || count <= 1
                                        title=move || if count <= 1 { "A project must keep at least one source root" } else { "Remove root from project; keep files" }
                                        on:click=move |_| crate::actions::delete_project_root(&remove_state, remove_project.host_id.clone(), remove_project.project_id.clone(), root.clone())
                                    >"Remove"</button>
                                </div>
                            }
                        }).collect_view()
                    })}
                    <button class="modal-button" disabled=move || busy.get() || !can_edit_roots.get()
                        on:click=move |_| crate::components::host_browser::open_add_root_browser_for(&add_state, add_project.clone())>"Add source root…"</button>
                    <Show when=move || !can_edit_roots.get()>
                        <p>"Roots are managed by workbench creation while this project is a workbench or has workbenches."</p>
                    </Show>
                </div>
                <Show when=move || delete_blocked.get().is_some()>
                    <p id="project-delete-blocked" role="status">{move || delete_blocked.get()}</p>
                </Show>
                <crate::notices::ProjectOperationDetails host=project.host_id.clone() project=project.project_id.clone() />
                <div class="modal-actions">
                    <button class="modal-button danger" aria-describedby=move || delete_blocked.get().map(|_| "project-delete-blocked")
                        disabled=move || confirming.get() || busy.get() || info.get().is_none() || delete_blocked.get().is_some()
                        on:click=on_delete>"Delete project"</button>
                    <button class="modal-button primary" on:click=move |_| close.set(None)>"Done"</button>
                </div>
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    //! Component-level rendering tests for the project rail.
    //!
    //! Run with: `tools/run-wasm-tests.sh wasm_tests::` (the script handles
    //! chromedriver and `wasm-bindgen-cli` setup automatically — see
    //! CLAUDE.md).

    use super::*;
    use crate::state::{AppState, ProjectInfo, sort_project_infos};
    use host_config::{ConfiguredHost, HostTransportConfig};
    use leptos::mount::mount_to;
    use protocol::{
        GitBranchName, Project, ProjectId, ProjectRootPath, ProjectSource, WorkbenchRoot,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 320px; height: 600px; \
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

    fn local_host(id: &str, label: &str) -> ConfiguredHost {
        ConfiguredHost {
            id: id.to_owned(),
            label: label.to_owned(),
            transport: HostTransportConfig::LocalEmbedded,
            auto_connect: false,
        }
    }

    fn standalone(id: &str, name: &str, sort_order: u64, root: &str) -> Project {
        Project {
            id: ProjectId(id.to_owned()),
            name: name.to_owned(),
            sort_order,
            source: ProjectSource::Standalone {
                roots: vec![ProjectRootPath(root.to_owned())],
            },
        }
    }

    fn workbench(
        id: &str,
        name: &str,
        sort_order: u64,
        parent_id: &str,
        branch: &str,
        parent_root: &str,
        worktree_root: &str,
    ) -> Project {
        Project {
            id: ProjectId(id.to_owned()),
            name: name.to_owned(),
            sort_order,
            source: ProjectSource::GitWorkbench {
                environment: Default::default(),
                parent_project_id: ProjectId(parent_id.to_owned()),
                branch: GitBranchName(branch.to_owned()),
                roots: vec![WorkbenchRoot {
                    parent_root: ProjectRootPath(parent_root.to_owned()),
                    worktree_root: ProjectRootPath(worktree_root.to_owned()),
                }],
            },
        }
    }

    /// Collect labels of workbench rows specifically. The nested-children
    /// container has class `rail-workbench-children`; its descendant
    /// `.rail-label` nodes are workbench labels.
    fn workbench_labels(container: &HtmlElement) -> Vec<String> {
        let nodes = container
            .query_selector_all(".rail-workbench-children .rail-label")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| {
                nodes
                    .item(i)?
                    .text_content()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            })
            .collect()
    }

    /// Returns one item per top-level standalone group: a tuple of
    /// (parent_label, child_count). Walks `.rail-project-group` to identify
    /// each parent and counts its `.rail-workbench-children > .rail-project-row`
    /// descendants.
    fn group_summary(container: &HtmlElement) -> Vec<(String, usize)> {
        let groups = container.query_selector_all(".rail-project-group").unwrap();
        let mut out = Vec::new();
        for i in 0..groups.length() {
            let group = groups
                .item(i)
                .unwrap()
                .dyn_into::<web_sys::Element>()
                .expect(".rail-project-group is an Element");
            // First .rail-label is the parent's label.
            let parent_label = group
                .query_selector(".rail-label")
                .unwrap()
                .and_then(|el: web_sys::Element| el.text_content())
                .unwrap_or_default()
                .trim()
                .to_owned();
            let children = group
                .query_selector_all(".rail-workbench-children > .rail-project-row")
                .unwrap()
                .length() as usize;
            out.push((parent_label, children));
        }
        out
    }

    fn make_state_with_fixture() -> AppState {
        let state = AppState::new();
        state
            .sidebar_layout
            .update(|layouts| layouts.mode = crate::sidebar_layout::InterfaceMode::Dinosaur);
        state
            .configured_hosts
            .set(vec![local_host("host-a", "Local")]);
        let mut projects = vec![
            ProjectInfo {
                host_id: "host-a".to_owned(),
                project: standalone("p-tyde", "Tyde2", 0, "/tmp/tyde2"),
            },
            ProjectInfo {
                host_id: "host-a".to_owned(),
                project: workbench(
                    "wb-feat",
                    "feature-login",
                    0,
                    "p-tyde",
                    "feature-login",
                    "/tmp/tyde2",
                    "/tmp/tyde2--feature-login",
                ),
            },
            ProjectInfo {
                host_id: "host-a".to_owned(),
                project: workbench(
                    "wb-fix",
                    "bugfix-x",
                    1,
                    "p-tyde",
                    "bugfix-x",
                    "/tmp/tyde2",
                    "/tmp/tyde2--bugfix-x",
                ),
            },
            ProjectInfo {
                host_id: "host-a".to_owned(),
                project: standalone("p-orphan", "OrphanProj", 1, "/tmp/orphan"),
            },
        ];
        sort_project_infos(&mut projects);
        state.projects.set(projects);
        state
    }

    #[wasm_bindgen_test]
    async fn renders_two_top_level_with_nested_workbenches() {
        let container = make_container();
        let state = make_state_with_fixture();
        let mount_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <ProjectRail /> }
        });

        next_tick().await;

        let summary = group_summary(&container);
        assert_eq!(
            summary.len(),
            2,
            "expected exactly two top-level (standalone) groups, got: {summary:?}"
        );

        let parent_labels: Vec<&str> = summary.iter().map(|(label, _)| label.as_str()).collect();
        assert!(
            parent_labels.contains(&"Tyde2"),
            "expected 'Tyde2' as a top-level row, got: {parent_labels:?}"
        );
        assert!(
            parent_labels.contains(&"OrphanProj"),
            "expected 'OrphanProj' as a top-level row, got: {parent_labels:?}"
        );

        let tyde_children = summary
            .iter()
            .find(|(label, _)| label == "Tyde2")
            .map(|(_, count)| *count)
            .expect("Tyde2 group");
        assert_eq!(
            tyde_children, 2,
            "Tyde2 should have two workbench children, got {tyde_children}"
        );

        let orphan_children = summary
            .iter()
            .find(|(label, _)| label == "OrphanProj")
            .map(|(_, count)| *count)
            .expect("OrphanProj group");
        assert_eq!(
            orphan_children, 0,
            "OrphanProj should have no children, got {orphan_children}"
        );

        let workbenches = workbench_labels(&container);
        assert!(
            workbenches.contains(&"feature-login".to_owned()),
            "expected 'feature-login' workbench label rendered, got: {workbenches:?}"
        );
        assert!(
            workbenches.contains(&"bugfix-x".to_owned()),
            "expected 'bugfix-x' workbench label rendered, got: {workbenches:?}"
        );
        assert_eq!(
            container
                .query_selector_all("[title='Native workbench']")
                .unwrap()
                .length(),
            2,
        );
        state.projects.update(|projects| {
            let project = projects
                .iter_mut()
                .find(|info| info.project.id.0 == "wb-feat")
                .unwrap();
            let ProjectSource::GitWorkbench { environment, .. } = &mut project.project.source
            else {
                panic!("fixture must be a workbench");
            };
            *environment = protocol::WorkbenchEnvironment::Bubblewrap {
                manifest_path: protocol::HostAbsPath("/tmp/workbench/manifest.json".to_owned()),
                logical_root: ProjectRootPath("/workspace".to_owned()),
                additional_roots: Vec::new(),
                build_blockers: Vec::new(),
                support: Default::default(),
                lifecycle: Default::default(),
                initialization: protocol::WorkbenchInitialization::Cold,
            };
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[title='Bubblewrap · Ready (cold)']")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            container
                .query_selector_all("[title='Native workbench']")
                .unwrap()
                .length(),
            1
        );
        state.projects.update(|projects| {
            let project = projects
                .iter_mut()
                .find(|info| info.project.id.0 == "wb-feat")
                .unwrap();
            let ProjectSource::GitWorkbench {
                environment: protocol::WorkbenchEnvironment::Bubblewrap { initialization, .. },
                ..
            } = &mut project.project.source
            else {
                panic!("fixture must be a Bubblewrap workbench");
            };
            *initialization = protocol::WorkbenchInitialization::Snapshot {
                cohort: "fixture".to_owned(),
            };
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[title='Bubblewrap · Snapshot seeded']")
                .unwrap()
                .is_some()
        );
        assert!(
            container
                .query_selector("[title='Bubblewrap · Ready (cold)']")
                .unwrap()
                .is_none()
        );
        for (initialization, title) in [
            (
                protocol::WorkbenchInitialization::Prepared {
                    cohort: "prepared".into(),
                },
                "Bubblewrap · Baseline prepared (initial build)",
            ),
            (
                protocol::WorkbenchInitialization::Blocked {
                    message: "No supported build manifest".into(),
                },
                "Bubblewrap · Warm builds blocked: No supported build manifest",
            ),
        ] {
            state.projects.update(|projects| {
                let project = projects
                    .iter_mut()
                    .find(|info| info.project.id.0 == "wb-feat")
                    .unwrap();
                let ProjectSource::GitWorkbench {
                    environment:
                        protocol::WorkbenchEnvironment::Bubblewrap {
                            initialization: current,
                            ..
                        },
                    ..
                } = &mut project.project.source
                else {
                    panic!("Bubblewrap fixture")
                };
                *current = initialization;
            });
            next_tick().await;
            let tag = container
                .query_selector(".rail-tag[title^='Bubblewrap']")
                .unwrap()
                .unwrap();
            assert_eq!(tag.get_attribute("title").as_deref(), Some(title));
        }
    }

    fn open_workbench_menu(container: &HtmlElement, name: &str) {
        let buttons = container
            .query_selector_all(".project-rail button[title]")
            .unwrap();
        let row = (0..buttons.length())
            .filter_map(|index| buttons.item(index)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.get_attribute("title").as_deref() == Some(name))
            .unwrap_or_else(|| panic!("workbench {name}"));
        let event_options = web_sys::MouseEventInit::new();
        event_options.set_cancelable(true);
        event_options.set_bubbles(true);
        let event =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &event_options)
                .unwrap();
        row.dispatch_event(&event).unwrap();
    }

    fn button(container: &HtmlElement, text: &str) -> HtmlElement {
        let buttons = container.query_selector_all("button").unwrap();
        (0..buttons.length())
            .filter_map(|index| buttons.item(index))
            .filter_map(|node| node.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().unwrap_or_default().trim() == text)
            .expect("visible action")
    }

    fn operation_event(
        state: &AppState,
        seq: u64,
        stream: &str,
        mutation: protocol::ProjectMutation,
        status: protocol::ProjectOperationState,
    ) {
        crate::dispatch::dispatch_envelope(
            state,
            "host-a",
            protocol::Envelope::from_payload(
                protocol::StreamPath("/host/a".to_owned()),
                protocol::FrameKind::ProjectOperation,
                seq,
                &protocol::ProjectOperationPayload {
                    host_id: protocol::HostFilterId("local".to_owned()),
                    stream: protocol::StreamPath(stream.to_owned()),
                    mutation,
                    state: status,
                },
            )
            .unwrap(),
        );
    }

    #[wasm_bindgen_test]
    async fn adding_a_workbench_reactively_appears_in_nested_list() {
        use protocol::{ProjectMutation, ProjectOperationStage, ProjectOperationState};
        install_send_stub();
        // Hold bridge admission independently of the server's operation events.
        js_sys::eval(
            r#"
            window.__normal_project_invoke = window.__TAURI__.core.invoke;
            window.__create_entered = new Promise(resolve => window.__create_enter = resolve);
            window.__TAURI__.core.invoke = function(cmd, args) {
                if (cmd === 'send_host_line' && JSON.parse(args.line).kind === 'workbench_create') {
                    window.__create_enter();
                    return new Promise(resolve => window.__release_create = resolve);
                }
                return window.__normal_project_invoke(cmd, args);
            };
        "#,
        )
        .unwrap();
        let container = make_container();
        let state = make_state_with_fixture();
        crate::dispatch::clear_host_seqs("host-a");
        state.host_streams.update(|streams| {
            streams.insert(
                "host-a".to_owned(),
                protocol::StreamPath("/host/a".to_owned()),
            );
        });
        state.connection_statuses.update(|statuses| {
            statuses.insert(
                "host-a".to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        let mount_state = state.clone();
        let prompt = RwSignal::new(Some(WorkbenchCreatePrompt {
            host_id: "host-a".to_owned(),
            parent_project_id: ProjectId("p-tyde".to_owned()),
            parent_name: "Tyde2".to_owned(),
        }));
        let composer = crate::state::ComposerHandle::default();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <crate::components::header::Header /><ProjectRail />
                <crate::components::chat_input::ChatInput composer=composer.clone() />
                <ProjectEditor /><crate::components::host_browser::HostBrowser />
                {move || prompt.get().map(|value| view! { <WorkbenchCreateModal prompt=value workbench_prompt=prompt /> })}
            }
        });
        next_tick().await;
        let input = container
            .query_selector(".workbench-create-modal input")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        input.set_value("experiment");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        button(&container, "Create Workbench").click();
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(
            &js_sys::eval("window.__create_entered").unwrap(),
        ))
        .await
        .unwrap();
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-create-modal")
                .unwrap()
                .is_none(),
            "submission must close before bridge admission completes"
        );
        assert_eq!(
            workbench_labels(&container).len(),
            2,
            "no optimistic workbench row"
        );
        let orphan = container
            .query_selector("[title='OrphanProj']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        orphan.click();
        let textarea = container
            .query_selector("textarea")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        textarea.set_value("Unrelated chat draft");
        textarea
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        let mutation = ProjectMutation::CreateWorkbench(protocol::WorkbenchCreatePayload {
            parent_project_id: ProjectId("p-tyde".to_owned()),
            branch: GitBranchName("experiment".to_owned()),
            name: "experiment".to_owned(),
        });
        operation_event(
            &state,
            0,
            "/project-operation/create",
            mutation.clone(),
            ProjectOperationState::Admitted,
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Create workbench experiment in Tyde2")
        );
        assert!(!container.text_content().unwrap().contains("Queued"));
        assert!(
            container
                .query_selector(".browser-notices .inline-notice")
                .unwrap()
                .is_none(),
            "routine in-flight progress must not render global notice"
        );
        js_sys::eval("window.__release_create(); window.__TAURI__.core.invoke = window.__normal_project_invoke;").unwrap();
        next_tick().await;
        assert_eq!(
            workbench_labels(&container).len(),
            2,
            "transport completion is not domain completion"
        );
        operation_event(
            &state,
            1,
            "/project-operation/create",
            mutation.clone(),
            ProjectOperationState::Running {
                stage: ProjectOperationStage::PrepareBuilds,
            },
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Preparing or restoring a verified warm build baseline"),
            "preparing must not render an app-wide banner"
        );
        assert!(
            container
                .query_selector(".browser-notices .inline-notice")
                .unwrap()
                .is_none(),
            "preparing must not render an app-wide banner"
        );
        assert!(
            textarea.value() == "Unrelated chat draft",
            "unrelated draft must remain unchanged"
        );
        let header = container.query_selector("header").unwrap().unwrap();
        let header_bottom = header.get_bounding_client_rect().bottom();
        let rail_top = container
            .query_selector(".project-rail")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect()
            .top();
        assert_eq!(
            header_bottom, rail_top,
            "preparation must not insert a strip between header and navigation"
        );
        assert!(
            container
                .query_selector(".project-operation-notices")
                .unwrap()
                .is_none(),
            "the global workbench banner must not exist"
        );
        open_workbench_menu(&container, "Tyde2");
        next_tick().await;
        assert!(
            button(&container, "New Workbench").has_attribute("disabled"),
            "preparation still owns its pending actions"
        );
        container
            .query_selector(".rail-context-backdrop")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let project = workbench(
            "wb-new",
            "experiment",
            2,
            "p-tyde",
            "experiment",
            "/tmp/tyde2",
            "/tmp/tyde2--experiment",
        );
        crate::dispatch::dispatch_envelope(
            &state,
            "host-a",
            protocol::Envelope::from_payload(
                protocol::StreamPath("/host/a".to_owned()),
                protocol::FrameKind::ProjectNotify,
                2,
                &protocol::ProjectNotifyPayload::Upsert { project },
            )
            .unwrap(),
        );
        next_tick().await;
        assert!(workbench_labels(&container).contains(&"experiment".to_owned()));
        assert_eq!(
            state.active_project.get_untracked().unwrap().project_id.0,
            "p-orphan",
            "late create must never steal navigation"
        );
        operation_event(
            &state,
            3,
            "/project-operation/create",
            mutation.clone(),
            ProjectOperationState::Committed {
                warnings: vec![protocol::ProjectOperationWarning {
                    stage: ProjectOperationStage::Workflow,
                    message: "Workflow discovery unavailable".to_owned(),
                }],
            },
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Saved, with warnings"),
            "a committed warning must not paint an app-wide notice"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Do not repeat this operation")
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Workflow discovery unavailable")
        );
        assert!(
            container
                .query_selector(".workbench-create-modal")
                .unwrap()
                .is_none()
        );
        assert!(
            textarea.value() == "Unrelated chat draft",
            "unrelated draft must remain unchanged"
        );
        // A different attempt has its own server identity; the older result cannot settle it.
        operation_event(
            &state,
            4,
            "/project-operation/another",
            mutation.clone(),
            ProjectOperationState::Admitted,
        );
        operation_event(
            &state,
            5,
            "/project-operation/create",
            mutation.clone(),
            ProjectOperationState::Committed { warnings: vec![] },
        );
        next_tick().await;
        assert!(
            !container.text_content().unwrap().contains("Queued"),
            "routine in-flight admission must not appear in global notices"
        );
        state.prepare_host_refresh("host-a");
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Completion is unknown"),
            "host refresh must not paint an app-wide notice"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Preparing or restoring a verified warm build baseline")
        );
        // An empty bootstrap removes p-orphan and disables Save because its
        // record is missing, not because another project's mutation is pending.
        let catalog = state.projects.with_untracked(|projects| {
            projects
                .iter()
                .filter(|entry| entry.host_id == "host-a")
                .map(|entry| entry.project.clone())
                .collect()
        });
        crate::dispatch::prime_host_with_bootstrap_for_tests(&state, "host-a", |bootstrap| {
            bootstrap.projects = catalog;
        });
        operation_event(
            &state,
            0,
            "/project-operation/another",
            mutation.clone(),
            ProjectOperationState::Running {
                stage: ProjectOperationStage::Persistence,
            },
        );
        next_tick().await;
        assert!(
            !container.text_content().unwrap().contains("Saving"),
            "routine saving stage must not appear in global notices"
        );
        assert!(
            container
                .query_selector("[title='OrphanProj']")
                .unwrap()
                .is_some(),
            "replayed catalog must keep the unrelated project available"
        );
        let branch_conflict = format!(
            "Branch already exists {}",
            "unbroken-diagnostic".repeat(1000)
        );
        operation_event(
            &state,
            1,
            "/project-operation/another",
            mutation,
            ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Conflict,
                message: branch_conflict.clone(),
                cleanup_may_be_partial: false,
            },
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Branch already exists"),
            "a failed create must not paint an app-wide notice"
        );
        assert!(state.project_operations.with(|operations| {
            operations.iter().any(|((host, _), operation)| {
                host == "host-a"
                    && matches!(
                        &operation.state,
                        ProjectOperationState::Failed { message, .. }
                            if message == &branch_conflict
                    )
            })
        }));
        assert!(
            container
                .query_selector(".workbench-create-modal")
                .unwrap()
                .is_none()
        );
        assert!(
            textarea.value() == "Unrelated chat draft",
            "unrelated draft must remain unchanged"
        );
        let orphan_project = ActiveProjectRef {
            host_id: "host-a".to_owned(),
            project_id: ProjectId("p-orphan".to_owned()),
        };
        state.project_editor.set(Some(orphan_project));
        next_tick().await;
        let name = container
            .query_selector("#project-editor-name")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        name.set_value("Edited orphan");
        name.dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(
            !button(&container, "Save name").has_attribute("disabled"),
            "another project's failure does not block this editor"
        );
        let root_mutation = ProjectMutation::AddRoot(protocol::ProjectAddRootPayload {
            id: ProjectId("p-orphan".to_owned()),
            root: ProjectRootPath("/tmp/added".to_owned()),
        });
        operation_event(
            &state,
            2,
            "/project-operation/root",
            root_mutation.clone(),
            ProjectOperationState::Running {
                stage: ProjectOperationStage::Persistence,
            },
        );
        next_tick().await;
        assert!(button(&container, "Save name").has_attribute("disabled"));
        assert!(button(&container, "Add source root…").has_attribute("disabled"));
        let roots = container
            .query_selector_all(".project-editor-root")
            .unwrap();
        assert_eq!(
            roots.length(),
            1,
            "no root may be inserted before the server catalog changes"
        );
        assert!(
            roots
                .item(0)
                .unwrap()
                .text_content()
                .unwrap()
                .contains("/tmp/orphan")
        );
        button(&container, "Done").click();
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Edit project']")
                .unwrap()
                .is_none(),
            "editor remains dismissible while mutation is held"
        );
        operation_event(
            &state,
            3,
            "/project-operation/root",
            root_mutation,
            ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Internal,
                message: "Could not save added root".to_owned(),
                cleanup_may_be_partial: false,
            },
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Could not save added root"),
            "a failed root change must not paint an app-wide notice"
        );
        assert!(
            container
                .query_selector("[aria-label='Folder browser']")
                .unwrap()
                .is_none(),
            "late add-root error cannot reopen a browser"
        );
        state.project_editor.set(Some(ActiveProjectRef {
            host_id: "host-a".to_owned(),
            project_id: ProjectId("p-orphan".to_owned()),
        }));
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Edit project']")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Could not save added root"),
            "closed-editor failures remain accessible in their project"
        );
        button(&container, "Done").click();
        next_tick().await;
        let mutations = [
            (
                ProjectMutation::Create(protocol::ProjectCreatePayload {
                    name: "New project".to_owned(),
                    roots: vec![ProjectRootPath("/tmp/new".to_owned())],
                }),
                "Create project New project",
            ),
            (
                ProjectMutation::DeleteRoot(protocol::ProjectDeleteRootPayload {
                    id: ProjectId("p-orphan".to_owned()),
                    root: ProjectRootPath("/tmp/orphan".to_owned()),
                }),
                "Remove /tmp/orphan from OrphanProj",
            ),
            (
                ProjectMutation::Delete(protocol::ProjectDeletePayload {
                    id: ProjectId("p-orphan".to_owned()),
                }),
                "Delete OrphanProj",
            ),
        ];
        for (index, (mutation, label)) in mutations.into_iter().enumerate() {
            let seq = 4 + index as u64 * 2;
            operation_event(
                &state,
                seq,
                &format!("/project-operation/other-{index}"),
                mutation.clone(),
                ProjectOperationState::Admitted,
            );
            next_tick().await;
            assert!(
                !container.text_content().unwrap().contains(label),
                "routine in-flight mutation must not appear in global notices"
            );
            assert!(
                container
                    .query_selector("[title='OrphanProj']")
                    .unwrap()
                    .is_some(),
                "catalog cannot disappear before server Delete"
            );
            operation_event(
                &state,
                seq + 1,
                &format!("/project-operation/other-{index}"),
                mutation,
                ProjectOperationState::Committed { warnings: vec![] },
            );
            next_tick().await;
        }
        // A closed form retains its actual unsent input on the error affordance.
        js_sys::eval("window.__TAURI__.core.invoke = (cmd,args) => cmd === 'send_host_line' && JSON.parse(args.line).kind === 'workbench_create' ? Promise.reject('Connection closed') : window.__normal_project_invoke(cmd,args);").unwrap();
        state.host_streams.update(|streams| {
            streams.insert(
                "host-a".to_owned(),
                protocol::StreamPath("/host/a".to_owned()),
            );
        });
        prompt.set(Some(WorkbenchCreatePrompt {
            host_id: "host-a".to_owned(),
            parent_project_id: ProjectId("p-tyde".to_owned()),
            parent_name: "Tyde2".to_owned(),
        }));
        next_tick().await;
        let input = container
            .query_selector(".workbench-create-modal input")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        input.set_value("recover-submitted-branch");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        button(&container, "Create Workbench").click();
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-create-modal")
                .unwrap()
                .is_none()
        );
        js_sys::eval("window.__TAURI__.core.invoke = window.__normal_project_invoke;").unwrap();
        crate::actions::delete_project(
            &state,
            "host-a".to_owned(),
            ProjectId("p-orphan".to_owned()),
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("could not be sent"),
            "a failed send must not paint an app-wide notice"
        );
        prompt.set(Some(WorkbenchCreatePrompt {
            host_id: "host-a".to_owned(),
            parent_project_id: ProjectId("p-tyde".to_owned()),
            parent_name: "Tyde2".to_owned(),
        }));
        next_tick().await;
        let modal = container
            .query_selector(".workbench-create-modal")
            .unwrap()
            .unwrap();
        assert!(
            modal
                .text_content()
                .unwrap()
                .contains("recover-submitted-branch"),
            "the scoped dialog must expose the unsent branch"
        );
        assert!(
            modal.text_content().unwrap().contains("Connection closed"),
            "the scoped dialog must expose the send failure"
        );
        let viewport = web_sys::window()
            .unwrap()
            .inner_height()
            .unwrap()
            .as_f64()
            .unwrap();
        assert!(
            button(&container, "Cancel")
                .get_bounding_client_rect()
                .bottom()
                <= viewport,
            "retained long errors must leave create controls reachable"
        );
        let diagnostics = modal
            .query_selector(".project-operation-details")
            .unwrap()
            .unwrap();
        assert!(
            diagnostics.scroll_height() > diagnostics.client_height(),
            "the complete long diagnostic stays scrollable"
        );
        button(&container, "Retry").click();
        next_tick().await;
        button(&container, "Cancel").click();
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-create-modal")
                .unwrap()
                .is_none()
        );
        assert!(js_sys::eval(r#"window.__test_send_calls.some(([cmd,args]) => cmd === 'send_host_line' && JSON.parse(JSON.parse(args).line).kind === 'workbench_create' && JSON.parse(JSON.parse(args).line).payload.branch === 'recover-submitted-branch')"#).unwrap().as_bool().unwrap());
        operation_event(
            &state,
            10,
            "/project-operation/open-failed",
            ProjectMutation::Create(protocol::ProjectCreatePayload {
                name: "Unavailable project".to_owned(),
                roots: vec![ProjectRootPath("/tmp/unavailable".to_owned())],
            }),
            ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Internal,
                message: "Could not open project".to_owned(),
                cleanup_may_be_partial: false,
            },
        );
        operation_event(
            &state,
            11,
            "/project-operation/reorder-failed",
            ProjectMutation::Reorder(protocol::ProjectReorderPayload {
                scope: protocol::ProjectReorderScope::TopLevel,
                project_ids: vec![
                    ProjectId("p-tyde".to_owned()),
                    ProjectId("p-orphan".to_owned()),
                ],
            }),
            ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Internal,
                message: "Could not reorder projects".to_owned(),
                cleanup_may_be_partial: false,
            },
        );
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Could not open project")
        );
        crate::components::host_browser::open_project_browser_for_host(&state, "host-a");
        next_tick().await;
        let browser = container
            .query_selector("[aria-label='Folder browser']")
            .unwrap()
            .unwrap();
        assert!(
            browser
                .text_content()
                .unwrap()
                .contains("Could not open project")
        );
        assert!(
            browser
                .text_content()
                .unwrap()
                .contains("Could not reorder projects")
        );
        button(&container, "Cancel").click();
        next_tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Could not open project")
        );
        // A long error cannot take navigation/chat out of the viewport.
        operation_event(
            &state,
            12,
            "/project-operation/long",
            ProjectMutation::RemoveWorkbench(protocol::WorkbenchRemovePayload {
                id: ProjectId("wb-feat".to_owned()),
                force: false,
            }),
            ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Internal,
                message: "untracked file diagnostic ".repeat(1000),
                cleanup_may_be_partial: true,
            },
        );
        next_tick().await;
        let notices = container
            .query_selector(".browser-notices")
            .unwrap()
            .unwrap();
        let viewport = web_sys::window()
            .unwrap()
            .inner_height()
            .unwrap()
            .as_f64()
            .unwrap();
        assert_eq!(
            notices.client_height(),
            0,
            "a long removal failure must not take app-wide space"
        );
        assert!(
            !notices
                .text_content()
                .unwrap_or_default()
                .contains("untracked file diagnostic"),
            "a long removal failure must not paint an app-wide notice"
        );
        open_workbench_menu(&container, "feature-login");
        next_tick().await;
        button(&container, "Remove Workbench").click();
        next_tick().await;
        let modal = container
            .query_selector(".workbench-remove-modal")
            .unwrap()
            .unwrap();
        assert!(
            modal
                .text_content()
                .unwrap()
                .contains("Cleanup may be incomplete")
        );
        let cancel = button(&container, "Cancel");
        assert!(
            cancel.get_bounding_client_rect().bottom() <= viewport,
            "long diagnostics must leave confirmation controls reachable"
        );
        cancel.click();
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-remove-modal")
                .unwrap()
                .is_none()
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Cleanup may be incomplete"),
            "closing the dialog must not leave an app-wide notice"
        );
        assert!(state.project_operations.with(|operations| {
            operations.iter().any(|((host, _), operation)| {
                host == "host-a"
                    && matches!(
                        &operation.state,
                        ProjectOperationState::Failed {
                            cleanup_may_be_partial: true,
                            ..
                        }
                    )
            })
        }));
        assert!(
            textarea.value() == "Unrelated chat draft",
            "unrelated draft must remain unchanged"
        );
    }

    #[wasm_bindgen_test]
    async fn removing_a_workbench_reactively_disappears_from_nested_list() {
        let container = make_container();
        let state_signal = leptos::prelude::StoredValue::new(None::<AppState>);
        let _handle = mount_to(container.clone(), move || {
            let state = make_state_with_fixture();
            state_signal.set_value(Some(state.clone()));
            provide_context(state);
            view! { <ProjectRail /> }
        });

        next_tick().await;

        let labels_before = workbench_labels(&container);
        assert_eq!(
            labels_before.len(),
            2,
            "fixture should start with two workbenches, got: {labels_before:?}"
        );
        assert!(
            labels_before.contains(&"bugfix-x".to_owned()),
            "fixture should include 'bugfix-x' before removal, got: {labels_before:?}"
        );

        let state = state_signal
            .get_value()
            .expect("state should be captured by the mount closure");
        state.projects.update(|projects| {
            projects.retain(|info| info.project.id.0 != "wb-fix");
        });

        next_tick().await;

        let labels_after = workbench_labels(&container);
        assert_eq!(
            labels_after.len(),
            1,
            "expected exactly one workbench after removal, got: {labels_after:?}"
        );
        assert!(
            !labels_after.contains(&"bugfix-x".to_owned()),
            "'bugfix-x' label should be gone after removal, got: {labels_after:?}"
        );
        assert!(
            labels_after.contains(&"feature-login".to_owned()),
            "'feature-login' should still be rendered, got: {labels_after:?}"
        );

        let summary = group_summary(&container);
        let tyde_children = summary
            .iter()
            .find(|(label, _)| label == "Tyde2")
            .map(|(_, count)| *count)
            .expect("Tyde2 group");
        assert_eq!(
            tyde_children, 1,
            "Tyde2 should have one workbench child after removal, got {tyde_children}"
        );
    }

    #[wasm_bindgen_test]
    async fn dirty_remove_prompt_shows_actions_and_delete_anyway_sends_force() {
        install_send_stub();
        let container = make_container();
        let state_signal = leptos::prelude::StoredValue::new(None::<AppState>);
        let _handle = mount_to(container.clone(), move || {
            let state = make_state_with_fixture();
            state.host_streams.update(|streams| {
                streams.insert(
                    "host-a".to_owned(),
                    protocol::StreamPath("/host/a".to_owned()),
                );
            });
            crate::dispatch::clear_host_seqs("host-a");
            crate::components::header::reset_user_notice_for_tests();
            operation_event(
                &state,
                0,
                "/project-operation/remove-other",
                protocol::ProjectMutation::RemoveWorkbench(protocol::WorkbenchRemovePayload {
                    id: ProjectId("wb-fix".to_owned()),
                    force: false,
                }),
                protocol::ProjectOperationState::Running {
                    stage: protocol::ProjectOperationStage::Cleanup,
                },
            );
            operation_event(
                &state,
                1,
                "/project-operation/remove",
                protocol::ProjectMutation::RemoveWorkbench(protocol::WorkbenchRemovePayload {
                    id: ProjectId("wb-feat".to_owned()),
                    force: false,
                }),
                protocol::ProjectOperationState::Failed {
                    code: protocol::CommandErrorCode::Conflict,
                    message: "worktree roots are dirty:\n?? implementation.md".to_owned(),
                    cleanup_may_be_partial: false,
                },
            );
            state_signal.set_value(Some(state.clone()));
            provide_context(state);
            view! { <crate::components::header::Header /> <ProjectRail /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector(".workbench-remove-modal")
                .unwrap()
                .is_none(),
            "remote failure must not open a modal"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("?? implementation.md"),
            "a dirty removal must not paint an app-wide notice"
        );
        open_workbench_menu(&container, "feature-login");
        next_tick().await;
        button(&container, "Remove Workbench").click();
        next_tick().await;
        let modal = container
            .query_selector(".workbench-remove-modal")
            .unwrap()
            .expect("dirty removal modal");
        assert_eq!(modal.get_attribute("role").as_deref(), Some("alertdialog"));
        assert!(
            button(&container, "Delete Anyway").has_attribute("disabled"),
            "inspection must not allow force removal while another family operation is running"
        );
        assert!(
            modal
                .text_content()
                .unwrap_or_default()
                .contains("?? implementation.md")
        );
        assert!(
            modal.text_content().unwrap().contains("feature-login"),
            "the error must not target the older pending workbench"
        );
        assert!(
            container
                .query_selector(".user-notice-banner")
                .unwrap()
                .is_none(),
            "the removal dialog already owns this error"
        );
        let buttons = modal.query_selector_all("button").unwrap();
        let labels = (0..buttons.length())
            .filter_map(|index| buttons.item(index)?.text_content())
            .map(|label| label.trim().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["Open Workbench", "Cancel", "Delete Anyway"]);

        button(&container, "Cancel").click();
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-remove-modal")
                .unwrap()
                .is_none()
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("?? implementation.md"),
            "closing the dialog must not leave an app-wide notice"
        );
        open_workbench_menu(&container, "feature-login");
        next_tick().await;
        button(&container, "Remove Workbench").click();
        next_tick().await;
        button(&container, "Open Workbench").click();
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-remove-modal")
                .unwrap()
                .is_none()
        );
        let state = state_signal.get_value().expect("state");
        assert!(
            state.project_operations.with(|operations| {
                operations.iter().any(|((host, _), operation)| {
                    host == "host-a"
                        && matches!(
                            &operation.state,
                            protocol::ProjectOperationState::Failed { message, .. }
                                if message.contains("implementation.md")
                        )
                })
            }),
            "opening the workbench must keep the failed removal"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("?? implementation.md"),
            "inspection must not leave an app-wide notice"
        );
        // Verify that the other in-flight removal (wb-fix / bugfix-x) is deterministically
        // reflected in the user-visible UI via disabled pending actions on the workbench.
        let buttons = container
            .query_selector_all(".project-rail button[title]")
            .unwrap();
        let fix_button = (0..buttons.length())
            .filter_map(|i| buttons.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.get_attribute("title").as_deref() == Some("bugfix-x"))
            .expect("bugfix-x workbench button");
        let event_options = web_sys::MouseEventInit::new();
        event_options.set_cancelable(true);
        event_options.set_bubbles(true);
        let event =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &event_options)
                .unwrap();
        fix_button.dispatch_event(&event).unwrap();
        next_tick().await;

        let menu_buttons = container
            .query_selector_all(".context-menu button")
            .unwrap();
        let remove_btn = (0..menu_buttons.length())
            .filter_map(|i| {
                menu_buttons
                    .item(i)?
                    .dyn_into::<web_sys::HtmlButtonElement>()
                    .ok()
            })
            .find(|b| b.text_content().as_deref().map(str::trim) == Some("Remove Workbench"))
            .expect("Remove Workbench menu button");
        assert!(
            remove_btn.disabled(),
            "Remove Workbench action must be disabled while removal is in flight"
        );
        let rename_btn = (0..menu_buttons.length())
            .filter_map(|i| {
                menu_buttons
                    .item(i)?
                    .dyn_into::<web_sys::HtmlButtonElement>()
                    .ok()
            })
            .find(|b| b.text_content().as_deref().map(str::trim) == Some("Rename"))
            .expect("Rename menu button");
        assert!(
            rename_btn.disabled(),
            "Rename action must be disabled while removal is in flight"
        );
        container
            .query_selector(".rail-context-backdrop")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        operation_event(
            &state,
            2,
            "/project-operation/remove-other",
            protocol::ProjectMutation::RemoveWorkbench(protocol::WorkbenchRemovePayload {
                id: ProjectId("wb-fix".to_owned()),
                force: false,
            }),
            protocol::ProjectOperationState::Committed { warnings: vec![] },
        );
        next_tick().await;
        open_workbench_menu(&container, "feature-login");
        next_tick().await;
        button(&container, "Remove Workbench").click();
        next_tick().await;
        button(&container, "Delete Anyway").click();
        next_tick().await;
        next_tick().await;
        assert!(
            container
                .query_selector(".workbench-remove-modal")
                .unwrap()
                .is_none()
        );

        let forced = js_sys::eval(
            r#"
            (window.__test_send_calls || []).some(([cmd, args]) => {
                if (cmd !== "send_host_line") return false;
                const envelope = JSON.parse(JSON.parse(args).line);
                return envelope.kind === "workbench_remove"
                    && envelope.payload.id === "wb-feat"
                    && envelope.payload.force === true;
            })
            "#,
        )
        .expect("inspect forced removal")
        .as_bool()
        .unwrap_or(false);
        assert!(forced, "Delete Anyway must send force=true");
        assert!(
            !container.text_content().unwrap().contains("Cleaning up"),
            "routine operation progress must not appear in global header notices"
        );
        assert!(
            container
                .query_selector(".browser-notices .inline-notice")
                .unwrap()
                .is_none(),
            "routine in-flight operations must not render global notice banner"
        );
        let notices_el = container
            .query_selector(".browser-notices")
            .unwrap()
            .expect("project operation notices container");
        assert_eq!(
            notices_el.client_height(),
            0,
            "no empty container height or layout shift when nothing real visible"
        );
    }

    #[wasm_bindgen_test]
    async fn delete_of_active_workbench_falls_back_to_parent() {
        let container = make_container();
        let state_signal = leptos::prelude::StoredValue::new(None::<AppState>);
        let _handle = mount_to(container.clone(), move || {
            let state = make_state_with_fixture();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("wb-feat".to_owned()),
            }));
            state_signal.set_value(Some(state.clone()));
            provide_context(state);
            view! { <ProjectRail /> }
        });
        next_tick().await;

        let state = state_signal
            .get_value()
            .expect("state should be captured by the mount closure");

        let deleted = workbench(
            "wb-feat",
            "feature-login",
            0,
            "p-tyde",
            "feature-login",
            "/tmp/tyde2",
            "/tmp/tyde2--feature-login",
        );
        crate::dispatch::handle_project_delete(&state, "host-a", &deleted);

        let active = state.active_project.get_untracked();
        assert_eq!(
            active,
            Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("p-tyde".to_owned()),
            }),
            "deleting an active workbench should fall back to its parent, got {active:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn delete_of_active_standalone_falls_back_to_home() {
        let container = make_container();
        let state_signal = leptos::prelude::StoredValue::new(None::<AppState>);
        let _handle = mount_to(container.clone(), move || {
            let state = make_state_with_fixture();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("p-orphan".to_owned()),
            }));
            state_signal.set_value(Some(state.clone()));
            provide_context(state);
            view! { <ProjectRail /> }
        });
        next_tick().await;

        let state = state_signal
            .get_value()
            .expect("state should be captured by the mount closure");

        let deleted = standalone("p-orphan", "OrphanProj", 1, "/tmp/orphan");
        crate::dispatch::handle_project_delete(&state, "host-a", &deleted);

        let active = state.active_project.get_untracked();
        assert_eq!(
            active, None,
            "deleting an active standalone should fall back to home (None), got {active:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn delete_of_active_workbench_falls_back_to_home_when_parent_missing() {
        let container = make_container();
        let state_signal = leptos::prelude::StoredValue::new(None::<AppState>);
        let _handle = mount_to(container.clone(), move || {
            let state = make_state_with_fixture();
            state.projects.update(|projects| {
                projects.retain(|info| info.project.id.0 != "p-tyde");
            });
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("wb-feat".to_owned()),
            }));
            state_signal.set_value(Some(state.clone()));
            provide_context(state);
            view! { <ProjectRail /> }
        });
        next_tick().await;

        let state = state_signal
            .get_value()
            .expect("state should be captured by the mount closure");

        let deleted = workbench(
            "wb-feat",
            "feature-login",
            0,
            "p-tyde",
            "feature-login",
            "/tmp/tyde2",
            "/tmp/tyde2--feature-login",
        );
        crate::dispatch::handle_project_delete(&state, "host-a", &deleted);

        let active = state.active_project.get_untracked();
        assert_eq!(
            active, None,
            "orphan workbench delete should fall back to home, got {active:?}"
        );
    }

    /// Capture outbound `send_host_line` Tauri invokes so a test can inspect
    /// which frames were put on the wire.
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

    fn clear_send_calls() {
        js_sys::eval("window.__test_send_calls = [];").expect("clear send calls");
    }

    /// Stream paths of every `project_accessed` frame on the wire, in send order.
    fn project_accessed_streams() -> Vec<String> {
        let joined = js_sys::eval(
            r#"
            (function() {
                let streams = [];
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "project_accessed") streams.push(env.stream);
                }
                return streams.join(",");
            })()
            "#,
        )
        .expect("probe send calls")
        .as_string()
        .unwrap_or_default();
        if joined.is_empty() {
            Vec::new()
        } else {
            joined.split(',').map(|s| s.to_owned()).collect()
        }
    }

    #[wasm_bindgen_test]
    async fn project_rail_selection_and_new_agent_flow() {
        install_send_stub();
        let container = make_container();
        let state = make_state_with_fixture();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount);
            view! {
                <ProjectRail />
                <crate::components::center_zone::CenterZone />
            }
        });
        next_tick().await;

        let button = container
            .query_selector(".rail-project-row button")
            .unwrap()
            .expect("a project row should render a clickable button")
            .dyn_into::<HtmlElement>()
            .unwrap();
        button.click();
        next_tick().await;

        let streams = project_accessed_streams();
        assert!(
            streams.contains(&"/project/p-tyde".to_owned()),
            "clicking the first project row should send project_accessed on \
             /project/p-tyde, got {streams:?}"
        );

        for (name, project_id, expected_draft) in [
            ("OrphanProj", Some("p-orphan"), ""),
            ("feature-login", Some("wb-feat"), ""),
            ("feature-login", Some("wb-feat"), "Keep this existing draft"),
            ("Home", None, ""),
            ("Home", None, "Keep this existing draft"),
        ] {
            let buttons = container
                .query_selector_all(".project-rail button[title]")
                .unwrap();
            let target = (0..buttons.length())
                .filter_map(|i| buttons.item(i)?.dyn_into::<HtmlElement>().ok())
                .find(|button| button.get_attribute("title").as_deref() == Some(name))
                .expect("target project button");
            let selected_before_menu = state.active_project.get_untracked();
            clear_send_calls();
            let event_options = web_sys::MouseEventInit::new();
            event_options.set_cancelable(true);
            event_options.set_bubbles(true);
            let event =
                web_sys::MouseEvent::new_with_mouse_event_init_dict("contextmenu", &event_options)
                    .unwrap();
            assert!(
                !target.dispatch_event(&event).unwrap(),
                "the rail must suppress the browser's context menu"
            );
            next_tick().await;
            assert_eq!(
                state.active_project.get_untracked(),
                selected_before_menu,
                "opening the menu must not change the workspace"
            );
            let items = container
                .query_selector_all(".context-menu button")
                .unwrap();
            let new_agent = (0..items.length())
                .filter_map(|i| items.item(i)?.dyn_into::<HtmlElement>().ok())
                .find(|button| button.text_content().as_deref().map(str::trim) == Some("New Agent"))
                .expect("every workspace menu must offer New Agent");
            if project_id.is_none() {
                assert_eq!(items.length(), 1, "Home only offers New Agent");
            }
            new_agent.click();
            next_tick().await;
            assert!(
                container
                    .query_selector(".rail-context-menu")
                    .unwrap()
                    .is_none(),
                "launching closes the menu"
            );
            assert_eq!(
                state.active_project.get_untracked(),
                project_id.map(|id| ActiveProjectRef {
                    host_id: "host-a".to_owned(),
                    project_id: ProjectId(id.to_owned()),
                }),
                "launch targets the clicked workspace, not the previous selection"
            );
            if project_id.is_none() {
                assert!(
                    project_accessed_streams().is_empty(),
                    "Home launch must not access the previously selected project"
                );
            }
            let composer = container
                .query_selector(".chat-textarea")
                .unwrap()
                .expect("new agent opens a composer")
                .dyn_into::<web_sys::HtmlTextAreaElement>()
                .unwrap();
            // New Chat reuses an unsent draft; repeating the shortcut must not erase it.
            assert_eq!(composer.value(), expected_draft);
            assert!(container.text_content().unwrap().contains("New Chat"));
            composer.set_value("Keep this existing draft");
            composer
                .dispatch_event(&web_sys::Event::new("input").unwrap())
                .unwrap();
            next_tick().await;
        }
        let tabs = container
            .query_selector_all(".tab-strip-scroll button.tab")
            .unwrap();
        assert_eq!(
            tabs.length(),
            1,
            "repeated launches reuse the unsent draft instead of duplicating it"
        );
        tabs.item(0)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let composer = container
            .query_selector(".chat-textarea")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        assert_eq!(composer.value(), "Keep this existing draft");
    }

    #[wasm_bindgen_test]
    async fn navigating_home_does_not_send_project_accessed() {
        install_send_stub();
        let state = make_state_with_fixture();

        // Selecting a project emits exactly one project_accessed frame.
        state.switch_active_project(Some(ActiveProjectRef {
            host_id: "host-a".to_owned(),
            project_id: ProjectId("p-tyde".to_owned()),
        }));
        next_tick().await;
        assert_eq!(
            project_accessed_streams(),
            vec!["/project/p-tyde".to_owned()],
            "selecting a project should send one project_accessed frame"
        );

        // Switching to home (None) must not put any project_accessed on the wire.
        clear_send_calls();
        state.switch_active_project(None);
        next_tick().await;
        assert!(
            project_accessed_streams().is_empty(),
            "navigating home must not send project_accessed, got {:?}",
            project_accessed_streams()
        );
    }
}
