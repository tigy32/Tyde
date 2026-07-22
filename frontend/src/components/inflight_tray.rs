//! The **In-flight tray**: the single live surface for everything a chat's
//! agent currently has running in the background — first-class Tyde child
//! agents, native sub-agents, workflow runs — plus the user's queued
//! messages, which are also "pending while the agent is busy" state.
//!
//! The tray exists because live progress used to render inside tool cards,
//! where one child agent appeared in *both* its spawn card and any await
//! card watching it — two identical live monitors by construction. Tool
//! cards are now historical receipts; this tray is the only place live
//! background state renders. It is deliberately independent of
//! `ToolOutputMode`: operational awareness of running work must not
//! disappear because the user prefers compact transcript history.
//!
//! Shape: one collapsed summary line ("2 running · 1 queued · …") that
//! expands into per-process rows. When nothing is in flight the tray
//! renders nothing at all. Finished items stay as muted rows until
//! "Clear finished"; failures stay pinned until individually dismissed —
//! a background failure must never scroll away silently.

use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentActivityStats, AgentActivitySummary, AgentActivitySummaryStaleReason,
    AgentActivitySummaryState, AgentControlProgressKind, AgentId, BackgroundTaskState,
    BackgroundTaskStatus, CancelQueuedMessagePayload, FrameKind, QueuedMessageId,
    SendQueuedMessageNowPayload, SubAgentProgress, ToolProgressUpdate, ToolRequestType,
    WorkflowRunState, WorkflowRunStatus,
};

use crate::components::chat_message::token_badge_data;
use crate::components::tool_card::{agent_display_name, open_child_agent};
use crate::components::workflow_view::run_status_label;
use crate::send::send_frame;
use crate::state::{ActiveAgentRef, AppState, TabContent, ToolCallId};

const STORAGE_INFLIGHT_TRAY_EXPANDED: &str = "tyde-inflight-tray-expanded";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_expanded() -> bool {
    local_storage()
        .and_then(|storage| {
            storage
                .get_item(STORAGE_INFLIGHT_TRAY_EXPANDED)
                .ok()
                .flatten()
        })
        .map(|value| value == "true")
        .unwrap_or(false)
}

fn persist_expanded(expanded: bool) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(
            STORAGE_INFLIGHT_TRAY_EXPANDED,
            if expanded { "true" } else { "false" },
        );
    }
}

/// Live status of one first-class child agent, derived from server-owned
/// state (`agents`, `agent_turn_active`, `streaming_text`) — the same
/// derivation the old spawn/await card rows used, now in exactly one place.
#[derive(Clone, PartialEq)]
enum ChildAgentStatus {
    Starting,
    Running,
    Idle,
    Failed(String),
}

impl ChildAgentStatus {
    fn label(&self) -> String {
        match self {
            Self::Starting => "Starting".to_owned(),
            Self::Running => "Running".to_owned(),
            Self::Idle => "Idle".to_owned(),
            Self::Failed(message) if message.trim().is_empty() => "Failed".to_owned(),
            Self::Failed(message) => format!("Failed: {}", truncate_inline(message, 72)),
        }
    }

    fn class(&self) -> &'static str {
        match self {
            Self::Starting | Self::Running => "tool-live-agent-status running",
            Self::Idle => "tool-live-agent-status idle",
            Self::Failed(_) => "tool-live-agent-status failed",
        }
    }
}

fn derive_child_status(state: &AppState, agent: &crate::state::AgentInfo) -> ChildAgentStatus {
    if let Some(error) = agent.fatal_error.clone() {
        return ChildAgentStatus::Failed(error);
    }
    if !agent.started {
        return ChildAgentStatus::Starting;
    }
    let typing = state
        .agent_turn_active
        .with(|map| map.get(&agent.agent_id).copied().unwrap_or(false));
    let streaming = state
        .streaming_text
        .with(|map| map.contains_key(&agent.agent_id));
    if typing || streaming {
        ChildAgentStatus::Running
    } else {
        ChildAgentStatus::Idle
    }
}

/// Session-local dismissal keys. Dismissing hides an item only in its
/// terminal/idle state — an idle child that starts working again reappears.
fn child_key(agent_id: &AgentId) -> String {
    format!("ag:{}", agent_id.0)
}

fn workflow_key(call_id: &ToolCallId) -> String {
    format!("wf:{}", call_id.0)
}

fn subagent_key(call_id: &ToolCallId) -> String {
    format!("sa:{}", call_id.0)
}

