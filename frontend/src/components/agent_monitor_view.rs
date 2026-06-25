use std::cmp::Ordering;
use std::collections::HashMap;

use leptos::prelude::*;
use protocol::{
    AgentGroupMode, AgentListDensity, AgentOrderKey, AgentOrigin, AgentProjectFilter,
    AgentSortMode, AgentStatusFilter, AgentsSmartViewsSnapshot, AgentsSmartViewsUpdate,
    AgentsViewFilters, AgentsViewPreferencesUpdate, BackendKind, HostFilterId, ProjectId,
    SmartView, SmartViewId,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::components::agents_panel::{
    DerivedAgentState, backend_class, backend_label, derive_agent_state, relative_time,
    status_class, status_icon, status_label,
};
use crate::state::{ActiveAgentRef, AgentInfo, AgentMonitorKey, AppState, ProjectInfo, TabContent};

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

/// Which Smart View name-input prompt is open in the switcher, if any. The
/// prompt is ephemeral interaction state (an open input box), not durable view
/// state — the saved/renamed view only exists once the server confirms it.
#[derive(Clone, Debug, PartialEq)]
enum SmartViewPrompt {
    SaveAs,
    Rename(SmartViewId),
}

#[derive(Clone, Debug, PartialEq)]
struct AgentMonitorGroup {
    /// `None` renders a flat list with no header (group mode `Flat`).
    label: Option<String>,
    rows: Vec<AgentMonitorRow>,
}

// ── Manual-order key mapping ────────────────────────────────────────────
//
// Manual order is stored server-side as `Vec<AgentOrderKey>`, keyed by
// `SessionId` whenever the agent has one and falling back to a transient
// (host, agent) key while it does not. These helpers translate between the
// durable key space and the live `AgentInfo` list.

pub(crate) fn agent_order_key(agent: &AgentInfo) -> AgentOrderKey {
    match &agent.session_id {
        Some(session_id) => AgentOrderKey::Session {
            session_id: session_id.clone(),
        },
        None => AgentOrderKey::TransientAgent {
            host_id: HostFilterId(agent.host_id.clone()),
            agent_id: agent.agent_id.clone(),
        },
    }
}

pub(crate) fn agent_matches_order_key(agent: &AgentInfo, key: &AgentOrderKey) -> bool {
    match key {
        AgentOrderKey::Session { session_id } => agent.session_id.as_ref() == Some(session_id),
        AgentOrderKey::TransientAgent { host_id, agent_id } => {
            host_id.0 == agent.host_id && agent_id == &agent.agent_id
        }
    }
}

/// Index of the first manual-order key this agent matches, or `None` when the
/// agent is not in the manual order (a freshly spawned agent).
pub(crate) fn manual_rank(agent: &AgentInfo, manual_order: &[AgentOrderKey]) -> Option<usize> {
    manual_order
        .iter()
        .position(|key| agent_matches_order_key(agent, key))
}

// ── Filtering ───────────────────────────────────────────────────────────

pub(crate) fn status_to_filter(status: DerivedAgentState) -> AgentStatusFilter {
    match status {
        DerivedAgentState::Initializing => AgentStatusFilter::Initializing,
        DerivedAgentState::Thinking => AgentStatusFilter::Thinking,
        DerivedAgentState::Compacting => AgentStatusFilter::Compacting,
        DerivedAgentState::Idle => AgentStatusFilter::Idle,
        DerivedAgentState::Terminated => AgentStatusFilter::Terminated,
    }
}

/// Pure predicate for the Agents Center. An empty filter list means "no
/// constraint on this dimension". `hide_finished` drops terminated/fatal rows
/// (the only present-but-done lifecycle signal in Phase 1a). `query_lc` is the
/// ephemeral, never-persisted search box, lowercased.
pub(crate) fn agent_passes_view_filters(
    agent: &AgentInfo,
    status: DerivedAgentState,
    filters: &AgentsViewFilters,
    hide_finished: bool,
    query_lc: &str,
) -> bool {
    if hide_finished && status == DerivedAgentState::Terminated {
        return false;
    }
    if !filters.host_ids.is_empty() && !filters.host_ids.iter().any(|id| id.0 == agent.host_id) {
        return false;
    }
    if !filters.project_ids.is_empty() {
        let matches = agent.project_id.as_ref().is_some_and(|project_id| {
            filters
                .project_ids
                .iter()
                .any(|filter| filter.host_id.0 == agent.host_id && &filter.project_id == project_id)
        });
        if !matches {
            return false;
        }
    }
    if !filters.statuses.is_empty() && !filters.statuses.contains(&status_to_filter(status)) {
        return false;
    }
    if !filters.backends.is_empty() && !filters.backends.contains(&agent.backend_kind) {
        return false;
    }
    if !filters.origins.is_empty() && !filters.origins.contains(&agent.origin) {
        return false;
    }
    if !query_lc.is_empty() && !agent.name.to_lowercase().contains(query_lc) {
        return false;
    }
    true
}

// ── Sorting ─────────────────────────────────────────────────────────────

fn activity_cmp(left: &AgentMonitorRow, right: &AgentMonitorRow) -> Ordering {
    monitor_status_rank(left.status)
        .cmp(&monitor_status_rank(right.status))
        .then_with(|| right.agent.created_at_ms.cmp(&left.agent.created_at_ms))
        .then_with(|| left.agent.host_id.cmp(&right.agent.host_id))
        .then_with(|| {
            project_id_cmp(
                left.agent.project_id.as_ref(),
                right.agent.project_id.as_ref(),
            )
        })
        .then_with(|| left.agent.name.cmp(&right.agent.name))
        .then_with(|| left.agent.agent_id.0.cmp(&right.agent.agent_id.0))
}

/// Sort rows in place according to the active sort mode. For
/// `ManualThenActivity`, rows present in `manual_order` come first in stored
/// order; the rest fall back to the activity comparator and are appended.
fn sort_rows(
    rows: &mut [AgentMonitorRow],
    sort_mode: AgentSortMode,
    manual_order: &[AgentOrderKey],
) {
    match sort_mode {
        AgentSortMode::ManualThenActivity => rows.sort_by(|a, b| {
            match (
                manual_rank(&a.agent, manual_order),
                manual_rank(&b.agent, manual_order),
            ) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => activity_cmp(a, b),
            }
        }),
        AgentSortMode::NewestFirst => rows.sort_by(|a, b| {
            b.agent
                .created_at_ms
                .cmp(&a.agent.created_at_ms)
                .then_with(|| activity_cmp(a, b))
        }),
        AgentSortMode::OldestFirst => rows.sort_by(|a, b| {
            a.agent
                .created_at_ms
                .cmp(&b.agent.created_at_ms)
                .then_with(|| activity_cmp(a, b))
        }),
        AgentSortMode::NameAsc => rows.sort_by(|a, b| {
            a.agent
                .name
                .to_lowercase()
                .cmp(&b.agent.name.to_lowercase())
                .then_with(|| activity_cmp(a, b))
        }),
        AgentSortMode::Status => rows.sort_by(activity_cmp),
        AgentSortMode::Backend => rows.sort_by(|a, b| {
            backend_label(a.agent.backend_kind)
                .cmp(backend_label(b.agent.backend_kind))
                .then_with(|| activity_cmp(a, b))
        }),
        AgentSortMode::Project => rows.sort_by(|a, b| {
            a.project_label
                .cmp(&b.project_label)
                .then_with(|| activity_cmp(a, b))
        }),
    }
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

// ── Grouping ────────────────────────────────────────────────────────────

fn group_label_for(row: &AgentMonitorRow, group_mode: AgentGroupMode) -> Option<String> {
    match group_mode {
        AgentGroupMode::Flat => None,
        AgentGroupMode::Status => Some(status_label(&row.status).to_owned()),
        AgentGroupMode::Backend => Some(backend_label(row.agent.backend_kind).to_owned()),
        AgentGroupMode::Project => Some(row.project_label.clone()),
    }
}

/// Build display groups. Rows are sorted first, then bucketed into groups in
/// first-seen order so the within-group sort and a deterministic group order
/// are both preserved.
fn build_groups(
    mut rows: Vec<AgentMonitorRow>,
    sort_mode: AgentSortMode,
    group_mode: AgentGroupMode,
    manual_order: &[AgentOrderKey],
) -> Vec<AgentMonitorGroup> {
    sort_rows(&mut rows, sort_mode, manual_order);
    if matches!(group_mode, AgentGroupMode::Flat) {
        return vec![AgentMonitorGroup { label: None, rows }];
    }
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<AgentMonitorRow>> = HashMap::new();
    for row in rows {
        let label = group_label_for(&row, group_mode).unwrap_or_default();
        if !buckets.contains_key(&label) {
            order.push(label.clone());
        }
        buckets.entry(label).or_default().push(row);
    }
    order
        .into_iter()
        .map(|label| AgentMonitorGroup {
            rows: buckets.remove(&label).unwrap_or_default(),
            label: Some(label),
        })
        .collect()
}

// ── Flat key reordering (drag / keyboard) ───────────────────────────────

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

// ── Labels ──────────────────────────────────────────────────────────────

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

// ── Enum <-> select-value plumbing ──────────────────────────────────────

fn sort_mode_value(mode: AgentSortMode) -> &'static str {
    match mode {
        AgentSortMode::ManualThenActivity => "manual_then_activity",
        AgentSortMode::NewestFirst => "newest_first",
        AgentSortMode::OldestFirst => "oldest_first",
        AgentSortMode::NameAsc => "name_asc",
        AgentSortMode::Status => "status",
        AgentSortMode::Backend => "backend",
        AgentSortMode::Project => "project",
    }
}

