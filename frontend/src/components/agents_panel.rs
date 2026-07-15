use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentAnnotationTarget, AgentGroup, AgentGroupId, AgentGroupsSnapshot, AgentGroupsUpdate,
    AgentId, AgentsSidebarPreferences, AgentsSidebarProjectVisibility, AgentsViewPreferencesUpdate,
    FrameKind, HostFilterId, ProjectId, SetAgentNamePayload,
};

use crate::components::center_zone::workspace_width;
use crate::components::command_palette::{
    ContextActionId, context_binding, split_creation_availability,
};
use crate::send::{close_agent, compact_agent, send_frame};
use crate::state::{
    AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON, ActiveAgentRef, ActiveProjectRef, AgentInfo,
    AgentOpenToSideResult, AgentsPanelFilters, AppState, CENTER_TABS_DISABLED_REASON,
    CompactionOldInfo, ConnectionStatus, OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON, ProjectInfo,
    StreamingState, TabContent, sort_project_infos,
};

/// Pure predicate used by the Agents panel filter memo. Extracted so the
/// filter behavior can be unit-tested without a Leptos runtime.
pub fn agent_passes_filters(
    agent: &AgentInfo,
    filters: &AgentsPanelFilters,
    active_project: Option<&ActiveProjectRef>,
    streaming: &HashMap<AgentId, StreamingState>,
    turn_active: &HashMap<AgentId, bool>,
    lowercase_query: &str,
) -> bool {
    if filters.hide_sub_agents && agent.parent_agent_id.is_some() {
        return false;
    }
    if filters.hide_inactive {
        let is_active = !agent.started
            || streaming.contains_key(&agent.agent_id)
            || turn_active.get(&agent.agent_id).copied().unwrap_or(false);
        if !is_active {
            return false;
        }
    }
    if !filters.show_other_projects {
        let matches = match active_project {
            None => agent.project_id.is_none(),
            Some(ap) => {
                agent.host_id == ap.host_id && agent.project_id.as_ref() == Some(&ap.project_id)
            }
        };
        if !matches {
            return false;
        }
    }
    if !lowercase_query.is_empty() && !agent.name.to_lowercase().contains(lowercase_query) {
        return false;
    }
    true
}

/// Resolve the effective "show other projects" value from the server-owned
/// project-visibility preference and the active project. `ContextualDefault`
/// keeps today's behavior (Home shows all, in-project shows current only); the
/// two pinned modes are absolute everywhere.
fn effective_show_other_projects(
    visibility: AgentsSidebarProjectVisibility,
    active_project: Option<&ActiveProjectRef>,
) -> bool {
    match visibility {
        AgentsSidebarProjectVisibility::ContextualDefault => active_project.is_none(),
        AgentsSidebarProjectVisibility::AllProjects => true,
        AgentsSidebarProjectVisibility::CurrentProjectOnly => false,
    }
}

/// Project the server-owned sidebar preferences into the predicate input the
/// filter memo consumes. This is a pure derivation, not stored state.
fn sidebar_to_panel_filters(
    sidebar: &AgentsSidebarPreferences,
    active_project: Option<&ActiveProjectRef>,
) -> AgentsPanelFilters {
    AgentsPanelFilters {
        hide_sub_agents: sidebar.hide_sub_agents,
        hide_inactive: sidebar.hide_inactive,
        show_other_projects: effective_show_other_projects(
            sidebar.project_visibility,
            active_project,
        ),
    }
}

/// Install the optimistic overlay for the new sidebar preferences, then send the
/// durable `SetSidebarPreferences` update to the primary local host. The server
/// fans out a full snapshot that reconciles (drops) the overlay.
fn persist_sidebar(state: &AppState, sidebar: AgentsSidebarPreferences) {
    state.set_agents_view_overlay(|overlay| overlay.sidebar = Some(sidebar.clone()));
    let Some(host_id) = state.agents_view_preferences_host.get_untracked() else {
        log::warn!("sidebar preference change with no primary host; overlay only");
        return;
    };
    let Some(stream) = state.host_stream_untracked(&host_id) else {
        log::warn!("primary host {host_id} has no stream; sidebar preference overlay only");
        return;
    };
    let update = AgentsViewPreferencesUpdate::SetSidebarPreferences { sidebar };
    spawn_local(async move {
        if let Err(error) = crate::send::set_agents_view_preferences(&host_id, stream, update).await
        {
            log::error!("failed to send sidebar preference update: {error}");
        }
    });
}

pub(crate) fn backend_class(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Tycode => "backend-badge tycode",
        protocol::BackendKind::Kiro => "backend-badge kiro",
        protocol::BackendKind::Claude => "backend-badge claude",
        protocol::BackendKind::Codex => "backend-badge codex",
        protocol::BackendKind::Antigravity => "backend-badge antigravity",
        protocol::BackendKind::Hermes => "backend-badge hermes",
    }
}

pub(crate) fn backend_label(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Tycode => "Tycode",
        protocol::BackendKind::Kiro => "Kiro",
        protocol::BackendKind::Claude => "Claude",
        protocol::BackendKind::Codex => "Codex",
        protocol::BackendKind::Antigravity => "Antigravity",
        protocol::BackendKind::Hermes => "Hermes",
    }
}

pub(crate) fn relative_time(created_at_ms: u64) -> String {
    let now = js_sys::Date::now() as u64;
    let diff_secs = now.saturating_sub(created_at_ms) / 1000;

    if diff_secs < 60 {
        "just now".to_string()
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86400)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DerivedAgentState {
    Initializing,
    Thinking,
    Idle,
    Compacting,
    Terminated,
}

pub(crate) fn derive_agent_state(
    agent: &AgentInfo,
    streaming: &HashMap<AgentId, StreamingState>,
    turn_active: &HashMap<AgentId, bool>,
    compaction: &HashMap<AgentId, CompactionOldInfo>,
) -> DerivedAgentState {
    if agent.fatal_error.is_some() {
        return DerivedAgentState::Terminated;
    }
    if !agent.started {
        return DerivedAgentState::Initializing;
    }
    if compaction.contains_key(&agent.agent_id) {
        return DerivedAgentState::Compacting;
    }
    let typing = turn_active.get(&agent.agent_id).copied().unwrap_or(false);
    let streaming_open = streaming.contains_key(&agent.agent_id);
    if typing || streaming_open {
        DerivedAgentState::Thinking
    } else {
        DerivedAgentState::Idle
    }
}

pub(crate) fn status_label(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "Initializing",
        DerivedAgentState::Thinking => "Thinking",
        DerivedAgentState::Compacting => "Compacting",
        DerivedAgentState::Idle => "Idle",
        DerivedAgentState::Terminated => "Terminated",
    }
}

pub(crate) fn status_icon(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "\u{25F7}", // ◷ clock (CSS animates)
        DerivedAgentState::Thinking => "\u{25F7}",     // ◷ clock (CSS animates)
        DerivedAgentState::Compacting => "\u{27F2}",   // ⟲ counter-clockwise gapped circle
        DerivedAgentState::Idle => "\u{2713}",         // ✓
        DerivedAgentState::Terminated => "\u{2022}",   // •
    }
}