fn command_key(call_id: &ToolCallId) -> String {
    format!("cmd:{}", call_id.0)
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct TrayCounts {
    running: usize,
    failed: usize,
    finished: usize,
    queued: usize,
}

/// Identity snapshot of everything the tray shows. Rows resolve their own
/// live detail reactively; this memo only decides *which* rows exist and
/// the header counts, so per-snapshot progress updates don't remount rows.
#[derive(Clone, PartialEq, Default)]
struct TraySnapshot {
    children: Vec<AgentId>,
    /// Agents referenced by a spawn's progress payload that have no registry
    /// record yet — the gap between the spawn result and the `NewAgent`
    /// frame. Rendered optimistically as "Starting", exactly as the spawn
    /// card's live rows treated an unknown spawned agent.
    pending_spawns: Vec<(AgentId, Option<String>)>,
    workflows: Vec<ToolCallId>,
    subagents: Vec<ToolCallId>,
    /// Backgrounded shell commands (`run_in_background` Bash calls), from
    /// server-reduced `BackgroundTask` progress snapshots.
    commands: Vec<ToolCallId>,
    queued: Vec<QueuedMessageId>,
    counts: TrayCounts,
}

impl TraySnapshot {
    fn is_empty(&self) -> bool {
        self.children.is_empty()
            && self.pending_spawns.is_empty()
            && self.workflows.is_empty()
            && self.subagents.is_empty()
            && self.commands.is_empty()
            && self.queued.is_empty()
    }
}

fn compute_snapshot(
    state: &AppState,
    parent: &ActiveAgentRef,
    dismissed: &HashSet<String>,
) -> TraySnapshot {
    let mut snapshot = TraySnapshot::default();
    let mut child_ids: HashSet<AgentId> = HashSet::new();

    state.agents.with(|agents| {
        for agent in agents {
            if agent.host_id != parent.host_id
                || agent.parent_agent_id.as_ref() != Some(&parent.agent_id)
            {
                continue;
            }
            child_ids.insert(agent.agent_id.clone());
            match derive_child_status(state, agent) {
                ChildAgentStatus::Starting | ChildAgentStatus::Running => {
                    snapshot.counts.running += 1;
                }
                ChildAgentStatus::Idle => {
                    if dismissed.contains(&child_key(&agent.agent_id)) {
                        continue;
                    }
                    snapshot.counts.finished += 1;
                }
                ChildAgentStatus::Failed(_) => {
                    if dismissed.contains(&child_key(&agent.agent_id)) {
                        continue;
                    }
                    snapshot.counts.failed += 1;
                }
            }
            snapshot.children.push(agent.agent_id.clone());
        }
    });

    state.tool_progress.with(|map| {
        for ((agent_id, call_id), progress) in map.iter() {
            if *agent_id != parent.agent_id {
                continue;
            }
            match progress.get().update {
                ToolProgressUpdate::Workflow(run) => {
                    match run.status {
                        WorkflowRunStatus::Running => snapshot.counts.running += 1,
                        WorkflowRunStatus::Failed => {
                            if dismissed.contains(&workflow_key(call_id)) {
                                continue;
                            }
                            snapshot.counts.failed += 1;
                        }
                        WorkflowRunStatus::Completed | WorkflowRunStatus::Unknown => {
                            if dismissed.contains(&workflow_key(call_id)) {
                                continue;
                            }
                            snapshot.counts.finished += 1;
                        }
                    }
                    snapshot.workflows.push(call_id.clone());
                }
                ToolProgressUpdate::SubAgent(sub) => {
                    // A sub-agent with a registry record already renders as a
                    // child row; a second row here would recreate the exact
                    // spawn/await duplication this tray exists to remove.
                    if child_ids.contains(&sub.agent_id) {
                        continue;
                    }
                    if sub.completed {
                        if dismissed.contains(&subagent_key(call_id)) {
                            continue;
                        }
                        snapshot.counts.finished += 1;
                    } else {
                        snapshot.counts.running += 1;
                    }
                    snapshot.subagents.push(call_id.clone());
                }
                ToolProgressUpdate::AgentControl(progress)
                    if progress.progress_kind == AgentControlProgressKind::Spawn =>
                {
                    for agent in progress.agents {
                        if child_ids.contains(&agent.agent_id)
                            || snapshot
                                .pending_spawns
                                .iter()
                                .any(|(id, _)| *id == agent.agent_id)
                        {
                            continue;
                        }
                        snapshot.counts.running += 1;
                        snapshot.pending_spawns.push((agent.agent_id, agent.name));
                    }
                }
                ToolProgressUpdate::BackgroundTask(task) => {
                    match task.status {
                        BackgroundTaskStatus::Running => snapshot.counts.running += 1,
                        BackgroundTaskStatus::Failed => {
                            if dismissed.contains(&command_key(call_id)) {
                                continue;
                            }
                            snapshot.counts.failed += 1;
                        }
                        BackgroundTaskStatus::Completed
                        | BackgroundTaskStatus::Stopped
                        | BackgroundTaskStatus::Unknown => {
                            if dismissed.contains(&command_key(call_id)) {
                                continue;
                            }
                            snapshot.counts.finished += 1;
                        }
                    }
                    snapshot.commands.push(call_id.clone());
                }
                _ => {}
            }
        }
    });
    // HashMap iteration order is unstable; sort so rows don't shuffle
    // between renders.
    snapshot.workflows.sort_by(|a, b| a.0.cmp(&b.0));
    snapshot.subagents.sort_by(|a, b| a.0.cmp(&b.0));
    snapshot.commands.sort_by(|a, b| a.0.cmp(&b.0));
    snapshot.pending_spawns.sort_by(|a, b| a.0.0.cmp(&b.0.0));

    snapshot.queued = state.agent_message_queue.with(|queue| {
        queue
            .get(&parent.agent_id)
            .map(|entries| entries.iter().map(|entry| entry.id.clone()).collect())
            .unwrap_or_default()
    });
    snapshot.counts.queued = snapshot.queued.len();
    snapshot
}

/// Keys of everything currently in a finished (idle/completed) state, for
/// the "Clear finished" action. Failures are deliberately excluded — they
/// stay pinned until individually dismissed.
fn finished_keys_untracked(state: &AppState, parent: &ActiveAgentRef) -> Vec<String> {
    let mut keys = Vec::new();
    state.agents.with_untracked(|agents| {
        for agent in agents {
            if agent.host_id != parent.host_id
                || agent.parent_agent_id.as_ref() != Some(&parent.agent_id)
            {
                continue;
            }
            if agent.fatal_error.is_none()
                && agent.started
                && !state
                    .agent_turn_active
                    .with_untracked(|map| map.get(&agent.agent_id).copied().unwrap_or(false))
                && !state
                    .streaming_text
                    .with_untracked(|map| map.contains_key(&agent.agent_id))
            {
                keys.push(child_key(&agent.agent_id));
            }
        }
    });
    state.tool_progress.with_untracked(|map| {
        for ((agent_id, call_id), progress) in map.iter() {
            if *agent_id != parent.agent_id {
                continue;
            }
            match progress.get_untracked().update {
                ToolProgressUpdate::Workflow(run)
                    if run.status != WorkflowRunStatus::Running
                        && run.status != WorkflowRunStatus::Failed =>
                {
                    keys.push(workflow_key(call_id));
                }
                ToolProgressUpdate::SubAgent(sub) if sub.completed => {
                    keys.push(subagent_key(call_id));
                }
                ToolProgressUpdate::BackgroundTask(task)
                    if task.status != BackgroundTaskStatus::Running
                        && task.status != BackgroundTaskStatus::Failed =>
                {
                    keys.push(command_key(call_id));
                }
                _ => {}
            }
        }
    });
    keys
}

#[component]
pub fn InflightTray(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();

    let expanded = RwSignal::new(load_expanded());
    let dismissed: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());

    let snapshot = Memo::new({
        let state = state.clone();
        move |_| {
            let Some(parent) = agent_ref.get() else {
                return TraySnapshot::default();
            };
            dismissed.with(|dismissed| compute_snapshot(&state, &parent, dismissed))
        }
    });

    // What the model is blocked on right now: the newest streaming tool
    // request whose result has not arrived. `ToolExecutionCompleted`
    // patches these entries in place (see dispatch), so `result: None`
    // means genuinely pending — this never guesses.
    let waiting = Signal::derive({
        let state = state.clone();
        move || -> Option<String> {
            let parent = agent_ref.get()?;
            let requests = state
                .streaming_text
                .with(|map| map.get(&parent.agent_id).map(|s| s.tool_requests.clone()))?;
            let pending = requests.with(|requests| {
                requests.iter().rev().find_map(|request| {
                    request.entry.with(|entry| {
                        entry
                            .result
                            .is_none()
                            .then(|| entry.request.tool_type.clone())
                    })
                })
            })?;
            match &pending {
                ToolRequestType::TydeAwaitAgents { agent_ids } => {
                    let names = agent_ids
                        .iter()
                        .map(|id| agent_display_name(&state, Some(parent.clone()), id, None))
                        .collect::<Vec<_>>()
                        .join(", ");
                    Some(format!("waiting on {names}"))
                }
                _ => None,
            }
        }
    });

    let header_text = move || {
        let snapshot = snapshot.get();
        let mut parts = Vec::new();
        if snapshot.counts.running > 0 {
            parts.push(format!("{} running", snapshot.counts.running));
        }
        if snapshot.counts.failed > 0 {
            parts.push(format!("{} failed", snapshot.counts.failed));
        }
        if snapshot.counts.queued > 0 {
            parts.push(format!("{} queued", snapshot.counts.queued));
        }
        if snapshot.counts.finished > 0 {
            parts.push(format!("{} finished", snapshot.counts.finished));
        }
        if let Some(waiting) = waiting.get() {
            parts.push(waiting);
        }
        parts.join(" \u{b7} ")
    };

    let has_finished = move || snapshot.get().counts.finished > 0;

    let on_toggle = move |_: web_sys::MouseEvent| {
        let next = !expanded.get_untracked();
        expanded.set(next);
        persist_expanded(next);
    };

    // Parked in a `StoredValue` so the handler's captures are all `Copy`:
    // the `Show` children closure must stay `Fn`, and a captured `AppState`
    // would degrade it to `FnOnce`.
    let clear_state = StoredValue::new_local(state.clone());
    let on_clear = move |_: web_sys::MouseEvent| {
        let Some(parent) = agent_ref.get_untracked() else {
            return;
        };
        let keys = clear_state.with_value(|state| finished_keys_untracked(state, &parent));
        dismissed.update(|set| set.extend(keys));
    };

    view! {
        <Show when=move || !snapshot.get().is_empty()>
            <div class="inflight-tray">
                <button type="button" class="inflight-tray-header" on:click=on_toggle>
                    <span
                        class="inflight-tray-chevron"
                        class:expanded=move || expanded.get()
                        aria-hidden="true"
                    >
                        "\u{25b6}"
                    </span>
                    <span class="inflight-tray-summary">{header_text}</span>
                </button>
                <Show when=move || expanded.get()>
                    <div class="inflight-tray-body">
                        <For
                            each=move || snapshot.get().children
                            key=|agent_id| agent_id.0.clone()
                            let:agent_id
                        >
                            <ChildAgentRow
                                parent_ref=agent_ref
                                agent_id=agent_id
                                dismissed=dismissed
                            />
                        </For>
                        <For
                            each=move || snapshot.get().pending_spawns
                            key=|(agent_id, _)| agent_id.0.clone()
                            let:pending
                        >
                            <div class="tool-live-agent-row inflight-tray-row">
                                <div class="tool-live-agent-main">
                                    <span class="tool-live-agent-name">
                                        {pending.1.unwrap_or_else(|| pending.0.0.clone())}
                                    </span>
                                    <span class="tool-live-agent-status running">"Starting"</span>
                                </div>
                            </div>
                        </For>
                        <For
                            each=move || snapshot.get().workflows
                            key=|call_id| call_id.0.clone()
                            let:call_id
                        >
                            <WorkflowRow
                                parent_ref=agent_ref
                                tool_call_id=call_id
                                dismissed=dismissed
                            />
                        </For>
                        <For
                            each=move || snapshot.get().subagents
                            key=|call_id| call_id.0.clone()
                            let:call_id
                        >
                            <SubagentRow parent_ref=agent_ref tool_call_id=call_id />
                        </For>
                        <For
                            each=move || snapshot.get().commands
                            key=|call_id| call_id.0.clone()
                            let:call_id
                        >
                            <CommandRow
                                parent_ref=agent_ref
                                tool_call_id=call_id
                                dismissed=dismissed
                            />
                        </For>
                        <Show when=move || !snapshot.get().queued.is_empty()>
                            <div class="inflight-tray-queue">
                                <For
                                    each=move || snapshot.get().queued
                                    key=|id| id.0.clone()
                                    let:id
                                >
                                    <QueuedMessageRow id=id agent_ref=agent_ref />
                                </For>
                            </div>
                        </Show>
                        <Show when=has_finished>
                            <button
                                type="button"
                                class="inflight-tray-clear"
                                on:click=on_clear
                            >
                                "Clear finished"
                            </button>
                        </Show>
                    </div>
                </Show>
            </div>
        </Show>
    }
}

