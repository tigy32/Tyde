use std::collections::HashSet;

use leptos::prelude::*;
use protocol::{
    BackendKind, TaskTimeInterval, TaskTimePhase, TaskTimePhaseEnd, TaskTimePhaseTotals,
    TaskTimingCategory, TaskTimingState, TaskTimingSummary, TaskTokenUsagePayload,
    TaskTokenUsageStatus,
};
use wasm_bindgen::JsCast;

use crate::components::agents_panel::backend_label;
use crate::components::backend_capacity::TaskCapacityLimits;
use crate::components::session_settings::{task_scope_view, task_usage_unavailable_text};
use crate::state::{ActiveAgentRef, ActiveProjectRef, AppState, TabContent};

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageTab {
    Overview,
    Time,
    Agents,
}

fn status_text(status: &TaskTokenUsageStatus) -> String {
    match status {
        TaskTokenUsageStatus::Known => "Reported usage".to_owned(),
        TaskTokenUsageStatus::Partial {
            unavailable_count, ..
        } => format!(
            "{unavailable_count} scope{} unreported",
            if *unavailable_count == 1 { "" } else { "s" }
        ),
        TaskTokenUsageStatus::Unavailable {
            unavailable_count, ..
        } => format!(
            "Unavailable · {unavailable_count} scope{} unreported",
            if *unavailable_count == 1 { "" } else { "s" }
        ),
    }
}

#[component]
pub(super) fn UsageInfo(label: String, detail: String) -> impl IntoView {
    view! {
        <span class="task-usage-info" tabindex="0" role="note" aria-label=detail.clone() title=detail.clone()>
            <span aria-hidden="true">{label}</span>
            <span class="task-usage-tooltip" role="tooltip" tabindex="0">{detail.clone()}</span>
        </span>
    }
}

fn aggregate_view(total: &protocol::TaskTokenUsageAggregate) -> AnyView {
    let tokens = match total.status {
        TaskTokenUsageStatus::Unavailable { .. } => view! {
            <span class="token-stat token-stat-input">"↑—"<span class="task-token-detail">" (cached —)"</span></span>
            <span class="token-stat token-stat-output">"↓—"<span class="task-token-detail">" (reasoning —)"</span></span>
        }.into_any(),
        _ => crate::components::session_settings::task_detail_tokens(&total.usage),
    };
    let combined = (total.usage.prompt_tokens.is_none()
        && total.usage.output_tokens.is_none()
        && !matches!(total.status, TaskTokenUsageStatus::Unavailable { .. }))
    .then(|| {
        format!(
            "Reported combined: {} · input/output unreported",
            crate::components::chat_message::format_compact(total.usage.total_tokens)
        )
    });
    view! { <span class="session-task-row-usage">{tokens}{combined.map(|text| view! { <span>{text}</span> })}</span> }.into_any()
}

fn usage_coverage(total: &protocol::TaskTokenUsageAggregate) -> Option<String> {
    if matches!(total.status, TaskTokenUsageStatus::Known) {
        return None;
    }
    let mut text = status_text(&total.status);
    match &total.status {
        TaskTokenUsageStatus::Partial { reasons, .. }
        | TaskTokenUsageStatus::Unavailable { reasons, .. } => {
            for reason in reasons {
                text.push_str(" · ");
                text.push_str(task_usage_unavailable_text(*reason));
            }
        }
        TaskTokenUsageStatus::Known => {}
    }
    text.push_str(". — means unreported, not zero.");
    Some(text)
}

fn availability(value: protocol::TimingAvailability) -> &'static str {
    match value {
        protocol::TimingAvailability::Unsupported => "unsupported",
        protocol::TimingAvailability::Unobserved => "unobserved",
        protocol::TimingAvailability::Observed => "observed",
        protocol::TimingAvailability::PersistenceFailed => "persistence failed",
    }
}

fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{:01}s", ms / 1_000, (ms % 1_000) / 100)
    } else if ms < 3_600_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else {
        format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

fn navigation_entry_capable(navigation: &protocol::TaskUsageNavigation) -> bool {
    !matches!(navigation, protocol::TaskUsageNavigation::Unavailable)
}

#[component]
pub(super) fn TaskUsagePopup(
    rollup: Memo<Option<TaskTokenUsagePayload>>,
    binding: Memo<Option<(String, BackendKind)>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let expanded = RwSignal::new(false);
    let tab = RwSignal::new(UsageTab::Overview);
    let backend_filter = RwSignal::new(None::<BackendKind>);
    let query = RwSignal::new(String::new());
    let flat = RwSignal::new(false);
    let selected_agent = RwSignal::new(None::<protocol::AgentId>);
    let collapsed = RwSignal::new(HashSet::<protocol::AgentId>::new());
    let heading: NodeRef<leptos::html::H2> = NodeRef::new();
    let panel_id = Memo::new(move |_| {
        let (host, _) = binding.get()?;
        let payload = rollup.get()?;
        Some(format!("task-usage-panel-{host}-{}", payload.root_agent_id))
    });
    let state = StoredValue::new(expect_context::<AppState>());
    Effect::new(move |_| {
        expanded.get();
        if let Some(heading) = heading.get() {
            let _ = heading.focus();
        }
    });
    let drilldown = Callback::new(move |kind| {
        backend_filter.set(Some(kind));
        tab.set(UsageTab::Agents);
        expanded.set(true);
    });
    view! {
        <section class="session-task-popover" class:task-usage-expanded=move || expanded.get()
            role="dialog" aria-label="Task usage" aria-modal="false">
            <header class="task-usage-heading">
                <h2 node_ref=heading tabindex="-1">"Task usage"</h2>
                <span class="task-usage-count" title="Current root agent and retained descendants">{move || rollup.get().map(|payload| format!("{} agent{}", payload.breakdown.len(), if payload.breakdown.len() == 1 { "" } else { "s" }))}</span>
                {move || rollup.get().and_then(|payload| usage_coverage(&payload.total).map(|detail| view! { <UsageInfo label=if matches!(payload.total.status, TaskTokenUsageStatus::Unavailable { .. }) { "Unreported" } else { "Unreported scopes" }.to_owned() detail=detail /> }))}
                <Show when=move || !expanded.get()>
                    <button class="task-usage-expand" aria-label="Expand task usage" title="Expand task usage" on:click=move |_| expanded.set(true)>"↗"</button>
                </Show>
                <Show when=move || expanded.get()>
                    <button class="task-usage-back" on:click=move |_| expanded.set(false)>"← Back"</button>
                </Show>
                <button class="task-usage-close" aria-label="Close task usage" on:click=move |_| on_close.run(())>"×"</button>
            </header>
            <Show when=move || expanded.get()>
                <div class="task-usage-tabs" role="tablist" aria-label="Task usage views">
                    {[(UsageTab::Overview, "Overview"), (UsageTab::Time, "Time breakdown"), (UsageTab::Agents, "Agents")].map(|(value, label)| view! {
                        <button role="tab" aria-selected=move || (tab.get() == value).to_string()
                            aria-controls=move || panel_id.get() tabindex=move || if tab.get() == value { 0 } else { -1 } on:click=move |_| tab.set(value)
                            on:keydown=move |event| {
                                let next = match event.key().as_str() {
                                    "ArrowRight" => Some(match value { UsageTab::Overview => UsageTab::Time, UsageTab::Time => UsageTab::Agents, UsageTab::Agents => UsageTab::Overview }),
                                    "ArrowLeft" => Some(match value { UsageTab::Overview => UsageTab::Agents, UsageTab::Time => UsageTab::Overview, UsageTab::Agents => UsageTab::Time }),
                                    "Home" => Some(UsageTab::Overview), "End" => Some(UsageTab::Agents), _ => None,
                                };
                                if let Some(next) = next {
                                    event.prevent_default(); tab.set(next);
                                    if let Some(target) = event.target().and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                                        .and_then(|target| target.parent_element())
                                        .and_then(|parent| parent.query_selector_all("[role=tab]").ok())
                                        .and_then(|items| items.item(match next { UsageTab::Overview => 0, UsageTab::Time => 1, UsageTab::Agents => 2 }))
                                        .and_then(|target| target.dyn_into::<web_sys::HtmlElement>().ok()) { let _ = target.focus(); }
                                }
                            }>{label}</button>
                    })}
                </div>
            </Show>
            <Show when=move || expanded.get()><div class="task-usage-summary">
                {move || rollup.get().map(|payload| aggregate_view(&payload.total))}
                {move || rollup.get().map(|payload| view! {
                    <span>{format!("{} agent{} · {} backend{}", payload.breakdown.len(), if payload.breakdown.len() == 1 { "" } else { "s" }, payload.backends.len(), if payload.backends.len() == 1 { "" } else { "s" })}</span>
                    <span class="session-task-popover-status">{status_text(&payload.total.status)}</span>
                })}
            </div></Show>
            <div class="task-usage-body" id=move || panel_id.get() role="tabpanel" aria-label=move || match tab.get() { UsageTab::Overview => "Overview", UsageTab::Time => "Time breakdown", UsageTab::Agents => "Agents" }>
                {move || rollup.get().and_then(|payload| (expanded.get() && !payload.helper_usage.is_empty()).then(|| view! {
                    <details class="task-timing-coverage"><summary>{format!("{} analyzer batches", payload.helper_usage.len())}</summary><p>"Grouped by reported backend. Shared batches are source scopes, not per-task allocations or additional agents; their timing is unobserved."</p></details>
                }))}
                <Show when=move || !expanded.get() || tab.get() == UsageTab::Overview>
                    <div class="task-backend-list">
                        <For each=move || rollup.get().map(|payload| payload.backends.iter().map(|group| group.backend_kind).collect::<Vec<_>>()).unwrap_or_default()
                            key=|kind| *kind children=move |kind| view! { <TaskBackendRow kind=kind rollup=rollup binding=binding drilldown=drilldown /> } />
                    </div>
                </Show>
                <Show when=move || expanded.get() && tab.get() == UsageTab::Time>
                    <TaskTimeView rollup=rollup />
                </Show>
                <Show when=move || expanded.get() && tab.get() == UsageTab::Agents>
                    <div class="task-agent-controls">
                        <label>"Backend "<select class="session-setting-select" aria-label="Filter by backend" on:change=move |event| {
                            let value = event_target_value(&event);
                            let kind = rollup.get_untracked().and_then(|payload| payload.backends.into_iter().find(|group| backend_label(group.backend_kind) == value).map(|group| group.backend_kind));
                            backend_filter.set(kind);
                        } prop:value=move || backend_filter.get().map(backend_label).unwrap_or("All backends")>
                            <option>"All backends"</option>
                            {move || rollup.get().map(|payload| payload.backends.iter().map(|group| view! { <option>{backend_label(group.backend_kind)}</option> }).collect_view())}
                        </select></label>
                        <button aria-pressed=move || (!flat.get()).to_string() on:click=move |_| flat.set(false)>"Tree"</button>
                        <button aria-pressed=move || flat.get().to_string() on:click=move |_| flat.set(true)>"Flat"</button>
                        <input aria-label="Filter agents" placeholder="Filter agents…" prop:value=move || query.get()
                            on:input=move |event| query.set(event_target_value(&event)) />
                    </div>
                    <p class="task-usage-note" title="Per-agent own usage, including closed descendants. No historical model allocation.">"Own usage · closed descendants included"</p>
                    <div class="task-agent-list" role="group" aria-label="Task token usage by agent">
                        {move || {
                            let payload = rollup.get()?;
                            let selected = backend_filter.get(); let text = query.get().to_lowercase(); let flat_view = flat.get();
                            let collapsed_ids = collapsed.get(); let mut hidden_depth = None;
                            let mut shown = 0;
                            let rows = payload.breakdown.iter().filter_map(|entry| {
                                if !flat_view && selected.is_none() && text.is_empty() {
                                    if hidden_depth.is_some_and(|depth| entry.depth > depth) { return None; }
                                    hidden_depth = collapsed_ids.contains(&entry.agent_id).then_some(entry.depth);
                                }
                                if selected.is_some_and(|kind| kind != entry.backend_kind) || !entry.name.to_lowercase().contains(&text) { return None; }
                                shown += 1;
                                let id = entry.agent_id.clone(); let toggle_id = id.clone(); let collapsed_now = collapsed_ids.contains(&id);
                                let has_children = payload.breakdown.iter().any(|other| other.parent_agent_id.as_ref() == Some(&id));
                                let detail_id = id.clone();
                                let can_open = navigation_entry_capable(&entry.navigation);
                                let host = binding.get().map(|(host, _)| host); let navigation_entry = entry.clone(); let navigation_state = state.get_value();
                                Some(view! {
                                    <div class="session-task-row" style=format!("padding-left: {}px", 8 + if flat_view { 0 } else { entry.depth.min(12) * 12 })>
                                        {(has_children && !flat_view).then(|| view! { <button class="task-tree-toggle" aria-label=format!("{} {}", if collapsed_now { "Expand" } else { "Collapse" }, entry.name) aria-expanded=(!collapsed_now).to_string() on:click=move |_| collapsed.update(|ids| { if !ids.remove(&toggle_id) { ids.insert(toggle_id.clone()); } })>{if collapsed_now { "›" } else { "⌄" }}</button> })}
                                        {(!has_children && !flat_view).then(|| view! { <span class="task-tree-spacer" aria-hidden="true"></span> })}
                                        <button class="session-task-row-name task-agent-details" title=entry.name.clone() aria-pressed=move || (selected_agent.get().as_ref() == Some(&detail_id)).to_string() on:click={let id = id.clone(); move |_| selected_agent.set(Some(id.clone()))}>{entry.name.clone()}</button>
                                        <span class="session-task-row-meta">{backend_label(entry.backend_kind)}</span>
                                        {task_scope_view(&entry.usage)}
                                        <button class="task-agent-open" disabled=!can_open title=if can_open { "Open this agent's chat" } else { "Chat cannot be resumed; select the name to inspect own usage" } on:click=move |_| {
                                            let Some(host) = host.clone() else { return; };
                                            match navigation_entry.navigation.clone() {
                                                protocol::TaskUsageNavigation::LiveAgent => {
                                                    on_close.run(());
                                                    navigation_state.switch_active_project(navigation_entry.project_id.clone().map(|project_id| ActiveProjectRef { host_id: host.clone(), project_id }));
                                                    navigation_state.open_tab(TabContent::chat_with_agent(ActiveAgentRef { host_id: host, agent_id: navigation_entry.agent_id.clone() }), navigation_entry.name.clone(), true);
                                                }
                                                protocol::TaskUsageNavigation::SavedSession { session_id } => {
                                                    on_close.run(()); crate::actions::resume_session_with_parent(&navigation_state, host, navigation_entry.backend_kind, session_id, navigation_entry.project_id.clone(), navigation_entry.parent_agent_id.clone());
                                                }
                                                protocol::TaskUsageNavigation::Unavailable => {},
                                            }
                                        }>{if can_open { "Open ↗" } else { "Chat unavailable" }}</button>
                                    </div>
                                })
                            }).collect_view();
                            Some(view! { {rows}<p class="task-usage-note">{format!("Showing {shown} of {} agents", payload.breakdown.len())}</p> })
                        }}
                    </div>
                    {move || {
                        let id = selected_agent.get()?;
                        let entry = rollup.get()?.breakdown.into_iter().find(|entry| entry.agent_id == id)?;
                        Some(view! { <aside class="task-agent-detail" aria-label="Selected agent usage">
                            <h3>{entry.name}</h3><p>{format!("{} · own usage only", backend_label(entry.backend_kind))}</p>
                            {task_scope_view(&entry.usage)}
                            <p>{entry.model.map(|model| format!("Latest observed model: {model}; not a historical allocation")).unwrap_or_else(|| "Latest model unreported".to_owned())}</p>
                            <p>{if navigation_entry_capable(&entry.navigation) { "Chat navigation available" } else { "Chat cannot be resumed in this context. Saved own usage remains available here." }}</p>
                        </aside> })
                    }}
                </Show>
            </div>
        </section>
    }
}

#[component]
fn TaskBackendRow(
    kind: BackendKind,
    rollup: Memo<Option<TaskTokenUsagePayload>>,
    binding: Memo<Option<(String, BackendKind)>>,
    drilldown: Callback<BackendKind>,
) -> impl IntoView {
    let group = Memo::new(move |_| {
        rollup.get().and_then(|payload| {
            payload
                .backends
                .into_iter()
                .find(|group| group.backend_kind == kind)
        })
    });
    view! {
        <article class="task-backend-row" aria-label=backend_label(kind)>
            <div class="task-backend-token-row">
                <div class="task-backend-name">
                    <button class="task-backend-drilldown" aria-label=format!("Show {} agents", backend_label(kind)) on:click=move |_| drilldown.run(kind)>{crate::components::launch_menu::backend_icon(kind)}{backend_label(kind)}<span>{move || group.get().map(|group| format!(" · {} agent{}", group.agent_count, if group.agent_count == 1 { "" } else { "s" }))}</span></button>
                    {move || group.get().and_then(|group| usage_coverage(&group.total).map(|detail| view! { <UsageInfo label=if matches!(group.total.status, TaskTokenUsageStatus::Unavailable { .. }) { "Unreported" } else { "Unreported scopes" }.to_owned() detail=format!("{}: {detail} {}/{} agents reporting; {} incomplete.", backend_label(kind), group.reporting_agent_count, group.agent_count, group.incomplete_agent_count) /> }))}
                </div>
                <div class="task-backend-tokens" role="group" aria-label=format!("{} task token usage: input, cached input, output and reasoning", backend_label(kind))>{move || group.get().map(|group| aggregate_view(&group.total))}</div>
                {move || group.get().and_then(|group| (group.analyzer_batch_count > 0).then(|| view! { <span class="task-backend-analyzers">{format!("{} analyzer batch{}", group.analyzer_batch_count, if group.analyzer_batch_count == 1 { "" } else { "es" })}</span> }))}
            </div>
            {move || binding.get().map(|(host, _)| view! { <TaskCapacityLimits host_id=host backend_kind=kind /> })}
        </article>
    }
}

fn timing_label(category: &TaskTimingCategory) -> (String, &'static str) {
    match category {
        TaskTimingCategory::Command { command, check } => (
            if *check {
                format!("Check · {command}")
            } else {
                command.clone()
            },
            "timing-command",
        ),
        TaskTimingCategory::DependencyWait {
            agent_ids,
            omitted_link_count,
            targets_observed,
        } => (
            if *targets_observed {
                format!(
                    "Await · {} linked{}",
                    agent_ids.len(),
                    match omitted_link_count {
                        Some(0) => String::new(),
                        Some(count) => format!(" · {count} omitted"),
                        None => " · additional links uncounted".to_owned(),
                    }
                )
            } else {
                "Delegation · targets unobserved".to_owned()
            },
            "timing-wait",
        ),
        TaskTimingCategory::Tool { name, classified } => (
            format!("{name}{}", if *classified { "" } else { " · unclassified" }),
            "timing-tool",
        ),
    }
}

fn timing_state(state: TaskTimingState) -> &'static str {
    match state {
        TaskTimingState::Open => "Open (observed so far)",
        TaskTimingState::Completed => "Completed",
        TaskTimingState::Cancelled => "Cancelled",
        TaskTimingState::Failed => "Failed",
        TaskTimingState::Interrupted => "Interrupted; endpoint unknown",
    }
}

