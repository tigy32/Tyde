use std::collections::{HashMap, HashSet};

use leptos::leptos_dom::helpers::{
    AnimationFrameRequestHandle, request_animation_frame_with_handle,
};
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentAnnotationTarget, AgentGroup, AgentGroupId, AgentGroupsSnapshot, AgentGroupsUpdate,
    AgentId, AgentOrigin, AgentPinsSnapshot, AgentPinsUpdate, AgentsSidebarPreferences,
    AgentsSidebarProjectVisibility, AgentsViewPreferencesUpdate, FrameKind, ProjectId,
    SetAgentNamePayload, Swarm, SwarmId, SwarmLifecycle, SwarmMemberState,
};

use crate::components::agent_monitor_view::{agent_annotation_target, send_pins_update};
use crate::components::card_menu::{
    CardContextMenu, CardMenuPosition, open_card_menu, open_card_menu_below,
};
use crate::components::launch_menu::LaunchDestination;
use crate::components::swarm_view::{lifecycle_label, member_is_working, open_swarm_tab};
use crate::send::{close_agent, send_frame};
use crate::state::{
    ActiveAgentRef, ActiveProjectRef, AgentInfo, AgentsPanelFilters, AppState, CompactionOldInfo,
    ContextCompactionUiState, ProjectInfo, StreamingState, TabContent, agent_ui_project_id,
    sort_project_infos,
};

fn agent_passes_filters(
    agent: &AgentInfo,
    filters: &AgentsPanelFilters,
    matches_project: bool,
    streaming: &HashMap<AgentId, StreamingState>,
    turn_active: &HashMap<AgentId, bool>,
    awaiting_user: &HashSet<AgentId>,
    lowercase_query: &str,
) -> bool {
    if agent.origin == AgentOrigin::Tychat {
        return lowercase_query.is_empty() || agent.name.to_lowercase().contains(lowercase_query);
    }
    if filters.hide_sub_agents && agent.parent_agent_id.is_some() {
        return false;
    }
    if filters.hide_inactive {
        let is_active = !agent.started
            || streaming.contains_key(&agent.agent_id)
            || turn_active.get(&agent.agent_id).copied().unwrap_or(false)
            || awaiting_user.contains(&agent.agent_id);
        if !is_active {
            return false;
        }
    }
    if !filters.show_other_projects && !matches_project {
        return false;
    }
    if !lowercase_query.is_empty() && !agent.name.to_lowercase().contains(lowercase_query) {
        return false;
    }
    true
}

fn swarm_passes_filters(
    host_id: &str,
    swarm: &Swarm,
    filters: &AgentsPanelFilters,
    active_project: Option<&ActiveProjectRef>,
    projects: &[ProjectInfo],
    lowercase_query: &str,
) -> bool {
    if filters.hide_inactive && swarm_working_count(swarm) == 0 {
        return false;
    }
    if !filters.show_other_projects
        && !active_project.is_some_and(|ap| {
            ap.host_id == host_id
                && agent_ui_project_id(projects, host_id, Some(&ap.project_id))
                    == agent_ui_project_id(projects, host_id, Some(&swarm.constraints.project_id))
        })
    {
        return false;
    }
    lowercase_query.is_empty() || swarm.name.to_lowercase().contains(lowercase_query)
}

fn swarm_working_count(swarm: &Swarm) -> usize {
    swarm
        .members
        .iter()
        .filter(|member| member_is_working(member))
        .count()
}

/// Aggregate status for a swarm's summary row: the lifecycle wins when it
/// stops the swarm from working, otherwise any working member makes the
/// whole swarm working.
fn swarm_row_status(swarm: &Swarm) -> (&'static str, &'static str) {
    match swarm.lifecycle {
        SwarmLifecycle::Pausing | SwarmLifecycle::Paused => (
            lifecycle_label(swarm.lifecycle),
            "agent-card-status cancelled",
        ),
        SwarmLifecycle::AttentionRequired => ("Needs attention", "agent-card-status awaiting"),
        SwarmLifecycle::Running | SwarmLifecycle::Transitioning => {
            if swarm_working_count(swarm) > 0 {
                ("Working", "agent-card-status running")
            } else {
                ("Idle", "agent-card-status")
            }
        }
    }
}

fn swarm_row_counts(swarm: &Swarm) -> String {
    let members = swarm
        .members
        .iter()
        .filter(|member| member.state != SwarmMemberState::Retired)
        .count();
    format!(
        "{members} agent{} · {} working",
        if members == 1 { "" } else { "s" },
        swarm_working_count(swarm)
    )
}

fn render_swarm_row(state: AppState, swarm_ref: SidebarSwarmRef) -> AnyView {
    let swarm_id_attr = swarm_ref.swarm_id.0.clone();
    let swarm_state = state.clone();
    let host_id = swarm_ref.host_id.clone();
    let swarm_id = swarm_ref.swarm_id.clone();
    let swarm = Memo::new(move |_| {
        swarm_state.swarms.with(|by_host| {
            by_host
                .get(&host_id)
                .and_then(|swarms| swarms.get(&swarm_id))
                .cloned()
        })
    });
    let open = move |_| {
        if let Some(current) = swarm.get_untracked() {
            open_swarm_tab(
                &state,
                swarm_ref.host_id.clone(),
                swarm_ref.swarm_id.clone(),
                current.name,
            );
        }
    };
    view! {
        <button
            class="agent-card agent-sidebar-swarm-row"
            data-swarm-id=swarm_id_attr
            title="Open the swarm"
            on:click=open
        >
            <div class="agent-card-top">
                <span class="agent-card-name">
                    {move || swarm.with(|s| s.as_ref().map(|s| format!("Swarm: {}", s.name)))}
                </span>
                <span class=move || swarm.with(|s| s.as_ref().map(|s| swarm_row_status(s).1).unwrap_or("agent-card-status"))>
                    {move || swarm.with(|s| s.as_ref().map(|s| swarm_row_status(s).0))}
                </span>
            </div>
            <div class="agent-card-bottom">
                <span class="agent-sidebar-swarm-counts">
                    {move || swarm.with(|s| s.as_ref().map(swarm_row_counts))}
                </span>
            </div>
        </button>
    }
    .into_any()
}

/// Swarm members and every agent they own belong to the Swarm view, which
/// lists the members and opens their conversations; the Agents list would
/// only duplicate them.
fn swarm_owned_agents(agents: &[AgentInfo]) -> HashSet<SidebarAgentRef> {
    let by_ref: HashMap<SidebarAgentRef, &AgentInfo> = agents
        .iter()
        .map(|agent| (agent_ref(agent), agent))
        .collect();
    agents
        .iter()
        .filter(|agent| {
            let mut current = Some(*agent);
            for _ in 0..=agents.len() {
                let Some(agent) = current else {
                    return false;
                };
                if agent.origin == AgentOrigin::SwarmMember {
                    return true;
                }
                current = agent.parent_agent_id.as_ref().and_then(|parent_id| {
                    by_ref
                        .get(&SidebarAgentRef {
                            host_id: agent.host_id.clone(),
                            agent_id: parent_id.clone(),
                        })
                        .copied()
                });
            }
            false
        })
        .map(agent_ref)
        .collect()
}

/// Resolve the effective "show other projects" value from the server-owned
/// project-visibility preference. It never depends on the active project:
/// opening an agent switches to that agent's project, so a project-dependent
/// default turned the selector off as soon as it was used. The unpinned
/// `ContextualDefault` therefore shows all projects everywhere.
fn effective_show_other_projects(visibility: AgentsSidebarProjectVisibility) -> bool {
    match visibility {
        AgentsSidebarProjectVisibility::ContextualDefault
        | AgentsSidebarProjectVisibility::AllProjects => true,
        AgentsSidebarProjectVisibility::CurrentProjectOnly => false,
    }
}

/// Project the server-owned sidebar preferences into the predicate input the
/// filter memo consumes. This is a pure derivation, not stored state.
fn sidebar_to_panel_filters(sidebar: &AgentsSidebarPreferences) -> AgentsPanelFilters {
    AgentsPanelFilters {
        hide_sub_agents: sidebar.hide_sub_agents,
        hide_inactive: sidebar.hide_inactive,
        show_other_projects: effective_show_other_projects(sidebar.project_visibility),
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
        protocol::BackendKind::Grok => "backend-badge grok",
        protocol::BackendKind::Opencode => "backend-badge opencode",
        protocol::BackendKind::Muse => "backend-badge muse",
    }
}

/// Every backend, in the order they are actually reached for — not
/// alphabetical. Alphabetical order buried the two backends in daily use under
/// ones that are rarely picked, on every list the user chooses from. This is
/// the single source of that order; lists that need one sort by it.
pub(crate) const BACKENDS_BY_PREFERENCE: [protocol::BackendKind; 8] = [
    protocol::BackendKind::Codex,
    protocol::BackendKind::Claude,
    protocol::BackendKind::Hermes,
    protocol::BackendKind::Antigravity,
    protocol::BackendKind::Kiro,
    protocol::BackendKind::Grok,
    protocol::BackendKind::Opencode,
    protocol::BackendKind::Muse,
];

/// Position in `BACKENDS_BY_PREFERENCE`, for use as a sort key.
pub(crate) fn backend_rank(kind: protocol::BackendKind) -> usize {
    BACKENDS_BY_PREFERENCE
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(BACKENDS_BY_PREFERENCE.len())
}

pub(crate) fn backend_label(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Tycode => "Tycode",
        protocol::BackendKind::Kiro => "Kiro",
        protocol::BackendKind::Claude => "Claude",
        protocol::BackendKind::Codex => "Codex",
        protocol::BackendKind::Antigravity => "Antigravity",
        protocol::BackendKind::Hermes => "Hermes",
        protocol::BackendKind::Grok => "Grok",
        protocol::BackendKind::Opencode => "OpenCode",
        protocol::BackendKind::Muse => "Muse",
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
    /// An interrupt has been sent and the backend has not yet answered it. A
    /// real phase, not an instant: the request can be in flight for as long as
    /// the backend takes to tear its turn down, and during that window the user
    /// needs to see that their cancel was heard.
    Cancelling,
    /// The server reports the agent waiting on a question or plan approval
    /// that only the user can answer: neither working nor finished.
    AwaitingUser,
    Idle,
    /// The turn is over but the agent still has work running: a backgrounded
    /// command, a child agent, a workflow. It will be woken when that work
    /// finishes, so the success check would wrongly say it needs the user.
    BackgroundWork,
    /// The last turn ended because the user cancelled it. Distinct from `Idle`:
    /// a cancelled turn is *not* a completed one, and rendering it with the
    /// success check told users their interrupt had produced a finished result.
    /// Cleared by the next turn: the reducer drops the agent from
    /// `last_turn_cancelled` on the next `TypingStatusChanged(true)` or
    /// `StreamStart`.
    Cancelled,
    /// The server owns a compaction request but is waiting for a safe point.
    /// Distinct from `Compacting`: "queued behind the current turn" is the
    /// state most likely to be mistaken for a hang, so it gets its own label.
    CompactionQueued,
    Compacting,
    Terminated,
}

/// The per-agent liveness maps [`derive_agent_state`] reads, borrowed from
/// [`AppState`] signals for the duration of one derivation.
struct AgentLiveness<'a> {
    streaming: &'a HashMap<AgentId, StreamingState>,
    turn_active: &'a HashMap<AgentId, bool>,
    awaiting_user: &'a HashSet<AgentId>,
    compaction: &'a HashMap<AgentId, CompactionOldInfo>,
    context_compaction: &'a HashMap<AgentId, ContextCompactionUiState>,
    last_turn_cancelled: &'a HashSet<AgentId>,
    interrupt_pending: &'a HashSet<AgentId>,
}

pub(crate) fn derive_agent_state(state: &AppState, agent: &AgentInfo) -> DerivedAgentState {
    state.compaction_in_progress.with(|compaction| {
        state.context_compactions.with(|context_compaction| {
            state.agent_turn_active.with(|turn_active| {
                state.streaming_text.with(|streaming| {
                    state.last_turn_cancelled.with(|last_turn_cancelled| {
                        state.interrupt_pending.with(|interrupt_pending| {
                            state.agent_awaiting_user.with(|awaiting_user| {
                                derive_from_liveness(
                                    agent,
                                    &AgentLiveness {
                                        streaming,
                                        turn_active,
                                        awaiting_user,
                                        compaction,
                                        context_compaction,
                                        last_turn_cancelled,
                                        interrupt_pending,
                                    },
                                )
                            })
                        })
                    })
                })
            })
        })
    })
}

fn derive_from_liveness(agent: &AgentInfo, liveness: &AgentLiveness<'_>) -> DerivedAgentState {
    let AgentLiveness {
        streaming,
        turn_active,
        awaiting_user,
        compaction,
        context_compaction,
        last_turn_cancelled,
        interrupt_pending,
    } = liveness;
    if agent.fatal_error.is_some() {
        return DerivedAgentState::Terminated;
    }
    if !agent.started {
        return DerivedAgentState::Initializing;
    }
    // Legacy replacement compaction.
    if compaction.contains_key(&agent.agent_id) {
        return DerivedAgentState::Compacting;
    }
    // In-place context compaction. Reported ahead of thinking/streaming
    // because a deferred operation coexists with a live turn by design — the
    // turn is exactly what it is waiting for — and "Compaction queued" is the
    // more informative of the two.
    if let Some(operation) = context_compaction.get(&agent.agent_id) {
        if operation.is_deferred() {
            return DerivedAgentState::CompactionQueued;
        }
        if operation.is_in_flight() {
            return DerivedAgentState::Compacting;
        }
    }
    if awaiting_user.contains(&agent.agent_id) {
        if interrupt_pending.contains(&agent.agent_id) {
            return DerivedAgentState::Cancelling;
        }
        return DerivedAgentState::AwaitingUser;
    }
    let typing = turn_active.get(&agent.agent_id).copied().unwrap_or(false);
    let streaming_open = streaming.contains_key(&agent.agent_id);
    if typing || streaming_open {
        // Still running, but the user has asked it to stop. Reporting plain
        // Thinking here hides that the interrupt was accepted at all.
        if interrupt_pending.contains(&agent.agent_id) {
            return DerivedAgentState::Cancelling;
        }
        return DerivedAgentState::Thinking;
    }
    // Not running. Distinguish "finished" from "was stopped": the terminal
    // outcome is a separate question from whether work is in flight, and only
    // the former deserves a success affordance.
    if last_turn_cancelled.contains(&agent.agent_id) {
        DerivedAgentState::Cancelled
    } else {
        DerivedAgentState::Idle
    }
}

/// [`derive_agent_state`] plus the one input it cannot see: whether the
/// in-flight tray still has running work for this agent. An idle agent
/// waiting on that work is reported as [`DerivedAgentState::BackgroundWork`].
pub(crate) fn derive_agent_state_with_background(
    state: &AppState,
    agent: &AgentInfo,
) -> DerivedAgentState {
    let derived = derive_agent_state(state, agent);
    if derived != DerivedAgentState::Idle {
        return derived;
    }
    let agent_ref = ActiveAgentRef {
        host_id: agent.host_id.clone(),
        agent_id: agent.agent_id.clone(),
    };
    if crate::components::inflight_tray::has_running_background_work(state, &agent_ref) {
        DerivedAgentState::BackgroundWork
    } else {
        DerivedAgentState::Idle
    }
}

pub(crate) fn status_label(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "Initializing",
        DerivedAgentState::Thinking => "Thinking",
        DerivedAgentState::Cancelling => "Cancelling",
        DerivedAgentState::AwaitingUser => "Needs your answer",
        DerivedAgentState::CompactionQueued => "Compaction queued",
        DerivedAgentState::Compacting => "Compacting context",
        DerivedAgentState::Idle => "Idle",
        DerivedAgentState::BackgroundWork => "Background work",
        DerivedAgentState::Cancelled => "Cancelled",
        DerivedAgentState::Terminated => "Terminated",
    }
}

fn status_label_visible(derived: &DerivedAgentState) -> bool {
    match derived {
        DerivedAgentState::Cancelling
        | DerivedAgentState::AwaitingUser
        | DerivedAgentState::CompactionQueued
        | DerivedAgentState::Compacting => true,
        DerivedAgentState::Initializing
        | DerivedAgentState::Thinking
        | DerivedAgentState::Idle
        | DerivedAgentState::BackgroundWork
        | DerivedAgentState::Cancelled
        | DerivedAgentState::Terminated => false,
    }
}

pub(crate) fn status_icon(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "\u{25F7}", // ◷ clock (CSS animates)
        DerivedAgentState::Thinking => "\u{25F7}",     // ◷ clock (CSS animates)
        DerivedAgentState::Cancelling => "\u{25F7}",   // ◷ clock (CSS animates)
        DerivedAgentState::AwaitingUser => "?",
        DerivedAgentState::CompactionQueued => "\u{27F2}", // ⟲ counter-clockwise gapped circle
        DerivedAgentState::Compacting => "\u{27F2}",       // ⟲ counter-clockwise gapped circle
        DerivedAgentState::Idle => "\u{2713}",             // ✓
        DerivedAgentState::BackgroundWork => "\u{29D7}",   // ⧗ hourglass
        DerivedAgentState::Cancelled => "\u{2298}",        // ⊘ circled division slash
        DerivedAgentState::Terminated => "\u{2022}",       // •
    }
}