/// One first-class child agent: live name, status, streaming preview while it
/// runs, server-owned activity summary/stats, and an open action. This is the
/// merged successor of the spawn card's preview row and the await card's
/// summary/stats row — one row, one surface.
#[component]
fn ChildAgentRow(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    agent_id: AgentId,
    dismissed: RwSignal<HashSet<String>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let display_name = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || agent_display_name(&state, parent_ref.get(), &agent_id, None)
    });

    let status = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || {
            let parent = parent_ref.get()?;
            let agent = state.agents.with(|agents| {
                agents
                    .iter()
                    .find(|agent| agent.host_id == parent.host_id && agent.agent_id == agent_id)
                    .cloned()
            })?;
            Some(derive_child_status(&state, &agent))
        }
    });

    let preview = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || {
            let handles = state.streaming_text.with(|map| {
                map.get(&agent_id)
                    .map(|stream| (stream.text.clone(), stream.reasoning.clone()))
            })?;
            let text = handles.0.get();
            let preview_source = if text.trim().is_empty() {
                handles.1.get()
            } else {
                text
            };
            streaming_preview(&preview_source)
        }
    });

    // Server-owned activity summary, rendered verbatim — the frontend never
    // infers a summary from streaming text. Shown only while no live
    // streaming preview is available, so a row carries one detail line.
    let activity_summary = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || {
            let parent = parent_ref.get()?;
            state.agents.with(|agents| {
                agents
                    .iter()
                    .find(|agent| agent.host_id == parent.host_id && agent.agent_id == agent_id)
                    .map(|agent| agent.activity_summary.clone())
            })
        }
    });

    let activity_stats: Signal<Option<AgentActivityStats>> = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || {
            let parent = parent_ref.get()?;
            let key = ActiveAgentRef {
                host_id: parent.host_id,
                agent_id: agent_id.clone(),
            };
            state
                .agent_activity_stats
                .with(|map| map.get(&key).cloned())
        }
    });

    let open_state = state.clone();
    let open_agent_id = agent_id.clone();
    let on_open = move |_: web_sys::MouseEvent| {
        let Some(parent) = parent_ref.get_untracked() else {
            log::error!("Open agent clicked on an in-flight row with no resolved agent");
            return;
        };
        open_child_agent(&open_state, &parent.host_id, &open_agent_id);
    };

    // Key precomputed into a `StoredValue` so the dismiss handler's captures
    // are all `Copy` — the failed-only `Show` children closure must stay `Fn`.
    let dismiss_key = StoredValue::new_local(child_key(&agent_id));
    let on_dismiss = move |_: web_sys::MouseEvent| {
        dismissed.update(|set| {
            set.insert(dismiss_key.get_value());
        });
    };
    let is_failed =
        Signal::derive(move || matches!(status.get(), Some(ChildAgentStatus::Failed(_))));

    view! {
        <div class="tool-live-agent-row inflight-tray-row">
            <div class="tool-live-agent-main">
                <span class="tool-live-agent-name">{move || display_name.get()}</span>
                {move || status.get().map(|status| view! {
                    <span class=status.class()>{status.label()}</span>
                })}
            </div>
            <div class="inflight-tray-row-actions">
                <button type="button" class="tool-live-link" on:click=on_open>"Open agent"</button>
                <Show when=move || is_failed.get()>
                    <button
                        type="button"
                        class="inflight-tray-dismiss"
                        title="Dismiss this failure"
                        on:click=on_dismiss
                    >
                        "\u{d7}"
                    </button>
                </Show>
            </div>
            {move || {
                if let Some(text) = preview.get() {
                    return Some(
                        view! { <div class="tool-live-agent-preview">{text}</div> }.into_any(),
                    );
                }
                match activity_summary.get().unwrap_or_default() {
                    AgentActivitySummaryState::Disabled => activity_stats
                        .get()
                        .and_then(|stats| stats.last_output_line)
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| {
                            view! { <div class="tool-live-agent-output">{line}</div> }.into_any()
                        }),
                    enabled => agent_activity_summary_view(enabled),
                }
            }}
            {move || {
                activity_stats
                    .get()
                    .filter(stats_has_content)
                    .map(agent_control_stats_line)
            }}
        </div>
    }
}

/// One workflow run: name, status, per-agent completion count, and a link to
/// the dedicated workflow tab. Live run detail (phase rows) stays on the
/// workflow tab and card; the tray row is the at-a-glance status.
#[component]
fn WorkflowRow(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    tool_call_id: ToolCallId,
    dismissed: RwSignal<HashSet<String>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let run: Signal<Option<WorkflowRunState>> = Signal::derive({
        let state = state.clone();
        let tool_call_id = tool_call_id.clone();
        move || {
            let parent = parent_ref.get()?;
            let key = (parent.agent_id, tool_call_id.clone());
            let progress = state.tool_progress.with(|map| map.get(&key).cloned())?;
            match progress.get().update {
                ToolProgressUpdate::Workflow(run) => Some(run),
                _ => None,
            }
        }
    });

    let title = move || {
        run.get().map(|run| {
            let done = run
                .agents
                .iter()
                .filter(|agent| agent.state == protocol::WorkflowAgentStatus::Done)
                .count();
            let total = run.agents.len();
            if total > 0 {
                format!("{} \u{b7} {done}/{total} agents done", run.workflow_name)
            } else {
                run.workflow_name
            }
        })
    };

    let status_label = move || run.get().map(|run| run_status_label(run.status).to_owned());
    let status_class = move || match run.get().map(|run| run.status) {
        Some(WorkflowRunStatus::Running) => "tool-live-agent-status running",
        Some(WorkflowRunStatus::Failed) => "tool-live-agent-status failed",
        _ => "tool-live-agent-status idle",
    };

    let open_state = state.clone();
    let open_call_id = tool_call_id.clone();
    let on_open = move |_: web_sys::MouseEvent| {
        let Some(parent) = parent_ref.get_untracked() else {
            log::error!("Open workflow clicked on an in-flight row with no resolved agent");
            return;
        };
        let Some(run) = run.get_untracked() else {
            log::error!("Open workflow clicked before any run snapshot");
            return;
        };
        open_state.open_tab(
            TabContent::Workflow {
                agent_ref: parent,
                tool_call_id: open_call_id.clone(),
            },
            format!("Workflow: {}", run.workflow_name),
            true,
        );
    };

    // Same `Copy`-captures requirement as the child row's dismiss handler.
    let dismiss_key = StoredValue::new_local(workflow_key(&tool_call_id));
    let on_dismiss = move |_: web_sys::MouseEvent| {
        dismissed.update(|set| {
            set.insert(dismiss_key.get_value());
        });
    };
    let is_failed = Signal::derive(move || {
        matches!(
            run.get().map(|run| run.status),
            Some(WorkflowRunStatus::Failed)
        )
    });

    view! {
        <div class="tool-live-agent-row inflight-tray-row">
            <div class="tool-live-agent-main">
                <span class="tool-live-agent-name">{title}</span>
                <span class=status_class>{status_label}</span>
            </div>
            <div class="inflight-tray-row-actions">
                <button type="button" class="tool-live-link" on:click=on_open>
                    "Open workflow"
                </button>
                <Show when=move || is_failed.get()>
                    <button
                        type="button"
                        class="inflight-tray-dismiss"
                        title="Dismiss this failure"
                        on:click=on_dismiss
                    >
                        "\u{d7}"
                    </button>
                </Show>
            </div>
        </div>
    }
}

fn background_status_label(status: BackgroundTaskStatus) -> &'static str {
    match status {
        BackgroundTaskStatus::Running => "Running",
        BackgroundTaskStatus::Completed => "Completed",
        BackgroundTaskStatus::Stopped => "Stopped",
        BackgroundTaskStatus::Failed => "Failed",
        BackgroundTaskStatus::Unknown => "Unknown",
    }
}