fn phase_label(phase: TaskTimePhase) -> &'static str {
    match phase {
        TaskTimePhase::Starting => "Starting",
        TaskTimePhase::ModelResponse => "Model response",
        TaskTimePhase::Command => "Commands",
        TaskTimePhase::Tool => "Other tools",
        TaskTimePhase::DependencyWait => "Waiting on agents",
        TaskTimePhase::HumanWait => "Waiting on you",
        TaskTimePhase::Queued => "Queued input held",
        TaskTimePhase::Idle => "Idle",
        TaskTimePhase::NotRunning => "Not running",
        TaskTimePhase::Unobserved => "Unobserved",
    }
}

fn phase_class(phase: TaskTimePhase) -> &'static str {
    match phase {
        TaskTimePhase::Starting => "phase-starting",
        TaskTimePhase::ModelResponse => "phase-model",
        TaskTimePhase::Command => "phase-command",
        TaskTimePhase::Tool => "phase-tool",
        TaskTimePhase::DependencyWait => "phase-wait",
        TaskTimePhase::HumanWait => "phase-human",
        TaskTimePhase::Queued => "phase-queued",
        TaskTimePhase::Idle => "phase-idle",
        TaskTimePhase::NotRunning => "phase-stopped",
        TaskTimePhase::Unobserved => "phase-unobserved",
    }
}

