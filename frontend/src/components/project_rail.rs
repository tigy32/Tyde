use leptos::prelude::*;
use leptos::tachys::dom::event_target_value;
use protocol::{Project, ProjectSource};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::actions::{
    create_workbench, delete_project, remove_workbench, rename_project, reorder_projects,
};
use crate::components::host_browser::open_project_browser;
use crate::state::{ActiveProjectRef, AppState, TabContent};

#[derive(Clone, Debug, PartialEq, Eq)]
struct DraggedProject {
    host_id: String,
    project_id: protocol::ProjectId,
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
    host_id: String,
    project_id: protocol::ProjectId,
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
        <nav class=nav_class>
            <div class="rail-items">
                <button class="rail-toggle" on:click=toggle_expanded title=toggle_title>
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                        <polyline points="9 18 15 12 9 6"/>
                    </svg>
                </button>

                <button class=home_class on:click=go_home title="Home">
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
                                let host_id = host_id.clone();
                                state.projects.get()
                                    .into_iter()
                                    .filter(move |project| {
                                        project.host_id == host_id
                                            && matches!(
                                                project.project.source,
                                                ProjectSource::Standalone { .. }
                                            )
                                    })
                                    .map(|info| info.project.id)
                                    .collect::<Vec<_>>()
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
            let host_id = host_id.clone();
            let parent_id = parent_id.clone();
            let state = expect_context::<AppState>();
            state
                .projects
                .get()
                .into_iter()
                .filter(move |info| {
                    info.host_id == host_id && info.project.parent_project_id() == Some(&parent_id)
                })
                .map(|info| info.project.id)
                .collect::<Vec<_>>()
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
            state.projects.get().into_iter().find_map(|info| {
                if info.host_id == host_id_for_lookup && info.project.id == project_id_for_lookup {
                    Some(info.project)
                } else {
                    None
                }
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
    let on_drag_start = move |ev: web_sys::DragEvent| {
        if let Some(data_transfer) = ev.data_transfer() {
            data_transfer.set_effect_allowed("move");
            let _ = data_transfer.set_data("text/plain", &project_id_for_drag.0);
        }
        drop_target.set(None);
        dragged_project.set(Some(DraggedProject {
            host_id: host_id_for_drag.clone(),
            project_id: project_id_for_drag.clone(),
        }));
    };

    let host_id_for_dragover = host_id.clone();
    let project_id_for_dragover = project_id.clone();
    let on_drag_over = move |ev: web_sys::DragEvent| {
        let Some(active_drag) = dragged_project.get() else {
            return;
        };
        if active_drag.host_id != host_id_for_dragover
            || active_drag.project_id == project_id_for_dragover
        {
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
            host_id: host_id_for_ctx.clone(),
            project_id: project_id_for_ctx.clone(),
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
                <span class="rail-tag">{move || abbrev()}</span>
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

    let host_id_for_lookup = menu.host_id.clone();
    let project_id_for_lookup = menu.project_id.clone();
    let project_signal: Memo<Option<Project>> = {
        let state = state.clone();
        Memo::new(move |_| {
            state.projects.get().into_iter().find_map(|info| {
                if info.host_id == host_id_for_lookup && info.project.id == project_id_for_lookup {
                    Some(info.project)
                } else {
                    None
                }
            })
        })
    };

    let is_workbench = move || {
        project_signal
            .get()
            .is_some_and(|project| project.is_workbench())
    };

    let host_id_for_rename = menu.host_id.clone();
    let project_id_for_rename = menu.project_id.clone();
    let on_rename = move |_| {
        context_menu.set(None);
        editing_project.set(Some((
            host_id_for_rename.clone(),
            project_id_for_rename.clone(),
        )));
    };

    let host_id_for_new = menu.host_id.clone();
    let project_id_for_new = menu.project_id.clone();
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

    let host_id_for_delete = menu.host_id.clone();
    let project_id_for_delete = menu.project_id.clone();
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
                "Delete project \"{project_name}\"? Sessions and history are preserved on the server."
            );
            if !crate::bridge::confirm_dialog("Delete project", &message).await {
                return;
            }
            delete_project(&state, host_id, project_id);
        });
    };

    let host_id_for_remove_wb = menu.host_id.clone();
    let project_id_for_remove_wb = menu.project_id.clone();
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
        spawn_local(async move {
            let message =
                format!("Remove workbench '{workbench_name}'? This will remove the git worktree.");
            if !crate::bridge::confirm_dialog("Remove workbench", &message).await {
                return;
            }
            remove_workbench(&state, host_id, workbench_id);
        });
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
                <button class="context-menu-item" on:click=on_rename>"Rename"</button>
                {move || (!is_workbench()).then(|| view! {
                    <button class="context-menu-item" on:click=on_new_workbench.clone()>
                        "New Workbench"
                    </button>
                    <button class="context-menu-item" on:click=on_delete.clone()>
                        "Delete Project"
                    </button>
                })}
                {move || is_workbench().then(|| view! {
                    <button class="context-menu-item" on:click=on_remove_workbench.clone()>
                        "Remove Workbench"
                    </button>
                })}
            </div>
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
    let close_for_submit = close;
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
        close_for_submit();
    };
    let submit_for_keydown = submit.clone();
    let submit_for_button = submit;

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        ev.stop_propagation();
        match ev.key().as_str() {
            "Enter" => {
                ev.prevent_default();
                submit_for_keydown();
            }
            "Escape" => {
                ev.prevent_default();
                close_for_cancel();
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
        let _handle = mount_to(container.clone(), move || {
            let state = make_state_with_fixture();
            provide_context(state);
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
    }

    #[wasm_bindgen_test]
    async fn adding_a_workbench_reactively_appears_in_nested_list() {
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

        // Append a new workbench under p-tyde — the rail should pick it up
        // through its reactive derivation rather than a manual refresh.
        let state = state_signal
            .get_value()
            .expect("state should be captured by the mount closure");
        state.projects.update(|projects| {
            projects.push(ProjectInfo {
                host_id: "host-a".to_owned(),
                project: workbench(
                    "wb-new",
                    "experiment",
                    2,
                    "p-tyde",
                    "experiment",
                    "/tmp/tyde2",
                    "/tmp/tyde2--experiment",
                ),
            });
            sort_project_infos(projects);
        });

        next_tick().await;

        let labels_after = workbench_labels(&container);
        assert_eq!(
            labels_after.len(),
            3,
            "expected three workbenches after add, got: {labels_after:?}"
        );
        assert!(
            labels_after.contains(&"experiment".to_owned()),
            "expected 'experiment' workbench to appear, got: {labels_after:?}"
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
}

#[cfg(test)]
mod tests {
    use super::abbreviate;

    #[test]
    fn camel_case_uses_capitals() {
        assert_eq!(abbreviate("ThingDoStuff"), "tds");
        assert_eq!(abbreviate("MyCoolApp"), "mca");
    }

    #[test]
    fn multi_token_uses_initials() {
        assert_eq!(abbreviate("api-gateway"), "ag");
        assert_eq!(abbreviate("web_app_server"), "was");
    }

    #[test]
    fn single_lowercase_uses_prefix() {
        assert_eq!(abbreviate("agentflow"), "agen");
        assert_eq!(abbreviate("go"), "go");
    }

    #[test]
    fn single_capital_falls_back_to_prefix() {
        assert_eq!(abbreviate("Anthropic"), "anth");
    }

    #[test]
    fn empty_returns_placeholder() {
        assert_eq!(abbreviate(""), "?");
        assert_eq!(abbreviate("   "), "?");
    }
}