fn sort_mode_from_value(value: &str) -> Option<AgentSortMode> {
    Some(match value {
        "manual_then_activity" => AgentSortMode::ManualThenActivity,
        "newest_first" => AgentSortMode::NewestFirst,
        "oldest_first" => AgentSortMode::OldestFirst,
        "name_asc" => AgentSortMode::NameAsc,
        "status" => AgentSortMode::Status,
        "backend" => AgentSortMode::Backend,
        "project" => AgentSortMode::Project,
        _ => return None,
    })
}

fn sort_mode_label(mode: AgentSortMode) -> &'static str {
    match mode {
        AgentSortMode::ManualThenActivity => "Manual, then activity",
        AgentSortMode::NewestFirst => "Newest first",
        AgentSortMode::OldestFirst => "Oldest first",
        AgentSortMode::NameAsc => "Name (A–Z)",
        AgentSortMode::Status => "Status",
        AgentSortMode::Backend => "Backend",
        AgentSortMode::Project => "Project",
    }
}

fn group_mode_value(mode: AgentGroupMode) -> &'static str {
    match mode {
        AgentGroupMode::Flat => "flat",
        AgentGroupMode::Status => "status",
        AgentGroupMode::Backend => "backend",
        AgentGroupMode::Project => "project",
    }
}

fn group_mode_from_value(value: &str) -> Option<AgentGroupMode> {
    Some(match value {
        "flat" => AgentGroupMode::Flat,
        "status" => AgentGroupMode::Status,
        "backend" => AgentGroupMode::Backend,
        "project" => AgentGroupMode::Project,
        _ => return None,
    })
}

fn group_mode_label(mode: AgentGroupMode) -> &'static str {
    match mode {
        AgentGroupMode::Flat => "No grouping",
        AgentGroupMode::Status => "Group by status",
        AgentGroupMode::Backend => "Group by backend",
        AgentGroupMode::Project => "Group by project",
    }
}

const SORT_MODES: [AgentSortMode; 7] = [
    AgentSortMode::ManualThenActivity,
    AgentSortMode::NewestFirst,
    AgentSortMode::OldestFirst,
    AgentSortMode::NameAsc,
    AgentSortMode::Status,
    AgentSortMode::Backend,
    AgentSortMode::Project,
];

const GROUP_MODES: [AgentGroupMode; 4] = [
    AgentGroupMode::Flat,
    AgentGroupMode::Status,
    AgentGroupMode::Backend,
    AgentGroupMode::Project,
];

const STATUS_FILTERS: [AgentStatusFilter; 5] = [
    AgentStatusFilter::Initializing,
    AgentStatusFilter::Thinking,
    AgentStatusFilter::Compacting,
    AgentStatusFilter::Idle,
    AgentStatusFilter::Terminated,
];

fn status_filter_label(filter: AgentStatusFilter) -> &'static str {
    match filter {
        AgentStatusFilter::Initializing => "Initializing",
        AgentStatusFilter::Thinking => "Thinking",
        AgentStatusFilter::Compacting => "Compacting",
        AgentStatusFilter::Idle => "Idle",
        AgentStatusFilter::Terminated => "Terminated",
    }
}

const BACKENDS: [BackendKind; 5] = [
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Tycode,
    BackendKind::Kiro,
    BackendKind::Antigravity,
];

const ORIGINS: [AgentOrigin; 6] = [
    AgentOrigin::User,
    AgentOrigin::AgentControl,
    AgentOrigin::SideQuestion,
    AgentOrigin::BackendNative,
    AgentOrigin::TeamMember,
    AgentOrigin::Workflow,
];

// ── Mutation plumbing ───────────────────────────────────────────────────

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

fn agent_for_key(state: &AppState, key: &AgentMonitorKey) -> Option<AgentInfo> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| {
                agent.host_id.as_str() == key.host_id.as_str()
                    && agent.agent_id.0.as_str() == key.agent_id.0.as_str()
            })
            .cloned()
    })
}

fn agent_name_for_key(state: &AppState, key: &AgentMonitorKey) -> String {
    agent_for_key(state, key)
        .map(|agent| agent.name)
        .unwrap_or_else(|| key.agent_id.0.clone())
}

/// Map a flat order of monitor keys to durable order keys, skipping any keys
/// whose live agent is no longer known.
fn manual_order_from_keys(state: &AppState, keys: &[AgentMonitorKey]) -> Vec<AgentOrderKey> {
    keys.iter()
        .filter_map(|key| agent_for_key(state, key))
        .map(|agent| agent_order_key(&agent))
        .collect()
}

/// Send a preference update to the primary local host. Caller installs the
/// optimistic overlay first; this only handles the wire send.
fn send_pref_update(state: &AppState, update: AgentsViewPreferencesUpdate) {
    let Some(host_id) = state.agents_view_preferences_host.get_untracked() else {
        log::warn!("agents-view preference change with no primary host; overlay only");
        return;
    };
    let Some(stream) = state.host_stream_untracked(&host_id) else {
        log::warn!("primary host {host_id} has no stream; preference overlay only");
        return;
    };
    spawn_local(async move {
        if let Err(error) = crate::send::set_agents_view_preferences(&host_id, stream, update).await
        {
            log::error!("failed to send agents-view preference update: {error}");
        }
    });
}

/// Send a Smart View mutation to the primary local host. Caller installs any
/// optimistic overlay first (only `SetActive` needs one); this only handles the
/// wire send. The server fans out a full `AgentsViewPreferencesNotify` that
/// reconciles the overlay and replaces the Smart View list.
fn send_smart_view_update(state: &AppState, update: AgentsSmartViewsUpdate) {
    let Some(host_id) = state.agents_view_preferences_host.get_untracked() else {
        log::warn!("smart-view change with no primary host; ignored");
        return;
    };
    let Some(stream) = state.host_stream_untracked(&host_id) else {
        log::warn!("primary host {host_id} has no stream; smart-view change ignored");
        return;
    };
    spawn_local(async move {
        if let Err(error) = crate::send::set_agents_smart_views(&host_id, stream, update).await {
            log::error!("failed to send smart-view update: {error}");
        }
    });
}

/// Optimistically reflect a Smart View selection then send `SetActive`. Copies
/// the view's query domains (`filters`, `sort_mode`, `group_mode`,
/// `hide_finished` — never search, density, or manual order, per dev-docs/26
/// §4.2/§4.4) plus the active view id into the overlay so the switcher and rows
/// update instantly; the next authoritative snapshot drops the overlay.
fn select_smart_view(state: &AppState, view: &SmartView) {
    let view = view.clone();
    let id = view.id.clone();
    state.set_agents_view_overlay(move |overlay| {
        overlay.filters = Some(view.filters);
        overlay.sort_mode = Some(view.sort_mode);
        overlay.group_mode = Some(view.group_mode);
        overlay.hide_finished = Some(view.hide_finished);
        overlay.active_view_id = Some(Some(view.id));
    });
    send_smart_view_update(state, AgentsSmartViewsUpdate::SetActive { id });
}

/// Optimistic `active_view_id` value after a direct query edit. Mirrors the
/// backend, which only clears `active_view_id` when the query actually diverges
/// from the active view's saved query: if the new query still equals the active
/// view's `filters`/`sort_mode`/`group_mode`/`hide_finished`, the highlight is
/// kept (e.g. Reset while already on the default-query "All" must not flash);
/// otherwise it clears. Returns the value to store in `overlay.active_view_id`.
fn active_view_after_query_edit(
    state: &AppState,
    filters: &AgentsViewFilters,
    sort_mode: AgentSortMode,
    group_mode: AgentGroupMode,
    hide_finished: bool,
) -> Option<SmartViewId> {
    let active_id = state.effective_active_smart_view_id()?;
    let snapshot = state.agents_view_preferences.get().smart_views;
    let still_matches = snapshot
        .built_in
        .iter()
        .chain(snapshot.user.iter())
        .find(|view| view.id == active_id)
        .is_some_and(|view| {
            &view.filters == filters
                && view.sort_mode == sort_mode
                && view.group_mode == group_mode
                && view.hide_finished == hide_finished
        });
    still_matches.then_some(active_id)
}

/// Move a user Smart View one slot left/right and send the full reordered id
/// list. Built-in ids are never included (only `user` views are reorderable).
/// The new order is applied when the server's full snapshot arrives, matching
/// the no-durable-local-list rule — there is no overlay for the view list.
fn reorder_user_view(
    state: &AppState,
    views: &AgentsSmartViewsSnapshot,
    id: &SmartViewId,
    move_left: bool,
) {
    let mut ids: Vec<SmartViewId> = views.user.iter().map(|view| view.id.clone()).collect();
    let Some(index) = ids.iter().position(|candidate| candidate == id) else {
        return;
    };
    let target = if move_left {
        index.checked_sub(1)
    } else {
        index.checked_add(1).filter(|next| *next < ids.len())
    };
    let Some(target) = target else {
        return;
    };
    ids.swap(index, target);
    send_smart_view_update(state, AgentsSmartViewsUpdate::Reorder { user_ids: ids });
}

/// Confirm with a native dialog, then delete a user Smart View. Uses the async
/// `confirm_dialog` bridge helper rather than `window.confirm`, which is a no-op
/// inside the Tauri webview (see CLAUDE.md). Deleting a view never touches
/// agents or sessions.
fn delete_user_view(state: &AppState, id: SmartViewId, name: String) {
    let state = state.clone();
    spawn_local(async move {
        let message =
            format!("Delete the \"{name}\" view? Your agents and sessions are not affected.");
        if crate::bridge::confirm_dialog("Delete Smart View", &message).await {
            send_smart_view_update(&state, AgentsSmartViewsUpdate::Delete { id });
        }
    });
}