fn phase_meaning(phase: TaskTimePhase) -> &'static str {
    match phase {
        TaskTimePhase::Starting => {
            "Tyde was launching the agent and setting up its session. The first message may already have been sent."
        }
        TaskTimePhase::ModelResponse => {
            "The agent was working with no tool running and nothing to wait for. This includes network delay, streaming and the model's own thinking; Tyde cannot tell them apart."
        }
        TaskTimePhase::Command => "A foreground command was running.",
        TaskTimePhase::Tool => "A foreground tool other than a command was running.",
        TaskTimePhase::DependencyWait => "Waiting for other agents to finish work it handed off.",
        TaskTimePhase::HumanWait => "Waiting for you to answer a question or approve something.",
        TaskTimePhase::Queued => "Not working; your queued messages had not been sent yet.",
        TaskTimePhase::Idle => "Not working and nothing waiting to be sent.",
        TaskTimePhase::NotRunning => "The agent was stopped, so nothing was running.",
        TaskTimePhase::Unobserved => {
            "Tyde has no record of this time: recording stopped unexpectedly, or the agent was not being tracked."
        }
    }
}

fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "—".to_owned();
    }
    let percent = 100.0 * part as f64 / whole as f64;
    if part > 0 && percent < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{percent:.0}%")
    }
}