pub(crate) fn status_class(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "agent-card-status running",
        DerivedAgentState::Thinking => "agent-card-status running",
        DerivedAgentState::Cancelling => "agent-card-status running",
        DerivedAgentState::AwaitingUser => "agent-card-status awaiting",
        DerivedAgentState::CompactionQueued => "agent-card-status running",
        DerivedAgentState::Compacting => "agent-card-status running",
        DerivedAgentState::Idle => "agent-card-status completed",
        DerivedAgentState::BackgroundWork => "agent-card-status background",
        DerivedAgentState::Cancelled => "agent-card-status cancelled",
        DerivedAgentState::Terminated => "agent-card-status error",
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AgentTreeGroup {
    parent: AgentInfo,
    children: Vec<AgentTreeGroup>,
}

impl AgentTreeGroup {
    fn member_count(&self) -> usize {
        1 + self.children.iter().map(Self::member_count).sum::<usize>()
    }
}

/// A swarm shown as one summary row in the project its members run in, in
/// place of the member rows the Agents list hides.
#[derive(Clone, Debug, PartialEq)]
struct SidebarSwarmRef {
    host_id: String,
    project_id: ProjectId,
    swarm_id: SwarmId,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentProjectSection {
    key: String,
    label: String,
    project: Option<ActiveProjectRef>,
    workbenches: Vec<ProjectInfo>,
    swarms: Vec<SidebarSwarmRef>,
    groups: Vec<AgentTreeGroup>,
}

impl AgentProjectSection {
    fn row_count(&self) -> usize {
        self.swarms.len()
            + self
                .groups
                .iter()
                .map(AgentTreeGroup::member_count)
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AgentHostSection {
    key: String,
    label: String,
    custom_groups: Vec<AgentCustomGroupSection>,
    projects: Vec<AgentProjectSection>,
}

#[derive(Clone, Debug, PartialEq)]
struct SidebarProjection {
    pinned: Vec<AgentTreeGroup>,
    hosts: Vec<AgentHostSection>,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentCustomGroupSection {
    group: AgentGroup,
    groups: Vec<AgentTreeGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SidebarAgentRef {
    host_id: String,
    agent_id: AgentId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarDropTarget {
    Agent(SidebarAgentRef),
    Group(AgentGroupId),
    Ungroup,
}

#[derive(Clone)]
struct AgentPointerDrag {
    agent: SidebarAgentRef,
    pointer_id: i32,
    start: (i32, i32),
    position: (i32, i32),
    active: bool,
    target: Option<SidebarDropTarget>,
}

fn pointer_drop_target(panel: &web_sys::Element, x: i32, y: i32) -> Option<SidebarDropTarget> {
    let element = panel
        .owner_document()?
        .element_from_point(x as f32, y as f32)?;
    if !panel.contains(Some(&element)) {
        return None;
    }
    let target = element.closest("[data-agent-drop-kind]").ok()??;
    match target.get_attribute("data-agent-drop-kind").as_deref() {
        Some("agent") => Some(SidebarDropTarget::Agent(SidebarAgentRef {
            host_id: target.get_attribute("data-agent-drop-host")?,
            agent_id: AgentId(target.get_attribute("data-agent-drop-id")?),
        })),
        Some("group") => Some(SidebarDropTarget::Group(AgentGroupId(
            target.get_attribute("data-group-id")?,
        ))),
        Some("ungroup") => Some(SidebarDropTarget::Ungroup),
        _ => {
            log::error!("invalid agent pointer drop target");
            None
        }
    }
}

fn scroll_agent_pointer_drag(
    panel_ref: NodeRef<leptos::html::Div>,
    scroll_ref: NodeRef<leptos::html::Div>,
    pointer_drag: RwSignal<Option<AgentPointerDrag>>,
    frame: StoredValue<Option<AnimationFrameRequestHandle>>,
    live_status: RwSignal<String>,
) {
    match request_animation_frame_with_handle(move || {
        frame.set_value(None);
        let Some(drag) = pointer_drag
            .try_get_untracked()
            .flatten()
            .filter(|drag| drag.active)
        else {
            return;
        };
        let (Some(panel), Some(scrollport)) = (
            panel_ref.try_get_untracked().flatten(),
            scroll_ref.try_get_untracked().flatten(),
        ) else {
            return;
        };
        let rect = scrollport.get_bounding_client_rect();
        let (x, y) = (f64::from(drag.position.0), f64::from(drag.position.1));
        let shift =
            if x >= rect.left() && x <= rect.right() && y >= rect.top() && y <= rect.bottom() {
                if y < rect.top() + 24.0 {
                    -12
                } else if y > rect.bottom() - 24.0 {
                    12
                } else {
                    0
                }
            } else {
                0
            };
        let before = scrollport.scroll_top();
        scrollport.set_scroll_top(before + shift);
        if scrollport.scroll_top() != before {
            let target = pointer_drop_target(&panel, drag.position.0, drag.position.1);
            pointer_drag.update(|current| {
                if let Some(current) = current {
                    current.target = target;
                }
            });
        }
        scroll_agent_pointer_drag(panel_ref, scroll_ref, pointer_drag, frame, live_status);
    }) {
        Ok(handle) => frame.set_value(Some(handle)),
        Err(error) => {
            log::error!("cannot schedule agent drag scrolling: {error:?}");
            pointer_drag.set(None);
            live_status
                .set("Cannot move agent: browser animation frames are unavailable".to_owned());
        }
    }
}

#[derive(Clone)]
struct AgentsPanelInteractions {
    editing_agent: RwSignal<Option<SidebarAgentRef>>,
    edit_value: RwSignal<String>,
    expanded_parents: RwSignal<HashSet<SidebarAgentRef>>,
    inactive_parents: RwSignal<HashSet<SidebarAgentRef>>,
    pointer_drag: RwSignal<Option<AgentPointerDrag>>,
    suppress_pointer_click: RwSignal<bool>,
    keyboard_agent: RwSignal<Option<SidebarAgentRef>>,
    keyboard_target: RwSignal<Option<SidebarDropTarget>>,
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
    fn take_tree(
        parent: AgentInfo,
        children_by_parent: &mut HashMap<SidebarAgentRef, Vec<AgentInfo>>,
    ) -> AgentTreeGroup {
        let children = children_by_parent
            .remove(&agent_ref(&parent))
            .unwrap_or_default()
            .into_iter()
            .map(|child| take_tree(child, children_by_parent))
            .collect();
        AgentTreeGroup { parent, children }
    }

    let visible_ids: HashSet<SidebarAgentRef> = agents.iter().map(agent_ref).collect();
    let mut children_by_parent: HashMap<SidebarAgentRef, Vec<AgentInfo>> = HashMap::new();
    let mut roots = Vec::new();
    for agent in agents {
        let parent = agent
            .parent_agent_id
            .as_ref()
            .map(|parent_id| SidebarAgentRef {
                host_id: agent.host_id.clone(),
                agent_id: parent_id.clone(),
            });
        match parent {
            Some(parent) if visible_ids.contains(&parent) => {
                children_by_parent.entry(parent).or_default().push(agent);
            }
            _ => roots.push(agent),
        }
    }
    let groups = roots
        .into_iter()
        .map(|parent| take_tree(parent, &mut children_by_parent))
        .collect();
    if !children_by_parent.is_empty() {
        log::error!(
            "server agent ownership contains a cycle; sidebar cannot render the affected tree"
        );
    }
    groups
}

fn build_sidebar_sections(
    groups: Vec<AgentTreeGroup>,
    swarms: Vec<SidebarSwarmRef>,
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

    type Leaf = (Vec<SidebarSwarmRef>, Vec<AgentTreeGroup>);
    let mut leaf_agents: HashMap<(String, Option<ProjectId>), Leaf> = HashMap::new();
    let mut first_seen_hosts: Vec<String> = Vec::new();
    let mut first_seen_projects: HashMap<String, Vec<Option<ProjectId>>> = HashMap::new();
    let mut leaf_for = |host_id: String, project_id: Option<ProjectId>| {
        if !known_host_order.contains(&host_id) && !first_seen_hosts.contains(&host_id) {
            first_seen_hosts.push(host_id.clone());
        }
        let project_order = first_seen_projects.entry(host_id.clone()).or_default();
        if !project_order.contains(&project_id) {
            project_order.push(project_id.clone());
        }
        (host_id, project_id)
    };
    let mut keyed_swarms = Vec::new();
    for swarm in swarms {
        keyed_swarms.push((
            leaf_for(
                swarm.host_id.clone(),
                agent_ui_project_id(&projects, &swarm.host_id, Some(&swarm.project_id)).cloned(),
            ),
            swarm,
        ));
    }
    let mut keyed_groups = Vec::new();
    for group in groups {
        keyed_groups.push((
            leaf_for(
                group.parent.host_id.clone(),
                agent_ui_project_id(
                    &projects,
                    &group.parent.host_id,
                    group.parent.project_id.as_ref(),
                )
                .cloned(),
            ),
            group,
        ));
    }
    for project in &projects {
        if project.project.parent_project_id().is_none() {
            let key = leaf_for(project.host_id.clone(), Some(project.project.id.clone()));
            leaf_agents.entry(key).or_default();
        }
    }
    for (key, swarm) in keyed_swarms {
        leaf_agents.entry(key).or_default().0.push(swarm);
    }
    for (key, group) in keyed_groups {
        leaf_agents.entry(key).or_default().1.push(group);
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

            project_order.sort_by_key(Option::is_some);
            let sections: Vec<AgentProjectSection> = project_order
                .into_iter()
                .filter_map(|project_id| {
                    let key = (host_id.clone(), project_id.clone());
                    let (swarms, mut groups) = leaf_agents.remove(&key)?;
                    groups.sort_by_key(|group| group.parent.origin != AgentOrigin::Tychat);
                    let label = project_label(&project_labels, &host_id, project_id.as_ref());
                    Some(AgentProjectSection {
                        key: project_id
                            .as_ref()
                            .map(|id| format!("{}:{}", host_id, id.0))
                            .unwrap_or_else(|| format!("{host_id}:no-project")),
                        label,
                        workbenches: projects
                            .iter()
                            .filter(|project| {
                                project.host_id == host_id
                                    && project_id.is_some()
                                    && project.project.parent_project_id() == project_id.as_ref()
                            })
                            .cloned()
                            .collect(),
                        project: project_id.map(|project_id| ActiveProjectRef {
                            host_id: host_id.clone(),
                            project_id,
                        }),
                        swarms,
                        groups,
                    })
                })
                .collect();

            (!sections.is_empty()).then(|| AgentHostSection {
                key: host_id.clone(),
                label: host_label(&host_labels, &host_id),
                custom_groups: Vec::new(),
                projects: sections,
            })
        })
        .collect()
}

fn agent_ref(agent: &AgentInfo) -> SidebarAgentRef {
    SidebarAgentRef {
        host_id: agent.host_id.clone(),
        agent_id: agent.agent_id.clone(),
    }
}

fn build_sidebar_projection(
    agents: Vec<AgentInfo>,
    swarms: Vec<SidebarSwarmRef>,
    configured_hosts: Vec<crate::bridge::ConfiguredHost>,
    projects: Vec<ProjectInfo>,
    groups_snapshot: AgentGroupsSnapshot,
    pins: AgentPinsSnapshot,
) -> SidebarProjection {
    // Keep unpinned descendants with their owner; independently pinned children
    // become roots exactly once and rejoin that owner on unpin.
    fn extract_pins(
        mut tree: AgentTreeGroup,
        pins: &HashSet<AgentAnnotationTarget>,
        pinned: &mut Vec<AgentTreeGroup>,
    ) -> Option<AgentTreeGroup> {
        tree.children = tree
            .children
            .into_iter()
            .filter_map(|child| extract_pins(child, pins, pinned))
            .collect();
        if pins.contains(&agent_annotation_target(&tree.parent)) {
            pinned.push(tree);
            None
        } else {
            Some(tree)
        }
    }
    let host_labels = configured_hosts
        .iter()
        .map(|host| (host.id.clone(), host.label.clone()))
        .collect();
    let mut host_order: Vec<String> = configured_hosts
        .iter()
        .map(|host| host.id.clone())
        .collect();
    for host in projects.iter().map(|project| &project.host_id).chain(
        swarms
            .iter()
            .map(|swarm| &swarm.host_id)
            .chain(agents.iter().map(|agent| &agent.host_id)),
    ) {
        if !host_order.contains(host) {
            host_order.push(host.clone());
        }
    }
    let order: HashMap<_, _> = agents
        .iter()
        .enumerate()
        .map(|(index, agent)| (agent_ref(agent), index))
        .collect();
    let pinned_targets = pins.pinned.into_iter().collect();
    let mut pinned = Vec::new();
    let trees = build_parent_child_groups(agents)
        .into_iter()
        .filter_map(|tree| extract_pins(tree, &pinned_targets, &mut pinned))
        .collect::<Vec<_>>();
    pinned.sort_by_key(|tree| order[&agent_ref(&tree.parent)]);

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

    let mut grouped_agents = HashMap::<AgentGroupId, Vec<AgentTreeGroup>>::new();
    let mut ungrouped_agents = Vec::new();
    for tree in trees {
        let target = agent_annotation_target(&tree.parent);
        if let Some(group_id) = assignments.get(&target) {
            grouped_agents
                .entry(group_id.clone())
                .or_default()
                .push(tree);
        } else {
            ungrouped_agents.push(tree);
        }
    }

    let custom_groups: Vec<_> = groups_snapshot
        .groups
        .into_iter()
        .filter_map(|group| {
            let groups = grouped_agents.remove(&group.id)?;
            (!groups.is_empty()).then_some(AgentCustomGroupSection { group, groups })
        })
        .collect();

    let mut default_hosts: HashMap<_, _> =
        build_sidebar_sections(ungrouped_agents, swarms, configured_hosts, projects)
            .into_iter()
            .map(|host| (host.key.clone(), host.projects))
            .collect();
    let hosts = host_order
        .into_iter()
        .filter_map(|host_id| {
            let host_custom = custom_groups
                .iter()
                .filter_map(|section| {
                    let groups = section
                        .groups
                        .iter()
                        .filter(|tree| tree.parent.host_id == host_id)
                        .cloned()
                        .collect::<Vec<_>>();
                    (!groups.is_empty()).then(|| AgentCustomGroupSection {
                        group: section.group.clone(),
                        groups,
                    })
                })
                .collect::<Vec<_>>();
            let projects = default_hosts.remove(&host_id).unwrap_or_default();
            (!host_custom.is_empty() || !projects.is_empty()).then(|| AgentHostSection {
                label: host_label(&host_labels, &host_id),
                key: host_id,
                custom_groups: host_custom,
                projects,
            })
        })
        .collect();
    SidebarProjection { pinned, hosts }
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
    keyboard_agent: RwSignal<Option<SidebarAgentRef>>,
    keyboard_target: RwSignal<Option<SidebarDropTarget>>,
    group_live_status: RwSignal<String>,
) -> impl IntoView {
    let ungroup_keyboard_state = state;
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
            on:focus=move |_| keyboard_target.set(Some(SidebarDropTarget::Ungroup))
            data-agent-drop-kind="ungroup"
            on:keydown=ungroup_on_keydown
        >
            "Ungroup"
        </div>
    }
}

pub(crate) const COLLAPSED_FOLDERS_STORAGE_KEY: &str = "tyde.agents.collapsed-folders.v1";

fn load_collapsed_folders() -> HashSet<String> {
    #[cfg(target_arch = "wasm32")]
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
        && let Ok(Some(encoded)) = storage.get_item(COLLAPSED_FOLDERS_STORAGE_KEY)
        && let Ok(folders) = serde_json::from_str(&encoded)
    {
        return folders;
    }
    HashSet::new()
}

fn folder_toggle(
    collapsed: RwSignal<HashSet<String>>,
    key: String,
    label: String,
    display_label: String,
    count: usize,
) -> impl IntoView {
    let toggle_key = key.clone();
    let expanded = Memo::new(move |_| !collapsed.with(|folders| folders.contains(&key)));
    view! {
        <button
            type="button"
            class="agent-folder-toggle"
            aria-label=label.clone()
            title=label.clone()
            aria-expanded=move || expanded.get().to_string()
            on:keydown=move |ev: web_sys::KeyboardEvent| ev.stop_propagation()
            on:click=move |ev: web_sys::MouseEvent| {
                ev.stop_propagation();
                collapsed.update(|folders| {
                    if !folders.remove(&toggle_key) {
                        folders.insert(toggle_key.clone());
                    }
                });
                if let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten()) {
                    let encoded = collapsed.with_untracked(|folders| serde_json::to_string(folders).expect("folder keys serialize"));
                    if let Err(error) = storage.set_item(COLLAPSED_FOLDERS_STORAGE_KEY, &encoded) {
                        log::warn!("failed to persist collapsed agent folders: {error:?}");
                    }
                }
            }
        >
            <svg class="agent-folder-chevron" class:expanded=move || expanded.get()
                width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <path d="m6 3 5 5-5 5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
            <svg class="agent-folder-icon" width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path d="M3 7V5a1 1 0 0 1 1-1h5l2 3h9a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V7h18" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" />
            </svg>
            <span class="agent-folder-label">{display_label}</span>
            {(count > 0).then(|| view! { <span class="agent-folder-count" aria-hidden="true">{count}</span> })}
        </button>
    }
}

fn folder_members(
    state: AppState,
    swarms: Vec<SidebarSwarmRef>,
    groups: Vec<AgentTreeGroup>,
    interactions: AgentsPanelInteractions,
    collapsed: RwSignal<HashSet<String>>,
    key: String,
) -> impl IntoView {
    let expanded = Memo::new(move |_| !collapsed.with(|folders| folders.contains(&key)));
    move || {
        if expanded.get() {
            let swarm_rows = swarms
                .clone()
                .into_iter()
                .map(|swarm| render_swarm_row(state.clone(), swarm))
                .collect_view();
            let agent_rows = groups
                .clone()
                .into_iter()
                .map(|group| render_agent_tree_group(state.clone(), group, interactions.clone()))
                .collect_view();
            view! { {swarm_rows} {agent_rows} }.into_any()
        } else {
            ().into_any()
        }
    }
}

fn restored_agent_label(agent: &protocol::RestoredAgentRef) -> String {
    match agent {
        protocol::RestoredAgentRef::Session {
            session_id, name, ..
        } => name
            .clone()
            .unwrap_or_else(|| format!("session {}", session_id.0)),
        protocol::RestoredAgentRef::StartupReservation { agent_id, name } => {
            name.clone().unwrap_or_else(|| format!("agent {agent_id}"))
        }
    }
}

fn restoration_failure_text(failure: &protocol::AgentRestorationFailure) -> String {
    let reason = match &failure.reason {
        protocol::AgentRestorationFailureReason::ReconstructFailed { message } => message.clone(),
        protocol::AgentRestorationFailureReason::ParentNotRestored => {
            "its parent agent was not restored".to_owned()
        }
    };
    let mut text = format!("{}: {reason}.", restored_agent_label(&failure.agent));
    if !failure.skipped.is_empty() {
        let skipped = failure
            .skipped
            .iter()
            .map(restored_agent_label)
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(" Also not restored: {skipped}."));
    }
    text.push_str(match failure.retry {
        protocol::AgentRestorationRetry::NextLaunch => " Restore will be retried on next launch.",
        protocol::AgentRestorationRetry::Abandoned => " It will not be restored.",
    });
    text
}

#[component]
fn AgentRestorationNotice() -> impl IntoView {
    let state = expect_context::<AppState>();
    move || {
        let mut hosts = state
            .agent_restoration_failures
            .get()
            .into_iter()
            .collect::<Vec<_>>();
        hosts.sort_by(|a, b| a.0.cmp(&b.0));
        hosts
            .into_iter()
            .map(|(_, failures)| {
                view! {
                    <div class="workflow-error-banner" role="alert">
                        <span class="workflow-error-banner-message">
                            "Some open agents could not be restored."
                        </span>
                        <ul>
                            {failures
                                .iter()
                                .map(|failure| view! { <li>{restoration_failure_text(failure)}</li> })
                                .collect_view()}
                        </ul>
                    </div>
                }
            })
            .collect_view()
    }
}

fn sidebar_agents(state: &AppState, filters: &AgentsPanelFilters, query: &str) -> Vec<AgentInfo> {
    let active_project = state.active_project.get();
    let projects = state.projects.get();
    let pins = state.agents_view_preferences.get().pins.pinned;

    // Read the noisy maps in place via `with` rather than cloning
    // them up-front. The Memo re-runs on every keystroke in the
    // panel-search input, and cloning the streaming/turn-active
    // HashMaps + the full agents Vec on each keystroke was the
    // dominant per-keystroke cost in the audit.
    state.streaming_text.with(|streaming_map| {
        state.agent_turn_active.with(|turn_active_map| {
            state.agent_awaiting_user.with(|awaiting_user| {
                state.agents.with(|agents| {
                    let swarm_owned = swarm_owned_agents(agents);
                    agents
                        .iter()
                        .filter(|a| {
                            if pins.contains(&agent_annotation_target(a)) {
                                return query.is_empty() || a.name.to_lowercase().contains(query);
                            }
                            !swarm_owned.contains(&agent_ref(a))
                                && agent_passes_filters(
                                    a,
                                    filters,
                                    match active_project.as_ref() {
                                        None => a.project_id.is_none(),
                                        Some(ap) => {
                                            a.host_id == ap.host_id
                                                && agent_ui_project_id(
                                                    &projects,
                                                    &a.host_id,
                                                    a.project_id.as_ref(),
                                                ) == agent_ui_project_id(
                                                    &projects,
                                                    &ap.host_id,
                                                    Some(&ap.project_id),
                                                )
                                        }
                                    },
                                    streaming_map,
                                    turn_active_map,
                                    awaiting_user,
                                    query,
                                )
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
        })
    })
}

fn sidebar_swarms(
    state: &AppState,
    filters: &AgentsPanelFilters,
    query: &str,
) -> Vec<SidebarSwarmRef> {
    let active_project = state.active_project.get();
    let projects = state.projects.get();
    state.swarms.with(|by_host| {
        let mut refs: Vec<(String, SidebarSwarmRef)> = by_host
            .iter()
            .flat_map(|(host_id, swarms)| swarms.values().map(move |swarm| (host_id, swarm)))
            .filter(|(host_id, swarm)| {
                swarm_passes_filters(
                    host_id,
                    swarm,
                    filters,
                    active_project.as_ref(),
                    &projects,
                    query,
                )
            })
            .map(|(host_id, swarm)| {
                (
                    swarm.name.to_lowercase(),
                    SidebarSwarmRef {
                        host_id: host_id.clone(),
                        project_id: swarm.constraints.project_id.clone(),
                        swarm_id: swarm.id.clone(),
                    },
                )
            })
            .collect();
        refs.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.swarm_id.0.cmp(&b.1.swarm_id.0))
        });
        refs.into_iter().map(|(_, swarm)| swarm).collect::<Vec<_>>()
    })
}

#[component]
pub fn AgentsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());
    let collapsed_folders = RwSignal::new(load_collapsed_folders());
    // Disclosure state is presentation only; children start collapsed.
    let expanded_parents = RwSignal::new(HashSet::new());
    let inactive_parents = RwSignal::new(HashSet::new());
    // Editing state lives here so it survives agent list re-renders caused by
    // streaming / turn-active updates. Only one agent can be renamed at a time.
    let editing_agent: RwSignal<Option<SidebarAgentRef>> = RwSignal::new(None);
    let edit_value: RwSignal<String> = RwSignal::new(String::new());
    let editing_group: RwSignal<Option<AgentGroupId>> = RwSignal::new(None);
    let group_edit_value: RwSignal<String> = RwSignal::new(String::new());
    let pointer_drag: RwSignal<Option<AgentPointerDrag>> = RwSignal::new(None);
    let suppress_pointer_click = RwSignal::new(false);
    let panel_ref = NodeRef::<leptos::html::Div>::new();
    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    let scroll_frame = StoredValue::new(None::<AnimationFrameRequestHandle>);
    let dragged_agent = Memo::new(move |_| {
        pointer_drag.with(|drag| {
            drag.as_ref()
                .filter(|drag| drag.active)
                .map(|drag| drag.agent.clone())
        })
    });
    let drag_hover_group = Memo::new(move |_| {
        pointer_drag.with(
            |drag| match drag.as_ref().and_then(|drag| drag.target.as_ref()) {
                Some(SidebarDropTarget::Group(group)) => Some(group.clone()),
                _ => None,
            },
        )
    });
    let keyboard_agent: RwSignal<Option<SidebarAgentRef>> = RwSignal::new(None);
    let keyboard_target: RwSignal<Option<SidebarDropTarget>> = RwSignal::new(None);
    let pending_rename_group_name: RwSignal<Option<String>> = RwSignal::new(None);
    let group_live_status: RwSignal<String> = RwSignal::new(String::new());
    let interactions = AgentsPanelInteractions {
        editing_agent,
        edit_value,
        expanded_parents,
        inactive_parents,
        pointer_drag,
        suppress_pointer_click,
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
    // overlay (`effective_agents_sidebar_preferences`) — no component-local
    // persistence, so it can never become a second source of truth or a
    // flicker source.
    let filters_state = state.clone();
    let current_filters = Memo::new(move |_| {
        let sidebar = filters_state.effective_agents_sidebar_preferences();
        sidebar_to_panel_filters(&sidebar)
    });

    let workbench_filter_state = state.clone();
    let show_workbenches = Memo::new(move |_| {
        workbench_filter_state
            .effective_agents_sidebar_preferences()
            .show_workbenches
    });

    let filter_state = state.clone();
    let filtered_agents = Memo::new(move |_| {
        sidebar_agents(
            &filter_state,
            &current_filters.get(),
            &search.get().to_lowercase(),
        )
    });

    let swarm_filter_state = state.clone();
    let filtered_swarms = Memo::new(move |_| {
        sidebar_swarms(
            &swarm_filter_state,
            &current_filters.get(),
            &search.get().to_lowercase(),
        )
    });

    let launch_target = RwSignal::new(None::<LaunchDestination>);
    let launch_open = RwSignal::new(false);
    let launch_destination = Memo::new(move |_| launch_target.get());
    let launch = Callback::new(move |target| {
        launch_target.set(Some(target));
        launch_open.set(true);
    });

    let section_state = state.clone();
    let projection = Memo::new(move |_| {
        let snapshot = section_state.agents_view_preferences.get();
        let projects = section_state.projects.get();
        let active = section_state.active_project.get();
        let show_other = current_filters.get().show_other_projects;
        let hide_empty = section_state
            .effective_agents_sidebar_preferences()
            .hide_empty_projects;
        let scoped_projects: Vec<_> = projects
            .iter()
            .filter(|project| {
                show_other
                    || active.as_ref().is_some_and(|active| {
                        project.host_id == active.host_id
                            && agent_ui_project_id(
                                &projects,
                                &project.host_id,
                                Some(&project.project.id),
                            ) == agent_ui_project_id(
                                &projects,
                                &active.host_id,
                                Some(&active.project_id),
                            )
                    })
            })
            .cloned()
            .collect();
        // Occupancy uses the same placement rules as rows, without search or
        // activity narrowing. Pins, custom groups and hidden children cannot
        // leave an empty heading behind in their original project.
        let occupancy = hide_empty.then(|| {
            let mut filters = current_filters.get();
            filters.hide_inactive = false;
            let agents = sidebar_agents(&section_state, &filters, "");
            let swarms = sidebar_swarms(&section_state, &filters, "");
            build_sidebar_projection(
                agents,
                swarms,
                section_state.configured_hosts.get(),
                scoped_projects.clone(),
                snapshot.groups.clone(),
                snapshot.pins.clone(),
            )
        });
        let mut projection = build_sidebar_projection(
            filtered_agents.get(),
            filtered_swarms.get(),
            section_state.configured_hosts.get(),
            scoped_projects,
            snapshot.groups,
            snapshot.pins,
        );
        if let Some(occupancy) = occupancy {
            for host in &mut projection.hosts {
                let occupied_host = occupancy.hosts.iter().find(|entry| entry.key == host.key);
                host.projects.retain_mut(|section| {
                    let occupied = occupied_host.and_then(|host| {
                        host.projects.iter().find(|entry| entry.key == section.key)
                    });
                    // Narrowing may filter out an owner and promote a matching
                    // child out of Pinned/a custom group into this project.
                    let visible_projects: HashSet<_> = section
                        .groups
                        .iter()
                        .filter_map(|tree| tree.parent.project_id.as_ref())
                        .chain(section.swarms.iter().map(|swarm| &swarm.project_id))
                        .collect();
                    section.workbenches.retain(|workbench| {
                        visible_projects.contains(&workbench.project.id)
                            || occupied.is_some_and(|occupied| {
                                occupied.groups.iter().any(|tree| {
                                    tree.parent.project_id.as_ref() == Some(&workbench.project.id)
                                }) || occupied
                                    .swarms
                                    .iter()
                                    .any(|swarm| swarm.project_id == workbench.project.id)
                            })
                    });
                    section.row_count() > 0
                        || occupied.is_some_and(|section| section.row_count() > 0)
                });
            }
            projection
                .hosts
                .retain(|host| !host.projects.is_empty() || !host.custom_groups.is_empty());
        }
        projection
    });

    Effect::new(move |_| {
        let active = dragged_agent.get().is_some();
        if let Some(frame) = scroll_frame.get_value() {
            frame.cancel();
            scroll_frame.set_value(None);
        }
        if active {
            scroll_agent_pointer_drag(
                panel_ref,
                scroll_ref,
                pointer_drag,
                scroll_frame,
                group_live_status,
            );
        }
    });
    let pointer_move = window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(mut drag) = pointer_drag.get_untracked() else {
            return;
        };
        if ev.pointer_id() != drag.pointer_id {
            return;
        }
        if ev.buttons() & 1 == 0 {
            pointer_drag.set(None);
            return;
        }
        drag.position = (ev.client_x(), ev.client_y());
        let dx = f64::from(drag.position.0) - f64::from(drag.start.0);
        let dy = f64::from(drag.position.1) - f64::from(drag.start.1);
        if !drag.active && dx * dx + dy * dy < 36.0 {
            return;
        }
        ev.prevent_default();
        drag.active = true;
        suppress_pointer_click.set(true);
        drag.target = panel_ref
            .get_untracked()
            .and_then(|panel| pointer_drop_target(&panel, ev.client_x(), ev.client_y()));
        pointer_drag.set(Some(drag));
    });
    let pointer_drop_state = state.clone();
    let pointer_up = window_event_listener(leptos::ev::pointerup, move |ev| {
        let Some(drag) = pointer_drag.get_untracked() else {
            return;
        };
        if ev.pointer_id() != drag.pointer_id || ev.button() != 0 {
            return;
        }
        let target = panel_ref
            .get_untracked()
            .and_then(|panel| pointer_drop_target(&panel, ev.client_x(), ev.client_y()));
        pointer_drag.set(None);
        if !drag.active {
            return;
        }
        ev.prevent_default();
        match target {
            Some(SidebarDropTarget::Agent(target)) => {
                if let Some(target) = agent_for_ref(&pointer_drop_state, &target) {
                    drop_agent_on_agent(
                        &pointer_drop_state,
                        Some(drag.agent),
                        target,
                        pending_rename_group_name,
                        group_live_status,
                    );
                } else {
                    group_live_status.set(
                        "Destination agent is no longer available. Choose another destination."
                            .to_owned(),
                    );
                }
            }
            Some(SidebarDropTarget::Group(group)) => drop_agent_on_group(
                &pointer_drop_state,
                Some(drag.agent),
                group,
                group_live_status,
            ),
            Some(SidebarDropTarget::Ungroup) => {
                drop_agent_on_ungroup(&pointer_drop_state, Some(drag.agent), group_live_status)
            }
            None => group_live_status.set("Move cancelled".to_owned()),
        }
    });
    let pointer_cancel = window_event_listener(leptos::ev::pointercancel, move |ev| {
        if pointer_drag.with_untracked(|drag| {
            drag.as_ref()
                .is_some_and(|drag| drag.pointer_id == ev.pointer_id())
        }) {
            pointer_drag.set(None);
            group_live_status.set("Move cancelled".to_owned());
        }
    });
    let pointer_blur = window_event_listener(leptos::ev::blur, move |_| pointer_drag.set(None));
    on_cleanup(move || {
        if let Some(frame) = scroll_frame.get_value() {
            frame.cancel();
        }
        pointer_move.remove();
        pointer_up.remove();
        pointer_cancel.remove();
        pointer_blur.remove();
    });
    let preview_state = StoredValue::new(state.clone());

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
            let mut sidebar = state.effective_agents_sidebar_preferences();
            sidebar.project_visibility =
                if effective_show_other_projects(sidebar.project_visibility) {
                    AgentsSidebarProjectVisibility::CurrentProjectOnly
                } else {
                    AgentsSidebarProjectVisibility::AllProjects
                };
            persist_sidebar(&state, sidebar);
        }
    };

    let toggle_workbenches = {
        let state = state.clone();
        move |_| {
            let mut sidebar = state.effective_agents_sidebar_preferences();
            sidebar.show_workbenches = !sidebar.show_workbenches;
            persist_sidebar(&state, sidebar);
        }
    };

    let empty_state = state.clone();
    let hide_empty_projects = Memo::new(move |_| {
        empty_state
            .effective_agents_sidebar_preferences()
            .hide_empty_projects
    });
    let toggle_empty_state = state.clone();
    let toggle_empty = move |_| {
        let mut sidebar = toggle_empty_state.effective_agents_sidebar_preferences();
        sidebar.hide_empty_projects = !sidebar.hide_empty_projects;
        persist_sidebar(&toggle_empty_state, sidebar);
    };
    let toolbar_state = state.clone();

    view! {
        <div class="panel agents-panel" node_ref=panel_ref
            data-agent-dragging=move || dragged_agent.get().is_some().to_string()
            on:keydown:capture=move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" && pointer_drag.get_untracked().is_some() {
                    pointer_drag.set(None);
                    group_live_status.set("Move cancelled".to_owned());
                }
            }>
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
                <button type="button" class="agents-new-button" title="New agent" aria-label="New agent"
                    on:click=move |_| {
                        launch_target.set(
                            toolbar_state
                                .active_project_ref_untracked()
                                .map(LaunchDestination::Project),
                        );
                        launch_open.set(true);
                    }><span aria-hidden="true">"+"</span><span class="agents-new-label">"New agent"</span></button>
            </div>
            <details class="agents-filter-menu" on:keydown=move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" {
                    ev.prevent_default();
                    ev.stop_propagation();
                    if let Some(menu) = ev.current_target().and_then(|target| target.dyn_into::<web_sys::HtmlElement>().ok()) {
                        let _ = menu.remove_attribute("open");
                        if let Ok(Some(summary)) = menu.query_selector("summary") {
                            let _ = summary.unchecked_into::<web_sys::HtmlElement>().focus();
                        }
                    }
                }
            }>
                <summary aria-label="Filter agents" title="Filter agents">"▽"</summary>
                <div class="panel-filters">
                    <label class="agents-filter-option settings-toggle">
                        <span>"Hide inactive"</span>
                        <input type="checkbox" aria-label="Hide inactive" prop:checked=move || current_filters.get().hide_inactive
                            on:change=toggle_inactive />
                        <span class="settings-toggle-slider" aria-hidden="true"></span>
                    </label>
                    <label class="agents-filter-option settings-toggle">
                        <span>"Hide sub-agents"</span>
                        <input type="checkbox" aria-label="Hide sub-agents" prop:checked=move || current_filters.get().hide_sub_agents
                            on:change=toggle_sub />
                        <span class="settings-toggle-slider" aria-hidden="true"></span>
                    </label>
                    <label class="agents-filter-option settings-toggle">
                        <span>"Show other projects"</span>
                        <input type="checkbox" aria-label="Show other projects" prop:checked=move || current_filters.get().show_other_projects
                            on:change=toggle_other_projects />
                        <span class="settings-toggle-slider" aria-hidden="true"></span>
                    </label>
                    <label class="agents-filter-option settings-toggle">
                        <span>"Show workbenches"</span>
                        <input type="checkbox" aria-label="Show workbenches" prop:checked=move || show_workbenches.get()
                            on:change=toggle_workbenches />
                        <span class="settings-toggle-slider" aria-hidden="true"></span>
                    </label>
                    <label class="agents-filter-option settings-toggle">
                        <span>"Hide empty projects"</span>
                        <input type="checkbox" aria-label="Hide empty projects" prop:checked=move || hide_empty_projects.get()
                            on:change=toggle_empty />
                        <span class="settings-toggle-slider" aria-hidden="true"></span>
                    </label>
                </div>
            </details>
            <AgentRestorationNotice />
            <div class="panel-content" node_ref=scroll_ref>
                {move || {
                    let projection = projection.get();
                    if projection.pinned.is_empty() && projection.hosts.is_empty() {
                        view! {
                            <div class="panel-empty">"No agents yet"</div>
                        }.into_any()
                    } else {
                        let has_custom_groups = !state.agents_view_preferences.get_untracked().groups.groups.is_empty();
                        let ungroup_target_state = state.clone();
                        let pinned = (!projection.pinned.is_empty()).then(|| view! {
                            <section class="agent-sidebar-pinned" aria-label="Pinned">
                                <div class="agent-sidebar-groups-heading">"Pinned"</div>
                                {projection.pinned.into_iter().map(|tree| render_agent_tree_group(state.clone(), tree, interactions.clone())).collect_view()}
                            </section>
                        });
                        let hosts = projection.hosts.into_iter().map(|host| {
                        let add_project_state = state.clone();
                        let add_project_host = host.key.clone();
                        let host_id_for_projects = host.key.clone();
                        let host_label_for_projects = host.label.clone();
                        let custom_groups = host.custom_groups;
                        let custom_groups_view = if custom_groups.is_empty() {
                            ().into_any()
                        } else {
                            view! {
                                    <section class="agent-sidebar-custom-groups-section">
                                        <div class="agent-sidebar-groups-heading">"Groups"</div>
                                        {custom_groups.into_iter().map(|custom_group| {
                                            let group_id = custom_group.group.id.clone();
                                            let group_id_attr = group_id.0.clone();
                                            let folder_key = format!("group:{}", group_id.0);
                                            let member_count = custom_group.groups.iter().map(|group| group.member_count()).sum();
                                            let group_name = custom_group.group.name.clone();
                                            let header_group_id = group_id.clone();
                                            let hover_class_group_id = group_id.clone();
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
                                            let members_key = folder_key.clone();
                                            view! {
                                                <section
                                                    class=move || {
                                                        if drag_hover_group.get().as_ref() == Some(&hover_class_group_id) {
                                                            "agent-sidebar-custom-group agent-sidebar-custom-group-drag-over"
                                                        } else {
                                                            "agent-sidebar-custom-group"
                                                        }
                                                    }
                                                    data-agent-drop-kind="group"
                                                    data-group-id=group_id_attr
                                                >
                                                    <div
                                                        class=move || if keyboard_target.get().as_ref() == Some(&SidebarDropTarget::Group(header_class_group_id.clone())) {
                                                            "agent-sidebar-custom-group-header agent-group-keyboard-focus"
                                                        } else {
                                                            "agent-sidebar-custom-group-header"
                                                        }
                                                        tabindex="0"
                                                        data-agent-group-keyboard-target="true"
                                                        aria-dropeffect=move || if keyboard_agent.get().is_some() { "move" } else { "none" }
                                                        on:focus=move |_| keyboard_target.set(Some(SidebarDropTarget::Group(header_focus_group_id.clone())))
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
                                                                    {folder_toggle(collapsed_folders, folder_key.clone(), group_name.clone(), group_name.clone(), member_count)}
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
                                                    {folder_members(state.clone(), Vec::new(), custom_group.groups, interactions.clone(), collapsed_folders, members_key)}
                                                </section>
                                            }
                                        }).collect_view()}
                                    </section>
                                }.into_any()
                        };
                        view! {
                            <section class="agent-sidebar-host-section" data-host-id=host.key>
                                <div class="agent-sidebar-host-header">
                                    <span aria-label=format!("Host: {}", host.label)>{host.label.clone()}</span>
                                    <button type="button" class="agent-new-project" aria-label=format!("New project on {}", host.label)
                                        on:click=move |_| crate::components::host_browser::open_project_browser_for_host(&add_project_state, &add_project_host)>"+ Project"</button>
                                </div>
                                <div
                                    class="agent-sidebar-default-tree"
                                    data-agent-drop-kind="ungroup"
                                >
                                            {host.projects.into_iter().map(|project| {
                                                let no_project_host = host_id_for_projects.clone();
                                                let no_project_host_label = host_label_for_projects.clone();
                                                let folder_key = format!("project:{}", project.key);
                                                let member_count = project.row_count();
                                                let editor = state.project_editor;
                                                let target = project.project.clone();
                                                let heading = if target.is_some() { format!("Project: {}", project.label) } else { project.label.clone() };
                                                let edit_name = format!("Edit project {}", project.label);
                                                view! {
                                                    <section class="agent-sidebar-project-section" data-project-key=project.key.clone()>
                                                        <div class="agent-sidebar-project-header">
                                                            {folder_toggle(collapsed_folders, folder_key.clone(), heading, project.label.clone(), member_count)}
                                                            {(member_count > 0).then(|| view! { <span class="project-heading-count" aria-label=format!("{member_count} agents")>{member_count}</span> })}
                                                            {match target {
                                                                Some(target) => view! {
                                                                    <ProjectNewAgent
                                                                        destination=LaunchDestination::Project(target.clone())
                                                                        name=format!("New agent in {}", project.label)
                                                                        launch=launch
                                                                    />
                                                                    <button class="project-edit-pencil" title=edit_name.clone() aria-label=edit_name
                                                                        on:click=move |_| editor.set(Some(target.clone()))>"⋯"</button>
                                                                }.into_any(),
                                                                None => view! {
                                                                    <ProjectNewAgent
                                                                        destination=LaunchDestination::NoProject { host_id: no_project_host.clone() }
                                                                        name=format!("New agent without a project on {no_project_host_label}")
                                                                        launch=launch
                                                                    />
                                                                }.into_any(),
                                                            }}
                                                        </div>
                                                        {project_members(state.clone(), project.clone(), interactions.clone(), collapsed_folders, folder_key, show_workbenches, launch)}
                                                    </section>
                                                }
                                            }).collect_view()}
                                </div>
                                {custom_groups_view}
                            </section>
                        }
                        }).collect_view();
                        view! { <div class="agent-card-list">
                                {pinned}
                                <div class="agent-group-live-status" aria-live="polite">
                                    {move || group_live_status.get()}
                                </div>
                                {move || {
                                    let dragging_or_picking =
                                        dragged_agent.get().is_some() || keyboard_agent.get().is_some();
                                    if dragging_or_picking && has_custom_groups {
                                        render_ungroup_drop_target(
                                            ungroup_target_state.clone(),
                                            keyboard_agent,
                                            keyboard_target,
                                            group_live_status,
                                        ).into_any()
                                    } else {
                                        ().into_any()
                                    }
                                }}
                            {hosts}
                        </div> }.into_any()
                    }
                }}
            </div>
            <leptos::portal::Portal>
                {move || pointer_drag.get().filter(|drag| drag.active).and_then(|drag| {
                    let (agent, derived) = preview_state.with_value(|state| {
                        let agent = state.agents.with(|agents| agents.iter().find(|agent| agent_ref(agent) == drag.agent).cloned())?;
                        let derived = derive_agent_state_with_background(state, &agent);
                        Some((agent, derived))
                    })?;
                    Some(view! {
                        <div class="agent-row-drag-preview" aria-hidden="true"
                            style=format!("left:clamp(8px, {}px, calc(100vw - 248px));top:clamp(8px, {}px, calc(100vh - 44px))", drag.position.0 + 14, drag.position.1 + 14)>
                            <span class=status_class(&derived) title=status_label(&derived)>
                                <span aria-hidden="true">{status_icon(&derived)}</span>
                                {status_label_visible(&derived).then(|| view! {
                                    <span class="agent-card-status-text">{status_label(&derived)}</span>
                                })}
                            </span>
                            <span class="agent-card-name">{agent.name.clone()}</span>
                            <span class="agent-backend-glyph">{crate::components::launch_menu::backend_icon(agent.backend_kind)}</span>
                        </div>
                    })
                })}
            </leptos::portal::Portal>
            <Show when=move || launch_open.get()>
                <leptos::portal::Portal><crate::components::launch_menu::LaunchChooser open_sig=launch_open destination=launch_destination /></leptos::portal::Portal>
            </Show>
        </div>
    }
}

fn project_members(
    state: AppState,
    project: AgentProjectSection,
    interactions: AgentsPanelInteractions,
    collapsed: RwSignal<HashSet<String>>,
    key: String,
    show_workbenches: Memo<bool>,
    launch: Callback<LaunchDestination>,
) -> impl IntoView {
    let expanded_key = key.clone();
    let project_expanded =
        Memo::new(move |_| !collapsed.with(|folders| folders.contains(&expanded_key)));
    move || {
        if !show_workbenches.get() {
            return folder_members(
                state.clone(),
                project.swarms.clone(),
                project.groups.clone(),
                interactions.clone(),
                collapsed,
                key.clone(),
            )
            .into_any();
        }
        if !project_expanded.get() {
            return ().into_any();
        }
        let mut groups = project.groups.clone();
        let mut swarms = project.swarms.clone();
        let workbenches = project.workbenches.iter().map(|workbench| {
            let target = ActiveProjectRef { host_id: workbench.host_id.clone(), project_id: workbench.project.id.clone() };
            let (members, remaining): (Vec<_>, Vec<_>) = groups.drain(..).partition(|tree| tree.parent.project_id.as_ref() == Some(&target.project_id));
            groups = remaining;
            let (member_swarms, remaining): (Vec<_>, Vec<_>) = swarms.drain(..).partition(|swarm| swarm.project_id == target.project_id);
            swarms = remaining;
            let count = members.iter().map(AgentTreeGroup::member_count).sum::<usize>() + member_swarms.len();
            let folder_key = format!("workbench:{}:{}", target.host_id, target.project_id.0);
            let label = workbench.project.name.clone();
            let edit_label = format!("Edit project {label}");
            let editor = state.project_editor;
            view! {
                <section class="agent-sidebar-workbench-section" data-project-id=target.project_id.0.clone()>
                    <div class="agent-sidebar-project-header agent-sidebar-workbench">
                        {folder_toggle(collapsed, folder_key.clone(), format!("Workbench: {label}"), label.clone(), count)}
                        {(count > 0).then(|| view! { <span class="project-heading-count">{count}</span> })}
                        <ProjectNewAgent destination=LaunchDestination::Project(target.clone()) name=format!("New agent in {label}") launch=launch />
                        <button class="project-edit-pencil" title=edit_label.clone() aria-label=edit_label
                            on:click={let target = target.clone(); move |_| editor.set(Some(target.clone()))}>"⋯"</button>
                    </div>
                    {folder_members(state.clone(), member_swarms, members, interactions.clone(), collapsed, folder_key)}
                </section>
            }
        }).collect_view();
        view! {
            {folder_members(state.clone(), swarms, groups, interactions.clone(), collapsed, key.clone())}
            <div class="agent-sidebar-workbenches">{workbenches}</div>
        }.into_any()
    }
}

#[component]
fn ProjectNewAgent(
    destination: LaunchDestination,
    name: String,
    launch: Callback<LaunchDestination>,
) -> impl IntoView {
    view! {
        <button type="button" class="project-new-agent" title=name.clone() aria-label=name
            on:click=move |_| launch.run(destination.clone())>"+"</button>
    }
}

fn render_agent_tree_group(
    state: AppState,
    group: AgentTreeGroup,
    interactions: AgentsPanelInteractions,
) -> AnyView {
    #[cfg(all(test, target_arch = "wasm32"))]
    wasm_tests::trace_sidebar_stack("tree group", std::ptr::addr_of!(state) as usize);
    let parent = group.parent;
    let children = group.children;
    let parent_ref = agent_ref(&parent);
    let group_id = parent.agent_id.0.clone();
    let expanded_parents = interactions.expanded_parents;
    let inactive_parents = interactions.inactive_parents;
    let children_agents = children
        .iter()
        .map(|child| child.parent.clone())
        .collect::<Vec<_>>();
    let running = {
        let state = state.clone();
        Memo::new(move |_| {
            children_agents
                .iter()
                .filter(|agent| child_is_running(&state, agent))
                .count()
        })
    };
    let child_count = children.len();
    let parent_ref_host = parent.host_id.clone();
    let parent_view = agent_card(
        state.clone(),
        parent,
        child_count,
        running,
        interactions.clone(),
    );
    let related = {
        fn refs(tree: &AgentTreeGroup, items: &mut Vec<SidebarAgentRef>) {
            items.push(agent_ref(&tree.parent));
            for child in &tree.children {
                refs(child, items);
            }
        }
        let mut items = vec![parent_ref.clone()];
        for child in &children {
            refs(child, &mut items);
        }
        let active = state.active_agent;
        Memo::new(move |_| {
            active.get().is_some_and(|selected| {
                items.iter().any(|item| {
                    item.host_id == selected.host_id && item.agent_id == selected.agent_id
                })
            })
        })
    };
    let children_view = children
        .into_iter()
        .map(|child| {
            let parent = parent_ref.clone();
            let child_agent = child.parent.clone();
            let child_state = state.clone();
            let visible = move || {
                expanded_parents.with(|set| set.contains(&parent))
                    && (child_is_running(&child_state, &child_agent)
                        || inactive_parents.with(|set| set.contains(&parent)))
            };
            view! {
                <div class="agent-card-child" class:agent-card-child-hidden=move || !visible()>
                    {render_agent_tree_group(state.clone(), child, interactions.clone())}
                </div>
            }
        })
        .collect_view();
    let parent_expanded = parent_ref.clone();
    let toggle_parent = parent_ref.clone();
    let inactive_open = parent_ref;
    let has_inactive = Memo::new(move |_| {
        child_count > running.get() && expanded_parents.with(|set| set.contains(&parent_expanded))
    });
    let inactive_expanded =
        Memo::new(move |_| inactive_parents.with(|set| set.contains(&inactive_open)));
    let inactive_button = (child_count > 0).then(|| view! {
        <button class="agent-show-inactive" style=move || if has_inactive.get() { "" } else { "display:none" } aria-expanded=move || inactive_expanded.get().to_string()
            on:click=move |_| inactive_parents.update(|set| { if !set.remove(&toggle_parent) { set.insert(toggle_parent.clone()); } })>
            {move || format!("{} {} inactive", if inactive_expanded.get() { "Hide" } else { "Show" }, child_count - running.get())}
        </button>
    });
    view! {
        <div class="agent-card-group" data-host-id=parent_ref_host data-agent-id=group_id data-related=move || related.get().to_string()>
            {parent_view}
            {children_view}
            {inactive_button}
        </div>
    }.into_any()
}

/// Switch the active project (and host) to the project the clicked agent
/// belongs to, then open (and activate) its chat tab. Agent-control children
/// created before project inheritance was added can have no project of their
/// own; for those records the parent agent's project is authoritative. A
/// genuinely unscoped agent opens in Home. Ordering is load-bearing:
/// `switch_active_project` replaces `center_zone`, so it MUST run before
/// `open_tab` or the new tab lands in the old project's zone and is discarded.
/// Switching to the already-active project (including Home) is a no-op.
///
/// Shared with the tool cards' "Open agent" action so every surface that opens
/// an agent chat uses the same project-switch-then-open ordering.
pub(crate) fn open_agent_chat(state: &AppState, agent: &AgentInfo) {
    let project = agent_project_ref(agent).or_else(|| parent_agent_project_ref(state, agent));
    log::info!(
        "open agent navigation agent={} host={} explicit_project={:?} resolved_project={:?} active_project_before={:?}",
        agent.agent_id.0,
        agent.host_id,
        agent.project_id,
        project,
        state.active_project.get_untracked(),
    );
    state.switch_active_project(project);
    let opened = state.open_tab(
        TabContent::chat_with_agent(agent_chat_ref(agent)),
        agent.name.clone(),
        true,
    );
    log::info!(
        "open agent navigation completed agent={} opened_tab={opened:?} active_project_after={:?} active_agent_after={:?}",
        agent.agent_id.0,
        state.active_project.get_untracked(),
        state.active_agent.get_untracked(),
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

fn parent_agent_project_ref(state: &AppState, agent: &AgentInfo) -> Option<ActiveProjectRef> {
    let parent_id = agent.parent_agent_id.as_ref()?;
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|candidate| {
                candidate.host_id == agent.host_id && candidate.agent_id == *parent_id
            })
            .and_then(agent_project_ref)
    })
}

// Erase the large view before recursive groups retain it on the Wasm stack.
fn child_is_running(state: &AppState, agent: &AgentInfo) -> bool {
    // Count the server's current turn, never transcript or tool history.
    state
        .agent_turn_active
        .with(|turns| turns.get(&agent.agent_id).copied().unwrap_or(false))
}