/// One backgrounded shell command. While it runs the row shows the
/// command's description; once terminal, the CLI's notification summary
/// (the only place the exit code exists) replaces it when present.
#[component]
fn CommandRow(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    tool_call_id: ToolCallId,
    dismissed: RwSignal<HashSet<String>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let task: Signal<Option<BackgroundTaskState>> = Signal::derive({
        let state = state.clone();
        let tool_call_id = tool_call_id.clone();
        move || {
            let parent = parent_ref.get()?;
            let key = (parent.agent_id, tool_call_id.clone());
            let progress = state.tool_progress.with(|map| map.get(&key).cloned())?;
            match progress.get().update {
                ToolProgressUpdate::BackgroundTask(task) => Some(task),
                _ => None,
            }
        }
    });

    let title = move || {
        task.get().map(|task| {
            let fallback = || format!("Background command {}", task.task_id);
            if task.status == BackgroundTaskStatus::Running {
                task.description.clone().unwrap_or_else(fallback)
            } else {
                task.summary
                    .clone()
                    .or_else(|| task.description.clone())
                    .unwrap_or_else(fallback)
            }
        })
    };

    let status_label = move || {
        task.get()
            .map(|task| background_status_label(task.status).to_owned())
    };
    let status_class = move || match task.get().map(|task| task.status) {
        Some(BackgroundTaskStatus::Running) => "tool-live-agent-status running",
        Some(BackgroundTaskStatus::Failed) => "tool-live-agent-status failed",
        _ => "tool-live-agent-status idle",
    };

    // Same `Copy`-captures requirement as the other rows' dismiss handlers.
    let dismiss_key = StoredValue::new_local(command_key(&tool_call_id));
    let on_dismiss = move |_: web_sys::MouseEvent| {
        dismissed.update(|set| {
            set.insert(dismiss_key.get_value());
        });
    };
    let is_failed = Signal::derive(move || {
        matches!(
            task.get().map(|task| task.status),
            Some(BackgroundTaskStatus::Failed)
        )
    });

    view! {
        <div class="tool-live-agent-row inflight-tray-row">
            <div class="tool-live-agent-main">
                <span class="tool-live-agent-name">{title}</span>
                <span class=status_class>{status_label}</span>
            </div>
            <Show when=move || is_failed.get()>
                <div class="inflight-tray-row-actions">
                    <button
                        type="button"
                        class="inflight-tray-dismiss"
                        title="Dismiss this failure"
                        on:click=on_dismiss
                    >
                        "\u{d7}"
                    </button>
                </div>
            </Show>
        </div>
    }
}

/// A native sub-agent that has live progress but no registry record — the
/// only case a `SubAgentProgress` snapshot is the sole evidence it exists.
/// With no registry record its owning project can't be resolved, so no open
/// action is offered (a button that logs an error is worse than none).
#[component]
fn SubagentRow(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    tool_call_id: ToolCallId,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let progress: Signal<Option<SubAgentProgress>> = Signal::derive({
        let state = state.clone();
        let tool_call_id = tool_call_id.clone();
        move || {
            let parent = parent_ref.get()?;
            let key = (parent.agent_id, tool_call_id.clone());
            let progress = state.tool_progress.with(|map| map.get(&key).cloned())?;
            match progress.get().update {
                ToolProgressUpdate::SubAgent(sub) => Some(sub),
                _ => None,
            }
        }
    });

    let title = move || {
        progress.get().map(|sub| {
            if sub.completed {
                format!(
                    "\u{2713} {} finished \u{b7} {} tool calls",
                    sub.agent_name, sub.tool_calls
                )
            } else {
                let last_tool = sub
                    .last_tool_name
                    .map(|name| format!(" \u{b7} last tool: {name}"))
                    .unwrap_or_default();
                format!(
                    "\u{27f3} {} running{last_tool} \u{b7} {} tool calls",
                    sub.agent_name, sub.tool_calls
                )
            }
        })
    };

    view! {
        <div class="tool-live-agent-row inflight-tray-row">
            <div class="tool-live-agent-main">
                <span class="tool-live-agent-name">{title}</span>
            </div>
        </div>
    }
}

/// One queued outbound message with its send-now/cancel actions. Lives in
/// the tray (not the composer) because a queued message is in-flight session
/// state: it exists exactly while the agent is busy, alongside the running
/// work it is queued behind.
#[component]
fn QueuedMessageRow(
    id: QueuedMessageId,
    agent_ref: Signal<Option<ActiveAgentRef>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let id_for_lookup = id.clone();
    let id_for_send = id.clone();
    let id_for_cancel = id.clone();
    let state_preview = state.clone();
    let state_send = state.clone();
    let state_cancel = state.clone();

    let preview = move || {
        let Some(active) = agent_ref.get() else {
            return String::new();
        };
        let queue = state_preview.agent_message_queue.get();
        let Some(entries) = queue.get(&active.agent_id) else {
            return String::new();
        };
        let Some(entry) = entries.iter().find(|entry| entry.id == id_for_lookup) else {
            return String::new();
        };
        let chars: Vec<char> = entry.message.chars().collect();
        if chars.len() > 80 {
            chars[..80].iter().collect::<String>() + "…"
        } else {
            entry.message.clone()
        }
    };

    let on_send_now = move |_| {
        let Some(active) = agent_ref.get_untracked() else {
            return;
        };
        let agents = state_send.agents.get_untracked();
        let Some(agent) = agents
            .iter()
            .find(|agent| agent.host_id == active.host_id && agent.agent_id == active.agent_id)
        else {
            return;
        };
        let host_id = agent.host_id.clone();
        let stream = agent.instance_stream.clone();
        let id = id_for_send.clone();
        spawn_local(async move {
            if let Err(error) = send_frame(
                &host_id,
                stream,
                FrameKind::SendQueuedMessageNow,
                &SendQueuedMessageNowPayload { id },
            )
            .await
            {
                log::error!("failed to send send_queued_message_now: {error}");
            }
        });
    };

    let on_cancel = move |_| {
        let Some(active) = agent_ref.get_untracked() else {
            return;
        };
        let agents = state_cancel.agents.get_untracked();
        let Some(agent) = agents
            .iter()
            .find(|agent| agent.host_id == active.host_id && agent.agent_id == active.agent_id)
        else {
            return;
        };
        let host_id = agent.host_id.clone();
        let stream = agent.instance_stream.clone();
        let id = id_for_cancel.clone();
        spawn_local(async move {
            if let Err(error) = send_frame(
                &host_id,
                stream,
                FrameKind::CancelQueuedMessage,
                &CancelQueuedMessagePayload { id },
            )
            .await
            {
                log::error!("failed to send cancel_queued_message: {error}");
            }
        });
    };

    view! {
        <div class="queued-message-item">
            <span class="queued-message-preview">{preview}</span>
            <button
                class="queued-message-btn queued-message-send-now"
                title="Send this message now"
                on:click=on_send_now
            >
                "↑ Send Now"
            </button>
            <button
                class="queued-message-btn queued-message-cancel"
                title="Cancel this queued message"
                on:click=on_cancel
            >
                "× Cancel"
            </button>
        </div>
    }
}

// ── Shared live-row formatting helpers ──────────────────────────────────
// Moved here from `tool_card` together with the live rows themselves; the
// tray is now the only surface that renders live per-agent detail.

fn token_usage_has_content(tokens: &protocol::TokenUsage) -> bool {
    tokens.input_tokens > 0
        || tokens.output_tokens > 0
        || tokens.cached_prompt_tokens.unwrap_or(0) > 0
        || tokens.cache_creation_input_tokens.unwrap_or(0) > 0
        || tokens.reasoning_tokens.unwrap_or(0) > 0
}

fn stats_has_content(stats: &AgentActivityStats) -> bool {
    stats.tool_calls > 0 || token_usage_has_content(&stats.token_usage)
}

/// Render an agent row's server-owned stats line: the running tool-call count
/// and token usage, formatted with the shared token badge helper so it reads
/// identically to the chat token UI (`↑input (cached) · ↓output (reasoning)`).
///
/// The token spans (and their reasoning/cache tooltip) are only rendered when
/// the agent has actually reported non-zero usage — a tool-call-only agent
/// shows just its tool-call count, never a fake `↑0 · ↓0` badge.
fn agent_control_stats_line(stats: AgentActivityStats) -> AnyView {
    let tool_label = if stats.tool_calls == 1 {
        "1 tool call".to_owned()
    } else {
        format!("{} tool calls", stats.tool_calls)
    };
    let token_spans = token_usage_has_content(&stats.token_usage).then(|| {
        let (input_text, output_text, tooltip) = token_badge_data(&stats.token_usage);
        view! {
            <span class="token-sep">"\u{00b7}"</span>
            <span class="token-stat token-stat-input" title=tooltip>{input_text}</span>
            <span class="token-sep">"\u{00b7}"</span>
            <span class="token-stat token-stat-output">{output_text}</span>
        }
    });
    view! {
        <div class="tool-live-agent-stats">
            <span class="tool-live-agent-stats-tools">{tool_label}</span>
            {token_spans}
        </div>
    }
    .into_any()
}

