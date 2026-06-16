use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use protocol::{AgentId, AgentOrigin, ProjectId};
use wasm_bindgen::JsCast;

use crate::components::agents_panel::{
    DerivedAgentState, backend_class, backend_label, derive_agent_state, relative_time,
    status_class, status_icon, status_label,
};
use crate::state::{
    ActiveAgentRef, AgentInfo, AgentMonitorKey, AppState, CompactionOldInfo, ProjectInfo,
    StreamingState, TabContent,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropPlacement {
    Before,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentMonitorDropTarget {
    key: AgentMonitorKey,
    placement: DropPlacement,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentMonitorRow {
    key: AgentMonitorKey,
    agent: AgentInfo,
    status: DerivedAgentState,
    host_label: String,
    project_label: String,
}

pub(crate) fn default_agent_monitor_order(
    agents: &[AgentInfo],
    streaming: &HashMap<AgentId, StreamingState>,
    turn_active: &HashMap<AgentId, bool>,
    compaction: &HashMap<AgentId, CompactionOldInfo>,
) -> Vec<AgentMonitorKey> {
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by(|left, right| {
        compare_agents_for_monitor(left, right, streaming, turn_active, compaction)
    });
    sorted
        .into_iter()
        .map(AgentMonitorKey::from_agent)
        .collect()
}

pub(crate) fn merge_agent_monitor_order(
    default_order: &[AgentMonitorKey],
    manual_order: &[AgentMonitorKey],
) -> Vec<AgentMonitorKey> {
    let live: HashSet<AgentMonitorKey> = default_order.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(default_order.len());

    for key in manual_order {
        if live.contains(key) && seen.insert(key.clone()) {
            merged.push(key.clone());
        }
    }
    for key in default_order {
        if seen.insert(key.clone()) {
            merged.push(key.clone());
        }
    }

    merged
}

pub(crate) fn reorder_agent_monitor_order(
    order: &mut Vec<AgentMonitorKey>,
    moved: &AgentMonitorKey,
    target: &AgentMonitorKey,
    place_after: bool,
) -> bool {
    if moved == target {
        return false;
    }
    let Some(from_index) = order.iter().position(|key| key == moved) else {
        return false;
    };
    let moved_key = order.remove(from_index);
    let Some(target_index) = order.iter().position(|key| key == target) else {
        order.insert(from_index.min(order.len()), moved_key);
        return false;
    };
    let insert_index = if place_after {
        target_index + 1
    } else {
        target_index
    };
    order.insert(insert_index.min(order.len()), moved_key);
    true
}

fn compare_agents_for_monitor(
    left: &AgentInfo,
    right: &AgentInfo,
    streaming: &HashMap<AgentId, StreamingState>,
    turn_active: &HashMap<AgentId, bool>,
    compaction: &HashMap<AgentId, CompactionOldInfo>,
) -> Ordering {
    let left_status = derive_agent_state(left, streaming, turn_active, compaction);
    let right_status = derive_agent_state(right, streaming, turn_active, compaction);
    monitor_status_rank(left_status)
        .cmp(&monitor_status_rank(right_status))
        .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
        .then_with(|| left.host_id.cmp(&right.host_id))
        .then_with(|| project_id_cmp(left.project_id.as_ref(), right.project_id.as_ref()))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.agent_id.0.cmp(&right.agent_id.0))
}

fn monitor_status_rank(status: DerivedAgentState) -> u8 {
    match status {
        DerivedAgentState::Initializing
        | DerivedAgentState::Thinking
        | DerivedAgentState::Compacting => 0,
        DerivedAgentState::Idle => 1,
        DerivedAgentState::Terminated => 2,
    }
}

fn project_id_cmp(left: Option<&ProjectId>, right: Option<&ProjectId>) -> Ordering {
    let left = left.map(|id| id.0.as_str()).unwrap_or("");
    let right = right.map(|id| id.0.as_str()).unwrap_or("");
    left.cmp(right)
}

fn host_label(host_labels: &HashMap<String, String>, host_id: &str) -> String {
    host_labels
        .get(host_id)
        .cloned()
        .unwrap_or_else(|| host_id.to_owned())
}

fn project_label(projects: &[ProjectInfo], agent: &AgentInfo) -> String {
    let Some(project_id) = agent.project_id.as_ref() else {
        return "No project".to_owned();
    };
    projects
        .iter()
        .find(|project| project.host_id == agent.host_id && project.project.id == *project_id)
        .map(|project| project.project.name.clone())
        .unwrap_or_else(|| project_id.0.clone())
}

fn origin_label(origin: AgentOrigin) -> &'static str {
    match origin {
        AgentOrigin::User => "User",
        AgentOrigin::AgentControl => "Agent control",
        AgentOrigin::SideQuestion => "Aside",
        AgentOrigin::BackendNative => "Native",
        AgentOrigin::TeamMember => "Team",
        AgentOrigin::Workflow => "Workflow",
    }
}

fn drag_drop_placement(ev: &web_sys::DragEvent) -> DropPlacement {
    let Some(current_target) = ev.current_target() else {
        return DropPlacement::Before;
    };
    let Ok(element) = current_target.dyn_into::<web_sys::HtmlElement>() else {
        return DropPlacement::Before;
    };
    let rect = element.get_bounding_client_rect();
    let y_in_row = f64::from(ev.client_y()) - rect.top();
    if y_in_row >= rect.height() / 2.0 {
        DropPlacement::After
    } else {
        DropPlacement::Before
    }
}

fn agent_name_for_key(state: &AppState, key: &AgentMonitorKey) -> String {
    state
        .agents
        .with_untracked(|agents| {
            agents
                .iter()
                .find(|agent| {
                    agent.host_id.as_str() == key.host_id.as_str()
                        && agent.agent_id.0.as_str() == key.agent_id.0.as_str()
                })
                .map(|agent| agent.name.clone())
        })
        .unwrap_or_else(|| key.agent_id.0.clone())
}

fn apply_manual_reorder(
    state: &AppState,
    visible_order: Vec<AgentMonitorKey>,
    moved: &AgentMonitorKey,
    target: &AgentMonitorKey,
    place_after: bool,
) -> bool {
    let mut changed = false;
    state.agent_monitor_order.update(|manual| {
        if manual.is_empty() || !manual.contains(moved) || !manual.contains(target) {
            *manual = visible_order;
        }
        changed = reorder_agent_monitor_order(manual, moved, target, place_after);
    });
    changed
}

fn apply_keyboard_move(
    state: &AppState,
    visible_order: Vec<AgentMonitorKey>,
    key: &AgentMonitorKey,
    move_down: bool,
) -> bool {
    let Some(index) = visible_order.iter().position(|candidate| candidate == key) else {
        return false;
    };
    let target_index = if move_down {
        index
            .checked_add(1)
            .filter(|idx| *idx < visible_order.len())
    } else {
        index.checked_sub(1)
    };
    let Some(target_index) = target_index else {
        return false;
    };
    let target = visible_order[target_index].clone();
    apply_manual_reorder(state, visible_order, key, &target, move_down)
}

fn open_agent_chat(state: &AppState, agent: &AgentInfo) {
    state.open_tab(
        TabContent::chat_with_agent(ActiveAgentRef {
            host_id: agent.host_id.clone(),
            agent_id: agent.agent_id.clone(),
        }),
        agent.name.clone(),
        true,
    );
}

#[component]
pub fn AgentMonitorView() -> impl IntoView {
    let state = expect_context::<AppState>();
    let dragged_key = RwSignal::new(None::<AgentMonitorKey>);
    let drop_target = RwSignal::new(None::<AgentMonitorDropTarget>);
    let announcement = RwSignal::new(String::new());

    let order_state = state.clone();
    let visible_order: Memo<Vec<AgentMonitorKey>> = Memo::new(move |_| {
        order_state.streaming_text.with(|streaming| {
            order_state.agent_turn_active.with(|turn_active| {
                order_state.compaction_in_progress.with(|compaction| {
                    order_state.agents.with(|agents| {
                        let default =
                            default_agent_monitor_order(agents, streaming, turn_active, compaction);
                        let manual = order_state.agent_monitor_order.get();
                        merge_agent_monitor_order(&default, &manual)
                    })
                })
            })
        })
    });

    let prune_state = state.clone();
    Effect::new(move |_| {
        let live: HashSet<AgentMonitorKey> = prune_state
            .agents
            .get()
            .iter()
            .map(AgentMonitorKey::from_agent)
            .collect();
        let has_stale = prune_state
            .agent_monitor_order
            .with_untracked(|order| order.iter().any(|key| !live.contains(key)));
        if has_stale {
            prune_state.agent_monitor_order.update(|order| {
                order.retain(|key| live.contains(key));
            });
        }
    });

    let rows_state = state.clone();
    let rows: Memo<Vec<AgentMonitorRow>> = Memo::new(move |_| {
        let order = visible_order.get();
        let agents = rows_state.agents.get();
        let projects = rows_state.projects.get();
        let host_labels: HashMap<String, String> = rows_state
            .configured_hosts
            .get()
            .into_iter()
            .map(|host| (host.id, host.label))
            .collect();

        rows_state.streaming_text.with(|streaming| {
            rows_state.agent_turn_active.with(|turn_active| {
                rows_state.compaction_in_progress.with(|compaction| {
                    let agents_by_key: HashMap<AgentMonitorKey, AgentInfo> = agents
                        .iter()
                        .map(|agent| (AgentMonitorKey::from_agent(agent), agent.clone()))
                        .collect();
                    order
                        .into_iter()
                        .filter_map(|key| {
                            let agent = agents_by_key.get(&key)?.clone();
                            Some(AgentMonitorRow {
                                key,
                                status: derive_agent_state(
                                    &agent,
                                    streaming,
                                    turn_active,
                                    compaction,
                                ),
                                host_label: host_label(&host_labels, &agent.host_id),
                                project_label: project_label(&projects, &agent),
                                agent,
                            })
                        })
                        .collect()
                })
            })
        })
    });

    let reset_state = state.clone();
    let on_reset = move |_| {
        reset_state.agent_monitor_order.set(Vec::new());
        announcement.set("Agent Monitor reset to default sort".to_owned());
    };

    let has_manual_order = {
        let state = state.clone();
        move || !state.agent_monitor_order.with(|order| order.is_empty())
    };

    view! {
        <div class="agent-monitor-view">
            <div class="agent-monitor-header">
                <div>
                    <h1 class="agent-monitor-title">"Agent Monitor"</h1>
                    <p class="agent-monitor-subtitle">
                        "Live agents across hosts and projects. Drag rows to arrange them; new agents stay below your manual order."
                    </p>
                </div>
                <div class="agent-monitor-header-actions">
                    <span class="agent-monitor-count">
                        {move || {
                            let count = rows.get().len();
                            if count == 1 {
                                "1 agent".to_owned()
                            } else {
                                format!("{count} agents")
                            }
                        }}
                    </span>
                    <button
                        type="button"
                        class="filter-toggle"
                        disabled=move || !has_manual_order()
                        on:click=on_reset
                        title="Reset Agent Monitor rows to the default sort"
                    >
                        "Reset sort"
                    </button>
                </div>
            </div>

            <div class="agent-monitor-live" aria-live="polite">
                {move || announcement.get()}
            </div>

            <div class="agent-monitor-body">
                {move || {
                    let current_rows = rows.get();
                    if current_rows.is_empty() {
                        view! {
                            <div class="agent-monitor-empty">
                                "No live agents yet"
                            </div>
                        }
                        .into_any()
                    } else {
                        view! {
                            <div class="agent-monitor-list">
                                <For
                                    each=move || rows.get()
                                    key=|row| row.key.clone()
                                    let:row
                                >
                                    <AgentMonitorRowView
                                        row=row
                                        visible_order=visible_order
                                        dragged_key=dragged_key
                                        drop_target=drop_target
                                        announcement=announcement
                                    />
                                </For>
                            </div>
                        }
                        .into_any()
                    }
                }}
            </div>
        </div>
    }
}

#[component]
fn AgentMonitorRowView(
    row: AgentMonitorRow,
    visible_order: Memo<Vec<AgentMonitorKey>>,
    dragged_key: RwSignal<Option<AgentMonitorKey>>,
    drop_target: RwSignal<Option<AgentMonitorDropTarget>>,
    announcement: RwSignal<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let key = row.key.clone();
    let agent = row.agent.clone();
    let name = row.agent.name.clone();
    let status = row.status;
    let backend = row.agent.backend_kind;
    let origin = row.agent.origin;
    let created = row.agent.created_at_ms;
    let host_label = row.host_label.clone();
    let project_label = row.project_label.clone();
    let key_attr = format!("{}:{}", key.host_id, key.agent_id.0);

    let row_class_key = key.clone();
    let row_class = move || {
        let mut class = String::from("agent-monitor-row");
        if dragged_key
            .get()
            .as_ref()
            .is_some_and(|dragged| dragged == &row_class_key)
        {
            class.push_str(" dragging");
        }
        if let Some(target) = drop_target.get()
            && target.key == row_class_key
        {
            class.push_str(match target.placement {
                DropPlacement::Before => " drop-before",
                DropPlacement::After => " drop-after",
            });
        }
        class
    };

    let state_for_open = state.clone();
    let agent_for_open = agent.clone();
    let on_open = move |_| {
        open_agent_chat(&state_for_open, &agent_for_open);
    };

    let handle_key = key.clone();
    let on_drag_start = move |ev: web_sys::DragEvent| {
        ev.stop_propagation();
        if let Some(data_transfer) = ev.data_transfer() {
            data_transfer.set_effect_allowed("move");
            let _ = data_transfer.set_data(
                "text/plain",
                &format!("{}:{}", handle_key.host_id, handle_key.agent_id.0),
            );
        }
        drop_target.set(None);
        dragged_key.set(Some(handle_key.clone()));
    };

    let key_for_dragover = key.clone();
    let on_drag_over = move |ev: web_sys::DragEvent| {
        let Some(active_drag) = dragged_key.get() else {
            return;
        };
        if active_drag == key_for_dragover {
            return;
        }
        ev.prevent_default();
        if let Some(data_transfer) = ev.data_transfer() {
            data_transfer.set_drop_effect("move");
        }
        drop_target.set(Some(AgentMonitorDropTarget {
            key: key_for_dragover.clone(),
            placement: drag_drop_placement(&ev),
        }));
    };

    let state_for_drop = state.clone();
    let key_for_drop = key.clone();
    let target_name_for_drop = name.clone();
    let on_drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        let Some(active_drag) = dragged_key.get() else {
            return;
        };
        dragged_key.set(None);
        drop_target.set(None);
        if active_drag == key_for_drop {
            return;
        }
        let placement = drag_drop_placement(&ev);
        let moved_name = agent_name_for_key(&state_for_drop, &active_drag);
        let placement_label = match placement {
            DropPlacement::Before => "before",
            DropPlacement::After => "after",
        };
        if apply_manual_reorder(
            &state_for_drop,
            visible_order.get_untracked(),
            &active_drag,
            &key_for_drop,
            matches!(placement, DropPlacement::After),
        ) {
            announcement.set(format!(
                "Moved {moved_name} {placement_label} {target_name_for_drop}"
            ));
        }
    };

    let on_drag_end = move |_| {
        dragged_key.set(None);
        drop_target.set(None);
    };

    let state_for_move_up = state.clone();
    let key_for_move_up = key.clone();
    let name_for_move_up = name.clone();
    let move_up = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        if apply_keyboard_move(
            &state_for_move_up,
            visible_order.get_untracked(),
            &key_for_move_up,
            false,
        ) {
            announcement.set(format!("Moved {name_for_move_up} up"));
        }
    };

    let state_for_move_down = state.clone();
    let key_for_move_down = key.clone();
    let name_for_move_down = name.clone();
    let move_down = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        if apply_keyboard_move(
            &state_for_move_down,
            visible_order.get_untracked(),
            &key_for_move_down,
            true,
        ) {
            announcement.set(format!("Moved {name_for_move_down} down"));
        }
    };

    let key_for_can_up = key.clone();
    let can_move_up = move || {
        visible_order
            .get()
            .iter()
            .position(|candidate| candidate == &key_for_can_up)
            .is_some_and(|index| index > 0)
    };
    let key_for_can_down = key.clone();
    let can_move_down = move || {
        visible_order
            .get()
            .iter()
            .position(|candidate| candidate == &key_for_can_down)
            .is_some_and(|index| index + 1 < visible_order.get().len())
    };

    let state_for_keydown = state.clone();
    let key_for_keydown = key.clone();
    let name_for_keydown = name.clone();
    let agent_for_keydown = agent.clone();
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.alt_key() {
            match ev.key().as_str() {
                "ArrowUp" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    if apply_keyboard_move(
                        &state_for_keydown,
                        visible_order.get_untracked(),
                        &key_for_keydown,
                        false,
                    ) {
                        announcement.set(format!("Moved {name_for_keydown} up"));
                    }
                }
                "ArrowDown" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    if apply_keyboard_move(
                        &state_for_keydown,
                        visible_order.get_untracked(),
                        &key_for_keydown,
                        true,
                    ) {
                        announcement.set(format!("Moved {name_for_keydown} down"));
                    }
                }
                _ => {}
            }
            return;
        }

        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            open_agent_chat(&state_for_keydown, &agent_for_keydown);
        }
    };

    let stop_click = |ev: web_sys::MouseEvent| ev.stop_propagation();
    let stop_keydown = |ev: web_sys::KeyboardEvent| ev.stop_propagation();

    view! {
        <div
            class=row_class
            data-agent-monitor-key=key_attr
            role="button"
            tabindex="0"
            on:click=on_open
            on:keydown=on_keydown
            on:dragover=on_drag_over
            on:drop=on_drop
        >
            <span
                class="agent-monitor-drag-handle"
                draggable="true"
                title="Drag to reorder"
                aria-hidden="true"
                on:dragstart=on_drag_start
                on:dragend=on_drag_end
                on:click=stop_click
            >
                "⋮⋮"
            </span>

            <div class="agent-monitor-status-cell">
                <span class=status_class(&status) title=status_label(&status)>
                    {status_icon(&status)}
                </span>
                <span class="agent-monitor-status-label">{status_label(&status)}</span>
            </div>

            <div class="agent-monitor-main">
                <div class="agent-monitor-name-line">
                    <span class="agent-monitor-name">{name.clone()}</span>
                    <span class={format!("{} agent-monitor-backend", backend_class(backend))}>
                        {backend_label(backend)}
                    </span>
                </div>
                <div class="agent-monitor-meta">
                    <span>{host_label}</span>
                    <span aria-hidden="true">"•"</span>
                    <span>{project_label}</span>
                    <span aria-hidden="true">"•"</span>
                    <span>{origin_label(origin)}</span>
                    <span aria-hidden="true">"•"</span>
                    <span>{relative_time(created)}</span>
                </div>
            </div>

            <div class="agent-monitor-actions">
                <button
                    type="button"
                    class="agent-monitor-move-btn"
                    title="Move up"
                    aria-label=format!("Move {name} up")
                    disabled=move || !can_move_up()
                    on:click=move_up
                    on:keydown=stop_keydown
                >
                    "↑"
                </button>
                <button
                    type="button"
                    class="agent-monitor-move-btn"
                    title="Move down"
                    aria-label=format!("Move {name} down")
                    disabled=move || !can_move_down()
                    on:click=move_down
                    on:keydown=stop_keydown
                >
                    "↓"
                </button>
                <button
                    type="button"
                    class="agent-monitor-open-btn"
                    on:click=move |ev| {
                        ev.stop_propagation();
                        open_agent_chat(&state, &agent);
                    }
                    on:keydown=stop_keydown
                >
                    "Open"
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{BackendKind, StreamPath};

    fn key(host: &str, agent: &str) -> AgentMonitorKey {
        AgentMonitorKey::new(host, AgentId(agent.to_owned()))
    }

    fn agent(
        name: &str,
        host: &str,
        project_id: Option<&str>,
        created_at_ms: u64,
        started: bool,
        fatal_error: bool,
    ) -> AgentInfo {
        AgentInfo {
            host_id: host.to_owned(),
            agent_id: AgentId(format!("agent-{name}")),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: project_id.map(|id| ProjectId(id.to_owned())),
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms,
            instance_stream: StreamPath(format!("/agent/{name}")),
            started,
            fatal_error: fatal_error.then(|| "failed".to_owned()),
        }
    }

    #[test]
    fn default_order_puts_active_then_newest_then_tie_breakers() {
        let idle_new = agent("idle-new", "b-host", Some("p2"), 300, true, false);
        let starting_old = agent("starting-old", "a-host", Some("p1"), 100, false, false);
        let thinking_mid = agent("thinking-mid", "a-host", Some("p1"), 200, true, false);
        let failed_newest = agent("failed-newest", "a-host", Some("p1"), 1_000, true, true);
        let idle_tie_a = agent("idle-tie-a", "a-host", Some("p1"), 300, true, false);

        let mut turn_active = HashMap::new();
        turn_active.insert(thinking_mid.agent_id.clone(), true);

        let order = default_agent_monitor_order(
            &[
                idle_new.clone(),
                starting_old.clone(),
                thinking_mid.clone(),
                failed_newest.clone(),
                idle_tie_a.clone(),
            ],
            &HashMap::new(),
            &turn_active,
            &HashMap::new(),
        );

        assert_eq!(
            order,
            vec![
                AgentMonitorKey::from_agent(&thinking_mid),
                AgentMonitorKey::from_agent(&starting_old),
                AgentMonitorKey::from_agent(&idle_tie_a),
                AgentMonitorKey::from_agent(&idle_new),
                AgentMonitorKey::from_agent(&failed_newest),
            ]
        );
    }

    #[test]
    fn merge_manual_order_freezes_known_rows_and_appends_new_default_rows() {
        let default = vec![key("h", "b"), key("h", "a"), key("h", "c"), key("h", "d")];
        let manual = vec![key("h", "c"), key("h", "a"), key("h", "b")];

        assert_eq!(
            merge_agent_monitor_order(&default, &manual),
            vec![key("h", "c"), key("h", "a"), key("h", "b"), key("h", "d")]
        );
    }

    #[test]
    fn merge_manual_order_drops_stale_and_duplicate_keys() {
        let default = vec![key("h", "a"), key("h", "b"), key("h", "c")];
        let manual = vec![
            key("h", "missing"),
            key("h", "c"),
            key("h", "c"),
            key("h", "a"),
        ];

        assert_eq!(
            merge_agent_monitor_order(&default, &manual),
            vec![key("h", "c"), key("h", "a"), key("h", "b")]
        );
    }

    #[test]
    fn reorder_moves_before_and_after_targets() {
        let mut order = vec![key("h", "a"), key("h", "b"), key("h", "c")];

        assert!(reorder_agent_monitor_order(
            &mut order,
            &key("h", "c"),
            &key("h", "a"),
            false,
        ));
        assert_eq!(order, vec![key("h", "c"), key("h", "a"), key("h", "b")]);

        assert!(reorder_agent_monitor_order(
            &mut order,
            &key("h", "c"),
            &key("h", "b"),
            true,
        ));
        assert_eq!(order, vec![key("h", "a"), key("h", "b"), key("h", "c")]);
    }

    #[test]
    fn reorder_ignores_missing_or_same_key() {
        let mut order = vec![key("h", "a"), key("h", "b")];
        assert!(!reorder_agent_monitor_order(
            &mut order,
            &key("h", "a"),
            &key("h", "a"),
            false,
        ));
        assert!(!reorder_agent_monitor_order(
            &mut order,
            &key("h", "missing"),
            &key("h", "a"),
            false,
        ));
        assert_eq!(order, vec![key("h", "a"), key("h", "b")]);
    }
}