/// The shared axis: the task's accounted interval, else the retained spans.
fn timing_axis(timing: &TaskTimingSummary) -> Option<(u64, u64)> {
    match &timing.interval {
        Some(interval) => Some((interval.started_at_ms, interval.observed_through_ms)),
        None => Some((timing.observed_start_ms?, timing.observed_end_ms?)),
    }
}

fn axis_style(axis: (u64, u64), start: u64, end: u64) -> String {
    let width = axis.1.saturating_sub(axis.0).max(1) as f64;
    let start = start.clamp(axis.0, axis.1);
    let end = end.clamp(start, axis.1);
    format!(
        "left: {}%; width: {}%",
        100.0 * start.saturating_sub(axis.0) as f64 / width,
        100.0 * end.saturating_sub(start) as f64 / width
    )
}

fn phase_rows(totals: &TaskTimePhaseTotals) -> Vec<(TaskTimePhase, u64)> {
    TaskTimePhase::ALL
        .into_iter()
        .map(|phase| (phase, totals.get(phase)))
        .filter(|(_, ms)| *ms > 0)
        .collect()
}

fn phase_totals_view(totals: &TaskTimePhaseTotals, whole: u64) -> impl IntoView + use<> {
    view! { <ul class="task-time-totals">
        {phase_rows(totals).into_iter().map(|(phase, ms)| view! {
            <li title=phase_meaning(phase)><span class=format!("task-time-swatch {}", phase_class(phase))></span><span>{phase_label(phase)}</span><span>{duration(ms)}</span><span>{share(ms, whole)}</span></li>
        }).collect_view()}
    </ul> }
}

#[component]
fn TaskTimeBreakdown(interval: Memo<Option<TaskTimeInterval>>) -> impl IntoView {
    let elapsed = move || {
        interval.with(|interval| {
            interval.as_ref().map_or(0, |interval| {
                interval
                    .observed_through_ms
                    .saturating_sub(interval.started_at_ms)
            })
        })
    };
    let wall_clock = move || {
        interval.with(|interval| {
            interval
                .as_ref()
                .map(|interval| interval.wall_clock.clone())
                .unwrap_or_default()
        })
    };
    let agent_time = move || {
        interval.with(|interval| {
            interval
                .as_ref()
                .map(|interval| (interval.agent_time.clone(), interval.accounted_agent_count))
                .unwrap_or_default()
        })
    };
    view! { <section class="task-time-breakdown" aria-label="Wall-clock breakdown">
        <h3>{move || format!("Wall clock · {}", duration(elapsed()))}</h3>
        <div class="task-time-stack" role="img" aria-label=move || format!("Wall clock {}: {}", duration(elapsed()), phase_rows(&wall_clock()).into_iter().map(|(phase, ms)| format!("{} {}", phase_label(phase), share(ms, elapsed()))).collect::<Vec<_>>().join(", "))>
            {move || { let elapsed = elapsed(); phase_rows(&wall_clock()).into_iter().map(|(phase, ms)| view! {
                <span class=format!("task-time-part {}", phase_class(phase)) style=format!("width: {}%", 100.0 * ms as f64 / elapsed.max(1) as f64) title=format!("{} · {} · {}", phase_label(phase), duration(ms), share(ms, elapsed))></span>
            }).collect_view() }}
        </div>
        {move || phase_totals_view(&wall_clock(), elapsed())}
        <p class="task-usage-note">"Each moment is counted once, so these add up to the wall clock. When several things happen at once, the time counts as waiting on you first, then waiting on agents, commands, other tools, and model response."</p>
        <details class="task-time-agent-time"><summary>{move || { let (totals, agents) = agent_time(); format!("Agent-time ∑ across {} accounted agent{} · {} (every agent's time summed, idle included; can exceed wall clock)", agents, if agents == 1 { "" } else { "s" }, duration(totals.sum())) }}</summary>
            {move || { let (totals, _) = agent_time(); let whole = totals.sum(); phase_totals_view(&totals, whole) }}
        </details>
    </section> }
}