/// Submit the open Smart View name prompt: `SaveCurrent` (server captures the
/// current query — never search/density/manual order) or `Rename`. A blank name
/// is ignored. Clears the prompt afterward.
fn submit_smart_view_prompt(
    state: &AppState,
    prompt: RwSignal<Option<SmartViewPrompt>>,
    prompt_text: RwSignal<String>,
) {
    let Some(open) = prompt.get_untracked() else {
        return;
    };
    let name = prompt_text.get_untracked().trim().to_owned();
    if name.is_empty() {
        return;
    }
    let update = match open {
        SmartViewPrompt::SaveAs => AgentsSmartViewsUpdate::SaveCurrent { name },
        SmartViewPrompt::Rename(id) => AgentsSmartViewsUpdate::Rename { id, name },
    };
    send_smart_view_update(state, update);
    prompt.set(None);
    prompt_text.set(String::new());
}

/// Install the optimistic overlay for a new manual order and send it.
fn apply_manual_reorder(
    state: &AppState,
    visible_keys: Vec<AgentMonitorKey>,
    moved: &AgentMonitorKey,
    target: &AgentMonitorKey,
    place_after: bool,
) -> bool {
    let mut keys = visible_keys;
    if !reorder_agent_monitor_order(&mut keys, moved, target, place_after) {
        return false;
    }
    let manual_order = manual_order_from_keys(state, &keys);
    state.set_agents_view_overlay(|overlay| overlay.manual_order = Some(manual_order.clone()));
    send_pref_update(
        state,
        AgentsViewPreferencesUpdate::SetManualOrder { manual_order },
    );
    true
}

fn apply_keyboard_move(
    state: &AppState,
    visible_keys: Vec<AgentMonitorKey>,
    key: &AgentMonitorKey,
    move_down: bool,
) -> bool {
    let Some(index) = visible_keys.iter().position(|candidate| candidate == key) else {
        return false;
    };
    let target_index = if move_down {
        index.checked_add(1).filter(|idx| *idx < visible_keys.len())
    } else {
        index.checked_sub(1)
    };
    let Some(target_index) = target_index else {
        return false;
    };
    let target = visible_keys[target_index].clone();
    apply_manual_reorder(state, visible_keys, key, &target, move_down)
}