/// Render the server-owned activity summary/status for an enabled agent row.
/// Returns `Some` for every enabled state that has something to show — summary
/// text (`Fresh`, `Stale`, `Pending`/`Error` with a previous summary) or a
/// status placeholder (`Pending` → "summarizing…", `Error` → "summary
/// unavailable"). Only `Disabled` and `Empty` return `None`. Crucially, when
/// summaries are enabled the row shows this and *never* the output line, so a
/// no-text state must surface a status here rather than leaking the output.
/// The freshness/stale/error framing comes straight from the server enum; the
/// frontend only formats the timestamp for display.
fn agent_activity_summary_view(state: AgentActivitySummaryState) -> Option<AnyView> {
    match state {
        AgentActivitySummaryState::Disabled | AgentActivitySummaryState::Empty => None,
        AgentActivitySummaryState::Pending { previous, .. } => match previous {
            Some(summary) => Some(
                view! {
                    <div class="tool-live-agent-summary">
                        <span class="tool-live-agent-summary-text">{summary.text}</span>
                        <span class="tool-live-agent-summary-meta updating">"updating\u{2026}"</span>
                    </div>
                }
                .into_any(),
            ),
            None => Some(
                view! {
                    <div class="tool-live-agent-summary">
                        <span class="tool-live-agent-summary-meta pending">"summarizing\u{2026}"</span>
                    </div>
                }
                .into_any(),
            ),
        },
        AgentActivitySummaryState::Fresh { summary } => {
            let freshness = format_summary_age(&summary);
            Some(
                view! {
                    <div class="tool-live-agent-summary">
                        <span class="tool-live-agent-summary-text">{summary.text}</span>
                        <span class="tool-live-agent-summary-meta">{freshness}</span>
                    </div>
                }
                .into_any(),
            )
        }
        AgentActivitySummaryState::Stale { summary, reason } => {
            let hint = match reason {
                AgentActivitySummaryStaleReason::NewActivity => "stale \u{00b7} new activity",
                AgentActivitySummaryStaleReason::MaxAge => "stale",
            };
            Some(
                view! {
                    <div class="tool-live-agent-summary">
                        <span class="tool-live-agent-summary-text">{summary.text}</span>
                        <span class="tool-live-agent-summary-meta stale">{hint}</span>
                    </div>
                }
                .into_any(),
            )
        }
        AgentActivitySummaryState::Error { previous, .. } => Some(
            view! {
                <div class="tool-live-agent-summary">
                    {previous.map(|summary| {
                        view! {
                            <span class="tool-live-agent-summary-text">{summary.text}</span>
                        }
                    })}
                    <span class="tool-live-agent-summary-meta error">"summary unavailable"</span>
                </div>
            }
            .into_any(),
        ),
    }
}

/// Compact "updated Ns ago" freshness label derived from the summary's
/// `generated_at_ms`. Mirrors the relative-time scheme used elsewhere in chat.
fn format_summary_age(summary: &AgentActivitySummary) -> String {
    if summary.generated_at_ms == 0 {
        return "updated just now".to_owned();
    }
    let now_ms = js_sys::Date::now() as u64;
    let diff_secs = now_ms.saturating_sub(summary.generated_at_ms) / 1000;
    if diff_secs < 60 {
        "updated just now".to_owned()
    } else if diff_secs < 3600 {
        format!("updated {}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("updated {}h ago", diff_secs / 3600)
    } else {
        format!("updated {}d ago", diff_secs / 86400)
    }
}

fn streaming_preview(text: &str) -> Option<String> {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(truncate_inline(&compact, 140))
    }
}