#[component]
fn TaskTimeView(rollup: Memo<Option<TaskTokenUsagePayload>>) -> impl IntoView {
    let selected = RwSignal::new(None::<(protocol::AgentId, String)>);
    let agents = Memo::new(move |_| {
        let mut agents = Vec::new();
        if let Some(payload) = rollup.get() {
            for agent_id in payload
                .timing
                .accounts
                .into_iter()
                .map(|entry| entry.agent_id)
                .chain(payload.timing.spans.into_iter().map(|entry| entry.agent_id))
            {
                if !agents.contains(&agent_id) {
                    agents.push(agent_id);
                }
            }
        }
        agents
    });
    let interval = Memo::new(move |_| {
        rollup.with(|payload| {
            payload
                .as_ref()
                .and_then(|payload| payload.timing.interval.clone())
        })
    });
    view! { <div class="task-time-view">
        <Show when=move || interval.with(Option::is_some) fallback=move || rollup.with(Option::is_some).then(|| view! { <p class="task-usage-note" title="Recorded before Tyde kept a wall-clock account; earlier activity is not reconstructed.">"Tool calls only · no wall-clock account recorded · time between them unknown"</p> })>
            <TaskTimeBreakdown interval=interval />
        </Show>
        {move || rollup.get().map(|payload| {
            let timing = payload.timing;
            let range = match (timing.observed_start_ms, timing.observed_end_ms) { (Some(start), Some(end)) => duration(end.saturating_sub(start)), _ => "Unobserved".to_owned() };
            view! {
                <dl class="task-time-metrics">
                    <div><dt title="First tool call start to last tool call end">"Tool-call range"</dt><dd>{range}</dd></div>
                    <div><dt title="Classified tools, summed parallel; not elapsed time">"Tools ∑"</dt><dd>{if timing.classified_tool_count == 0 { "Unobserved".to_owned() } else { duration(timing.summed_tool_ms) }}</dd></div>
                    <div><dt title="Waiting on agents, summed across agents; their work is not added">"Waiting on agents ∑"</dt><dd>{if timing.covered_agent_count == 0 { "Unobserved".to_owned() } else { duration(timing.summed_dependency_wait_ms) }}</dd></div>
                    <div><dt title="Unclassified round-trips; excluded from the tool sum">"Unclassified (excluded)"</dt><dd>{duration(timing.summed_unclassified_ms)}</dd></div>
                    <div><dt>"Agents with observations"</dt><dd>{format!("{}/{} agents", timing.covered_agent_count, payload.breakdown.len())}</dd></div>
                </dl>
                <p class="task-usage-note">{format!("{} command runs · {} spans dropped · {} additional spans omitted · {} persistence failures", timing.command_count, timing.dropped_span_count, timing.omitted_span_count, timing.persistence_failed_agent_count)}</p>
            }
        })}
        <Show when=move || interval.with(Option::is_some)>
        <div class="task-timing-legend task-time-phase-legend" aria-label="Phase lane">
            {TaskTimePhase::ALL.into_iter().map(|phase| view! { <span title=phase_meaning(phase)><span class=format!("task-time-swatch {}", phase_class(phase))></span>{phase_label(phase)}</span> }).collect_view()}
            <span title="Older phase boundaries beyond the retained limit; their time stays in the totals"><span class="task-time-swatch task-time-compacted"></span>"Detail not retained"</span>
        </div>
        </Show>
        <div class="task-timing-legend"><span class="timing-command">"Commands (tool round-trip)"</span><span class="timing-tool">"Other tools"</span><span class="timing-wait">"Waiting on agents"</span><span>"Tool rows may overlap · Interrupted = end time unknown"</span></div>
        {move || rollup.get().and_then(|payload| {
            let (start, end) = timing_axis(&payload.timing)?;
            Some(view! { <div class="task-timing-axis" aria-label="Shared elapsed time axis"><span>"Agent"</span><span><span>"0"</span><span>{duration(end.saturating_sub(start))}</span></span></div> })
        })}
        <div class="task-timeline" role="group" aria-label="Agent timeline">
            <Show when=move || agents.get().is_empty()><p class="task-usage-note">"No observed operations"</p></Show>
            <For each=move || agents.get() key=|id| id.clone() children=move |agent_id| view! {
                <TaskTimingAgentRow agent_id=agent_id rollup=rollup selected=selected />
            } />
        </div>
        {move || rollup.get().and_then(|payload| {
            let key = selected.get()?;
            let entry = payload.timing.spans.iter().find(|entry| entry.agent_id == key.0 && entry.span.tool_call_id == key.1)?;
            Some(view! { <aside class="task-timing-detail" id="task-timing-operation" aria-label="Observed operation"><h3>{timing_label(&entry.span.category).0}</h3><p>{format!("{} · Start {} ms · Observed through {} ms", timing_state(entry.span.state), entry.span.started_at_ms, entry.span.observed_through_ms)}</p><p>{format!("Server-observed tool round-trip: {}", duration(entry.span.observed_duration_ms))}</p>
                <p>{format!("Process lifetime: {} · CPU: {}", availability(payload.timing.process_lifetime), availability(payload.timing.cpu))}</p>
                {match &entry.span.category {
                    TaskTimingCategory::DependencyWait { agent_ids, omitted_link_count, targets_observed } => view! { <p>{if !targets_observed { "Delegation targets unobserved; excluded from classified tool time.".to_owned() } else { format!("Observed causal links: {} ({}). Excluded from summed tool time; child work is not added to this wait.", agent_ids.iter().map(|id| payload.breakdown.iter().find(|agent| agent.agent_id == *id).map(|agent| agent.name.as_str()).unwrap_or("Agent outside this scope")).collect::<Vec<_>>().join(", "), omitted_link_count.map(|count| format!("{count} links omitted")).unwrap_or_else(|| "additional links uncounted".to_owned())) }}</p> }.into_any(),
                    TaskTimingCategory::Tool { classified: false, .. } => view! { <p>"Unclassified calls are excluded from the classified tool sum; causal/process boundaries are unobserved."</p> }.into_any(),
                    _ => view! { <p>"Tool completion is not evidence of background process exit."</p> }.into_any(),
                }}
            </aside> })
        })}
        <details class="task-timing-coverage"><summary>"Coverage details"</summary>
        {move || rollup.get().map(|payload| view! { <p>{format!("Provider-request round-trip: {} · Model compute: {} · Process lifetime: {} · CPU: {}", availability(payload.timing.provider_request_round_trip), availability(payload.timing.model_compute), availability(payload.timing.process_lifetime), availability(payload.timing.cpu))}</p> })}
        <p>"Review-round identities and command-wait interactions not exposed by the typed source remain unobserved. Commands are runs, not agents. No historical timings are reconstructed. Parallel durations are summed, not elapsed time."</p></details>
    </div> }
}