fn toggle_in_vec<T: Clone + PartialEq>(items: &[T], value: &T) -> Vec<T> {
    if items.contains(value) {
        items
            .iter()
            .filter(|item| *item != value)
            .cloned()
            .collect()
    } else {
        let mut next = items.to_vec();
        next.push(value.clone());
        next
    }
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
    let search = RwSignal::new(String::new());
    let dragged_key = RwSignal::new(None::<AgentMonitorKey>);
    let drop_target = RwSignal::new(None::<AgentMonitorDropTarget>);
    let announcement = RwSignal::new(String::new());

    // Effective preferences: durable server snapshot + non-persisted overlay.
    let prefs_state = state.clone();
    let prefs = Memo::new(move |_| prefs_state.effective_agents_view_preferences());

    let rows_state = state.clone();
    let groups: Memo<Vec<AgentMonitorGroup>> = Memo::new(move |_| {
        let preferences = prefs.get();
        let query = search.get().to_lowercase();
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
                    let rows: Vec<AgentMonitorRow> = agents
                        .iter()
                        .filter_map(|agent| {
                            let status =
                                derive_agent_state(agent, streaming, turn_active, compaction);
                            if !agent_passes_view_filters(
                                agent,
                                status,
                                &preferences.filters,
                                preferences.hide_finished,
                                &query,
                            ) {
                                return None;
                            }
                            Some(AgentMonitorRow {
                                key: AgentMonitorKey::from_agent(agent),
                                status,
                                host_label: host_label(&host_labels, &agent.host_id),
                                project_label: project_label(&projects, agent),
                                agent: agent.clone(),
                            })
                        })
                        .collect();
                    build_groups(
                        rows,
                        preferences.sort_mode,
                        preferences.group_mode,
                        &preferences.manual_order,
                    )
                })
            })
        })
    });

    // Flat key order across all groups, for drag / keyboard reorder.
    let visible_keys: Memo<Vec<AgentMonitorKey>> = Memo::new(move |_| {
        groups
            .get()
            .into_iter()
            .flat_map(|group| group.rows.into_iter().map(|row| row.key))
            .collect()
    });

    let total_rows = move || visible_keys.get().len();

    let reset_state = state.clone();
    let on_reset = move |_| {
        // Optimistically show defaults, then send Reset; the notify confirms.
        // Keep the highlight if the default query still matches the active view
        // (e.g. resetting while already on the default-query "All").
        let active = active_view_after_query_edit(
            &reset_state,
            &AgentsViewFilters::default(),
            AgentSortMode::default(),
            AgentGroupMode::default(),
            false,
        );
        reset_state.set_agents_view_overlay(|overlay| {
            overlay.filters = Some(AgentsViewFilters::default());
            overlay.sort_mode = Some(AgentSortMode::default());
            overlay.group_mode = Some(AgentGroupMode::default());
            overlay.density = Some(AgentListDensity::default());
            overlay.hide_finished = Some(false);
            overlay.manual_order = Some(Vec::new());
            overlay.active_view_id = Some(active);
        });
        send_pref_update(&reset_state, AgentsViewPreferencesUpdate::Reset);
        announcement.set("Agents view reset to defaults".to_owned());
    };

    let sort_state = state.clone();
    let on_sort_change = move |ev: leptos::ev::Event| {
        let Some(mode) = sort_mode_from_value(&event_target_value(&ev)) else {
            return;
        };
        let prefs = sort_state.effective_agents_view_preferences();
        let active = active_view_after_query_edit(
            &sort_state,
            &prefs.filters,
            mode,
            prefs.group_mode,
            prefs.hide_finished,
        );
        sort_state.set_agents_view_overlay(|overlay| {
            overlay.sort_mode = Some(mode);
            overlay.active_view_id = Some(active);
        });
        send_pref_update(
            &sort_state,
            AgentsViewPreferencesUpdate::SetSortMode { sort_mode: mode },
        );
    };

    let group_state = state.clone();
    let on_group_change = move |ev: leptos::ev::Event| {
        let Some(mode) = group_mode_from_value(&event_target_value(&ev)) else {
            return;
        };
        let prefs = group_state.effective_agents_view_preferences();
        let active = active_view_after_query_edit(
            &group_state,
            &prefs.filters,
            prefs.sort_mode,
            mode,
            prefs.hide_finished,
        );
        group_state.set_agents_view_overlay(|overlay| {
            overlay.group_mode = Some(mode);
            overlay.active_view_id = Some(active);
        });
        send_pref_update(
            &group_state,
            AgentsViewPreferencesUpdate::SetGroupMode { group_mode: mode },
        );
    };

    let density_state = state.clone();
    let toggle_density = move |_| {
        let next = match density_state.effective_agents_view_preferences().density {
            AgentListDensity::Comfortable => AgentListDensity::Compact,
            AgentListDensity::Compact => AgentListDensity::Comfortable,
        };
        density_state.set_agents_view_overlay(|overlay| overlay.density = Some(next));
        send_pref_update(
            &density_state,
            AgentsViewPreferencesUpdate::SetDensity { density: next },
        );
    };

    let hide_state = state.clone();
    let toggle_hide_finished = move |_| {
        let prefs = hide_state.effective_agents_view_preferences();
        let next = !prefs.hide_finished;
        let active = active_view_after_query_edit(
            &hide_state,
            &prefs.filters,
            prefs.sort_mode,
            prefs.group_mode,
            next,
        );
        hide_state.set_agents_view_overlay(|overlay| {
            overlay.hide_finished = Some(next);
            overlay.active_view_id = Some(active);
        });
        send_pref_update(
            &hide_state,
            AgentsViewPreferencesUpdate::SetHideFinished {
                hide_finished: next,
            },
        );
    };

    let on_search = move |ev: leptos::ev::Event| search.set(event_target_value(&ev));

    let prefs_for_controls = prefs;
    let pending_state = state.clone();
    let sync_pending = move || pending_state.agents_view_overlay_pending();
    let list_class = move || {
        let density = prefs_for_controls.get().density;
        match density {
            AgentListDensity::Comfortable => "agent-monitor-list agents-density-comfortable",
            AgentListDensity::Compact => "agent-monitor-list agents-density-compact",
        }
    };

    let filter_state = state.clone();
    view! {
        <div class="agent-monitor-view">
            <div class="agent-monitor-header">
                <div>
                    <h1 class="agent-monitor-title">"Agents Center"</h1>
                    <p class="agent-monitor-subtitle">
                        "Live agents across hosts and projects. Filters, sort, grouping, and manual order are saved on the server and follow you across reconnects."
                    </p>
                </div>
                <div class="agent-monitor-header-actions">
                    <span class="agent-monitor-count">
                        {move || {
                            let count = total_rows();
                            if count == 1 { "1 agent".to_owned() } else { format!("{count} agents") }
                        }}
                    </span>
                    {move || {
                        sync_pending().then(|| {
                            view! { <span class="agent-monitor-sync" title="Saving view preferences">"Syncing…"</span> }
                        })
                    }}
                    <button
                        type="button"
                        class="filter-toggle"
                        on:click=on_reset
                        title="Reset all Agents view preferences to defaults"
                    >
                        "Reset view"
                    </button>
                </div>
            </div>

            <SmartViewSwitcher />

            <div class="agent-monitor-toolbar">
                <input
                    type="text"
                    class="panel-search-input agent-monitor-search"
                    placeholder="Search agents..."
                    prop:value=search
                    on:input=on_search
                    spellcheck="false"
                    autocapitalize="none"
                    autocomplete="off"
                />
                <select
                    class="agent-monitor-select"
                    data-test="agent-monitor-sort"
                    aria-label="Sort agents"
                    prop:value=move || sort_mode_value(prefs_for_controls.get().sort_mode)
                    on:change=on_sort_change
                >
                    {SORT_MODES
                        .iter()
                        .map(|mode| {
                            let mode = *mode;
                            view! {
                                <option value=sort_mode_value(mode)>{sort_mode_label(mode)}</option>
                            }
                        })
                        .collect_view()}
                </select>
                <select
                    class="agent-monitor-select"
                    data-test="agent-monitor-group"
                    aria-label="Group agents"
                    prop:value=move || group_mode_value(prefs_for_controls.get().group_mode)
                    on:change=on_group_change
                >
                    {GROUP_MODES
                        .iter()
                        .map(|mode| {
                            let mode = *mode;
                            view! {
                                <option value=group_mode_value(mode)>{group_mode_label(mode)}</option>
                            }
                        })
                        .collect_view()}
                </select>
                <button
                    type="button"
                    class=move || {
                        if matches!(prefs_for_controls.get().density, AgentListDensity::Compact) {
                            "filter-toggle active"
                        } else {
                            "filter-toggle"
                        }
                    }
                    data-test="agent-monitor-density"
                    on:click=toggle_density
                >
                    "Compact"
                </button>
                <button
                    type="button"
                    class=move || {
                        if prefs_for_controls.get().hide_finished {
                            "filter-toggle active"
                        } else {
                            "filter-toggle"
                        }
                    }
                    data-test="agent-monitor-hide-finished"
                    on:click=toggle_hide_finished
                >
                    "Hide finished"
                </button>
            </div>

            <div class="agent-monitor-filters">
                <div class="agent-monitor-filter-group" data-test="agent-monitor-status-filters">
                    {STATUS_FILTERS
                        .iter()
                        .map(|filter| {
                            let filter = *filter;
                            let prefs = prefs_for_controls;
                            let chip_state = filter_state.clone();
                            let active = move || prefs.get().filters.statuses.contains(&filter);
                            let on_click = move |_| {
                                let prefs = chip_state.effective_agents_view_preferences();
                                let mut filters = prefs.filters.clone();
                                filters.statuses = toggle_in_vec(&filters.statuses, &filter);
                                let active = active_view_after_query_edit(
                                    &chip_state,
                                    &filters,
                                    prefs.sort_mode,
                                    prefs.group_mode,
                                    prefs.hide_finished,
                                );
                                chip_state
                                    .set_agents_view_overlay(|overlay| {
                                        overlay.filters = Some(filters.clone());
                                        overlay.active_view_id = Some(active);
                                    });
                                send_pref_update(
                                    &chip_state,
                                    AgentsViewPreferencesUpdate::SetFilters { filters },
                                );
                            };
                            view! {
                                <button
                                    type="button"
                                    class=move || if active() { "filter-toggle active" } else { "filter-toggle" }
                                    on:click=on_click
                                >
                                    {status_filter_label(filter)}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <div class="agent-monitor-filter-group" data-test="agent-monitor-backend-filters">
                    {BACKENDS
                        .iter()
                        .map(|backend| {
                            let backend = *backend;
                            let prefs = prefs_for_controls;
                            let chip_state = filter_state.clone();
                            let active = move || prefs.get().filters.backends.contains(&backend);
                            let on_click = move |_| {
                                let prefs = chip_state.effective_agents_view_preferences();
                                let mut filters = prefs.filters.clone();
                                filters.backends = toggle_in_vec(&filters.backends, &backend);
                                let active = active_view_after_query_edit(
                                    &chip_state,
                                    &filters,
                                    prefs.sort_mode,
                                    prefs.group_mode,
                                    prefs.hide_finished,
                                );
                                chip_state
                                    .set_agents_view_overlay(|overlay| {
                                        overlay.filters = Some(filters.clone());
                                        overlay.active_view_id = Some(active);
                                    });
                                send_pref_update(
                                    &chip_state,
                                    AgentsViewPreferencesUpdate::SetFilters { filters },
                                );
                            };
                            view! {
                                <button
                                    type="button"
                                    class=move || if active() { "filter-toggle active" } else { "filter-toggle" }
                                    on:click=on_click
                                >
                                    {backend_label(backend)}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <div class="agent-monitor-filter-group" data-test="agent-monitor-origin-filters">
                    {ORIGINS
                        .iter()
                        .map(|origin| {
                            let origin = *origin;
                            let prefs = prefs_for_controls;
                            let chip_state = filter_state.clone();
                            let active = move || prefs.get().filters.origins.contains(&origin);
                            let on_click = move |_| {
                                let prefs = chip_state.effective_agents_view_preferences();
                                let mut filters = prefs.filters.clone();
                                filters.origins = toggle_in_vec(&filters.origins, &origin);
                                let active = active_view_after_query_edit(
                                    &chip_state,
                                    &filters,
                                    prefs.sort_mode,
                                    prefs.group_mode,
                                    prefs.hide_finished,
                                );
                                chip_state
                                    .set_agents_view_overlay(|overlay| {
                                        overlay.filters = Some(filters.clone());
                                        overlay.active_view_id = Some(active);
                                    });
                                send_pref_update(
                                    &chip_state,
                                    AgentsViewPreferencesUpdate::SetFilters { filters },
                                );
                            };
                            view! {
                                <button
                                    type="button"
                                    class=move || if active() { "filter-toggle active" } else { "filter-toggle" }
                                    on:click=on_click
                                >
                                    {origin_label(origin)}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <HostProjectFilters />
            </div>

            <div class="agent-monitor-live" aria-live="polite">
                {move || announcement.get()}
            </div>

            <div class="agent-monitor-body">
                {move || {
                    let current = groups.get();
                    let row_count = current.iter().map(|group| group.rows.len()).sum::<usize>();
                    if row_count == 0 {
                        view! { <div class="agent-monitor-empty">"No agents match the current view"</div> }
                            .into_any()
                    } else {
                        let reorderable = matches!(
                            prefs_for_controls.get().sort_mode,
                            AgentSortMode::ManualThenActivity
                        ) && matches!(prefs_for_controls.get().group_mode, AgentGroupMode::Flat);
                        view! {
                            <div class=list_class>
                                <For
                                    each=move || groups.get()
                                    key=|group| group.label.clone().unwrap_or_default()
                                    let:group
                                >
                                    {group
                                        .label
                                        .clone()
                                        .map(|label| {
                                            view! {
                                                <div class="agent-monitor-group-header">{label}</div>
                                            }
                                        })}
                                    <For
                                        each=move || group.rows.clone()
                                        key=|row| row.key.clone()
                                        let:row
                                    >
                                        <AgentMonitorRowView
                                            row=row
                                            reorderable=reorderable
                                            visible_keys=visible_keys
                                            dragged_key=dragged_key
                                            drop_target=drop_target
                                            announcement=announcement
                                        />
                                    </For>
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
fn SmartViewSwitcher() -> impl IntoView {
    let state = expect_context::<AppState>();

    // Server-owned Smart View list (built-ins then user views). The list is a
    // pure projection of the snapshot — rename/reorder/delete reflect when the
    // server notify replaces it; there is no durable local copy.
    let views_state = state.clone();
    let smart_views = Memo::new(move |_| {
        views_state
            .agents_view_preferences
            .get()
            .smart_views
            .clone()
    });
    // Active id includes the optimistic overlay so selecting a view highlights
    // instantly; `None` means no view is highlighted (custom/divergent query).
    let active_state = state.clone();
    let active_id = Memo::new(move |_| active_state.effective_active_smart_view_id());

    let prompt = RwSignal::new(None::<SmartViewPrompt>);
    let prompt_text = RwSignal::new(String::new());
    let open_menu = RwSignal::new(None::<SmartViewId>);

    // Auto-focus the name input whenever a save-as / rename prompt opens. Reads
    // both `prompt` and the node ref so the effect re-runs once the input mounts.
    let prompt_input_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if prompt.get().is_some()
            && let Some(input) = prompt_input_ref.get()
        {
            let _ = input.focus();
        }
    });

    let tabs_state = state.clone();
    let tabs = move || {
        let snapshot = smart_views.get();
        let user_count = snapshot.user.len();
        snapshot
            .built_in
            .iter()
            .cloned()
            .map(|view| (view, None))
            .chain(
                snapshot
                    .user
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(index, view)| (view, Some(index))),
            )
            .map(|(view, user_index)| {
                let state = tabs_state.clone();
                smart_view_tab(
                    state,
                    view,
                    user_index,
                    user_count,
                    active_id,
                    smart_views,
                    open_menu,
                    prompt,
                    prompt_text,
                )
            })
            .collect_view()
    };

    let on_save_as = move |_| {
        open_menu.set(None);
        prompt_text.set(String::new());
        prompt.set(Some(SmartViewPrompt::SaveAs));
    };

    let submit_state = state.clone();
    let on_input = move |ev: leptos::ev::Event| prompt_text.set(event_target_value(&ev));
    let on_keydown = {
        let submit_state = submit_state.clone();
        move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
            "Enter" => {
                ev.prevent_default();
                submit_smart_view_prompt(&submit_state, prompt, prompt_text);
            }
            "Escape" => {
                ev.prevent_default();
                prompt.set(None);
                prompt_text.set(String::new());
            }
            _ => {}
        }
    };
    let on_confirm = {
        let submit_state = submit_state.clone();
        move |_| submit_smart_view_prompt(&submit_state, prompt, prompt_text)
    };
    let on_cancel = move |_| {
        prompt.set(None);
        prompt_text.set(String::new());
    };

    view! {
        <div class="agent-monitor-smart-views" data-test="smart-view-switcher">
            // Click-away layer: while a manage menu is open, a click anywhere
            // else closes it. Sits below the menu (z-index) but above page content.
            {move || {
                open_menu
                    .get()
                    .is_some()
                    .then(|| {
                        view! {
                            <div
                                class="smart-view-menu-backdrop"
                                on:click=move |_| open_menu.set(None)
                            ></div>
                        }
                    })
            }}
            <div class="smart-view-tabs" role="tablist" aria-label="Smart views">
                {tabs}
            </div>
            <button
                type="button"
                class="filter-toggle smart-view-save"
                data-test="smart-view-save-as"
                title="Save the current filters, sort, and grouping as a reusable view"
                on:click=on_save_as
            >
                "Save current view as…"
            </button>
            {move || {
                prompt
                    .get()
                    .map(|open| {
                        let confirm_label = match open {
                            SmartViewPrompt::SaveAs => "Save",
                            SmartViewPrompt::Rename(_) => "Rename",
                        };
                        view! {
                            <div class="smart-view-prompt" data-test="smart-view-prompt">
                                <input
                                    type="text"
                                    node_ref=prompt_input_ref
                                    class="panel-search-input smart-view-name-input"
                                    data-test="smart-view-name-input"
                                    placeholder="View name"
                                    prop:value=prompt_text
                                    on:input=on_input
                                    on:keydown=on_keydown.clone()
                                    spellcheck="false"
                                    autocapitalize="none"
                                    autocomplete="off"
                                />
                                <button
                                    type="button"
                                    class="filter-toggle smart-view-name-confirm"
                                    data-test="smart-view-name-confirm"
                                    on:click=on_confirm.clone()
                                >
                                    {confirm_label}
                                </button>
                                <button
                                    type="button"
                                    class="filter-toggle"
                                    on:click=on_cancel
                                >
                                    "Cancel"
                                </button>
                            </div>
                        }
                    })
            }}
        </div>
    }
}

/// Render one Smart View tab. Built-in views get a select button only; user
/// views also get a manage menu (rename / update / reorder / delete). The tab's
/// active state and name are looked up reactively so a rename or selection that
/// keeps the same id still re-renders.
#[allow(clippy::too_many_arguments)]
fn smart_view_tab(
    state: AppState,
    view: SmartView,
    user_index: Option<usize>,
    user_count: usize,
    active_id: Memo<Option<SmartViewId>>,
    smart_views: Memo<AgentsSmartViewsSnapshot>,
    open_menu: RwSignal<Option<SmartViewId>>,
    prompt: RwSignal<Option<SmartViewPrompt>>,
    prompt_text: RwSignal<String>,
) -> impl IntoView {
    let view_id = view.id.clone();
    let name = view.name.clone();

    let active_for_attr = view_id.clone();
    let aria_current = move || {
        if active_id.get().as_ref() == Some(&active_for_attr) {
            "true"
        } else {
            "false"
        }
    };
    let active_for_class = view_id.clone();
    let tab_class = move || {
        if active_id.get().as_ref() == Some(&active_for_class) {
            "smart-view-tab active"
        } else {
            "smart-view-tab"
        }
    };

    let select_state = state.clone();
    let select_view = view.clone();
    let on_select = move |_| {
        open_menu.set(None);
        select_smart_view(&select_state, &select_view);
    };

    let manage = user_index.map(|index| {
        let at_first = index == 0;
        let at_last = index + 1 >= user_count;
        let menu_id = view_id.clone();
        let menu_open = {
            let menu_id = menu_id.clone();
            move || open_menu.get().as_ref() == Some(&menu_id)
        };
        let toggle_id = menu_id.clone();
        let on_toggle = move |_| {
            let toggle_id = toggle_id.clone();
            open_menu
                .update(|current| {
                    *current = if current.as_ref() == Some(&toggle_id) {
                        None
                    } else {
                        Some(toggle_id)
                    };
                });
        };

        let rename_id = menu_id.clone();
        let rename_name = name.clone();
        let on_rename = move |_| {
            open_menu.set(None);
            prompt_text.set(rename_name.clone());
            prompt.set(Some(SmartViewPrompt::Rename(rename_id.clone())));
        };

        let update_state = state.clone();
        let update_id = menu_id.clone();
        let on_update = move |_| {
            open_menu.set(None);
            send_smart_view_update(
                &update_state,
                AgentsSmartViewsUpdate::Update {
                    id: update_id.clone(),
                },
            );
        };

        let left_state = state.clone();
        let left_id = menu_id.clone();
        let on_move_left = move |_| {
            open_menu.set(None);
            reorder_user_view(&left_state, &smart_views.get_untracked(), &left_id, true);
        };

        let right_state = state.clone();
        let right_id = menu_id.clone();
        let on_move_right = move |_| {
            open_menu.set(None);
            reorder_user_view(&right_state, &smart_views.get_untracked(), &right_id, false);
        };

        let delete_state = state.clone();
        let delete_id = menu_id.clone();
        let delete_name = name.clone();
        let on_delete = move |_| {
            open_menu.set(None);
            delete_user_view(&delete_state, delete_id.clone(), delete_name.clone());
        };

        view! {
            <button
                type="button"
                class="smart-view-menu-toggle"
                data-test="smart-view-menu-toggle"
                title="Manage view"
                aria-label=format!("Manage {name} view")
                on:click=on_toggle
            >
                "⋯"
            </button>
            {move || {
                menu_open()
                    .then(|| {
                        view! {
                            <div class="smart-view-menu" data-test="smart-view-menu">
                                <button type="button" class="smart-view-menu-item" data-test="smart-view-rename" on:click=on_rename.clone()>
                                    "Rename"
                                </button>
                                <button type="button" class="smart-view-menu-item" data-test="smart-view-update" on:click=on_update.clone()>
                                    "Update from current"
                                </button>
                                <button type="button" class="smart-view-menu-item" data-test="smart-view-move-left" disabled=at_first on:click=on_move_left.clone()>
                                    "Move left"
                                </button>
                                <button type="button" class="smart-view-menu-item" data-test="smart-view-move-right" disabled=at_last on:click=on_move_right.clone()>
                                    "Move right"
                                </button>
                                <button type="button" class="smart-view-menu-item smart-view-delete" data-test="smart-view-delete" on:click=on_delete.clone()>
                                    "Delete"
                                </button>
                            </div>
                        }
                    })
            }}
        }
    });

    view! {
        <div class="smart-view-tab-group">
            <button
                type="button"
                class=tab_class
                data-test="smart-view-tab"
                role="tab"
                aria-current=aria_current
                on:click=on_select
            >
                {name.clone()}
            </button>
            {manage}
        </div>
    }
}

#[component]
fn HostProjectFilters() -> impl IntoView {
    let state = expect_context::<AppState>();
    let prefs_state = state.clone();
    let prefs = Memo::new(move |_| prefs_state.effective_agents_view_preferences());

    let host_state = state.clone();
    let hosts = move || host_state.configured_hosts.get();
    let project_state = state.clone();
    let projects = move || project_state.projects.get();

    let host_chip_state = state.clone();
    let project_chip_state = state.clone();

    view! {
        <div class="agent-monitor-filter-group" data-test="agent-monitor-host-filters">
            <For each=hosts key=|host| host.id.clone() let:host>
                {
                    let host_id = host.id.clone();
                    let label = host.label.clone();
                    let chip_state = host_chip_state.clone();
                    let active = move || {
                        let host_id = host_id.clone();
                        prefs.get().filters.host_ids.iter().any(|id| id.0 == host_id)
                    };
                    let click_host = host.id.clone();
                    let on_click = move |_| {
                        let prefs = chip_state.effective_agents_view_preferences();
                        let mut filters = prefs.filters.clone();
                        let value = HostFilterId(click_host.clone());
                        filters.host_ids = toggle_in_vec(&filters.host_ids, &value);
                        let active = active_view_after_query_edit(
                            &chip_state,
                            &filters,
                            prefs.sort_mode,
                            prefs.group_mode,
                            prefs.hide_finished,
                        );
                        chip_state.set_agents_view_overlay(|overlay| {
                            overlay.filters = Some(filters.clone());
                            overlay.active_view_id = Some(active);
                        });
                        send_pref_update(
                            &chip_state,
                            AgentsViewPreferencesUpdate::SetFilters { filters },
                        );
                    };
                    view! {
                        <button
                            type="button"
                            class=move || if active() { "filter-toggle active" } else { "filter-toggle" }
                            on:click=on_click
                        >
                            {label}
                        </button>
                    }
                }
            </For>
        </div>
        <div class="agent-monitor-filter-group" data-test="agent-monitor-project-filters">
            <For
                each=projects
                key=|project| (project.host_id.clone(), project.project.id.clone())
                let:project
            >
                {
                    let host_id = project.host_id.clone();
                    let project_id = project.project.id.clone();
                    let label = project.project.name.clone();
                    let chip_state = project_chip_state.clone();
                    let active_host = host_id.clone();
                    let active_project = project_id.clone();
                    let active = move || {
                        prefs.get().filters.project_ids.iter().any(|filter| {
                            filter.host_id.0 == active_host && filter.project_id == active_project
                        })
                    };
                    let on_click = move |_| {
                        let prefs = chip_state.effective_agents_view_preferences();
                        let mut filters = prefs.filters.clone();
                        let value = AgentProjectFilter {
                            host_id: HostFilterId(host_id.clone()),
                            project_id: project_id.clone(),
                        };
                        filters.project_ids = toggle_in_vec(&filters.project_ids, &value);
                        let active = active_view_after_query_edit(
                            &chip_state,
                            &filters,
                            prefs.sort_mode,
                            prefs.group_mode,
                            prefs.hide_finished,
                        );
                        chip_state.set_agents_view_overlay(|overlay| {
                            overlay.filters = Some(filters.clone());
                            overlay.active_view_id = Some(active);
                        });
                        send_pref_update(
                            &chip_state,
                            AgentsViewPreferencesUpdate::SetFilters { filters },
                        );
                    };
                    view! {
                        <button
                            type="button"
                            class=move || if active() { "filter-toggle active" } else { "filter-toggle" }
                            on:click=on_click
                        >
                            {label}
                        </button>
                    }
                }
            </For>
        </div>
    }
}

#[component]
fn AgentMonitorRowView(
    row: AgentMonitorRow,
    reorderable: bool,
    visible_keys: Memo<Vec<AgentMonitorKey>>,
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
        if status == DerivedAgentState::Terminated {
            class.push_str(" agent-row-finished");
        }
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
            visible_keys.get_untracked(),
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
            visible_keys.get_untracked(),
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
            visible_keys.get_untracked(),
            &key_for_move_down,
            true,
        ) {
            announcement.set(format!("Moved {name_for_move_down} down"));
        }
    };

    let key_for_can_up = key.clone();
    let can_move_up = move || {
        visible_keys
            .get()
            .iter()
            .position(|candidate| candidate == &key_for_can_up)
            .is_some_and(|index| index > 0)
    };
    let key_for_can_down = key.clone();
    let can_move_down = move || {
        visible_keys
            .get()
            .iter()
            .position(|candidate| candidate == &key_for_can_down)
            .is_some_and(|index| index + 1 < visible_keys.get().len())
    };

    let state_for_keydown = state.clone();
    let key_for_keydown = key.clone();
    let name_for_keydown = name.clone();
    let agent_for_keydown = agent.clone();
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if reorderable && ev.alt_key() {
            match ev.key().as_str() {
                "ArrowUp" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    if apply_keyboard_move(
                        &state_for_keydown,
                        visible_keys.get_untracked(),
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
                        visible_keys.get_untracked(),
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
            {reorderable
                .then(|| {
                    view! {
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
                    }
                })}

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
                {reorderable
                    .then(|| {
                        view! {
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
                        }
                    })}
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
    use protocol::{AgentId, SessionId, StreamPath};

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

    fn row(agent: AgentInfo, status: DerivedAgentState) -> AgentMonitorRow {
        AgentMonitorRow {
            key: AgentMonitorKey::from_agent(&agent),
            project_label: agent
                .project_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "No project".to_owned()),
            host_label: agent.host_id.clone(),
            status,
            agent,
        }
    }

    #[test]
    fn manual_order_uses_session_id_when_present() {
        let mut a = agent("a", "h", None, 1, true, false);
        a.session_id = Some(SessionId("sess-a".to_owned()));
        let b = agent("b", "h", None, 2, true, false);

        assert_eq!(
            agent_order_key(&a),
            AgentOrderKey::Session {
                session_id: SessionId("sess-a".to_owned())
            }
        );
        assert_eq!(
            agent_order_key(&b),
            AgentOrderKey::TransientAgent {
                host_id: HostFilterId("h".to_owned()),
                agent_id: b.agent_id.clone(),
            }
        );
        // A session-keyed manual order matches the live agent that resolves to
        // that session, surviving reconnects that change the agent id.
        let order = vec![agent_order_key(&a)];
        let mut a2 = agent("a", "h", None, 1, true, false);
        a2.agent_id = AgentId("agent-a-reconnected".to_owned());
        a2.session_id = Some(SessionId("sess-a".to_owned()));
        assert_eq!(manual_rank(&a2, &order), Some(0));
    }

    #[test]
    fn view_filters_apply_each_dimension() {
        let claude = agent("c", "h1", Some("p1"), 1, true, false);
        let terminated = agent("t", "h1", Some("p1"), 2, true, true);

        // hide_finished drops terminated rows only.
        assert!(agent_passes_view_filters(
            &claude,
            DerivedAgentState::Idle,
            &AgentsViewFilters::default(),
            true,
            "",
        ));
        assert!(!agent_passes_view_filters(
            &terminated,
            DerivedAgentState::Terminated,
            &AgentsViewFilters::default(),
            true,
            "",
        ));

        // status filter keeps only matching states.
        let only_idle = AgentsViewFilters {
            statuses: vec![AgentStatusFilter::Idle],
            ..AgentsViewFilters::default()
        };
        assert!(agent_passes_view_filters(
            &claude,
            DerivedAgentState::Idle,
            &only_idle,
            false,
            "",
        ));
        assert!(!agent_passes_view_filters(
            &claude,
            DerivedAgentState::Thinking,
            &only_idle,
            false,
            "",
        ));

        // host filter.
        let other_host = AgentsViewFilters {
            host_ids: vec![HostFilterId("h2".to_owned())],
            ..AgentsViewFilters::default()
        };
        assert!(!agent_passes_view_filters(
            &claude,
            DerivedAgentState::Idle,
            &other_host,
            false,
            "",
        ));

        // search narrows by name.
        assert!(!agent_passes_view_filters(
            &claude,
            DerivedAgentState::Idle,
            &AgentsViewFilters::default(),
            false,
            "zzz",
        ));
    }

    #[test]
    fn sort_modes_order_rows() {
        let newest = row(
            agent("newest", "h", None, 300, true, false),
            DerivedAgentState::Idle,
        );
        let oldest = row(
            agent("oldest", "h", None, 100, true, false),
            DerivedAgentState::Idle,
        );
        let mid = row(
            agent("mid", "h", None, 200, true, false),
            DerivedAgentState::Idle,
        );

        let mut rows = vec![mid.clone(), newest.clone(), oldest.clone()];
        sort_rows(&mut rows, AgentSortMode::NewestFirst, &[]);
        assert_eq!(
            rows.iter()
                .map(|r| r.agent.name.clone())
                .collect::<Vec<_>>(),
            vec!["newest", "mid", "oldest"]
        );

        let mut rows = vec![mid.clone(), newest.clone(), oldest.clone()];
        sort_rows(&mut rows, AgentSortMode::OldestFirst, &[]);
        assert_eq!(
            rows.iter()
                .map(|r| r.agent.name.clone())
                .collect::<Vec<_>>(),
            vec!["oldest", "mid", "newest"]
        );
    }

    #[test]
    fn manual_then_activity_freezes_ordered_rows_and_appends_new() {
        let a = row(
            agent("a", "h", None, 100, true, false),
            DerivedAgentState::Idle,
        );
        let b = row(
            agent("b", "h", None, 200, true, false),
            DerivedAgentState::Idle,
        );
        let c = row(
            agent("c", "h", None, 300, true, false),
            DerivedAgentState::Idle,
        );

        // Manual order pins c, a; b is unranked and appended by activity.
        let manual = vec![agent_order_key(&c.agent), agent_order_key(&a.agent)];
        let mut rows = vec![a.clone(), b.clone(), c.clone()];
        sort_rows(&mut rows, AgentSortMode::ManualThenActivity, &manual);
        assert_eq!(
            rows.iter()
                .map(|r| r.agent.name.clone())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    #[test]
    fn grouping_buckets_in_first_seen_order() {
        let claude = row(
            agent("c", "h", None, 100, true, false),
            DerivedAgentState::Idle,
        );
        let mut codex_agent = agent("x", "h", None, 200, true, false);
        codex_agent.backend_kind = BackendKind::Codex;
        let codex = row(codex_agent, DerivedAgentState::Idle);

        let groups = build_groups(
            vec![claude.clone(), codex.clone()],
            AgentSortMode::OldestFirst,
            AgentGroupMode::Backend,
            &[],
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label.as_deref(), Some("Claude"));
        assert_eq!(groups[1].label.as_deref(), Some("Codex"));
    }

    #[test]
    fn flat_grouping_is_single_unlabeled_group() {
        let a = row(
            agent("a", "h", None, 100, true, false),
            DerivedAgentState::Idle,
        );
        let groups = build_groups(vec![a], AgentSortMode::Status, AgentGroupMode::Flat, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, None);
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppState;
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentSortMode, AgentStatusFilter, AgentsSmartViewsSnapshot, AgentsViewPreferences,
        AgentsViewPreferencesSnapshot, BuiltInSmartViewId, SmartView, SmartViewId, StreamPath,
        UserSmartViewId,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const HOST: &str = "local";

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 900px; height: 700px; \
                 z-index: 2147483647; background: white;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    /// Capture outbound `send_host_line` Tauri invokes so a test can inspect
    /// which preference frame was put on the wire.
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

    /// `update.kind` of the last `set_agents_view_preferences` frame on the
    /// wire, or empty string if none was sent.
    fn last_pref_update_kind() -> String {
        js_sys::eval(
            r#"
            (function() {
                let kind = "";
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "set_agents_view_preferences") {
                        kind = env.payload.update.kind;
                    }
                }
                return kind;
            })()
            "#,
        )
        .expect("probe send calls")
        .as_string()
        .unwrap_or_default()
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

    fn agent(name: &str, created_at_ms: u64, fatal: bool) -> AgentInfo {
        AgentInfo {
            host_id: HOST.to_owned(),
            agent_id: AgentId(format!("agent-{name}")),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms,
            instance_stream: StreamPath(format!("/agent/{name}")),
            started: true,
            fatal_error: fatal.then(|| "boom".to_owned()),
        }
    }

    fn snapshot(prefs: AgentsViewPreferences) -> AgentsViewPreferencesSnapshot {
        AgentsViewPreferencesSnapshot {
            preferences: prefs,
            load_error: None,
            smart_views: Default::default(),
        }
    }

    /// Prime an AppState as the primary local host with a server preference
    /// snapshot and a routable stream, ready to mount.
    fn primed_state(agents: Vec<AgentInfo>, prefs: AgentsViewPreferences) -> AppState {
        let state = AppState::new();
        state.agents.set(agents);
        state.host_streams.update(|streams| {
            streams.insert(HOST.to_owned(), StreamPath("/host/local".to_owned()));
        });
        state.apply_agents_view_snapshot(HOST, snapshot(prefs));
        state
    }

    fn rendered_names(container: &HtmlElement) -> Vec<String> {
        let nodes = container.query_selector_all(".agent-monitor-name").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .filter_map(|node| node.text_content())
            .collect()
    }

    fn transient_key(name: &str) -> AgentOrderKey {
        AgentOrderKey::TransientAgent {
            host_id: HostFilterId(HOST.to_owned()),
            agent_id: AgentId(format!("agent-{name}")),
        }
    }

    // (a) A bootstrap snapshot with non-default prefs (sort + manual order +
    // active filter) drives the rendered order and filtering: render-from-server.
    #[wasm_bindgen_test]
    async fn renders_order_and_filters_from_server_snapshot() {
        let prefs = AgentsViewPreferences {
            sort_mode: AgentSortMode::NameAsc,
            filters: AgentsViewFilters {
                statuses: vec![AgentStatusFilter::Idle],
                ..AgentsViewFilters::default()
            },
            ..AgentsViewPreferences::default()
        };
        // "zeta" idle, "alpha" idle, "mid" terminated (filtered out by status).
        let agents = vec![
            agent("zeta", 100, false),
            agent("alpha", 200, false),
            agent("mid", 300, true),
        ];
        let state = primed_state(agents, prefs);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // NameAsc order, terminated "mid" excluded by the Idle status filter.
        assert_eq!(rendered_names(&container), vec!["alpha", "zeta"]);
    }

    // (b) A user change emits the correct SetAgentsViewPreferences frame AND
    // updates the view immediately via the overlay (no server round-trip).
    #[wasm_bindgen_test]
    async fn user_toggle_emits_frame_and_updates_view_immediately() {
        install_send_stub();
        let agents = vec![agent("live", 100, false), agent("dead", 200, true)];
        let state = primed_state(agents, AgentsViewPreferences::default());
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // Both rows visible before hiding finished.
        assert_eq!(rendered_names(&container).len(), 2);

        let toggle: HtmlElement = container
            .query_selector("[data-test='agent-monitor-hide-finished']")
            .unwrap()
            .expect("hide-finished toggle present")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;

        // Overlay applied instantly: terminated "dead" row gone, no notify yet.
        assert_eq!(rendered_names(&container), vec!["live"]);
        // The correct typed frame went on the wire.
        assert_eq!(last_pref_update_kind(), "set_hide_finished");
        // The durable server snapshot was NOT mutated locally.
        assert!(
            !state
                .agents_view_preferences
                .get_untracked()
                .preferences
                .hide_finished
        );
    }

    // (c) FLICKER REGRESSION: after a simulated host-state churn the order does
    // not reset to default — it stays the server-driven manual order.
    #[wasm_bindgen_test]
    async fn order_survives_host_churn() {
        let prefs = AgentsViewPreferences {
            sort_mode: AgentSortMode::ManualThenActivity,
            manual_order: vec![transient_key("c"), transient_key("a")],
            ..AgentsViewPreferences::default()
        };
        let agents = vec![
            agent("a", 100, false),
            agent("b", 200, false),
            agent("c", 300, false),
        ];
        let state = primed_state(agents.clone(), prefs);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // Manual order pins c,a; b appended. (Default activity sort would be
        // c,b,a by newest-first, so this proves the manual order is in force.)
        assert_eq!(rendered_names(&container), vec!["c", "a", "b"]);

        // Simulate the churn that historically reset the order: a host runtime
        // cleanup (which used to prune the local order map) followed by the
        // host reconnecting and re-delivering its agents.
        state.clear_host_runtime(HOST);
        next_tick().await;
        state.agents.set(agents);
        next_tick().await;

        // The server-owned preferences were never pruned, so the order holds.
        assert_eq!(rendered_names(&container), vec!["c", "a", "b"]);
    }

    // (d) An AgentsViewPreferencesNotify reconciles/drops the overlay so the
    // view matches server state.
    #[wasm_bindgen_test]
    async fn notify_reconciles_and_drops_overlay() {
        install_send_stub();
        let agents = vec![agent("live", 100, false), agent("dead", 200, true)];
        let state = primed_state(agents, AgentsViewPreferences::default());
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // User hides finished → overlay installed.
        let toggle: HtmlElement = container
            .query_selector("[data-test='agent-monitor-hide-finished']")
            .unwrap()
            .expect("hide-finished toggle present")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;
        assert!(state.agents_view_overlay_pending());
        assert_eq!(rendered_names(&container), vec!["live"]);

        // Server confirms the change via a full-snapshot notify.
        let confirmed = AgentsViewPreferences {
            hide_finished: true,
            ..AgentsViewPreferences::default()
        };
        state.apply_agents_view_snapshot(HOST, snapshot(confirmed));
        next_tick().await;

        // Overlay dropped (server is now the sole input) and the view still
        // reflects the server state.
        assert!(!state.agents_view_overlay_pending());
        assert!(
            state
                .agents_view_preferences
                .get_untracked()
                .preferences
                .hide_finished
        );
        assert_eq!(rendered_names(&container), vec!["live"]);
    }

    // (e) BLOCKER REGRESSION: when the server's canonical value DIFFERS from the
    // optimistic overlay (the server keeps a different manual order than the
    // visible-only one the client sent), the notify still drops the overlay and
    // the view snaps to the SERVER value rather than sticking on the optimistic
    // one. An equality-only reconcile would leave the overlay stuck here.
    #[wasm_bindgen_test]
    async fn notify_drops_overlay_even_when_server_value_differs() {
        install_send_stub();
        let prefs = AgentsViewPreferences {
            sort_mode: AgentSortMode::ManualThenActivity,
            ..AgentsViewPreferences::default()
        };
        // created newest-first => default render order is c, b, a.
        let agents = vec![
            agent("a", 100, false),
            agent("b", 200, false),
            agent("c", 300, false),
        ];
        let state = primed_state(agents, prefs);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;
        assert_eq!(rendered_names(&container), vec!["c", "b", "a"]);

        // User drags/keys "c" down one slot → optimistic order b, c, a.
        let move_c_down: HtmlElement = container
            .query_selector("[aria-label='Move c down']")
            .unwrap()
            .expect("move-down button for c present")
            .dyn_into()
            .unwrap();
        move_c_down.click();
        next_tick().await;
        assert!(state.agents_view_overlay_pending());
        assert_eq!(rendered_names(&container), vec!["b", "c", "a"]);
        assert_eq!(last_pref_update_kind(), "set_manual_order");

        // Server canonicalizes to a DIFFERENT order than the optimistic one and
        // notifies. (Optimistic was [b, c, a]; server returns [a, b, c].)
        let server = AgentsViewPreferences {
            sort_mode: AgentSortMode::ManualThenActivity,
            manual_order: vec![transient_key("a"), transient_key("b"), transient_key("c")],
            ..AgentsViewPreferences::default()
        };
        state.apply_agents_view_snapshot(HOST, snapshot(server));
        next_tick().await;

        // Overlay cleared and the view shows the server order, not the stuck
        // optimistic [b, c, a].
        assert!(!state.agents_view_overlay_pending());
        assert_eq!(rendered_names(&container), vec!["a", "b", "c"]);
    }

    // (f) Group-mode render: Backend grouping emits one header per backend.
    #[wasm_bindgen_test]
    async fn group_mode_renders_group_headers() {
        let prefs = AgentsViewPreferences {
            group_mode: AgentGroupMode::Backend,
            ..AgentsViewPreferences::default()
        };
        let mut codex = agent("x", 200, false);
        codex.backend_kind = BackendKind::Codex;
        let agents = vec![agent("c", 100, false), codex];
        let state = primed_state(agents, prefs);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        let headers: Vec<String> = {
            let nodes = container
                .query_selector_all(".agent-monitor-group-header")
                .unwrap();
            (0..nodes.length())
                .filter_map(|i| nodes.item(i))
                .filter_map(|node| node.text_content())
                .collect()
        };
        assert!(headers.contains(&"Claude".to_owned()));
        assert!(headers.contains(&"Codex".to_owned()));
    }

    // ── Smart View switcher (Phase 2a) ──────────────────────────────────────

    fn built_in_view(id: BuiltInSmartViewId, name: &str) -> SmartView {
        SmartView {
            id: SmartViewId::BuiltIn(id),
            name: name.to_owned(),
            filters: AgentsViewFilters::default(),
            sort_mode: AgentSortMode::default(),
            group_mode: AgentGroupMode::default(),
            hide_finished: false,
        }
    }

    fn user_view(id: &str, name: &str, sort_mode: AgentSortMode) -> SmartView {
        SmartView {
            id: SmartViewId::User(UserSmartViewId(id.to_owned())),
            name: name.to_owned(),
            filters: AgentsViewFilters::default(),
            sort_mode,
            group_mode: AgentGroupMode::default(),
            hide_finished: false,
        }
    }

    /// The three built-ins plus the given user views, with `active` selected.
    fn smart_views(user: Vec<SmartView>, active: Option<SmartViewId>) -> AgentsSmartViewsSnapshot {
        AgentsSmartViewsSnapshot {
            built_in: vec![
                built_in_view(BuiltInSmartViewId::All, "All"),
                built_in_view(BuiltInSmartViewId::Active, "Active"),
                built_in_view(BuiltInSmartViewId::FailedTerminated, "Failed / terminated"),
            ],
            user,
            active_view_id: active,
        }
    }

    fn snapshot_with_views(
        prefs: AgentsViewPreferences,
        views: AgentsSmartViewsSnapshot,
    ) -> AgentsViewPreferencesSnapshot {
        AgentsViewPreferencesSnapshot {
            preferences: prefs,
            load_error: None,
            smart_views: views,
        }
    }

    fn primed_state_views(
        agents: Vec<AgentInfo>,
        prefs: AgentsViewPreferences,
        views: AgentsSmartViewsSnapshot,
    ) -> AppState {
        let state = AppState::new();
        state.agents.set(agents);
        state.host_streams.update(|streams| {
            streams.insert(HOST.to_owned(), StreamPath("/host/local".to_owned()));
        });
        state.apply_agents_view_snapshot(HOST, snapshot_with_views(prefs, views));
        state
    }

    fn tab_names(container: &HtmlElement) -> Vec<String> {
        let nodes = container
            .query_selector_all("[data-test='smart-view-tab']")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .filter_map(|node| node.text_content())
            .collect()
    }

    /// Names of the tabs marked active via the semantic `aria-current="true"`
    /// state (not a CSS class, per CLAUDE.md UI-test rules).
    fn active_tab_names(container: &HtmlElement) -> Vec<String> {
        let nodes = container
            .query_selector_all("[data-test='smart-view-tab'][aria-current='true']")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .filter_map(|node| node.text_content())
            .collect()
    }

    /// JSON of the last `set_agents_smart_views` update put on the wire, or
    /// empty string if none was sent.
    fn last_smart_view_update_json() -> String {
        js_sys::eval(
            r#"
            (function() {
                let out = "";
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "set_agents_smart_views") {
                        out = JSON.stringify(env.payload.update);
                    }
                }
                return out;
            })()
            "#,
        )
        .expect("probe smart-view sends")
        .as_string()
        .unwrap_or_default()
    }

    // (a) The switcher renders built-ins then user views from the snapshot, and
    // highlights the one matching active_view_id.
    #[wasm_bindgen_test]
    async fn switcher_renders_views_and_highlights_active() {
        let views = smart_views(
            vec![user_view("v1", "My idle agents", AgentSortMode::NameAsc)],
            Some(SmartViewId::User(UserSmartViewId("v1".to_owned()))),
        );
        let state = primed_state_views(vec![agent("a", 100, false)], Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        assert_eq!(
            tab_names(&container),
            vec!["All", "Active", "Failed / terminated", "My idle agents"]
        );
        // Only the active user view is highlighted.
        assert_eq!(active_tab_names(&container), vec!["My idle agents"]);
    }

    // (b) Selecting a view emits SetActive for that id.
    #[wasm_bindgen_test]
    async fn selecting_view_emits_set_active() {
        install_send_stub();
        let views = smart_views(
            vec![user_view("v1", "Mine", AgentSortMode::NameAsc)],
            Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)),
        );
        let state = primed_state_views(vec![agent("a", 100, false)], Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // Click the "Active" built-in tab (index 1).
        let tabs = container
            .query_selector_all("[data-test='smart-view-tab']")
            .unwrap();
        let active_tab: HtmlElement = tabs.item(1).unwrap().dyn_into().unwrap();
        active_tab.click();
        next_tick().await;

        let update = last_smart_view_update_json();
        assert!(
            update.contains("\"kind\":\"set_active\""),
            "update was {update}"
        );
        assert!(
            update.contains("\"kind\":\"built_in\""),
            "update was {update}"
        );
        assert!(update.contains("\"id\":\"active\""), "update was {update}");
    }

    // (c) Save-as emits SaveCurrent { name } and never includes search text.
    #[wasm_bindgen_test]
    async fn save_as_emits_save_current_with_name() {
        install_send_stub();
        let views = smart_views(vec![], Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)));
        let state = primed_state_views(vec![agent("a", 100, false)], Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // Open the name prompt.
        let save: HtmlElement = container
            .query_selector("[data-test='smart-view-save-as']")
            .unwrap()
            .expect("save-as button present")
            .dyn_into()
            .unwrap();
        save.click();
        next_tick().await;

        // Type a name and confirm.
        let input: web_sys::HtmlInputElement = container
            .query_selector("[data-test='smart-view-name-input']")
            .unwrap()
            .expect("name input present")
            .dyn_into()
            .unwrap();
        input.set_value("My View");
        let ev = web_sys::Event::new("input").unwrap();
        input.dispatch_event(&ev).unwrap();
        next_tick().await;

        let confirm: HtmlElement = container
            .query_selector("[data-test='smart-view-name-confirm']")
            .unwrap()
            .expect("confirm button present")
            .dyn_into()
            .unwrap();
        confirm.click();
        next_tick().await;

        let update = last_smart_view_update_json();
        assert!(
            update.contains("\"kind\":\"save_current\""),
            "update was {update}"
        );
        assert!(
            update.contains("\"name\":\"My View\""),
            "update was {update}"
        );
        // The query is captured server-side; the payload carries only the name,
        // so search text can never leak into a saved view.
        assert!(!update.contains("search"), "update was {update}");
    }

    // (d) When active_view_id is None (divergent/custom query), no tab is
    // highlighted.
    #[wasm_bindgen_test]
    async fn divergent_active_highlights_nothing() {
        let views = smart_views(vec![user_view("v1", "Mine", AgentSortMode::NameAsc)], None);
        let state = primed_state_views(vec![agent("a", 100, false)], Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // All four tabs render, none highlighted.
        assert_eq!(tab_names(&container).len(), 4);
        assert!(active_tab_names(&container).is_empty());
    }

    // (e) Optimistic: selecting a view reflects active + its query immediately
    // (before any notify), then a server notify reconciles and drops the
    // overlay while keeping the same view active.
    #[wasm_bindgen_test]
    async fn selecting_view_is_optimistic_then_reconciles() {
        install_send_stub();
        // Server starts on "All"; the user view sorts NameAsc.
        let v1 = user_view("v1", "Alpha order", AgentSortMode::NameAsc);
        let views = smart_views(
            vec![v1.clone()],
            Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)),
        );
        // Newest-first default would render z, a; NameAsc renders a, z.
        let agents = vec![agent("z", 200, false), agent("a", 100, false)];
        let state = primed_state_views(agents, Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;

        // Default sort: newest-first (z then a). "All" is highlighted.
        assert_eq!(rendered_names(&container), vec!["z", "a"]);
        assert_eq!(active_tab_names(&container), vec!["All"]);

        // Select the user view → instant optimistic feedback, no notify yet.
        let user_tab: HtmlElement = container
            .query_selector_all("[data-test='smart-view-tab']")
            .unwrap()
            .item(3)
            .unwrap()
            .dyn_into()
            .unwrap();
        user_tab.click();
        next_tick().await;

        assert!(state.agents_view_overlay_pending());
        // The view's query is applied (NameAsc → a, z) and the view is active.
        assert_eq!(rendered_names(&container), vec!["a", "z"]);
        assert_eq!(active_tab_names(&container), vec!["Alpha order"]);
        let update = last_smart_view_update_json();
        assert!(
            update.contains("\"kind\":\"set_active\""),
            "update was {update}"
        );
        assert!(update.contains("\"id\":\"v1\""), "update was {update}");

        // Server confirms: active is now v1 and the prefs carry its query.
        let confirmed_prefs = AgentsViewPreferences {
            sort_mode: AgentSortMode::NameAsc,
            ..AgentsViewPreferences::default()
        };
        let confirmed_views = smart_views(
            vec![v1.clone()],
            Some(SmartViewId::User(UserSmartViewId("v1".to_owned()))),
        );
        state.apply_agents_view_snapshot(
            HOST,
            snapshot_with_views(confirmed_prefs, confirmed_views),
        );
        next_tick().await;

        // Overlay dropped; view stays active and ordered from the server snapshot.
        assert!(!state.agents_view_overlay_pending());
        assert_eq!(active_tab_names(&container), vec!["Alpha order"]);
        assert_eq!(rendered_names(&container), vec!["a", "z"]);
    }

    // (#1 regression) Resetting while the active view's query already equals the
    // default ("All") must NOT transiently de-highlight "All" — the optimistic
    // overlay only clears the highlight when the new query actually diverges.
    #[wasm_bindgen_test]
    async fn reset_on_all_keeps_all_highlighted() {
        install_send_stub();
        let views = smart_views(vec![], Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)));
        let state = primed_state_views(vec![agent("a", 100, false)], Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;
        assert_eq!(active_tab_names(&container), vec!["All"]);

        let reset: HtmlElement = container
            .query_selector("[title='Reset all Agents view preferences to defaults']")
            .unwrap()
            .expect("reset button present")
            .dyn_into()
            .unwrap();
        reset.click();
        next_tick().await;

        // "All" stays highlighted purely from the optimistic overlay (default
        // query still matches the active view), before any server notify.
        assert!(state.agents_view_overlay_pending());
        assert_eq!(active_tab_names(&container), vec!["All"]);
    }

    // (#1, divergence branch) Editing the query to something the active view
    // does NOT match clears the highlight optimistically.
    #[wasm_bindgen_test]
    async fn divergent_edit_clears_highlight_optimistically() {
        install_send_stub();
        // Active is the default-query "All"; toggling "Hide finished" diverges.
        let views = smart_views(vec![], Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)));
        let state = primed_state_views(vec![agent("a", 100, false)], Default::default(), views);
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <AgentMonitorView /> }
        });
        next_tick().await;
        assert_eq!(active_tab_names(&container), vec!["All"]);

        let hide: HtmlElement = container
            .query_selector("[data-test='agent-monitor-hide-finished']")
            .unwrap()
            .expect("hide-finished toggle present")
            .dyn_into()
            .unwrap();
        hide.click();
        next_tick().await;

        // Query now diverges from "All" → no tab highlighted.
        assert!(state.agents_view_overlay_pending());
        assert!(active_tab_names(&container).is_empty());
    }
}