fn truncate_inline(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}\u{2026}")
    } else {
        truncated
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::{count, make_container, next_tick, text};
    use crate::state::{AgentInfo, ToolCallId};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, QueuedMessageEntry, StreamPath, ToolProgressData,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn parent_ref() -> ActiveAgentRef {
        ActiveAgentRef {
            host_id: "host-1".to_owned(),
            agent_id: AgentId("agent-parent".to_owned()),
        }
    }

    fn child_agent(agent_id: &str, name: &str) -> AgentInfo {
        AgentInfo {
            host_id: "host-1".to_owned(),
            agent_id: AgentId(agent_id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::AgentControl,
            backend_kind: BackendKind::Codex,
            workspace_roots: vec!["/tmp/work".to_owned()],
            project_id: None,
            parent_agent_id: Some(parent_ref().agent_id),
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 1,
            instance_stream: StreamPath(format!("/agents/{agent_id}")),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn mount_tray(setup: impl FnOnce(&AppState) + 'static) -> (HtmlElement, AppState) {
        // The tray persists its expanded state; clear it so one test's
        // expansion can't leak into the next.
        if let Some(storage) = local_storage() {
            let _ = storage.remove_item(STORAGE_INFLIGHT_TRAY_EXPANDED);
        }
        let state = AppState::new();
        setup(&state);
        let container = make_container();
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            let agent_ref = Signal::derive(|| Some(parent_ref()));
            view! { <InflightTray agent_ref=agent_ref /> }
        });
        handle.forget();
        (container, state)
    }

    async fn expand(container: &HtmlElement) {
        let header = container
            .query_selector(".inflight-tray-header")
            .unwrap()
            .expect("tray header exists")
            .dyn_into::<HtmlElement>()
            .unwrap();
        header.click();
        next_tick().await;
    }

    /// The common case — nothing in flight — must cost zero chrome: no tray
    /// element at all, not an empty shell.
    #[wasm_bindgen_test]
    async fn tray_hidden_when_nothing_in_flight() {
        let (container, _state) = mount_tray(|_| {});
        next_tick().await;
        assert_eq!(count(&container, ".inflight-tray"), 0, "no tray when idle");
    }

    /// A running child agent produces the collapsed count line and, once
    /// expanded, exactly one live row naming the agent with its status.
    #[wasm_bindgen_test]
    async fn running_child_renders_one_live_row_and_count() {
        let (container, _state) = mount_tray(|state| {
            state
                .agents
                .update(|agents| agents.push(child_agent("agent-a", "Builder")));
            state.agent_turn_active.update(|map| {
                map.insert(AgentId("agent-a".to_owned()), true);
            });
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 running"),
            "collapsed line reports the running count: {body}"
        );

        expand(&container).await;
        assert_eq!(
            count(&container, ".inflight-tray-row"),
            1,
            "exactly one live row for one child"
        );
        let body = text(&container);
        assert!(
            body.contains("Builder") && body.contains("Running"),
            "the row names the agent with live status: {body}"
        );
    }

    /// A queued message is in-flight state: it must appear in the tray with
    /// its preview and both actions, and be counted in the collapsed line.
    #[wasm_bindgen_test]
    async fn queued_message_renders_with_actions() {
        let (container, _state) = mount_tray(|state| {
            state.agent_message_queue.update(|queue| {
                queue.insert(
                    parent_ref().agent_id,
                    vec![QueuedMessageEntry {
                        id: QueuedMessageId("q-1".to_owned()),
                        message: "also fix the flaky test".to_owned(),
                        images: Vec::new(),
                        origin: None,
                    }],
                );
            });
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 queued"),
            "collapsed line reports the queued count: {body}"
        );

        expand(&container).await;
        let body = text(&container);
        assert!(
            body.contains("also fix the flaky test"),
            "the queued message preview is visible: {body}"
        );
        assert_eq!(
            count(&container, ".queued-message-send-now"),
            1,
            "send-now action present"
        );
        assert_eq!(
            count(&container, ".queued-message-cancel"),
            1,
            "cancel action present"
        );
    }

    /// A failed child is pinned: counted in the collapsed line and named with
    /// its error in the expanded row. Failures must not scroll away silently.
    #[wasm_bindgen_test]
    async fn failed_child_is_pinned_and_named() {
        let (container, _state) = mount_tray(|state| {
            let mut agent = child_agent("agent-b", "Broken Worker");
            agent.fatal_error = Some("backend crashed".to_owned());
            state.agents.update(|agents| agents.push(agent));
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 failed"),
            "collapsed line reports the failure: {body}"
        );

        expand(&container).await;
        let body = text(&container);
        assert!(
            body.contains("Broken Worker") && body.contains("backend crashed"),
            "the failed agent is named with its error: {body}"
        );
    }

    /// A running workflow surfaces as a live row with its name and status,
    /// sourced from the same tool-progress store the workflow card reads.
    #[wasm_bindgen_test]
    async fn running_workflow_renders_named_row() {
        let (container, _state) = mount_tray(|state| {
            let run = WorkflowRunState {
                workflow_name: "review-changes".to_owned(),
                description: None,
                script: None,
                status: WorkflowRunStatus::Running,
                summary: None,
                total_tokens: 0,
                tool_uses: 0,
                duration_ms: 0,
                agents: Vec::new(),
            };
            state.tool_progress.update(|map| {
                map.insert(
                    (parent_ref().agent_id, ToolCallId("call-1".to_owned())),
                    ArcRwSignal::new(ToolProgressData {
                        tool_call_id: "call-1".to_owned(),
                        tool_name: "Workflow".to_owned(),
                        update: ToolProgressUpdate::Workflow(run),
                    }),
                );
            });
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 running"),
            "collapsed line counts the workflow: {body}"
        );

        expand(&container).await;
        let body = text(&container);
        assert!(
            body.contains("review-changes"),
            "the workflow row is named: {body}"
        );
    }

    fn background_command_progress(
        status: BackgroundTaskStatus,
        summary: Option<&str>,
    ) -> ToolProgressData {
        ToolProgressData {
            tool_call_id: "toolu_bg_bash".to_owned(),
            tool_name: "Bash".to_owned(),
            update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                task_id: "task-bg".to_owned(),
                description: Some("Run repository validation".to_owned()),
                status,
                summary: summary.map(str::to_owned),
            }),
        }
    }

    /// A backgrounded shell command is in-flight work: it must count as
    /// running and render a row naming it — the exact class of state that
    /// used to be invisible because the server dropped bash task frames.
    #[wasm_bindgen_test]
    async fn running_background_command_renders_named_row() {
        let (container, _state) = mount_tray(|state| {
            seed_progress(
                state,
                background_command_progress(BackgroundTaskStatus::Running, None),
            );
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 running"),
            "collapsed line counts the background command: {body}"
        );

        expand(&container).await;
        let body = text(&container);
        assert!(
            body.contains("Run repository validation") && body.contains("Running"),
            "the command row shows its description and live status: {body}"
        );
    }

    /// Once the command completes, the row surfaces the CLI's summary —
    /// the only carrier of the exit code — counts as finished, and is
    /// removed by "Clear finished".
    #[wasm_bindgen_test]
    async fn completed_background_command_shows_summary_and_clears() {
        let (container, _state) = mount_tray(|state| {
            seed_progress(
                state,
                background_command_progress(
                    BackgroundTaskStatus::Completed,
                    Some(
                        "Background command \"Run repository validation\" completed (exit code 0)",
                    ),
                ),
            );
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 finished"),
            "a completed command counts as finished: {body}"
        );

        expand(&container).await;
        let body = text(&container);
        assert!(
            body.contains("completed (exit code 0)"),
            "the finished row carries the exit-code summary: {body}"
        );

        let clear = container
            .query_selector(".inflight-tray-clear")
            .unwrap()
            .expect("clear button exists")
            .dyn_into::<HtmlElement>()
            .unwrap();
        clear.click();
        next_tick().await;
        assert_eq!(
            count(&container, ".inflight-tray"),
            0,
            "clearing the only finished command hides the tray"
        );
    }

    /// A failed background command stays pinned — it must not be swept by
    /// "Clear finished" — until individually dismissed.
    #[wasm_bindgen_test]
    async fn failed_background_command_pins_until_dismissed() {
        let (container, _state) = mount_tray(|state| {
            seed_progress(
                state,
                background_command_progress(BackgroundTaskStatus::Failed, None),
            );
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 failed"),
            "a failed command counts as failed: {body}"
        );

        expand(&container).await;
        let dismiss = container
            .query_selector(".inflight-tray-dismiss")
            .unwrap()
            .expect("failed command row offers a dismiss action")
            .dyn_into::<HtmlElement>()
            .unwrap();
        dismiss.click();
        next_tick().await;
        assert_eq!(
            count(&container, ".inflight-tray"),
            0,
            "dismissing the only failure hides the tray"
        );
    }

    /// An idle (finished) child counts as finished, and "Clear finished"
    /// removes it — after which the tray disappears entirely.
    #[wasm_bindgen_test]
    async fn clear_finished_removes_idle_child_and_hides_tray() {
        let (container, _state) = mount_tray(|state| {
            state
                .agents
                .update(|agents| agents.push(child_agent("agent-c", "Done Worker")));
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 finished"),
            "an idle child counts as finished: {body}"
        );

        expand(&container).await;
        let clear = container
            .query_selector(".inflight-tray-clear")
            .unwrap()
            .expect("clear button exists")
            .dyn_into::<HtmlElement>()
            .unwrap();
        clear.click();
        next_tick().await;

        assert_eq!(
            count(&container, ".inflight-tray"),
            0,
            "clearing the only finished item hides the tray"
        );
    }

    // ── Contracts ported from `tool_card::live_card_wasm_tests` ─────────
    //
    // The spawn/await cards' live rows moved here wholesale; these tests
    // moved with them. Each preserves the behavioral contract its
    // predecessor pinned on the card rows — server-owned summaries and
    // stats rendered verbatim, host scoping, streaming previews, the
    // optimistic Starting state — now asserted on the tray, the single
    // surface that renders live per-agent detail.

    use crate::components::tool_card::ToolCardView;
    use crate::state::{StreamingState, ToolRequestEntry};
    use protocol::{
        AgentControlAgentRef, AgentControlProgress, ToolExecutionCompletedData,
        ToolExecutionResult, ToolRequest,
    };
    use serde_json::json;

    fn streaming_state(text: &str) -> StreamingState {
        StreamingState {
            agent_name: "codex".to_owned(),
            model: None,
            text: ArcRwSignal::new(text.to_owned()),
            reasoning: ArcRwSignal::new(String::new()),
            tool_requests: ArcRwSignal::new(Vec::new()),
        }
    }

    fn activity_stats(
        last_output_line: Option<&str>,
        tool_calls: u64,
        token_usage: protocol::TokenUsage,
    ) -> protocol::AgentActivityStats {
        protocol::AgentActivityStats {
            last_output_line: last_output_line.map(|s| s.to_owned()),
            tool_calls,
            token_usage,
            source_through_seq: None,
        }
    }

    fn token_usage(input: u64, cached: u64, output: u64, reasoning: u64) -> protocol::TokenUsage {
        protocol::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_prompt_tokens: (cached > 0).then_some(cached),
            cache_creation_input_tokens: None,
            reasoning_tokens: (reasoning > 0).then_some(reasoning),
        }
    }

    fn seed_stats_on_host(
        state: &AppState,
        host_id: &str,
        agent_id: &str,
        stats: protocol::AgentActivityStats,
    ) {
        state.agent_activity_stats.update(|map| {
            map.insert(
                ActiveAgentRef {
                    host_id: host_id.to_owned(),
                    agent_id: AgentId(agent_id.to_owned()),
                },
                stats,
            );
        });
    }

    fn seed_stats(state: &AppState, agent_id: &str, stats: protocol::AgentActivityStats) {
        // Child agents in these fixtures live on the parent chat's host.
        seed_stats_on_host(state, "host-1", agent_id, stats);
    }

    fn stats_line_text(container: &HtmlElement) -> String {
        container
            .query_selector(".tool-live-agent-stats")
            .expect("query stats")
            .expect("stats line present")
            .text_content()
            .unwrap_or_default()
    }

    fn fresh_summary(text: &str) -> AgentActivitySummaryState {
        AgentActivitySummaryState::Fresh {
            summary: AgentActivitySummary {
                text: text.to_owned(),
                generated_at_ms: js_sys::Date::now() as u64,
                source_from_seq: Some(1),
                source_through_seq: Some(9),
            },
        }
    }

    fn spawn_progress_for(agent_id: &str, name: &str) -> ToolProgressData {
        ToolProgressData {
            tool_call_id: "toolu_agent_control".to_owned(),
            tool_name: "tyde_spawn_agent".to_owned(),
            update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                progress_kind: AgentControlProgressKind::Spawn,
                agents: vec![AgentControlAgentRef {
                    agent_id: AgentId(agent_id.to_owned()),
                    name: Some(name.to_owned()),
                }],
            }),
        }
    }

    fn seed_progress(state: &AppState, progress: ToolProgressData) {
        state.tool_progress.update(|map| {
            map.insert(
                (
                    parent_ref().agent_id,
                    ToolCallId(progress.tool_call_id.clone()),
                ),
                ArcRwSignal::new(progress),
            );
        });
    }

    /// Ported from `agent_control_spawn_card_tracks_live_agent_state`: a
    /// running child renders its live AppState name, Running status, the
    /// streaming preview, and an open action; when its stream and turn end
    /// the row goes Idle and counts as finished.
    #[wasm_bindgen_test]
    async fn running_child_shows_streaming_preview_then_idles() {
        let (container, state) = mount_tray(|state| {
            state
                .agents
                .update(|agents| agents.push(child_agent("agent-a", "Worker Real")));
            state.agent_turn_active.update(|map| {
                map.insert(AgentId("agent-a".to_owned()), true);
            });
            state.streaming_text.update(|map| {
                map.insert(
                    AgentId("agent-a".to_owned()),
                    streaming_state("Implementing live tool cards"),
                );
            });
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(body.contains("Worker Real"), "AppState name wins: {body}");
        assert!(body.contains("Running"), "agent status visible: {body}");
        assert!(
            body.contains("Implementing live tool cards"),
            "streaming preview visible: {body}"
        );
        assert!(body.contains("Open agent"), "open-agent affordance: {body}");

        state.agent_turn_active.update(|map| {
            map.remove(&AgentId("agent-a".to_owned()));
        });
        state.streaming_text.update(|map| {
            map.remove(&AgentId("agent-a".to_owned()));
        });
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Idle"), "agent row goes idle: {body}");
        assert!(
            body.contains("1 finished"),
            "an idle child counts as finished: {body}"
        );
    }

    /// Ported from `agent_control_spawn_card_treats_unknown_agent_as_starting`:
    /// an agent referenced by spawn progress with no registry record yet
    /// renders optimistically as Starting and counts as running.
    #[wasm_bindgen_test]
    async fn unknown_spawned_agent_renders_starting_row() {
        let (container, _state) = mount_tray(|state| {
            seed_progress(state, spawn_progress_for("agent-unknown", "Worker"));
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("1 running"),
            "pending spawn counts as running: {body}"
        );

        expand(&container).await;
        let body = text(&container);
        assert!(
            body.contains("Worker") && body.contains("Starting"),
            "unknown spawned agent row starts optimistic: {body}"
        );
    }

    /// Ported from `agent_control_await_card_renders_server_activity_summary`:
    /// the summary states follow the server enum verbatim. Fresh shows the
    /// text with a freshness label; Pending-without-previous shows a
    /// "summarizing…" placeholder and never the output line; Disabled shows
    /// the server output line. The frontend infers none of it.
    #[wasm_bindgen_test]
    async fn summary_states_follow_server_enum() {
        let (container, state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = fresh_summary("Refactoring the auth module and adding tests");
            state.agents.update(|agents| agents.push(info));
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(
            body.contains("Refactoring the auth module and adding tests"),
            "fresh summary text visible: {body}"
        );
        assert!(
            body.contains("updated"),
            "fresh summary shows a freshness label: {body}"
        );

        seed_stats(
            &state,
            "agent-a",
            activity_stats(Some("Running cargo test"), 0, token_usage(0, 0, 0, 0)),
        );
        state.agents.update(|agents| {
            if let Some(agent) = agents
                .iter_mut()
                .find(|agent| agent.agent_id == AgentId("agent-a".to_owned()))
            {
                agent.activity_summary = AgentActivitySummaryState::Pending {
                    requested_at_ms: js_sys::Date::now() as u64,
                    previous: None,
                };
            }
        });
        next_tick().await;
        let body = text(&container);
        assert!(
            body.contains("summarizing"),
            "pending-without-text shows a summarizing status: {body}"
        );
        assert!(
            !body.contains("Running cargo test"),
            "enabled summaries must not show the output line: {body}"
        );
        assert!(
            !body.contains("Refactoring the auth module"),
            "no stale summary text once pending with no previous: {body}"
        );

        state.agents.update(|agents| {
            if let Some(agent) = agents
                .iter_mut()
                .find(|agent| agent.agent_id == AgentId("agent-a".to_owned()))
            {
                agent.activity_summary = AgentActivitySummaryState::Disabled;
            }
        });
        next_tick().await;
        let body = text(&container);
        assert!(
            !body.contains("updated") && !body.contains("Refactoring"),
            "disabled renders no summary line: {body}"
        );
        assert!(
            body.contains("Running cargo test"),
            "disabled summaries show the server output line: {body}"
        );
    }

    /// Ported from `await_summary_hides_output_line`: summary XOR output —
    /// when an enabled summary has renderable text, the row shows the summary
    /// and NOT the server output line.
    #[wasm_bindgen_test]
    async fn summary_hides_output_line() {
        let (container, _state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = fresh_summary("Writing the migration");
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(
                    Some("output line that must hide"),
                    3,
                    token_usage(0, 0, 0, 0),
                ),
            );
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(
            body.contains("Writing the migration"),
            "summary text shows: {body}"
        );
        assert!(
            !body.contains("output line that must hide"),
            "output line must be hidden while a summary has text: {body}"
        );
    }

    /// Ported from `await_disabled_summary_shows_server_output_not_streaming`,
    /// adapted to the merged row. The output line (`.tool-live-agent-output`)
    /// is server-owned: with summaries disabled and no live stream it renders
    /// the stats line verbatim. While the child streams, the row shows the
    /// live *preview* element instead — the deliberate merge of the spawn
    /// row's preview with the await row's server detail — and no output-line
    /// element is fabricated from streaming text.
    #[wasm_bindgen_test]
    async fn disabled_summary_shows_server_output_not_streaming() {
        let (container, state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = AgentActivitySummaryState::Disabled;
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(Some("Compiling crate"), 1, token_usage(0, 0, 0, 0)),
            );
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(
            body.contains("Compiling crate"),
            "disabled summary shows server output line: {body}"
        );

        state.streaming_text.update(|map| {
            map.insert(
                AgentId("agent-a".to_owned()),
                streaming_state("live stream preview"),
            );
        });
        next_tick().await;

        assert_eq!(
            count(&container, ".tool-live-agent-output"),
            0,
            "no output-line element while the live preview shows"
        );
        let preview = container
            .query_selector(".tool-live-agent-preview")
            .expect("query preview")
            .expect("preview element present while streaming")
            .text_content()
            .unwrap_or_default();
        assert!(
            preview.contains("live stream preview"),
            "the preview element carries the stream: {preview}"
        );
    }

    /// Ported from `await_enabled_empty_summary_hides_output`: an enabled
    /// (Empty) summary state must not fall back to the output line.
    #[wasm_bindgen_test]
    async fn enabled_empty_summary_hides_output() {
        let (container, _state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = AgentActivitySummaryState::Empty;
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(Some("Reading files"), 2, token_usage(0, 0, 0, 0)),
            );
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(
            !body.contains("Reading files"),
            "enabled (Empty) summary must not show the output line: {body}"
        );
    }

    /// Ported from `await_stats_line_renders_tool_calls_and_tokens`: the
    /// stats line renders the running tool-call count and token usage with
    /// the shared token badge format, independent of the summary choice.
    #[wasm_bindgen_test]
    async fn stats_line_renders_tool_calls_and_tokens() {
        let (container, _state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = fresh_summary("Doing work");
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(None, 5, token_usage(1200, 300, 800, 64)),
            );
        });
        next_tick().await;
        expand(&container).await;

        let stats_line = stats_line_text(&container);
        assert!(
            stats_line.contains("5 tool calls"),
            "stats line shows running tool-call count: {stats_line}"
        );
        assert!(
            stats_line.contains("cached"),
            "stats line shows cached-token detail like the chat token badge: {stats_line}"
        );
        assert!(
            stats_line.contains("reasoning"),
            "stats line shows reasoning-token detail like the chat token badge: {stats_line}"
        );
    }

    /// Ported from `await_stats_line_shows_server_cumulative_verbatim`: the
    /// server-authoritative cumulative renders exactly as reported, with no
    /// client-side summing or inference.
    #[wasm_bindgen_test]
    async fn stats_line_shows_server_cumulative_verbatim() {
        let (container, _state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = AgentActivitySummaryState::Disabled;
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(None, 12, token_usage(900_000, 0, 30_000, 0)),
            );
        });
        next_tick().await;
        expand(&container).await;

        let stats_line = stats_line_text(&container);
        assert!(
            stats_line.contains("900.0K"),
            "stats line shows the server cumulative input verbatim: {stats_line}"
        );
        assert!(
            stats_line.contains("30.0K"),
            "stats line shows the server cumulative output verbatim: {stats_line}"
        );
    }

    /// Ported from `await_stats_line_tool_calls_only_shows_no_token_badge`: a
    /// tool-call-only agent (every token counter zero, or only `total_tokens`
    /// set) shows its count with NO token badge — a fake `↑0 · ↓0` would
    /// misrepresent a non-reporting backend as reporting zero usage.
    #[wasm_bindgen_test]
    async fn stats_line_tool_calls_only_shows_no_token_badge() {
        let (container, state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = AgentActivitySummaryState::Disabled;
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(None, 12, token_usage(0, 0, 0, 0)),
            );
        });
        next_tick().await;
        expand(&container).await;

        let stats_line = stats_line_text(&container);
        assert!(
            stats_line.contains("12 tool calls"),
            "tool-call count is still shown: {stats_line}"
        );
        assert!(
            !stats_line.contains('\u{2191}') && !stats_line.contains('\u{2193}'),
            "no token arrows when every counter is zero: {stats_line}"
        );
        assert!(
            container
                .query_selector(".token-stat-input")
                .expect("query input span")
                .is_none()
                && container
                    .query_selector(".token-stat-output")
                    .expect("query output span")
                    .is_none(),
            "no token span elements for an all-zero usage"
        );

        // Total-only edge case: the badge displays input/output (+cache/
        // reasoning), never `total_tokens`, so a total-only usage must also
        // render no badge.
        let total_only = protocol::TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 123,
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        };
        seed_stats(&state, "agent-a", activity_stats(None, 5, total_only));
        next_tick().await;

        let stats_line = stats_line_text(&container);
        assert!(
            stats_line.contains("5 tool calls"),
            "total-only: tool-call count is still shown: {stats_line}"
        );
        assert!(
            !stats_line.contains('\u{2191}') && !stats_line.contains('\u{2193}'),
            "total-only usage must render no token arrows: {stats_line}"
        );
    }

    /// Ported from `await_stats_line_replaces_cumulative_on_new_frame`: a
    /// later stats frame re-renders the mounted row in place, replacing the
    /// cumulative — never accumulating.
    #[wasm_bindgen_test]
    async fn stats_line_replaces_cumulative_on_new_frame() {
        let (container, state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = AgentActivitySummaryState::Disabled;
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                state,
                "agent-a",
                activity_stats(None, 3, token_usage(100_000, 0, 5_000, 0)),
            );
        });
        next_tick().await;
        expand(&container).await;

        let stats_line = stats_line_text(&container);
        assert!(
            stats_line.contains("100.0K"),
            "initial cumulative input renders: {stats_line}"
        );

        seed_stats(
            &state,
            "agent-a",
            activity_stats(None, 7, token_usage(250_000, 0, 9_000, 0)),
        );
        next_tick().await;

        let stats_line = stats_line_text(&container);
        assert!(
            stats_line.contains("250.0K"),
            "cumulative updates live to the new total: {stats_line}"
        );
        assert!(
            stats_line.contains("7 tool calls"),
            "tool-call count updates live: {stats_line}"
        );
        assert!(
            !stats_line.contains("100.0K"),
            "old cumulative is replaced, not appended: {stats_line}"
        );
    }

    /// Ported from `await_stats_are_scoped_to_owning_host`: stats are keyed
    /// by (host_id, agent_id) — a frame for the same agent id on a different
    /// host must not leak into this chat's tray.
    #[wasm_bindgen_test]
    async fn stats_are_scoped_to_owning_host() {
        let (container, state) = mount_tray(|state| {
            let mut info = child_agent("agent-a", "Awaited Worker");
            info.activity_summary = AgentActivitySummaryState::Disabled;
            state.agents.update(|agents| agents.push(info));
            seed_stats_on_host(
                state,
                "other-host",
                "agent-a",
                activity_stats(
                    Some("stats from another host"),
                    9,
                    token_usage(50, 0, 50, 0),
                ),
            );
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(
            !body.contains("stats from another host") && !body.contains("9 tool calls"),
            "stats for the same agent id on another host must not leak: {body}"
        );

        seed_stats_on_host(
            &state,
            "host-1",
            "agent-a",
            activity_stats(Some("stats from this host"), 4, token_usage(10, 0, 10, 0)),
        );
        next_tick().await;
        let body = text(&container);
        assert!(
            body.contains("stats from this host") && body.contains("4 tool calls"),
            "owning-host stats render: {body}"
        );
    }

    /// Ported from `task_card_shows_live_subagent_status_and_open_link` (the
    /// live-detail half): a registry-less native sub-agent's last tool and
    /// tool-call count render here — the card now defers to this tray.
    #[wasm_bindgen_test]
    async fn subagent_row_shows_last_tool_and_count() {
        let (container, _state) = mount_tray(|state| {
            seed_progress(
                state,
                ToolProgressData {
                    tool_call_id: "toolu_task".to_owned(),
                    tool_name: "Task".to_owned(),
                    update: ToolProgressUpdate::SubAgent(SubAgentProgress {
                        agent_id: AgentId("agent-sub".to_owned()),
                        agent_name: "Explore".to_owned(),
                        last_tool_name: Some("Read".to_owned()),
                        tool_calls: 12,
                        completed: false,
                    }),
                },
            );
        });
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert!(body.contains("Explore running"), "live status: {body}");
        assert!(body.contains("last tool: Read"), "last tool: {body}");
        assert!(body.contains("12 tool calls"), "tool count: {body}");
    }

    /// The open action on a child row opens that agent's chat — same
    /// contract the card rows carried (`native_codex_wait_card_opens_awaited_agent`).
    #[wasm_bindgen_test]
    async fn open_action_opens_child_agent_chat() {
        let (container, state) = mount_tray(|state| {
            let mut child = child_agent("native-child", "Sleeper");
            child.origin = AgentOrigin::BackendNative;
            state.agents.update(|agents| agents.push(child));
            state.agent_turn_active.update(|map| {
                map.insert(AgentId("native-child".to_owned()), true);
            });
        });
        next_tick().await;
        expand(&container).await;

        let button = container
            .query_selector(".inflight-tray-row .tool-live-link")
            .expect("query open-agent button")
            .expect("open-agent button is rendered")
            .dyn_into::<HtmlElement>()
            .expect("open-agent button is an HTML element");
        button.click();
        next_tick().await;

        let opened = state
            .active_agent
            .get_untracked()
            .expect("clicking the rendered action opens the child");
        assert_eq!(opened.agent_id, AgentId("native-child".to_owned()));
        assert_eq!(opened.host_id, "host-1");
    }

    /// Strengthened successor of `activity_summary_renders_in_await_card_not_spawn_card`.
    /// That test pinned "the same agent's summary appears exactly once across
    /// spawn + await cards"; with the cards demoted to receipts the invariant
    /// tightens: across the spawn card, the await card, AND the tray, the
    /// summary renders exactly once — in the tray.
    #[wasm_bindgen_test]
    async fn summary_appears_exactly_once_across_cards_and_tray() {
        const SUMMARY: &str = "Refactoring the auth module and adding tests";

        fn card_entry(tool_name: &str) -> ToolRequestEntry {
            ToolRequestEntry {
                request: ToolRequest {
                    tool_call_id: format!("toolu_{tool_name}"),
                    tool_name: tool_name.to_owned(),
                    tool_type: ToolRequestType::Other { args: json!({}) },
                },
                result: Some(ToolExecutionCompletedData {
                    tool_call_id: format!("toolu_{tool_name}"),
                    tool_name: tool_name.to_owned(),
                    tool_result: ToolExecutionResult::Other { result: json!({}) },
                    success: true,
                    error: None,
                    normalization_failure: None,
                }),
            }
        }

        fn control_progress(
            tool_name: &str,
            progress_kind: AgentControlProgressKind,
        ) -> ToolProgressData {
            ToolProgressData {
                tool_call_id: format!("toolu_{tool_name}"),
                tool_name: tool_name.to_owned(),
                update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                    progress_kind,
                    agents: vec![AgentControlAgentRef {
                        agent_id: AgentId("agent-a".to_owned()),
                        name: Some("Worker".to_owned()),
                    }],
                }),
            }
        }

        if let Some(storage) = local_storage() {
            let _ = storage.remove_item(STORAGE_INFLIGHT_TRAY_EXPANDED);
        }
        let state = AppState::new();
        let mut info = child_agent("agent-a", "Worker");
        info.activity_summary = fresh_summary(SUMMARY);
        state.agents.update(|agents| agents.push(info));
        seed_progress(
            &state,
            control_progress("tyde_spawn_agent", AgentControlProgressKind::Spawn),
        );
        seed_progress(
            &state,
            control_progress("tyde_await_agents", AgentControlProgressKind::Await),
        );

        let container = make_container();
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            let agent_ref = Signal::derive(|| Some(parent_ref()));
            view! {
                <ToolCardView agent_ref=agent_ref entry=card_entry("tyde_spawn_agent") />
                <ToolCardView agent_ref=agent_ref entry=card_entry("tyde_await_agents") />
                <InflightTray agent_ref=agent_ref />
            }
        });
        handle.forget();
        next_tick().await;
        expand(&container).await;

        let body = text(&container);
        assert_eq!(
            body.matches(SUMMARY).count(),
            1,
            "the agent's summary renders exactly once across both cards and the tray: {body}"
        );
        assert_eq!(
            count(&container, ".tool-live-agent-summary"),
            1,
            "exactly one summary element on screen"
        );
        assert_eq!(
            count(&container, ".tool-live-agent-status"),
            1,
            "exactly one live status badge on screen — the tray's"
        );
    }

    /// A failed child's dismiss control hides it; with nothing else in
    /// flight the tray disappears. Failures are pinned until this explicit
    /// dismissal — never auto-cleared.
    #[wasm_bindgen_test]
    async fn dismissing_failed_child_hides_it() {
        let (container, _state) = mount_tray(|state| {
            let mut agent = child_agent("agent-b", "Broken Worker");
            agent.fatal_error = Some("backend crashed".to_owned());
            state.agents.update(|agents| agents.push(agent));
        });
        next_tick().await;
        expand(&container).await;

        let dismiss = container
            .query_selector(".inflight-tray-dismiss")
            .unwrap()
            .expect("failed row exposes a dismiss control")
            .dyn_into::<HtmlElement>()
            .unwrap();
        dismiss.click();
        next_tick().await;

        assert_eq!(
            count(&container, ".inflight-tray"),
            0,
            "dismissing the only failure hides the tray"
        );
    }
}
