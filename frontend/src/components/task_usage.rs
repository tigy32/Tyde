use std::collections::HashSet;

use leptos::prelude::*;
use protocol::{
    BackendKind, TaskTimingCategory, TaskTimingState, TaskTokenUsagePayload, TaskTokenUsageScope,
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
        } => format!("Partial · {unavailable_count} unreported scopes"),
        TaskTokenUsageStatus::Unavailable {
            unavailable_count, ..
        } => format!("Unavailable · {unavailable_count} unreported scopes"),
    }
}

fn aggregate_view(total: &protocol::TaskTokenUsageAggregate) -> AnyView {
    match &total.status {
        TaskTokenUsageStatus::Unavailable { reasons, .. } => view! {
            <span class="session-task-row-unavailable">{format!("Usage not reported · {}", reasons.iter().map(|reason| task_usage_unavailable_text(*reason)).collect::<Vec<_>>().join(", "))}</span>
        }.into_any(),
        _ => task_scope_view(&TaskTokenUsageScope::Known { usage: Box::new(total.usage.clone()) }),
    }
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
                <h2 node_ref=heading tabindex="-1">{move || if expanded.get() { "Task usage" } else { "By backend" }}</h2>
                <span class="task-usage-scope">"Current root agent + descendants"</span>
                <Show when=move || !expanded.get()>
                    <button class="task-usage-expand" on:click=move |_| expanded.set(true)>"Expand ↗"</button>
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
            <div class="task-usage-summary">
                <Show when=move || expanded.get()>{move || rollup.get().map(|payload| aggregate_view(&payload.total))}</Show>
                {move || rollup.get().map(|payload| view! {
                    <span>{format!("{} agent{} · {} backend{}", payload.breakdown.len(), if payload.breakdown.len() == 1 { "" } else { "s" }, payload.backends.len(), if payload.backends.len() == 1 { "" } else { "s" })}</span>
                    <span class="session-task-popover-status">{status_text(&payload.total.status)}</span>
                })}
            </div>
            <div class="task-usage-body" id=move || panel_id.get() role="tabpanel" aria-label=move || match tab.get() { UsageTab::Overview => "Overview", UsageTab::Time => "Time breakdown", UsageTab::Agents => "Agents" }>
                {move || rollup.get().and_then(|payload| (!payload.helper_usage.is_empty()).then(|| view! {
                    <p class="task-usage-note">{format!("Includes {} associated analyzer batches, grouped by their reported backend. Shared batches are source scopes, not per-task allocations or additional agents; their timing is unobserved.", payload.helper_usage.len())}</p>
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
                        <label>"Backend "<select aria-label="Filter by backend" on:change=move |event| {
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
                    <p class="task-usage-note">"Per-agent own usage · closed descendants included · no historical model allocation"</p>
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
                                    <div class="session-task-row" style=format!("padding-left: {}px", 8 + if flat_view { 0 } else { entry.depth.min(12) * 18 })>
                                        {(has_children && !flat_view).then(|| view! { <button class="task-tree-toggle" aria-label=format!("{} {}", if collapsed_now { "Expand" } else { "Collapse" }, entry.name) aria-expanded=(!collapsed_now).to_string() on:click=move |_| collapsed.update(|ids| { if !ids.remove(&toggle_id) { ids.insert(toggle_id.clone()); } })>{if collapsed_now { "›" } else { "⌄" }}</button> })}
                                        {(!has_children && !flat_view).then(|| view! { <span class="task-tree-spacer" aria-hidden="true"></span> })}
                                        <button class="session-task-row-name task-agent-details" aria-pressed=move || (selected_agent.get().as_ref() == Some(&detail_id)).to_string() on:click={let id = id.clone(); move |_| selected_agent.set(Some(id.clone()))}>{entry.name.clone()}</button>
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
                <button class="task-backend-drilldown" on:click=move |_| drilldown.run(kind)>{crate::components::launch_menu::backend_icon(kind)}{backend_label(kind)}<span>{move || group.get().map(|group| format!(" · {} agent{} ›", group.agent_count, if group.agent_count == 1 { "" } else { "s" }))}</span></button>
                <div role="group" aria-label=format!("{} task token usage", backend_label(kind))>{move || group.get().map(|group| aggregate_view(&group.total))}</div>
                {move || group.get().and_then(|group| (group.analyzer_batch_count > 0).then(|| view! { <span class="task-backend-analyzers">{format!("{} analyzer batch{}", group.analyzer_batch_count, if group.analyzer_batch_count == 1 { "" } else { "es" })}</span> }))}
                <span class="task-backend-coverage">{move || group.get().and_then(|group| (!matches!(group.total.status, TaskTokenUsageStatus::Known)).then(|| format!("{} · {}/{} agents reporting · {} agents incomplete", status_text(&group.total.status), group.reporting_agent_count, group.agent_count, group.incomplete_agent_count)))}</span>
            </div>
            {move || binding.get().map(|(host, _)| view! { <TaskCapacityLimits host_id=host backend_kind=kind /> })}
        </article>
    }
}

#[component]
fn TaskTimeView(rollup: Memo<Option<TaskTokenUsagePayload>>) -> impl IntoView {
    let selected = RwSignal::new(None::<(protocol::AgentId, String)>);
    view! { <div class="task-time-view">
        <p class="task-usage-note">"Retained live observations, not lifetime coverage or transcript timestamps. Tool round-trips are not process lifetime or model compute. Gaps are unobserved."</p>
        {move || rollup.get().map(|payload| {
            let timing = payload.timing;
            let range = match (timing.observed_start_ms, timing.observed_end_ms) { (Some(start), Some(end)) => duration(end.saturating_sub(start)), _ => "Unobserved".to_owned() };
            view! {
                <dl class="task-time-metrics">
                    <div><dt>"Observed span range"</dt><dd>{range}</dd></div>
                    <div><dt>"Classified tools, summed parallel"</dt><dd>{if timing.classified_tool_count == 0 { "Unobserved".to_owned() } else { duration(timing.summed_tool_ms) }}</dd></div>
                    <div><dt>"Dependency waits, summed separately"</dt><dd>{if timing.covered_agent_count == 0 { "Unobserved".to_owned() } else { duration(timing.summed_dependency_wait_ms) }}</dd></div>
                    <div><dt>"Unclassified round-trips (excluded)"</dt><dd>{duration(timing.summed_unclassified_ms)}</dd></div>
                    <div><dt>"Agents with observations"</dt><dd>{format!("{}/{} agents", timing.covered_agent_count, payload.breakdown.len())}</dd></div>
                </dl>
                <p class="task-usage-note">{format!("{} command runs · {} spans dropped · {} additional spans omitted · {} persistence failures", timing.command_count, timing.dropped_span_count, timing.omitted_span_count, timing.persistence_failed_agent_count)}</p>
            }
        })}
        <div class="task-timing-legend"><span class="timing-command">"Commands (tool round-trip)"</span><span class="timing-tool">"Other tools"</span><span class="timing-wait">"Dependency waits"</span><span>"Interrupted = unknown endpoint"</span></div>
        <div class="task-timeline" role="group" aria-label="Observed tool timeline">
            {move || rollup.get().map(|payload| {
                let start = payload.timing.observed_start_ms;
                let end = payload.timing.observed_end_ms;
                payload.timing.spans.iter().map(|entry| {
                    let span = &entry.span;
                    let agent = payload.breakdown.iter().find(|agent| agent.agent_id == entry.agent_id).map(|agent| agent.name.clone()).unwrap_or_else(|| "Agent unavailable".to_owned());
                    let (label, class) = match &span.category {
                        TaskTimingCategory::Command { command, check } => (if *check { format!("Check · {command}") } else { command.clone() }, "timing-command"),
                        TaskTimingCategory::DependencyWait { agent_ids, omitted_link_count, targets_observed } => (if *targets_observed { format!("Await · {} linked{}", agent_ids.len(), match omitted_link_count { Some(0) => String::new(), Some(count) => format!(" · {count} omitted"), None => " · additional links uncounted".to_owned() }) } else { "Delegation · targets unobserved".to_owned() }, "timing-wait"),
                        TaskTimingCategory::Tool { name, classified } => (format!("{name}{}", if *classified { "" } else { " · unclassified" }), "timing-tool"),
                    };
                    let key = (entry.agent_id.clone(), span.tool_call_id.clone());
                    let pressed_key = key.clone();
                    let state = match span.state { TaskTimingState::Open => "Open (observed so far)", TaskTimingState::Completed => "Completed", TaskTimingState::Cancelled => "Cancelled", TaskTimingState::Failed => "Failed", TaskTimingState::Interrupted => "Interrupted; endpoint unknown" };
                    let width = match (start, end) { (Some(start), Some(end)) if end > start => 100.0 * span.observed_through_ms.saturating_sub(span.started_at_ms) as f64 / (end - start) as f64, _ => 0.0 };
                    let offset = match (start, end) { (Some(start), Some(end)) if end > start => 100.0 * span.started_at_ms.saturating_sub(start) as f64 / (end - start) as f64, _ => 0.0 };
                    view! { <button class="task-timing-row" aria-pressed=move || (selected.get().as_ref() == Some(&pressed_key)).to_string() on:click={let key = key.clone(); move |_| selected.set(Some(key.clone()))}>
                        <span>{agent}</span><span class="task-timing-label">{label}</span>
                        <span class="task-timing-lane"><span class=class style=format!("left: {offset}%; width: {width}%")></span></span>
                        <span>{format!("{} · {state}", duration(span.observed_duration_ms))}</span>
                    </button> }
                }).collect_view()
            })}
        </div>
        {move || rollup.get().and_then(|payload| {
            let key = selected.get()?;
            let entry = payload.timing.spans.iter().find(|entry| entry.agent_id == key.0 && entry.span.tool_call_id == key.1)?;
            Some(view! { <aside class="task-timing-detail"><h3>"Observed operation"</h3><p>{format!("Server-observed tool round-trip: {}", duration(entry.span.observed_duration_ms))}</p>
                <p>{format!("Process lifetime: {} · CPU: {}", availability(payload.timing.process_lifetime), availability(payload.timing.cpu))}</p>
                {match &entry.span.category {
                    TaskTimingCategory::DependencyWait { agent_ids, omitted_link_count, targets_observed } => view! { <p>{if !targets_observed { "Delegation targets unobserved; excluded from classified tool time.".to_owned() } else { format!("Observed causal links: {} ({}). Excluded from summed tool time; child work is not added to this wait.", agent_ids.iter().map(|id| payload.breakdown.iter().find(|agent| agent.agent_id == *id).map(|agent| agent.name.as_str()).unwrap_or("Agent outside this scope")).collect::<Vec<_>>().join(", "), omitted_link_count.map(|count| format!("{count} links omitted")).unwrap_or_else(|| "additional links uncounted".to_owned())) }}</p> }.into_any(),
                    TaskTimingCategory::Tool { classified: false, .. } => view! { <p>"Unclassified calls are excluded from the classified tool sum; causal/process boundaries are unobserved."</p> }.into_any(),
                    _ => view! { <p>"Tool completion is not evidence of background process exit."</p> }.into_any(),
                }}
            </aside> })
        })}
        {move || rollup.get().map(|payload| view! { <p class="task-usage-note">{format!("Provider-request round-trip: {} · Model compute: {} · Process lifetime: {} · CPU: {}", availability(payload.timing.provider_request_round_trip), availability(payload.timing.model_compute), availability(payload.timing.process_lifetime), availability(payload.timing.cpu))}</p> })}
        <p class="task-usage-note">"Review-round identities and command-wait interactions not exposed by the typed source remain unobserved. Commands are runs, not agents. No historical timings are reconstructed."</p>
    </div> }
}