pub(crate) fn status_class(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "agent-card-status running",
        DerivedAgentState::Thinking => "agent-card-status running",
        DerivedAgentState::Compacting => "agent-card-status running",
        DerivedAgentState::Idle => "agent-card-status completed",
        DerivedAgentState::Terminated => "agent-card-status error",
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AgentTreeGroup {
    parent: AgentInfo,
    children: Vec<AgentInfo>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentProjectSection {
    key: String,
    label: String,
    groups: Vec<AgentTreeGroup>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentHostSection {
    key: String,
    label: String,
    projects: Vec<AgentProjectSection>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentCustomGroupSection {
    group: AgentGroup,
    groups: Vec<AgentTreeGroup>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentSidebarProjection {
    custom_groups: Vec<AgentCustomGroupSection>,
    default_hosts: Vec<AgentHostSection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SidebarAgentRef {
    host_id: String,
    agent_id: AgentId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarKeyboardTarget {
    Agent(SidebarAgentRef),
    Group(AgentGroupId),
    Ungroup,
}

#[derive(Clone)]
struct AgentsPanelInteractions {
    editing_agent: RwSignal<Option<protocol::AgentId>>,
    edit_value: RwSignal<String>,
    collapsed_parents: RwSignal<HashSet<AgentId>>,
    dragged_agent: RwSignal<Option<SidebarAgentRef>>,
    keyboard_agent: RwSignal<Option<SidebarAgentRef>>,
    keyboard_target: RwSignal<Option<SidebarKeyboardTarget>>,
    pending_rename_group_name: RwSignal<Option<String>>,
    group_live_status: RwSignal<String>,
}

fn host_label(host_labels: &HashMap<String, String>, host_id: &str) -> String {
    host_labels
        .get(host_id)
        .cloned()
        .unwrap_or_else(|| host_id.to_owned())
}

fn project_label(
    project_labels: &HashMap<(String, ProjectId), String>,
    host_id: &str,
    project_id: Option<&ProjectId>,
) -> String {
    let Some(project_id) = project_id else {
        return "No project".to_owned();
    };
    project_labels
        .get(&(host_id.to_owned(), project_id.clone()))
        .cloned()
        .unwrap_or_else(|| project_id.0.clone())
}

fn build_parent_child_groups(agents: Vec<AgentInfo>) -> Vec<AgentTreeGroup> {
    let visible_ids: HashSet<AgentId> = agents.iter().map(|a| a.agent_id.clone()).collect();
    let mut children_by_parent: HashMap<AgentId, Vec<AgentInfo>> = HashMap::new();
    let mut top_level: Vec<AgentInfo> = Vec::new();
    let mut orphans: Vec<AgentInfo> = Vec::new();

    for agent in agents {
        match &agent.parent_agent_id {
            Some(parent_id) if visible_ids.contains(parent_id) => {
                children_by_parent
                    .entry(parent_id.clone())
                    .or_default()
                    .push(agent);
            }
            Some(_) => orphans.push(agent),
            None => top_level.push(agent),
        }
    }

    let mut groups = Vec::with_capacity(top_level.len() + orphans.len());
    for parent in top_level {
        let children = children_by_parent
            .remove(&parent.agent_id)
            .unwrap_or_default();
        groups.push(AgentTreeGroup { parent, children });
    }
    for orphan in orphans {
        groups.push(AgentTreeGroup {
            parent: orphan,
            children: Vec::new(),
        });
    }
    groups
}

fn build_sidebar_sections(
    agents: Vec<AgentInfo>,
    configured_hosts: Vec<crate::bridge::ConfiguredHost>,
    mut projects: Vec<ProjectInfo>,
) -> Vec<AgentHostSection> {
    let host_labels: HashMap<String, String> = configured_hosts
        .iter()
        .map(|host| (host.id.clone(), host.label.clone()))
        .collect();
    let known_host_order: Vec<String> = configured_hosts
        .iter()
        .map(|host| host.id.clone())
        .collect();

    sort_project_infos(&mut projects);
    let project_labels: HashMap<(String, ProjectId), String> = projects
        .iter()
        .map(|project| {
            (
                (project.host_id.clone(), project.project.id.clone()),
                project.project.name.clone(),
            )
        })
        .collect();

    let mut leaf_agents: HashMap<(String, Option<ProjectId>), Vec<AgentInfo>> = HashMap::new();
    let mut first_seen_hosts: Vec<String> = Vec::new();
    let mut first_seen_projects: HashMap<String, Vec<Option<ProjectId>>> = HashMap::new();
    for agent in agents {
        let host_id = agent.host_id.clone();
        let project_id = agent.project_id.clone();
        if !known_host_order.contains(&host_id) && !first_seen_hosts.contains(&host_id) {
            first_seen_hosts.push(host_id.clone());
        }
        let project_order = first_seen_projects.entry(host_id.clone()).or_default();
        if !project_order.contains(&project_id) {
            project_order.push(project_id.clone());
        }
        leaf_agents
            .entry((host_id, project_id))
            .or_default()
            .push(agent);
    }

    let mut host_order: Vec<String> = known_host_order
        .into_iter()
        .filter(|host_id| {
            leaf_agents
                .keys()
                .any(|(leaf_host, _)| leaf_host == host_id)
        })
        .collect();
    host_order.extend(first_seen_hosts);

    host_order
        .into_iter()
        .filter_map(|host_id| {
            let mut project_order: Vec<Option<ProjectId>> = projects
                .iter()
                .filter(|project| project.host_id == host_id)
                .filter_map(|project| {
                    let key = (host_id.clone(), Some(project.project.id.clone()));
                    leaf_agents
                        .contains_key(&key)
                        .then_some(Some(project.project.id.clone()))
                })
                .collect();

            if let Some(first_seen) = first_seen_projects.get(&host_id) {
                for project_id in first_seen {
                    if !project_order.contains(project_id) {
                        project_order.push(project_id.clone());
                    }
                }
            }

            let sections: Vec<AgentProjectSection> = project_order
                .into_iter()
                .filter_map(|project_id| {
                    let key = (host_id.clone(), project_id.clone());
                    let agents = leaf_agents.remove(&key)?;
                    let label = project_label(&project_labels, &host_id, project_id.as_ref());
                    Some(AgentProjectSection {
                        key: project_id
                            .as_ref()
                            .map(|id| format!("{}:{}", host_id, id.0))
                            .unwrap_or_else(|| format!("{host_id}:no-project")),
                        label,
                        groups: build_parent_child_groups(agents),
                    })
                })
                .collect();

            (!sections.is_empty()).then(|| AgentHostSection {
                key: host_id.clone(),
                label: host_label(&host_labels, &host_id),
                projects: sections,
            })
        })
        .collect()
}

fn agent_annotation_target(agent: &AgentInfo) -> AgentAnnotationTarget {
    let host_id = HostFilterId(agent.host_id.clone());
    match agent.session_id.clone() {
        Some(session_id) => AgentAnnotationTarget::Session {
            host_id,
            session_id,
        },
        None => AgentAnnotationTarget::TransientAgent {
            host_id,
            agent_id: agent.agent_id.clone(),
        },
    }
}

fn agent_ref(agent: &AgentInfo) -> SidebarAgentRef {
    SidebarAgentRef {
        host_id: agent.host_id.clone(),
        agent_id: agent.agent_id.clone(),
    }
}

fn build_sidebar_projection(
    agents: Vec<AgentInfo>,
    configured_hosts: Vec<crate::bridge::ConfiguredHost>,
    projects: Vec<ProjectInfo>,
    groups_snapshot: AgentGroupsSnapshot,
) -> AgentSidebarProjection {
    let known_groups = groups_snapshot
        .groups
        .iter()
        .map(|group| group.id.clone())
        .collect::<HashSet<_>>();
    let assignments = groups_snapshot
        .assignments
        .into_iter()
        .filter(|assignment| known_groups.contains(&assignment.group_id))
        .map(|assignment| (assignment.target, assignment.group_id))
        .collect::<HashMap<_, _>>();

    let mut grouped_agents = HashMap::<AgentGroupId, Vec<AgentInfo>>::new();
    let mut ungrouped_agents = Vec::new();
    for agent in agents {
        let target = agent_annotation_target(&agent);
        if let Some(group_id) = assignments.get(&target) {
            grouped_agents
                .entry(group_id.clone())
                .or_default()
                .push(agent);
        } else {
            ungrouped_agents.push(agent);
        }
    }

    let custom_groups = groups_snapshot
        .groups
        .into_iter()
        .filter_map(|group| {
            let agents = grouped_agents.remove(&group.id)?;
            let groups = build_parent_child_groups(agents);
            (!groups.is_empty()).then_some(AgentCustomGroupSection { group, groups })
        })
        .collect();

    AgentSidebarProjection {
        custom_groups,
        default_hosts: build_sidebar_sections(ungrouped_agents, configured_hosts, projects),
    }
}

fn agent_for_ref(state: &AppState, agent_ref: &SidebarAgentRef) -> Option<AgentInfo> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| {
                agent.host_id == agent_ref.host_id && agent.agent_id == agent_ref.agent_id
            })
            .cloned()
    })
}

fn target_for_ref(state: &AppState, agent_ref: &SidebarAgentRef) -> Option<AgentAnnotationTarget> {
    agent_for_ref(state, agent_ref).map(|agent| agent_annotation_target(&agent))
}

fn group_id_for_agent(state: &AppState, agent: &AgentInfo) -> Option<AgentGroupId> {
    let target = agent_annotation_target(agent);
    state
        .agents_view_preferences
        .get_untracked()
        .groups
        .assignments
        .into_iter()
        .find(|assignment| assignment.target == target)
        .map(|assignment| assignment.group_id)
}

fn auto_group_name(state: &AppState, dragged: &AgentInfo, target: &AgentInfo) -> String {
    let base = format!("{} + {}", dragged.name, target.name);
    let existing = state
        .agents_view_preferences
        .get_untracked()
        .groups
        .groups
        .into_iter()
        .map(|group| group.name)
        .collect::<HashSet<_>>();
    if !existing.contains(&base) {
        return base;
    }
    let mut suffix = 2_u64;
    loop {
        let candidate = format!("{base} {suffix}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn send_groups_update(state: &AppState, update: AgentGroupsUpdate) {
    let Some(host_id) = state.agents_view_preferences_host.get_untracked() else {
        log::error!("cannot send agent group update before primary preferences host is known");
        return;
    };
    let Some(stream) = state.host_stream_untracked(&host_id) else {
        log::error!("cannot send agent group update without host stream for {host_id}");
        return;
    };
    spawn_local(async move {
        if let Err(error) = crate::send::set_agent_groups(&host_id, stream, update).await {
            log::error!("failed to send SetAgentGroups: {error}");
        }
    });
}

fn drop_agent_on_group(
    state: &AppState,
    dragged: Option<SidebarAgentRef>,
    group_id: AgentGroupId,
    live_status: RwSignal<String>,
) {
    let Some(dragged) = dragged else {
        return;
    };
    let Some(target) = target_for_ref(state, &dragged) else {
        live_status.set("Agent is no longer available to move".to_owned());
        return;
    };
    send_groups_update(
        state,
        AgentGroupsUpdate::MoveTargets {
            group_id: Some(group_id),
            targets: vec![target],
        },
    );
    live_status.set("Moving agent to group".to_owned());
}

fn drop_agent_on_ungroup(
    state: &AppState,
    dragged: Option<SidebarAgentRef>,
    live_status: RwSignal<String>,
) {
    let Some(dragged) = dragged else {
        return;
    };
    let Some(target) = target_for_ref(state, &dragged) else {
        live_status.set("Agent is no longer available to ungroup".to_owned());
        return;
    };
    send_groups_update(
        state,
        AgentGroupsUpdate::MoveTargets {
            group_id: None,
            targets: vec![target],
        },
    );
    live_status.set("Removing agent from its custom group".to_owned());
}

fn drop_agent_on_agent(
    state: &AppState,
    dragged: Option<SidebarAgentRef>,
    target_agent: AgentInfo,
    pending_rename_group_name: RwSignal<Option<String>>,
    live_status: RwSignal<String>,
) {
    let Some(dragged_ref) = dragged else {
        return;
    };
    let target_ref = agent_ref(&target_agent);
    if dragged_ref == target_ref {
        live_status.set("Move cancelled".to_owned());
        return;
    }
    let Some(dragged_agent) = agent_for_ref(state, &dragged_ref) else {
        live_status.set("Agent is no longer available to move".to_owned());
        return;
    };
    let Some(dragged_target) = target_for_ref(state, &dragged_ref) else {
        live_status.set("Agent is no longer available to move".to_owned());
        return;
    };
    if let Some(group_id) = group_id_for_agent(state, &target_agent) {
        send_groups_update(
            state,
            AgentGroupsUpdate::MoveTargets {
                group_id: Some(group_id),
                targets: vec![dragged_target],
            },
        );
        live_status.set("Moving agent to group".to_owned());
    } else {
        let group_name = auto_group_name(state, &dragged_agent, &target_agent);
        pending_rename_group_name.set(Some(group_name.clone()));
        send_groups_update(
            state,
            AgentGroupsUpdate::CreateGroup {
                name: group_name,
                targets: vec![dragged_target, agent_annotation_target(&target_agent)],
            },
        );
        live_status.set("Creating custom group".to_owned());
    }
}

fn focus_relative_drop_target(current: &web_sys::EventTarget, offset: i32) {
    let Some(current_element) = current.dyn_ref::<web_sys::Element>() else {
        return;
    };
    let Some(document) = current_element.owner_document() else {
        return;
    };
    let Ok(nodes) = document.query_selector_all("[data-agent-group-keyboard-target='true']") else {
        return;
    };
    let len = nodes.length();
    if len == 0 {
        return;
    }
    let mut current_index = None;
    for index in 0..len {
        if let Some(node) = nodes.get(index)
            && let Ok(element) = node.dyn_into::<web_sys::Element>()
            && element.is_same_node(Some(current_element))
        {
            current_index = Some(index);
            break;
        }
    }
    let Some(current_index) = current_index else {
        return;
    };
    let next_index = if offset < 0 {
        current_index.checked_sub(1).unwrap_or(len - 1)
    } else {
        (current_index + 1) % len
    };
    if let Some(node) = nodes.get(next_index)
        && let Ok(element) = node.dyn_into::<web_sys::HtmlElement>()
    {
        let _ = element.focus();
    }
}

fn render_ungroup_drop_target(
    state: AppState,
    dragged_agent: RwSignal<Option<SidebarAgentRef>>,
    keyboard_agent: RwSignal<Option<SidebarAgentRef>>,
    keyboard_target: RwSignal<Option<SidebarKeyboardTarget>>,
    group_live_status: RwSignal<String>,
) -> impl IntoView {
    let ungroup_drag_state = state.clone();
    let ungroup_keyboard_state = state;
    let ungroup_on_dragover = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
    };
    let ungroup_on_drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        drop_agent_on_ungroup(
            &ungroup_drag_state,
            dragged_agent.get_untracked(),
            group_live_status,
        );
        dragged_agent.set(None);
    };
    let ungroup_on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        " " | "Enter" => {
            if let Some(agent) = keyboard_agent.get_untracked() {
                ev.prevent_default();
                drop_agent_on_ungroup(&ungroup_keyboard_state, Some(agent), group_live_status);
                keyboard_agent.set(None);
                keyboard_target.set(None);
            }
        }
        "Escape" => {
            keyboard_agent.set(None);
            keyboard_target.set(None);
            group_live_status.set("Move cancelled".to_owned());
        }
        "ArrowDown" | "ArrowRight" => {
            ev.prevent_default();
            focus_relative_drop_target(&ev.target().expect("keydown target"), 1);
        }
        "ArrowUp" | "ArrowLeft" => {
            ev.prevent_default();
            focus_relative_drop_target(&ev.target().expect("keydown target"), -1);
        }
        _ => {}
    };

    view! {
        <div
            class=move || if keyboard_agent.get().is_some() {
                "agent-ungroup-drop-target agent-group-keyboard-active"
            } else {
                "agent-ungroup-drop-target"
            }
            tabindex="0"
            role="button"
            data-agent-group-keyboard-target="true"
            aria-label="Ungroup selected agent"
            aria-dropeffect=move || if keyboard_agent.get().is_some() { "move" } else { "none" }
            on:focus=move |_| keyboard_target.set(Some(SidebarKeyboardTarget::Ungroup))
            on:dragover=ungroup_on_dragover
            on:drop=ungroup_on_drop
            on:keydown=ungroup_on_keydown
        >
            "Ungroup"
        </div>
    }
}

#[component]
pub fn AgentsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());
    // Per-parent collapse state: parents whose children are hidden.
    let collapsed_parents: RwSignal<HashSet<AgentId>> = RwSignal::new(HashSet::new());
    // Editing state lives here so it survives agent list re-renders caused by
    // streaming / turn-active updates. Only one agent can be renamed at a time.
    let editing_agent: RwSignal<Option<protocol::AgentId>> = RwSignal::new(None);
    let edit_value: RwSignal<String> = RwSignal::new(String::new());
    let editing_group: RwSignal<Option<AgentGroupId>> = RwSignal::new(None);
    let group_edit_value: RwSignal<String> = RwSignal::new(String::new());
    let dragged_agent: RwSignal<Option<SidebarAgentRef>> = RwSignal::new(None);
    let drag_hover_group: RwSignal<Option<AgentGroupId>> = RwSignal::new(None);
    let keyboard_agent: RwSignal<Option<SidebarAgentRef>> = RwSignal::new(None);
    let keyboard_target: RwSignal<Option<SidebarKeyboardTarget>> = RwSignal::new(None);
    let pending_rename_group_name: RwSignal<Option<String>> = RwSignal::new(None);
    let group_live_status: RwSignal<String> = RwSignal::new(String::new());
    let interactions = AgentsPanelInteractions {
        editing_agent,
        edit_value,
        collapsed_parents,
        dragged_agent,
        keyboard_agent,
        keyboard_target,
        pending_rename_group_name,
        group_live_status,
    };

    let pending_rename_state = state.clone();
    Effect::new(move |_| {
        let Some(name) = pending_rename_group_name.get() else {
            return;
        };
        let snapshot = pending_rename_state.agents_view_preferences.get().groups;
        if let Some(group) = snapshot.groups.iter().find(|group| group.name == name) {
            editing_group.set(Some(group.id.clone()));
            group_edit_value.set(group.name.clone());
            pending_rename_group_name.set(None);
            group_live_status.set(format!(
                "Created group {}; rename field is focused",
                group.name
            ));
        }
    });

    // Sidebar selectors (hide inactive / hide sub-agents / project visibility)
    // are server-owned preferences (dev-docs/26 §12.1). The effective predicate
    // input is derived per render from the durable snapshot plus the optimistic
    // overlay (`effective_agents_sidebar_preferences`) and the active project —
    // no component-local persistence, so it can never become a second source of
    // truth or a flicker source.
    let filters_state = state.clone();
    let current_filters = Memo::new(move |_| {
        let sidebar = filters_state.effective_agents_sidebar_preferences();
        let active = filters_state.active_project.get();
        sidebar_to_panel_filters(&sidebar, active.as_ref())
    });

    let filter_state = state.clone();
    let filtered_agents = Memo::new(move |_| {
        let active_project = filter_state.active_project.get();
        let query = search.get().to_lowercase();
        let filters = current_filters.get();

        // Read the noisy maps in place via `with` rather than cloning
        // them up-front. The Memo re-runs on every keystroke in the
        // panel-search input, and cloning the streaming/turn-active
        // HashMaps + the full agents Vec on each keystroke was the
        // dominant per-keystroke cost in the audit.
        filter_state.streaming_text.with(|streaming_map| {
            filter_state.agent_turn_active.with(|turn_active_map| {
                filter_state.agents.with(|agents| {
                    agents
                        .iter()
                        .filter(|a| {
                            agent_passes_filters(
                                a,
                                &filters,
                                active_project.as_ref(),
                                streaming_map,
                                turn_active_map,
                                &query,
                            )
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
        })
    });

    let section_state = state.clone();
    let projection = Memo::new(move |_| {
        build_sidebar_projection(
            filtered_agents.get(),
            section_state.configured_hosts.get(),
            section_state.projects.get(),
            section_state.agents_view_preferences.get().groups,
        )
    });

    let on_search = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        search.set(val);
    };

    let toggle_inactive = {
        let state = state.clone();
        move |_| {
            let mut sidebar = state.effective_agents_sidebar_preferences();
            sidebar.hide_inactive = !sidebar.hide_inactive;
            persist_sidebar(&state, sidebar);
        }
    };

    let toggle_sub = {
        let state = state.clone();
        move |_| {
            let mut sidebar = state.effective_agents_sidebar_preferences();
            sidebar.hide_sub_agents = !sidebar.hide_sub_agents;
            persist_sidebar(&state, sidebar);
        }
    };

    // The button reflects the *effective* visibility; a click pins an explicit
    // durable value (the opposite of what is shown) rather than toggling a
    // local bool — so the choice persists across project switches via the
    // server snapshot.
    let toggle_other_projects = {
        let state = state.clone();
        move |_| {
            let active = state.active_project.get_untracked();
            let mut sidebar = state.effective_agents_sidebar_preferences();
            sidebar.project_visibility =
                if effective_show_other_projects(sidebar.project_visibility, active.as_ref()) {
                    AgentsSidebarProjectVisibility::CurrentProjectOnly
                } else {
                    AgentsSidebarProjectVisibility::AllProjects
                };
            persist_sidebar(&state, sidebar);
        }
    };

    view! {
        <div class="panel agents-panel">
            <div class="panel-search">
                <input
                    type="text"
                    class="panel-search-input"
                    placeholder="Filter agents..."
                    prop:value=search
                    on:input=on_search
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                />
            </div>
            <div class="panel-filters">
                <button
                    class=move || if current_filters.get().hide_inactive { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_inactive
                >
                    "Hide inactive"
                </button>
                <button
                    class=move || if current_filters.get().hide_sub_agents { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_sub
                >
                    "Hide sub-agents"
                </button>
                <button
                    class=move || if current_filters.get().show_other_projects { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_other_projects
                >
                    "Show other projects"
                </button>
            </div>
            <div class="panel-content">
                {move || {
                    let projection = projection.get();
                    if projection.custom_groups.is_empty() && projection.default_hosts.is_empty() {
                        view! {
                            <div class="panel-empty">"No agents yet"</div>
                        }.into_any()
                    } else {
                        let default_drop_state = state.clone();
                        let custom_groups = projection.custom_groups;
                        let has_custom_groups = !custom_groups.is_empty();
                        let ungroup_target_state = state.clone();
                        let default_hosts = projection.default_hosts;
                        let custom_groups_view = if custom_groups.is_empty() {
                            ().into_any()
                        } else {
                            view! {
                                    <section class="agent-sidebar-custom-groups-section">
                                        <div class="agent-sidebar-groups-heading">"Groups"</div>
                                        {custom_groups.into_iter().map(|custom_group| {
                                            let group_id = custom_group.group.id.clone();
                                            let group_id_attr = group_id.0.clone();
                                            let group_name = custom_group.group.name.clone();
                                            let section_group_id = group_id.clone();
                                            let header_group_id = group_id.clone();
                                            let hover_class_group_id = group_id.clone();
                                            let hover_enter_group_id = group_id.clone();
                                            let hover_leave_group_id = group_id.clone();
                                            let drop_state = state.clone();
                                            let drop_dragged = dragged_agent;
                                            let drop_status = group_live_status;
                                            let hover_on_drop = drag_hover_group;
                                            let on_group_dragenter = move |ev: web_sys::DragEvent| {
                                                ev.prevent_default();
                                                drag_hover_group.set(Some(hover_enter_group_id.clone()));
                                            };
                                            let on_group_dragleave = move |ev: web_sys::DragEvent| {
                                                ev.prevent_default();
                                                if drag_hover_group.with_untracked(|current| current.as_ref() == Some(&hover_leave_group_id)) {
                                                    drag_hover_group.set(None);
                                                }
                                            };
                                            let on_group_dragover = move |ev: web_sys::DragEvent| {
                                                ev.prevent_default();
                                            };
                                            let on_group_drop = move |ev: web_sys::DragEvent| {
                                                ev.prevent_default();
                                                ev.stop_propagation();
                                                drop_agent_on_group(
                                                    &drop_state,
                                                    drop_dragged.get_untracked(),
                                                    section_group_id.clone(),
                                                    drop_status,
                                                );
                                                drop_dragged.set(None);
                                                hover_on_drop.set(None);
                                            };
                                            let keyboard_state = state.clone();
                                            let keyboard_group_id = group_id.clone();
                                            let keyboard_status = group_live_status;
                                            let on_group_keydown = move |ev: web_sys::KeyboardEvent| {
                                                match ev.key().as_str() {
                                                    " " | "Enter" => {
                                                        if let Some(agent) = keyboard_agent.get_untracked() {
                                                            ev.prevent_default();
                                                            drop_agent_on_group(
                                                                &keyboard_state,
                                                                Some(agent),
                                                                keyboard_group_id.clone(),
                                                                keyboard_status,
                                                            );
                                                            keyboard_agent.set(None);
                                                            keyboard_target.set(None);
                                                        }
                                                    }
                                                    "Escape" => {
                                                        keyboard_agent.set(None);
                                                        keyboard_target.set(None);
                                                        keyboard_status.set("Move cancelled".to_owned());
                                                    }
                                                    "ArrowDown" | "ArrowRight" => {
                                                        ev.prevent_default();
                                                        focus_relative_drop_target(&ev.target().expect("keydown target"), 1);
                                                    }
                                                    "ArrowUp" | "ArrowLeft" => {
                                                        ev.prevent_default();
                                                        focus_relative_drop_target(&ev.target().expect("keydown target"), -1);
                                                    }
                                                    _ => {}
                                                }
                                            };
                                            let rename_group_id = group_id.clone();
                                            let rename_group_name = group_name.clone();
                                            let on_group_rename = move |ev: web_sys::MouseEvent| {
                                                ev.stop_propagation();
                                                group_edit_value.set(rename_group_name.clone());
                                                editing_group.set(Some(rename_group_id.clone()));
                                            };
                                            let delete_state = state.clone();
                                            let delete_group_id = group_id.clone();
                                            let on_group_delete = move |ev: web_sys::MouseEvent| {
                                                ev.stop_propagation();
                                                send_groups_update(
                                                    &delete_state,
                                                    AgentGroupsUpdate::DeleteGroup {
                                                        id: delete_group_id.clone(),
                                                    },
                                                );
                                                group_live_status.set("Deleted group; members return to Host and Project".to_owned());
                                            };
                                            let edit_state_base = state.clone();
                                            let edit_group_id_base = group_id.clone();
                                            let edit_compare_name_base = group_name.clone();
                                            let header_class_group_id = header_group_id.clone();
                                            let header_focus_group_id = header_group_id.clone();
                                            view! {
                                                <section
                                                    class=move || {
                                                        if drag_hover_group.get().as_ref() == Some(&hover_class_group_id) {
                                                            "agent-sidebar-custom-group agent-sidebar-custom-group-drag-over"
                                                        } else {
                                                            "agent-sidebar-custom-group"
                                                        }
                                                    }
                                                    data-group-id=group_id_attr
                                                    on:dragenter=on_group_dragenter
                                                    on:dragleave=on_group_dragleave
                                                    on:dragover=on_group_dragover
                                                    on:drop=on_group_drop
                                                >
                                                    <div
                                                        class=move || if keyboard_target.get().as_ref() == Some(&SidebarKeyboardTarget::Group(header_class_group_id.clone())) {
                                                            "agent-sidebar-custom-group-header agent-group-keyboard-focus"
                                                        } else {
                                                            "agent-sidebar-custom-group-header"
                                                        }
                                                        tabindex="0"
                                                        data-agent-group-keyboard-target="true"
                                                        aria-dropeffect=move || if keyboard_agent.get().is_some() { "move" } else { "none" }
                                                        on:focus=move |_| keyboard_target.set(Some(SidebarKeyboardTarget::Group(header_focus_group_id.clone())))
                                                        on:keydown=on_group_keydown
                                                    >
                                                        {move || {
                                                            if editing_group.with(|current| current.as_ref() == Some(&group_id)) {
                                                                let keydown_state = edit_state_base.clone();
                                                                let keydown_group_id = edit_group_id_base.clone();
                                                                let keydown_compare_name = edit_compare_name_base.clone();
                                                                let on_group_edit_keydown = move |ev: web_sys::KeyboardEvent| {
                                                                    ev.stop_propagation();
                                                                    match ev.key().as_str() {
                                                                        "Enter" => {
                                                                            let new_name = group_edit_value.get_untracked().trim().to_owned();
                                                                            editing_group.set(None);
                                                                            if !new_name.is_empty() && new_name != keydown_compare_name {
                                                                                send_groups_update(
                                                                                    &keydown_state,
                                                                                    AgentGroupsUpdate::RenameGroup {
                                                                                        id: keydown_group_id.clone(),
                                                                                        name: new_name,
                                                                                    },
                                                                                );
                                                                            }
                                                                        }
                                                                        "Escape" => editing_group.set(None),
                                                                        _ => {}
                                                                    }
                                                                };
                                                                let blur_state = edit_state_base.clone();
                                                                let blur_group_id = edit_group_id_base.clone();
                                                                let blur_compare_name = edit_compare_name_base.clone();
                                                                let on_group_edit_blur = move |_: web_sys::FocusEvent| {
                                                                    if editing_group.with_untracked(|current| current.as_ref() != Some(&blur_group_id)) {
                                                                        return;
                                                                    }
                                                                    let new_name = group_edit_value.get_untracked().trim().to_owned();
                                                                    editing_group.set(None);
                                                                    if !new_name.is_empty() && new_name != blur_compare_name {
                                                                        send_groups_update(
                                                                            &blur_state,
                                                                            AgentGroupsUpdate::RenameGroup {
                                                                                id: blur_group_id.clone(),
                                                                                name: new_name,
                                                                            },
                                                                        );
                                                                    }
                                                                };
                                                                view! {
                                                                    <input
                                                                        type="text"
                                                                        class="agent-sidebar-group-name-input"
                                                                        prop:value=move || group_edit_value.get()
                                                                        on:input=move |ev| group_edit_value.set(event_target_value(&ev))
                                                                        on:keydown=on_group_edit_keydown
                                                                        on:blur=on_group_edit_blur
                                                                        autofocus=true
                                                                        spellcheck="false"
                                                                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                                                                        autocapitalize="none"
                                                                        autocomplete="off"
                                                                    />
                                                                }.into_any()
                                                            } else {
                                                                view! {
                                                                    <span class="agent-sidebar-custom-group-name">{group_name.clone()}</span>
                                                                }.into_any()
                                                            }
                                                        }}
                                                        <span class="agent-sidebar-custom-group-actions">
                                                            <button
                                                                type="button"
                                                                class="filter-toggle agent-sidebar-group-rename"
                                                                aria-label="Rename group"
                                                                title="Rename group"
                                                                on:click=on_group_rename
                                                            >
                                                                "\u{270E}"
                                                            </button>
                                                            <button
                                                                type="button"
                                                                class="filter-toggle agent-sidebar-group-delete"
                                                                aria-label="Delete group"
                                                                title="Delete group"
                                                                on:click=on_group_delete
                                                            >
                                                                "\u{00D7}"
                                                            </button>
                                                        </span>
                                                    </div>
                                                    {custom_group.groups.into_iter().map(|group| {
                                                        render_agent_tree_group(
                                                            state.clone(),
                                                            group,
                                                            interactions.clone(),
                                                        )
                                                    }).collect_view()}
                                                </section>
                                            }
                                        }).collect_view()}
                                    </section>
                                }.into_any()
                        };
                        view! {
                            <div class="agent-card-list">
                                <div class="agent-group-live-status" aria-live="polite">
                                    {move || group_live_status.get()}
                                </div>
                                {move || {
                                    let dragging_or_picking =
                                        dragged_agent.get().is_some() || keyboard_agent.get().is_some();
                                    if dragging_or_picking && has_custom_groups {
                                        render_ungroup_drop_target(
                                            ungroup_target_state.clone(),
                                            dragged_agent,
                                            keyboard_agent,
                                            keyboard_target,
                                            group_live_status,
                                        ).into_any()
                                    } else {
                                        ().into_any()
                                    }
                                }}
                                {custom_groups_view}
                                <div
                                    class="agent-sidebar-default-tree"
                                    on:dragover=move |ev: web_sys::DragEvent| {
                                        ev.prevent_default();
                                    }
                                    on:drop=move |ev: web_sys::DragEvent| {
                                        ev.prevent_default();
                                        drop_agent_on_ungroup(
                                            &default_drop_state,
                                            dragged_agent.get_untracked(),
                                            group_live_status,
                                        );
                                        dragged_agent.set(None);
                                    }
                                >
                                {default_hosts.into_iter().map(|host| {
                                    view! {
                                        <section class="agent-sidebar-host-section" data-host-id=host.key>
                                            <div class="agent-sidebar-host-header">{format!("Host: {}", host.label)}</div>
                                            {host.projects.into_iter().map(|project| {
                                                view! {
                                                    <section class="agent-sidebar-project-section" data-project-key=project.key>
                                                        <div class="agent-sidebar-project-header">{format!("Project: {}", project.label)}</div>
                                                        {project.groups.into_iter().map(|group| {
                                                            render_agent_tree_group(
                                                                state.clone(),
                                                                group,
                                                                interactions.clone(),
                                                            )
                                                        }).collect_view()}
                                                    </section>
                                                }
                                            }).collect_view()}
                                        </section>
                                    }
                                }).collect_view()}
                                </div>
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}

fn render_agent_tree_group(
    state: AppState,
    group: AgentTreeGroup,
    interactions: AgentsPanelInteractions,
) -> impl IntoView {
    let parent = group.parent;
    let children = group.children;
    let parent_id = parent.agent_id.clone();
    let group_id = parent_id.0.clone();
    let child_count = children.len();
    let collapsed_parents = interactions.collapsed_parents;
    let parent_view = agent_card(state.clone(), parent, child_count, interactions.clone());
    let children_view = children
        .into_iter()
        .map(|child| {
            let pid = parent_id.clone();
            view! {
                <div
                    class=move || {
                        if collapsed_parents.with(|s| s.contains(&pid)) {
                            "agent-card-child agent-card-child-hidden"
                        } else {
                            "agent-card-child"
                        }
                    }
                >
                    {agent_card(
                        state.clone(),
                        child,
                        0,
                        interactions.clone(),
                    )}
                </div>
            }
        })
        .collect_view();
    view! {
        <div class="agent-card-group" data-agent-id=group_id>
            {parent_view}
            {children_view}
        </div>
    }
}

/// Switch the active project (and host) to the project the clicked agent
/// belongs to, then open (and activate) its chat tab. Ordering is
/// load-bearing: `switch_active_project` replaces `center_zone`, so it MUST
/// run before `open_tab` or the new tab lands in the old project's zone and is
/// discarded. A `None` project switches to Home; the chat tab still carries the
/// agent's `host_id` so host context stays correct. Switching to the already-
/// active project is a no-op (early-returns inside `switch_active_project`).
fn open_agent_chat(state: &AppState, agent: &AgentInfo) {
    state.switch_active_project(agent_project_ref(agent));
    state.open_tab(
        TabContent::chat_with_agent(agent_chat_ref(agent)),
        agent.name.clone(),
        true,
    );
}

pub(crate) fn agent_chat_ref(agent: &AgentInfo) -> ActiveAgentRef {
    ActiveAgentRef {
        host_id: agent.host_id.clone(),
        agent_id: agent.agent_id.clone(),
    }
}

/// The project an agent's chat belongs to. `None` means the agent is not scoped
/// to a project, which matches a Home (no active project) context.
pub(crate) fn agent_project_ref(agent: &AgentInfo) -> Option<ActiveProjectRef> {
    agent.project_id.clone().map(|project_id| ActiveProjectRef {
        host_id: agent.host_id.clone(),
        project_id,
    })
}

/// Ask the state layer whether Open to the Side is available for this agent, and
/// why not when it is not. `None` means eligible; `Some(result)` carries the
/// authoritative refusal.
///
/// This is the *only* thing either agent surface knows about availability. The
/// policy and the reason text live in `AppState`, which shares them with
/// activation, so the rendered `aria-disabled` state and the behavior on click
/// cannot disagree. This function constructs arguments; it decides nothing.
///
/// The query is reactive, so the control re-renders when tabs, the active
/// project, or the layout change.
pub(crate) fn agent_side_open_eligibility(
    state: &AppState,
    agent: &AgentInfo,
) -> Option<AgentOpenToSideResult> {
    state.agent_open_to_side_eligibility(&agent_chat_ref(agent), agent_project_ref(agent).as_ref())
}

/// Why Open to the Side is unavailable, from whichever authority owns the
/// question. Neither is re-derived here.
///
/// The two authorities own genuinely different things: the state layer owns
/// layout policy (tabs, cross-project, occurrence, sole-tab), while the width
/// gate is a property of the *rendered window* that typed state cannot see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentSideOpenBlock {
    /// The state layer refused, with its sentence. A block always carries a
    /// reason — a refusal with nothing to say is not representable.
    State(&'static str),
    Width(&'static str),
}

impl AgentSideOpenBlock {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::State(reason) | Self::Width(reason) => reason,
        }
    }
}

/// What a side-open result means to this UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideOpenOutcome {
    Opened,
    Moved,
    Revealed,
    Refused(&'static str),
}

/// Classify a result, exhaustively. No catch-all, no default, no `unwrap`: if the
/// state layer grows a success or refusal variant, this stops compiling instead
/// of quietly announcing a generic sentence for it. Every arm is reachable.
///
/// Every refusal sentence is the state layer's canonical constant — this file
/// holds no copy of any user-visible text. `MoveTabRefusal` is refusal-only and
/// owns its own sentence (`disabled_reason()` returns `&'static str`, not an
/// `Option`), so move refusals resolve canonically without being enumerated
/// here, and a refusal that carries a success is no longer representable at all.
pub(crate) fn side_open_outcome(result: AgentOpenToSideResult) -> SideOpenOutcome {
    match result {
        AgentOpenToSideResult::Opened { .. } => SideOpenOutcome::Opened,
        AgentOpenToSideResult::Moved { .. } => SideOpenOutcome::Moved,
        AgentOpenToSideResult::Revealed { .. } => SideOpenOutcome::Revealed,
        AgentOpenToSideResult::TabsDisabled => {
            SideOpenOutcome::Refused(CENTER_TABS_DISABLED_REASON)
        }
        AgentOpenToSideResult::CrossProject => {
            SideOpenOutcome::Refused(AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON)
        }
        AgentOpenToSideResult::NothingWouldRemain => {
            SideOpenOutcome::Refused(OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON)
        }
        AgentOpenToSideResult::MoveRefused(refusal) => {
            SideOpenOutcome::Refused(refusal.disabled_reason())
        }
    }
}

/// The full availability of Open to the Side: the state layer's eligibility
/// composed with the UI-owned width gate. `None` means available.
///
/// `split_creation_availability` gates only *creating* a pane, so an existing
/// split stays actionable in narrow mode — a workspace that has since shrunk is
/// handled by reversible narrow mode, never by disabling the action.
/// **The single composition.** Rendering and activation both go through this, so
/// when a state refusal and a width refusal coexist they cannot disagree about
/// which one the user is told about.
///
/// Precedence is state, then width: an agent that cannot be opened beside *at
/// all* (another project, tabs off, sole tab) is told why, rather than being
/// told to widen a window that would not help it.
pub(crate) fn agent_side_open_block(
    state: &AppState,
    width: Option<f64>,
    agent: &AgentInfo,
) -> Option<AgentSideOpenBlock> {
    if let Some(result) = agent_side_open_eligibility(state, agent) {
        // Eligibility yields only refusals. Classifying exhaustively means a
        // future variant must be decided here; a variant the state layer treats
        // as a success is honestly *not* a block, rather than a generic sentence.
        return match side_open_outcome(result) {
            SideOpenOutcome::Refused(reason) => Some(AgentSideOpenBlock::State(reason)),
            SideOpenOutcome::Opened | SideOpenOutcome::Moved | SideOpenOutcome::Revealed => None,
        };
    }
    split_creation_availability(state, width)
        .reason()
        .map(AgentSideOpenBlock::Width)
}

/// Activate Open to the Side, rechecking **both** authorities through the very
/// same composition the control rendered from — one call path, one precedence.
///
/// A refusal returns `Err` before state is touched, so neither a window that
/// shrank nor a layout that changed since the last render can create a tab or a
/// split. On the eligible path the state API still rechecks its own policy and
/// returns before mutating, so a layout that changed underneath is caught there
/// too and reported with the same reason text.
pub(crate) fn activate_agent_side_open(
    state: &AppState,
    width: Option<f64>,
    agent: &AgentInfo,
) -> Result<AgentOpenToSideResult, &'static str> {
    if let Some(block) = untrack(|| agent_side_open_block(state, width, agent)) {
        return Err(block.reason());
    }
    Ok(state.open_agent_chat_to_side(
        agent_chat_ref(agent),
        agent_project_ref(agent),
        agent.name.clone(),
    ))
}

/// Describe a side-open outcome for a polite live region. Refusals are announced
/// with the state layer's own reason text; every variant is classified, so there
/// is no generic sentence to fall back to.
pub(crate) fn side_open_announcement(name: &str, result: AgentOpenToSideResult) -> String {
    match side_open_outcome(result) {
        SideOpenOutcome::Opened => format!("Opened {name} in the other pane"),
        SideOpenOutcome::Moved => format!("Moved {name} to the other pane"),
        SideOpenOutcome::Revealed => format!("Revealed {name} in the other pane"),
        SideOpenOutcome::Refused(reason) => reason.to_owned(),
    }
}

/// Accessible name for the Open to the Side action on an agent surface.
pub(crate) fn side_open_label(name: &str) -> String {
    format!("Open {name} to the side")
}

/// Tooltip for the action: the refusal reason when it is unavailable, and
/// otherwise the shortcut hint derived from the *bound* chord — so the displayed
/// shortcut is platform-correct (⌘ on macOS, Ctrl elsewhere) and cannot drift
/// from the chord that actually matches.
pub(crate) fn side_open_title(block: Option<AgentSideOpenBlock>) -> String {
    match block {
        Some(block) => block.reason().to_owned(),
        None => format!("Open to the side ({})", side_open_chord_hint()),
    }
}

/// The platform-aware hint for the element-scoped Open to the Side chord.
pub(crate) fn side_open_chord_hint() -> String {
    context_binding(ContextActionId::OpenToSide).chord().hint()
}

/// A ≥44px pointer target, sized inside the component that owns the markup so
/// no shared stylesheet has to change. Inline styles beat the 22px class rules
/// these action buttons otherwise inherit.
pub(crate) const SIDE_OPEN_TARGET_STYLE: &str = "min-width: 44px; min-height: 44px; display: inline-flex; \
     align-items: center; justify-content: center;";

/// Visually hidden, still in the accessibility tree: carries the refusal reason
/// as a description for the disabled action.
pub(crate) const SIDE_OPEN_REASON_STYLE: &str = "position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; \
     overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; border: 0;";

fn agent_card(
    state: AppState,
    agent: AgentInfo,
    child_count: usize,
    interactions: AgentsPanelInteractions,
) -> impl IntoView {
    let editing_agent = interactions.editing_agent;
    let edit_value = interactions.edit_value;
    let collapsed_parents = interactions.collapsed_parents;
    let dragged_agent = interactions.dragged_agent;
    let keyboard_agent = interactions.keyboard_agent;
    let keyboard_target = interactions.keyboard_target;
    let pending_rename_group_name = interactions.pending_rename_group_name;
    let group_live_status = interactions.group_live_status;
    let agent_id = agent.agent_id.clone();
    let name = agent.name.clone();
    let backend = agent.backend_kind;
    let is_side_question = matches!(agent.origin, protocol::AgentOrigin::SideQuestion);
    let workflow_badge_title = agent
        .workflow
        .as_ref()
        .map(|metadata| format!("Workflow run {}", metadata.workflow_run_id));
    let created = agent.created_at_ms;
    let custom_agent_id = agent.custom_agent_id.clone();
    let custom_agent_host_id = agent.host_id.clone();
    let custom_agent_state = state.clone();
    let custom_agent_name = move || {
        custom_agent_id.as_ref().and_then(|id| {
            custom_agent_state
                .custom_agents
                .get()
                .get(&custom_agent_host_id)
                .and_then(|map| map.get(id).map(|a| a.name.clone()))
        })
    };

    let error_msg = agent.fatal_error.as_ref().map(|msg| {
        let truncated: String = msg.chars().take(80).collect();
        truncated
    });

    let click_state = state.clone();
    let click_agent = agent.clone();
    let on_click = move |_: web_sys::MouseEvent| {
        // Switch to the agent's own project/host first, then open (and
        // activate) its chat tab; `active_agent` follows from the Memo over
        // `center_zone`. See `open_agent_chat` for the load-bearing ordering.
        open_agent_chat(&click_state, &click_agent);
    };

    // Open to the Side. Availability composes the state layer's eligibility with
    // the UI-owned width gate; activation rechecks both.
    let side_width = workspace_width();
    let side_block_state = state.clone();
    let side_block_agent = agent.clone();
    let side_block = Memo::new(move |_| {
        agent_side_open_block(&side_block_state, side_width.get(), &side_block_agent)
    });
    let side_disabled = move || side_block.get().is_some();
    let side_reason_id = format!("agent-open-side-reason-{}", agent.agent_id.0);
    let side_label = side_open_label(&name);

    // A refusal the user cannot perceive is the same as a dead control: every
    // refusal is rendered as visible text on the card *and* announced politely.
    let side_refusal: RwSignal<Option<&'static str>> = RwSignal::new(None);

    let side_state = state.clone();
    let side_agent = agent.clone();
    let side_name = name.clone();
    let open_to_side = move || {
        // Rechecks the width gate and then the state API, so neither a window
        // that shrank nor a layout that changed since the last render can be
        // acted on.
        match activate_agent_side_open(&side_state, side_width.get_untracked(), &side_agent) {
            Ok(result) => {
                side_refusal.set(result.disabled_reason());
                group_live_status.set(side_open_announcement(&side_name, result));
            }
            Err(reason) => {
                side_refusal.set(Some(reason));
                group_live_status.set(reason.to_owned());
            }
        }
    };

    let kd_state = state.clone();
    let kd_agent = agent.clone();
    let kd_side = open_to_side.clone();
    let on_keydown_card = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
            // Element-scoped chord (dev-docs/32 §12). The chat composer owns
            // Cmd/Ctrl+Enter for send/steer, so this must never reach a global
            // handler.
            ev.prevent_default();
            ev.stop_propagation();
            kd_side();
            return;
        }
        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            // Identical behavior to the mouse handler so the two can't drift.
            open_agent_chat(&kd_state, &kd_agent);
        }
    };

    let on_side_click = {
        let open_to_side = open_to_side.clone();
        move |ev: web_sys::MouseEvent| {
            // The card itself would otherwise open the chat in the focused pane.
            ev.stop_propagation();
            open_to_side();
        }
    };

    let drag_ref = agent_ref(&agent);
    let drag_name = name.clone();
    let on_dragstart = {
        let drag_ref = drag_ref.clone();
        let drag_name = drag_name.clone();
        move |ev: web_sys::DragEvent| {
            dragged_agent.set(Some(drag_ref.clone()));
            group_live_status.set(format!("Moving {drag_name}"));
            if let Some(data_transfer) = ev.data_transfer() {
                data_transfer.set_effect_allowed("move");
                let _ = data_transfer.set_data("text/plain", &drag_ref.agent_id.0);
            }
        }
    };
    let on_dragover_agent = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        ev.stop_propagation();
    };
    let drop_state = state.clone();
    let drop_agent = agent.clone();
    let on_drop_agent = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        drop_agent_on_agent(
            &drop_state,
            dragged_agent.get_untracked(),
            drop_agent.clone(),
            pending_rename_group_name,
            group_live_status,
        );
        dragged_agent.set(None);
    };
    let on_dragend = move |_: web_sys::DragEvent| {
        dragged_agent.set(None);
    };

    let move_handle_ref = drag_ref.clone();
    let move_handle_name = name.clone();
    let move_handle_state = state.clone();
    let move_handle_agent = agent.clone();
    let on_move_handle_keydown = move |ev: web_sys::KeyboardEvent| {
        ev.stop_propagation();
        match ev.key().as_str() {
            " " | "Enter" => {
                ev.prevent_default();
                if let Some(picked) = keyboard_agent.get_untracked() {
                    if picked == move_handle_ref {
                        keyboard_agent.set(None);
                        keyboard_target.set(None);
                        group_live_status.set("Move cancelled".to_owned());
                    } else {
                        drop_agent_on_agent(
                            &move_handle_state,
                            Some(picked),
                            move_handle_agent.clone(),
                            pending_rename_group_name,
                            group_live_status,
                        );
                        keyboard_agent.set(None);
                        keyboard_target.set(None);
                    }
                } else {
                    keyboard_agent.set(Some(move_handle_ref.clone()));
                    keyboard_target
                        .set(Some(SidebarKeyboardTarget::Agent(move_handle_ref.clone())));
                    group_live_status.set(format!(
                        "Picked up {move_handle_name}. Move focus to a group, agent, or Ungroup and press Space or Enter."
                    ));
                }
            }
            "Escape" => {
                keyboard_agent.set(None);
                keyboard_target.set(None);
                group_live_status.set("Move cancelled".to_owned());
            }
            "ArrowDown" | "ArrowRight" => {
                ev.prevent_default();
                focus_relative_drop_target(&ev.target().expect("keydown target"), 1);
            }
            "ArrowUp" | "ArrowLeft" => {
                ev.prevent_default();
                focus_relative_drop_target(&ev.target().expect("keydown target"), -1);
            }
            _ => {}
        }
    };
    let move_focus_ref = drag_ref.clone();
    let on_move_handle_focus = move |_| {
        keyboard_target.set(Some(SidebarKeyboardTarget::Agent(move_focus_ref.clone())));
    };
    let move_click_name = name.clone();
    let class_drag_ref = drag_ref.clone();
    let aria_drag_ref = drag_ref.clone();
    let click_drag_ref = drag_ref.clone();

    let input_ref = NodeRef::<leptos::html::Input>::new();

    let agent_id_for_effect = agent_id.clone();
    // Auto-focus and select-all when editing mode activates.
    Effect::new(move |_| {
        if editing_agent.with(|e| e.as_ref() == Some(&agent_id_for_effect))
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
            el.select();
        }
    });

    let rename_name = name.clone();
    let agent_id_for_rename = agent_id.clone();
    let on_rename = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        edit_value.set(rename_name.clone());
        editing_agent.set(Some(agent_id_for_rename.clone()));
    };

    let host_id_for_edit = agent.host_id.clone();
    let stream_for_edit = agent.instance_stream.clone();

    let close_host_id = agent.host_id.clone();
    let close_stream = agent.instance_stream.clone();
    let close_name = name.clone();
    let close_agent_id = agent_id.clone();
    let close_state = state.clone();
    let on_close = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let is_active = close_state
            .active_agent
            .with_untracked(|a| a.as_ref().is_some_and(|a| a.agent_id == close_agent_id));
        let has_draft = is_active
            && !close_state
                .chat_input
                .with_untracked(|s| s.trim().is_empty());
        let message = if has_draft {
            format!(
                "Close agent \"{}\"?\n\nYou have unsent input — it will be discarded. Continue?",
                close_name
            )
        } else {
            format!("Close agent \"{}\"?", close_name)
        };
        let host_id = close_host_id.clone();
        let stream = close_stream.clone();
        spawn_local(async move {
            if !crate::bridge::confirm_dialog("Close agent", &message).await {
                return;
            }
            if let Err(e) = close_agent(&host_id, stream).await {
                log::error!("failed to send CloseAgent: {e}");
            }
        });
    };

    let agent_for_derived = agent.clone();
    let derived = {
        let streaming = state.streaming_text;
        let turn_active = state.agent_turn_active;
        let compaction = state.compaction_in_progress;
        move || {
            compaction.with(|compaction| {
                turn_active.with(|turn_active| {
                    streaming.with(|streaming| {
                        derive_agent_state(&agent_for_derived, streaming, turn_active, compaction)
                    })
                })
            })
        }
    };

    let status_class_sig = {
        let derived = derived.clone();
        move || status_class(&derived())
    };
    let status_icon_sig = {
        let derived = derived.clone();
        move || status_icon(&derived())
    };

    // Compact (Compact/Rotate) action — gated on the agent being idle on a
    // connected host with at least one chat row, and not already mid-
    // compaction. Hidden when gating fails so the button surface mirrors
    // the existing hover-revealed Close (`agent-card-action`) UX.
    let can_compact = {
        let host_id = agent.host_id.clone();
        let agent_id = agent_id.clone();
        let derived = derived.clone();
        let state = state.clone();
        move || {
            if !matches!(
                state.connection_status_for_host(&host_id),
                ConnectionStatus::Connected
            ) {
                return false;
            }
            if state
                .chat_rows
                .with(|map| map.get(&agent_id).is_none_or(|rows| rows.is_empty()))
            {
                return false;
            }
            matches!(derived(), DerivedAgentState::Idle)
        }
    };
    let compact_host_id = agent.host_id.clone();
    let compact_agent_id = agent_id.clone();
    let compact_agent_stream = agent.instance_stream.clone();
    let compact_name = name.clone();
    let compact_state = state.clone();
    let on_compact = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let host_id = compact_host_id.clone();
        let aid = compact_agent_id.clone();
        let agent_stream = compact_agent_stream.clone();
        // The server marks the predecessor session non-resumable as
        // part of the compaction protocol, so don't promise the user
        // they can pick it back up. The summary remains visible in
        // Sessions as a read-only record of what was kept.
        let message = format!(
            "Compact agent \"{}\"?\n\nThe agent will write a summary of context worth keeping and a fresh replacement will start from that summary. The original session is closed and kept in Sessions as a read-only record — you can view it, but it can't be resumed.",
            compact_name
        );
        let state = compact_state.clone();
        spawn_local(async move {
            if !crate::bridge::confirm_dialog("Compact agent", &message).await {
                return;
            }
            state.mark_compaction_started(&host_id, aid.clone());
            if let Err(e) = compact_agent(&host_id, agent_stream).await {
                log::error!("failed to send AgentCompact: {e}");
                state.finish_compaction_failure(aid, e);
            }
        });
    };

    let compaction_error_msg = {
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || state.compaction_errors.with(|m| m.get(&agent_id).cloned())
    };

    let agent_id_for_editing_block = agent_id.clone();

    view! {
        <div
            class=move || {
                if keyboard_target.get().as_ref() == Some(&SidebarKeyboardTarget::Agent(class_drag_ref.clone())) {
                    "agent-card agent-group-keyboard-focus"
                } else {
                    "agent-card"
                }
            }
            tabindex="0"
            role="button"
            draggable="true"
            aria-dropeffect=move || if keyboard_agent.get().is_some() { "move" } else { "none" }
            on:click=on_click
            on:keydown=on_keydown_card
            on:dragstart=on_dragstart
            on:dragover=on_dragover_agent
            on:drop=on_drop_agent
            on:dragend=on_dragend
        >
            <div class="agent-card-top">
                <div class="agent-card-top-main">
                {move || {
                    if editing_agent.with(|e| e.as_ref() == Some(&agent_id_for_editing_block)) {
                        let host_id = host_id_for_edit.clone();
                        let stream = stream_for_edit.clone();
                        let compare = name.clone();
                        let agent_id_for_blur = agent_id_for_editing_block.clone();
                        let on_keydown = move |ev: web_sys::KeyboardEvent| {
                            ev.stop_propagation();
                            match ev.key().as_str() {
                                "Enter" => {
                                    let new_name = edit_value.get_untracked().trim().to_string();
                                    editing_agent.set(None);
                                    if !new_name.is_empty() && new_name != compare {
                                        let host_id = host_id.clone();
                                        let stream = stream.clone();
                                        spawn_local(async move {
                                            if let Err(e) = send_frame(
                                                &host_id,
                                                stream,
                                                FrameKind::SetAgentName,
                                                &SetAgentNamePayload { name: new_name },
                                            )
                                            .await
                                            {
                                                log::error!("failed to send SetAgentName: {e}");
                                            }
                                        });
                                    }
                                }
                                "Escape" => editing_agent.set(None),
                                _ => {}
                            }
                        };
                        let on_blur = {
                            let host_id = host_id_for_edit.clone();
                            let stream = stream_for_edit.clone();
                            let compare = name.clone();
                            move |_: web_sys::FocusEvent| {
                                // Guard against double-send when Enter already committed.
                                if editing_agent.with_untracked(|e| e.as_ref() != Some(&agent_id_for_blur)) {
                                    return;
                                }
                                let new_name = edit_value.get_untracked().trim().to_string();
                                editing_agent.set(None);
                                if !new_name.is_empty() && new_name != compare {
                                    let host_id = host_id.clone();
                                    let stream = stream.clone();
                                    spawn_local(async move {
                                        if let Err(e) = send_frame(
                                            &host_id,
                                            stream,
                                            FrameKind::SetAgentName,
                                            &SetAgentNamePayload { name: new_name },
                                        )
                                        .await
                                        {
                                            log::error!("failed to send SetAgentName: {e}");
                                        }
                                    });
                                }
                            }
                        };
                        view! {
                            <input
                                type="text"
                                class="agent-card-name-input"
                                node_ref=input_ref
                                prop:value=move || edit_value.get()
                                on:input=move |ev| edit_value.set(event_target_value(&ev))
                                on:keydown=on_keydown
                                on:blur=on_blur
                                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                                spellcheck="false"
                                {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                                autocapitalize="none"
                                autocomplete="off"
                            />
                        }.into_any()
                    } else {
                        view! {
                            <span class="agent-card-name">{name.clone()}</span>
                        }.into_any()
                    }
                }}
                {(child_count > 0).then(|| {
                    let agent_id_col = agent_id.clone();
                    let agent_id_icon = agent_id.clone();
                    let toggle = move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        let id = agent_id_col.clone();
                        collapsed_parents.update(|set| {
                            if set.contains(&id) {
                                set.remove(&id);
                            } else {
                                set.insert(id);
                            }
                        });
                    };
                    view! {
                        <span class="agent-card-child-badge">
                            <span class="agent-child-count">{child_count}</span>
                            <button
                                type="button"
                                class="agent-card-collapse-toggle"
                                title="Toggle sub-agents"
                                on:click=toggle
                                on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                            >
                                {move || if collapsed_parents.with(|s| s.contains(&agent_id_icon)) {
                                    "\u{25B6}"
                                } else {
                                    "\u{25BC}"
                                }}
                            </button>
                        </span>
                    }
                })}
                </div>
                <div class="agent-card-top-actions">
                    <button
                        type="button"
                        class="filter-toggle agent-card-move agent-card-action"
                        title="Move agent to group"
                        aria-label="Move agent to group"
                        aria-grabbed=move || {
                            keyboard_agent
                                .get()
                                .as_ref()
                                .is_some_and(|picked| picked == &aria_drag_ref)
                                .to_string()
                        }
                        data-agent-group-keyboard-target="true"
                        on:focus=on_move_handle_focus
                        on:keydown=on_move_handle_keydown
                        on:click=move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            if keyboard_agent.get_untracked().as_ref() == Some(&click_drag_ref) {
                                keyboard_agent.set(None);
                                keyboard_target.set(None);
                                group_live_status.set("Move cancelled".to_owned());
                            } else {
                                keyboard_agent.set(Some(click_drag_ref.clone()));
                                keyboard_target.set(Some(SidebarKeyboardTarget::Agent(click_drag_ref.clone())));
                                group_live_status.set(format!(
                                    "Picked up {}. Move focus to a group, agent, or Ungroup and press Space or Enter.",
                                    move_click_name
                                ));
                            }
                        }
                    >
                        "\u{2630}"
                    </button>
                    <button
                        type="button"
                        class="filter-toggle agent-card-action"
                        title="Rename agent"
                        aria-label="Rename agent"
                        on:click=on_rename
                    >
                        "\u{270E}"
                    </button>
                    <button
                        type="button"
                        class="filter-toggle agent-card-action agent-card-open-side"
                        style=SIDE_OPEN_TARGET_STYLE
                        data-test="agent-open-side"
                        aria-label=side_label
                        aria-disabled=move || side_disabled().then_some("true")
                        aria-describedby={
                            let side_reason_id = side_reason_id.clone();
                            move || side_disabled().then(|| side_reason_id.clone())
                        }
                        title=move || side_open_title(side_block.get())
                        on:click=on_side_click
                        on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                    >
                        <span aria-hidden="true">"\u{29C9}"</span>
                    </button>
                    {
                        let side_reason_id = side_reason_id.clone();
                        move || side_block.get().map(|block| view! {
                            <span id=side_reason_id.clone() style=SIDE_OPEN_REASON_STYLE>
                                {block.reason()}
                            </span>
                        })
                    }
                    {move || can_compact().then(|| view! {
                        <button
                            type="button"
                            class="filter-toggle agent-card-compact agent-card-action"
                            title="Compact agent"
                            aria-label="Compact agent"
                            on:click=on_compact.clone()
                            on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                        >
                            "\u{27F2}"
                        </button>
                    })}
                    <button
                        type="button"
                        class="filter-toggle agent-card-close agent-card-action"
                        title="Close agent"
                        aria-label="Close agent"
                        on:click=on_close
                        on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                    >
                        "\u{00D7}"
                    </button>
                </div>
            </div>
            <div class="agent-card-bottom">
                <span class=status_class_sig>{status_icon_sig}</span>
                <span class="agent-card-time">{relative_time(created)}</span>
                {move || custom_agent_name().map(|n| {
                    let title = format!("Custom agent: {n}");
                    view! {
                        <span class="agent-card-custom-agent" title=title>{n}</span>
                    }
                })}
                {is_side_question.then(|| view! {
                    <span
                        class="agent-card-side-question-badge"
                        title="Fork + send — forked from another agent's session"
                    >
                        "Aside"
                    </span>
                })}
                {workflow_badge_title.map(|title| view! {
                    <span class="agent-card-workflow-badge" title=title>"Workflow"</span>
                })}
                <span class={format!("{} agent-card-backend", backend_class(backend))}>{backend_label(backend)}</span>
            </div>
            {error_msg.map(|msg| view! {
                <div class="agent-card-error">{msg}</div>
            })}
            {move || compaction_error_msg().map(|msg| view! {
                <div class="agent-card-error agent-card-error-compaction">{msg}</div>
            })}
            {move || side_refusal.get().map(|reason| view! {
                <div class="agent-card-error agent-card-side-refusal" role="status">{reason}</div>
            })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        MOVE_ALREADY_IN_TARGET_PANE_REASON, MOVE_RESOURCE_ALREADY_IN_TARGET_REASON, MoveTabRefusal,
        PaneId, TAB_SOURCE_MISSING_REASON, TabId,
    };
    use protocol::{AgentOrigin, BackendKind, ProjectId, StreamPath};

    /// Every refusal `AgentOpenToSideResult` can carry — including `TabsDisabled`
    /// and every `MoveTabRefusal` — classifies to the state layer's **canonical**
    /// constant, and the two things the user actually perceives (the visible
    /// reason on the control, and the live announcement) are that same sentence.
    ///
    /// This file holds no copy of any refusal text, so the assertions name the
    /// canonical constants directly. A reworded constant flows through
    /// automatically; a *new* refusal variant breaks the exhaustive match in
    /// `side_open_outcome` first, and then this list.
    #[test]
    fn side_open_outcome_refusals_use_the_canonical_state_sentence_everywhere() {
        let refusals = [
            (
                AgentOpenToSideResult::TabsDisabled,
                CENTER_TABS_DISABLED_REASON,
            ),
            (
                AgentOpenToSideResult::CrossProject,
                AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON,
            ),
            (
                AgentOpenToSideResult::NothingWouldRemain,
                OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON,
            ),
            (
                AgentOpenToSideResult::MoveRefused(MoveTabRefusal::SourceTabMissing),
                TAB_SOURCE_MISSING_REASON,
            ),
            (
                AgentOpenToSideResult::MoveRefused(MoveTabRefusal::AlreadyInTargetPane),
                MOVE_ALREADY_IN_TARGET_PANE_REASON,
            ),
            (
                AgentOpenToSideResult::MoveRefused(MoveTabRefusal::ResourceAlreadyInTarget {
                    existing: TabId(7),
                }),
                MOVE_RESOURCE_ALREADY_IN_TARGET_REASON,
            ),
        ];

        for (result, canonical) in refusals {
            assert_eq!(
                side_open_outcome(result),
                SideOpenOutcome::Refused(canonical),
                "{result:?} must classify to the canonical sentence"
            );
            assert_eq!(
                result.disabled_reason(),
                Some(canonical),
                "{result:?}: the state layer itself must say the same sentence, so \
                 this UI cannot drift from it"
            );

            // What the user perceives: the tooltip / aria-description on the
            // disabled control, and the polite live announcement on refusal.
            let block = AgentSideOpenBlock::State(canonical);
            assert_eq!(
                block.reason(),
                canonical,
                "{result:?}: the visible reason must be the canonical sentence"
            );
            assert_eq!(
                side_open_title(Some(block)),
                canonical,
                "{result:?}: the tooltip must be the canonical sentence, not a shortcut"
            );
            assert_eq!(
                side_open_announcement("Agent A", result),
                canonical,
                "{result:?}: the announcement must be the same canonical sentence"
            );
        }
    }

    /// Every success classifies as the action it performed, announces that action,
    /// and carries no refusal reason — so a refusal sentence can never leak into a
    /// success announcement. `MoveRefused` is refusal-only now, so a refusal
    /// holding a success is unrepresentable rather than merely unhandled.
    #[test]
    fn side_open_outcome_successes_announce_the_action_and_carry_no_reason() {
        let cases = [
            (
                AgentOpenToSideResult::Opened {
                    tab: TabId(1),
                    pane: PaneId::Secondary,
                },
                SideOpenOutcome::Opened,
                "Opened Agent A in the other pane",
            ),
            (
                AgentOpenToSideResult::Moved {
                    tab: TabId(1),
                    source: PaneId::Primary,
                    target: PaneId::Secondary,
                },
                SideOpenOutcome::Moved,
                "Moved Agent A to the other pane",
            ),
            (
                AgentOpenToSideResult::Revealed {
                    tab: TabId(1),
                    pane: PaneId::Secondary,
                },
                SideOpenOutcome::Revealed,
                "Revealed Agent A in the other pane",
            ),
        ];

        for (result, outcome, announcement) in cases {
            assert_eq!(
                side_open_outcome(result),
                outcome,
                "{result:?} must classify as the action it performed"
            );
            assert_eq!(
                side_open_announcement("Agent A", result),
                announcement,
                "{result:?} must announce that action"
            );
            assert!(
                result.disabled_reason().is_none(),
                "{result:?} is a success and must carry no refusal reason"
            );
        }
    }

    /// Every `MoveTabRefusal` reaches the UI as its canonical sentence. This is
    /// the arm `side_open_outcome` delegates rather than enumerating, so it is
    /// covered directly here.
    #[test]
    fn every_move_tab_refusal_reaches_the_ui_as_its_canonical_sentence() {
        let refusals = [
            (MoveTabRefusal::SourceTabMissing, TAB_SOURCE_MISSING_REASON),
            (
                MoveTabRefusal::AlreadyInTargetPane,
                MOVE_ALREADY_IN_TARGET_PANE_REASON,
            ),
            (
                MoveTabRefusal::ResourceAlreadyInTarget { existing: TabId(3) },
                MOVE_RESOURCE_ALREADY_IN_TARGET_REASON,
            ),
        ];

        for (refusal, canonical) in refusals {
            assert_eq!(
                refusal.disabled_reason(),
                canonical,
                "{refusal:?} must carry the canonical sentence"
            );
            assert_eq!(
                side_open_outcome(AgentOpenToSideResult::MoveRefused(refusal)),
                SideOpenOutcome::Refused(canonical),
                "{refusal:?} must reach the UI as that same sentence"
            );
        }
    }

    fn mk_agent(
        name: &str,
        host: &str,
        project_id: Option<&str>,
        parent: Option<&str>,
        started: bool,
    ) -> AgentInfo {
        AgentInfo {
            host_id: host.to_string(),
            agent_id: AgentId(format!("agent-{name}")),
            name: name.to_string(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Tycode,
            workspace_roots: vec![],
            project_id: project_id.map(|s| ProjectId(s.to_string())),
            parent_agent_id: parent.map(|p| AgentId(p.to_string())),
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath("s".to_string()),
            started,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn active(host: &str, project: &str) -> ActiveProjectRef {
        ActiveProjectRef {
            host_id: host.to_string(),
            project_id: ProjectId(project.to_string()),
        }
    }

    fn no_runtime() -> (HashMap<AgentId, StreamingState>, HashMap<AgentId, bool>) {
        (HashMap::new(), HashMap::new())
    }

    #[test]
    fn hide_sub_agents_drops_children_keeps_parents() {
        let parent = mk_agent("p", "h", Some("proj"), None, true);
        let child = mk_agent("c", "h", Some("proj"), Some("agent-p"), true);
        let (s, t) = no_runtime();
        let filters = AgentsPanelFilters {
            hide_sub_agents: true,
            hide_inactive: false,
            show_other_projects: true,
        };
        assert!(agent_passes_filters(
            &parent,
            &filters,
            Some(&active("h", "proj")),
            &s,
            &t,
            "",
        ));
        assert!(!agent_passes_filters(
            &child,
            &filters,
            Some(&active("h", "proj")),
            &s,
            &t,
            "",
        ));
    }

    #[test]
    fn hide_inactive_keeps_starting_streaming_and_turn_active() {
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: true,
            show_other_projects: true,
        };

        // Not yet started → treated as active (initializing).
        let starting = mk_agent("starting", "h", None, None, false);
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(&starting, &filters, None, &s, &t, ""));

        // Started + streaming.
        let streaming_agent = mk_agent("streaming", "h", None, None, true);
        let mut stream_map: HashMap<AgentId, StreamingState> = HashMap::new();
        stream_map.insert(
            streaming_agent.agent_id.clone(),
            StreamingState {
                agent_name: "streaming".to_string(),
                model: None,
                text: leptos::prelude::ArcRwSignal::new(String::new()),
                reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
            },
        );
        assert!(agent_passes_filters(
            &streaming_agent,
            &filters,
            None,
            &stream_map,
            &t,
            "",
        ));

        // Started + turn active.
        let turn_agent = mk_agent("turn", "h", None, None, true);
        let mut turn_map: HashMap<AgentId, bool> = HashMap::new();
        turn_map.insert(turn_agent.agent_id.clone(), true);
        let (s, _) = no_runtime();
        assert!(agent_passes_filters(
            &turn_agent,
            &filters,
            None,
            &s,
            &turn_map,
            "",
        ));

        // Started, idle, not streaming → hidden.
        let idle = mk_agent("idle", "h", None, None, true);
        let (s, t) = no_runtime();
        assert!(!agent_passes_filters(&idle, &filters, None, &s, &t, ""));
    }

    #[test]
    fn show_other_projects_off_on_home_keeps_only_none_project() {
        assert!(AgentsPanelFilters::defaults_for(None).show_other_projects);
        // Override to simulate user turning it off on Home.
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: false,
        };
        let home_agent = mk_agent("home", "h", None, None, true);
        let project_agent = mk_agent("proj", "h", Some("p1"), None, true);
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(
            &home_agent,
            &filters,
            None,
            &s,
            &t,
            ""
        ));
        assert!(!agent_passes_filters(
            &project_agent,
            &filters,
            None,
            &s,
            &t,
            ""
        ));
    }

    #[test]
    fn show_other_projects_off_in_project_requires_host_and_project_match() {
        let filters = AgentsPanelFilters::defaults_for(Some(&active("h1", "p1")));
        // Specific-project default is false.
        assert!(!filters.show_other_projects);

        let same = mk_agent("same", "h1", Some("p1"), None, true);
        let other_project = mk_agent("other_p", "h1", Some("p2"), None, true);
        let other_host = mk_agent("other_h", "h2", Some("p1"), None, true);
        let home_agent = mk_agent("home", "h1", None, None, true);
        let active_ref = active("h1", "p1");
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(
            &same,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            ""
        ));
        assert!(!agent_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(!agent_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(!agent_passes_filters(
            &home_agent,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
    }

    #[test]
    fn show_other_projects_on_bypasses_project_check() {
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: true,
        };
        let other_project = mk_agent("other_p", "h1", Some("p2"), None, true);
        let other_host = mk_agent("other_h", "h2", Some("p1"), None, true);
        let home_agent = mk_agent("home", "h1", None, None, true);
        let active_ref = active("h1", "p1");
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(agent_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(agent_passes_filters(
            &home_agent,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
    }

    #[test]
    fn search_matches_case_insensitively() {
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: true,
        };
        let agent = mk_agent("Foo Bar", "h", None, None, true);
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(&agent, &filters, None, &s, &t, "foo"));
        assert!(agent_passes_filters(&agent, &filters, None, &s, &t, "bar"));
        assert!(!agent_passes_filters(&agent, &filters, None, &s, &t, "baz"));
        // Empty query passes all.
        assert!(agent_passes_filters(&agent, &filters, None, &s, &t, ""));
    }

    #[test]
    fn defaults_for_home_shows_other_projects_true() {
        assert!(AgentsPanelFilters::defaults_for(None).show_other_projects);
    }

    #[test]
    fn defaults_for_specific_project_shows_other_projects_false() {
        let ap = active("h", "p");
        assert!(!AgentsPanelFilters::defaults_for(Some(&ap)).show_other_projects);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::center_zone::CenterWorkspaceWidth;
    use crate::dispatch::dispatch_envelope;
    use crate::state::{
        ChatMessageEntry, ChatRowHandle, FileResourceKey, OpenTarget, PaneId, TabId,
    };
    use leptos::mount::mount_to;
    use protocol::types::{
        AgentCompactNotifyPayload, AgentCompactStatus, TeamCompactNotifyPayload, TeamCompactStatus,
    };
    use protocol::{
        AgentAnnotationTarget, AgentGroup, AgentGroupAssignment, AgentGroupId, AgentGroupsSnapshot,
        AgentOrigin, AgentsViewPreferences, AgentsViewPreferencesSnapshot, BackendKind,
        ChatMessage, Envelope, HostFilterId, MessageSender, NewAgentPayload, Project, ProjectId,
        ProjectPath, ProjectRootPath, ProjectSource, StreamPath, TeamId, TeamMemberId,
    };
    use serde_json::Value as JsonValue;
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
                "position: absolute; top: 0; left: 0; width: 600px; height: 800px;",
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

    /// Stub `window.__TAURI__.core.invoke` so every call is recorded into
    /// `window.__test_send_calls`, `plugin:dialog|message` resolves to
    /// `"Ok"` (the user clicked OK on the native confirm), and everything
    /// else resolves to undefined. The recorded JS array is returned so
    /// tests can read it after triggering UI actions.
    fn install_send_stub_with_dialog_ok() -> js_sys::Array {
        let calls = js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    if (cmd === 'plugin:dialog|message') {
                        return Promise.resolve('Ok');
                    }
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
            "#,
        )
        .expect("install tauri stub");
        calls.dyn_into::<js_sys::Array>().expect("array")
    }

    /// Walk `window.__test_send_calls` and return `(frame_kind, payload)`
    /// tuples for every `send_host_line` invoke. Mirrors the
    /// `recorded_frames` helper in teams_panel's tests so the assertion
    /// shape stays consistent across the crate.
    fn recorded_frames(calls: &js_sys::Array) -> Vec<(String, JsonValue, String)> {
        let mut out = Vec::new();
        for entry in calls.iter() {
            let arr = entry.dyn_into::<js_sys::Array>().expect("entry array");
            let cmd = arr.get(0).as_string().expect("cmd is string");
            if cmd != "send_host_line" {
                continue;
            }
            let args_json = arr.get(1).as_string().expect("args json string");
            let args: JsonValue = serde_json::from_str(&args_json).expect("args parse");
            let line = args
                .get("line")
                .and_then(|v| v.as_str())
                .expect("line present");
            let envelope: JsonValue = serde_json::from_str(line).expect("envelope parse");
            let kind = envelope
                .get("kind")
                .and_then(|v| v.as_str())
                .expect("kind present")
                .to_string();
            let stream = envelope
                .get("stream")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let payload = envelope.get("payload").cloned().unwrap_or(JsonValue::Null);
            out.push((kind, payload, stream));
        }
        out
    }

    fn last_group_update_json(calls: &js_sys::Array) -> JsonValue {
        recorded_frames(calls)
            .into_iter()
            .filter(|(kind, _, _)| kind == "set_agent_groups")
            .map(|(_, payload, _)| payload["update"].clone())
            .next_back()
            .expect("expected a set_agent_groups frame")
    }

    fn dispatch_dom_event(element: &HtmlElement, event_name: &str) {
        let event = web_sys::Event::new(event_name).expect("event");
        element.dispatch_event(&event).expect("dispatch event");
    }

    fn dispatch_drag_event(element: &HtmlElement, event_name: &str) {
        let event = web_sys::DragEvent::new(event_name).expect("drag event");
        element.dispatch_event(&event).expect("dispatch drag event");
    }

    fn dispatch_key(element: &HtmlElement, key: &str) {
        let escaped_key = serde_json::to_string(key).expect("serialize key");
        let event: web_sys::Event = js_sys::eval(&format!(
            "new KeyboardEvent('keydown', {{ key: {escaped_key}, bubbles: true, cancelable: true }})"
        ))
        .expect("keyboard event")
        .dyn_into()
        .expect("KeyboardEvent is an Event");
        element
            .dispatch_event(&event)
            .expect("dispatch keyboard event");
    }

    /// Synthesize an `Envelope` and feed it through `dispatch_envelope`
    /// for the tests that drive the AgentCompactNotify state machine.
    /// Sequence is advanced per (host, stream) so the seq validator
    /// doesn't reject subsequent frames in the same test.
    fn dispatch_frame<T: serde::Serialize>(
        state: &AppState,
        host_id: &str,
        stream: StreamPath,
        kind: FrameKind,
        seq: u64,
        payload: &T,
    ) {
        let envelope =
            Envelope::from_payload(stream, kind, seq, payload).expect("envelope serialize");
        dispatch_envelope(state, host_id, envelope);
    }

    fn make_app_state(host_id: &str) -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|map| {
            map.insert(host_id.to_owned(), StreamPath(format!("/host/{host_id}")));
        });
        state.connection_statuses.update(|map| {
            map.insert(
                host_id.to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        state
    }

    fn push_agent(state: &AppState, host_id: &str, agent_id: &str, name: &str, started: bool) {
        push_agent_with_scope(state, host_id, agent_id, name, started, None, None);
    }

    fn push_agent_with_scope(
        state: &AppState,
        host_id: &str,
        agent_id: &str,
        name: &str,
        started: bool,
        project_id: Option<&str>,
        parent_agent_id: Option<&str>,
    ) {
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: project_id.map(|id| ProjectId(id.to_owned())),
                parent_agent_id: parent_agent_id.map(|id| AgentId(id.to_owned())),
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                // Mirror the real backend format `/agent/<id>/<uuid>`.
                // Using a stable suffix keeps tests deterministic; the
                // protocol validator only cares about the registered
                // path equality, not the uuid value.
                instance_stream: StreamPath(format!("/agent/{agent_id}/inst")),
                started,
                fatal_error: None,
                activity_summary: Default::default(),
            });
        });
    }

    fn configured_host(id: &str, label: &str) -> crate::bridge::ConfiguredHost {
        crate::bridge::ConfiguredHost {
            id: id.to_owned(),
            label: label.to_owned(),
            transport: if id == "local" {
                crate::bridge::HostTransportConfig::LocalEmbedded
            } else {
                crate::bridge::HostTransportConfig::SshStdio {
                    ssh_destination: id.to_owned(),
                    remote_command: None,
                    lifecycle: Default::default(),
                }
            },
            auto_connect: true,
        }
    }

    fn project_info(host_id: &str, project_id: &str, name: &str, sort_order: u64) -> ProjectInfo {
        ProjectInfo {
            host_id: host_id.to_owned(),
            project: Project {
                id: ProjectId(project_id.to_owned()),
                name: name.to_owned(),
                sort_order,
                source: ProjectSource::Standalone {
                    roots: vec![ProjectRootPath(format!("/tmp/{project_id}"))],
                },
            },
        }
    }

    fn seed_sidebar_group_fixture(state: &AppState) {
        state.configured_hosts.set(vec![
            configured_host("local", "Local Host"),
            configured_host("remote", "Remote Host"),
        ]);
        state.projects.set(vec![
            project_info("local", "alpha", "Alpha Project", 0),
            project_info("local", "beta", "Beta Project", 1),
            project_info("remote", "gamma", "Gamma Project", 0),
        ]);
        push_agent_with_scope(
            state,
            "local",
            "parent-alpha",
            "Parent Alpha Agent",
            true,
            Some("alpha"),
            None,
        );
        push_agent_with_scope(
            state,
            "local",
            "child-alpha",
            "Child Alpha Agent",
            true,
            Some("alpha"),
            Some("parent-alpha"),
        );
        push_agent_with_scope(
            state,
            "local",
            "beta-agent",
            "Beta Agent",
            true,
            Some("beta"),
            None,
        );
        push_agent_with_scope(
            state,
            "remote",
            "gamma-agent",
            "Gamma Agent",
            true,
            Some("gamma"),
            None,
        );
    }

    fn local_target(agent_id: &str) -> AgentAnnotationTarget {
        AgentAnnotationTarget::TransientAgent {
            host_id: HostFilterId("local".to_owned()),
            agent_id: AgentId(agent_id.to_owned()),
        }
    }

    fn apply_group_snapshot(state: &AppState, groups: AgentGroupsSnapshot) {
        state.apply_agents_view_snapshot(
            "local",
            AgentsViewPreferencesSnapshot {
                preferences: AgentsViewPreferences::default(),
                sidebar: Default::default(),
                load_error: None,
                smart_views: Default::default(),
                tags: Default::default(),
                pins: Default::default(),
                groups,
            },
        );
    }

    fn assigned_group(id: &str, name: &str, targets: &[&str]) -> AgentGroupsSnapshot {
        let group_id = AgentGroupId(id.to_owned());
        AgentGroupsSnapshot {
            groups: vec![AgentGroup {
                id: group_id.clone(),
                name: name.to_owned(),
            }],
            assignments: targets
                .iter()
                .map(|target| AgentGroupAssignment {
                    group_id: group_id.clone(),
                    target: local_target(target),
                })
                .collect(),
        }
    }

    fn seed_chat_row(state: &AppState, agent_id: &str) {
        state.chat_rows.update(|m| {
            m.insert(
                AgentId(agent_id.to_owned()),
                vec![ChatRowHandle::new(ChatMessageEntry {
                    message: ChatMessage {
                        message_id: None,
                        timestamp: 0,
                        sender: MessageSender::User,
                        content: "hi".to_owned(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                    tool_requests: Vec::new(),
                })],
            );
        });
    }

    fn compact_btn(container: &HtmlElement) -> Option<HtmlElement> {
        container
            .query_selector(".agent-card-compact")
            .unwrap()
            .map(|e| e.dyn_into::<HtmlElement>().unwrap())
    }

    /// Mount `AgentsPanel` and return the handle. Caller MUST bind the
    /// handle to a local (e.g. `_handle`) — dropping it tears down the
    /// Leptos root, which empties the container and makes any DOM probe
    /// trivially fail.
    fn mount_panel(container: &HtmlElement, state: AppState) -> impl Sized {
        // The center-workspace width is a *thread-local global* (center_zone.rs),
        // so it survives across tests on the shared wasm test thread. Every mount
        // must therefore state its own width precondition, or it silently
        // inherits whatever the previous test measured — a leaked narrow width
        // makes `split_creation_availability` refuse, and every side-open here
        // would quietly do nothing. This mount declares "nothing has measured
        // the workspace", which the width gate treats as wide enough.
        CenterWorkspaceWidth::forget_measurement();
        let state_for_mount = state;
        mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <AgentsPanel /> }
        })
    }

    fn text_position(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("expected rendered text to contain {needle:?}; got {text:?}"))
    }

    /// The sidebar groups visible agents by host, then project, using the
    /// server-provided labels that users see in the rest of the UI.
    #[wasm_bindgen_test]
    async fn sidebar_renders_host_and_project_section_headers() {
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);

        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = container.text_content().unwrap_or_default();
        for expected in [
            "Host: Local Host",
            "Project: Alpha Project",
            "Project: Beta Project",
            "Host: Remote Host",
            "Project: Gamma Project",
        ] {
            assert!(
                text.contains(expected),
                "sidebar should render section header {expected:?}; got {text:?}"
            );
        }

        assert!(
            text_position(&text, "Host: Local Host")
                < text_position(&text, "Project: Alpha Project")
        );
        assert!(
            text_position(&text, "Project: Alpha Project")
                < text_position(&text, "Parent Alpha Agent")
        );
        assert!(
            text_position(&text, "Parent Alpha Agent")
                < text_position(&text, "Project: Beta Project")
        );
        assert!(text_position(&text, "Project: Beta Project") < text_position(&text, "Beta Agent"));
        assert!(text_position(&text, "Beta Agent") < text_position(&text, "Host: Remote Host"));
        assert!(text_position(&text, "Host: Remote Host") < text_position(&text, "Gamma Agent"));
    }

    /// Parent/child sub-agent nesting remains inside the host/project leaf:
    /// the parent keeps its visible child-count affordance and the child stays
    /// under the same project before the next project section starts.
    #[wasm_bindgen_test]
    async fn sidebar_preserves_parent_child_nesting_within_project_leaf() {
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);

        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = container.text_content().unwrap_or_default();
        let alpha = text_position(&text, "Project: Alpha Project");
        let parent = text_position(&text, "Parent Alpha Agent");
        let child = text_position(&text, "Child Alpha Agent");
        let beta = text_position(&text, "Project: Beta Project");
        assert!(
            alpha < parent && parent < child && child < beta,
            "parent and child should render together inside Alpha before Beta; got {text:?}"
        );

        let parent_group = container
            .query_selector("[data-agent-id='parent-alpha']")
            .unwrap()
            .expect("parent group present");
        let group_text = parent_group.text_content().unwrap_or_default();
        assert!(
            group_text.contains("Parent Alpha Agent")
                && group_text.contains("Child Alpha Agent")
                && group_text.contains('1'),
            "parent group should show parent, child, and visible child count; got {group_text:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn custom_group_renders_members_only_in_groups_section() {
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(
            &state,
            assigned_group("review", "Review Group", &["beta-agent"]),
        );

        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let custom_group: HtmlElement = container
            .query_selector(".agent-sidebar-custom-group")
            .unwrap()
            .expect("custom group renders")
            .dyn_into()
            .unwrap();
        let group_text = custom_group.text_content().unwrap_or_default();
        assert!(
            group_text.contains("Review Group") && group_text.contains("Beta Agent"),
            "custom group should show its header and assigned member; got {group_text:?}"
        );
        let default_tree = container
            .query_selector(".agent-sidebar-default-tree")
            .unwrap()
            .expect("default tree renders");
        let default_text = default_tree.text_content().unwrap_or_default();
        assert!(
            default_text.contains("Parent Alpha Agent")
                && default_text.contains("Child Alpha Agent")
                && !default_text.contains("Beta Agent"),
            "grouped agents must not be duplicated in Host/Project; got {default_text:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn dragging_ungrouped_agent_onto_agent_sends_create_group() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(&state, AgentGroupsSnapshot::default());

        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector(".agent-ungroup-drop-target")
                .unwrap()
                .is_none(),
            "Ungroup target should stay hidden when there are no custom groups"
        );

        let beta_card: HtmlElement = container
            .query_selector("[data-agent-id='beta-agent'] .agent-card")
            .unwrap()
            .expect("beta card")
            .dyn_into()
            .unwrap();
        let parent_card: HtmlElement = container
            .query_selector("[data-agent-id='parent-alpha'] .agent-card")
            .unwrap()
            .expect("parent card")
            .dyn_into()
            .unwrap();
        dispatch_drag_event(&beta_card, "dragstart");
        dispatch_drag_event(&parent_card, "drop");
        for _ in 0..4 {
            next_tick().await;
        }

        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "create_group");
        assert_eq!(update["targets"].as_array().expect("targets").len(), 2);
        assert!(
            update["name"]
                .as_str()
                .expect("name")
                .contains("Beta Agent"),
            "new group should receive an automatic member-based name: {update:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn ungroup_target_stays_hidden_for_keyboard_pickup_without_groups() {
        let _calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(&state, AgentGroupsSnapshot::default());

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector(".agent-ungroup-drop-target")
                .unwrap()
                .is_none(),
            "Ungroup target should not clutter the sidebar before keyboard pickup"
        );

        let move_button: HtmlElement = container
            .query_selector("[data-agent-id='beta-agent'] .agent-card-move")
            .unwrap()
            .expect("move handle")
            .dyn_into()
            .unwrap();
        dispatch_key(&move_button, " ");
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector(".agent-ungroup-drop-target")
                .unwrap()
                .is_none(),
            "Ungroup target has nothing to ungroup into without groups, so a \
             keyboard pickup must not reveal it"
        );
    }

    #[wasm_bindgen_test]
    async fn keyboard_pickup_can_drop_on_ungroup_target() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(
            &state,
            assigned_group("review", "Review Group", &["beta-agent"]),
        );

        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector(".agent-ungroup-drop-target")
                .unwrap()
                .is_none(),
            "Ungroup target should stay hidden when groups exist but no move is active"
        );

        let move_button: HtmlElement = container
            .query_selector("[data-agent-id='beta-agent'] .agent-card-move")
            .unwrap()
            .expect("move handle")
            .dyn_into()
            .unwrap();
        dispatch_key(&move_button, " ");
        for _ in 0..4 {
            next_tick().await;
        }
        let ungroup_target: HtmlElement = container
            .query_selector(".agent-ungroup-drop-target")
            .unwrap()
            .expect("ungroup target")
            .dyn_into()
            .unwrap();
        dispatch_key(&ungroup_target, "Enter");
        for _ in 0..4 {
            next_tick().await;
        }

        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "move_targets");
        assert!(update["group_id"].is_null());
        assert_eq!(update["targets"].as_array().expect("targets").len(), 1);
    }

    /// The dashed "Ungroup" drop target is an interaction affordance: it must
    /// appear only while an agent is actively being moved (mouse drag or
    /// keyboard pickup) AND there is a custom group to ungroup out of. A
    /// permanently-visible target that can't accept a drop is the regression
    /// this guards.
    #[wasm_bindgen_test]
    async fn ungroup_drop_target_gated_by_active_interaction() {
        let _calls = install_send_stub_with_dialog_ok();

        // Groups present, but no drag and no keyboard pickup.
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(
            &state,
            assigned_group("review", "Review Group", &["beta-agent"]),
        );
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let count = |container: &HtmlElement| {
            container
                .query_selector_all(".agent-ungroup-drop-target")
                .unwrap()
                .length()
        };

        // Case 1 (the regression): groups exist, no interaction -> 0 targets.
        assert_eq!(
            count(&container),
            0,
            "Ungroup target must stay hidden when groups exist but nothing is being moved"
        );

        // Case 2: an active mouse drag with groups present -> exactly 1 target.
        // Drag a default-tree (ungrouped) agent so the card is draggable.
        let parent_card: HtmlElement = container
            .query_selector("[data-agent-id='parent-alpha'] .agent-card")
            .unwrap()
            .expect("parent card")
            .dyn_into()
            .unwrap();
        dispatch_drag_event(&parent_card, "dragstart");
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            count(&container),
            1,
            "an active drag with groups present should reveal exactly one Ungroup target"
        );

        // Ending the drag tears the target back down.
        dispatch_drag_event(&parent_card, "dragend");
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            count(&container),
            0,
            "Ungroup target must disappear once the drag ends"
        );

        // Case 3: keyboard pickup active but NO custom groups -> 0 targets.
        let container_no_groups = make_container();
        let state_no_groups = make_app_state("local");
        seed_sidebar_group_fixture(&state_no_groups);
        apply_group_snapshot(&state_no_groups, AgentGroupsSnapshot::default());
        let _handle_no_groups = mount_panel(&container_no_groups, state_no_groups);
        for _ in 0..4 {
            next_tick().await;
        }
        let move_button: HtmlElement = container_no_groups
            .query_selector("[data-agent-id='beta-agent'] .agent-card-move")
            .unwrap()
            .expect("move handle")
            .dyn_into()
            .unwrap();
        dispatch_key(&move_button, " ");
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            count(&container_no_groups),
            0,
            "a keyboard pickup with no custom groups has nothing to ungroup into"
        );
    }

    #[wasm_bindgen_test]
    async fn group_rename_and_delete_use_inline_controls_and_snapshots() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(
            &state,
            assigned_group("review", "Review Group", &["beta-agent"]),
        );

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let rename: HtmlElement = container
            .query_selector(".agent-sidebar-group-rename")
            .unwrap()
            .expect("rename button")
            .dyn_into()
            .unwrap();
        rename.click();
        for _ in 0..2 {
            next_tick().await;
        }
        let input_el: HtmlElement = container
            .query_selector(".agent-sidebar-group-name-input")
            .unwrap()
            .expect("rename input")
            .dyn_into()
            .unwrap();
        let input: web_sys::HtmlInputElement = input_el.clone().dyn_into().unwrap();
        input.set_value("Renamed Group");
        dispatch_dom_event(&input_el, "input");
        dispatch_key(&input_el, "Enter");
        for _ in 0..4 {
            next_tick().await;
        }
        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "rename_group");
        assert_eq!(update["id"], "review");
        assert_eq!(update["name"], "Renamed Group");

        apply_group_snapshot(
            &state,
            assigned_group("review", "Renamed Group", &["beta-agent"]),
        );
        for _ in 0..4 {
            next_tick().await;
        }
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Renamed Group"),
            "server snapshot should render renamed group; got {text:?}"
        );

        let delete: HtmlElement = container
            .query_selector(".agent-sidebar-group-delete")
            .unwrap()
            .expect("delete button")
            .dyn_into()
            .unwrap();
        delete.click();
        for _ in 0..4 {
            next_tick().await;
        }
        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "delete_group");
        assert_eq!(update["id"], "review");
    }

    /// The retired Hide finished control must not render in the sidebar.
    #[wasm_bindgen_test]
    async fn sidebar_does_not_render_hide_finished_control() {
        let container = make_container();
        let state = make_app_state("local");
        push_agent(&state, "local", "a-idle", "Agent", true);

        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Hide finished"),
            "sidebar must not render a Hide finished control; got {text:?}"
        );
    }

    /// Idle agent on a connected host with at least one chat row should
    /// expose the Compact action.
    #[wasm_bindgen_test]
    async fn compact_button_visible_when_idle_with_history_and_connected() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-idle", "Agent", true);
        seed_chat_row(&state, "a-idle");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let btn = compact_btn(&container).expect("compact button should render for idle agent");
        assert_eq!(
            btn.get_attribute("aria-label").as_deref(),
            Some("Compact agent"),
            "compact button must keep a labelled affordance"
        );
    }

    /// A side-question agent renders a compact "Aside" badge so the user
    /// can tell it apart from ordinary agents in the sidebar.
    #[wasm_bindgen_test]
    async fn side_question_agent_shows_aside_badge() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-btw", "Side question", true);
        state.agents.update(|agents| {
            if let Some(agent) = agents.iter_mut().find(|a| a.agent_id.0 == "a-btw") {
                agent.origin = AgentOrigin::SideQuestion;
            }
        });
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let badge = container
            .query_selector(".agent-card-side-question-badge")
            .unwrap()
            .expect("side-question agent must render an Aside badge");
        assert_eq!(
            badge.text_content().unwrap_or_default().trim(),
            "Aside",
            "side-question badge text must read Aside"
        );
    }

    /// Initializing (server hasn't echoed AgentStart) — Compact must be
    /// hidden so the user can't fire a rotation before the agent is even
    /// ready.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_initializing() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-init", "Agent", false);
        seed_chat_row(&state, "a-init");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render for the initializing agent"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden while the agent is still initializing"
        );
    }

    /// Thinking (turn active or streaming open) — Compact must be hidden.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_thinking() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-thinking", "Agent", true);
        seed_chat_row(&state, "a-thinking");
        state.agent_turn_active.update(|m| {
            m.insert(AgentId("a-thinking".to_owned()), true);
        });
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render for the thinking agent"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden while the agent is taking a turn"
        );
    }

    /// No chat rows yet — compaction is wasted spend on an unused agent.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_no_chat_history() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-blank", "Agent", true);
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render even with no chat rows"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden for agents that have no chat rows yet"
        );
    }

    /// Disconnected host — Compact must be hidden because the request
    /// can't reach the server.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_host_disconnected() {
        let container = make_container();
        let state = make_app_state("h");
        state.connection_statuses.update(|m| {
            m.insert("h".to_owned(), crate::state::ConnectionStatus::Disconnected);
        });
        push_agent(&state, "h", "a-disc", "Agent", true);
        seed_chat_row(&state, "a-disc");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render even when host is disconnected"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden when the host is disconnected"
        );
    }

    /// Already compacting — Compact button must be hidden so the user
    /// can't double-fire, and the status pill must render the running-
    /// blue style we use elsewhere for in-flight work.
    #[wasm_bindgen_test]
    async fn compacting_state_hides_button_and_shows_running_pill() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-busy", "Agent", true);
        seed_chat_row(&state, "a-busy");
        state.mark_compaction_started("h", AgentId("a-busy".to_owned()));
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden once a compaction is in flight"
        );
        let status_pill: HtmlElement = container
            .query_selector(".agent-card-status")
            .unwrap()
            .expect("status pill present")
            .dyn_into()
            .unwrap();
        let class = status_pill.get_attribute("class").unwrap_or_default();
        assert!(
            class.contains("running"),
            "compacting status pill should use the running class for the blue pulse, got: {class}"
        );
    }

    /// Compaction failure surfaces a non-fatal inline error and the
    /// predecessor agent is back to idle (Compact button is offered
    /// again).
    #[wasm_bindgen_test]
    async fn compaction_failure_shows_inline_error_and_reenables_button() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-fail", "Agent", true);
        seed_chat_row(&state, "a-fail");
        state.finish_compaction_failure(
            AgentId("a-fail".to_owned()),
            "summary backend returned an error".to_owned(),
        );
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let error_row: HtmlElement = container
            .query_selector(".agent-card-error-compaction")
            .unwrap()
            .expect("compaction error footer present")
            .dyn_into()
            .unwrap();
        assert!(
            error_row
                .text_content()
                .unwrap_or_default()
                .contains("summary backend"),
            "error row should display the server-reported reason"
        );
        assert!(
            compact_btn(&container).is_some(),
            "compact button should be offered again after a non-fatal failure"
        );
    }

    /// Clicking Compact through the OK-stubbed confirm dialog actually
    /// sends an `AgentCompact` frame on the *agent's* instance stream
    /// (not the host stream), with a `Default::default()` payload as
    /// per the Backend contract. The local state also flips to
    /// in-flight so the next render shows the running pill.
    #[wasm_bindgen_test]
    async fn clicking_compact_sends_agent_compact_frame_on_agent_stream() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-click", "Agent", true);
        seed_chat_row(&state, "a-click");
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let btn = compact_btn(&container).expect("compact button should render");
        btn.click();
        for _ in 0..8 {
            next_tick().await;
        }

        let frames = recorded_frames(&calls);
        let compact_frames: Vec<_> = frames
            .iter()
            .filter(|(kind, _, _)| kind == &FrameKind::AgentCompact.to_string())
            .collect();
        assert_eq!(
            compact_frames.len(),
            1,
            "exactly one AgentCompact frame should be sent, all frames: {frames:?}"
        );
        let (_, payload, stream) = compact_frames[0];
        assert_eq!(
            stream, "/agent/a-click/inst",
            "AgentCompact must target the agent's instance stream, not the host stream"
        );
        assert_eq!(
            payload,
            &serde_json::json!({}),
            "default AgentCompactPayload omits the optional tuning fields"
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-click".to_owned()))),
            "agent should be flagged as in-flight while the server processes"
        );
    }

    /// The dispatcher's INBOUND_SEQ and INBOUND_PROTOCOL validators are
    /// process-wide thread-locals that persist across wasm tests. Each
    /// compaction test dispatches a fresh `(host_id, stream)` pair, so we
    /// reset that host's seq state AND wipe the protocol validator's
    /// stream registry at the top of every test. Without the protocol
    /// reset, a NewAgent for `/agent/a-new/inst` in one test would trip
    /// the duplicate-stream check in the next test that uses the same
    /// path.
    fn reset_inbound_seqs(state: &AppState, host_id: &str) {
        crate::dispatch::prime_host_for_tests(state, host_id);
    }

    /// Real backend stream format for an agent instance. The protocol
    /// validator rejects agent-stream traffic on streams that were
    /// never registered via NewAgent, so tests that send AgentCompact*
    /// or AgentClosed frames must use stream paths that match the
    /// `/agent/<agent_id>/<uuid>` pattern the server actually emits.
    fn agent_stream(agent_id: &str) -> StreamPath {
        StreamPath(format!("/agent/{agent_id}/inst"))
    }

    /// Dispatch a NewAgent frame so the protocol validator registers
    /// the agent's `/agent/<id>/inst` instance stream. Without this,
    /// subsequent AgentCompactNotify / AgentClosed frames on the agent
    /// stream are rejected as "unknown agent_id". The seq returned is
    /// the next free seq on the `/host/<host_id>` stream so callers
    /// can chain further host-stream frames.
    /// Open the chat tab a user actually ends up with for a new agent, then
    /// register the agent with a real `NewAgent` frame.
    ///
    /// **`NewAgent` never creates a tab.** The dispatcher resolves an *exact*
    /// intent: `upgrade_draft_chat_tab` mutates the one unique draft
    /// (`TabContent::empty_chat()`) the user opened with New Chat, in place —
    /// same `TabId`, same pane, same selection — and a programmatic agent
    /// "never creates or selects a tab from a server event" (dispatch.rs). A
    /// fixture that sends `NewAgent` with no draft open therefore leaves Home
    /// active and no chat tab at all, which is exactly what these tests were
    /// asserting against.
    ///
    /// Returns the upgraded tab's `TabId` — the identity the retarget tests pin.
    fn open_user_chat_via_new_agent(
        state: &AppState,
        host_id: &str,
        agent_id: &str,
        name: &str,
        host_seq: u64,
        created_at_ms: u64,
    ) -> TabId {
        // The user clicks New Chat: this draft is the intent `NewAgent` resolves.
        state.open_tab(TabContent::empty_chat(), "New Chat".to_owned(), true);
        let draft_tab = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id())
            .expect("the New Chat draft tab is open and active");

        register_agent_via_new_agent(state, host_id, agent_id, name, host_seq, created_at_ms);

        // Protocol shape: the draft was upgraded *in place* — not replaced, not
        // duplicated — so its TabId survives and its content now carries exactly
        // the agent this NewAgent announced, on the host that announced it.
        let (tab_id, agent_ref) = state.center_zone.with_untracked(|cz| {
            let tab = cz
                .active_tab()
                .expect("the upgraded chat tab is the active tab");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(agent_ref),
                    ..
                } => agent_ref.clone(),
                other => panic!(
                    "NewAgent must upgrade the draft into a live chat; active tab content is {other:?}"
                ),
            };
            (tab.id, agent_ref)
        });
        assert_eq!(
            tab_id, draft_tab,
            "NewAgent upgrades the draft in place and must preserve its TabId"
        );
        assert_eq!(
            agent_ref.agent_id,
            AgentId(agent_id.to_owned()),
            "the upgraded tab must point at the announced agent"
        );
        assert_eq!(
            agent_ref.host_id, host_id,
            "the upgraded tab must carry the announcing host"
        );
        tab_id
    }

    fn register_agent_via_new_agent(
        state: &AppState,
        host_id: &str,
        agent_id: &str,
        name: &str,
        host_seq: u64,
        created_at_ms: u64,
    ) {
        dispatch_frame(
            state,
            host_id,
            StreamPath(format!("/host/{host_id}")),
            FrameKind::NewAgent,
            host_seq,
            &NewAgentPayload {
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                workspace_roots: Vec::new(),
                custom_agent_id: None,
                team_id: None,
                team_member_id: None,
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                workflow: None,
                created_at_ms,
                instance_stream: agent_stream(agent_id),
                activity_summary: Default::default(),
            },
        );
        // Prime the agent's instance stream so subsequent
        // AgentCompactNotify / AgentClosed / ChatEvent frames pass the
        // bootstrap-first check the protocol validator now enforces.
        crate::dispatch::prime_agent_stream_for_tests(
            state,
            host_id,
            &agent_stream(agent_id),
            &protocol::AgentStartPayload {
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                workspace_roots: Vec::new(),
                custom_agent_id: None,
                team_id: None,
                team_member_id: None,
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                workflow: None,
                created_at_ms,
            },
        );
    }

    /// `AgentCompactNotify` with status `Started` flips the agent into
    /// `compaction_in_progress` even if the user never clicked Compact
    /// (e.g. compaction was kicked off by a server-side rule). Uses a
    /// real `/agent/<id>/<uuid>` stream so the protocol validator
    /// path is exercised, not bypassed.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_started_marks_in_progress() {
        let state = make_app_state("h-started");
        reset_inbound_seqs(&state, "h-started");
        register_agent_via_new_agent(&state, "h-started", "a-old", "Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        dispatch_frame(
            &state,
            "h-started",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Started,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: None,
                new_session_id: None,
                summary_preview: None,
                message: None,
            },
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "Started notify must mark the old agent in-flight"
        );
    }

    /// `Failed` notify clears the in-flight flag and stores the
    /// server-reported reason as a non-fatal error so the card surfaces
    /// it inline without flipping the agent to Terminated.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_failed_clears_in_progress_and_stores_error() {
        let state = make_app_state("h-failed");
        reset_inbound_seqs(&state, "h-failed");
        register_agent_via_new_agent(&state, "h-failed", "a-old", "Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        state.mark_compaction_started("h-failed", AgentId("a-old".to_owned()));
        dispatch_frame(
            &state,
            "h-failed",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Failed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: None,
                new_session_id: None,
                summary_preview: None,
                message: Some("summary backend returned an error".to_owned()),
            },
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "Failed notify must clear the in-flight flag"
        );
        let err = state
            .compaction_errors
            .with(|m| m.get(&AgentId("a-old".to_owned())).cloned())
            .expect("error message stored");
        assert!(err.contains("summary backend"), "got error {err:?}");
    }

    /// `Completed` notify when the replacement's `NewAgent` echo is
    /// already in state retargets every chat tab pointing at the old
    /// agent over to the new one — same TabId / scroll / focus, just a
    /// new agent_ref. Mirrors the `upgrade_pending_team_member_tab`
    /// contract. Uses real `/agent/<id>/<uuid>` streams.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_completed_after_new_agent_retargets_tab() {
        let state = make_app_state("h-after");
        reset_inbound_seqs(&state, "h-after");
        // The user's New Chat draft, upgraded in place by a real NewAgent frame
        // — that upgraded tab is the very user-perceived tab the retarget must
        // preserve.
        let tab_id_before =
            open_user_chat_via_new_agent(&state, "h-after", "a-old", "Old Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        let tabs_before = state.center_zone.with_untracked(|cz| cz.all_tabs().count());
        // User clicks Compact: the fingerprint is captured now. When
        // NewAgent for the replacement arrives next, the fingerprint
        // suppression keeps it from stealing focus / opening a duplicate.
        state.mark_compaction_started("h-after", AgentId("a-old".to_owned()));
        register_agent_via_new_agent(&state, "h-after", "a-new", "Compacted Agent", 1, 1);
        assert_eq!(
            state.center_zone.with_untracked(|cz| cz.all_tabs().count()),
            tabs_before,
            "replacement NewAgent must not open a duplicate tab while compaction is in flight"
        );

        dispatch_frame(
            &state,
            "h-after",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: Some(AgentId("a-new".to_owned())),
                new_session_id: None,
                summary_preview: Some("Worked on the wizard.".to_owned()),
                message: None,
            },
        );

        let (label, ar, tab_id_after) = state.center_zone.with_untracked(|cz| {
            let tab = cz.active_tab().expect("active tab still present");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => ar.clone(),
                _ => panic!("active tab should still be a Chat after retarget"),
            };
            (tab.label.clone(), agent_ref, tab.id)
        });
        assert_eq!(
            tab_id_after, tab_id_before,
            "retarget must preserve the TabId so the tab does not remount"
        );
        assert_eq!(ar.agent_id, AgentId("a-new".to_owned()));
        assert_eq!(label, "Compacted Agent");
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "in-flight flag cleared on Completed"
        );
        assert!(
            state.compaction_pending_completion.with(|m| m.is_empty()),
            "no pending mapping should linger when NewAgent is already in state"
        );
    }

    /// `Completed` notify can race ahead of the replacement's
    /// `NewAgent` echo. When that happens the dispatcher stashes the
    /// (host, new) → old mapping in `compaction_pending_completion`,
    /// and the `NewAgent` arm later flushes it to do the retarget.
    /// This test exercises that ordering using real `/agent/<id>/<uuid>`
    /// streams so the protocol validator path is exercised.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_completed_before_new_agent_defers_then_flushes() {
        let state = make_app_state("h-defer");
        reset_inbound_seqs(&state, "h-defer");
        let tab_id_before =
            open_user_chat_via_new_agent(&state, "h-defer", "a-old", "Old Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        state.mark_compaction_started("h-defer", AgentId("a-old".to_owned()));

        // Completed arrives FIRST, while the replacement isn't in
        // state.agents yet. Note we send on a-old's REAL agent stream
        // — the backend's new contract is that Completed lands while
        // the old stream is still valid (i.e. before AgentClosed
        // invalidates it).
        dispatch_frame(
            &state,
            "h-defer",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: Some(AgentId("a-new".to_owned())),
                new_session_id: None,
                summary_preview: None,
                message: None,
            },
        );
        // The retarget is deferred; the tab still points at the old
        // agent, but the pending mapping is recorded.
        let still_old = state.center_zone.with_untracked(|cz| {
            cz.active_tab().and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => Some(ar.agent_id.clone()),
                _ => None,
            })
        });
        assert_eq!(still_old, Some(AgentId("a-old".to_owned())));
        assert!(
            state
                .compaction_pending_completion
                .with(|m| m.contains_key(&("h-defer".to_owned(), AgentId("a-new".to_owned())))),
            "pending mapping should be recorded until NewAgent arrives"
        );

        // Now the replacement's NewAgent echo lands on the host stream
        // (seq=1 since a-old's NewAgent occupied seq=0). The NewAgent
        // dispatch arm should flush the pending mapping and call
        // finish_compaction_success.
        register_agent_via_new_agent(&state, "h-defer", "a-new", "Compacted Agent", 1, 1);

        let (label, ar, tab_id_after) = state.center_zone.with_untracked(|cz| {
            let tab = cz.active_tab().expect("active tab still present");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => ar.clone(),
                _ => panic!("active tab should still be a Chat after retarget"),
            };
            (tab.label.clone(), agent_ref, tab.id)
        });
        assert_eq!(tab_id_after, tab_id_before, "TabId preserved across flush");
        assert_eq!(ar.agent_id, AgentId("a-new".to_owned()));
        assert_eq!(label, "Compacted Agent");
        assert!(
            state.compaction_pending_completion.with(|m| m.is_empty()),
            "pending mapping must be drained after the NewAgent flush"
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "in-flight flag cleared once retarget finalizes"
        );
    }

    /// Fixed backend contract regression: `NewAgent` (replacement) →
    /// `AgentCompactNotify::Completed` on the old agent's still-valid
    /// stream → `AgentClosed` (old). All frames use real
    /// `/agent/<id>/<uuid>` stream paths so the protocol validator
    /// path is exercised (the validator rejects agent-stream traffic
    /// after `AgentClosed` removes the stream, which is exactly why
    /// the backend must deliver `Completed` BEFORE `AgentClosed`).
    ///
    /// Asserts the user-visible contract:
    ///   1. Replacement `NewAgent` does NOT open a duplicate chat tab.
    ///   2. `Completed` retargets the existing tab to the replacement
    ///      in place — same `TabId`, new `agent_ref`, new label.
    ///   3. The subsequent `AgentClosed` for old does NOT close the
    ///      retargeted tab.
    ///   4. Once `AgentClosed` runs the old agent's transient state
    ///      (agents row, chat_rows, etc.) is gone.
    #[wasm_bindgen_test]
    async fn qa_ordering_new_then_completed_then_close_preserves_tab() {
        let state = make_app_state("h-qa");
        reset_inbound_seqs(&state, "h-qa");
        // The user's New Chat draft, upgraded in place by a real NewAgent.
        let tab_id_before =
            open_user_chat_via_new_agent(&state, "h-qa", "a-old", "Old Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        let tabs_before = state.center_zone.with_untracked(|cz| cz.all_tabs().count());

        // User clicks Compact — fingerprint captured. Replacement
        // NewAgent arrives next; without the dispatcher's fingerprint
        // suppression it would steal focus into a duplicate tab.
        state.mark_compaction_started("h-qa", AgentId("a-old".to_owned()));

        // 1. Replacement NewAgent arrives on /host/h-qa (seq=1 because
        //    a-old's NewAgent occupied seq=0).
        register_agent_via_new_agent(&state, "h-qa", "a-new", "Compacted Agent", 1, 1);
        let after_new_agent_tab_count =
            state.center_zone.with_untracked(|cz| cz.all_tabs().count());
        let after_new_agent_active = state.center_zone.with_untracked(|cz| {
            cz.active_tab().and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => Some(ar.agent_id.clone()),
                _ => None,
            })
        });
        assert_eq!(
            after_new_agent_tab_count, tabs_before,
            "replacement NewAgent must not open a duplicate chat tab"
        );
        assert_eq!(
            after_new_agent_active,
            Some(AgentId("a-old".to_owned())),
            "active tab must still point at the old agent until Completed retargets it"
        );

        // 2. Completed arrives on the OLD agent's instance stream,
        //    while that stream is still valid (the protocol validator
        //    would reject this frame if it arrived after AgentClosed).
        dispatch_frame(
            &state,
            "h-qa",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: Some(AgentId("a-new".to_owned())),
                new_session_id: None,
                summary_preview: Some("Worked on the wizard.".to_owned()),
                message: None,
            },
        );
        let (label, ar, tab_id_after, tabs_after) = state.center_zone.with_untracked(|cz| {
            let tab = cz.active_tab().expect("active tab still present");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => ar.clone(),
                _ => panic!("active tab should be a Chat after retarget"),
            };
            (tab.label.clone(), agent_ref, tab.id, cz.all_tabs().count())
        });
        assert_eq!(
            tab_id_after, tab_id_before,
            "Completed retarget must preserve the TabId so the tab does not remount"
        );
        assert_eq!(
            ar.agent_id,
            AgentId("a-new".to_owned()),
            "tab agent_ref should now point at the replacement"
        );
        assert_eq!(label, "Compacted Agent");
        assert_eq!(
            tabs_after, tabs_before,
            "no duplicate tab introduced through retarget"
        );
        assert!(
            !state
                .compaction_in_progress
                .with_untracked(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "in-flight flag cleared on Completed"
        );

        // 3. AgentClosed for old arrives last. This is the "normal"
        //    close path (compaction_in_progress no longer has a-old),
        //    so we expect transient state for a-old to be cleaned up.
        //    The retargeted tab now points at a-new, so the close
        //    sweep finds no matching Chat tab and must leave it alone.
        // seq=2 on /host/h-qa: a-old=0, a-new=1, AgentClosed=2.
        dispatch_frame(
            &state,
            "h-qa",
            StreamPath("/host/h-qa".to_owned()),
            FrameKind::AgentClosed,
            2,
            &protocol::AgentClosedPayload {
                agent_id: AgentId("a-old".to_owned()),
            },
        );

        // The retargeted tab is still here, still pointing at a-new.
        let (final_label, final_ar, final_tab_id, final_tab_count) =
            state.center_zone.with_untracked(|cz| {
                let tab = cz.active_tab().expect("active tab still present");
                let agent_ref = match &tab.content {
                    TabContent::Chat {
                        agent_ref: Some(ar),
                        ..
                    } => ar.clone(),
                    _ => panic!("active tab should still be a Chat after AgentClosed"),
                };
                (tab.label.clone(), agent_ref, tab.id, cz.all_tabs().count())
            });
        assert_eq!(
            final_tab_id, tab_id_before,
            "AgentClosed must not remount or replace the retargeted tab"
        );
        assert_eq!(
            final_ar.agent_id,
            AgentId("a-new".to_owned()),
            "AgentClosed for the old agent must not flip agent_ref back"
        );
        assert_eq!(final_label, "Compacted Agent");
        assert_eq!(
            final_tab_count, tabs_before,
            "AgentClosed for the old agent must not close the retargeted tab"
        );

        // 4. Old agent transient state cleaned up by the normal
        //    apply_agent_closed path (compaction_in_progress was
        //    empty so no defer; teardown ran immediately).
        assert!(
            state.agents.with_untracked(|agents| agents
                .iter()
                .all(|a| a.agent_id != AgentId("a-old".to_owned()))),
            "old AgentInfo must be cleaned up after AgentClosed"
        );
        assert!(
            !state
                .chat_rows
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "old chat_rows must be cleaned up after AgentClosed"
        );
        assert!(
            !state
                .agent_session_settings
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "old agent_session_settings must be cleaned up after AgentClosed"
        );
        assert!(
            state.compaction_pending_close.with(|set| set.is_empty()),
            "pending-close set must remain empty under the new contract"
        );
    }

    /// Defensive belt: `finalize_compaction_close` cleans up the same
    /// transient maps `apply_agent_closed` does. The new backend
    /// contract delivers `Completed` before `AgentClosed`, so the
    /// deferred-close path normally isn't exercised — but we still
    /// want the cleanup parity intact in case ordering ever inverts.
    /// This drives `finalize_compaction_close` directly via the
    /// state API to keep the assertion narrow and protocol-free.
    #[wasm_bindgen_test]
    async fn finalize_compaction_close_clears_agent_session_settings() {
        let state = make_app_state("h-clean");
        push_agent(&state, "h-clean", "a-old", "Old Agent", true);
        seed_chat_row(&state, "a-old");
        state.agent_session_settings.update(|map| {
            map.insert(
                AgentId("a-old".to_owned()),
                protocol::SessionSettingsValues::default(),
            );
        });
        // Drive the same code path finish_compaction_success calls
        // after retargeting: drop the deferred-close entry's transient
        // state for the old agent.
        state.finish_compaction_success(
            &AgentId("a-old".to_owned()),
            &AgentInfo {
                host_id: "h-clean".to_owned(),
                agent_id: AgentId("a-new".to_owned()),
                name: "New".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-new/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            },
        );
        // Without an entry in compaction_pending_close,
        // finish_compaction_success does NOT call finalize — that's
        // intentional. Add one and re-trigger by calling
        // defer_compaction_close + a synthetic
        // finish_compaction_success.
        state.defer_compaction_close("h-clean", AgentId("a-old".to_owned()));
        state.finish_compaction_success(
            &AgentId("a-old".to_owned()),
            &AgentInfo {
                host_id: "h-clean".to_owned(),
                agent_id: AgentId("a-new".to_owned()),
                name: "New".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-new/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            },
        );
        assert!(
            !state
                .agent_session_settings
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "finalize_compaction_close must drop agent_session_settings for the old agent"
        );
        assert!(
            !state
                .chat_rows
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "finalize_compaction_close must drop chat_rows for the old agent"
        );
        assert!(
            state.agents.with_untracked(|agents| agents
                .iter()
                .all(|a| a.agent_id != AgentId("a-old".to_owned()))),
            "finalize_compaction_close must drop the old AgentInfo"
        );
    }

    /// `TeamCompactNotify::Started` flips every targeted agent into
    /// `compaction_in_progress` even when the user never clicked Compact
    /// in this client (a team compact may have been initiated from
    /// another client / server-side rule). Idempotent if the local
    /// click handler had already marked them.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_started_marks_all_targets_in_progress() {
        let state = make_app_state("h-team-started");
        reset_inbound_seqs(&state, "h-team-started");
        register_agent_via_new_agent(&state, "h-team-started", "a-mgr", "Manager", 0, 0);
        register_agent_via_new_agent(&state, "h-team-started", "a-rep", "Reporter", 1, 1);
        seed_chat_row(&state, "a-mgr");
        seed_chat_row(&state, "a-rep");
        dispatch_frame(
            &state,
            "h-team-started",
            StreamPath("/host/h-team-started".to_owned()),
            FrameKind::TeamCompactNotify,
            2,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Started,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![
                    TeamMemberId("m-mgr".to_owned()),
                    TeamMemberId("m-rep".to_owned()),
                ],
                agent_ids: vec![AgentId("a-mgr".to_owned()), AgentId("a-rep".to_owned())],
                results: Vec::new(),
                message: None,
            },
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-mgr".to_owned()))),
            "Started team notify must mark every targeted agent in-flight (a-mgr)"
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-rep".to_owned()))),
            "Started team notify must mark every targeted agent in-flight (a-rep)"
        );
    }

    /// `TeamCompactNotify::Completed` carries one
    /// `AgentCompactNotifyPayload` per target. The dispatcher must
    /// drive each through the same per-agent state machine: chat tabs
    /// retarget to the new agent, `compaction_in_progress` clears.
    /// Per-agent `AgentCompactNotify` frames are NOT emitted to the
    /// client during a team compact, so this aggregated path is the
    /// only place the UI learns of completion.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_completed_retargets_each_member_tab() {
        let state = make_app_state("h-team-completed");
        reset_inbound_seqs(&state, "h-team-completed");
        register_agent_via_new_agent(&state, "h-team-completed", "a-mgr-old", "Manager", 0, 0);
        register_agent_via_new_agent(&state, "h-team-completed", "a-rep-old", "Reporter", 1, 1);
        seed_chat_row(&state, "a-mgr-old");
        seed_chat_row(&state, "a-rep-old");
        state.mark_compaction_started("h-team-completed", AgentId("a-mgr-old".to_owned()));
        state.mark_compaction_started("h-team-completed", AgentId("a-rep-old".to_owned()));
        // Replacement agents land first (server emits them on the host
        // stream, then sends TeamCompactNotify on the host stream).
        register_agent_via_new_agent(
            &state,
            "h-team-completed",
            "a-mgr-new",
            "Manager (compacted)",
            2,
            2,
        );
        register_agent_via_new_agent(
            &state,
            "h-team-completed",
            "a-rep-new",
            "Reporter (compacted)",
            3,
            3,
        );

        dispatch_frame(
            &state,
            "h-team-completed",
            StreamPath("/host/h-team-completed".to_owned()),
            FrameKind::TeamCompactNotify,
            4,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Completed,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![
                    TeamMemberId("m-mgr".to_owned()),
                    TeamMemberId("m-rep".to_owned()),
                ],
                agent_ids: vec![
                    AgentId("a-mgr-old".to_owned()),
                    AgentId("a-rep-old".to_owned()),
                ],
                results: vec![
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Completed,
                        old_agent_id: AgentId("a-mgr-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: Some(AgentId("a-mgr-new".to_owned())),
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    },
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Completed,
                        old_agent_id: AgentId("a-rep-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: Some(AgentId("a-rep-new".to_owned())),
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    },
                ],
                message: None,
            },
        );

        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-mgr-old".to_owned()))),
            "team Completed must clear in-flight for a-mgr-old"
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-rep-old".to_owned()))),
            "team Completed must clear in-flight for a-rep-old"
        );
        // Each per-agent result drives the same retarget path as a
        // solo compaction. Both old→new mappings must finalize without
        // anything left behind in `compaction_pending_completion`.
        assert!(
            state.compaction_pending_completion.with(|m| m.is_empty()),
            "all per-agent retargets must finalize since both replacements are in state"
        );
    }

    /// Partial `TeamCompactNotify::Failed` — one agent succeeded, one
    /// failed. Each per-agent result must drive its own state path:
    /// the successful one retargets and clears in-flight, the failed
    /// one clears in-flight and surfaces the error message inline so
    /// the per-agent Compact button re-enables.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_failed_applies_per_agent_results() {
        let state = make_app_state("h-team-mixed");
        reset_inbound_seqs(&state, "h-team-mixed");
        register_agent_via_new_agent(&state, "h-team-mixed", "a-ok-old", "OK", 0, 0);
        register_agent_via_new_agent(&state, "h-team-mixed", "a-bad-old", "Bad", 1, 1);
        seed_chat_row(&state, "a-ok-old");
        seed_chat_row(&state, "a-bad-old");
        state.mark_compaction_started("h-team-mixed", AgentId("a-ok-old".to_owned()));
        state.mark_compaction_started("h-team-mixed", AgentId("a-bad-old".to_owned()));
        register_agent_via_new_agent(&state, "h-team-mixed", "a-ok-new", "OK (compacted)", 2, 2);

        dispatch_frame(
            &state,
            "h-team-mixed",
            StreamPath("/host/h-team-mixed".to_owned()),
            FrameKind::TeamCompactNotify,
            3,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Failed,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![
                    TeamMemberId("m-ok".to_owned()),
                    TeamMemberId("m-bad".to_owned()),
                ],
                agent_ids: vec![
                    AgentId("a-ok-old".to_owned()),
                    AgentId("a-bad-old".to_owned()),
                ],
                results: vec![
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Completed,
                        old_agent_id: AgentId("a-ok-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: Some(AgentId("a-ok-new".to_owned())),
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    },
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: AgentId("a-bad-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("summary backend exploded".to_owned()),
                    },
                ],
                message: Some("1 of 2 team agents failed to compact".to_owned()),
            },
        );

        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-ok-old".to_owned()))),
            "successful per-agent result must clear in-flight"
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-bad-old".to_owned()))),
            "failed per-agent result must also clear in-flight (re-enable Compact button)"
        );
        let err = state
            .compaction_errors
            .with(|m| m.get(&AgentId("a-bad-old".to_owned())).cloned())
            .expect("per-agent failure must surface an error for the failed agent");
        assert!(
            err.contains("summary backend"),
            "per-agent error message must come from the result's message, got {err:?}"
        );
        assert!(
            state
                .compaction_errors
                .with(|m| !m.contains_key(&AgentId("a-ok-old".to_owned()))),
            "successful per-agent result must NOT record an error"
        );
    }

    /// Defensive: if the server's `Failed` notify lists an agent in
    /// `agent_ids` but provides no matching `results` entry (e.g. the
    /// per-agent task aborted before producing a payload), the
    /// dispatcher must still clear that agent's in-flight flag using
    /// the team-level message — otherwise the per-agent Compact button
    /// would remain disabled forever.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_missing_result_falls_back_to_team_message() {
        let state = make_app_state("h-team-missing");
        reset_inbound_seqs(&state, "h-team-missing");
        register_agent_via_new_agent(&state, "h-team-missing", "a-orphan", "Orphan", 0, 0);
        seed_chat_row(&state, "a-orphan");
        state.mark_compaction_started("h-team-missing", AgentId("a-orphan".to_owned()));

        dispatch_frame(
            &state,
            "h-team-missing",
            StreamPath("/host/h-team-missing".to_owned()),
            FrameKind::TeamCompactNotify,
            1,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Failed,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![TeamMemberId("m-orphan".to_owned())],
                agent_ids: vec![AgentId("a-orphan".to_owned())],
                results: Vec::new(),
                message: Some("team compaction aborted".to_owned()),
            },
        );

        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-orphan".to_owned()))),
            "missing per-agent result must still clear in-flight via team-level fallback"
        );
        let err = state
            .compaction_errors
            .with(|m| m.get(&AgentId("a-orphan".to_owned())).cloned())
            .expect("team-level message must be surfaced when no per-agent result was emitted");
        assert!(
            err.contains("team compaction aborted"),
            "fallback must use the team-level message, got {err:?}"
        );
    }

    // ── Sidebar selector persistence (server-owned) ──────────────────────────

    fn sidebar_snapshot(sidebar: AgentsSidebarPreferences) -> AgentsViewPreferencesSnapshot {
        AgentsViewPreferencesSnapshot {
            preferences: AgentsViewPreferences::default(),
            sidebar,
            load_error: None,
            smart_views: Default::default(),
            tags: Default::default(),
            pins: Default::default(),
            groups: Default::default(),
        }
    }

    /// Prime an AppState as the primary local host with a server sidebar
    /// preference snapshot, ready to mount the panel and drive selectors.
    fn primed_sidebar_state(sidebar: AgentsSidebarPreferences) -> AppState {
        let state = make_app_state("local");
        state.apply_agents_view_snapshot("local", sidebar_snapshot(sidebar));
        state
    }

    fn filter_button(container: &HtmlElement, label: &str) -> HtmlElement {
        let buttons = container
            .query_selector_all(".panel-filters button")
            .unwrap();
        for i in 0..buttons.length() {
            let el = buttons.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            if el.text_content().unwrap_or_default().contains(label) {
                return el;
            }
        }
        panic!("filter button {label:?} not found");
    }

    fn button_active(container: &HtmlElement, label: &str) -> bool {
        filter_button(container, label)
            .class_name()
            .contains("active")
    }

    /// The most recent `set_agents_view_preferences` update payload on the wire.
    fn last_view_pref_update(calls: &js_sys::Array) -> JsonValue {
        recorded_frames(calls)
            .into_iter()
            .filter(|(kind, _, _)| kind == "set_agents_view_preferences")
            .map(|(_, payload, _)| payload["update"].clone())
            .next_back()
            .expect("expected a set_agents_view_preferences frame")
    }

    /// ContextualDefault: at Home the "Show other projects" selector reads as
    /// effective-on; inside a project it reads as effective-off — all derived
    /// from the server snapshot, with no component-local persistence.
    #[wasm_bindgen_test]
    async fn sidebar_contextual_default_reflects_active_project() {
        let container = make_container();
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        assert!(
            button_active(&container, "Show other projects"),
            "ContextualDefault at Home shows all projects (effective-on)"
        );

        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("p1".to_owned()),
        }));
        next_tick().await;
        assert!(
            !button_active(&container, "Show other projects"),
            "ContextualDefault inside a project shows current only (effective-off)"
        );
    }

    /// Clicking a selector pins an explicit durable value through the server
    /// update path (`set_sidebar_preferences`) rather than mutating local state.
    #[wasm_bindgen_test]
    async fn sidebar_toggle_emits_typed_set_sidebar_preferences() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        // At Home, ContextualDefault is effective-on, so a click pins the
        // opposite explicit value: current_project_only.
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        filter_button(&container, "Show other projects").click();
        next_tick().await;

        let update = last_view_pref_update(&calls);
        assert_eq!(
            update["kind"], "set_sidebar_preferences",
            "typed sidebar update emitted: {update}"
        );
        assert_eq!(
            update["sidebar"]["project_visibility"], "current_project_only",
            "click pins the explicit opposite of the effective value: {update}"
        );
        // Optimistic overlay flips the button immediately.
        assert!(
            !button_active(&container, "Show other projects"),
            "overlay reflects the pinned current_project_only immediately"
        );
    }

    /// A pinned selector persists across active-project switches (the overlay is
    /// not project-keyed), and an authoritative server snapshot then carries the
    /// same value so it stays after the overlay is dropped.
    #[wasm_bindgen_test]
    async fn sidebar_hide_inactive_persists_across_project_switch_and_snapshot() {
        let _calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;
        assert!(!button_active(&container, "Hide inactive"));

        filter_button(&container, "Hide inactive").click();
        next_tick().await;
        assert!(
            button_active(&container, "Hide inactive"),
            "overlay reflects hide_inactive immediately"
        );

        // Switch project: the selector is server-owned, not project-local, so it
        // stays pinned.
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("p1".to_owned()),
        }));
        next_tick().await;
        assert!(
            button_active(&container, "Hide inactive"),
            "pinned hide_inactive survives a project switch"
        );

        // Authoritative snapshot confirms the value and drops the overlay.
        state.apply_agents_view_snapshot(
            "local",
            sidebar_snapshot(AgentsSidebarPreferences {
                hide_inactive: true,
                ..Default::default()
            }),
        );
        next_tick().await;
        assert!(
            button_active(&container, "Hide inactive"),
            "server snapshot keeps hide_inactive after the overlay is reconciled"
        );
    }

    /// An authoritative server snapshot overrides an in-flight optimistic
    /// overlay even when the server value disagrees.
    #[wasm_bindgen_test]
    async fn sidebar_server_snapshot_overrides_overlay() {
        let _calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        filter_button(&container, "Hide sub-agents").click();
        next_tick().await;
        assert!(
            button_active(&container, "Hide sub-agents"),
            "overlay turns hide_sub_agents on optimistically"
        );

        // Server says it is off; the snapshot wins and drops the overlay.
        state.apply_agents_view_snapshot(
            "local",
            sidebar_snapshot(AgentsSidebarPreferences {
                hide_sub_agents: false,
                ..Default::default()
            }),
        );
        next_tick().await;
        assert!(
            !button_active(&container, "Hide sub-agents"),
            "server snapshot overrides the optimistic overlay"
        );
    }

    // ── Sidebar agent click routes to the agent's own project ────────────────

    /// Pin the sidebar to "all projects" so an agent that lives outside the
    /// active project is still rendered (and therefore clickable).
    fn apply_show_all_projects(state: &AppState) {
        state.apply_agents_view_snapshot(
            "local",
            sidebar_snapshot(AgentsSidebarPreferences {
                project_visibility: AgentsSidebarProjectVisibility::AllProjects,
                ..Default::default()
            }),
        );
    }

    fn agent_card_el(container: &HtmlElement, agent_id: &str) -> HtmlElement {
        container
            .query_selector(&format!("[data-agent-id='{agent_id}'] .agent-card"))
            .unwrap()
            .unwrap_or_else(|| panic!("agent card for {agent_id} should be rendered"))
            .dyn_into()
            .unwrap()
    }

    fn project_accessed_streams(calls: &js_sys::Array) -> Vec<String> {
        recorded_frames(calls)
            .into_iter()
            .filter(|(kind, _, _)| kind == "project_accessed")
            .map(|(_, _, stream)| stream)
            .collect()
    }

    /// Clicking an agent that belongs to a different project than the active one
    /// switches the active project to the agent's project FIRST, then opens its
    /// chat tab — so the tab lands in the agent's project center zone instead of
    /// the previously-active project's.
    #[wasm_bindgen_test]
    async fn clicking_agent_switches_to_its_project_then_opens_chat() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        state
            .configured_hosts
            .set(vec![configured_host("local", "Local Host")]);
        state.projects.set(vec![
            project_info("local", "abc", "ABC Project", 0),
            project_info("local", "xyz", "XYZ Project", 1),
        ]);
        push_agent_with_scope(
            &state,
            "local",
            "agent-abc",
            "ABC Agent",
            true,
            Some("abc"),
            None,
        );
        // Start on a different project; pin "all projects" so the abc agent is
        // visible even though it is outside the active project.
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("xyz".to_owned()),
        }));
        apply_show_all_projects(&state);

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        agent_card_el(&container, "agent-abc").click();
        for _ in 0..8 {
            next_tick().await;
        }

        let active_project = state
            .active_project
            .get_untracked()
            .expect("active project should be set after click");
        assert_eq!(active_project.host_id, "local");
        assert_eq!(
            active_project.project_id,
            ProjectId("abc".to_owned()),
            "click must switch the active project to the clicked agent's project"
        );

        let active_agent = state
            .active_agent
            .get_untracked()
            .expect("active agent should be set after the chat tab opens");
        assert_eq!(active_agent.host_id, "local");
        assert_eq!(
            active_agent.agent_id,
            AgentId("agent-abc".to_owned()),
            "the clicked agent's chat tab must be the active tab"
        );

        // The switch notifies the server on the agent's project stream exactly
        // once (no ProjectAccessed is sent merely by setting up the fixture).
        assert_eq!(
            project_accessed_streams(&calls),
            vec!["/project/abc".to_owned()],
            "exactly one ProjectAccessed, targeting the agent's project stream"
        );
    }

    /// A sub-agent / cross-host click switches BOTH the active host and project
    /// to the clicked agent's own, and the opened chat tab carries that host.
    #[wasm_bindgen_test]
    async fn clicking_agent_on_another_host_switches_host_and_project() {
        let _calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        state.configured_hosts.set(vec![
            configured_host("local", "Local Host"),
            configured_host("remote", "Remote Host"),
        ]);
        state.projects.set(vec![
            project_info("local", "alpha", "Alpha Project", 0),
            project_info("remote", "gamma", "Gamma Project", 0),
        ]);
        push_agent_with_scope(
            &state,
            "remote",
            "agent-gamma",
            "Gamma Agent",
            true,
            Some("gamma"),
            None,
        );
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("alpha".to_owned()),
        }));
        apply_show_all_projects(&state);

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        agent_card_el(&container, "agent-gamma").click();
        for _ in 0..8 {
            next_tick().await;
        }

        let active_project = state
            .active_project
            .get_untracked()
            .expect("active project should be set after click");
        assert_eq!(
            active_project.host_id, "remote",
            "cross-host click must switch the active host"
        );
        assert_eq!(active_project.project_id, ProjectId("gamma".to_owned()));

        let active_agent = state
            .active_agent
            .get_untracked()
            .expect("active agent should be set after click");
        assert_eq!(
            active_agent.host_id, "remote",
            "the opened chat tab must carry the agent's own host"
        );
        assert_eq!(active_agent.agent_id, AgentId("agent-gamma".to_owned()));
    }

    /// Clicking an agent that is already in the active project still opens its
    /// chat tab, but `switch_active_project` early-returns so no redundant
    /// project switch (and no ProjectAccessed frame) is emitted.
    #[wasm_bindgen_test]
    async fn clicking_agent_already_on_its_project_opens_chat_without_reswitch() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        state
            .configured_hosts
            .set(vec![configured_host("local", "Local Host")]);
        state
            .projects
            .set(vec![project_info("local", "abc", "ABC Project", 0)]);
        push_agent_with_scope(
            &state,
            "local",
            "agent-abc",
            "ABC Agent",
            true,
            Some("abc"),
            None,
        );
        // Already on the agent's project (ContextualDefault keeps it visible).
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("abc".to_owned()),
        }));

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        agent_card_el(&container, "agent-abc").click();
        for _ in 0..8 {
            next_tick().await;
        }

        let active_project = state
            .active_project
            .get_untracked()
            .expect("active project should remain set");
        assert_eq!(active_project.host_id, "local");
        assert_eq!(
            active_project.project_id,
            ProjectId("abc".to_owned()),
            "active project must be unchanged when already on the agent's project"
        );

        let active_agent = state
            .active_agent
            .get_untracked()
            .expect("active agent should be set after the chat tab opens");
        assert_eq!(
            active_agent.agent_id,
            AgentId("agent-abc".to_owned()),
            "the clicked agent's chat tab must be active"
        );

        assert!(
            project_accessed_streams(&calls).is_empty(),
            "no project switch (and no ProjectAccessed) when already on the project"
        );
    }

    // ── Open Agent to the Side (dev-docs/32 §9, §12) ─────────────────────
    //
    // The panel never decides the outcome itself: every activation goes
    // through `AppState::open_agent_chat_to_side`, so these assert the
    // user-visible markup (name, aria-disabled, reason, target size,
    // announcement) *and* the authoritative layout the action produced. The
    // center zone is not mounted here, so the placement it renders from is the
    // observable result for this component.

    fn side_open_fixture(state: &AppState, agent_project: Option<&str>) -> AgentInfo {
        state
            .configured_hosts
            .set(vec![configured_host("local", "Local Host")]);
        state.projects.set(vec![
            project_info("local", "alpha", "Alpha Project", 0),
            project_info("local", "beta", "Beta Project", 1),
        ]);
        state.tabs_enabled.set(true);
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("alpha".to_owned()),
        }));
        // Show every project's agents so a cross-project agent still has a card
        // to render its (disabled) action on.
        state.set_agents_view_overlay(|overlay| {
            overlay.sidebar = Some(AgentsSidebarPreferences {
                project_visibility: AgentsSidebarProjectVisibility::AllProjects,
                ..Default::default()
            });
        });
        push_agent_with_scope(
            state,
            "local",
            "a-1",
            "Alpha Agent",
            true,
            agent_project,
            None,
        );
        state
            .agents
            .get_untracked()
            .into_iter()
            .find(|candidate| candidate.agent_id == AgentId("a-1".to_owned()))
            .expect("seeded agent")
    }

    fn side_button(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector("[data-test='agent-open-side']")
            .unwrap()
            .expect("agent card exposes an Open to the Side action")
            .dyn_into::<HtmlElement>()
            .unwrap()
    }

    /// Mount with an explicit measured center-workspace width, the way the real
    /// center zone's `ResizeObserver` reports it. `mount_panel` declares the
    /// workspace unmeasured, which the width gate treats as wide enough.
    ///
    /// The width is a thread-local global, not context — a copy of the handle
    /// resolves the same signal everywhere — so this sets it directly rather
    /// than providing it.
    fn mount_panel_at_width(
        container: &HtmlElement,
        state: AppState,
        width: Option<f64>,
    ) -> impl Sized {
        workspace_width().set(width);
        mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <AgentsPanel /> }
        })
    }

    fn text_of(container: &HtmlElement, selector: &str) -> String {
        container
            .query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("expected {selector:?} to be rendered"))
            .text_content()
            .unwrap_or_default()
    }

    /// The shortcut hint the action should advertise, from the same bound chord
    /// the handler matches — never a hardcoded "Ctrl+Enter".
    fn expected_side_title() -> String {
        format!("Open to the side ({})", side_open_chord_hint())
    }

    /// Every occurrence of this agent's chat across the whole layout. Chats are
    /// never duplicated, so this must never exceed one entry.
    fn chat_occurrences(state: &AppState, agent: &AgentInfo) -> Vec<(PaneId, TabId)> {
        let content = TabContent::chat_with_agent(agent_chat_ref(agent));
        state
            .center_zone
            .with_untracked(|center_zone| center_zone.occurrences(&content))
    }

    fn pane_tab_ids(state: &AppState, pane: PaneId) -> Vec<TabId> {
        state.center_zone.with_untracked(|center_zone| {
            center_zone
                .pane(pane)
                .map(|pane| pane.tabs.iter().map(|tab| tab.id).collect())
                .unwrap_or_default()
        })
    }

    fn dispatch_chord(element: &HtmlElement, key: &str) {
        let escaped_key = serde_json::to_string(key).expect("serialize key");
        let event: web_sys::Event = js_sys::eval(&format!(
            "new KeyboardEvent('keydown', {{ key: {escaped_key}, ctrlKey: true, \
             bubbles: true, cancelable: true }})"
        ))
        .expect("keyboard event")
        .dyn_into()
        .expect("KeyboardEvent is an Event");
        element
            .dispatch_event(&event)
            .expect("dispatch keyboard event");
    }

    /// An agent with no tab yet opens exactly once, in the *other* pane, and
    /// the outcome is announced.
    #[wasm_bindgen_test]
    async fn open_to_side_opens_an_unopened_agent_chat_in_the_other_pane() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        assert_eq!(
            untrack(|| agent_side_open_eligibility(&state, &agent)),
            None,
            "the authoritative query reports this agent as eligible"
        );
        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled"),
            None,
            "an eligible action is not aria-disabled"
        );
        assert_eq!(
            button.get_attribute("aria-describedby"),
            None,
            "an eligible action has no refusal description"
        );
        assert_eq!(
            button.get_attribute("title"),
            Some(expected_side_title()),
            "an eligible action advertises its shortcut, derived from the bound \
             chord so it is platform-correct rather than a hardcoded Ctrl+Enter"
        );

        button.click();
        next_tick().await;

        let occurrences = chat_occurrences(&state, &agent);
        assert_eq!(
            occurrences.len(),
            1,
            "the chat must open exactly once — chats are never duplicated"
        );
        assert_eq!(
            occurrences[0].0,
            PaneId::Secondary,
            "Open to the Side must land the chat in the other pane"
        );
        assert!(
            state.center_zone.get_untracked().is_split(),
            "opening beside an unsplit workspace creates the split"
        );

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Opened Alpha Agent in the other pane"),
            "the outcome must be announced; got {text:?}"
        );
    }

    /// An occurrence already in the focused pane is *moved*, keeping its
    /// `TabId` — not copied into a second chat tab.
    #[wasm_bindgen_test]
    async fn open_to_side_moves_a_focused_pane_chat_and_preserves_its_tab_id() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        // Home + the chat, both in the focused pane, so the source pane still
        // has content after the move.
        state.open_tab(
            TabContent::chat_with_agent(agent_chat_ref(&agent)),
            agent.name.clone(),
            true,
        );
        let before = chat_occurrences(&state, &agent);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0, PaneId::Primary);
        let original_tab = before[0].1;

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        assert_eq!(
            untrack(|| agent_side_open_eligibility(&state, &agent)),
            None,
            "a chat beside other tabs in the focused pane is eligible to move"
        );

        side_button(&container).click();
        next_tick().await;

        let after = chat_occurrences(&state, &agent);
        assert_eq!(after.len(), 1, "moving must not create a second chat tab");
        assert_eq!(
            after[0].0,
            PaneId::Secondary,
            "the chat moved to the other pane"
        );
        assert_eq!(
            after[0].1, original_tab,
            "a move preserves the TabId (and with it the tab's view state)"
        );
        assert!(
            !pane_tab_ids(&state, PaneId::Primary).contains(&original_tab),
            "the chat must no longer be in the source pane"
        );

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Moved Alpha Agent to the other pane"),
            "got {text:?}"
        );
    }

    /// An occurrence already in the other pane is revealed, not reopened.
    #[wasm_bindgen_test]
    async fn open_to_side_reveals_an_existing_other_pane_chat_without_duplicating() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        state.open_tab_at(
            OpenTarget::Beside,
            TabContent::chat_with_agent(agent_chat_ref(&agent)),
            agent.name.clone(),
            true,
        );
        state.focus_pane(PaneId::Primary);

        let before = chat_occurrences(&state, &agent);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0, PaneId::Secondary);
        let original_tab = before[0].1;

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        assert_eq!(
            untrack(|| agent_side_open_eligibility(&state, &agent)),
            None,
            "an occurrence already in the other pane is eligible to reveal"
        );

        side_button(&container).click();
        next_tick().await;

        let after = chat_occurrences(&state, &agent);
        assert_eq!(
            after.len(),
            1,
            "revealing must not open a second occurrence of the same chat"
        );
        assert_eq!(after[0], (PaneId::Secondary, original_tab));

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Revealed Alpha Agent in the other pane"),
            "got {text:?}"
        );
    }

    /// A cross-project agent keeps a visible action that is `aria-disabled`,
    /// carries the reason as an accessible description, and mutates nothing —
    /// not even the active project.
    #[wasm_bindgen_test]
    async fn open_to_side_is_disabled_for_a_cross_project_agent_and_mutates_nothing() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("beta"));

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        // The state layer classifies this, and the control must render exactly
        // what it says — no locally derived policy or reason text.
        let eligibility = untrack(|| agent_side_open_eligibility(&state, &agent));
        assert_eq!(
            eligibility,
            Some(AgentOpenToSideResult::CrossProject),
            "the authoritative eligibility query refuses a cross-project agent"
        );
        let reason = eligibility
            .expect("refused")
            .disabled_reason()
            .expect("a refusal carries a reason");
        assert_eq!(
            reason, "This agent is in another project — open that project first.",
            "the reason text is the state layer's, and this pins it"
        );

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "the cross-project action stays visible and is aria-disabled"
        );
        assert_eq!(
            button.get_attribute("aria-label").as_deref(),
            Some("Open Alpha Agent to the side"),
            "the action names its agent for screen readers"
        );

        let described_by = button
            .get_attribute("aria-describedby")
            .expect("a disabled action describes why");
        let description = container
            .query_selector(&format!("#{described_by}"))
            .unwrap()
            .expect("the description element exists")
            .text_content()
            .unwrap_or_default();
        assert_eq!(
            description, reason,
            "the description is the state layer's own reason text"
        );
        assert_eq!(
            button.get_attribute("title").as_deref(),
            Some(reason),
            "the tooltip explains the refusal instead of the shortcut"
        );

        let tabs_before = pane_tab_ids(&state, PaneId::Primary);
        button.click();
        next_tick().await;

        assert!(
            chat_occurrences(&state, &agent).is_empty(),
            "a refused side-open must not open the chat anywhere"
        );
        assert_eq!(
            pane_tab_ids(&state, PaneId::Primary),
            tabs_before,
            "a refused side-open must not touch the layout"
        );
        assert!(
            !state.center_zone.get_untracked().is_split(),
            "a refused side-open must not create a split"
        );
        assert_eq!(
            state
                .active_project
                .get_untracked()
                .expect("active project unchanged")
                .project_id,
            ProjectId("alpha".to_owned()),
            "the side action is active-project-only: it never switches project the \
             way an ordinary click does"
        );
    }

    /// Moving the focused pane's only tab out would leave an empty pane, which
    /// the layout cannot represent. The action stays visible, disabled, and
    /// inert.
    #[wasm_bindgen_test]
    async fn open_to_side_is_disabled_when_the_chat_is_the_panes_only_tab() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        // Split with the chat alone in the (focused) secondary pane.
        state.open_tab_at(
            OpenTarget::Beside,
            TabContent::chat_with_agent(agent_chat_ref(&agent)),
            agent.name.clone(),
            true,
        );
        let before = chat_occurrences(&state, &agent);
        assert_eq!(before[0].0, PaneId::Secondary);
        let original = before[0].1;
        assert_eq!(pane_tab_ids(&state, PaneId::Secondary), vec![original]);

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        let eligibility = untrack(|| agent_side_open_eligibility(&state, &agent));
        assert_eq!(
            eligibility,
            Some(AgentOpenToSideResult::NothingWouldRemain),
            "the authoritative eligibility query refuses emptying the pane"
        );
        let reason = eligibility
            .expect("refused")
            .disabled_reason()
            .expect("a refusal carries a reason");
        assert_eq!(reason, "Nothing would be left in this pane.");

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "the sole-tab case stays visible and aria-disabled"
        );
        assert_eq!(button.get_attribute("title").as_deref(), Some(reason));
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains(reason),
            "the reason is in the accessibility tree as a description"
        );

        button.click();
        next_tick().await;

        assert_eq!(
            chat_occurrences(&state, &agent),
            vec![(PaneId::Secondary, original)],
            "a refused sole-tab move leaves the chat exactly where it was"
        );
        assert!(
            state.center_zone.get_untracked().is_split(),
            "the split must survive a refused move"
        );
    }

    /// The chord is bound to the card, not to the window: it opens to the side
    /// and must not travel on to a global handler, because the chat composer
    /// owns Cmd/Ctrl+Enter for send/steer.
    #[wasm_bindgen_test]
    async fn ctrl_enter_on_a_card_opens_to_the_side_and_never_escapes_the_card() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        // Stands in for the app's global keydown binding: it must observe
        // nothing at all.
        let escaped = js_sys::eval(
            r#"
            (function() {
                window.__escaped_chords = 0;
                window.__chord_listener = function(ev) {
                    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) {
                        window.__escaped_chords += 1;
                    }
                };
                window.addEventListener('keydown', window.__chord_listener);
                return 0;
            })();
            "#,
        );
        assert!(escaped.is_ok());

        let card = container
            .query_selector(".agent-card")
            .unwrap()
            .expect("agent card")
            .dyn_into::<HtmlElement>()
            .unwrap();
        dispatch_chord(&card, "Enter");
        next_tick().await;

        let escaped_count = js_sys::eval(
            "(function() { window.removeEventListener('keydown', window.__chord_listener); \
             return window.__escaped_chords; })();",
        )
        .expect("read counter")
        .as_f64()
        .expect("number");
        assert_eq!(
            escaped_count, 0.0,
            "the contextual chord must not bubble to a global/composer handler"
        );

        let occurrences = chat_occurrences(&state, &agent);
        assert_eq!(
            occurrences.len(),
            1,
            "the chord opened the chat exactly once"
        );
        assert_eq!(
            occurrences[0].0,
            PaneId::Secondary,
            "the chord opens to the side, not in the focused pane"
        );
    }

    /// The new action must not disturb the ordinary click, which still opens in
    /// the focused pane and still switches to the agent's own project.
    #[wasm_bindgen_test]
    async fn ordinary_card_click_still_switches_project_and_opens_in_the_focused_pane() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("beta"));
        // The project switch puts frames on the wire.
        let _calls = install_send_stub_with_dialog_ok();

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        let card = container
            .query_selector(".agent-card")
            .unwrap()
            .expect("agent card")
            .dyn_into::<HtmlElement>()
            .unwrap();
        card.click();
        next_tick().await;

        assert_eq!(
            state
                .active_project
                .get_untracked()
                .expect("active project")
                .project_id,
            ProjectId("beta".to_owned()),
            "an ordinary click still switches to the agent's project"
        );
        let occurrences = chat_occurrences(&state, &agent);
        assert_eq!(occurrences.len(), 1);
        assert_eq!(
            occurrences[0].0,
            PaneId::Primary,
            "an ordinary click opens in the focused pane and never splits"
        );
        assert!(!state.center_zone.get_untracked().is_split());
    }

    /// Too narrow to create a second pane: the action stays visible, refuses
    /// with the *shared* width reason, and creates neither a tab nor a split.
    /// The refusal is both visible on the card and announced.
    #[wasm_bindgen_test]
    async fn open_to_side_is_disabled_in_a_narrow_workspace_with_no_split() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        let _handle = mount_panel_at_width(&container, state.clone(), Some(500.0));
        next_tick().await;

        // The state layer alone would allow this — it is the UI-owned width gate
        // that refuses, and the two compose.
        assert_eq!(
            untrack(|| agent_side_open_eligibility(&state, &agent)),
            None,
            "state eligibility does not refuse here; the width gate does"
        );
        let reason = split_creation_availability(&state, Some(500.0))
            .reason()
            .expect("a 500px workspace cannot create a split");
        assert_eq!(
            reason, "Not enough width to split — widen the window or hide a side panel.",
            "the shared sentence every side-open surface uses, pinned"
        );

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true"),
            "a too-narrow workspace leaves the action visible and aria-disabled"
        );
        assert_eq!(button.get_attribute("title").as_deref(), Some(reason));
        let described_by = button
            .get_attribute("aria-describedby")
            .expect("a disabled action describes why");
        assert_eq!(text_of(&container, &format!("#{described_by}")), reason);

        button.click();
        next_tick().await;

        assert!(
            chat_occurrences(&state, &agent).is_empty(),
            "a width-refused side-open must create no tab"
        );
        assert!(
            !state.center_zone.get_untracked().is_split(),
            "a width-refused side-open must create no split"
        );
        assert_eq!(
            text_of(&container, ".agent-card-side-refusal"),
            reason,
            "the refusal is visible on the card, not just in the a11y tree"
        );
        assert_eq!(
            text_of(&container, ".agent-group-live-status"),
            reason,
            "the refusal is also announced politely"
        );
    }

    /// Once a split exists the width gate no longer applies: a workspace that has
    /// since shrunk is handled by reversible narrow mode, never by disabling the
    /// action. The same 500px width that refused above is actionable here.
    #[wasm_bindgen_test]
    async fn a_narrow_workspace_stays_actionable_once_a_split_exists() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        // A split already exists, holding an unrelated file in the other pane.
        state.open_tab_at(
            OpenTarget::Beside,
            TabContent::File {
                key: FileResourceKey {
                    host_id: "local".to_owned(),
                    project_id: ProjectId("alpha".to_owned()),
                    path: ProjectPath {
                        root: ProjectRootPath("/tmp/alpha".to_owned()),
                        relative_path: "notes.rs".to_owned(),
                    },
                },
            },
            "notes.rs".to_owned(),
            true,
        );
        state.focus_pane(PaneId::Primary);
        assert!(state.center_zone.get_untracked().is_split());

        let _handle = mount_panel_at_width(&container, state.clone(), Some(500.0));
        next_tick().await;

        assert!(
            split_creation_availability(&state, Some(500.0)).is_enabled(),
            "an existing split needs no width to create a pane it already has"
        );

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled"),
            None,
            "narrow mode must not disable the action when a split already exists"
        );
        assert_eq!(button.get_attribute("title"), Some(expected_side_title()));

        button.click();
        next_tick().await;

        let occurrences = chat_occurrences(&state, &agent);
        assert_eq!(occurrences.len(), 1, "chats are never duplicated");
        assert_eq!(
            occurrences[0].0,
            PaneId::Secondary,
            "the chat still opens into the existing other pane"
        );
        assert!(
            container
                .query_selector(".agent-card-side-refusal")
                .unwrap()
                .is_none(),
            "a successful open shows no refusal"
        );
    }

    /// REGRESSION: when a state refusal and a width refusal coexist, rendering
    /// and activation must name the *same* one.
    ///
    /// Rendering composed state-then-width while activation checked width first,
    /// so a cross-project agent in a narrow workspace showed "another project"
    /// on the card but announced "not enough width" on click. Both now go
    /// through one composition, so the visible reason, the aria description, the
    /// visible notice, and the live announcement are identical.
    #[wasm_bindgen_test]
    async fn cross_project_in_a_narrow_workspace_names_one_reason_everywhere() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("beta"));

        let _handle = mount_panel_at_width(&container, state.clone(), Some(500.0));
        next_tick().await;

        // Both authorities refuse — that is what makes this a combination.
        let state_reason = untrack(|| agent_side_open_eligibility(&state, &agent))
            .expect("the state layer refuses a cross-project agent")
            .disabled_reason()
            .expect("a refusal carries a reason");
        let width_reason = split_creation_availability(&state, Some(500.0))
            .reason()
            .expect("a 500px workspace also cannot create a split");
        assert_ne!(
            state_reason, width_reason,
            "the two refusals must be distinguishable for this test to mean anything"
        );

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true")
        );
        assert_eq!(
            button.get_attribute("title").as_deref(),
            Some(state_reason),
            "state precedence: the agent cannot be opened beside at all, so \
             widening the window would not help it"
        );
        let described_by = button
            .get_attribute("aria-describedby")
            .expect("a disabled action describes why");
        assert_eq!(
            text_of(&container, &format!("#{described_by}")),
            state_reason
        );

        button.click();
        next_tick().await;

        assert_eq!(
            text_of(&container, ".agent-card-side-refusal"),
            state_reason,
            "the visible notice must name the same refusal the control rendered"
        );
        assert_eq!(
            text_of(&container, ".agent-group-live-status"),
            state_reason,
            "the announcement must name the same refusal the control rendered"
        );
        assert!(
            chat_occurrences(&state, &agent).is_empty(),
            "a doubly-refused side-open opens nothing"
        );
        assert!(
            !state.center_zone.get_untracked().is_split(),
            "a doubly-refused side-open creates no split"
        );
        assert_eq!(
            state
                .active_project
                .get_untracked()
                .expect("active project")
                .project_id,
            ProjectId("alpha".to_owned()),
            "and switches no project"
        );
    }

    /// The same precedence with the other state refusal: tabs disabled *and* a
    /// workspace too narrow to split.
    #[wasm_bindgen_test]
    async fn tabs_disabled_in_a_narrow_workspace_names_one_reason_everywhere() {
        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));
        state.tabs_enabled.set(false);

        let _handle = mount_panel_at_width(&container, state.clone(), Some(500.0));
        next_tick().await;

        let state_reason = untrack(|| agent_side_open_eligibility(&state, &agent))
            .expect("the state layer refuses when tabs are disabled")
            .disabled_reason()
            .expect("a refusal carries a reason");
        assert_eq!(state_reason, "Enable tabs to use split view.");
        let width_reason = split_creation_availability(&state, Some(500.0))
            .reason()
            .expect("a 500px workspace also cannot create a split");
        assert_ne!(state_reason, width_reason);

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled").as_deref(),
            Some("true")
        );
        assert_eq!(button.get_attribute("title").as_deref(), Some(state_reason));
        let described_by = button
            .get_attribute("aria-describedby")
            .expect("a disabled action describes why");
        assert_eq!(
            text_of(&container, &format!("#{described_by}")),
            state_reason
        );

        button.click();
        next_tick().await;

        assert_eq!(
            text_of(&container, ".agent-card-side-refusal"),
            state_reason,
            "visible notice matches what the control rendered"
        );
        assert_eq!(
            text_of(&container, ".agent-group-live-status"),
            state_reason,
            "announcement matches what the control rendered"
        );
        assert!(
            chat_occurrences(&state, &agent).is_empty(),
            "a doubly-refused side-open opens nothing"
        );
        assert!(!state.center_zone.get_untracked().is_split());
    }

    /// REGRESSION: a narrow width measured by an *earlier* mount must not disable
    /// this one.
    ///
    /// The center-workspace width is a thread-local global that lives for the
    /// life of the thread, so a 500px measurement set by one test used to leak
    /// into every later test on the shared wasm test thread. The width gate then
    /// refused to create a split, and every Open to the Side — pointer and chord,
    /// on both agent surfaces — silently did nothing while still reporting zero
    /// escaped events, so it read as "the chord never fired".
    #[wasm_bindgen_test]
    async fn a_narrow_width_measured_earlier_does_not_leak_into_a_fresh_mount() {
        // A previously-rendered workspace measured too narrow to split.
        workspace_width().set(Some(500.0));

        let container = make_container();
        let state = make_app_state("local");
        let agent = side_open_fixture(&state, Some("alpha"));

        // This mount declares its own precondition: nothing has measured it.
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        assert_eq!(
            workspace_width().get_untracked(),
            None,
            "mounting must forget the previous workspace's measurement"
        );

        let button = side_button(&container);
        assert_eq!(
            button.get_attribute("aria-disabled"),
            None,
            "a stale narrow measurement must not disable the action"
        );

        button.click();
        next_tick().await;

        let occurrences = chat_occurrences(&state, &agent);
        assert_eq!(
            occurrences.len(),
            1,
            "the chat must still open exactly once — chats are never duplicated"
        );
        assert_eq!(
            occurrences[0].0,
            PaneId::Secondary,
            "and it must still land in the other pane"
        );
    }

    /// The action is a real pointer target, sized in this component's own
    /// markup rather than in the shared stylesheet.
    #[wasm_bindgen_test]
    async fn the_side_open_action_is_at_least_a_44px_target() {
        let container = make_container();
        let state = make_app_state("local");
        let _agent = side_open_fixture(&state, Some("alpha"));

        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        let rect = side_button(&container).get_bounding_client_rect();
        assert!(
            rect.width() >= 44.0 && rect.height() >= 44.0,
            "Open to the Side must be at least a 44x44px target; got {}x{}",
            rect.width(),
            rect.height()
        );
    }
}