fn agent_card(
    state: AppState,
    agent: AgentInfo,
    child_count: usize,
    running: Memo<usize>,
    interactions: AgentsPanelInteractions,
) -> AnyView {
    #[cfg(all(test, target_arch = "wasm32"))]
    wasm_tests::trace_sidebar_stack("agent card", std::ptr::addr_of!(state) as usize);
    let moving_project = RwSignal::new(false);
    let move_project_agent = agent.clone();
    let move_project_state = state.clone();
    let editing_agent = interactions.editing_agent;
    let edit_value = interactions.edit_value;
    let expanded_parents = interactions.expanded_parents;
    let pointer_drag = interactions.pointer_drag;
    let suppress_pointer_click = interactions.suppress_pointer_click;
    let keyboard_agent = interactions.keyboard_agent;
    let keyboard_target = interactions.keyboard_target;
    let pending_rename_group_name = interactions.pending_rename_group_name;
    let group_live_status = interactions.group_live_status;
    let pin_state = state.clone();
    let pin_ref = agent_ref(&agent);
    let is_pinned = Memo::new(move |_| {
        pin_state.agents.with(|agents| {
            agents
                .iter()
                .find(|agent| agent_ref(agent) == pin_ref)
                .is_some_and(|agent| {
                    pin_state.agents_view_preferences.with(|snapshot| {
                        snapshot
                            .pins
                            .pinned
                            .contains(&agent_annotation_target(agent))
                    })
                })
        })
    });
    let pin_host_id = agent.host_id.clone();
    let pin_host_state = state.clone();
    let pin_host_label = Memo::new(move |_| {
        pin_host_state.configured_hosts.with(|hosts| {
            hosts
                .iter()
                .find(|host| host.id == pin_host_id)
                .map(|host| host.label.clone())
        })
    });
    let pin_settings_open = state.settings_open;
    let pin_settings_request = state.settings_tab_request;
    let pin_click_state = state.clone();
    let pin_click_ref = agent_ref(&agent);
    let toggle_pin = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let Some(target) = target_for_ref(&pin_click_state, &pin_click_ref) else {
            return;
        };
        let update = if is_pinned.get_untracked() {
            AgentPinsUpdate::Unpin { target }
        } else {
            AgentPinsUpdate::Pin { target }
        };
        send_pins_update(&pin_click_state, update);
    };
    let agent_id = agent.agent_id.clone();
    let name = agent.name.clone();
    let backend = agent.backend_kind;

    let error_msg = agent.fatal_error.clone();

    let click_state = state.clone();
    let click_agent = agent.clone();
    let on_click = move |ev: web_sys::MouseEvent| {
        if ev.detail() > 0 && suppress_pointer_click.get_untracked() {
            suppress_pointer_click.set(false);
            return;
        }
        // Switch to the agent's own project/host first, then open (and
        // activate) its chat tab; `active_agent` follows from the Memo over
        // `center_zone`. See `open_agent_chat` for the load-bearing ordering.
        open_agent_chat(&click_state, &click_agent);
    };

    let drag_ref = agent_ref(&agent);
    let pointer_ref = drag_ref.clone();
    let on_pointerdown = move |ev: web_sys::PointerEvent| {
        suppress_pointer_click.set(false);
        if ev.button() != 0 || !ev.is_primary() || ev.pointer_type() == "touch" {
            return;
        }
        let Some(target) = ev
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        if target
            .closest("button, input, textarea, select, a, summary")
            .ok()
            .flatten()
            .is_some()
            || target.closest(".agent-card-top").ok().flatten().is_none()
        {
            return;
        }
        pointer_drag.set(Some(AgentPointerDrag {
            agent: pointer_ref.clone(),
            pointer_id: ev.pointer_id(),
            start: (ev.client_x(), ev.client_y()),
            position: (ev.client_x(), ev.client_y()),
            active: false,
            target: None,
        }));
    };

    // The row is its own keyboard drop target, the same way it is the pointer
    // drop target: once an agent is picked up (from the actions menu), focus
    // walks the rows, groups, and Ungroup, and Space or Enter drops onto the
    // focused one. Otherwise Space and Enter open the chat.
    let kd_state = state.clone();
    let kd_agent = agent.clone();
    let kd_ref = drag_ref.clone();
    let on_keydown_card = move |ev: web_sys::KeyboardEvent| {
        let Some(picked) = keyboard_agent.get_untracked() else {
            if matches!(ev.key().as_str(), "Enter" | " ") {
                ev.prevent_default();
                // Identical behavior to the mouse handler so the two can't drift.
                open_agent_chat(&kd_state, &kd_agent);
            }
            return;
        };
        match ev.key().as_str() {
            " " | "Enter" => {
                ev.prevent_default();
                ev.stop_propagation();
                if picked == kd_ref {
                    group_live_status.set("Move cancelled".to_owned());
                } else {
                    drop_agent_on_agent(
                        &kd_state,
                        Some(picked),
                        kd_agent.clone(),
                        pending_rename_group_name,
                        group_live_status,
                    );
                }
                keyboard_agent.set(None);
                keyboard_target.set(None);
            }
            "Escape" => {
                ev.stop_propagation();
                keyboard_agent.set(None);
                keyboard_target.set(None);
                group_live_status.set("Move cancelled".to_owned());
            }
            "ArrowDown" | "ArrowRight" => {
                ev.prevent_default();
                ev.stop_propagation();
                if let Some(row) = ev.current_target() {
                    focus_relative_drop_target(&row, 1);
                }
            }
            "ArrowUp" | "ArrowLeft" => {
                ev.prevent_default();
                ev.stop_propagation();
                if let Some(row) = ev.current_target() {
                    focus_relative_drop_target(&row, -1);
                }
            }
            _ => {}
        }
    };
    let focus_drop_ref = drag_ref.clone();
    let on_card_focus = move |_: web_sys::FocusEvent| {
        if keyboard_agent.get_untracked().is_some() {
            keyboard_target.set(Some(SidebarDropTarget::Agent(focus_drop_ref.clone())));
        }
    };
    let class_drag_ref = drag_ref.clone();
    let aria_drag_ref = drag_ref.clone();

    let input_ref = NodeRef::<leptos::html::Input>::new();

    let edit_ref_for_effect = agent_ref(&agent);
    // Auto-focus and select-all when editing mode activates.
    Effect::new(move |_| {
        if editing_agent.with(|e| e.as_ref() == Some(&edit_ref_for_effect))
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
            el.select();
        }
    });

    let rename_name = name.clone();
    let edit_ref_for_rename = agent_ref(&agent);
    let on_rename = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        edit_value.set(rename_name.clone());
        editing_agent.set(Some(edit_ref_for_rename.clone()));
    };

    let host_id_for_edit = agent.host_id.clone();
    let stream_for_edit = agent.instance_stream.clone();

    let is_tychat = agent.origin == AgentOrigin::Tychat;
    let close_host_id = agent.host_id.clone();
    let close_stream = agent.instance_stream.clone();
    let close_name = name.clone();
    let close_agent_id = agent_id.clone();
    let close_state = state.clone();
    let on_close = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        if is_tychat {
            return;
        }
        let is_active = close_state
            .active_agent
            .with_untracked(|a| a.as_ref().is_some_and(|a| a.agent_id == close_agent_id));
        let has_draft = is_active
            && !close_state
                .composer_untracked()
                .text
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

    // A Memo rather than a plain closure: every status closure below would
    // otherwise carry its own copy of `AppState`, and that per-card weight is
    // enough to overflow the wasm stack while the sidebar view is built.
    let agent_for_derived = agent.clone();
    let derived = {
        let state = state.clone();
        Memo::new(move |_| derive_agent_state_with_background(&state, &agent_for_derived))
    };

    let status_class_sig = move || status_class(&derived.get());
    let status_icon_sig = move || status_icon(&derived.get());
    let status_title_sig = move || status_label(&derived.get());
    let status_label_sig = move || status_label(&derived.get());
    let status_label_visible_sig = move || status_label_visible(&derived.get());

    // Compact action.
    //
    // Visible-but-disabled rather than hidden. The most common blocker used to
    // be "the agent is not idle", which is transient and self-resolving — a
    // control that silently vanishes teaches the user it does not exist. It is
    // now not a blocker at all: the server accepts a request during a turn and
    // reports it deferred, so the client no longer duplicates that decision.
    //
    // The old `chat_rows` non-empty gate is gone too: it was wrong, not just
    // conservative. A restored session showing only the collapsed-history
    // banner has zero loaded rows, so the button hid on exactly the long
    // sessions that most need compacting.
    let compaction_control = {
        let host_id = agent.host_id.clone();
        let agent_id = agent_id.clone();
        let state = state.clone();
        move || {
            crate::actions::compaction_control_state(
                &state,
                &ActiveAgentRef {
                    host_id: host_id.clone(),
                    agent_id: agent_id.clone(),
                },
            )
        }
    };
    let compact_host_id = agent.host_id.clone();
    let compact_agent_id = agent_id.clone();
    let compact_name = name.clone();
    let compact_state = state.clone();
    let on_compact = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let state = compact_state.clone();
        let agent = ActiveAgentRef {
            host_id: compact_host_id.clone(),
            agent_id: compact_agent_id.clone(),
        };
        let name = compact_name.clone();
        spawn_local(async move {
            crate::actions::request_context_compaction(state, agent, name).await;
        });
    };

    let compaction_error_msg = {
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || state.compaction_errors.with(|m| m.get(&agent_id).cloned())
    };

    let edit_ref_for_block = agent_ref(&agent);
    let card_anchor = NodeRef::<leptos::html::Div>::new();
    // The row is one line. Its actions live in one menu, reached by right-click
    // or by the row's `⋯` button; nothing opens on hover or inside the row.
    let card_menu: CardMenuPosition = RwSignal::new(None);
    let menu_rename = on_rename;
    let menu_close = on_close;
    let menu_toggle_pin = toggle_pin.clone();
    let menu_move_ref = drag_ref.clone();
    let menu_move_name = name.clone();
    let on_menu_move = move |_: web_sys::MouseEvent| {
        if keyboard_agent.get_untracked().as_ref() == Some(&menu_move_ref) {
            keyboard_agent.set(None);
            keyboard_target.set(None);
            group_live_status.set("Move cancelled".to_owned());
        } else {
            keyboard_agent.set(Some(menu_move_ref.clone()));
            keyboard_target.set(Some(SidebarDropTarget::Agent(menu_move_ref.clone())));
            group_live_status.set(format!(
                "Picked up {}. Move focus to a group, agent, or Ungroup and press Space or Enter.",
                menu_move_name
            ));
            // The menu item that had focus is gone; the keyboard continues
            // from the row it picked up.
            if let Some(card) = card_anchor.get_untracked() {
                let _ = card.focus();
            }
        }
    };
    let agent_ref_for_disclosure = agent_ref(&agent);
    let selected_agent = agent_chat_ref(&agent);
    let selected = {
        let active = state.active_agent;
        Memo::new(move |_| active.get().as_ref() == Some(&selected_agent))
    };

    view! {
        <div
            class=move || {
                if keyboard_target.get().as_ref() == Some(&SidebarDropTarget::Agent(class_drag_ref.clone())) {
                    "agent-card agent-group-keyboard-focus"
                } else {
                    "agent-card"
                }
            }
            node_ref=card_anchor
            data-sidebar-backend=backend_label(backend)
            aria-current=move || if selected.get() { "true" } else { "false" }
            tabindex="0"
            role="button"
            draggable="false"
            data-agent-drop-kind="agent"
            data-agent-drop-host=agent.host_id.clone()
            data-agent-drop-id=agent.agent_id.0.clone()
            data-agent-group-keyboard-target="true"
            aria-dropeffect=move || if keyboard_agent.get().is_some() { "move" } else { "none" }
            aria-grabbed=move || {
                keyboard_agent
                    .get()
                    .as_ref()
                    .is_some_and(|picked| picked == &aria_drag_ref)
                    .to_string()
            }
            on:click=on_click
            on:contextmenu=move |ev: web_sys::MouseEvent| open_card_menu(card_menu, &ev)
            on:keydown=on_keydown_card
            on:focus=on_card_focus
            on:pointerdown=on_pointerdown
            on:dragstart=|ev: web_sys::DragEvent| { ev.prevent_default(); ev.stop_propagation(); }
        >
            <div class="agent-card-top">
                <span
                    class=move || {
                        if status_label_visible_sig() {
                            format!("{} agent-card-status-labelled", status_class_sig())
                        } else {
                            status_class_sig().to_string()
                        }
                    }
                    title=status_title_sig
                >
                    <span aria-hidden="true">{status_icon_sig}</span>
                    <span class=move || {
                        if status_label_visible_sig() {
                            "agent-card-status-text"
                        } else {
                            "visually-hidden"
                        }
                    }>{status_label_sig}</span>
                </span>
                <div class="agent-card-top-main">
                {move || {
                    if editing_agent.with(|e| e.as_ref() == Some(&edit_ref_for_block)) {
                        let host_id = host_id_for_edit.clone();
                        let stream = stream_for_edit.clone();
                        let compare = name.clone();
                        let edit_ref_for_blur = edit_ref_for_block.clone();
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
                                if editing_agent.with_untracked(|e| e.as_ref() != Some(&edit_ref_for_blur)) {
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
                            <span class="agent-card-name">{name.clone()}{is_tychat.then_some(" · Tychat")}</span>
                        }.into_any()
                    }
                }}
                {(child_count > 0).then(|| {
                    let parent_ref = agent_ref(&agent);
                    let expanded_ref = parent_ref.clone();
                    view! {
                        <button type="button" class="agent-card-collapse-toggle"
                            title="Toggle sub-agents" aria-label="Toggle sub-agents"
                            aria-expanded=move || expanded_parents.with(|set| set.contains(&expanded_ref)).to_string()
                            on:click=move |ev: web_sys::MouseEvent| {
                                ev.stop_propagation();
                                expanded_parents.update(|set| { if !set.remove(&parent_ref) { set.insert(parent_ref.clone()); } });
                            }
                            on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                        >
                            {move || format!("{} running", running.get())}
                            <span aria-hidden="true">{move || if expanded_parents.with(|set| set.contains(&agent_ref_for_disclosure)) { " ▾" } else { " ▸" }}</span>
                        </button>
                    }
                })}
                </div>
                <Show when=move || is_pinned.get()>
                    {move || match pin_host_label.get() {
                        Some(label) => {
                            let title = label.clone();
                            view! { <span class="agent-pin-host" title=title>{label}</span> }.into_any()
                        },
                        None => {
                            view! {
                                <button class="agent-pin-host agent-pin-host-error"
                                    title="Host label unavailable. Open Settings → Hosts to check host configuration."
                                    aria-label="Host label unavailable. Open host settings"
                                    on:click=move |ev: web_sys::MouseEvent| {
                                        ev.stop_propagation();
                                        pin_settings_request.set(Some("Hosts"));
                                        pin_settings_open.set(true);
                                    }
                                    on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                                >"Host label missing"</button>
                            }.into_any()
                        }
                    }}
                </Show>
                <button
                    type="button"
                    class="agent-card-pin"
                    class:pinned=move || is_pinned.get()
                    aria-pressed=move || is_pinned.get().to_string()
                    aria-label=move || if is_pinned.get() { "Unpin chat" } else { "Pin chat" }
                    title=move || if is_pinned.get() { "Unpin chat" } else { "Pin chat" }
                    on:click=toggle_pin
                    on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                    on:dragstart=|ev: web_sys::DragEvent| { ev.prevent_default(); ev.stop_propagation(); }
                >
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                        <path d="m16 3 5 5-4 1-3 5v3l-3-3-6 6-1-1 6-6-3-3h3l5-3z" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
                    </svg>
                </button>
                <span class="agent-backend-glyph" role="img" aria-label=backend_label(backend) title=backend_label(backend)>
                    {crate::components::launch_menu::backend_icon(backend)}
                </span>
                <button
                    type="button"
                    class="agent-card-actions"
                    aria-haspopup="menu"
                    aria-expanded=move || card_menu.get().is_some().to_string()
                    title="Agent actions"
                    aria-label="Agent actions"
                    on:click=move |ev: web_sys::MouseEvent| open_card_menu_below(card_menu, &ev)
                    on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                >
                    "⋯"
                </button>

            </div>
            {error_msg.map(|msg| view! {
                <div class="agent-card-error">{msg}</div>
            })}
            {move || compaction_error_msg().map(|msg| view! {
                <div class="agent-card-error agent-card-error-compaction">{msg}</div>
            })}
            <Show when=move || moving_project.get()>
                <MoveAgentProjectDialog state=move_project_state.clone() agent=move_project_agent.clone() open=moving_project />
            </Show>
            <CardContextMenu menu=card_menu label="Agent actions" anchor=card_anchor>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    on:click={
                        let rename = menu_rename.clone();
                        move |ev: web_sys::MouseEvent| {
                            card_menu.set(None);
                            rename(ev);
                        }
                    }
                >
                    "Rename agent"
                </button>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    on:click={
                        let toggle = menu_toggle_pin.clone();
                        move |ev: web_sys::MouseEvent| {
                            card_menu.set(None);
                            toggle(ev);
                        }
                    }
                >
                    {move || if is_pinned.get() { "Unpin chat" } else { "Pin chat" }}
                </button>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    on:click={
                        let pick_up = on_menu_move.clone();
                        move |ev: web_sys::MouseEvent| {
                            card_menu.set(None);
                            pick_up(ev);
                        }
                    }
                >
                    "Move to group"
                </button>
                <button type="button" class="context-menu-item" role="menuitem"
                    on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); card_menu.set(None); moving_project.set(true); }>
                    "Move to project…"
                </button>
                {
                    let control = compaction_control.clone();
                    let compact = on_compact.clone();
                    move || {
                    let control = control();
                    let enabled = control.is_enabled();
                    // Unavailable stays visible and says why. `aria-disabled`
                    // rather than `disabled`, so the reason is reachable by
                    // keyboard and screen reader, not only by hover; the action
                    // re-checks the gate, so it is inert either way.
                    let label = match control.reason() {
                        None => "Compact context".to_owned(),
                        Some(reason) => format!("Compact context — unavailable: {reason}"),
                    };
                    let compact = compact.clone();
                    let title = label.clone();
                    view! {
                        <button
                            type="button"
                            class="context-menu-item"
                            role="menuitem"
                            data-test="agent-menu-compact"
                            aria-disabled=move || if enabled { "false" } else { "true" }
                            title=title
                            on:click=move |ev: web_sys::MouseEvent| {
                                card_menu.set(None);
                                compact(ev);
                            }
                        >
                            {label}
                        </button>
                    }
                    }
                }
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    disabled=is_tychat
                    on:click={
                        let close = menu_close.clone();
                        move |ev: web_sys::MouseEvent| {
                            card_menu.set(None);
                            close(ev);
                        }
                    }
                >
                    "Close agent"
                </button>
            </CardContextMenu>
        </div>
    }
    .into_any()
}

