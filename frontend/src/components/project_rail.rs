use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::actions::{delete_project, reorder_projects};
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

#[component]
pub fn ProjectRail() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_connected = state.clone();
    let state_for_add = state.clone();
    let state_for_hosts = state.clone();
    let dragged_project = RwSignal::new(None::<DraggedProject>);
    let drop_target = RwSignal::new(None::<ProjectDropTarget>);

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
                        let state = state.clone();
                        let host_id = host.id.clone();
                        let host_label = host.label.clone();

                        let state_for_status = state.clone();
                        let host_id_for_status = host_id.clone();
                        let status = move || {
                            state_for_status.connection_statuses
                                .get()
                                .get(&host_id_for_status)
                                .cloned()
                                .unwrap_or(crate::state::ConnectionStatus::Disconnected)
                        };

                        let state_for_projects = state.clone();
                        let host_id_for_projects = host_id.clone();
                        let dragged_project_for_projects = dragged_project;
                        let drop_target_for_projects = drop_target;
                        let projects_view = move || {
                            let state = state_for_projects.clone();
                            let host_id_filter = host_id_for_projects.clone();
                            let dragged_project = dragged_project_for_projects;
                            let drop_target = drop_target_for_projects;
                            state.projects.get()
                                .into_iter()
                                .filter(move |project| project.host_id == host_id_filter)
                                .map(|project_info| {
                                    let state = state.clone();
                                    let host_id = project_info.host_id.clone();
                                    let host_id_for_class = host_id.clone();
                                    let project = project_info.project.clone();
                                    let project_id = project.id.clone();
                                    let project_id_for_class = project_id.clone();
                                    let item_class = move || {
                                        if state.active_project.get().as_ref().is_some_and(|active| {
                                            active.host_id == host_id_for_class && active.project_id == project_id_for_class
                                        }) {
                                            "rail-item rail-project active"
                                        } else {
                                            "rail-item rail-project"
                                        }
                                    };
                                    let dragged_project_for_row = dragged_project;
                                    let drop_target_for_row = drop_target;
                                    let host_id_for_row_class = host_id.clone();
                                    let project_id_for_row_class = project_id.clone();
                                    let row_class = move || {
                                        let mut class = "rail-project-row".to_string();
                                        if dragged_project_for_row.get().as_ref().is_some_and(|dragged| {
                                            dragged.host_id == host_id_for_row_class
                                                && dragged.project_id == project_id_for_row_class
                                        }) {
                                            class.push_str(" dragging");
                                        }
                                        if let Some(target) = drop_target_for_row.get()
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
                                    let state_for_remove = state.clone();
                                    let state_for_reorder = state.clone();
                                    let host_id_for_remove = host_id.clone();
                                    let host_id_for_drag = host_id.clone();
                                    let host_id_for_drop = host_id.clone();
                                    let host_id_for_dragover = host_id.clone();
                                    let project_id_for_remove = project_id.clone();
                                    let project_id_for_drag = project_id.clone();
                                    let project_id_for_drop = project_id.clone();
                                    let project_id_for_dragover = project_id.clone();
                                    let on_click = move |_| {
                                        state.switch_active_project(Some(ActiveProjectRef {
                                            host_id: host_id.clone(),
                                            project_id: project_id.clone(),
                                        }));
                                    };
                                    let on_remove = move |_| {
                                        delete_project(
                                            &state_for_remove,
                                            host_id_for_remove.clone(),
                                            project_id_for_remove.clone(),
                                        );
                                    };
                                    let dragged_project_for_start = dragged_project;
                                    let drop_target_for_start = drop_target;
                                    let on_drag_start = move |ev: web_sys::DragEvent| {
                                        if let Some(data_transfer) = ev.data_transfer() {
                                            data_transfer.set_effect_allowed("move");
                                            let _ =
                                                data_transfer.set_data("text/plain", &project_id_for_drag.0);
                                        }
                                        drop_target_for_start.set(None);
                                        dragged_project_for_start.set(Some(DraggedProject {
                                            host_id: host_id_for_drag.clone(),
                                            project_id: project_id_for_drag.clone(),
                                        }));
                                    };
                                    let dragged_project_for_over = dragged_project;
                                    let drop_target_for_over = drop_target;
                                    let on_drag_over = move |ev: web_sys::DragEvent| {
                                        let Some(active_drag) = dragged_project_for_over.get() else {
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
                                        drop_target_for_over.set(Some(ProjectDropTarget {
                                            host_id: host_id_for_dragover.clone(),
                                            project_id: project_id_for_dragover.clone(),
                                            placement: drag_drop_placement(&ev),
                                        }));
                                    };
                                    let dragged_project_for_drop = dragged_project;
                                    let drop_target_for_drop = drop_target;
                                    let on_drop = move |ev: web_sys::DragEvent| {
                                        ev.prevent_default();
                                        let Some(active_drag) = dragged_project_for_drop.get() else {
                                            return;
                                        };
                                        drop_target_for_drop.set(None);
                                        dragged_project_for_drop.set(None);
                                        if active_drag.host_id != host_id_for_drop
                                            || active_drag.project_id == project_id_for_drop
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
                                    let dragged_project_for_end = dragged_project;
                                    let drop_target_for_end = drop_target;
                                    let on_drag_end = move |_| {
                                        drop_target_for_end.set(None);
                                        dragged_project_for_end.set(None);
                                    };
                                    let abbrev = abbreviate(&project.name);
                                    let project_name = project.name.clone();
                                    let project_name_for_title = project_name.clone();
                                    view! {
                                        <div
                                            class=row_class
                                            draggable="true"
                                            on:dragstart=on_drag_start
                                            on:dragover=on_drag_over
                                            on:drop=on_drop
                                            on:dragend=on_drag_end
                                        >
                                            <button class=item_class title=project_name_for_title on:click=on_click>
                                                <span class="rail-tag">{abbrev}</span>
                                                <span class="rail-label">{project_name.clone()}</span>
                                            </button>
                                            <button class="rail-project-remove" title="Remove project" on:click=on_remove>
                                                "×"
                                            </button>
                                        </div>
                                    }
                                })
                                .collect_view()
                        };

                        view! {
                            <div class="rail-host-group">
                                <div class="rail-host-label" title=host_label.clone()>
                                    {host_label.clone()}
                                    <span class="rail-host-state">
                                        {move || match status() {
                                            crate::state::ConnectionStatus::Connected => "●",
                                            crate::state::ConnectionStatus::Connecting => "◐",
                                            crate::state::ConnectionStatus::Disconnected => "○",
                                            crate::state::ConnectionStatus::Error(_) => "!",
                                        }}
                                    </span>
                                </div>
                                {projects_view}
                            </div>
                        }
                    }
                </For>
            </div>

            <div class="rail-bottom">
                <button
                    class="rail-item rail-add"
                    title="New Project on Selected Host"
                    on:click=on_add_click
                    disabled=move || !connected.get()
                >
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="12" y1="5" x2="12" y2="19"/>
                        <line x1="5" y1="12" x2="19" y2="12"/>
                    </svg>
                </button>
            </div>
        </nav>
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