#[component]
fn TaskTimingAgentRow(
    agent_id: protocol::AgentId,
    rollup: Memo<Option<TaskTokenUsagePayload>>,
    selected: RwSignal<Option<(protocol::AgentId, String)>>,
) -> impl IntoView {
    let agent = StoredValue::new(agent_id);
    let name = Memo::new(move |_| {
        rollup.with(|payload| {
            payload.as_ref().and_then(|payload| {
                payload
                    .breakdown
                    .iter()
                    .find(|entry| entry.agent_id == agent.get_value())
                    .map(|entry| entry.name.clone())
            })
        })
    });
    let placements = Memo::new(move |_| {
        let mut entries = rollup.with(|payload| {
            payload
                .as_ref()
                .map(|payload| {
                    payload
                        .timing
                        .spans
                        .iter()
                        .filter(|entry| entry.agent_id == agent.get_value())
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        entries.sort_by_key(|entry| entry.span.started_at_ms);
        let mut lane_ends = Vec::<u64>::new();
        entries
            .into_iter()
            .map(|entry| {
                let lane = lane_ends
                    .iter()
                    .position(|end| *end < entry.span.started_at_ms)
                    .unwrap_or(lane_ends.len());
                if lane == lane_ends.len() {
                    lane_ends.push(entry.span.observed_through_ms);
                } else {
                    lane_ends[lane] = entry.span.observed_through_ms;
                }
                (entry, lane)
            })
            .collect::<Vec<_>>()
    });
    let lanes = Memo::new(move |_| {
        placements.with(|entries| entries.iter().map(|(_, lane)| lane + 1).max().unwrap_or(0))
    });
    let account = Memo::new(move |_| {
        rollup.with(|payload| {
            payload.as_ref().and_then(|payload| {
                payload
                    .timing
                    .accounts
                    .iter()
                    .find(|entry| entry.agent_id == agent.get_value())
                    .map(|entry| entry.account.clone())
            })
        })
    });
    let axis = Memo::new(move |_| {
        rollup.with(|payload| {
            payload
                .as_ref()
                .and_then(|payload| timing_axis(&payload.timing))
        })
    });
    view! { <div class="task-timing-agent">
        <details class="task-timing-events"><summary title=move || name.get()>{move || name.get().unwrap_or_else(|| "Agent unavailable".to_owned())}<span>{move || format!("{} ops{}", placements.with(Vec::len), if lanes.get() > 1 { " · overlaps" } else { "" })}</span></summary>
            {move || account.get().map(|account| {
                let accounted = account.observed_through_ms.saturating_sub(account.started_at_ms);
                view! { <div class="task-time-agent-account"><p>{format!("Accounted {} · {}", duration(accounted), match account.phases.last().map(|phase| phase.end) { Some(TaskTimePhaseEnd::Open) => "live, observed so far", Some(TaskTimePhaseEnd::Interrupted) => "last endpoint unknown", _ => "ended" })}</p>{phase_totals_view(&account.totals, accounted)}</div> }
            })}
            <For each={move || placements.with(|entries| entries.iter().map(|(entry, _)| entry.span.tool_call_id.clone()).collect::<Vec<_>>())} key=|id| id.clone() children=move |tool_call_id| view! {
                <TaskTimingOperation tool_call_id=tool_call_id placements=placements lanes=lanes rollup=rollup selected=selected segment=false />
            } />
        </details>
        <div class="task-time-lanes">
        <div class="task-time-phase-lane" role="group" aria-label="Exclusive phase lane">
            {move || match (account.get(), axis.get()) {
                (Some(account), Some(axis)) => {
                    let retained_from = account.phases.first().map_or(account.observed_through_ms, |phase| phase.started_at_ms);
                    let compacted = (account.compacted_phase_count > 0 && retained_from > account.started_at_ms).then(|| {
                        let label = format!("{} earlier phases not retained · {} included in totals", account.compacted_phase_count, duration(retained_from - account.started_at_ms));
                        view! { <span class="task-time-phase task-time-compacted" style=axis_style(axis, account.started_at_ms, retained_from) title=label.clone() aria-label=label></span> }
                    });
                    let phases = account.phases.into_iter().map(|phase| {
                        let label = format!("{} · {} · {}", phase_label(phase.phase), duration(phase.ended_at_ms.saturating_sub(phase.started_at_ms)), match phase.end {
                            TaskTimePhaseEnd::Open => "in progress, observed so far",
                            TaskTimePhaseEnd::Ended => "ended",
                            TaskTimePhaseEnd::Interrupted => "interrupted; endpoint unknown",
                        });
                        view! { <span class=format!("task-time-phase {}{}", phase_class(phase.phase), if phase.end == TaskTimePhaseEnd::Interrupted { " task-time-interrupted" } else { "" }) style=axis_style(axis, phase.started_at_ms, phase.ended_at_ms) title=label.clone() aria-label=label></span> }
                    }).collect_view();
                    view! { {compacted}{phases} }.into_any()
                }
                _ => view! { <span class="task-time-no-account">"No wall-clock account recorded"</span> }.into_any(),
            }}
        </div>
        <div class="task-timing-lane" style=move || format!("height: {}px", 12.max(lanes.get() * 8)) role="group" aria-label="Tool round-trips; may overlap">
            <For each={move || placements.with(|entries| entries.iter().map(|(entry, _)| entry.span.tool_call_id.clone()).collect::<Vec<_>>())} key=|id| id.clone() children=move |tool_call_id| view! {
                <TaskTimingOperation tool_call_id=tool_call_id placements=placements lanes=lanes rollup=rollup selected=selected segment=true />
            } />
        </div>
        </div>
    </div> }
}

#[component]
fn TaskTimingOperation(
    tool_call_id: String,
    placements: Memo<Vec<(protocol::TaskTimingEntry, usize)>>,
    lanes: Memo<usize>,
    rollup: Memo<Option<TaskTokenUsagePayload>>,
    selected: RwSignal<Option<(protocol::AgentId, String)>>,
    segment: bool,
) -> impl IntoView {
    let entry = Memo::new(move |_| {
        placements.with(|entries| {
            entries
                .iter()
                .find(|(entry, _)| entry.span.tool_call_id == tool_call_id)
                .cloned()
        })
    });
    let select = move || {
        if let Some((entry, _)) = entry.get_untracked() {
            selected.set(Some((entry.agent_id, entry.span.tool_call_id)));
        }
    };
    let description = Memo::new(move |_| {
        entry.get().map(|(entry, _)| {
            format!(
                "{} · {} · {} · start {} ms",
                timing_label(&entry.span.category).0,
                duration(entry.span.observed_duration_ms),
                timing_state(entry.span.state),
                entry.span.started_at_ms
            )
        })
    });
    view! { <button
        class=move || if segment { entry.get().map(|(entry, _)| format!("task-timing-segment {}", timing_label(&entry.span.category).1)) } else { Some("task-timing-event".to_owned()) }
        style=move || {
            if !segment { return None; }
            let (entry, lane) = entry.get()?;
            let axis = rollup.with(|payload| payload.as_ref().and_then(|payload| timing_axis(&payload.timing)))?;
            let span = entry.span;
            Some(format!("{}; top: {}%; height: {}%", axis_style(axis, span.started_at_ms, span.observed_through_ms), 100.0 * lane as f64 / lanes.get() as f64, 100.0 / lanes.get() as f64))
        }
        title=move || description.get() aria-label=move || description.get() aria-controls="task-timing-operation"
        aria-pressed=move || entry.get().is_some_and(|(entry, _)| selected.get().as_ref() == Some(&(entry.agent_id, entry.span.tool_call_id))).to_string()
        on:mouseenter=move |_| { if segment { select(); } }
        on:focus=move |_| select()
        on:click=move |_| select()>
        {move || (!segment).then(|| entry.get().map(|(entry, _)| format!("{} · {} · {}", timing_label(&entry.span.category).0, duration(entry.span.observed_duration_ms), timing_state(entry.span.state))))}
    </button> }
}