#[component]
fn MoveAgentProjectDialog(
    state: AppState,
    agent: AgentInfo,
    open: RwSignal<bool>,
) -> impl IntoView {
    let search = RwSignal::new(String::new());
    let selected = RwSignal::new(None::<Option<ProjectId>>);
    let pending = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let input = NodeRef::<leptos::html::Input>::new();
    let previous_focus = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.active_element());
    on_cleanup(move || {
        use wasm_bindgen::JsCast;
        if let Some(element) =
            previous_focus.and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
            && element.is_connected()
        {
            let _ = element.focus();
        }
    });
    Effect::new(move |_| {
        if let Some(input) = input.get() {
            let _ = input.focus();
        }
    });
    let host = agent.host_id.clone();
    let current = agent.project_id.clone();
    let can_unassign = current.is_some();
    let projects = state.projects;
    let destinations = Memo::new(move |_| {
        let query = search.get().to_lowercase();
        let mut choices = projects
            .get()
            .into_iter()
            .filter(|item| {
                item.host_id == host
                    && Some(&item.project.id) != current.as_ref()
                    && (item.project.name.to_lowercase().contains(&query)
                        || item
                            .project
                            .root_paths()
                            .iter()
                            .any(|root| root.0.to_lowercase().contains(&query)))
            })
            .collect::<Vec<_>>();
        choices.sort_by_key(|item| item.project.name.to_lowercase());
        choices
    });
    let busy_agent = agent.agent_id.clone();
    let active_turns = state.agent_turn_active;
    let unavailable = Memo::new(move |_| {
        if agent.parent_agent_id.is_some()
            || agent.team_member_id.is_some()
            || agent.workflow.is_some()
        {
            Some("Team, workflow, and child agents belong to their existing project.")
        } else if !agent.started
            || active_turns
                .get()
                .get(&busy_agent)
                .copied()
                .unwrap_or(false)
        {
            Some("Wait until the agent finishes its turn before moving it.")
        } else {
            None
        }
    });
    let connections = state.connection_statuses;
    let connection_host = agent.host_id.clone();
    Effect::new(move |_| {
        if pending.get().is_some()
            && !matches!(
                connections.get().get(&connection_host),
                Some(crate::state::ConnectionStatus::Connected)
            )
        {
            pending.set(None);
            error.set(Some("Connection lost while moving. Reconnect to check the agent's project before trying again.".to_owned()));
        }
    });
    let result = state.agent_move_result;
    let result_host = agent.host_id.clone();
    Effect::new(move |_| {
        let Some(request) = pending.get() else { return };
        if let Some((host, response)) = result.get()
            && host == result_host
            && response.request_id == request
        {
            pending.set(None);
            match response.result {
                Ok(_) => open.set(false),
                Err(message) => error.set(Some(message)),
            }
        }
    });
    let send_host = agent.host_id.clone();
    let send_agent = agent.agent_id.clone();
    let streams = state.host_streams;
    let send = move |_| {
        if pending.get_untracked().is_some() || unavailable.get_untracked().is_some() {
            return;
        }
        let Some(project_id) = selected.get_untracked() else {
            return;
        };
        let Some(stream) = streams.get_untracked().get(&send_host).cloned() else {
            error.set(Some(
                "The host is disconnected. Reconnect and try again.".to_owned(),
            ));
            return;
        };
        let request_id = crate::state::new_history_request_id();
        pending.set(Some(request_id.clone()));
        error.set(None);
        let host = send_host.clone();
        let payload = protocol::types::AgentMovePayload {
            request_id,
            agent_id: send_agent.clone(),
            project_id,
        };
        leptos::task::spawn_local(async move {
            if let Err(message) =
                crate::send::send_frame(&host, stream, FrameKind::AgentMove, &payload).await
            {
                pending.set(None);
                error.set(Some(message));
            }
        });
    };
    view! {
        <div class="modal-backdrop agent-move-backdrop"
            on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); if pending.get_untracked().is_none() { open.set(false); } } />
        <div class="modal workbench-create-modal agent-move-modal" role="dialog" aria-modal="true" aria-label="Move agent to project"
            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                use wasm_bindgen::JsCast;
                ev.stop_propagation();
                if ev.key() == "Escape" && pending.get_untracked().is_none() { open.set(false); }
                if ev.key() == "Tab" && let Some(dialog) = ev.current_target().and_then(|target| target.dyn_into::<web_sys::Element>().ok()) && let Ok(elements) = dialog.query_selector_all("button:not(:disabled), input:not(:disabled)") {
                    if elements.length() == 0 { ev.prevent_default(); return; }
                    let first = elements.item(0).unwrap().dyn_into::<web_sys::HtmlElement>().unwrap();
                    let last = elements.item(elements.length() - 1).unwrap().dyn_into::<web_sys::HtmlElement>().unwrap();
                    let active = web_sys::window().and_then(|window| window.document()).and_then(|document| document.active_element());
                    if ev.shift_key() && active.as_ref() == Some(first.as_ref()) { ev.prevent_default(); let _ = last.focus(); }
                    else if !ev.shift_key() && active.as_ref() == Some(last.as_ref()) { ev.prevent_default(); let _ = first.focus(); }
                }
            }>
            <header class="agent-move-header">
                <div class="modal-title">"Move to project"</div>
                <p class="agent-move-description" title=agent.name.clone()>"Choose a new home for “"{agent.name.clone()}"”."</p>
                <label class="agent-move-search">
                    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" aria-hidden="true">
                        <circle cx="10.5" cy="10.5" r="6.5" /><path d="m16 16 4 4" />
                    </svg>
                    <input type="search" aria-label="Search projects" placeholder="Search projects or folders…" node_ref=input
                        disabled=move || pending.get().is_some() prop:value=move || search.get() on:input=move |ev| search.set(event_target_value(&ev)) />
                </label>
            </header>
            <div class="agent-move-destinations" role="group" aria-label="Destination project">
                <Show when=move || can_unassign>
                    <button type="button" class="agent-move-choice agent-move-unassigned" aria-pressed=move || (selected.get() == Some(None)).to_string()
                        disabled=move || pending.get().is_some() on:click=move |_| { selected.set(Some(None)); error.set(None); }>
                        <span class="agent-move-icon">
                            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                <rect x="4" y="4" width="16" height="16" rx="4" /><path d="M8 12h8" />
                            </svg>
                        </span>
                        <span class="agent-move-copy"><strong>"No project"</strong><span>"Keep current folders, without a project"</span></span>
                        <span class="agent-move-check" aria-hidden="true">
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 4 4 10-10" /></svg>
                        </span>
                    </button>
                </Show>
                <div class="agent-move-section-label">"Projects"</div>
                {move || destinations.get().into_iter().map(|item| {
                    let project = item.project;
                    let id = project.id.clone();
                    let selected_id = id.clone();
                    let roots = project.root_paths();
                    let full_paths = roots.iter().map(|root| root.0.as_str()).collect::<Vec<_>>().join("\n");
                    let extra_roots = roots.len().saturating_sub(1);
                    let branch = match project.source {
                        protocol::ProjectSource::GitWorkbench { branch, .. } => Some(branch.0),
                        protocol::ProjectSource::Standalone { .. } => None,
                    };
                    view! {
                        <button type="button" class="agent-move-choice" title=full_paths aria-pressed=move || (selected.get().as_ref() == Some(&Some(selected_id.clone()))).to_string()
                            disabled=move || pending.get().is_some() on:click=move |_| { selected.set(Some(Some(id.clone()))); error.set(None); }>
                            <span class="agent-move-icon">
                                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                    <path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
                                </svg>
                            </span>
                            <span class="agent-move-copy">
                                <span class="agent-move-name"><strong title=project.name.clone()>{project.name.clone()}</strong>{branch.map(|branch| view! { <span class="agent-move-badge" title=branch>"Workbench"</span> })}</span>
                                <span class="agent-move-folders">
                                    {roots.first().map(|root| view! { <span class="agent-move-path">{root.0.clone()}</span> })}
                                    {(extra_roots > 0).then(|| view! { <span class="agent-move-count">{format!("+{extra_roots} {}", if extra_roots == 1 { "folder" } else { "folders" })}</span> })}
                                </span>
                            </span>
                            <span class="agent-move-check" aria-hidden="true">
                                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 4 4 10-10" /></svg>
                            </span>
                        </button>
                    }
                }).collect_view()}
                <Show when=move || destinations.get().is_empty()><p class="agent-move-empty">"No matching projects on this host."</p></Show>
            </div>
            <footer class="agent-move-footer">
                <p class="agent-move-note">"Instructions and tool settings stay the same."</p>
                {move || unavailable.get().map(|message| view! { <p class="agent-move-note" role="status">{message}</p> })}
                {move || error.get().map(|message| view! { <p class="modal-error" role="alert">{message}</p> })}
                <div class="modal-actions">
                    <button type="button" class="modal-button" disabled=move || pending.get().is_some() on:click=move |_| open.set(false)>"Cancel"</button>
                    <button type="button" class="modal-button primary" disabled=move || pending.get().is_some() || unavailable.get().is_some() || selected.get().is_none() on:click=send>
                        {move || if pending.get().is_some() { "Moving…" } else { "Move agent" }}
                    </button>
                </div>
            </footer>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::center_zone::CenterWorkspaceWidth;
    use crate::dispatch::dispatch_envelope;
    use crate::state::{ChatMessageEntry, ChatRowHandle, TabId};
    use leptos::mount::mount_to;
    use protocol::types::{
        AgentCompactNotifyPayload, AgentCompactStatus, TeamCompactNotifyPayload, TeamCompactStatus,
    };
    use protocol::{
        AgentAnnotationTarget, AgentGroup, AgentGroupAssignment, AgentGroupId, AgentGroupsSnapshot,
        AgentOrigin, AgentsViewPreferences, AgentsViewPreferencesSnapshot, BackendKind,
        ChatMessage, Envelope, HostFilterId, MessageSender, NewAgentPayload, Project, ProjectId,
        ProjectRootPath, ProjectSource, StreamPath, TeamId, TeamMemberId,
    };
    use serde_json::Value as JsonValue;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen::prelude::wasm_bindgen(
        inline_js = "export async function settle_sidebar_styles(element) { getComputedStyle(element).backgroundColor; await Promise.all(element.getAnimations().map(animation => animation.finished)); }"
    )]
    extern "C" {
        #[wasm_bindgen::prelude::wasm_bindgen(catch)]
        async fn settle_sidebar_styles(
            element: &web_sys::Element,
        ) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
    }

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
    /// Like `install_send_stub_with_dialog_ok`, but the confirmation promise is
    /// held open until the test releases it, so state can be mutated strictly
    /// between the pre-dialog eligibility check and the claim.
    fn install_send_stub_with_held_dialog() -> js_sys::Array {
        let calls = js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__test_release_dialog = null;
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    if (cmd === 'plugin:dialog|message') {
                        return new Promise(function (resolve) {
                            window.__test_release_dialog = function () { resolve('Ok'); };
                        });
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

    fn release_held_dialog() {
        let _ = js_sys::eval("window.__test_release_dialog && window.__test_release_dialog();");
    }

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
                        if (window.__test_dialog_error) {
                            return Promise.reject('Native confirmation unavailable');
                        }
                        return Promise.resolve(window.__test_dialog_result || 'Ok');
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

    fn pointer_event(element: &web_sys::Element, name: &str, x: i32, y: i32) {
        let init = web_sys::PointerEventInit::new();
        init.set_bubbles(true);
        init.set_cancelable(true);
        init.set_pointer_id(1);
        init.set_pointer_type("mouse");
        init.set_is_primary(true);
        init.set_button(0);
        init.set_buttons(if name == "pointerup" { 0 } else { 1 });
        init.set_client_x(x);
        init.set_client_y(y);
        element
            .dispatch_event(&web_sys::PointerEvent::new_with_event_init_dict(name, &init).unwrap())
            .unwrap();
    }

    fn begin_agent_drag(card: &HtmlElement) {
        let name = card.query_selector(".agent-card-name").unwrap().unwrap();
        let rect = name.get_bounding_client_rect();
        let x = rect.left() as i32 + 2;
        let y = (rect.top() + rect.height() / 2.0) as i32;
        pointer_event(&name, "pointerdown", x, y);
        pointer_event(&name, "pointermove", x + 8, y);
    }

    fn finish_agent_drag(target: &HtmlElement) {
        let rect = target.get_bounding_client_rect();
        pointer_event(
            target,
            "pointerup",
            (rect.left() + rect.width() / 2.0) as i32,
            (rect.top() + rect.height() / 2.0) as i32,
        );
    }

    fn cancel_agent_drag(card: &HtmlElement) {
        pointer_event(card, "pointercancel", 0, 0);
    }

    /// A real right-click, at a real point in the viewport.
    fn dispatch_context_menu(element: &HtmlElement, x: f64, y: f64) {
        let event: web_sys::Event = js_sys::eval(&format!(
            "new MouseEvent('contextmenu', {{ bubbles: true, cancelable: true, \
             clientX: {x}, clientY: {y} }})"
        ))
        .expect("mouse event")
        .dyn_into()
        .expect("MouseEvent is an Event");
        element
            .dispatch_event(&event)
            .expect("dispatch contextmenu event");
    }

    /// Open a row's actions menu from its visible `⋯` button.
    async fn open_row_menu(row: &web_sys::Element) {
        row.query_selector(".agent-card-actions")
            .unwrap()
            .expect("every agent row offers its actions")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
    }

    /// Pick a row up for a keyboard move, through its actions menu.
    async fn pick_up_for_keyboard_move(container: &HtmlElement, agent_id: &str) {
        open_row_menu(&agent_card_el(container, agent_id)).await;
        menu_item(container, "Move to group").click();
        for _ in 0..4 {
            next_tick().await;
        }
    }

    /// Every item of the open card menu: its text, `aria-disabled`, and
    /// whether it is natively disabled.
    fn open_menu_items(container: &HtmlElement) -> Vec<(String, Option<String>, bool)> {
        let items = container
            .query_selector_all(".sidebar-card-menu [role='menuitem']")
            .unwrap();
        (0..items.length())
            .map(|index| {
                let item: web_sys::HtmlButtonElement =
                    items.item(index).unwrap().dyn_into().unwrap();
                (
                    item.text_content().unwrap_or_default(),
                    item.get_attribute("aria-disabled"),
                    item.disabled(),
                )
            })
            .collect()
    }

    /// The open card menu's item with this label.
    fn menu_item(container: &HtmlElement, label: &str) -> HtmlElement {
        let items = container
            .query_selector_all(".sidebar-card-menu .context-menu-item")
            .unwrap();
        for index in 0..items.length() {
            let item: HtmlElement = items.item(index).unwrap().dyn_into().unwrap();
            if item.text_content().unwrap_or_default().contains(label) {
                return item;
            }
        }
        panic!("the open menu offers no {label:?} item");
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
                tyde_permissions: Default::default(),
                host_id: host_id.to_owned(),
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: project_id.map(|id| ProjectId(id.to_owned())),
                parent_agent_id: parent_agent_id.map(|id| AgentId(id.to_owned())),
                team_member_id: None,
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
                review_recipient: false,
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
                        origin: None,
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

    /// Declare the capability the server would send. The control fails closed
    /// without one, so any test that expects an *enabled* control must install
    /// it — that is the gate, not an incidental setup step.
    fn declare_compaction_available(state: &AppState, agent_id: &str) {
        state.compaction_capability.update(|map| {
            map.insert(
                AgentId(agent_id.to_owned()),
                crate::state::CompactionCapabilitySnapshot {
                    logical_session_id: protocol::SessionId(String::new()),
                    availability: protocol::RequestedCompactionAvailability::Available {
                        route: protocol::RequestedCompactionRoute::NativeOnly,
                    },
                },
            );
        });
    }

    /// Opens the first row's actions menu and returns its Compact item. The
    /// item is always offered; unavailability is reported through
    /// `aria-disabled` plus a reason in its name.
    async fn compact_btn_state(container: &HtmlElement) -> (HtmlElement, bool, String) {
        let row = container
            .query_selector(".agent-card")
            .unwrap()
            .expect("an agent row is rendered");
        open_row_menu(&row).await;
        let btn: HtmlElement = container
            .query_selector("[data-test='agent-menu-compact']")
            .unwrap()
            .expect("the actions menu always offers Compact context")
            .dyn_into()
            .unwrap();
        let enabled = btn.get_attribute("aria-disabled").as_deref() == Some("false");
        let label = btn.text_content().unwrap_or_default();
        (btn, enabled, label)
    }

    /// Mount `AgentsPanel` and return the handle. Caller MUST bind the
    /// handle to a local (e.g. `_handle`) — dropping it tears down the
    /// Leptos root, which empties the container and makes any DOM probe
    /// trivially fail.
    fn mount_panel(container: &HtmlElement, state: AppState) -> impl Sized {
        // The center-workspace width is a *thread-local global* (center_zone.rs),
        // so it survives across tests on the shared wasm test thread. Each mount
        // declares "nothing has measured the workspace" so it does not inherit a
        // previous test's measurement.
        CenterWorkspaceWidth::forget_measurement();
        let state_for_mount = state;
        mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <AgentsPanel />
            }
        })
    }

    #[wasm_bindgen_test]
    async fn compact_cards_keep_identity_and_offer_persistent_color_only_mode() {
        use crate::components::sessions_panel::SessionsPanel;
        use crate::components::settings_panel::{SettingsPanel, restore_appearance};
        use crate::state::SessionInfo;
        use protocol::{SessionId, SessionSummary};

        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        push_agent(
            &state,
            "local",
            "compact-card",
            "Review authentication flow",
            true,
        );
        push_agent(&state, "local", "neighbour-card", "Neighbour chat", true);
        state.agents.update(|agents| {
            agents
                .iter_mut()
                .find(|agent| agent.agent_id.0 == "compact-card")
                .unwrap()
                .workspace_roots = vec!["/workspace/ui-review".into()]
        });
        state.sessions.set(vec![SessionInfo {
            host_id: "local".to_owned(),
            summary: SessionSummary {
                id: SessionId("saved-card-full-id".to_owned()),
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                workspace_roots: vec!["/workspace/tyde".to_owned()],
                project_id: None,
                alias: Some("Fix history sorting".to_owned()),
                user_alias: None,
                parent_id: None,
                created_at_ms: 0,
                updated_at_ms: 100,
                message_count: 12,
                token_count: None,
                resumable: true,
                compacted_from_session_id: None,
                compacted_to_session_id: None,
                compacted_at_ms: None,
                compaction_summary_preview: None,
            },
        }]);
        CenterWorkspaceWidth::forget_measurement();
        let mounted_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted_state.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <div style="width:320px;height:260px"><AgentsPanel /></div>
                <div style="width:320px;height:260px"><SessionsPanel /></div>
                <SettingsPanel />
            }
        });
        for _ in 0..4 {
            next_tick().await;
        }
        let agent = agent_card_el(&container, "compact-card");
        let neighbour = agent_card_el(&container, "neighbour-card");
        let history: HtmlElement = container
            .query_selector(".session-card")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            agent
                .query_selector("[role='img'][aria-label='Claude'] svg")
                .unwrap()
                .is_some(),
            "Agent identity uses the shared backend glyph"
        );
        assert!(
            agent.query_selector(".backend-badge").unwrap().is_none(),
            "An agent row does not repeat its backend as text"
        );
        // A conversation row is one 20px line plus 4px of padding above and
        // below it: nothing reserves room for a handle or a hidden panel.
        let collapsed_height = agent.get_bounding_client_rect().height();
        assert!(
            (27.0..=29.0).contains(&collapsed_height),
            "an agent row must be one 28px line, got {collapsed_height}px"
        );
        let history_height = history.get_bounding_client_rect().height();
        assert!(
            (30.0..=40.0).contains(&history_height),
            "a saved conversation stays one compact line, got {history_height}px"
        );
        let row_text = agent.text_content().unwrap_or_default();
        assert!(
            !row_text.contains('⠿'),
            "no row carries a permanent drag grip, got {row_text:?}"
        );
        for card in [&history] {
            let label = card.query_selector(".backend-badge").unwrap().unwrap();
            let window = web_sys::window().unwrap();
            let edge = window
                .get_computed_style(card)
                .unwrap()
                .unwrap()
                .get_property_value("border-left-color")
                .unwrap();
            let ink = window
                .get_computed_style(&label)
                .unwrap()
                .unwrap()
                .get_property_value("color")
                .unwrap();
            assert_eq!(
                edge, ink,
                "the backend label must teach the matching edge color"
            );
        }
        // Nothing opens on hover or on focus: a row stays one line.
        agent.focus().unwrap();
        next_tick().await;
        assert_eq!(
            agent.get_bounding_client_rect().height(),
            collapsed_height,
            "focusing a row must not grow it"
        );

        // The row's `⋯` is a menu button for the same menu right-click opens,
        // not a disclosure: nothing expands inside the row.
        let actions: HtmlElement = agent
            .query_selector(".agent-card-actions")
            .unwrap()
            .expect("every agent row offers its actions")
            .dyn_into()
            .unwrap();
        assert_eq!(actions.text_content().as_deref(), Some("⋯"));
        assert_eq!(
            actions.get_attribute("aria-label").as_deref(),
            Some("Agent actions")
        );
        assert_eq!(
            actions.get_attribute("aria-haspopup").as_deref(),
            Some("menu")
        );
        assert_eq!(
            actions.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert!(
            container
                .query_selector(".sidebar-card-menu")
                .unwrap()
                .is_none(),
            "no menu until it is asked for"
        );
        actions.click();
        next_tick().await;
        let menu = container
            .query_selector(".sidebar-card-menu")
            .unwrap()
            .expect("the row's actions button opens its menu");
        assert!(
            agent.contains(Some(&menu)),
            "the menu belongs to the row whose button was pressed"
        );
        assert_eq!(
            actions.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        let menu_rect = menu.get_bounding_client_rect();
        let actions_rect = actions.get_bounding_client_rect();
        assert!(
            (menu_rect.top() - actions_rect.bottom()).abs() <= 1.0
                && menu_rect.left() <= actions_rect.left() + 1.0
                && menu_rect.right() >= actions_rect.right() - 1.0,
            "the menu opens directly under its button"
        );
        assert_eq!(
            agent.get_bounding_client_rect().height(),
            collapsed_height,
            "opening the actions does not expand the row"
        );
        let row_text = agent
            .query_selector(".agent-card-top")
            .unwrap()
            .unwrap()
            .text_content()
            .unwrap_or_default();
        assert!(
            !row_text.contains("ui-review") && !row_text.contains("Claude"),
            "the row shows no inline details, got {row_text:?}"
        );
        assert!(
            agent
                .query_selector(".sidebar-card-details")
                .unwrap()
                .is_none(),
            "an agent row has no inline details panel"
        );
        let from_button = open_menu_items(&container);
        let labels: Vec<&str> = from_button
            .iter()
            .map(|(text, _, _)| text.as_str())
            .collect();
        assert_eq!(
            labels.len(),
            6,
            "the actions menu offers every row action: {labels:?}"
        );
        assert_eq!(
            [labels[0], labels[1], labels[2], labels[3], labels[5]],
            [
                "Rename agent",
                "Pin chat",
                "Move to group",
                "Move to project…",
                "Close agent",
            ],
        );
        assert!(
            labels[4].starts_with("Compact context — unavailable:")
                && from_button[4].1.as_deref() == Some("true"),
            "unavailable compaction stays visible and says why: {:?}",
            from_button[4]
        );
        dispatch_key(&menu.clone().dyn_into::<HtmlElement>().unwrap(), "Escape");
        next_tick().await;
        assert!(
            container
                .query_selector(".sidebar-card-menu")
                .unwrap()
                .is_none(),
            "Escape closes the menu"
        );
        assert_eq!(
            actions.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert_eq!(
            document()
                .active_element()
                .map(|el| el.is_same_node(Some(&agent))),
            Some(true),
            "dismissing the menu returns the keyboard to the row it came from"
        );

        // Right-click is the fast path to the very same menu.
        dispatch_context_menu(&agent, 40.0, 60.0);
        next_tick().await;
        let menu = container
            .query_selector(".sidebar-card-menu")
            .unwrap()
            .expect("right-clicking a row opens its actions");
        assert!(agent.contains(Some(&menu)));
        assert_eq!(
            open_menu_items(&container),
            from_button,
            "right-click and the actions button open one menu, with the same availability"
        );
        dispatch_key(&menu.clone().dyn_into::<HtmlElement>().unwrap(), "Escape");
        next_tick().await;

        // A menu item does its own row's action, then gets out of the way.
        actions.click();
        next_tick().await;
        menu_item(&container, "Rename agent").click();
        next_tick().await;
        assert!(
            agent
                .query_selector(".agent-card-name-input")
                .unwrap()
                .is_some(),
            "Rename from the menu opens this row's own name editor"
        );
        assert!(
            neighbour
                .query_selector(".agent-card-name-input")
                .unwrap()
                .is_none(),
            "and no other row's"
        );
        assert!(
            container
                .query_selector(".sidebar-card-menu")
                .unwrap()
                .is_none(),
            "choosing an action closes the menu"
        );
        dispatch_key(
            &container
                .query_selector(".agent-card-name-input")
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap(),
            "Escape",
        );
        next_tick().await;

        // The pin item asks the server to change this row's pin, and the menu
        // follows the server's authoritative pin state.
        let pin_requests = || -> Vec<(JsonValue, String)> {
            recorded_frames(&calls)
                .into_iter()
                .filter(|(kind, _, _)| kind == "set_agent_pins")
                .map(|(_, payload, stream)| (payload["update"].clone(), stream))
                .collect()
        };
        let row_actions = || -> HtmlElement {
            agent_card_el(&container, "compact-card")
                .query_selector(".agent-card-actions")
                .unwrap()
                .expect("the row offers its actions")
                .dyn_into()
                .unwrap()
        };
        let pins_from_server = |pinned: Vec<AgentAnnotationTarget>| {
            let mut snapshot = state.agents_view_preferences.get_untracked();
            snapshot.pins.pinned = pinned;
            state.apply_agents_view_snapshot("local", snapshot);
        };
        pins_from_server(Vec::new());
        next_tick().await;
        row_actions().click();
        next_tick().await;
        menu_item(&container, "Pin chat").click();
        next_tick().await;
        let pin_frame = |kind: &str| {
            (
                serde_json::json!({"kind": kind, "target": local_target("compact-card")}),
                "/host/local".to_owned(),
            )
        };
        assert_eq!(
            pin_requests(),
            vec![pin_frame("pin")],
            "the menu pins exactly this row, on its own host"
        );
        pins_from_server(vec![local_target("compact-card")]);
        next_tick().await;
        row_actions().click();
        next_tick().await;
        assert!(
            open_menu_items(&container)
                .iter()
                .any(|(text, _, _)| text == "Unpin chat"),
            "a pinned row's menu offers Unpin"
        );
        menu_item(&container, "Unpin chat").click();
        next_tick().await;
        assert_eq!(
            pin_requests(),
            vec![pin_frame("pin"), pin_frame("unpin")],
            "Unpin from the menu unpins the same row"
        );
        pins_from_server(Vec::new());
        next_tick().await;
        let agent = agent_card_el(&container, "compact-card");

        // A saved conversation is the same row shape, with its own actions.
        let history_chevron: HtmlElement = history
            .query_selector(".sidebar-card-expand")
            .unwrap()
            .expect("a saved conversation offers a chevron too")
            .dyn_into()
            .unwrap();
        let history_collapsed = history.get_bounding_client_rect().height();
        history_chevron.click();
        next_tick().await;
        let history_details = history
            .query_selector(".sidebar-card-details")
            .unwrap()
            .unwrap();
        assert!(
            history_details
                .text_content()
                .unwrap_or_default()
                .contains("saved-card-full-id"),
            "the expanded conversation shows the session it will resume"
        );
        let delete: HtmlElement = history_details
            .query_selector("button")
            .unwrap()
            .expect("the expanded conversation offers Delete")
            .dyn_into()
            .unwrap();
        assert_eq!(delete.text_content().unwrap_or_default(), "Delete");
        assert!(
            delete.get_bounding_client_rect().height() > 0.0,
            "Delete is visible once the row is expanded"
        );
        history_chevron.click();
        next_tick().await;
        assert_eq!(
            history.get_bounding_client_rect().height(),
            history_collapsed,
            "the chevron closes what it opened"
        );
        dispatch_context_menu(&history, 40.0, 120.0);
        next_tick().await;
        let history_menu = container
            .query_selector(".session-card .sidebar-card-menu")
            .unwrap()
            .expect("right-clicking a saved conversation opens its actions");
        let history_menu_text = history_menu.text_content().unwrap_or_default();
        for item in ["Resume conversation", "Delete conversation"] {
            assert!(
                history_menu_text.contains(item),
                "the conversation menu must offer {item}, got: {history_menu_text:?}"
            );
        }
        dispatch_key(
            &history_menu.clone().dyn_into::<HtmlElement>().unwrap(),
            "Escape",
        );
        next_tick().await;

        state.settings_open.set(true);
        next_tick().await;
        let toggle = container
            .query_selector("#sidebar-backend-labels")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        assert!(toggle.checked(), "backend labels are visible by default");
        toggle.click();
        next_tick().await;
        assert!(
            history.query_selector(".backend-badge").unwrap().is_none(),
            "color-only mode removes the inline label"
        );
        assert!(history.get_bounding_client_rect().height() <= 40.0);
        assert_eq!(
            agent.get_bounding_client_rect().height(),
            collapsed_height,
            "an agent row has no label to remove, so it does not change"
        );
        let restored = AppState::new();
        restore_appearance(&restored);
        restored.settings_open.set(true);
        let reopened = make_container();
        let reopened_handle = mount_to(reopened.clone(), move || {
            provide_context(restored.clone());
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        let saved_toggle = reopened
            .query_selector("#sidebar-backend-labels")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        assert!(
            !saved_toggle.checked(),
            "reopened Appearance keeps labels off"
        );
        drop(reopened_handle);
        reopened.remove();
        toggle.click();
        next_tick().await;
        assert!(
            history.query_selector(".backend-badge").unwrap().is_some(),
            "turning labels back on restores them immediately"
        );
        assert!(
            agent.query_selector(".backend-badge").unwrap().is_none(),
            "and still never adds one to an agent row"
        );
        drop(handle);
        container.remove();
    }

    /// F-06: the cancelled outcome must be reactively visible on the card, and
    /// must reset when the next turn starts. Pure-function coverage of
    /// `derive_agent_state` cannot see a card regressing to the completed
    /// class/check, losing its text, or failing to clear.
    #[wasm_bindgen_test]
    async fn cancelled_card_shows_the_outcome_and_resets_on_the_next_turn() {
        let container = make_container();
        let state = make_app_state("local");
        push_agent(&state, "local", "a1", "Cancel Me", true);
        let agent_id = AgentId("a1".to_owned());

        // A live turn.
        state.agent_turn_active.update(|map| {
            map.insert(agent_id.clone(), true);
        });
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        let card = agent_card_el(&container, "a1");
        assert!(
            !card
                .text_content()
                .unwrap_or_default()
                .contains("Cancelled"),
            "a running turn is not cancelled"
        );

        // The user interrupts; the request is in flight but unanswered.
        state.interrupt_pending.update(|pending| {
            pending.insert(agent_id.clone());
        });
        for _ in 0..3 {
            next_tick().await;
        }
        let card = agent_card_el(&container, "a1");
        assert!(
            card.text_content()
                .unwrap_or_default()
                .contains("Cancelling"),
            "the in-flight interrupt must be visible, got: {:?}",
            card.text_content()
        );

        // The backend answers. Driven through the real reducer rather than by
        // poking state: the cancelled outcome is now its own record, and the
        // thing worth guarding is that the event actually sets it.
        crate::dispatch::apply_chat_event(
            &state,
            "local",
            &agent_id,
            protocol::ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                message: "Operation cancelled".to_owned(),
            }),
        );
        for _ in 0..3 {
            next_tick().await;
        }
        let card = agent_card_el(&container, "a1");
        let text = card.text_content().unwrap_or_default();
        assert!(
            text.contains("Cancelled"),
            "the cancelled outcome must stay available to assistive tech, got: {text:?}"
        );
        let status = card
            .query_selector(".agent-card-status")
            .unwrap()
            .expect("the card renders a status element");
        let class = status.get_attribute("class").unwrap_or_default();
        assert!(
            !class.contains("completed"),
            "a cancelled turn must not reuse the completed style, got: {class}"
        );
        let hidden = status
            .query_selector(".visually-hidden")
            .unwrap()
            .expect("cancelled keeps an accessible label");
        assert_eq!(hidden.text_content().as_deref(), Some("Cancelled"));
        assert_eq!(status.get_attribute("title").as_deref(), Some("Cancelled"));
        assert!(
            hidden.get_bounding_client_rect().width() <= 1.0,
            "the cancelled word must not paint"
        );
        assert!(
            text.contains('\u{2298}'),
            "a cancelled turn shows the cancel glyph, got: {text:?}"
        );
        assert!(
            !text.contains('\u{2713}'),
            "a cancelled turn must not show the success check, got: {text:?}"
        );

        // The next turn clears the outcome, again through the reducer.
        crate::dispatch::apply_chat_event(
            &state,
            "local",
            &agent_id,
            protocol::ChatEvent::TypingStatusChanged(true),
        );
        for _ in 0..3 {
            next_tick().await;
        }
        let card = agent_card_el(&container, "a1");
        assert!(
            !card
                .text_content()
                .unwrap_or_default()
                .contains("Cancelled"),
            "the cancelled outcome describes the last turn only and must reset"
        );
    }

    /// A turn that ends with a backgrounded command still running is not a
    /// finished result: the backend wakes the agent when the command exits.
    /// Rendering the success check there told users the agent was waiting on
    /// them. The card must show a distinct "background work" status until the
    /// command completes, and a new turn takes precedence over it.
    #[wasm_bindgen_test]
    async fn idle_card_with_running_background_command_is_not_shown_as_done() {
        let container = make_container();
        let state = make_app_state("local");
        push_agent(&state, "local", "a1", "Background Runner", true);
        let agent_id = AgentId("a1".to_owned());
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let status_of = |container: &HtmlElement| {
            let card = agent_card_el(container, "a1");
            let status = card
                .query_selector(".agent-card-status")
                .unwrap()
                .expect("the card renders a status element");
            (
                status.get_attribute("title").unwrap_or_default(),
                status.get_attribute("class").unwrap_or_default(),
                card.text_content().unwrap_or_default(),
            )
        };

        let (title, class, text) = status_of(&container);
        assert_eq!(title, "Idle");
        assert!(
            text.contains('\u{2713}'),
            "an idle agent shows the check: {text:?}"
        );
        assert!(
            class.contains("completed"),
            "idle uses the completed style: {class}"
        );

        // The model runs a command in the background and ends its turn.
        let apply = |event: protocol::ChatEvent| {
            crate::dispatch::apply_chat_event(&state, "local", &agent_id, event);
        };
        apply(protocol::ChatEvent::TypingStatusChanged(true));
        apply(protocol::ChatEvent::StreamStart(
            protocol::StreamStartData {
                agent: "claude".to_owned(),
                model: None,
            },
        ));
        apply(protocol::ChatEvent::StreamEnd(protocol::StreamEndData {
            message: protocol::ChatMessage {
                origin: None,
                message_id: None,
                timestamp: 1,
                sender: protocol::MessageSender::Assistant {
                    agent: "claude".to_owned(),
                },
                content: "running the check in the background".to_owned(),
                reasoning: None,
                tool_calls: vec![protocol::ToolUseData {
                    tool_call_id: "toolu_bg".to_owned(),
                    name: "run_command".to_owned(),
                    arguments: JsonValue::Object(Default::default()),
                    content_offset: None,
                }],
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        }));
        apply(protocol::ChatEvent::ToolRequest(protocol::ToolRequest {
            tool_call_id: "toolu_bg".to_owned(),
            tool_name: "run_command".to_owned(),
            tool_type: protocol::ToolRequestType::RunCommand {
                command: "./dev.sh check".to_owned(),
                working_directory: "/tmp/work".to_owned(),
            },
        }));
        apply(protocol::ChatEvent::ToolProgress(
            protocol::ToolProgressData {
                tool_call_id: "toolu_bg".to_owned(),
                execution_mode: protocol::ToolExecutionMode::Background,
                cancellable: false,
                update: protocol::ToolProgressUpdate::Other {
                    payload: serde_json::json!({
                        "task_id": "task-bg",
                        "description": "./dev.sh check",
                    }),
                },
            },
        ));
        apply(protocol::ChatEvent::TypingStatusChanged(false));
        for _ in 0..3 {
            next_tick().await;
        }

        let (title, class, text) = status_of(&container);
        assert_eq!(title, "Background work");
        assert!(
            text.contains('\u{29D7}'),
            "background work must show the hourglass: {text:?}"
        );
        assert!(
            !text.contains('\u{2713}'),
            "an agent waiting on background work must not show the success check: {text:?}"
        );
        assert!(
            !text.contains('\u{25F7}'),
            "background work is not a running turn and must not reuse the clock: {text:?}"
        );
        assert!(
            class.contains("background")
                && !class.contains("completed")
                && !class.contains("running"),
            "background work uses the warning style, not completed or running: {class}"
        );
        let status = agent_card_el(&container, "a1")
            .query_selector(".agent-card-status")
            .unwrap()
            .expect("the card renders a status element");
        let hidden_label = status
            .query_selector(".visually-hidden")
            .unwrap()
            .expect("background-work label stays available to screen readers");
        assert_eq!(
            hidden_label.text_content().as_deref(),
            Some("Background work")
        );
        let label_box = hidden_label.get_bounding_client_rect();
        assert!(
            label_box.width() <= 1.0 && label_box.height() <= 1.0,
            "the hourglass and yellow are enough; the label must not take visible space: {}x{}",
            label_box.width(),
            label_box.height()
        );

        // The command exits; the agent is genuinely idle again.
        apply(protocol::ChatEvent::ToolExecutionCompleted(
            crate::components::tool_card::test_utils::succeeded_completion(
                "toolu_bg",
                protocol::ToolExecutionResult::RunCommand {
                    exit_code: 0,
                    stdout: "done".to_owned(),
                    stderr: String::new(),
                },
            ),
        ));
        for _ in 0..3 {
            next_tick().await;
        }
        let (title, _, text) = status_of(&container);
        assert_eq!(title, "Idle");
        assert!(
            text.contains('\u{2713}') && !text.contains("Background work"),
            "a completed command returns the card to the idle check: {text:?}"
        );

        // A new turn while another background command runs reads as Thinking.
        apply(protocol::ChatEvent::ToolProgress(
            protocol::ToolProgressData {
                tool_call_id: "toolu_bg_2".to_owned(),
                execution_mode: protocol::ToolExecutionMode::Background,
                cancellable: false,
                update: protocol::ToolProgressUpdate::Other {
                    payload: serde_json::json!({
                        "task_id": "task-bg-2",
                        "description": "sleep 30",
                    }),
                },
            },
        ));
        apply(protocol::ChatEvent::TypingStatusChanged(true));
        for _ in 0..3 {
            next_tick().await;
        }
        let (title, _, _) = status_of(&container);
        assert_eq!(title, "Thinking", "a live turn outranks background work");
    }

    fn text_position(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("expected rendered text to contain {needle:?}; got {text:?}"))
    }

    /// The sidebar groups visible agents by host, then project, using the
    /// server-provided labels that users see in the rest of the UI.
    #[wasm_bindgen_test]
    async fn sidebar_renders_host_and_project_section_headers() {
        crate::dispatch::clear_host_seqs("local");
        crate::dispatch::clear_host_seqs("remote");
        let calls = install_send_stub_with_dialog_ok();
        let container =
            crate::wasm_test_support::ScopedRoot::new(make_container(), "agents-panel-launch");
        let state = make_app_state("local");
        state
            .sidebar_layout
            .update(|layouts| layouts.mode = crate::sidebar_layout::InterfaceMode::Dinosaur);
        seed_sidebar_group_fixture(&state);
        for host in ["local", "remote"] {
            push_agent(&state, host, "unscoped", "Unscoped chat", true);
        }

        state.projects.update(|projects| {
            projects.push(project_info("local", "empty", "Empty Local Project", 2));
            projects.push(project_info("remote", "empty", "Empty Remote Project", 2));
        });
        state.launch_profile_catalog.update(|catalogs| {
            catalogs.insert(
                "remote".into(),
                protocol::LaunchProfileCatalog {
                    entries: vec![protocol::LaunchProfileEntry::Ready {
                        profile: protocol::LaunchProfile {
                            id: protocol::LaunchProfileId("remote-codex".into()),
                            kind: protocol::LaunchProfileKind::BackendDefault,
                            label: "Codex".into(),
                            description: None,
                            backend_kind: BackendKind::Codex,
                            session_settings: Default::default(),
                        },
                    }],
                    default_profile_id: Some(protocol::LaunchProfileId("remote-codex".into())),
                    home_custom_agent_id: None,
                    custom_profile_schemas: Default::default(),
                },
            );
        });
        CenterWorkspaceWidth::forget_measurement();
        let mounted = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mounted.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <crate::components::project_rail::ProjectRail />
                <AgentsPanel />
                <crate::components::project_rail::ProjectEditor />
            }
        });
        for _ in 0..4 {
            next_tick().await;
        }

        let text = container
            .query_selector(".agents-panel")
            .unwrap()
            .unwrap()
            .text_content()
            .unwrap_or_default();
        for expected in [
            "Local Host",
            "Alpha Project",
            "Beta Project",
            "Remote Host",
            "Gamma Project",
        ] {
            assert!(
                text.contains(expected),
                "sidebar should render section header {expected:?}; got {text:?}"
            );
        }

        assert!(text_position(&text, "Local Host") < text_position(&text, "Alpha Project"));
        assert!(text_position(&text, "Alpha Project") < text_position(&text, "Parent Alpha Agent"));
        assert!(text_position(&text, "Parent Alpha Agent") < text_position(&text, "Beta Project"));
        assert!(text_position(&text, "Beta Project") < text_position(&text, "Beta Agent"));
        assert!(text_position(&text, "Beta Agent") < text_position(&text, "Remote Host"));
        assert!(text_position(&text, "Remote Host") < text_position(&text, "Gamma Agent"));

        for host in ["local", "remote"] {
            let section = container
                .query_selector(&format!(
                    ".agent-sidebar-host-section[data-host-id='{host}']"
                ))
                .unwrap()
                .unwrap();
            let headings = section
                .query_selector_all(".agent-sidebar-project-header")
                .unwrap();
            assert!(
                headings
                    .item(0)
                    .unwrap()
                    .text_content()
                    .unwrap()
                    .contains("No project"),
                "No-project chats precede projects on every host, regardless of roster order"
            );
            assert!(
                !headings
                    .item(0)
                    .unwrap()
                    .text_content()
                    .unwrap()
                    .contains("Project:")
            );
        }

        let parent = agent_card_el(&container, "parent-alpha");
        parent.click();
        next_tick().await;
        let selected = container
            .query_selector(".agent-card[aria-current='true']")
            .unwrap()
            .unwrap();
        // A reactive tick can sample the real selection transition mid-color.
        settle_sidebar_styles(&selected).await.unwrap();
        let theme = web_sys::window().unwrap();
        let sample = theme.document().unwrap().create_element("span").unwrap();
        sample
            .set_attribute("style", "background:var(--accent-subtle)")
            .unwrap();
        container.append_child(&sample).unwrap();
        assert_eq!(
            theme
                .get_computed_style(&selected)
                .unwrap()
                .unwrap()
                .get_property_value("background-color")
                .unwrap(),
            theme
                .get_computed_style(&sample)
                .unwrap()
                .unwrap()
                .get_property_value("background-color")
                .unwrap(),
            "Selection uses the existing theme token, not a filled blue mix"
        );
        sample.remove();
        let row_style = theme.get_computed_style(&selected).unwrap().unwrap();
        assert_eq!(
            row_style.get_property_value("border-left-width").unwrap(),
            "0px",
            "Flat conversation rows have no decorative rail"
        );
        assert_eq!(
            row_style.get_property_value("border-radius").unwrap(),
            "0px",
            "Conversation rows are not rounded cards"
        );
        assert_eq!(row_style.get_property_value("box-shadow").unwrap(), "none");
        let glyph = selected
            .query_selector("[role='img'][aria-label='Claude']")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        assert_eq!(
            (glyph.width(), glyph.height()),
            (18.0, 18.0),
            "Provider slot stays compact and square"
        );
        // A leaf conversation is one 28px line: no grip, no reserved space.
        let leaf = agent_card_el(&container, "beta-agent");
        assert!(
            leaf.get_bounding_client_rect().height() <= 29.0,
            "Conversations without a child disclosure remain compact"
        );
        assert!(
            !container
                .query_selector(".agents-panel")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap_or_default()
                .contains('⠿'),
            "No row carries a permanent drag grip"
        );
        let name = selected
            .query_selector(".agent-card-name")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        let disclosure = selected
            .query_selector("[aria-label='Toggle sub-agents']")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        assert!(
            disclosure.top() >= name.bottom(),
            "The child disclosure is below the full conversation name, not competing with it for width"
        );
        assert!(
            selected.get_bounding_client_rect().height() <= 60.0,
            "A parent keeps its name and child disclosure within two compact lines"
        );
        assert!(
            selected
                .query_selector("[role='img'][aria-label='Claude']")
                .unwrap()
                .is_some(),
            "Backend identity remains available"
        );
        for (host, label) in [("local", "Local Host"), ("remote", "Remote Host")] {
            let header = container
                .query_selector(&format!(
                    "[data-host-id='{host}'] > .agent-sidebar-host-header"
                ))
                .unwrap()
                .unwrap();
            let host_name = header
                .query_selector("span")
                .unwrap()
                .unwrap()
                .get_bounding_client_rect();
            let action = header
                .query_selector("button")
                .unwrap()
                .unwrap()
                .get_bounding_client_rect();
            // Flex line boxes can differ by 1/128px after layout quantization.
            assert!(
                (action.top() + action.height() / 2.0 - host_name.top() - host_name.height() / 2.0)
                    .abs()
                    <= 1.0,
                "Host and Project action share a visual row"
            );
            assert!(
                action.left() >= host_name.right(),
                "The project action must not overlap the host name"
            );
            assert!(header.text_content().unwrap().contains("+ Project"));
            assert!(header.text_content().unwrap().contains(label));
            assert!(
                header.get_bounding_client_rect().height() <= 32.0,
                "Host heading stays compact"
            );
            let empty = container
                .query_selector(&format!("[data-project-key='{host}:empty']"))
                .unwrap()
                .unwrap();
            assert!(empty.text_content().unwrap().contains("Empty"));
            assert_eq!(empty.query_selector_all(".agent-card").unwrap().length(), 0);
            assert!(
                !empty.text_content().unwrap().contains('0'),
                "Empty projects never show zero counts"
            );
            assert!(
                empty.get_bounding_client_rect().height() <= 40.0,
                "An empty project occupies only its tight heading row"
            );
            let label = empty
                .query_selector(".agent-folder-label")
                .unwrap()
                .unwrap();
            let style = theme.get_computed_style(&label).unwrap().unwrap();
            assert_eq!(
                style.get_property_value("text-transform").unwrap(),
                "none",
                "Project labels preserve server-provided casing"
            );
            assert_eq!(
                empty
                    .query_selector_all("button[title^='New agent in']")
                    .unwrap()
                    .length(),
                1
            );
        }
        let menu = container.query_selector("details").unwrap().unwrap();
        menu.set_attribute("open", "").unwrap();
        assert_eq!(
            menu.query_selector_all("input[type='checkbox']")
                .unwrap()
                .length(),
            5,
            "Every boolean filter exposes the same native keyboard control"
        );
        for name in [
            "Hide inactive",
            "Hide sub-agents",
            "Show other projects",
            "Show workbenches",
            "Hide empty projects",
        ] {
            let control = filter_control(&container, name);
            control.focus().unwrap();
            let slider = control.next_element_sibling().unwrap();
            let bounds = slider.get_bounding_client_rect();
            assert_eq!(bounds.width(), 36.0, "The standard Tyde toggle is visible");
            assert_eq!(bounds.height(), 20.0);
            let style = theme.get_computed_style(&slider).unwrap().unwrap();
            assert_eq!(style.get_property_value("outline-style").unwrap(), "solid");
        }
        let key = web_sys::KeyboardEventInit::new();
        key.set_key("Escape");
        key.set_bubbles(true);
        filter_control(&container, "Hide empty projects")
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &key)
                    .unwrap(),
            )
            .unwrap();
        assert!(!menu.has_attribute("open"));
        assert!(
            theme
                .document()
                .unwrap()
                .active_element()
                .unwrap()
                .is_same_node(Some(&menu.query_selector("summary").unwrap().unwrap())),
            "Escape restores the filter disclosure's keyboard focus"
        );
        let original_selection = state.active_project.get_untracked();
        let original_tabs = state
            .center_zone
            .with_untracked(|center| (center.all_tab_ids(), center.active_tab_id()));
        let remote_new = || {
            container
                .query_selector("[data-project-key='remote:empty'] button[title^='New agent in']")
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
        };
        container
            .query_selector("button[aria-label='New agent']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let document = web_sys::window().unwrap().document().unwrap();
        let dialog = || {
            document
                .query_selector("[role='dialog'][aria-label='New agent']")
                .unwrap()
                .unwrap()
        };
        // Host and Project are now separate controls, not a serialized tuple
        // in a select or a combined "Host · Project" label in textContent.
        let assert_destination = |host_id: &str, host_label: &str, project: &str| {
            let host = dialog()
                .query_selector("select[aria-label='Host']")
                .unwrap()
                .unwrap()
                .dyn_into::<web_sys::HtmlSelectElement>()
                .unwrap();
            assert_eq!(host.value(), host_id);
            assert_eq!(
                host.item(host.selected_index() as u32)
                    .unwrap()
                    .text_content()
                    .as_deref(),
                Some(host_label)
            );
            assert_eq!(
                dialog()
                    .query_selector("button[aria-label='Project']")
                    .unwrap()
                    .unwrap()
                    .text_content()
                    .as_deref(),
                Some(project)
            );
        };
        assert_destination("local", "Local Host", "Alpha Project");
        assert_eq!(state.active_project.get_untracked(), original_selection);
        document
            .query_selector("[aria-label='Close agent chooser']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;

        // Each No project heading offers the same New agent button as a
        // project, and it launches into No project on that heading's own
        // host, never into the active project or another host.
        let project_add = container
            .query_selector("[data-project-key='local:empty'] button[title^='New agent in']")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        for (host, host_label) in [("remote", "Remote Host"), ("local", "Local Host")] {
            let group = container
                .query_selector(&format!("[data-project-key='{host}:no-project']"))
                .unwrap()
                .unwrap_or_else(|| panic!("{host} renders its No project group"));
            let rows = group.query_selector_all(".agent-card").unwrap().length();
            assert!(
                rows > 0,
                "{host}'s No project group holds its unscoped chat"
            );
            let header_text = group
                .query_selector(".agent-sidebar-project-header")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap_or_default();
            let add: HtmlElement = group
                .query_selector(".agent-sidebar-project-header button.project-new-agent")
                .unwrap()
                .unwrap_or_else(|| panic!("{host}'s No project heading offers New agent"))
                .dyn_into()
                .unwrap();
            assert_eq!(add.text_content().as_deref(), Some("+"));
            assert_eq!(
                add.get_attribute("aria-label"),
                Some(format!("New agent without a project on {host_label}"))
            );
            let add_rect = add.get_bounding_client_rect();
            assert_eq!(
                (add_rect.width(), add_rect.height()),
                (project_add.width(), project_add.height()),
                "No project's button is the same unobtrusive control a project has"
            );
            add.click();
            next_tick().await;
            assert_destination(host, host_label, "No project / Home");
            assert_eq!(
                state.active_project.get_untracked(),
                original_selection,
                "Opening the chooser cannot navigate"
            );
            dialog()
                .query_selector("[aria-label='Close agent chooser']")
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
                .click();
            next_tick().await;
            assert_eq!(
                group.query_selector_all(".agent-card").unwrap().length(),
                rows
            );
            assert_eq!(
                group
                    .query_selector(".agent-sidebar-project-header")
                    .unwrap()
                    .unwrap()
                    .text_content()
                    .unwrap_or_default(),
                header_text,
                "the heading and its count are unchanged"
            );
        }
        remote_new().click();
        next_tick().await;
        assert_destination("remote", "Remote Host", "Empty Remote Project");
        state.projects.update(|projects| {
            projects
                .iter_mut()
                .find(|project| project.host_id == "remote" && project.project.id.0 == "empty")
                .unwrap()
                .project
                .name = "Updated Empty Remote Project".into()
        });
        next_tick().await;
        assert_destination("remote", "Remote Host", "Updated Empty Remote Project");
        assert_eq!(
            state.active_project.get_untracked(),
            original_selection,
            "Opening a project chooser cannot navigate"
        );
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center| (center.all_tab_ids(), center.active_tab_id())),
            original_tabs
        );
        dialog()
            .query_selector("[aria-label='Close agent chooser']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        remote_new().click();
        next_tick().await;
        let buttons = dialog().query_selector_all("button").unwrap();
        (0..buttons.length())
            .filter_map(|i| buttons.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().as_deref() == Some("Create agent"))
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            state.active_project.get_untracked(),
            Some(ActiveProjectRef {
                host_id: "remote".into(),
                project_id: ProjectId("empty".into())
            })
        );
        let draft = state
            .center_zone
            .with_untracked(|center| center.active_tab_id())
            .unwrap();
        let composer = state.composer_for(draft);
        assert_eq!(
            composer.selection_host.get_untracked().as_deref(),
            Some("remote")
        );
        assert_eq!(
            composer.launch_profile_id.get_untracked(),
            Some(protocol::LaunchProfileId("remote-codex".into()))
        );
        state.switch_active_project(original_selection);
        next_tick().await;
        state.projects.update(|projects| {
            for id in ["alpha-workbench", "alpha-workbench-two"] {
                let mut workbench = project_info("local", id, id, 2);
                workbench.project.source = ProjectSource::GitWorkbench {
                    environment: Default::default(),
                    parent_project_id: ProjectId("alpha".to_owned()),
                    branch: protocol::GitBranchName(id.to_owned()),
                    roots: vec![protocol::WorkbenchRoot {
                        parent_root: ProjectRootPath("/tmp/alpha".to_owned()),
                        worktree_root: ProjectRootPath(format!("/tmp/{id}")),
                    }],
                };
                projects.push(workbench);
            }
            projects.push(project_info(
                "remote",
                "alpha-workbench",
                "Remote Collision",
                1,
            ));
        });
        push_agent_with_scope(
            &state,
            "local",
            "workbench-agent",
            "Workbench Agent",
            true,
            Some("alpha-workbench"),
            None,
        );
        push_agent_with_scope(
            &state,
            "remote",
            "remote-collision",
            "Remote Collision Agent",
            true,
            Some("alpha-workbench"),
            None,
        );
        state.agents.update(|agents| {
            let moved = agents
                .iter_mut()
                .find(|agent| agent.agent_id.0 == "parent-alpha")
                .unwrap();
            moved.project_id = Some(ProjectId("alpha-workbench-two".to_owned()));
            moved.workspace_roots = vec!["/tmp/alpha-workbench-two".to_owned()];
        });
        for _ in 0..4 {
            next_tick().await;
        }
        let text = container.text_content().unwrap_or_default();
        assert!(
            container
                .query_selector(
                    ".agent-sidebar-project-header [aria-label='Project: alpha-workbench']"
                )
                .unwrap()
                .is_none(),
            "Workbench context belongs on its row, not a second project heading"
        );
        let workbench_checkbox = container
            .query_selector("input[aria-label='Show workbenches']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        assert!(
            !workbench_checkbox.checked(),
            "Workbench grouping is opt-in"
        );
        assert!(
            container
                .query_selector(".agent-sidebar-workbench-section")
                .unwrap()
                .is_none(),
            "All workbench headings are absent by default"
        );
        let alpha = container
            .query_selector("[data-project-key='local:alpha']")
            .unwrap()
            .unwrap();
        let alpha_text = alpha.text_content().unwrap_or_default();
        for name in ["Parent Alpha Agent", "Child Alpha Agent", "Workbench Agent"] {
            assert!(
                alpha_text.contains(name),
                "the parent project must contain {name}"
            );
        }
        assert!(!alpha_text.contains("Remote Collision Agent"));
        assert!(text.contains("Remote Collision"));
        workbench_checkbox.click();
        next_tick().await;
        assert!(workbench_checkbox.checked());
        container
            .query_selector("[aria-label='Project: Beta Project']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let bench = container
            .query_selector("[data-project-key='local:alpha'] [data-project-id='alpha-workbench']")
            .unwrap()
            .unwrap();
        assert!(
            bench.text_content().unwrap().contains("Workbench Agent"),
            "Visible workbench grouping contains its exact-project conversation"
        );
        assert!(
            !bench
                .text_content()
                .unwrap()
                .contains("Remote Collision Agent"),
            "Workbench membership is host scoped"
        );
        assert!(container.query_selector("[data-project-key='local:alpha'] [data-project-id='alpha-workbench-two'] button[aria-label='New agent in alpha-workbench-two']").unwrap().is_some(), "Visible workbenches retain exact-project launch controls");
        let disclosure = bench
            .query_selector("[aria-label='Workbench: alpha-workbench']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        disclosure.focus().unwrap();
        disclosure.click();
        next_tick().await;
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&disclosure)),
            "Workbench disclosure retains keyboard focus when toggled"
        );
        assert!(
            container
                .query_selector(
                    "[data-project-id='alpha-workbench'] [data-agent-id='workbench-agent']"
                )
                .unwrap()
                .is_none(),
            "Workbench disclosure collapses its conversations"
        );
        workbench_checkbox.click();
        next_tick().await;
        assert!(
            container
                .query_selector(
                    "[data-project-key='local:alpha'] [data-agent-id='workbench-agent']"
                )
                .unwrap()
                .is_some(),
            "Hiding grouping reveals conversations even when that workbench was collapsed"
        );
        let mut confirmed = state.effective_agents_sidebar_preferences();
        confirmed.show_workbenches = true;
        state.apply_agents_view_snapshot("local", sidebar_snapshot(confirmed));
        next_tick().await;
        assert!(
            workbench_checkbox.checked(),
            "Authoritative preference updates the checkbox"
        );
        confirmed = state.effective_agents_sidebar_preferences();
        confirmed.show_workbenches = false;
        state.apply_agents_view_snapshot("local", sidebar_snapshot(confirmed));
        next_tick().await;

        let before_empty_filter = state.agents_view_preferences.get_untracked();
        let mut filtered_preferences = before_empty_filter.clone();
        filtered_preferences.sidebar.hide_empty_projects = true;
        filtered_preferences.sidebar.hide_inactive = true;
        filtered_preferences.sidebar.show_workbenches = true;
        state.projects.update(|projects| {
            let mut empty_workbench = projects
                .iter()
                .find(|project| {
                    project.host_id == "local" && project.project.id.0 == "alpha-workbench"
                })
                .unwrap()
                .clone();
            empty_workbench.project.id = ProjectId("alpha-empty-workbench".into());
            empty_workbench.project.name = "Empty workbench".into();
            projects.push(empty_workbench);
        });
        filtered_preferences
            .pins
            .pinned
            .push(local_target("workbench-agent"));
        state.agents_view_preferences.set(filtered_preferences);
        next_tick().await;
        for host in ["local", "remote"] {
            assert!(
                container
                    .query_selector(&format!("[data-project-key='{host}:empty']"))
                    .unwrap()
                    .is_none(),
                "Only truly empty projects disappear"
            );
        }
        for key in [
            "local:alpha",
            "local:beta",
            "remote:gamma",
            "remote:alpha-workbench",
        ] {
            assert!(
                container
                    .query_selector(&format!("[data-project-key='{key}']"))
                    .unwrap()
                    .is_some(),
                "Idle project and workbench conversations remain occupied when activity filters hide rows"
            );
        }
        assert!(
            container
                .query_selector("[data-project-id='alpha-empty-workbench']")
                .unwrap()
                .is_none(),
            "An empty workbench is hidden even when its parent contains agents"
        );
        // The rendered pinned-only workbench has zero rows: its conversation
        // moved to Pinned. Keeping its former heading certified the defect.
        assert!(
            container
                .query_selector("[data-project-id='alpha-workbench']")
                .unwrap()
                .is_none(),
            "The pinned-only workbench has no rows; its conversation is in Pinned"
        );
        assert!(
            container
                .query_selector(".agent-sidebar-pinned [data-agent-id='workbench-agent']")
                .unwrap()
                .is_some(),
            "Hiding its empty former heading never hides the pinned conversation"
        );
        let before_ghost_agents = state.agents.get_untracked();
        let before_ghost_projects = state.projects.get_untracked();
        for id in ["child-only", "group-only", "pin-only"] {
            state.projects.update(|projects| {
                projects.push(project_info("local", id, id, 3));
            });
            push_agent_with_scope(
                &state,
                "local",
                id,
                id,
                true,
                Some(id),
                (id == "child-only").then_some("parent-alpha"),
            );
        }
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.pins.pinned.push(local_target("pin-only"));
        preferences.groups = AgentGroupsSnapshot {
            groups: vec![AgentGroup {
                id: AgentGroupId("recovery-group".into()),
                name: "Recovery group".into(),
            }],
            assignments: vec![AgentGroupAssignment {
                group_id: AgentGroupId("recovery-group".into()),
                target: local_target("group-only"),
            }],
        };
        preferences.sidebar.hide_sub_agents = true;
        preferences.sidebar.hide_inactive = false;
        for show in [false, true] {
            preferences.sidebar.show_workbenches = show;
            state.agents_view_preferences.set(preferences.clone());
            next_tick().await;
            for id in ["child-only", "group-only", "pin-only"] {
                assert!(
                    container
                        .query_selector(&format!("[data-project-key='local:{id}']"))
                        .unwrap()
                        .is_none(),
                    "Relocated or hidden rows cannot retain an empty source heading"
                );
            }
            assert!(
                container
                    .query_selector(".agent-sidebar-custom-group [data-agent-id='group-only']")
                    .unwrap()
                    .is_some()
            );
            assert!(
                container
                    .query_selector(".agent-sidebar-pinned [data-agent-id='pin-only']")
                    .unwrap()
                    .is_some()
            );
            assert!(
                container
                    .query_selector(
                        "[data-project-key='local:alpha'] [data-agent-id='parent-alpha']"
                    )
                    .unwrap()
                    .is_some(),
                "An idle conversation stays under its project with either grouping choice"
            );
        }
        let ownership_search = container
            .query_selector("input[placeholder='Filter agents...']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        preferences.sidebar.hide_sub_agents = false;
        for (owner, child, name, query) in [
            (
                "pin-only",
                "matching-child",
                "Matching child",
                "Matching child",
            ),
            ("group-only", "working-child", "Working child", ""),
        ] {
            push_agent_with_scope(&state, "local", child, name, true, Some(owner), Some(owner));
            preferences.sidebar.hide_inactive = owner == "group-only";
            state.agent_turn_active.update(|active| {
                active.insert(AgentId(child.into()), owner == "group-only");
            });
            state.agents_view_preferences.set(preferences.clone());
            ownership_search.set_value(query);
            ownership_search
                .dispatch_event(&web_sys::Event::new("input").unwrap())
                .unwrap();
            for destination in [owner, "alpha-empty-workbench"] {
                state.agents.update(|agents| {
                    for agent in agents
                        .iter_mut()
                        .filter(|agent| agent.agent_id.0 == owner || agent.agent_id.0 == child)
                    {
                        agent.project_id = Some(ProjectId(destination.into()));
                    }
                });
                next_tick().await;
                let scope = if destination == owner {
                    format!("[data-project-key='local:{owner}']")
                } else {
                    "[data-project-id='alpha-empty-workbench']".into()
                };
                assert!(
                    container
                        .query_selector(&format!("{scope} [data-agent-id='{child}']"))
                        .unwrap()
                        .is_some(),
                    "A matching child remains in its exact project/workbench when its owner is filtered out"
                );
            }
        }
        ownership_search.set_value("");
        ownership_search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        state.agent_turn_active.update(|active| {
            for child in ["matching-child", "working-child"] {
                active.remove(&AgentId(child.into()));
            }
        });
        state.agents.set(before_ghost_agents);
        state.projects.set(before_ghost_projects);
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.groups = before_empty_filter.groups.clone();
        preferences.pins = before_empty_filter.pins.clone();
        preferences.sidebar.hide_inactive = true;
        state.agents_view_preferences.set(preferences);
        next_tick().await;
        let search_input = container
            .query_selector("input[placeholder='Filter agents...']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        search_input.set_value("no agent matches this search");
        search_input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-project-key='local:alpha']")
                .unwrap()
                .is_some(),
            "A project with only filtered-out workbench agents is not empty"
        );
        assert!(
            container
                .query_selector("[data-project-key='remote:gamma']")
                .unwrap()
                .is_some(),
            "Search results cannot define actual catalog membership"
        );
        search_input.set_value("");
        search_input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        state.projects.update(|projects| {
            projects.retain(|project| project.project.id.0 != "alpha-empty-workbench")
        });
        state.agents_view_preferences.set(before_empty_filter);
        next_tick().await;
        assert!(
            container
                .query_selector("[data-project-key='local:empty']")
                .unwrap()
                .is_some(),
            "Empty projects return when the explicit option is cleared"
        );

        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.sidebar.project_visibility = AgentsSidebarProjectVisibility::CurrentProjectOnly;
        state.agents_view_preferences.set(preferences);
        for selected in ["alpha", "alpha-workbench", "alpha-workbench-two"] {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "local".to_owned(),
                project_id: ProjectId(selected.to_owned()),
            }));
            for _ in 0..4 {
                next_tick().await;
            }
            let text = container.text_content().unwrap_or_default();
            for name in ["Parent Alpha Agent", "Child Alpha Agent", "Workbench Agent"] {
                assert!(
                    text.contains(name),
                    "selecting {selected} must keep {name} visible"
                );
            }
            for name in ["Beta Agent", "Gamma Agent", "Remote Collision Agent"] {
                assert!(
                    !text.contains(name),
                    "selecting {selected} must exclude {name}"
                );
            }
        }

        let rail = container.query_selector("nav").unwrap().unwrap();
        rail.query_selector("button[title='Expand']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let rail_button = |name: &str| {
            rail.query_selector(&format!("button[title='{name}']"))
                .unwrap()
                .expect("named rail button")
                .dyn_into::<HtmlElement>()
                .unwrap()
        };
        let selected_button = rail_button("alpha-workbench-two");
        let selected = state.active_project.get_untracked();
        let notify = |seq, payload| {
            dispatch_frame(
                &state,
                "local",
                StreamPath("/host/local".into()),
                FrameKind::ProjectNotify,
                seq,
                &payload,
            );
        };
        let mut beta = project_info("local", "beta", "Renamed Beta", 0).project;
        beta.source = ProjectSource::Standalone {
            roots: vec![
                ProjectRootPath("/tmp/beta".into()),
                ProjectRootPath("/tmp/extra-root".into()),
            ],
        };
        let alpha = project_info("local", "alpha", "Renamed Alpha", 3).project;
        let mut workbench = state.projects.with(|projects| {
            projects
                .iter()
                .find(|info| info.host_id == "local" && info.project.id.0 == "alpha-workbench-two")
                .unwrap()
                .project
                .clone()
        });
        workbench.name = "Renamed workbench".into();
        workbench.sort_order = 0;
        for (seq, project) in [alpha, beta.clone(), workbench.clone()]
            .into_iter()
            .enumerate()
        {
            notify(
                seq as u64,
                protocol::ProjectNotifyPayload::Upsert { project },
            );
        }
        next_tick().await;
        assert_eq!(
            selected_button.get_attribute("title").as_deref(),
            Some("Renamed workbench")
        );
        assert!(
            selected_button
                .text_content()
                .unwrap()
                .contains("Renamed workbench")
        );
        assert_eq!(state.active_project.get_untracked(), selected);
        let buttons = rail.query_selector_all(".rail-project-row button").unwrap();
        let labels: Vec<_> = (0..buttons.length())
            .map(|i| {
                buttons
                    .item(i)
                    .unwrap()
                    .dyn_into::<web_sys::Element>()
                    .unwrap()
                    .get_attribute("title")
                    .unwrap()
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Renamed Beta",
                "Empty Local Project",
                "Renamed Alpha",
                "Renamed workbench",
                "alpha-workbench",
                "Gamma Project",
                "Remote Collision",
                "Updated Empty Remote Project"
            ]
        );
        let sidebar = container.query_selector(".agents-panel").unwrap().unwrap();
        let text = sidebar.text_content().unwrap();
        assert!(text.contains("Renamed Alpha"));
        assert!(text.contains("Workbench Agent"));
        assert!(!text.contains("Remote Collision Agent"));

        rail_button("Renamed Beta")
            .dispatch_event(&web_sys::MouseEvent::new("contextmenu").unwrap())
            .unwrap();
        next_tick().await;
        let menu_buttons = container
            .query_selector_all(".context-menu button")
            .unwrap();
        (0..menu_buttons.length())
            .filter_map(|i| menu_buttons.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().unwrap().contains("Edit project"))
            .unwrap()
            .click();
        next_tick().await;
        let editor = container
            .query_selector("[role='dialog'][aria-label='Edit project']")
            .unwrap()
            .unwrap();
        assert!(editor.text_content().unwrap().contains("/tmp/extra-root"));
        beta.source = ProjectSource::Standalone {
            roots: vec![ProjectRootPath("/tmp/replaced-root".into())],
        };
        notify(3, protocol::ProjectNotifyPayload::Upsert { project: beta });
        next_tick().await;
        let text = editor.text_content().unwrap();
        assert!(text.contains("/tmp/replaced-root"));
        assert!(!text.contains("/tmp/extra-root"));
        assert!(!text.contains("/tmp/beta"));
        assert_eq!(
            state.active_project.get_untracked(),
            selected,
            "editing another project's catalog must not change selection"
        );
        let editor_buttons = editor.query_selector_all("button").unwrap();
        (0..editor_buttons.length())
            .filter_map(|i| editor_buttons.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().as_deref() == Some("Done"))
            .unwrap()
            .click();
        rail_button("Renamed Alpha").click();
        next_tick().await;
        selected_button.click();
        next_tick().await;
        assert!(
            recorded_frames(&calls)
                .iter()
                .any(|(kind, _, stream)| kind == "project_accessed"
                    && stream == "/project/alpha-workbench-two")
        );

        notify(
            4,
            protocol::ProjectNotifyPayload::Delete { project: workbench },
        );
        next_tick().await;
        assert!(
            rail.query_selector("button[title='Renamed workbench']")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state.active_project.get_untracked(),
            Some(ActiveProjectRef {
                host_id: "local".into(),
                project_id: ProjectId("alpha".into()),
            })
        );
        let live_empty = project_info("local", "live-empty", "Live Empty Project", 4).project;
        notify(
            5,
            protocol::ProjectNotifyPayload::Upsert {
                project: live_empty.clone(),
            },
        );
        next_tick().await;
        assert!(
            container
                .query_selector(".agents-panel [data-project-key='local:live-empty']")
                .unwrap()
                .is_none(),
            "Current-project scope excludes unrelated empty catalog entries"
        );
        state.agents_view_preferences.update(|preferences| {
            preferences.sidebar.project_visibility = AgentsSidebarProjectVisibility::AllProjects
        });
        next_tick().await;
        assert!(container.query_selector(".agents-panel [data-project-key='local:live-empty'] button[aria-label='New agent in Live Empty Project']").unwrap().is_some());
        notify(
            6,
            protocol::ProjectNotifyPayload::Delete {
                project: live_empty,
            },
        );
        next_tick().await;
        assert!(
            container
                .query_selector(".agents-panel [data-project-key='local:live-empty']")
                .unwrap()
                .is_none(),
            "Catalog deletion removes empty project without a refresh"
        );
        state.agents_view_preferences.update(|preferences| {
            preferences.sidebar.project_visibility =
                AgentsSidebarProjectVisibility::CurrentProjectOnly
        });
        next_tick().await;
        let remote = rail_button("Remote Collision");
        remote.click();
        next_tick().await;
        assert_eq!(
            state.active_project.get_untracked(),
            Some(ActiveProjectRef {
                host_id: "remote".into(),
                project_id: ProjectId("alpha-workbench".into()),
            })
        );
        let text = sidebar.text_content().unwrap();
        assert!(text.contains("Remote Collision Agent"));
        assert!(!text.contains("Workbench Agent"));
        dispatch_frame(
            &state,
            "remote",
            StreamPath("/host/remote".into()),
            FrameKind::ProjectNotify,
            0,
            &protocol::ProjectNotifyPayload::Delete {
                project: project_info("remote", "alpha-workbench", "Remote Collision", 1).project,
            },
        );
        next_tick().await;
        assert!(
            rail.query_selector("button[title='Remote Collision']")
                .unwrap()
                .is_none()
        );
        assert!(
            rail_button("alpha-workbench")
                .text_content()
                .unwrap()
                .contains("alpha-workbench")
        );
        assert!(state.active_project.get_untracked().is_none());
    }

    /// Parent/child sub-agent nesting remains inside the host/project leaf:
    /// the parent keeps its visible child-count affordance and the child stays
    /// under the same project before the next project section starts.
    #[wasm_bindgen_test]
    async fn sidebar_preserves_parent_child_nesting_within_project_leaf() {
        crate::dispatch::clear_host_seqs("local");
        let container = make_container();
        // Shared-document geometry fixtures leave fixed max-z-index boxes.
        // Own this page just as CenterZone does, so the hit test measures
        // our two modal layers rather than an earlier empty fixture DIV.
        container
            .set_attribute(
                "style",
                "position: fixed; inset: 0; z-index: 2147483647; background: var(--surface-1);",
            )
            .unwrap();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);

        let calls = install_send_stub_with_dialog_ok();
        push_agent_with_scope(
            &state,
            "local",
            "inactive-alpha",
            "Quiet child",
            true,
            Some("alpha"),
            Some("parent-alpha"),
        );
        state.agent_turn_active.update(|turns| {
            turns.insert(AgentId("child-alpha".into()), true);
        });
        state.projects.update(|projects| {
            let alpha = projects
                .iter_mut()
                .find(|info| info.project.id.0 == "alpha" && info.host_id == "local")
                .unwrap();
            alpha.project.source = ProjectSource::Standalone {
                roots: vec![
                    ProjectRootPath("/tmp/alpha".into()),
                    ProjectRootPath("/tmp/second".into()),
                ],
            };
        });
        let mounted = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted.clone());
            view! { <style>{include_str!("../../styles.css")}</style><crate::notices::ProjectDialogs /><AgentsPanel /><crate::components::project_rail::ProjectEditor /><crate::components::host_browser::HostBrowser /> }
        });
        for _ in 0..4 {
            next_tick().await;
        }

        let text = container.text_content().unwrap_or_default();
        let alpha = text_position(&text, "Alpha Project");
        let parent = text_position(&text, "Parent Alpha Agent");
        let child = text_position(&text, "Child Alpha Agent");
        let beta = text_position(&text, "Beta Project");
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
                && group_text.contains("1 running"),
            "parent group should show parent, child, and visible child count; got {group_text:?}"
        );
        let parent = agent_card_el(&container, "parent-alpha");
        let child = agent_card_el(&container, "child-alpha");
        let quiet = agent_card_el(&container, "inactive-alpha");
        assert_eq!(
            child.get_bounding_client_rect().height(),
            0.0,
            "Children start collapsed"
        );
        let disclosure = parent
            .query_selector(".agent-card-collapse-toggle")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            disclosure.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        disclosure.click();
        next_tick().await;
        assert!(
            child.get_bounding_client_rect().height() > 0.0,
            "Running child is revealed"
        );
        assert_eq!(
            quiet.get_bounding_client_rect().height(),
            0.0,
            "Inactive child stays quiet"
        );
        let inactive = parent_group
            .query_selector(".agent-show-inactive")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(inactive.text_content().unwrap().trim(), "Show 1 inactive");
        inactive.click();
        next_tick().await;
        assert!(quiet.get_bounding_client_rect().height() > 0.0);
        state.agent_turn_active.update(|turns| {
            turns.insert(AgentId("child-alpha".into()), false);
        });
        next_tick().await;
        assert!(
            parent.text_content().unwrap().contains("0 running"),
            "Count reacts to authoritative activity"
        );
        assert_eq!(
            child
                .query_selector(".agent-card-status")
                .unwrap()
                .unwrap()
                .get_attribute("title")
                .as_deref(),
            Some("Idle")
        );
        // Project editing uses the same domain commands as the project rail,
        // and renders only the roots in the server's project snapshot.
        let pencil = container
            .query_selector("[aria-label='Edit project Alpha Project']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        pencil.focus().unwrap();
        pencil.click();
        next_tick().await;
        let editor = container
            .query_selector("[role='dialog'][aria-label='Edit project']")
            .unwrap()
            .unwrap();
        assert!(editor.text_content().unwrap().contains("/tmp/second"));
        let name_field = editor
            .query_selector("#project-editor-name")
            .unwrap()
            .unwrap();
        let save_button = editor.query_selector("button").unwrap().unwrap();
        assert!(
            save_button.has_attribute("disabled"),
            "Unchanged names cannot enqueue redundant renames"
        );
        assert!(
            save_button.get_bounding_client_rect().left()
                >= name_field.get_bounding_client_rect().right(),
            "Name and Save share one compact row"
        );
        let input = editor
            .query_selector("input")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        input.set_value("Renamed Alpha");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        editor
            .query_selector("button")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let frames = recorded_frames(&calls);
        assert!(frames.iter().any(
            |(kind, payload, _)| kind == "project_rename" && payload["name"] == "Renamed Alpha"
        ));
        editor
            .query_selector("[aria-label='Remove source root /tmp/second']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            recorded_frames(&calls)
                .iter()
                .any(|(kind, payload, _)| kind == "project_delete_root"
                    && payload["root"] == "/tmp/second")
        );
        assert!(
            editor.text_content().unwrap().contains("/tmp/second"),
            "Sending a command must not fabricate a successful root removal"
        );
        let mut updated = state
            .projects
            .get_untracked()
            .into_iter()
            .find(|info| info.host_id == "local" && info.project.id.0 == "alpha")
            .unwrap()
            .project;
        updated.name = "Renamed Alpha".into();
        updated.source = ProjectSource::Standalone {
            roots: vec![ProjectRootPath("/tmp/alpha".into())],
        };
        dispatch_frame(
            &state,
            "local",
            StreamPath("/host/local".into()),
            FrameKind::ProjectNotify,
            0,
            &protocol::ProjectNotifyPayload::Upsert { project: updated },
        );
        next_tick().await;
        assert!(!editor.text_content().unwrap().contains("/tmp/second"));
        // The name now appears only in the editable field; input values are
        // excluded from textContent. Save disabling also verifies the server echo.
        assert_eq!(input.value(), "Renamed Alpha");
        assert!(
            editor
                .query_selector("button")
                .unwrap()
                .unwrap()
                .has_attribute("disabled"),
            "The acknowledged name no longer offers a redundant Save"
        );
        // Open the existing host browser without switching the selected project.
        let before = state.active_project.get_untracked();
        let add = editor.query_selector_all("button").unwrap();
        let add = (0..add.length())
            .filter_map(|i| add.item(i))
            .find(|node| node.text_content().unwrap().contains("Add source root"))
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        add.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[role='dialog'][aria-label='Folder browser']")
                .unwrap()
                .is_some()
        );
        assert_eq!(state.active_project.get_untracked(), before);
        assert!(
            recorded_frames(&calls)
                .iter()
                .any(|(kind, _, _)| kind == "host_browse_start")
        );
        let browse = state.browse_dialog.get_untracked().unwrap();
        let stream = browse.browse_stream.get_untracked();
        dispatch_frame(
            &state,
            "local",
            stream.clone(),
            FrameKind::HostBrowseOpened,
            0,
            &protocol::HostBrowseOpenedPayload {
                home: protocol::HostAbsPath("/tmp".into()),
                root: protocol::HostAbsPath("/".into()),
                separator: '/',
                platform: protocol::HostPlatform::Linux,
            },
        );
        dispatch_frame(
            &state,
            "local",
            stream,
            FrameKind::HostBrowseEntries,
            1,
            &protocol::HostBrowseEntriesPayload {
                path: protocol::HostAbsPath("/tmp/added".into()),
                parent: Some(protocol::HostAbsPath("/tmp".into())),
                entries: vec![],
            },
        );
        next_tick().await;
        let browser_button = container
            .query_selector(".browser-btn-primary")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlButtonElement>()
            .unwrap();
        assert!(
            !browser_button.disabled(),
            "A real folder response must enable selection before hit-testing"
        );
        let bounds = browser_button.get_bounding_client_rect();
        let hit = document()
            .element_from_point(
                (bounds.left() + bounds.right()) as f32 / 2.0,
                (bounds.top() + bounds.bottom()) as f32 / 2.0,
            )
            .unwrap();
        wasm_bindgen_test::console_log!(
            "Folder selection hit: tag={}, class={}, bounds=({},{},{},{}), browser_z={}, editor_z={}",
            hit.tag_name(),
            hit.class_name(),
            bounds.x(),
            bounds.y(),
            bounds.width(),
            bounds.height(),
            web_sys::window()
                .unwrap()
                .get_computed_style(
                    &container
                        .query_selector(".browser-backdrop")
                        .unwrap()
                        .unwrap()
                )
                .unwrap()
                .unwrap()
                .get_property_value("z-index")
                .unwrap(),
            web_sys::window()
                .unwrap()
                .get_computed_style(
                    &container
                        .query_selector(".project-editor-overlay")
                        .unwrap()
                        .unwrap()
                )
                .unwrap()
                .unwrap()
                .get_property_value("z-index")
                .unwrap()
        );
        assert!(
            hit.is_same_node(Some(&browser_button)),
            "Folder selection must be pointer-reachable above the project editor after the folder response settles"
        );
        container
            .query_selector(".browser-btn-primary")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            recorded_frames(&calls)
                .iter()
                .any(|(kind, payload, _)| kind == "project_add_root"
                    && payload["root"] == "/tmp/added"
                    && payload["id"] == "alpha")
        );
        assert!(
            !editor.text_content().unwrap().contains("/tmp/added"),
            "The editor waits for authoritative roots"
        );
        let mut updated = state
            .projects
            .get_untracked()
            .into_iter()
            .find(|info| info.host_id == "local" && info.project.id.0 == "alpha")
            .unwrap()
            .project;
        updated.source = ProjectSource::Standalone {
            roots: vec![
                ProjectRootPath("/tmp/alpha".into()),
                ProjectRootPath("/tmp/added".into()),
            ],
        };
        dispatch_frame(
            &state,
            "local",
            StreamPath("/host/local".into()),
            FrameKind::ProjectNotify,
            1,
            &protocol::ProjectNotifyPayload::Upsert { project: updated },
        );
        next_tick().await;
        assert!(editor.text_content().unwrap().contains("/tmp/added"));
        assert!(
            container
                .query_selector(".browser-modal")
                .unwrap()
                .is_none()
        );
        let delete_button = |editor: &web_sys::Element| {
            let buttons = editor.query_selector_all("button").unwrap();
            (0..buttons.length())
                .filter_map(|index| buttons.item(index))
                .find(|node| node.text_content().as_deref() == Some("Delete project"))
                .unwrap()
                .dyn_into::<web_sys::HtmlButtonElement>()
                .unwrap()
        };
        let mut child = project_info("local", "delete-child", "Delete child", 1).project;
        child.source = ProjectSource::GitWorkbench {
            parent_project_id: ProjectId("alpha".into()),
            branch: protocol::GitBranchName("delete-child".into()),
            roots: vec![],
            environment: protocol::WorkbenchEnvironment::Native,
        };
        dispatch_frame(
            &state,
            "local",
            StreamPath("/host/local".into()),
            FrameKind::ProjectNotify,
            2,
            &protocol::ProjectNotifyPayload::Upsert { project: child },
        );
        next_tick().await;
        let blocked = delete_button(&editor);
        assert!(blocked.disabled());
        assert!(
            editor
                .text_content()
                .unwrap()
                .contains("Remove this project's workbenches")
        );
        let before_delete = calls.length();
        blocked.click();
        next_tick().await;
        assert_eq!(
            calls.length(),
            before_delete,
            "Blocked deletion never opens confirmation or submits a command"
        );
        state.project_editor.set(None);
        next_tick().await;
        container
            .query_selector("[aria-label='Edit project Gamma Project']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let editor = container
            .query_selector("[aria-label='Edit project']")
            .unwrap()
            .unwrap();
        crate::notices::report_project_send_failure(
            &state,
            "remote".into(),
            protocol::ProjectMutation::Delete(protocol::ProjectDeletePayload {
                id: ProjectId("another-remote-project".into()),
            }),
            "Another project's deletion could not be sent",
        );
        next_tick().await;
        assert!(
            crate::notices::has_notice(&crate::notices::NoticeScope::ProjectOperations(
                "remote".into(),
            )),
            "the send failure stays recorded"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Another project's deletion could not be sent"),
            "a send failure must not paint an app-wide notice"
        );
        assert!(
            !editor
                .text_content()
                .unwrap()
                .contains("Another project's deletion could not be sent"),
            "An unrelated project's failure must not appear inside this editor"
        );
        crate::notices::clear_scope(&crate::notices::NoticeScope::ProjectOperations(
            "remote".into(),
        ));
        let delete = delete_button(&editor);
        assert!(
            !delete.disabled(),
            "A remote standalone project is deletable even while a local project has workbenches"
        );
        js_sys::eval("window.__test_dialog_result = 'Cancel'").unwrap();
        let before_projects = state.projects.get_untracked();
        let before_agents = state.agents.get_untracked();
        delete.click();
        next_tick().await;
        assert!(
            !recorded_frames(&calls)
                .iter()
                .any(|(kind, _, _)| kind == "project_delete")
        );
        assert_eq!(state.projects.get_untracked(), before_projects);
        assert_eq!(state.agents.get_untracked(), before_agents);
        assert!(
            container
                .query_selector("[aria-label='Edit project']")
                .unwrap()
                .is_some()
        );
        let dialog = calls
            .iter()
            .filter_map(|call| {
                let call = call.dyn_into::<js_sys::Array>().unwrap();
                (call.get(0).as_string().as_deref() == Some("plugin:dialog|message")).then(|| {
                    serde_json::from_str::<JsonValue>(&call.get(1).as_string().unwrap()).unwrap()
                })
            })
            .next_back()
            .unwrap();
        let message = dialog["message"].as_str().unwrap();
        for expected in [
            "Gamma Project",
            "Source files stay on disk",
            "sessions and history are preserved",
            "steering and reviews are removed",
        ] {
            assert!(
                message.contains(expected),
                "Confirmation must explain the target and actual effects"
            );
        }
        js_sys::eval("delete window.__test_dialog_result; window.__test_dialog_error = true")
            .unwrap();
        delete.click();
        next_tick().await;
        assert!(
            editor
                .text_content()
                .unwrap()
                .contains("Native confirmation unavailable")
        );
        assert!(
            !recorded_frames(&calls)
                .iter()
                .any(|(kind, _, _)| kind == "project_delete")
        );
        assert_eq!(state.projects.get_untracked(), before_projects);
        assert_eq!(state.agents.get_untracked(), before_agents);
        js_sys::eval("delete window.__test_dialog_error").unwrap();
        editor
            .query_selector("[aria-label='Dismiss notice']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            !editor
                .text_content()
                .unwrap()
                .contains("Native confirmation unavailable")
        );
        state.host_streams.update(|streams| {
            streams.insert("remote".into(), StreamPath("/host/remote".into()));
        });
        delete.click();
        next_tick().await;
        let frames = recorded_frames(&calls);
        let (_, payload, operation_stream) = frames
            .iter()
            .find(|(kind, _, _)| kind == "project_delete")
            .unwrap();
        assert_eq!(payload["id"], "gamma");
        let send = calls
            .iter()
            .filter_map(|call| {
                let call = call.dyn_into::<js_sys::Array>().unwrap();
                let args: JsonValue =
                    serde_json::from_str(&call.get(1).as_string().unwrap()).unwrap();
                (call.get(0).as_string().as_deref() == Some("send_host_line")
                    && args["line"]
                        .as_str()
                        .is_some_and(|line| line.contains("project_delete")))
                .then_some(args)
            })
            .next_back()
            .unwrap();
        assert_eq!(
            send["hostId"], "remote",
            "Deletion routes to the editor's host, not the active project"
        );
        assert_eq!(
            state.projects.get_untracked(),
            before_projects,
            "Confirmation is not a catalog update"
        );
        crate::dispatch::reset_inbound_state_for_host("remote");
        dispatch_frame(
            &state,
            "remote",
            StreamPath("/host/remote".into()),
            FrameKind::ProjectOperation,
            0,
            &protocol::ProjectOperationPayload {
                host_id: protocol::HostFilterId("remote".into()),
                stream: StreamPath(operation_stream.clone()),
                mutation: protocol::ProjectMutation::Delete(protocol::ProjectDeletePayload {
                    id: ProjectId("gamma".into()),
                }),
                state: protocol::ProjectOperationState::Failed {
                    code: protocol::CommandErrorCode::Conflict,
                    message: "Remove this project's workbenches before deleting the project".into(),
                    cleanup_may_be_partial: false,
                },
            },
        );
        next_tick().await;
        assert!(
            editor
                .query_selector("[role='alert']")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Remove this project's workbenches")
        );
        assert_eq!(
            state.projects.get_untracked(),
            before_projects,
            "Failure leaves the catalog unchanged"
        );
        delete.click();
        next_tick().await;
        assert!(
            !editor
                .text_content()
                .unwrap()
                .contains("Remove this project's workbenches"),
            "A confirmed retry retires the superseded failure"
        );
        dispatch_frame(
            &state,
            "remote",
            StreamPath("/host/remote".into()),
            FrameKind::ProjectOperation,
            1,
            &protocol::ProjectOperationPayload {
                host_id: protocol::HostFilterId("remote".into()),
                stream: StreamPath("/project-operation/second-delete".into()),
                mutation: protocol::ProjectMutation::Delete(protocol::ProjectDeletePayload {
                    id: ProjectId("gamma".into()),
                }),
                state: protocol::ProjectOperationState::Failed {
                    code: protocol::CommandErrorCode::Internal,
                    message: "Project catalog could not be saved".into(),
                    cleanup_may_be_partial: false,
                },
            },
        );
        next_tick().await;
        assert!(
            editor
                .text_content()
                .unwrap()
                .contains("Project catalog could not be saved")
        );
        assert!(
            !editor
                .text_content()
                .unwrap()
                .contains("Remove this project's workbenches")
        );
        let gamma = state.projects.with_untracked(|projects| {
            projects
                .iter()
                .find(|info| info.host_id == "remote" && info.project.id.0 == "gamma")
                .unwrap()
                .project
                .clone()
        });
        dispatch_frame(
            &state,
            "local",
            StreamPath("/host/local".into()),
            FrameKind::ProjectNotify,
            3,
            &protocol::ProjectNotifyPayload::Delete {
                project: gamma.clone(),
            },
        );
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Edit project']")
                .unwrap()
                .is_some(),
            "Another host's deletion must not close this editor"
        );
        let agent = state.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|agent| agent.agent_id.0 == "gamma-agent")
                .unwrap()
                .clone()
        });
        dispatch_frame(
            &state,
            "remote",
            StreamPath("/agent/gamma-agent".into()),
            FrameKind::AgentProjectChanged,
            0,
            &protocol::AgentStartPayload {
                tyde_permissions: agent.tyde_permissions,
                swarm_membership: None,
                agent_id: agent.agent_id,
                name: agent.name,
                origin: agent.origin,
                backend_kind: agent.backend_kind,
                launch_profile_id: None,
                workspace_roots: agent.workspace_roots,
                project_id: None,
                custom_agent_id: None,
                team_id: None,
                team_member_id: None,
                parent_agent_id: None,
                session_id: None,
                workflow: None,
                created_at_ms: agent.created_at_ms,
            },
        );
        dispatch_frame(
            &state,
            "remote",
            StreamPath("/host/remote".into()),
            FrameKind::ProjectNotify,
            2,
            &protocol::ProjectNotifyPayload::Delete { project: gamma },
        );
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Edit project']")
                .unwrap()
                .is_none()
        );
        assert!(
            container
                .query_selector("[data-project-key='remote:gamma']")
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .agents
                .get_untracked()
                .iter()
                .any(|agent| agent.name == "Gamma Agent"),
            "Project deletion must not delete conversations"
        );
        crate::dispatch::reset_inbound_state_for_host("remote");
        crate::dispatch::reset_inbound_state_for_host("local");
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen::prelude::wasm_bindgen(
        inline_js = r#"export function trace_sidebar_mount(callback) { const limit = Error.stackTraceLimit; Error.stackTraceLimit = 80; try { callback(); } catch (error) { document.getElementById('output').textContent += '\nSidebar mount exception: ' + error.stack + '\n'; throw error; } finally { Error.stackTraceLimit = limit; } }"#
    )]
    extern "C" {
        fn trace_sidebar_mount(callback: &js_sys::Function);
    }

    #[wasm_bindgen::prelude::wasm_bindgen(
        inline_js = r#"export function trace_project_entry_errors() {
            const report = event => {
                const error = event.error || event.reason;
                const output = document.getElementById('output');
                output.textContent += '\nProject-entry browser exception: ' +
                    (error && error.stack ? error.stack : event.type) + '\n';
            };
            window.addEventListener('error', report);
            window.addEventListener('unhandledrejection', report);
            return () => {
                window.removeEventListener('error', report);
                window.removeEventListener('unhandledrejection', report);
            };
        }"#
    )]
    extern "C" {
        fn trace_project_entry_errors() -> js_sys::Function;
    }

    thread_local! {
        static TRACE_SIDEBAR_STACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn trace_sidebar_stack(phase: &str, address: usize) {
        if !TRACE_SIDEBAR_STACK.with(|enabled| enabled.get()) {
            return;
        }
        let output = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("output")
            .unwrap();
        output.set_text_content(Some(&format!(
            "{}Sidebar stack phase: {phase}, address: {address}\n",
            output.text_content().unwrap()
        )));
    }

    #[wasm_bindgen_test]
    async fn sidebar_keeps_children_nested_across_project_and_group_boundaries() {
        TRACE_SIDEBAR_STACK.with(|enabled| enabled.set(true));
        let trace = |phase: &str| {
            let output = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id("output")
                .unwrap();
            output.set_text_content(Some(&format!(
                "{}Sidebar nesting DOM phase: {phase}\n",
                output.text_content().unwrap()
            )));
        };
        trace("starting");
        let mut failures = Vec::new();
        for scope in ["another project", "no project", "parent custom group"] {
            trace(scope);
            let container = make_container();
            let state = make_app_state("local");
            seed_sidebar_group_fixture(&state);
            state.agents.update(|agents| {
                let child = agents
                    .iter_mut()
                    .find(|agent| agent.agent_id.0 == "child-alpha")
                    .expect("fixture child");
                child.origin = AgentOrigin::AgentControl;
                child.project_id = match scope {
                    "another project" => Some(ProjectId("beta".to_owned())),
                    "no project" => None,
                    _ => child.project_id.clone(),
                };
            });
            if scope == "parent custom group" {
                apply_group_snapshot(
                    &state,
                    assigned_group("owners", "Owners", &["parent-alpha"]),
                );
            }

            trace("fixture prepared");
            CenterWorkspaceWidth::forget_measurement();
            trace("measurement forgotten");
            let mounted_state = state.clone();
            let mounted_container = container.clone();
            let mount_result = std::rc::Rc::new(std::cell::RefCell::new(None));
            let result = mount_result.clone();
            let mount = wasm_bindgen::closure::Closure::once(move || {
                let handle = mount_to(mounted_container, move || {
                    provide_context(mounted_state.clone());
                    view! {
                        <style>{include_str!("../../styles.css")}</style>
                        <AgentsPanel />
                    }
                });
                *result.borrow_mut() = Some(handle);
            });
            trace_sidebar_mount(mount.as_ref().unchecked_ref());
            let handle = mount_result.borrow_mut().take().unwrap();
            trace("mounted, awaiting ticks");
            for _ in 0..4 {
                next_tick().await;
            }
            trace("mount ticks completed");

            let parent = agent_card_el(&container, "parent-alpha");
            let names = container.query_selector_all(".agent-card-name").unwrap();
            let child_names = (0..names.length())
                .filter_map(|index| names.item(index))
                .filter(|name| name.text_content().as_deref() == Some("Child Alpha Agent"))
                .collect::<Vec<_>>();
            assert_eq!(
                child_names.len(),
                1,
                "{scope}: render the child exactly once"
            );
            let child = child_names[0]
                .clone()
                .dyn_into::<web_sys::Element>()
                .unwrap()
                .closest(".agent-card")
                .unwrap()
                .expect("child card");
            // The approved disclosure now starts collapsed and separates
            // inactive children. Reveal both before measuring indentation.
            let disclosure: HtmlElement = parent
                .query_selector("button[title='Toggle sub-agents']")
                .unwrap()
                .unwrap()
                .dyn_into()
                .unwrap();
            disclosure.click();
            next_tick().await;
            if let Some(inactive) = container.query_selector(".agent-show-inactive").unwrap() {
                inactive.dyn_into::<HtmlElement>().unwrap().click();
                next_tick().await;
            }
            let parent_box = parent.get_bounding_client_rect();
            let child_box = child.get_bounding_client_rect();
            let toggle = parent
                .query_selector("button[title='Toggle sub-agents']")
                .unwrap();
            if child_box.left() <= parent_box.left() || toggle.is_none() {
                failures.push(format!(
                    "{scope}: child indentation={}px, parent has child toggle={}",
                    child_box.left() - parent_box.left(),
                    toggle.is_some()
                ));
            }
            if let Some(toggle) = toggle {
                trace("collapsing parent");
                toggle.dyn_into::<HtmlElement>().unwrap().click();
                trace("parent clicked");
                for _ in 0..4 {
                    next_tick().await;
                }
                assert_eq!(
                    child.get_bounding_client_rect().height(),
                    0.0,
                    "{scope}: collapsing the parent must hide its child"
                );
                assert!(parent.get_bounding_client_rect().height() > 0.0);
            }
            trace("dropping mount");
            drop(handle);
            trace("mount dropped");
            container.remove();
        }
        trace("all scopes completed");
        TRACE_SIDEBAR_STACK.with(|enabled| enabled.set(false));
        assert!(
            failures.is_empty(),
            "Owned children must not become root cards at sidebar boundaries: {failures:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn sidebar_global_pins_preserve_custom_groups_and_host_routing() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        crate::dispatch::prime_host_for_tests(&state, "local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(
            &state,
            assigned_group("review", "Review Group", &["beta-agent"]),
        );

        CenterWorkspaceWidth::forget_measurement();
        let mounted_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mounted_state.clone());
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <crate::components::header::Header />
                <AgentsPanel />
            }
        });
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
        let pin_button = |host: &str, id: &str| -> HtmlElement {
            container
                .query_selector(&format!(
                    ".agent-card-group[data-host-id='{host}'][data-agent-id='{id}'] > .agent-card .agent-card-pin"
                ))
                .unwrap()
                .expect("every sidebar chat offers a pin action")
                .dyn_into()
                .unwrap()
        };
        let pinned_section = || container.query_selector("[aria-label='Pinned']").unwrap();
        let notify_seq = std::cell::Cell::new(0);
        let notify = |snapshot: AgentsViewPreferencesSnapshot| {
            dispatch_frame(
                &state,
                "local",
                StreamPath("/host/local".to_owned()),
                FrameKind::AgentsViewPreferencesNotify,
                notify_seq.get(),
                &protocol::AgentsViewPreferencesNotifyPayload { snapshot },
            );
            notify_seq.set(notify_seq.get() + 1);
        };
        let last_pin_update = || {
            recorded_frames(&calls)
                .into_iter()
                .filter(|(kind, _, _)| kind == "set_agent_pins")
                .map(|(_, payload, stream)| (payload["update"].clone(), stream))
                .next_back()
                .expect("pin frame")
        };
        assert!(pinned_section().is_none());
        let pin = pin_button("local", "beta-agent");
        assert_eq!(pin.get_attribute("aria-label").as_deref(), Some("Pin chat"));
        assert_eq!(pin.get_attribute("aria-pressed").as_deref(), Some("false"));
        pin.focus().unwrap();
        dispatch_key(&pin, "Enter");
        dispatch_key(&pin, " ");
        assert!(
            state.active_agent.get_untracked().is_none(),
            "pin keyboard events must not open the chat"
        );
        pin.click();
        next_tick().await;
        let (update, stream) = last_pin_update();
        assert_eq!(
            update,
            serde_json::json!({"kind":"pin", "target":local_target("beta-agent")})
        );
        assert_eq!(stream, "/host/local");
        assert!(state.active_agent.get_untracked().is_none());
        assert!(state.active_project.get_untracked().is_none());
        assert!(
            pinned_section().is_none(),
            "wait for authoritative pin state"
        );
        assert_eq!(
            recorded_frames(&calls)
                .iter()
                .filter(|(kind, _, _)| kind == "set_agent_name"
                    || kind == "move_agent_to_project"
                    || kind == "set_agent_groups")
                .count(),
            0
        );

        // The same raw identity on another host must not inherit the local pin.
        push_agent_with_scope(
            &state,
            "remote",
            "beta-agent",
            "Remote Beta",
            true,
            Some("gamma"),
            None,
        );
        push_agent(&state, "local", "name-only", "Pinned in name only", true);
        let mut snapshot = state.agents_view_preferences.get_untracked();
        snapshot.groups.assignments.push(AgentGroupAssignment {
            group_id: snapshot.groups.groups[0].id.clone(),
            target: local_target("name-only"),
        });
        snapshot.pins.pinned = vec![local_target("beta-agent")];
        notify(snapshot.clone());
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[aria-label='Pinned']")
                .unwrap()
                .length(),
            1
        );
        assert_eq!(
            pin_button("remote", "beta-agent")
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("false")
        );
        let section = pinned_section().expect("Pinned section");
        assert!(
            section
                .closest(".agent-sidebar-host-section")
                .unwrap()
                .is_none(),
            "One global Pinned section must precede every host, not belong to a host"
        );
        assert!(section.text_content().unwrap().contains("Pinned"));
        assert!(section.text_content().unwrap().contains("Beta Agent"));
        assert!(
            !section
                .text_content()
                .unwrap()
                .contains("Pinned in name only")
        );
        let local_host = section.parent_element().unwrap();
        let local_text = local_host.text_content().unwrap();
        let normal_group = local_host
            .query_selector(".agent-sidebar-custom-group")
            .unwrap()
            .unwrap();
        let normal_project = local_host
            .query_selector(".agent-sidebar-project-section")
            .unwrap()
            .unwrap();
        assert!(
            section.get_bounding_client_rect().bottom()
                <= normal_group.get_bounding_client_rect().top()
        );
        assert!(
            section.get_bounding_client_rect().bottom()
                <= normal_project.get_bounding_client_rect().top()
        );
        assert!(local_text.find("Pinned").unwrap() < local_text.find("Alpha").unwrap());
        assert!(local_text.find("Pinned").unwrap() < local_text.find("Review Group").unwrap());
        assert_eq!(
            container
                .query_selector_all(
                    ".agent-card-group[data-host-id='local'][data-agent-id='beta-agent']"
                )
                .unwrap()
                .length(),
            1
        );
        assert_eq!(
            pin_button("local", "beta-agent")
                .get_attribute("aria-label")
                .as_deref(),
            Some("Unpin chat")
        );
        assert_eq!(
            pin_button("local", "beta-agent")
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("true")
        );

        let beta_card = pin_button("local", "beta-agent")
            .closest(".agent-card")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        begin_agent_drag(&beta_card);
        next_tick().await;
        let group = container
            .query_selector("[data-group-id='review']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        finish_agent_drag(&group);
        next_tick().await;
        assert_eq!(last_group_update_json(&calls)["kind"], "move_targets");
        assert!(state.active_agent.get_untracked().is_none());

        state
            .configured_hosts
            .update(|hosts| hosts.push(configured_host("offline", "Unavailable Host")));
        state.connection_statuses.update(|statuses| {
            statuses.insert(
                "offline".to_owned(),
                crate::state::ConnectionStatus::Disconnected,
            );
        });
        snapshot
            .pins
            .pinned
            .push(AgentAnnotationTarget::TransientAgent {
                host_id: HostFilterId("offline".to_owned()),
                agent_id: AgentId("absent".to_owned()),
            });
        let remote_beta = AgentAnnotationTarget::TransientAgent {
            host_id: HostFilterId("remote".to_owned()),
            agent_id: AgentId("beta-agent".to_owned()),
        };
        snapshot.pins.pinned.push(remote_beta.clone());
        notify(snapshot.clone());
        next_tick().await;
        let global = pinned_section().unwrap();
        let host_labels = global.query_selector_all(".agent-pin-host").unwrap();
        assert_eq!(
            host_labels.length(),
            2,
            "Each global pin identifies its host"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Unavailable Host"),
            "A saved pin never fabricates a disconnected host roster"
        );
        assert_eq!(
            host_labels.item(0).unwrap().text_content().as_deref(),
            Some("Local Host")
        );
        assert_eq!(
            host_labels.item(1).unwrap().text_content().as_deref(),
            Some("Remote Host")
        );
        let configured = state.configured_hosts.get_untracked();
        state
            .configured_hosts
            .update(|hosts| hosts.retain(|host| host.id != "remote"));
        next_tick().await;
        let missing_label = container
            .query_selector("[aria-label='Host label unavailable. Open host settings']")
            .unwrap()
            .expect("Missing canonical host labels must be an actionable error, not an ID fallback")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            missing_label.text_content().as_deref(),
            Some("Host label missing")
        );
        missing_label.click();
        next_tick().await;
        assert!(state.settings_open.get_untracked());
        assert_eq!(state.settings_tab_request.get_untracked(), Some("Hosts"));
        assert!(
            state.active_agent.get_untracked().is_none(),
            "Host-configuration error action must not open a chat"
        );
        state.settings_open.set(false);
        state.settings_tab_request.set(None);
        state.configured_hosts.set(configured);
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Host label unavailable. Open host settings']")
                .unwrap()
                .is_none()
        );
        for host in ["local", "remote"] {
            assert_eq!(
                container
                    .query_selector_all(&format!(
                        "[data-host-id='{host}'][data-agent-id='beta-agent']"
                    ))
                    .unwrap()
                    .length(),
                1
            );
            assert!(container.query_selector(&format!(".agent-sidebar-host-section [data-host-id='{host}'][data-agent-id='beta-agent']")).unwrap().is_none(), "Pins are never duplicated in ordinary groups");
        }
        for mode in ["Vibe", "Dinosaur"] {
            container
                .query_selector("button[aria-label='Interface mode']")
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
                .click();
            next_tick().await;
            container
                .query_selector(&format!("[role='menuitemradio'][aria-label='{mode}']"))
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
                .click();
            next_tick().await;
            assert_eq!(
                container
                    .query_selector_all("[aria-label='Pinned']")
                    .unwrap()
                    .length(),
                1
            );
            let global = pinned_section().unwrap();
            let host = container
                .query_selector(".agent-sidebar-host-section")
                .unwrap()
                .unwrap();
            assert!(
                global.get_bounding_client_rect().bottom() <= host.get_bounding_client_rect().top(),
                "All pins precede every host in either mode"
            );
            for host in ["remote", "local"] {
                let card = pin_button(host, "beta-agent")
                    .closest(".agent-card")
                    .unwrap()
                    .unwrap()
                    .dyn_into::<HtmlElement>()
                    .unwrap();
                card.click();
                next_tick().await;
                let active = state.active_agent.get_untracked().unwrap();
                assert_eq!(
                    active.host_id, host,
                    "Opening a global pin routes by host, not raw agent id"
                );
                assert_eq!(active.agent_id.0, "beta-agent");
                assert_eq!(card.get_attribute("aria-current").as_deref(), Some("true"));
                let other = if host == "local" { "remote" } else { "local" };
                assert_eq!(
                    pin_button(other, "beta-agent")
                        .closest(".agent-card")
                        .unwrap()
                        .unwrap()
                        .get_attribute("aria-current")
                        .as_deref(),
                    Some("false")
                );
            }
        }
        // Explicitly scoped unpin payloads still go to the canonical preferences owner.
        pin_button("remote", "beta-agent").click();
        next_tick().await;
        assert_eq!(
            last_pin_update().0,
            serde_json::json!({"kind":"unpin", "target":remote_beta})
        );
        assert_eq!(last_pin_update().1, "/host/local");
        assert!(
            pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Remote Beta"),
            "Do not relocate until the authoritative snapshot"
        );
        snapshot.pins.pinned.retain(|target| target != &remote_beta);
        notify(snapshot.clone());
        next_tick().await;
        assert!(
            !pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Remote Beta")
        );
        assert_eq!(container.query_selector_all(".agent-sidebar-host-section [data-host-id='remote'][data-agent-id='beta-agent']").unwrap().length(), 1);
        state.switch_active_project(None);
        next_tick().await;

        // Search narrows pins; automatic inactive/project/subagent hiding never hides explicit pins.
        let search: web_sys::HtmlInputElement = container
            .query_selector("input[placeholder='Filter agents...']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        search.set_value("Parent Alpha");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(pinned_section().is_none());
        search.set_value("");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        snapshot.sidebar.hide_inactive = true;
        notify(snapshot.clone());
        next_tick().await;
        assert!(pinned_section().is_some());
        snapshot.sidebar.hide_inactive = false;
        snapshot.sidebar.project_visibility = AgentsSidebarProjectVisibility::CurrentProjectOnly;
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("alpha".to_owned()),
        }));
        notify(snapshot.clone());
        next_tick().await;
        assert!(pinned_section().is_some());
        snapshot.sidebar.project_visibility = AgentsSidebarProjectVisibility::AllProjects;
        state.active_project.set(None);

        // Independently pinned children float out once, then return to their tree.
        notify(snapshot.clone());
        next_tick().await;
        pin_button("local", "child-alpha").click();
        next_tick().await;
        assert_eq!(
            last_pin_update().0,
            serde_json::json!({"kind":"pin", "target":local_target("child-alpha")})
        );
        pin_button("remote", "gamma-agent").click();
        next_tick().await;
        assert_eq!(
            last_pin_update().1,
            "/host/local",
            "remote targets mutate the canonical preferences host"
        );
        assert_eq!(last_pin_update().0["target"]["host_id"], "remote");

        snapshot.pins.pinned.push(local_target("child-alpha"));
        snapshot
            .pins
            .pinned
            .push(AgentAnnotationTarget::TransientAgent {
                host_id: HostFilterId("remote".to_owned()),
                agent_id: AgentId("gamma-agent".to_owned()),
            });
        notify(snapshot.clone());
        next_tick().await;
        assert!(
            pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Gamma Agent")
        );
        assert!(
            pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Child Alpha Agent")
        );
        assert_eq!(
            container
                .query_selector_all("[data-agent-id='child-alpha']")
                .unwrap()
                .length(),
            1
        );
        snapshot.sidebar.hide_sub_agents = true;
        notify(snapshot.clone());
        next_tick().await;
        assert!(
            pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Child Alpha Agent")
        );
        snapshot.sidebar.hide_sub_agents = false;
        snapshot
            .pins
            .pinned
            .retain(|target| target != &local_target("child-alpha"));
        notify(snapshot.clone());
        next_tick().await;
        let parent = agent_card_el(&container, "parent-alpha");
        parent
            .query_selector("button[title='Toggle sub-agents']")
            .unwrap()
            .expect("restored tree toggle")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            agent_card_el(&container, "child-alpha")
                .get_bounding_client_rect()
                .height(),
            0.0
        );
        snapshot.pins.pinned.push(local_target("parent-alpha"));
        notify(snapshot.clone());
        next_tick().await;
        assert!(
            pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Parent Alpha Agent")
        );
        assert_eq!(
            container
                .query_selector_all("[data-agent-id='child-alpha']")
                .unwrap()
                .length(),
            1
        );
        assert_eq!(
            agent_card_el(&container, "child-alpha")
                .get_bounding_client_rect()
                .height(),
            0.0,
            "parent collapse survives pinning"
        );

        // Session promotion changes the canonical target without losing the pin.
        state.agents.update(|agents| {
            for agent in agents
                .iter_mut()
                .filter(|agent| agent.agent_id.0 == "beta-agent")
            {
                agent.session_id = Some(protocol::SessionId("shared-session".to_owned()));
            }
        });
        let session_target = AgentAnnotationTarget::Session {
            host_id: HostFilterId("local".to_owned()),
            session_id: protocol::SessionId("shared-session".to_owned()),
        };
        snapshot.pins.pinned[0] = session_target.clone();
        snapshot.groups.assignments[0].target = session_target.clone();
        notify(snapshot.clone());
        next_tick().await;
        assert_eq!(
            pin_button("local", "beta-agent")
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            pin_button("remote", "beta-agent")
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("false")
        );
        pin_button("local", "beta-agent").click();
        next_tick().await;
        assert_eq!(
            last_pin_update().0,
            serde_json::json!({"kind":"unpin", "target":session_target})
        );
        assert!(state.active_agent.get_untracked().is_none());
        assert!(
            pinned_section()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Beta Agent"),
            "unpin also waits for the server"
        );
        // Change only pin state from the application's latest authoritative
        // snapshot, not the fixture's expected ownership assignments.
        let mut unpinned = state.agents_view_preferences.get_untracked();
        unpinned.pins.pinned.clear();
        notify(unpinned);
        next_tick().await;
        assert!(pinned_section().is_none());
        let custom = container
            .query_selector(".agent-sidebar-custom-group")
            .unwrap()
            .unwrap();
        assert!(
            custom.text_content().unwrap().contains("Beta Agent"),
            "unpin restores the custom group"
        );
        assert_eq!(
            container
                .query_selector_all(
                    ".agent-card-group[data-host-id='local'][data-agent-id='beta-agent']"
                )
                .unwrap()
                .length(),
            1
        );
        assert_eq!(
            agent_card_el(&container, "child-alpha")
                .get_bounding_client_rect()
                .height(),
            0.0
        );
        assert_eq!(
            state
                .agents
                .get_untracked()
                .iter()
                .find(|agent| agent.host_id == "local" && agent.agent_id.0 == "beta-agent")
                .unwrap()
                .project_id,
            Some(ProjectId("beta".to_owned()))
        );
        agent_card_el(&container, "beta-agent").click();
        next_tick().await;
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id,
            AgentId("beta-agent".to_owned())
        );
        assert_eq!(
            state.active_project.get_untracked().unwrap().project_id,
            ProjectId("beta".to_owned())
        );
    }

    /// Swarm members (and the agents they own) are replaced in the Agents list
    /// by one summary row per swarm, filed under the project the members ran
    /// in. The row's status follows member runtime status and the swarm
    /// lifecycle, folder counts only count rendered rows, and clicking the
    /// row opens the swarm.
    #[wasm_bindgen_test]
    async fn swarm_members_collapse_into_one_swarm_row() {
        use crate::components::swarm_view::wasm_tests::{make_swarm, member};
        use protocol::{AgentControlStatus, SwarmId, SwarmLifecycle, SwarmMemberState};

        let container = make_container();
        let state = make_app_state("local");
        state
            .configured_hosts
            .set(vec![configured_host("local", "Local Host")]);
        state.projects.set(vec![
            project_info("local", "alpha", "Alpha Project", 0),
            project_info("local", "bamboo", "Bamboo Project", 1),
        ]);
        push_agent_with_scope(
            &state,
            "local",
            "normal",
            "Normal Agent",
            true,
            Some("alpha"),
            None,
        );
        push_agent_with_scope(
            &state,
            "local",
            "fable",
            "Fable",
            true,
            Some("bamboo"),
            None,
        );
        push_agent_with_scope(&state, "local", "sol", "Sol", true, Some("bamboo"), None);
        push_agent_with_scope(
            &state,
            "local",
            "fable-helper",
            "Fable Helper",
            true,
            Some("bamboo"),
            Some("fable"),
        );
        state.agents.update(|agents| {
            for agent in agents.iter_mut() {
                match agent.agent_id.0.as_str() {
                    "fable" | "sol" => agent.origin = AgentOrigin::SwarmMember,
                    "fable-helper" => agent.origin = AgentOrigin::AgentControl,
                    _ => {}
                }
            }
        });
        let mut swarm = make_swarm(
            "bamboo-swarm",
            "Bamboo Crew",
            vec![
                member(
                    "fable",
                    "Fable",
                    SwarmMemberState::Live,
                    Some("fable"),
                    Some(AgentControlStatus::Thinking),
                ),
                member(
                    "sol",
                    "Sol",
                    SwarmMemberState::Live,
                    Some("sol"),
                    Some(AgentControlStatus::Idle),
                ),
            ],
        );
        swarm.constraints.project_id = ProjectId("bamboo".to_owned());
        let swarm_id = swarm.id.clone();
        state.swarms.set(HashMap::from([(
            "local".to_owned(),
            HashMap::from([(swarm_id.clone(), swarm)]),
        )]));
        let set_swarm = |update: &dyn Fn(&mut protocol::Swarm)| {
            state.swarms.update(|by_host| {
                update(
                    by_host
                        .get_mut("local")
                        .and_then(|swarms| swarms.get_mut(&SwarmId("bamboo-swarm".to_owned())))
                        .unwrap(),
                )
            });
        };

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Normal Agent"),
            "the normal agent stays listed; got {text:?}"
        );
        for hidden in ["Fable", "Sol", "Fable Helper"] {
            assert!(
                !text.contains(hidden),
                "{hidden:?} belongs to a swarm and must not be in the Agents list; got {text:?}"
            );
        }
        let bamboo: HtmlElement = container
            .query_selector("[data-project-key='local:bamboo']")
            .unwrap()
            .expect("the swarm's project section renders")
            .dyn_into()
            .unwrap();
        let rows = bamboo.query_selector_all("button").unwrap();
        let row: HtmlElement = (0..rows.length())
            .map(|index| rows.item(index).unwrap().dyn_into::<HtmlElement>().unwrap())
            .find(|row| {
                row.text_content()
                    .unwrap_or_default()
                    .contains("Bamboo Crew")
            })
            .expect("the swarm summary row renders under its project");
        let row_text = || row.text_content().unwrap_or_default();
        assert!(
            row_text().contains("Working") && row_text().contains("2 agents · 1 working"),
            "a working member makes the swarm working; got {:?}",
            row_text()
        );
        let counts: Vec<String> = {
            let nodes = container.query_selector_all(".agent-folder-count").unwrap();
            (0..nodes.length())
                .map(|index| {
                    nodes
                        .item(index)
                        .unwrap()
                        .text_content()
                        .unwrap_or_default()
                })
                .collect()
        };
        assert_eq!(
            counts,
            vec!["1".to_owned(), "1".to_owned()],
            "Alpha counts its one agent and Bamboo its one swarm row, not the hidden members"
        );

        set_swarm(&|swarm| swarm.members[0].runtime_status = Some(AgentControlStatus::Idle));
        next_tick().await;
        assert!(
            row_text().contains("Idle") && row_text().contains("2 agents · 0 working"),
            "with no working member the swarm reads idle; got {:?}",
            row_text()
        );
        set_swarm(&|swarm| swarm.members[1].runtime_status = Some(AgentControlStatus::Thinking));
        next_tick().await;
        assert!(
            row_text().contains("Working") && row_text().contains("2 agents · 1 working"),
            "the row flips back to working; got {:?}",
            row_text()
        );
        set_swarm(&|swarm| swarm.lifecycle = SwarmLifecycle::Paused);
        next_tick().await;
        assert!(
            row_text().contains("Paused") && !row_text().contains("Working"),
            "a paused swarm reads paused; got {:?}",
            row_text()
        );
        set_swarm(&|swarm| swarm.lifecycle = SwarmLifecycle::AttentionRequired);
        next_tick().await;
        assert!(
            row_text().contains("Needs attention"),
            "the swarm lifecycle surfaces needs-attention; got {:?}",
            row_text()
        );

        row.click();
        next_tick().await;
        assert_eq!(
            state
                .center_zone
                .with_untracked(|cz| cz.active_content().cloned()),
            Some(TabContent::Swarm {
                host_id: "local".to_owned(),
                swarm_id,
            }),
            "clicking the swarm row opens the swarm"
        );
        state.projects.update(|projects| {
            for id in ["bamboo-bench", "bamboo-sibling"] {
                let mut workbench = project_info("local", id, id, 2);
                workbench.project.source = ProjectSource::GitWorkbench {
                    environment: Default::default(),
                    parent_project_id: ProjectId("bamboo".to_owned()),
                    branch: protocol::GitBranchName(id.to_owned()),
                    roots: vec![],
                };
                projects.push(workbench);
            }
            projects.push(project_info("remote", "bamboo-bench", "Remote bench", 0));
        });
        set_swarm(&|swarm| swarm.constraints.project_id = ProjectId("bamboo-bench".to_owned()));
        state.swarms.update(|by_host| {
            let mut remote = by_host["local"][&SwarmId("bamboo-swarm".to_owned())].clone();
            remote.name = "Remote Crew".to_owned();
            by_host.insert(
                "remote".to_owned(),
                HashMap::from([(remote.id.clone(), remote)]),
            );
        });
        for _ in 0..4 {
            next_tick().await;
        }
        let bamboo = container
            .query_selector("[data-project-key='local:bamboo']")
            .unwrap()
            .unwrap();
        assert!(bamboo.text_content().unwrap().contains("Bamboo Crew"));
        assert!(!bamboo.text_content().unwrap().contains("Remote Crew"));
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.sidebar.show_workbenches = true;
        state.agents_view_preferences.set(preferences);
        next_tick().await;
        // Explicit grouping preserves the owning parent and exact launch target.
        assert!(
            container
                .query_selector("[data-project-key='local:bamboo-bench']")
                .unwrap()
                .is_none()
        );
        assert!(
            bamboo
                .query_selector(
                    "[data-project-id='bamboo-bench'] [aria-label='New agent in bamboo-bench']"
                )
                .unwrap()
                .is_some()
        );
        assert!(container.text_content().unwrap().contains("Remote bench"));
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.sidebar.project_visibility = AgentsSidebarProjectVisibility::CurrentProjectOnly;
        state.agents_view_preferences.set(preferences);
        for selected in ["bamboo", "bamboo-bench", "bamboo-sibling"] {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "local".to_owned(),
                project_id: ProjectId(selected.to_owned()),
            }));
            for _ in 0..4 {
                next_tick().await;
            }
            let text = container.text_content().unwrap();
            assert!(
                text.contains("Bamboo Project") && text.contains("Bamboo Crew"),
                "parent and sibling selections retain the swarm under its parent"
            );
            assert!(!text.contains("Normal Agent") && !text.contains("Remote Crew"));
        }

        // Recovery retains swarm conversations but clears live agent handles.
        state
            .agents
            .update(|agents| agents.retain(|agent| agent.agent_id.0 == "normal"));
        state.swarms.update(|hosts| {
            for swarms in hosts.values_mut() {
                for swarm in swarms.values_mut() {
                    swarm.lifecycle = SwarmLifecycle::Paused;
                    for member in &mut swarm.members {
                        member.state = SwarmMemberState::Dormant;
                        member.session_id =
                            Some(protocol::SessionId(format!("saved-{}", member.spec.id)));
                        member.agent_id = None;
                        member.runtime_status = None;
                    }
                }
            }
        });
        state.active_project.set(None);
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.sidebar.project_visibility = AgentsSidebarProjectVisibility::AllProjects;
        preferences.sidebar.hide_empty_projects = true;
        state.agents_view_preferences.set(preferences);
        next_tick().await;
        let bamboo = container
            .query_selector("[data-project-key='local:bamboo']")
            .unwrap()
            .expect("A saved swarm keeps its parent project even without live agents");
        let saved_swarm = bamboo
            .query_selector("[data-project-id='bamboo-bench'] [data-swarm-id='bamboo-swarm']")
            .unwrap()
            .expect("A saved swarm keeps its exact workbench conversation row");
        let text = saved_swarm.text_content().unwrap();
        assert!(
            text.contains("Bamboo Crew") && text.contains("Paused") && text.contains("2 agents")
        );
        assert!(
            container
                .query_selector("[data-project-id='bamboo-sibling']")
                .unwrap()
                .is_none(),
            "A genuinely empty sibling workbench is still hidden"
        );
        let search = container
            .query_selector("input[placeholder='Filter agents...']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        search.set_value("no swarm matches this search");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-project-key='local:bamboo']")
                .unwrap()
                .is_some(),
            "Filtering out a saved swarm does not make its project empty"
        );
        state.swarms.update(|hosts| {
            hosts.get_mut("local").unwrap().clear();
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-project-key='local:bamboo']")
                .unwrap()
                .is_none(),
            "Another host's same-ID swarm cannot keep an empty local project visible"
        );
        assert!(
            container
                .query_selector("[data-project-key='remote:bamboo-bench']")
                .unwrap()
                .is_some()
        );
    }

    #[wasm_bindgen_test]
    async fn folder_collapse_survives_project_switch_rename_and_remount() {
        let storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
        storage
            .remove_item("tyde.agents.collapsed-folders.v1")
            .unwrap();
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(
            &state,
            assigned_group("collapse-review", "Review Group", &["beta-agent"]),
        );
        let handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let toggle = |label: &str| -> HtmlElement {
            container
                .query_selector(&format!("button[aria-label='{label}'][aria-expanded]"))
                .unwrap()
                .expect("folder has an accessible disclosure button")
                .dyn_into()
                .unwrap()
        };
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Parent Alpha Agent")
        );
        assert!(container.text_content().unwrap().contains("Beta Agent"));
        toggle("Project: Alpha Project").click();
        toggle("Review Group").click();
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            toggle("Project: Alpha Project")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );
        assert_eq!(
            toggle("Review Group")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );
        let text = container.text_content().unwrap();
        assert!(!text.contains("Parent Alpha Agent") && !text.contains("Child Alpha Agent"));
        assert!(!text.contains("Beta Agent"));
        assert!(
            text.contains("Gamma Agent"),
            "other folders remain expanded"
        );

        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("alpha".to_owned()),
        }));
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.sidebar.project_visibility = AgentsSidebarProjectVisibility::CurrentProjectOnly;
        state.agents_view_preferences.set(preferences);
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            toggle("Project: Alpha Project")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Parent Alpha Agent")
        );
        assert!(
            toggle("Project: Alpha Project")
                .get_bounding_client_rect()
                .height()
                > 0.0,
            "a view containing only collapsed agents still exposes its heading"
        );
        state.active_project.set(None);
        apply_group_snapshot(
            &state,
            assigned_group("collapse-review", "Renamed Review", &["beta-agent"]),
        );
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            toggle("Renamed Review")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );
        drop(handle);
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            toggle("Project: Alpha Project")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );
        assert_eq!(
            toggle("Renamed Review")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );
        assert!(!container.text_content().unwrap().contains("Beta Agent"));
        toggle("Project: Alpha Project").click();
        toggle("Renamed Review").click();
        for _ in 0..4 {
            next_tick().await;
        }
        let text = container.text_content().unwrap();
        assert!(
            text.contains("Parent Alpha Agent")
                && text.contains("Child Alpha Agent")
                && text.contains("Beta Agent")
        );
        storage
            .remove_item("tyde.agents.collapsed-folders.v1")
            .unwrap();
    }

    #[wasm_bindgen_test]
    async fn agent_pointer_drag_keeps_one_row_and_exact_group_targets() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("local");
        seed_sidebar_group_fixture(&state);
        apply_group_snapshot(&state, AgentGroupsSnapshot::default());
        state.agents_view_preferences.update(|snapshot| {
            snapshot.sidebar.project_visibility = AgentsSidebarProjectVisibility::AllProjects
        });
        push_agent_with_scope(
            &state,
            "remote",
            "parent-alpha",
            "Other-host parent",
            true,
            Some("gamma"),
            None,
        );
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;
        assert!(
            container
                .query_selector(".agent-ungroup-drop-target")
                .unwrap()
                .is_none(),
            "Ungroup stays hidden when there are no custom groups"
        );
        let beta = agent_card_el(&container, "beta-agent");
        let name = beta.query_selector(".agent-card-name").unwrap().unwrap();
        let rect = name.get_bounding_client_rect();
        let x = rect.left() as i32 + 2;
        let y = (rect.top() + rect.height() / 2.0) as i32;
        pointer_event(&name, "pointerdown", x, y);
        pointer_event(&name, "pointermove", x + 3, y + 3);
        next_tick().await;
        let document = container.owner_document().unwrap();
        assert!(
            document
                .query_selector(".agent-row-drag-preview")
                .unwrap()
                .is_none(),
            "Small pointer motion is still an ordinary click"
        );
        pointer_event(&name, "pointerup", x + 3, y + 3);
        name.dyn_ref::<HtmlElement>().unwrap().click();
        next_tick().await;
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id.0,
            "beta-agent"
        );
        assert!(
            recorded_frames(&calls)
                .iter()
                .all(|(kind, _, _)| kind != "set_agent_groups")
        );

        let parent = agent_card_el(&container, "parent-alpha");
        open_row_menu(&parent).await;
        menu_item(&container, "Rename agent").click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".agent-card-name-input")
                .unwrap()
                .length(),
            1,
            "Only the intended host's row enters inline editing"
        );
        let input = parent
            .query_selector(".agent-card-name-input")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            document.active_element().unwrap(),
            input.clone().dyn_into::<web_sys::Element>().unwrap()
        );
        dispatch_key(&input, "Escape");
        next_tick().await;
        begin_agent_drag(&parent);
        next_tick().await;
        assert!(
            container
                .query_selector(".agent-ungroup-drop-target")
                .unwrap()
                .is_none(),
            "A held drag cannot reveal Ungroup without custom groups"
        );
        let preview = document
            .query_selector(".agent-row-drag-preview")
            .unwrap()
            .expect("one held preview");
        assert_eq!(
            document
                .query_selector_all(".agent-row-drag-preview")
                .unwrap()
                .length(),
            1
        );
        let preview_el = preview.dyn_ref::<HtmlElement>().unwrap();
        assert_eq!(
            preview_el
                .query_selector(".agent-card-name")
                .unwrap()
                .unwrap()
                .dyn_ref::<HtmlElement>()
                .unwrap()
                .inner_text(),
            "Parent Alpha Agent"
        );
        let left_of = |selector: &str| {
            preview
                .query_selector(selector)
                .unwrap()
                .unwrap()
                .get_bounding_client_rect()
                .left()
        };
        assert!(
            left_of(".agent-card-status") < left_of(".agent-card-name")
                && left_of(".agent-card-name") < left_of(".agent-backend-glyph"),
            "the drag preview leads with activity and trails the backend glyph"
        );
        let size = preview.get_bounding_client_rect();
        assert!(
            size.height() <= 36.0 && size.width() <= 242.0,
            "Expanded metadata and child rows must not enter the preview"
        );
        let style = web_sys::window()
            .unwrap()
            .get_computed_style(&preview)
            .unwrap()
            .unwrap();
        assert_eq!(style.get_property_value("opacity").unwrap(), "1");
        assert!(
            !style
                .get_property_value("background-color")
                .unwrap()
                .contains("rgba"),
            "Preview background is opaque"
        );
        assert_eq!(style.get_property_value("pointer-events").unwrap(), "none");
        let assert_drag_status = |label: &str, glyph: &str| {
            let row = agent_card_el(&container, "parent-alpha");
            let preview = document
                .query_selector(".agent-row-drag-preview")
                .unwrap()
                .expect("held drag stays mounted across status changes");
            for surface in [row.as_ref(), &preview] {
                let status = surface
                    .query_selector(".agent-card-status")
                    .unwrap()
                    .unwrap();
                assert_eq!(status.get_attribute("title").as_deref(), Some(label));
                assert_eq!(
                    status
                        .query_selector("[aria-hidden='true']")
                        .unwrap()
                        .unwrap()
                        .text_content()
                        .as_deref(),
                    Some(glyph),
                    "row and drag preview must agree on live activity"
                );
            }
        };
        assert_drag_status("Idle", "\u{2713}");
        let dragged_id = AgentId("parent-alpha".to_owned());
        let apply = |event| {
            crate::dispatch::apply_chat_event(&state, "local", &dragged_id, event);
        };
        apply(protocol::ChatEvent::ToolProgress(
            protocol::ToolProgressData {
                tool_call_id: "drag-background".to_owned(),
                execution_mode: protocol::ToolExecutionMode::Background,
                cancellable: false,
                update: protocol::ToolProgressUpdate::Other {
                    payload: serde_json::json!({
                        "task_id": "drag-task",
                        "description": "Held background work",
                    }),
                },
            },
        ));
        next_tick().await;
        assert_drag_status("Background work", "\u{29D7}");
        apply(protocol::ChatEvent::TypingStatusChanged(true));
        next_tick().await;
        assert_drag_status("Thinking", "\u{25F7}");
        state.agent_awaiting_user.update(|awaiting| {
            awaiting.insert(dragged_id.clone());
        });
        next_tick().await;
        assert_drag_status("Needs your answer", "?");
        state.agent_awaiting_user.update(|awaiting| {
            awaiting.remove(&dragged_id);
        });
        apply(protocol::ChatEvent::OperationCancelled(
            protocol::OperationCancelledData {
                message: "Operation cancelled".to_owned(),
            },
        ));
        next_tick().await;
        assert_drag_status("Cancelled", "\u{2298}");
        apply(protocol::ChatEvent::TypingStatusChanged(true));
        next_tick().await;
        assert_drag_status("Thinking", "\u{25F7}");
        apply(protocol::ChatEvent::TypingStatusChanged(false));
        next_tick().await;
        assert_drag_status("Background work", "\u{29D7}");
        apply(protocol::ChatEvent::ToolExecutionCompleted(
            crate::components::tool_card::test_utils::succeeded_completion(
                "drag-background",
                protocol::ToolExecutionResult::RunCommand {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ),
        ));
        next_tick().await;
        assert_drag_status("Idle", "\u{2713}");
        dispatch_key(&parent, "Escape");
        next_tick().await;
        assert!(
            document
                .query_selector(".agent-row-drag-preview")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id.0,
            "beta-agent",
            "Cancelling a move cannot open its source"
        );

        begin_agent_drag(&beta);
        finish_agent_drag(&parent);
        next_tick().await;
        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "create_group");
        assert_eq!(
            update["targets"],
            serde_json::json!([local_target("beta-agent"), local_target("parent-alpha")])
        );
        assert!(update["name"].as_str().unwrap().contains("Beta Agent"));
        assert!(
            document
                .query_selector(".agent-row-drag-preview")
                .unwrap()
                .is_none()
        );

        let agents_before_removal = state.agents.get_untracked();
        let command_count = recorded_frames(&calls)
            .iter()
            .filter(|(kind, _, _)| kind == "set_agent_groups")
            .count();
        begin_agent_drag(&beta);
        state.agents.update(|agents| {
            agents.retain(|agent| !(agent.host_id == "local" && agent.agent_id.0 == "parent-alpha"))
        });
        finish_agent_drag(&parent);
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Destination agent is no longer available. Choose another destination.")
        );
        assert_eq!(
            recorded_frames(&calls)
                .iter()
                .filter(|(kind, _, _)| kind == "set_agent_groups")
                .count(),
            command_count
        );
        state.agents.set(agents_before_removal);
        next_tick().await;

        apply_group_snapshot(&state, assigned_group("review", "Review", &["beta-agent"]));
        state.agents_view_preferences.update(|snapshot| {
            snapshot.sidebar.project_visibility = AgentsSidebarProjectVisibility::AllProjects
        });
        next_tick().await;
        let parent = agent_card_el(&container, "parent-alpha");
        let group = container
            .query_selector("[data-group-id='review']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        begin_agent_drag(&parent);
        finish_agent_drag(&group);
        next_tick().await;
        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "move_targets");
        assert_eq!(update["group_id"], "review");
        assert_eq!(
            update["targets"],
            serde_json::json!([local_target("parent-alpha")])
        );
        assert!(
            state
                .agents
                .with_untracked(|agents| agents.iter().any(|agent| agent.agent_id.0
                    == "child-alpha"
                    && agent
                        .parent_agent_id
                        .as_ref()
                        .is_some_and(|id| id.0 == "parent-alpha"))),
            "Moving the displayed parent leaves child lifecycle ownership intact"
        );
        for index in 0..12 {
            push_agent_with_scope(
                &state,
                "local",
                &format!("edge-{index}"),
                &format!("Edge member {index}"),
                true,
                Some("alpha"),
                None,
            );
        }
        next_tick().await;
        let scroll = container
            .query_selector(".panel-content")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        scroll
            .set_attribute("style", "flex:none;height:180px;overflow:auto")
            .unwrap();
        scroll.set_scroll_top(0);
        let group = container
            .query_selector("[data-group-id='review'] > .agent-sidebar-custom-group-header")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        let viewport = scroll.get_bounding_client_rect();
        assert!(
            group.get_bounding_client_rect().top() > viewport.bottom(),
            "The destination starts below the visible list"
        );
        js_sys::eval("window.__agentFrames = new Map(); window.__agentFrameId = 0; window.__agentOriginalRaf = window.requestAnimationFrame; window.__agentOriginalCancel = window.cancelAnimationFrame; window.requestAnimationFrame = callback => { const id = ++window.__agentFrameId; window.__agentFrames.set(id, callback); return id; }; window.cancelAnimationFrame = id => window.__agentFrames.delete(id); window.__agentTick = () => { const callbacks = [...window.__agentFrames.values()]; window.__agentFrames.clear(); callbacks.forEach(callback => callback(0)); };").unwrap();
        struct FrameClock;
        impl Drop for FrameClock {
            fn drop(&mut self) {
                js_sys::eval("window.requestAnimationFrame = window.__agentOriginalRaf; window.cancelAnimationFrame = window.__agentOriginalCancel; delete window.__agentFrames; delete window.__agentTick; delete window.__agentOriginalRaf; delete window.__agentOriginalCancel; delete window.__agentFrameId;").unwrap();
            }
        }
        let clock = FrameClock;
        let before_edge_commands = recorded_frames(&calls)
            .iter()
            .filter(|(kind, _, _)| kind == "set_agent_groups")
            .count();
        let parent = agent_card_el(&container, "parent-alpha");
        begin_agent_drag(&parent);
        pointer_event(
            &parent,
            "pointermove",
            (viewport.right() - 8.0) as i32,
            (viewport.bottom() - 2.0) as i32,
        );
        next_tick().await;
        let mut visible = false;
        for _ in 0..(scroll.scroll_height() / 12 + 2) {
            js_sys::eval("window.__agentTick()").unwrap();
            let rect = group.get_bounding_client_rect();
            if rect.top() >= viewport.top() && rect.bottom() <= viewport.bottom() {
                visible = true;
                break;
            }
        }
        assert!(
            visible && scroll.scroll_top() > 0,
            "Held edge scrolling reveals the destination without a wheel or real-time wait"
        );
        finish_agent_drag(&group);
        next_tick().await;
        assert_eq!(
            recorded_frames(&calls)
                .iter()
                .filter(|(kind, _, _)| kind == "set_agent_groups")
                .count(),
            before_edge_commands + 1,
            "The offscreen drop must submit a new command"
        );
        let update = last_group_update_json(&calls);
        assert_eq!(update["kind"], "move_targets");
        assert_eq!(update["group_id"], "review");
        assert_eq!(
            update["targets"],
            serde_json::json!([local_target("parent-alpha")])
        );
        let settled_scroll = scroll.scroll_top();
        js_sys::eval("window.__agentTick()").unwrap();
        assert_eq!(
            scroll.scroll_top(),
            settled_scroll,
            "Dropping cancels the scroll frame"
        );
        drop(clock);
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

        // A keyboard user picks a row up from its actions menu.
        pick_up_for_keyboard_move(&container, "beta-agent").await;
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

        // A keyboard user picks a row up from its actions menu.
        pick_up_for_keyboard_move(&container, "beta-agent").await;
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
        // Start a default-tree agent through its row body.
        let parent_card: HtmlElement = container
            .query_selector("[data-agent-id='parent-alpha'] .agent-card")
            .unwrap()
            .expect("parent card")
            .dyn_into()
            .unwrap();
        begin_agent_drag(&parent_card);
        for _ in 0..4 {
            next_tick().await;
        }
        assert_eq!(
            count(&container),
            1,
            "an active drag with groups present should reveal exactly one Ungroup target"
        );

        // Ending the drag tears the target back down.
        cancel_agent_drag(&parent_card);
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
        pick_up_for_keyboard_move(&container_no_groups, "beta-agent").await;
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

    /// An idle agent on a connected host whose backend has declared a
    /// compaction route exposes an enabled Compact action.
    ///
    /// Evidence for the change: the control now renders
    /// `"Compact context — unavailable: Still checking what this backend
    /// supports"` when no capability snapshot exists, because the gate fails
    /// closed. The labelled-affordance contract is unchanged and is asserted
    /// more precisely below — label *and* enabled state, rather than label
    /// alone.
    #[wasm_bindgen_test]
    async fn compact_button_enabled_when_idle_connected_and_capability_declared() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-idle", "Agent", true);
        seed_chat_row(&state, "a-idle");

        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        // Capability unknown → visible, disabled, and it says why.
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            !enabled,
            "unknown capability must not optimistically enable"
        );
        assert!(
            label.contains("unavailable:"),
            "and the reason must be on the accessible name: {label}"
        );

        declare_compaction_available(&state, "a-idle");
        for _ in 0..4 {
            next_tick().await;
        }

        let (btn, enabled, label) = compact_btn_state(&container).await;
        assert!(enabled, "a declared-available idle agent is compactable");
        assert_eq!(
            label, "Compact context",
            "compact button must keep a labelled affordance"
        );
        assert!(
            !btn.has_attribute("disabled"),
            "and stays focusable, so the reason is reachable without a pointer"
        );
    }

    /// A BTW fork is a stand-alone top-level chat: the agent it was forked
    /// from does not own it, so the sidebar must not nest it as a child or
    /// count it against that agent. The owned agent in the same fixture is
    /// the control — it proves the panel still nests when there really is a
    /// parent, so this is not passing for want of nesting support.
    #[wasm_bindgen_test]
    async fn forked_agent_renders_as_top_level_row() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-source", "Source", true);
        push_agent_with_scope(
            &state,
            "h",
            "a-owned",
            "Owned",
            true,
            None,
            Some("a-source"),
        );
        push_agent(&state, "h", "a-btw", "BTW: why is this slow?", true);
        state.agent_turn_active.update(|turns| {
            turns.insert(AgentId("a-owned".into()), true);
            turns.insert(AgentId("a-btw".into()), true);
        });
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let child_names = container
            .query_selector_all(".agent-card-child")
            .unwrap()
            .length();
        assert_eq!(
            child_names, 1,
            "only the genuinely owned agent may render as a nested child row"
        );
        let nested = container
            .query_selector(".agent-card-child")
            .unwrap()
            .expect("the owned agent renders nested, proving nesting is live")
            .text_content()
            .unwrap_or_default();
        assert!(
            nested.contains("Owned"),
            "the nested row must be the owned agent, not the fork; got {nested:?}"
        );
        assert!(
            !nested.contains("BTW"),
            "the fork must never render inside another agent's child list; got {nested:?}"
        );
        let counted = container
            .query_selector(".agent-card-collapse-toggle")
            .unwrap()
            .expect("the owning agent still shows its child count")
            .text_content()
            .unwrap_or_default();
        assert_eq!(
            counted.trim(),
            "1 running ▸",
            "the fork must not be counted as a child of the agent it forked from"
        );
    }

    /// Initializing (server hasn't echoed AgentStart) — Compact must be
    /// refused so the user can't fire a request before the agent is ready.
    ///
    /// The refusal is now *visible and explained* rather than hidden: a control
    /// that disappears teaches the user it does not exist, and this blocker is
    /// transient. The guarantee (no request while initializing) is unchanged.
    #[wasm_bindgen_test]
    async fn compact_button_disabled_when_initializing() {
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
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            !enabled,
            "compact must be refused while the agent is still initializing"
        );
        assert!(
            label.contains("still starting"),
            "and must say so rather than vanishing: {label}"
        );
    }

    /// A running turn is **not** a refusal any more.
    ///
    /// The server defers a compaction request to a safe point and reports it
    /// deferred, so a client-side idle gate refused requests the server
    /// accepts. This test now pins the inverted contract deliberately: mid-turn
    /// and declared-available, the control is offered.
    #[wasm_bindgen_test]
    async fn compact_button_stays_offered_while_the_agent_is_thinking() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-thinking", "Agent", true);
        seed_chat_row(&state, "a-thinking");
        state.agent_turn_active.update(|m| {
            m.insert(AgentId("a-thinking".to_owned()), true);
        });
        declare_compaction_available(&state, "a-thinking");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render for the thinking agent"
        );
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            enabled,
            "an active turn must not refuse a request the server would defer: {label}"
        );
    }

    /// An attached agent learns it is waiting on the user from the server's
    /// live `AgentActivityChanged` frame on its own stream. Before that
    /// state existed, an agent blocked on a question rendered "✓ Idle",
    /// identical to one that had finished.
    #[wasm_bindgen_test]
    async fn live_activity_frames_render_awaiting_user_distinctly() {
        let container = make_container();
        let state = make_app_state("h-await");
        reset_inbound_seqs(&state, "h-await");
        register_agent_via_new_agent(&state, "h-await", "a-await", "Agent", 0, 0);
        let handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        let status = agent_card_el(&container, "a-await")
            .query_selector(".agent-card-status")
            .unwrap()
            .expect("agent row status");
        for (seq, activity, label) in [
            (0, protocol::AgentActivity::Thinking, "Thinking"),
            (
                1,
                protocol::AgentActivity::AwaitingUser,
                "Needs your answer",
            ),
            (2, protocol::AgentActivity::Thinking, "Thinking"),
            (
                3,
                protocol::AgentActivity::AwaitingUser,
                "Needs your answer",
            ),
            (4, protocol::AgentActivity::Idle, "Idle"),
        ] {
            dispatch_frame(
                &state,
                "h-await",
                agent_stream("a-await"),
                FrameKind::AgentActivityChanged,
                seq * 2,
                &protocol::AgentActivityChangedPayload { activity },
            );
            dispatch_frame(
                &state,
                "h-await",
                agent_stream("a-await"),
                FrameKind::ChatEvent,
                seq * 2 + 1,
                &protocol::ChatEvent::TypingStatusChanged(
                    activity != protocol::AgentActivity::Idle,
                ),
            );
            next_tick().await;
            assert_eq!(status.get_attribute("title").as_deref(), Some(label));
            let text = status.text_content().unwrap_or_default();
            assert!(
                text.contains(label),
                "status must read {label}, got {text:?}"
            );
            if activity == protocol::AgentActivity::AwaitingUser {
                assert!(
                    text.contains('?'),
                    "human attention must outrank live typing"
                );
            }
            assert_eq!(
                text.contains('\u{2713}'),
                activity == protocol::AgentActivity::Idle,
                "only an idle agent shows the completed check, got {text:?}"
            );
        }
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn restored_turn_status_uses_host_frames_without_loading_chat() {
        use crate::dispatch::restore_fixtures::{restore_agent_payload, restore_bootstrap};

        for host_bootstrap in [true, false] {
            let container = make_container();
            let state = make_app_state("h");
            // Each connection starts at zero; the validator outlives AppState.
            crate::dispatch::prime_host_for_tests(&state, "h");
            let mut agent = restore_agent_payload("a-restored", None);
            agent.backend_kind = BackendKind::Codex;
            agent.session_id = Some(protocol::SessionId("restored-session".to_owned()));
            agent.activity = protocol::AgentActivity::Thinking;
            if host_bootstrap {
                dispatch_frame(
                    &state,
                    "h",
                    StreamPath("/host/h".to_owned()),
                    FrameKind::HostBootstrap,
                    0,
                    &restore_bootstrap(
                        vec![BackendKind::Codex],
                        Default::default(),
                        vec![],
                        vec![agent],
                    ),
                );
            } else {
                dispatch_frame(
                    &state,
                    "h",
                    StreamPath("/host/h".to_owned()),
                    FrameKind::NewAgent,
                    0,
                    &agent,
                );
            }
            let handle = mount_panel(&container, state.clone());
            for _ in 0..4 {
                next_tick().await;
            }
            wasm_bindgen_test::console_log!(
                "Restored status: host_bootstrap={}, agents={}, connection={:?}",
                host_bootstrap,
                state.agents.get_untracked().len(),
                state.connection_statuses.get_untracked().get("h"),
            );
            let status = agent_card_el(&container, "a-restored")
                .query_selector(".agent-card-status")
                .unwrap()
                .expect("restored row status");
            assert_eq!(status.get_attribute("title").as_deref(), Some("Thinking"));
            assert!(status.text_content().unwrap().contains("Thinking"));
            // A member waiting on the user is neither thinking nor done, and
            // must not render the completed check.
            for (seq, activity, label) in [
                (1, protocol::AgentActivity::Idle, "Idle"),
                (2, protocol::AgentActivity::Thinking, "Thinking"),
                (
                    3,
                    protocol::AgentActivity::AwaitingUser,
                    "Needs your answer",
                ),
                (4, protocol::AgentActivity::Idle, "Idle"),
            ] {
                dispatch_frame(
                    &state,
                    "h",
                    StreamPath("/host/h".to_owned()),
                    FrameKind::AgentTurnStateNotify,
                    seq,
                    &protocol::AgentTurnStateNotifyPayload {
                        agent_id: AgentId("a-restored".to_owned()),
                        activity,
                    },
                );
                next_tick().await;
                assert_eq!(status.get_attribute("title").as_deref(), Some(label));
                assert!(status.text_content().unwrap().contains(label));
                assert_eq!(
                    status.text_content().unwrap().contains('\u{2713}'),
                    activity == protocol::AgentActivity::Idle,
                    "only an idle agent shows the completed check ({label})"
                );
            }
            drop(handle);
            container.remove();
        }
    }

    #[wasm_bindgen_test]
    async fn status_exposes_text_and_hides_decorative_glyph() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-accessible", "Agent", true);
        state.agent_turn_active.update(|map| {
            map.insert(AgentId("a-accessible".to_owned()), true);
        });
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let status = container
            .query_selector(".agent-card-status")
            .unwrap()
            .expect("status present");
        assert_eq!(status.get_attribute("title").as_deref(), Some("Thinking"));
        let text = status
            .query_selector(".visually-hidden")
            .unwrap()
            .expect("accessible status text present");
        assert_eq!(text.text_content().as_deref(), Some("Thinking"));
        let glyph = status
            .query_selector("[aria-hidden='true']")
            .unwrap()
            .expect("decorative glyph present");
        assert_eq!(glyph.get_attribute("aria-hidden").as_deref(), Some("true"));
    }

    /// Loaded chat rows are **not** a capability signal.
    ///
    /// The old gate required at least one loaded row, which was wrong rather
    /// than merely conservative: a restored session that renders only the
    /// collapsed-history banner has zero loaded rows, so the control hid on
    /// exactly the long sessions that most need compacting. Availability is the
    /// server's to declare.
    #[wasm_bindgen_test]
    async fn compact_button_does_not_depend_on_loaded_chat_rows() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-blank", "Agent", true);
        declare_compaction_available(&state, "a-blank");
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render even with no chat rows"
        );
        assert!(
            state
                .chat_rows
                .with_untracked(|map| map.get(&AgentId("a-blank".to_owned())).is_none()),
            "precondition: this agent has no loaded rows"
        );
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            enabled,
            "an agent with no loaded rows is still compactable when declared available: {label}"
        );
    }

    /// Disconnected host — Compact must be refused because the request can't
    /// reach the server. Visible and explained rather than hidden.
    #[wasm_bindgen_test]
    async fn compact_button_disabled_when_host_disconnected() {
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
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            !enabled,
            "compact must be refused when the host is disconnected"
        );
        assert!(
            label.contains("not connected"),
            "and must name the blocker: {label}"
        );
    }

    /// Already compacting — the user must not be able to double-fire, and the
    /// status pill must render the running-blue style used for in-flight work.
    ///
    /// Both guarantees are unchanged; only the presentation of the first moved
    /// from hidden to visible-and-disabled. This case covers the **legacy
    /// replacement** protocol, whose in-flight flag the shared gate must honour
    /// as well as its own — otherwise the two protocols each guard only
    /// themselves and a legacy compaction leaves this control armed.
    #[wasm_bindgen_test]
    async fn compacting_state_disables_button_and_shows_running_pill() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-busy", "Agent", true);
        seed_chat_row(&state, "a-busy");
        // Declared available, so capability is not what disables it here.
        declare_compaction_available(&state, "a-busy");
        state.mark_compaction_started("h", AgentId("a-busy".to_owned()));
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            !enabled,
            "compact must be refused once a compaction is in flight"
        );
        assert!(
            label.contains("already in progress"),
            "and must say why: {label}"
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

    /// The individual post-confirmation race, mirroring the team one.
    ///
    /// The confirmation dialog is open for as long as the user takes to read
    /// it. Everything the decision rests on can change in that window, and the
    /// pre-dialog check is stale by the time the user accepts. Two distinct
    /// invalidations are covered because they fail through different gates:
    /// a **legacy** compaction starting (the cross-protocol duplicate gate),
    /// and capability being **revoked** (the fail-closed gate). Neither may
    /// dispatch, and neither may strand a typed `Requesting` claim.
    #[wasm_bindgen_test]
    async fn compact_aborts_when_a_legacy_compaction_starts_during_confirmation() {
        let calls = install_send_stub_with_held_dialog();
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-race-legacy", "Agent", true);
        seed_chat_row(&state, "a-race-legacy");
        declare_compaction_available(&state, "a-race-legacy");
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let (btn, enabled, _) = compact_btn_state(&container).await;
        assert!(enabled, "precondition: the control is offered");
        btn.click();
        for _ in 0..4 {
            next_tick().await;
        }

        // The legacy replacement protocol starts a compaction for this agent
        // while the dialog is still open.
        state.mark_compaction_started("h", AgentId("a-race-legacy".to_owned()));
        for _ in 0..4 {
            next_tick().await;
        }
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            !enabled && label.contains("already in progress"),
            "the mounted control reflects the change while the dialog is open: {label}"
        );

        release_held_dialog();
        for _ in 0..8 {
            next_tick().await;
        }

        let frames = recorded_frames(&calls);
        assert!(
            frames
                .iter()
                .all(|(kind, _, _)| kind != &FrameKind::AgentCompact.to_string()),
            "accepting a stale confirmation must not dispatch: {frames:?}"
        );
        assert!(
            state
                .context_compactions
                .with_untracked(|map| !map.contains_key(&AgentId("a-race-legacy".to_owned()))),
            "and must not strand a typed request claim"
        );
        assert!(
            state
                .compaction_in_progress
                .with_untracked(|map| map.contains_key(&AgentId("a-race-legacy".to_owned()))),
            "while the legacy operation that won the race is left untouched"
        );
    }

    /// Same race, invalidated through the fail-closed capability gate instead
    /// of the cross-protocol one.
    #[wasm_bindgen_test]
    async fn compact_aborts_when_capability_is_revoked_during_confirmation() {
        let calls = install_send_stub_with_held_dialog();
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-race-cap", "Agent", true);
        seed_chat_row(&state, "a-race-cap");
        declare_compaction_available(&state, "a-race-cap");
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let (btn, enabled, _) = compact_btn_state(&container).await;
        assert!(enabled, "precondition: the control is offered");
        btn.click();
        for _ in 0..4 {
            next_tick().await;
        }

        // The server withdraws the route while the dialog is open.
        state.compaction_capability.update(|map| {
            map.insert(
                AgentId("a-race-cap".to_owned()),
                crate::state::CompactionCapabilitySnapshot {
                    logical_session_id: protocol::SessionId(String::new()),
                    availability: protocol::RequestedCompactionAvailability::Unavailable {
                        reason: protocol::CompactionAvailabilityReason::NativeTriggerUnavailable,
                    },
                },
            );
        });
        for _ in 0..4 {
            next_tick().await;
        }
        let (_, enabled, _) = compact_btn_state(&container).await;
        assert!(
            !enabled,
            "the mounted control disables when capability is withdrawn"
        );

        release_held_dialog();
        for _ in 0..8 {
            next_tick().await;
        }

        let frames = recorded_frames(&calls);
        assert!(
            frames
                .iter()
                .all(|(kind, _, _)| kind != &FrameKind::AgentCompact.to_string()),
            "a withdrawn route must not be dispatched against: {frames:?}"
        );
        assert!(
            state
                .context_compactions
                .with_untracked(|map| !map.contains_key(&AgentId("a-race-cap".to_owned()))),
            "and must not strand a typed request claim"
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
        declare_compaction_available(&state, "a-fail");
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
        // `is_some()` became vacuous when the control stopped hiding: it is now
        // rendered for every agent card regardless of state. Assert what the
        // line was actually claiming — that the control is *offered* again.
        let (_, enabled, label) = compact_btn_state(&container).await;
        assert!(
            enabled,
            "compact should be offered again after a non-fatal failure: {label}"
        );
    }

    #[wasm_bindgen_test]
    async fn move_project_picker_sends_destination_and_shows_server_errors() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("h");
        crate::dispatch::prime_host_for_tests(&state, "h");
        push_agent_with_scope(
            &state,
            "h",
            "move-me",
            "Moving agent",
            true,
            Some("source"),
            None,
        );
        state.projects.set(vec![
            project_info("h", "source", "Source", 0),
            project_info("h", "destination", "Destination", 0),
            project_info("other", "foreign", "Other host project", 0),
        ]);
        state.projects.update(|projects| {
            projects[1].project.source = ProjectSource::Standalone {
                roots: vec![
                    ProjectRootPath(format!("/tmp/destination/{}", "nested-folder/".repeat(12))),
                    ProjectRootPath("/tmp/additional-folder".to_owned()),
                ],
            };
            for index in 0..12 {
                projects.push(project_info(
                    "h",
                    &format!("other-{index}"),
                    &format!("Project {index}"),
                    0,
                ));
            }
        });
        let agent = state.agents.get_untracked()[0].clone();
        open_agent_chat(&state, &agent);
        state.open_tab(TabContent::AgentMonitor, "Source overview".to_owned(), true);
        state.switch_active_project(Some(crate::state::ActiveProjectRef {
            host_id: "h".to_owned(),
            project_id: ProjectId("destination".to_owned()),
        }));
        state.open_tab(
            TabContent::AgentMonitor,
            "Destination overview".to_owned(),
            true,
        );
        open_agent_chat(&state, &agent);
        state
            .composer_untracked()
            .text
            .set("Keep this draft".to_owned());
        let open = RwSignal::new(true);
        let mounted_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            view! {
                <style>{include_str!("../../styles.css")}</style>
                <MoveAgentProjectDialog state=mounted_state.clone() agent=agent.clone() open=open />
            }
        });
        for _ in 0..4 {
            next_tick().await;
        }
        let text = container.text_content().unwrap();
        assert!(
            text.contains("No project"),
            "a chat can be moved out of its project"
        );
        assert!(text.contains("Destination") && text.contains("/tmp/destination"));
        assert!(!text.contains("Other host project"));
        assert!(text.contains("+1 folder"));
        assert!(!text.contains("(default directory)"));
        let buttons = container.query_selector_all("button").unwrap();
        let find = |label: &str| -> HtmlElement {
            (0..buttons.length())
                .map(|i| buttons.item(i).unwrap().dyn_into::<HtmlElement>().unwrap())
                .find(|button| button.text_content().unwrap_or_default().contains(label))
                .unwrap()
        };
        let submit = find("Move agent");
        let destination = find("Destination");
        assert!(
            destination
                .get_attribute("title")
                .unwrap()
                .contains("/tmp/additional-folder")
        );
        let window = web_sys::window().unwrap();
        let search: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Search projects']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        let font = |element: &web_sys::Element| {
            window
                .get_computed_style(element)
                .unwrap()
                .unwrap()
                .get_property_value("font-family")
                .unwrap()
        };
        let heading = container.query_selector("header").unwrap().unwrap();
        assert_eq!(font(&destination), font(&heading));
        assert_eq!(font(&search), font(&heading));
        let dialog: HtmlElement = container
            .query_selector("[role='dialog']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        dialog.style().set_property("width", "320px").unwrap();
        dialog.style().set_property("max-height", "400px").unwrap();
        dialog.style().set_property("animation", "none").unwrap();
        next_tick().await;
        let list: HtmlElement = container
            .query_selector("[role='group']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        let bounds = dialog.get_bounding_client_rect();
        assert!(
            destination.get_bounding_client_rect().height() < 72.0,
            "long paths must not create tall, wrapped destination rows"
        );
        assert!(
            dialog.scroll_width() <= dialog.client_width(),
            "the narrow dialog must not overflow horizontally"
        );
        assert!(
            list.scroll_height() > list.client_height(),
            "a long project list scrolls inside the dialog"
        );
        let submit_before_scroll = submit.get_bounding_client_rect();
        assert!(submit_before_scroll.bottom() <= bounds.bottom());
        assert!(search.get_bounding_client_rect().top() >= bounds.top());
        list.set_scroll_top(list.scroll_height());
        next_tick().await;
        assert_eq!(
            submit.get_bounding_client_rect().top(),
            submit_before_scroll.top(),
            "the move action stays visible while browsing projects"
        );
        list.set_scroll_top(0);
        assert!(submit.has_attribute("disabled"));
        find("Destination").click();
        next_tick().await;
        assert!(!submit.has_attribute("disabled"));
        submit.click();
        for _ in 0..6 {
            next_tick().await;
        }
        let frames = recorded_frames(&calls);
        let (_, payload, stream) = frames
            .iter()
            .find(|(kind, _, _)| kind == "agent_move")
            .expect("move command");
        assert_eq!(payload["agent_id"], "move-me");
        assert_eq!(payload["project_id"], "destination");
        assert!(stream.starts_with("/host/"));
        assert!(container.text_content().unwrap().contains("Moving…"));
        dispatch_frame(
            &state,
            "h",
            StreamPath(stream.clone()),
            FrameKind::AgentMoveResult,
            0,
            &protocol::types::AgentMoveResultPayload {
                request_id: payload["request_id"].as_str().unwrap().to_owned(),
                agent_id: AgentId("move-me".to_owned()),
                result: Err("Wait for background work to finish".to_owned()),
            },
        );
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Wait for background work to finish")
        );
        assert!(!submit.has_attribute("disabled"));
        assert!(open.get_untracked());
        submit.click();
        for _ in 0..6 {
            next_tick().await;
        }
        let frames = recorded_frames(&calls);
        let (_, retry, _) = frames
            .iter()
            .rev()
            .find(|(kind, _, _)| kind == "agent_move")
            .unwrap();
        let start = protocol::AgentStartPayload {
            tyde_permissions: Default::default(),
            swarm_membership: None,
            agent_id: AgentId("move-me".to_owned()),
            name: "Moving agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp/destination".to_owned()],
            project_id: Some(ProjectId("destination".to_owned())),
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 0,
        };
        dispatch_frame(
            &state,
            "h",
            StreamPath(stream.clone()),
            FrameKind::AgentMoveResult,
            1,
            &protocol::types::AgentMoveResultPayload {
                request_id: retry["request_id"].as_str().unwrap().to_owned(),
                agent_id: start.agent_id.clone(),
                result: Ok(start.clone()),
            },
        );
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(!open.get_untracked());
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id.0,
            "move-me"
        );
        assert_eq!(
            state.active_project.get_untracked().unwrap().project_id.0,
            "destination"
        );
        assert_eq!(
            state.composer_untracked().text.get_untracked(),
            "Keep this draft"
        );
        assert_eq!(
            state.agents.get_untracked()[0].workspace_roots,
            vec!["/tmp/destination"]
        );
        let labels = || {
            state.center_zone.with_untracked(|zone| {
                zone.all_tabs()
                    .map(|(_, tab)| tab.label.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert!(
            labels().contains(&"Destination overview".to_owned()),
            "destination view was not restored"
        );
        assert!(!labels().contains(&"Source overview".to_owned()));
        state.switch_active_project(Some(crate::state::ActiveProjectRef {
            host_id: "h".to_owned(),
            project_id: ProjectId("source".to_owned()),
        }));
        assert!(labels().contains(&"Source overview".to_owned()));
        assert!(!state.center_zone.with_untracked(|zone| zone.all_tabs().any(|(_, tab)| matches!(&tab.content, TabContent::Chat { agent_ref: Some(agent), .. } if agent.agent_id.0 == "move-me"))));
        state.switch_active_project(Some(crate::state::ActiveProjectRef {
            host_id: "h".to_owned(),
            project_id: ProjectId("destination".to_owned()),
        }));
        assert_eq!(
            state.composer_untracked().text.get_untracked(),
            "Keep this draft"
        );
        let unassign_container = make_container();
        let moved_agent = state.agents.get_untracked()[0].clone();
        let unassign_open = RwSignal::new(true);
        let mounted_state = state.clone();
        let _unassign_handle = mount_to(unassign_container.clone(), move || {
            view! { <MoveAgentProjectDialog state=mounted_state.clone() agent=moved_agent.clone() open=unassign_open /> }
        });
        for _ in 0..4 {
            next_tick().await;
        }
        let buttons = unassign_container.query_selector_all("button").unwrap();
        let find = |label: &str| -> HtmlElement {
            (0..buttons.length())
                .map(|i| buttons.item(i).unwrap().dyn_into::<HtmlElement>().unwrap())
                .find(|button| button.text_content().unwrap_or_default().contains(label))
                .unwrap()
        };
        let submit = find("Move agent");
        assert!(submit.has_attribute("disabled"));
        let search: web_sys::HtmlInputElement = unassign_container
            .query_selector("input")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        search.set_value("no matching project");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(
            unassign_container
                .text_content()
                .unwrap()
                .contains("No matching projects")
        );
        find("No project").click();
        next_tick().await;
        assert_eq!(
            find("No project").get_attribute("aria-pressed").as_deref(),
            Some("true")
        );
        assert!(!submit.has_attribute("disabled"));
        submit.click();
        for _ in 0..6 {
            next_tick().await;
        }
        let frames = recorded_frames(&calls);
        let (_, payload, stream) = frames
            .iter()
            .rev()
            .find(|(kind, _, _)| kind == "agent_move")
            .unwrap();
        assert_eq!(payload["agent_id"], "move-me");
        assert!(payload.get("project_id").unwrap().is_null());
        dispatch_frame(
            &state,
            "h",
            StreamPath(stream.clone()),
            FrameKind::AgentMoveResult,
            2,
            &protocol::types::AgentMoveResultPayload {
                request_id: payload["request_id"].as_str().unwrap().to_owned(),
                agent_id: AgentId("move-me".to_owned()),
                result: Ok(protocol::AgentStartPayload {
                    tyde_permissions: Default::default(),
                    swarm_membership: None,
                    project_id: None,
                    ..start
                }),
            },
        );
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(!unassign_open.get_untracked());
        assert!(state.active_project.get_untracked().is_none());
        assert!(state.agents.get_untracked()[0].project_id.is_none());
        assert_eq!(
            state.active_agent.get_untracked().unwrap().agent_id.0,
            "move-me"
        );
        assert_eq!(
            state.agents.get_untracked()[0].workspace_roots,
            vec!["/tmp/destination"]
        );
        assert_eq!(
            state.composer_untracked().text.get_untracked(),
            "Keep this draft"
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
        // The control fails closed without a declared route, so the send path
        // is only reachable once the server has spoken.
        declare_compaction_available(&state, "a-click");
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let (btn, enabled, _) = compact_btn_state(&container).await;
        assert!(enabled, "precondition: the control is offered");
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
        // In-flight now lives on the shared request gate that every compaction
        // control claims, rather than on the legacy replacement flag.
        assert!(
            state.context_compactions.with_untracked(|map| map
                .get(&AgentId("a-click".to_owned()))
                .is_some_and(|operation| operation.is_in_flight())),
            "agent should be flagged as in-flight while the server processes"
        );
        // And the same gate refuses a second submit.
        let (btn, enabled, _) = compact_btn_state(&container).await;
        assert!(
            !enabled,
            "the control disables while its request is in flight"
        );
        btn.click();
        for _ in 0..8 {
            next_tick().await;
        }
        assert_eq!(
            recorded_frames(&calls)
                .iter()
                .filter(|(kind, _, _)| kind == &FrameKind::AgentCompact.to_string())
                .count(),
            1,
            "a second click must not send another frame"
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
                tyde_permissions: Default::default(),
                swarm_membership: None,
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
                activity: protocol::AgentActivity::Idle,
                review_recipient: false,
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
                tyde_permissions: Default::default(),
                swarm_membership: None,
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
                tyde_permissions: Default::default(),
                host_id: "h-clean".to_owned(),
                agent_id: AgentId("a-new".to_owned()),
                name: "New".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                team_member_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-new/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
                review_recipient: false,
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
                tyde_permissions: Default::default(),
                host_id: "h-clean".to_owned(),
                agent_id: AgentId("a-new".to_owned()),
                name: "New".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                team_member_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-new/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
                review_recipient: false,
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

    fn filter_control(container: &HtmlElement, label: &str) -> web_sys::HtmlInputElement {
        container
            .query_selector(&format!(".panel-filters input[aria-label='{label}']"))
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap()
    }

    fn filter_enabled(container: &HtmlElement, label: &str) -> bool {
        filter_control(container, label).checked()
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

    /// "Show other projects" is sticky across projects. Opening an agent from
    /// another project switches to that project; the unpinned selector used to
    /// read as off inside a project, so the list collapsed as soon as the user
    /// picked something from it. It must stay on, and a pinned "off" must
    /// likewise survive the trip back Home.
    #[wasm_bindgen_test]
    async fn show_other_projects_is_sticky_across_project_switches() {
        let _calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
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
        push_agent_with_scope(
            &state,
            "local",
            "agent-xyz",
            "XYZ Agent",
            true,
            Some("xyz"),
            None,
        );
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        let card_rendered = |agent_id: &str| {
            container
                .query_selector(&format!("[data-agent-id='{agent_id}'] .agent-card"))
                .unwrap()
                .is_some()
        };

        assert!(filter_enabled(&container, "Show other projects"));
        assert!(card_rendered("agent-abc") && card_rendered("agent-xyz"));

        agent_card_el(&container, "agent-abc").click();
        for _ in 0..8 {
            next_tick().await;
        }
        assert_eq!(
            state
                .active_project
                .get_untracked()
                .map(|project| project.project_id),
            Some(ProjectId("abc".to_owned())),
            "precondition: opening the agent switched to its project"
        );
        assert!(
            filter_enabled(&container, "Show other projects"),
            "opening an agent in another project must not turn the selector off"
        );
        assert!(
            card_rendered("agent-xyz"),
            "agents from other projects stay listed after the project switch"
        );

        filter_control(&container, "Show other projects").click();
        next_tick().await;
        assert!(!filter_enabled(&container, "Show other projects"));
        assert!(card_rendered("agent-abc") && !card_rendered("agent-xyz"));

        state.switch_active_project(None);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            !filter_enabled(&container, "Show other projects"),
            "a pinned off selector stays off when returning Home"
        );
        assert!(
            !card_rendered("agent-abc") && !card_rendered("agent-xyz"),
            "Home with other projects hidden lists no project agents"
        );
    }

    /// Clicking a selector pins an explicit durable value through the server
    /// update path (`set_sidebar_preferences`) rather than mutating local state.
    #[wasm_bindgen_test]
    async fn sidebar_toggle_emits_typed_set_sidebar_preferences() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        // The unpinned ContextualDefault is effective-on, so a click pins the
        // opposite explicit value: current_project_only.
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;

        filter_control(&container, "Show other projects").click();
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
            !filter_enabled(&container, "Show other projects"),
            "overlay reflects the pinned current_project_only immediately"
        );
    }

    /// A pinned selector persists across active-project switches (the overlay is
    /// not project-keyed), and an authoritative server snapshot then carries the
    /// same value so it stays after the overlay is dropped.
    #[wasm_bindgen_test]
    async fn sidebar_hide_inactive_persists_across_project_switch_and_snapshot() {
        let trace = |phase: &str| {
            let output = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id("output")
                .unwrap();
            output.set_text_content(Some(&format!(
                "{}Sidebar pin DOM phase: {phase}\n",
                output.text_content().unwrap()
            )));
        };
        trace("starting");
        let _calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = primed_sidebar_state(AgentsSidebarPreferences::default());
        let _handle = mount_panel(&container, state.clone());
        next_tick().await;
        trace("mounted");
        assert!(!filter_enabled(&container, "Hide inactive"));

        trace("clicking selector");
        filter_control(&container, "Hide inactive").click();
        trace("selector clicked");
        next_tick().await;
        assert!(
            filter_enabled(&container, "Hide inactive"),
            "overlay reflects hide_inactive immediately"
        );

        trace("overlay rendered");
        // Switch project: the selector is server-owned, not project-local, so it
        // stays pinned.
        trace("switching project");
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: ProjectId("p1".to_owned()),
        }));
        next_tick().await;
        assert!(
            filter_enabled(&container, "Hide inactive"),
            "pinned hide_inactive survives a project switch"
        );

        trace("project switch rendered");
        // Authoritative snapshot confirms the value and drops the overlay.
        state.apply_agents_view_snapshot(
            "local",
            sidebar_snapshot(AgentsSidebarPreferences {
                hide_inactive: true,
                ..Default::default()
            }),
        );
        trace("snapshot applied");
        next_tick().await;
        trace("snapshot rendered");
        assert!(
            filter_enabled(&container, "Hide inactive"),
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

        filter_control(&container, "Hide sub-agents").click();
        next_tick().await;
        assert!(
            filter_enabled(&container, "Hide sub-agents"),
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
            !filter_enabled(&container, "Hide sub-agents"),
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

    #[wasm_bindgen_test]
    async fn server_project_entry_moves_the_visible_chat_into_its_workbench() {
        use crate::components::center_zone::CenterZone;

        let trace = |phase: &str| {
            let output = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id("output")
                .unwrap();
            output.set_text_content(Some(&format!(
                "{}Project-entry DOM phase: {phase}\n",
                output.text_content().unwrap()
            )));
        };
        trace("starting");
        let remove_error_trace = trace_project_entry_errors();
        install_send_stub_with_dialog_ok();
        let storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
        storage.remove_item("tyde-composer-draft").unwrap();
        let container = make_container();
        let state = make_app_state("local");
        state
            .configured_hosts
            .set(vec![configured_host("local", "Local Host")]);
        let mut workbench = project_info("local", "workbench", "Task workbench", 1);
        workbench.project.source = ProjectSource::GitWorkbench {
            environment: Default::default(),
            parent_project_id: ProjectId("source".to_owned()),
            branch: protocol::GitBranchName("task".to_owned()),
            roots: vec![protocol::WorkbenchRoot {
                parent_root: ProjectRootPath("/tmp/source".to_owned()),
                worktree_root: ProjectRootPath("/tmp/source--task".to_owned()),
            }],
        };
        state.projects.set(vec![
            project_info("local", "source", "Source project", 0),
            workbench,
        ]);
        push_agent_with_scope(
            &state,
            "local",
            "worker",
            "Task worker",
            true,
            Some("source"),
            None,
        );
        for (id, name, project) in [
            ("source-note", "Source notes", "source"),
            ("workbench-note", "Workbench notes", "workbench"),
        ] {
            push_agent_with_scope(&state, "local", id, name, true, Some(project), None);
        }
        apply_show_all_projects(&state);
        trace("fixture prepared");
        CenterWorkspaceWidth::forget_measurement();
        let mounted = state.clone();
        let mounted_container = container.clone();
        let mount_result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let result = mount_result.clone();
        let mount = wasm_bindgen::closure::Closure::once(move || {
            trace("mount entered");
            let handle = mount_to(mounted_container, move || {
                provide_context(mounted.clone());
                trace("installing browser effects");
                mounted.install_browser_effects();
                trace("constructing sidebar and workspace views");
                view! { <AgentsPanel /> <CenterZone /> }
            });
            trace("mount returned");
            *result.borrow_mut() = Some(handle);
        });
        trace_sidebar_mount(mount.as_ref().unchecked_ref());
        let _handle = mount_result.borrow_mut().take().unwrap();
        for _ in 0..4 {
            next_tick().await;
        }
        trace("mount ticks completed");
        for id in ["workbench-note", "source-note"] {
            trace("opening remembered chat");
            agent_card_el(&container, id).click();
            trace("remembered-chat click returned");
            for _ in 0..8 {
                next_tick().await;
            }
            trace("remembered-chat ticks completed");
        }
        trace("opening conversation before entry");
        agent_card_el(&container, "worker").click();
        trace("conversation click returned");
        for _ in 0..8 {
            next_tick().await;
        }
        trace("conversation ticks completed");
        state
            .composer_untracked()
            .text
            .set("Keep my draft while entering".to_owned());
        trace("draft set");
        let before = agent_card_el(&container, "worker")
            .closest("[data-project-key]")
            .unwrap()
            .unwrap();
        assert!(before.text_content().unwrap().contains("Source project"));
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Source notes']")
                .unwrap()
                .is_some()
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Workbench notes']")
                .unwrap()
                .is_none()
        );
        let payload = protocol::AgentStartPayload {
            tyde_permissions: Default::default(),
            swarm_membership: None,
            agent_id: AgentId("worker".to_owned()),
            name: "Task worker".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp/source--task".to_owned()],
            project_id: Some(ProjectId("workbench".to_owned())),
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 0,
        };
        trace("dispatching project assignment");
        dispatch_frame(
            &state,
            "local",
            StreamPath("/agent/worker/inst".to_owned()),
            FrameKind::AgentProjectChanged,
            0,
            &payload,
        );
        trace("assignment dispatch returned");
        for _ in 0..8 {
            next_tick().await;
        }
        trace("assignment ticks completed");
        let after = agent_card_el(&container, "worker")
            .closest("[data-project-key]")
            .unwrap()
            .unwrap();
        // Entry still changes the working project, but agent-list headings
        // deliberately stay on the parent rather than the workbench name.
        let grouped_text = after.text_content().unwrap();
        for name in [
            "Source project",
            "Task worker",
            "Source notes",
            "Workbench notes",
        ] {
            assert!(
                grouped_text.contains(name),
                "the parent group must include {name}"
            );
        }
        assert!(
            after
                .query_selector(
                    ".agent-sidebar-project-header [aria-label='Project: Task workbench']"
                )
                .unwrap()
                .is_none(),
            "A workbench does not split its parent group"
        );
        // Rows no longer carry the inline details panel, whose collapsed-hidden
        // "Workbench:" line this once probed; the sidebar's visible workbench
        // grouping is what identifies the row's actual workbench.
        let show_workbenches = container
            .query_selector("input[aria-label='Show workbenches']")
            .unwrap()
            .expect("the agent filters offer workbench grouping")
            .dyn_into::<HtmlElement>()
            .unwrap();
        show_workbenches.click();
        next_tick().await;
        let bench = container
            .query_selector("[data-project-key='local:source'] [data-project-id='workbench']")
            .unwrap()
            .expect("the parent group renders the workbench the chat entered");
        assert!(
            bench
                .query_selector("[aria-label='Workbench: Task workbench']")
                .unwrap()
                .is_some(),
            "the workbench grouping is labelled with the workbench's name"
        );
        assert!(
            bench.contains(Some(&agent_card_el(&container, "worker"))),
            "The grouped row still identifies its actual workbench"
        );
        show_workbenches.click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[data-project-key]")
                .unwrap()
                .length(),
            1
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-selected='true'][aria-label='Task worker']")
                .unwrap()
                .is_some(),
            "the conversation stays open after server entry"
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Workbench notes']")
                .unwrap()
                .is_some(),
            "entry must restore the destination workspace's other tabs"
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Source notes']")
                .unwrap()
                .is_none(),
            "the source workspace's unrelated tabs must not follow the moved chat"
        );
        let inputs = container.query_selector_all("textarea").unwrap();
        assert!(
            (0..inputs.length()).any(|index| inputs
                .item(index)
                .unwrap()
                .dyn_into::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value()
                == "Keep my draft while entering"),
            "entering preserves the user's visible draft"
        );
        trace("destination assertions completed; returning to source");
        agent_card_el(&container, "source-note").click();
        trace("return-to-source click returned");
        for _ in 0..8 {
            next_tick().await;
        }
        trace("return-to-source ticks completed");
        assert!(
            container
                .query_selector("[role='tab'][aria-selected='true'][aria-label='Source notes']")
                .unwrap()
                .is_some()
        );
        for label in ["Task worker", "Workbench notes"] {
            assert!(
                container
                    .query_selector(&format!("[role='tab'][aria-label='{label}']"))
                    .unwrap()
                    .is_none(),
                "moved and destination chats must not remain in the source workspace"
            );
        }
        trace("source assertions completed; reopening moved conversation");
        agent_card_el(&container, "worker").click();
        trace("reopen click returned");
        for _ in 0..8 {
            next_tick().await;
        }
        trace("reopen ticks completed");
        assert!(
            container
                .query_selector("[role='tab'][aria-selected='true'][aria-label='Task worker']")
                .unwrap()
                .is_some()
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Workbench notes']")
                .unwrap()
                .is_some(),
            "opening the parent-grouped worker restores its workbench's tabs"
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Source notes']")
                .unwrap()
                .is_none(),
            "opening the parent-grouped worker must not keep the parent workspace active"
        );
        let inputs = container.query_selector_all("textarea").unwrap();
        assert!(
            (0..inputs.length()).any(|index| inputs
                .item(index)
                .unwrap()
                .dyn_into::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value()
                == "Keep my draft while entering"),
            "the moved draft must also survive returning to its workspace"
        );
        trace("all assertions completed; dropping mount");
        drop(_handle);
        trace("mount dropped");
        container.remove();
        storage.remove_item("tyde-composer-draft").unwrap();
        remove_error_trace
            .call0(&wasm_bindgen::JsValue::NULL)
            .unwrap();
        trace("cleanup completed");
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

    #[wasm_bindgen_test]
    async fn clicking_unscoped_agent_opens_home_without_changing_project_tabs() {
        use crate::components::center_zone::CenterZone;

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
        push_agent(&state, "local", "agent-home", "Home Agent", true);
        push_agent(&state, "local", "agent-home-two", "Second Home Agent", true);
        apply_show_all_projects(&state);
        CenterWorkspaceWidth::forget_measurement();
        let mounted_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mounted_state.clone());
            view! { <AgentsPanel /> <CenterZone /> }
        });
        for _ in 0..4 {
            next_tick().await;
        }
        agent_card_el(&container, "agent-abc").click();
        for _ in 0..8 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector("[role='tab'][aria-selected='true'][aria-label='ABC Agent']")
                .unwrap()
                .is_some()
        );

        agent_card_el(&container, "agent-home").click();
        for _ in 0..8 {
            next_tick().await;
        }
        assert!(
            state.active_project.get_untracked().is_none(),
            "a no-project agent must switch to Home before its chat opens"
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-selected='true'][aria-label='Home Agent']")
                .unwrap()
                .is_some(),
            "the no-project chat must be selected in Home"
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='ABC Agent']")
                .unwrap()
                .is_none(),
            "project tabs must not remain in the Home workspace"
        );
        assert_eq!(project_accessed_streams(&calls), vec!["/project/abc"]);

        dispatch_key(&agent_card_el(&container, "agent-home-two"), "Enter");
        for _ in 0..8 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector(
                    "[role='tab'][aria-selected='true'][aria-label='Second Home Agent']"
                )
                .unwrap()
                .is_some()
        );
        assert!(
            container
                .query_selector("[role='tab'][aria-label='Home Agent']")
                .unwrap()
                .is_some(),
            "opening another unscoped chat in Home must preserve its existing tabs"
        );

        agent_card_el(&container, "agent-abc").click();
        for _ in 0..8 {
            next_tick().await;
        }
        assert!(
            container
                .query_selector("[role='tab'][aria-selected='true'][aria-label='ABC Agent']")
                .unwrap()
                .is_some(),
            "returning to the project must restore its chat"
        );
        for label in ["Home Agent", "Second Home Agent"] {
            assert!(
                container
                    .query_selector(&format!("[role='tab'][aria-label='{label}']"))
                    .unwrap()
                    .is_none(),
                "Home chats must not leak into the project's saved tabs"
            );
        }
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

    /// Splits are created by dragging tabs: agent cards carry no
    /// open-to-the-side (duplicate) control.
    #[wasm_bindgen_test]
    async fn agent_cards_have_no_side_open_control() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a1", "Agent A", true);
        seed_chat_row(&state, "a1");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        assert!(
            container
                .query_selector("[data-test='agent-open-side']")
                .unwrap()
                .is_none(),
            "no open-to-the-side control renders on an agent card"
        );
        assert!(
            container
                .query_selector(".agent-card-open-side")
                .unwrap()
                .is_none(),
            "and the side-open class is gone entirely"
        );
    }

    /// A restoration pass that could not bring an agent back is announced,
    /// naming the failed agent, its skipped children and the deferred retry,
    /// whether the host reports it in its bootstrap or as a live status, and
    /// the notice disappears when the host reports no failures.
    #[wasm_bindgen_test]
    async fn restoration_failures_name_the_skipped_subtree_until_cleared() {
        use crate::dispatch::restore_fixtures::restore_bootstrap;

        let failure = protocol::AgentRestorationFailure {
            agent: protocol::RestoredAgentRef::Session {
                session_id: protocol::SessionId("parent-session".to_owned()),
                agent_id: Some(AgentId("parent-agent".to_owned())),
                name: Some("failing parent".to_owned()),
            },
            reason: protocol::AgentRestorationFailureReason::ReconstructFailed {
                message: "backend refused the resume".to_owned(),
            },
            retry: protocol::AgentRestorationRetry::NextLaunch,
            skipped: vec![protocol::RestoredAgentRef::Session {
                session_id: protocol::SessionId("child-session".to_owned()),
                agent_id: None,
                name: None,
            }],
        };
        let container = make_container();
        let state = make_app_state("h");
        crate::dispatch::prime_host_for_tests(&state, "h");
        let mut bootstrap = restore_bootstrap(vec![], Default::default(), vec![], vec![]);
        bootstrap.agent_restoration_failures = vec![failure.clone()];
        dispatch_frame(
            &state,
            "h",
            StreamPath("/host/h".to_owned()),
            FrameKind::HostBootstrap,
            0,
            &bootstrap,
        );
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        let expected = "Some open agents could not be restored.\
             failing parent: backend refused the resume. \
             Also not restored: session child-session. \
             Restore will be retried on next launch.";
        let notice_text = || {
            container
                .query_selector("[role='alert']")
                .unwrap()
                .map(|alert| alert.text_content().unwrap_or_default())
        };
        assert_eq!(
            notice_text().as_deref(),
            Some(expected),
            "the bootstrap's restoration failure is announced"
        );

        dispatch_frame(
            &state,
            "h",
            StreamPath("/host/h".to_owned()),
            FrameKind::AgentRestorationStatus,
            1,
            &protocol::AgentRestorationStatusPayload { failures: vec![] },
        );
        next_tick().await;
        assert_eq!(notice_text(), None, "a cleared status removes the notice");

        dispatch_frame(
            &state,
            "h",
            StreamPath("/host/h".to_owned()),
            FrameKind::AgentRestorationStatus,
            2,
            &protocol::AgentRestorationStatusPayload {
                failures: vec![failure],
            },
        );
        next_tick().await;
        assert_eq!(
            notice_text().as_deref(),
            Some(expected),
            "a live restoration status is announced"
        );
    }

    #[wasm_bindgen_test]
    async fn agents_panel_does_not_overflow_horizontally_at_narrow_and_normal_widths() {
        let _calls = install_send_stub_with_dialog_ok();
        let state = make_app_state("local");
        crate::dispatch::prime_host_for_tests(&state, "local");

        let long_host_name =
            "Very Long Host Name That Tests Boundary Layout Truncation In The Sidebar";
        state
            .configured_hosts
            .set(vec![configured_host("local", long_host_name)]);

        let parent_project_id = "parent-project";
        let parent_project_name = "Long Parent Project Name Designed To Exceed Container Boundary";
        let parent_proj = project_info("local", parent_project_id, parent_project_name, 0);

        let workbench_id = "workbench-project";
        let workbench_name = "Long Workbench Name Nested Within Parent Project In Sidebar";
        let mut workbench = project_info("local", workbench_id, workbench_name, 1);
        workbench.project.source = ProjectSource::GitWorkbench {
            environment: Default::default(),
            parent_project_id: ProjectId(parent_project_id.to_owned()),
            branch: protocol::GitBranchName("feature/nested-workbench-branch-name".to_owned()),
            roots: vec![protocol::WorkbenchRoot {
                parent_root: ProjectRootPath(format!("/tmp/{parent_project_id}")),
                worktree_root: ProjectRootPath(format!("/tmp/{workbench_id}")),
            }],
        };
        state.projects.set(vec![parent_proj, workbench]);

        let group_id = AgentGroupId("custom-group".to_owned());
        let mut preferences = state.agents_view_preferences.get_untracked();
        preferences.sidebar.show_workbenches = true;
        preferences
            .pins
            .pinned
            .push(local_target("workbench-agent"));
        preferences.groups.groups.push(AgentGroup {
            id: group_id.clone(),
            name: "Custom Group With Long Label And Action Buttons".to_owned(),
        });
        preferences.groups.assignments.push(AgentGroupAssignment {
            target: local_target("grouped-agent"),
            group_id,
        });
        state.agents_view_preferences.set(preferences);

        // Parent agent with children
        push_agent_with_scope(
            &state,
            "local",
            "parent-agent",
            "Parent Agent With Long Name And Child Disclosure Toggle",
            true,
            Some(parent_project_id),
            None,
        );
        // Child agent under parent agent
        push_agent_with_scope(
            &state,
            "local",
            "child-agent",
            "Child Agent With Deeply Nested Indentation And Long Name",
            true,
            Some(parent_project_id),
            Some("parent-agent"),
        );
        // Workbench agent
        push_agent_with_scope(
            &state,
            "local",
            "workbench-agent",
            "Workbench Agent Inside Nested Branch With Long Name",
            true,
            Some(workbench_id),
            None,
        );
        // Grouped agent
        push_agent_with_scope(
            &state,
            "local",
            "grouped-agent",
            "Grouped Agent With Long Name In Custom Folder",
            true,
            Some(parent_project_id),
            None,
        );

        // AwaitingUser status on parent agent ("Needs your answer")
        state.agent_awaiting_user.update(|set| {
            set.insert(AgentId("parent-agent".to_owned()));
            set.insert(AgentId("grouped-agent".to_owned()));
        });
        // In-flight compaction status on workbench agent ("Compacting context")
        state.mark_compaction_started("local", AgentId("workbench-agent".to_owned()));
        // Active turn on child agent so it is revealed when parent is expanded
        state.agent_turn_active.update(|turns| {
            turns.insert(AgentId("child-agent".to_owned()), true);
        });

        for width in [220, 320] {
            let container = make_container();
            container
                .set_attribute(
                    "style",
                    &format!(
                        "position: absolute; top: 0; left: 0; width: {width}px; height: 800px; overflow: hidden;"
                    ),
                )
                .unwrap();
            let _handle = mount_panel(&container, state.clone());

            for _ in 0..4 {
                next_tick().await;
            }

            // Explicitly require parent card and its collapse toggle (fail if missing)
            let parent_card = container
                .query_selector("[data-agent-drop-id='parent-agent']")
                .unwrap()
                .expect("parent-agent card must exist in DOM");
            let toggle: HtmlElement = parent_card
                .query_selector(".agent-card-collapse-toggle")
                .unwrap()
                .expect("parent-agent must have a collapse toggle for its children")
                .dyn_into()
                .unwrap();
            toggle.click();
            for _ in 0..4 {
                next_tick().await;
            }

            // Explicitly assert that child agent rendered with visible, nonzero geometry
            let child_card: HtmlElement = container
                .query_selector("[data-agent-drop-id='child-agent']")
                .unwrap()
                .expect("child-agent card must be rendered in DOM after expanding parent")
                .dyn_into()
                .unwrap();
            let child_rect = child_card.get_bounding_client_rect();
            assert!(
                child_rect.width() > 0.0 && child_rect.height() > 0.0,
                "child-agent card must have visible nonzero geometry when expanded at {width}px",
            );

            // Assert that agent name maintains meaningful visible width for identification
            let child_name: HtmlElement = child_card
                .query_selector(".agent-card-name")
                .unwrap()
                .expect("child-agent name must exist")
                .dyn_into()
                .unwrap();
            let child_name_rect = child_name.get_bounding_client_rect();
            assert!(
                child_name_rect.width() >= 40.0,
                "child-agent name must maintain meaningful visible width for identification at {width}px (got {}px)",
                child_name_rect.width(),
            );

            let panel_content: HtmlElement = container
                .query_selector(".panel-content")
                .unwrap()
                .expect("panel-content exists")
                .dyn_into()
                .unwrap();

            // Assert that horizontal scroll width does not exceed client width (within 1px subpixel tolerance)
            assert!(
                panel_content.scroll_width() <= panel_content.client_width() + 1,
                "panel-content must not overflow horizontally at {width}px (scroll_width: {}, client_width: {})",
                panel_content.scroll_width(),
                panel_content.client_width(),
            );

            // Verify no horizontal scroll displacement occurs when set
            panel_content.set_scroll_left(50);
            assert_eq!(
                panel_content.scroll_left(),
                0,
                "panel-content must have zero horizontal scroll range at {width}px",
            );

            let panel_rect = panel_content.get_bounding_client_rect();
            let tolerance = 1.0;

            // Host new-project button stays within bounds
            let host_new_projects = container.query_selector_all(".agent-new-project").unwrap();
            for i in 0..host_new_projects.length() {
                let btn: HtmlElement = host_new_projects.item(i).unwrap().dyn_into().unwrap();
                let rect = btn.get_bounding_client_rect();
                assert!(
                    rect.right() <= panel_rect.right() + tolerance,
                    "agent-new-project #{i} must remain within panel at {width}px (right: {}, panel_right: {})",
                    rect.right(),
                    panel_rect.right(),
                );
            }

            // Project & workbench edit pencils stay within bounds
            let edit_pencils = container
                .query_selector_all(".project-edit-pencil")
                .unwrap();
            for i in 0..edit_pencils.length() {
                let btn: HtmlElement = edit_pencils.item(i).unwrap().dyn_into().unwrap();
                let rect = btn.get_bounding_client_rect();
                assert!(
                    rect.right() <= panel_rect.right() + tolerance,
                    "project-edit-pencil #{i} must remain within panel at {width}px (right: {}, panel_right: {})",
                    rect.right(),
                    panel_rect.right(),
                );
            }

            // Custom group action buttons stay within bounds
            let group_actions = container
                .query_selector_all(".agent-sidebar-custom-group-actions")
                .unwrap();
            for i in 0..group_actions.length() {
                let el: HtmlElement = group_actions.item(i).unwrap().dyn_into().unwrap();
                let rect = el.get_bounding_client_rect();
                assert!(
                    rect.right() <= panel_rect.right() + tolerance,
                    "custom group actions #{i} must remain within panel at {width}px (right: {}, panel_right: {})",
                    rect.right(),
                    panel_rect.right(),
                );
            }

            // Every row's actions button stays visible and within bounds
            let action_buttons = container.query_selector_all(".agent-card-actions").unwrap();
            assert_eq!(
                action_buttons.length(),
                container
                    .query_selector_all(".agent-card[data-agent-drop-id]")
                    .unwrap()
                    .length(),
                "every agent row offers its actions at {width}px"
            );
            for i in 0..action_buttons.length() {
                let btn: HtmlElement = action_buttons.item(i).unwrap().dyn_into().unwrap();
                let rect = btn.get_bounding_client_rect();
                assert!(
                    rect.width() > 0.0 && rect.right() <= panel_rect.right() + tolerance,
                    "agent actions #{i} must be visible within panel at {width}px (width: {}, right: {}, panel_right: {})",
                    rect.width(),
                    rect.right(),
                    panel_rect.right(),
                );
            }

            // Verify pinned agent has visible, nonzero geometry and stays within bounds
            let pinned_pin: HtmlElement = container
                .query_selector("[data-agent-drop-id='workbench-agent'] .agent-card-pin.pinned")
                .unwrap()
                .expect("pinned workbench-agent must render a visible .agent-card-pin.pinned")
                .dyn_into()
                .unwrap();
            let pinned_rect = pinned_pin.get_bounding_client_rect();
            assert!(
                pinned_rect.width() > 0.0 && pinned_rect.height() > 0.0,
                "pinned pin button must have nonzero visible geometry at {width}px (width: {}, height: {})",
                pinned_rect.width(),
                pinned_rect.height(),
            );
            assert!(
                pinned_rect.right() <= panel_rect.right() + tolerance,
                "pinned pin button must remain within panel at {width}px (right: {}, panel_right: {})",
                pinned_rect.right(),
                panel_rect.right(),
            );

            // Revealing the pin (hover and focus share one rule) must not move
            // anything already on the row, or change its height.
            let row_geometry = |card: &HtmlElement| {
                let mut geometry = vec![card.get_bounding_client_rect().height()];
                for part in [
                    ".agent-backend-glyph",
                    ".agent-card-name",
                    ".agent-card-status",
                    ".agent-card-actions",
                ] {
                    let rect = card
                        .query_selector(part)
                        .unwrap()
                        .unwrap_or_else(|| panic!("child-agent row must render {part}"))
                        .get_bounding_client_rect();
                    geometry.extend([rect.left(), rect.top()]);
                }
                geometry
            };
            let child_pin_width = || {
                child_card
                    .query_selector(".agent-card-pin")
                    .unwrap()
                    .expect("child-agent pin button must exist")
                    .get_bounding_client_rect()
                    .width()
            };
            assert_eq!(
                child_pin_width(),
                0.0,
                "the pin of an unpinned row waits for hover or focus"
            );
            let at_rest = row_geometry(&child_card);
            assert!(
                (27.0..=29.0).contains(&at_rest[0]),
                "a row with a glyph-only status is one 28px line at {width}px, got {}px",
                at_rest[0]
            );
            // height, glyph.left, glyph.top, name.left, name.top, status.left, status.top, actions.left, actions.top
            assert!(
                at_rest[5] < at_rest[3] && at_rest[3] < at_rest[1] && at_rest[1] < at_rest[7],
                "activity leads the name and the backend glyph trails it at {width}px: {at_rest:?}"
            );

            // Focus child-agent card to reveal its pin control via :focus-within and verify visible geometry
            child_card.focus().unwrap();
            for _ in 0..2 {
                next_tick().await;
            }
            assert!(child_pin_width() > 0.0);
            assert_eq!(
                row_geometry(&child_card),
                at_rest,
                "revealing the pin must not move the glyph, name, status or actions at {width}px"
            );
            let child_pin: HtmlElement = child_card
                .query_selector(".agent-card-pin")
                .unwrap()
                .expect("child-agent pin button must exist")
                .dyn_into()
                .unwrap();
            let child_pin_rect = child_pin.get_bounding_client_rect();
            assert!(
                child_pin_rect.width() > 0.0 && child_pin_rect.height() > 0.0,
                "focused child card pin button must have nonzero visible geometry under focus at {width}px (width: {}, height: {})",
                child_pin_rect.width(),
                child_pin_rect.height(),
            );
            assert!(
                child_pin_rect.right() <= panel_rect.right() + tolerance,
                "focused child card pin button must remain within panel at {width}px (right: {}, panel_right: {})",
                child_pin_rect.right(),
                panel_rect.right(),
            );

            for id in ["grouped-agent", "workbench-agent"] {
                let card = agent_card_el(&container, id);
                let at_rest = row_geometry(&card);
                assert_eq!(
                    at_rest[0], 28.0,
                    "a labelled leaf row stays one line at {width}px"
                );
                assert!(at_rest[5] < at_rest[3] && at_rest[3] < at_rest[1]);
                card.focus().unwrap();
                next_tick().await;
                assert_eq!(
                    row_geometry(&card),
                    at_rest,
                    "revealing a labelled row's pin cannot wrap or move its content at {width}px"
                );
            }

            // Verify awaiting-user status text has visible nonzero geometry and stays within panel
            let awaiting_text: HtmlElement = container
                .query_selector(
                    "[data-agent-drop-id='parent-agent'] .agent-card-status.awaiting .agent-card-status-text",
                )
                .unwrap()
                .expect("parent-agent must display awaiting status text")
                .dyn_into()
                .unwrap();
            let awaiting_rect = awaiting_text.get_bounding_client_rect();
            assert!(
                awaiting_rect.width() > 0.0 && awaiting_rect.height() > 0.0,
                "awaiting status text must have nonzero visible geometry at {width}px",
            );
            assert!(
                awaiting_rect.right() <= panel_rect.right() + tolerance,
                "awaiting status text must remain within panel at {width}px (right: {}, panel_right: {})",
                awaiting_rect.right(),
                panel_rect.right(),
            );

            // Verify compacting status text has visible nonzero geometry and stays within panel
            let compacting_text: HtmlElement = container
                .query_selector(
                    "[data-agent-drop-id='workbench-agent'] .agent-card-status .agent-card-status-text",
                )
                .unwrap()
                .expect("workbench-agent must display compaction status text")
                .dyn_into()
                .unwrap();
            let compacting_rect = compacting_text.get_bounding_client_rect();
            assert!(
                compacting_rect.width() > 0.0 && compacting_rect.height() > 0.0,
                "compaction status text must have nonzero visible geometry at {width}px",
            );
            assert!(
                compacting_rect.right() <= panel_rect.right() + tolerance,
                "compaction status text must remain within panel at {width}px (right: {}, panel_right: {})",
                compacting_rect.right(),
                panel_rect.right(),
            );

            container.remove();
        }
    }
}
