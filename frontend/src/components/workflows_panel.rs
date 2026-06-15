use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentId, BackendKind, ProjectId, WorkflowDiagnostic, WorkflowDiagnosticSeverity, WorkflowId,
    WorkflowRunId, WorkflowRunSnapshot, WorkflowRunSnapshotStatus, WorkflowSourceScope,
    WorkflowStepRunId, WorkflowStepRunSnapshot, WorkflowStepRunSnapshotStatus, WorkflowSummary,
};

use crate::send;
use crate::state::{ActiveAgentRef, AgentInfo, AppState, TabContent};

fn backend_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Kiro => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Antigravity => "backend-badge antigravity",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
    }
}

fn run_status_label(status: WorkflowRunSnapshotStatus) -> &'static str {
    match status {
        WorkflowRunSnapshotStatus::Running => "Running",
        WorkflowRunSnapshotStatus::Completed => "Completed",
        WorkflowRunSnapshotStatus::Failed => "Failed",
        WorkflowRunSnapshotStatus::Cancelled => "Cancelled",
    }
}

fn run_status_class(status: WorkflowRunSnapshotStatus) -> &'static str {
    match status {
        WorkflowRunSnapshotStatus::Running => "workflow-status running",
        WorkflowRunSnapshotStatus::Completed => "workflow-status completed",
        WorkflowRunSnapshotStatus::Failed => "workflow-status failed",
        WorkflowRunSnapshotStatus::Cancelled => "workflow-status cancelled",
    }
}

fn step_status_label(status: WorkflowStepRunSnapshotStatus) -> &'static str {
    match status {
        WorkflowStepRunSnapshotStatus::Pending => "Pending",
        WorkflowStepRunSnapshotStatus::Running => "Running",
        WorkflowStepRunSnapshotStatus::Completed => "Completed",
        WorkflowStepRunSnapshotStatus::Failed => "Failed",
        WorkflowStepRunSnapshotStatus::Cancelled => "Cancelled",
    }
}

fn source_project_id(summary: &WorkflowSummary) -> Option<ProjectId> {
    match &summary.source.scope {
        WorkflowSourceScope::Global => None,
        WorkflowSourceScope::Project { project_id, .. } => Some(project_id.clone()),
    }
}

fn summary_matches_context(summary: &WorkflowSummary, active_project: Option<&ProjectId>) -> bool {
    match &summary.source.scope {
        WorkflowSourceScope::Global => true,
        WorkflowSourceScope::Project { project_id, .. } => active_project == Some(project_id),
    }
}

fn diagnostic_matches_context(
    diagnostic: &WorkflowDiagnostic,
    active_project: Option<&ProjectId>,
) -> bool {
    match diagnostic.source.as_ref().map(|source| &source.scope) {
        Some(WorkflowSourceScope::Global) | None => true,
        Some(WorkflowSourceScope::Project { project_id, .. }) => active_project == Some(project_id),
    }
}

fn run_matches_context(run: &WorkflowRunSnapshot, active_project: Option<&ProjectId>) -> bool {
    match active_project {
        Some(active_project) => run
            .project_id
            .as_ref()
            .is_none_or(|id| id == active_project),
        None => run.project_id.is_none(),
    }
}

fn diagnostic_key(index: usize, diagnostic: &WorkflowDiagnostic) -> String {
    let source_path = diagnostic
        .source
        .as_ref()
        .map(|source| source.path.as_str())
        .unwrap_or("<unknown>");
    let workflow_id = diagnostic
        .workflow_id
        .as_ref()
        .map(|id| id.0.as_str())
        .unwrap_or("<none>");
    format!(
        "{index}:{workflow_id}:{source_path}:{:?}:{}",
        diagnostic.severity, diagnostic.message
    )
}

fn source_label(scope: &WorkflowSourceScope) -> String {
    match scope {
        WorkflowSourceScope::Global => "Global".to_owned(),
        WorkflowSourceScope::Project { root, .. } => format!("Project · {}", root.0),
    }
}

fn open_agent_chat(state: &AppState, host_id: String, agent_id: AgentId, label: String) {
    state.open_tab(
        TabContent::chat_with_agent(ActiveAgentRef { host_id, agent_id }),
        label,
        true,
    );
}

fn agent_name_for_host(agents: &[AgentInfo], host_id: &str, agent_id: &AgentId) -> String {
    agents
        .iter()
        .find(|agent| agent.host_id == host_id && agent.agent_id == *agent_id)
        .map(|agent| agent.name.clone())
        .unwrap_or_else(|| agent_id.0.clone())
}

fn agent_button_view(
    state: AppState,
    host_id: String,
    agent_id: AgentId,
    label: String,
) -> AnyView {
    let name_state = state.clone();
    let name_host = host_id.clone();
    let name_agent_id = agent_id.clone();
    let name = Memo::new(move |_| {
        name_state
            .agents
            .with(|agents| agent_name_for_host(agents, &name_host, &name_agent_id))
    });
    let title = move || format!("Open chat for {}", name.get());
    let open_state = state;
    let open_host = host_id;
    let open_agent_id = agent_id;
    view! {
        <button
            type="button"
            class="workflow-agent-row"
            title=title
            on:click=move |_| {
                open_agent_chat(
                    &open_state,
                    open_host.clone(),
                    open_agent_id.clone(),
                    name.get_untracked(),
                )
            }
        >
            <span class="workflow-agent-row-label">{label}</span>
            <span class="workflow-agent-row-name">{move || name.get()}</span>
        </button>
    }
    .into_any()
}

fn step_tree_views(
    state: AppState,
    host_id: String,
    steps: &[WorkflowStepRunSnapshot],
    parent_id: Option<&WorkflowStepRunId>,
    depth: usize,
) -> Vec<AnyView> {
    let mut children = steps
        .iter()
        .filter(|step| step.parent_step_id.as_ref() == parent_id)
        .cloned()
        .collect::<Vec<_>>();
    children.sort_by_key(|step| step.created_at_ms);

    children
        .into_iter()
        .map(|step| {
            let nested = step_tree_views(
                state.clone(),
                host_id.clone(),
                steps,
                Some(&step.id),
                depth + 1,
            );
            let margin = format!("margin-left: {}px;", depth * 14);
            let agent = step.agent_id.as_ref().map(|agent_id| {
                agent_button_view(
                    state.clone(),
                    host_id.clone(),
                    agent_id.clone(),
                    "Agent".to_owned(),
                )
            });
            let message = step.message.clone();
            view! {
                <details class="workflow-step" open=true style=margin>
                    <summary class="workflow-step-summary">
                        <span class="workflow-step-title">{step.title}</span>
                        <span class="workflow-step-status">{step_status_label(step.status)}</span>
                    </summary>
                    {agent}
                    {message.map(|message| view! { <div class="workflow-step-message">{message}</div> })}
                    <div class="workflow-step-children">{nested}</div>
                </details>
            }
            .into_any()
        })
        .collect()
}

fn agent_rows_for_run(run: &WorkflowRunSnapshot) -> Vec<(AgentId, String)> {
    let mut agent_ids = Vec::new();
    if let Some(coordinator) = run.coordinator_agent_id.clone() {
        agent_ids.push((coordinator, "Coordinator".to_owned()));
    }
    let mut seen = agent_ids
        .iter()
        .map(|(agent_id, _)| agent_id.clone())
        .collect::<HashSet<_>>();
    for agent_id in run.agent_ids.clone() {
        if seen.insert(agent_id.clone()) {
            agent_ids.push((agent_id, "Agent".to_owned()));
        }
    }
    agent_ids
}

#[component]
pub fn WorkflowsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();

    let active_host = {
        let state = state.clone();
        Memo::new(move |_| {
            state
                .active_project
                .get()
                .map(|active| active.host_id)
                .or_else(|| state.selected_host_id.get())
        })
    };
    let active_project = {
        let state = state.clone();
        Memo::new(move |_| state.active_project.get().map(|active| active.project_id))
    };

    let summary_ids = {
        let state = state.clone();
        Memo::new(move |_| {
            let Some(host_id) = active_host.get() else {
                return Vec::new();
            };
            let active_project_id = active_project.get();
            state
                .workflow_summaries
                .with(|map| map.get(&host_id).cloned().unwrap_or_default())
                .into_iter()
                .filter(|summary| summary_matches_context(summary, active_project_id.as_ref()))
                .map(|summary| summary.id)
                .collect::<Vec<_>>()
        })
    };

    let diagnostics = {
        let state = state.clone();
        Memo::new(move |_| {
            let Some(host_id) = active_host.get() else {
                return Vec::new();
            };
            let active_project_id = active_project.get();
            state
                .workflow_diagnostics
                .with(|map| map.get(&host_id).cloned().unwrap_or_default())
                .into_iter()
                .filter(|diagnostic| {
                    diagnostic_matches_context(diagnostic, active_project_id.as_ref())
                })
                .enumerate()
                .map(|(index, diagnostic)| (diagnostic_key(index, &diagnostic), diagnostic))
                .collect::<Vec<_>>()
        })
    };

    let run_ids = {
        let state = state.clone();
        Memo::new(move |_| {
            let Some(host_id) = active_host.get() else {
                return Vec::new();
            };
            let active_project_id = active_project.get();
            let mut runs = state.workflow_runs.with(|map| {
                map.get(&host_id)
                    .map(|runs| {
                        runs.values()
                            .filter(|run| run_matches_context(run, active_project_id.as_ref()))
                            .map(|run| (run.id.clone(), run.created_at_ms))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            runs.sort_by_key(|(_, created_at_ms)| std::cmp::Reverse(*created_at_ms));
            runs.into_iter().map(|(run_id, _)| run_id).collect()
        })
    };

    let refresh = move |_| {
        let Some(host_id) = active_host.get_untracked() else {
            return;
        };
        let Some(host_stream) = state
            .host_streams
            .with_untracked(|streams| streams.get(&host_id).cloned())
        else {
            return;
        };
        spawn_local(async move {
            if let Err(error) = send::workflow_refresh(&host_id, host_stream).await {
                log::error!("failed to refresh workflows: {error}");
            }
        });
    };

    view! {
        <div class="workflows-panel">
            <div class="panel-header workflows-panel-header">
                <div>
                    <div class="panel-title">"Workflows"</div>
                    <div class="panel-subtitle">"Markdown workflows for this host/project"</div>
                </div>
                <button type="button" class="filter-toggle" on:click=refresh>"Refresh"</button>
            </div>

            {move || if active_host.get().is_none() {
                view! { <div class="empty-state">"Connect to a host to use Workflows."</div> }.into_any()
            } else {
                view! {
                    <div class="workflows-panel-body">
                        <section class="workflow-section">
                            <h3>"Catalog"</h3>
                            <For
                                each=move || diagnostics.get()
                                key=|(key, _)| key.clone()
                                children=move |(_, diagnostic)| {
                                    let class = match diagnostic.severity {
                                        WorkflowDiagnosticSeverity::Error => "workflow-diagnostic error",
                                        WorkflowDiagnosticSeverity::Warning => "workflow-diagnostic warning",
                                    };
                                    view! { <div class=class>{diagnostic.message}</div> }
                                }
                            />
                            <For
                                each=move || summary_ids.get()
                                key=|workflow_id| workflow_id.0.clone()
                                let:workflow_id
                            >
                                <WorkflowSummaryCard
                                    active_host=active_host
                                    active_project=active_project
                                    workflow_id=workflow_id
                                />
                            </For>
                            {move || summary_ids.get().is_empty().then(|| view! {
                                <div class="empty-state">"No workflows found for the current context."</div>
                            })}
                        </section>
                        <section class="workflow-section">
                            <h3>"Runs"</h3>
                            <For
                                each=move || run_ids.get()
                                key=|run_id| run_id.0.clone()
                                let:run_id
                            >
                                <WorkflowRunCard
                                    active_host=active_host
                                    active_project=active_project
                                    run_id=run_id
                                />
                            </For>
                            {move || run_ids.get().is_empty().then(|| view! {
                                <div class="empty-state">"No workflow runs yet."</div>
                            })}
                        </section>
                    </div>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn WorkflowSummaryCard(
    active_host: Memo<Option<String>>,
    active_project: Memo<Option<ProjectId>>,
    workflow_id: WorkflowId,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let workflow_id_for_lookup = workflow_id.clone();
    let state_for_lookup = state.clone();
    let summary = Memo::new(move |_| {
        let host_id = active_host.get()?;
        let active_project_id = active_project.get();
        state_for_lookup.workflow_summaries.with(|map| {
            map.get(&host_id).and_then(|summaries| {
                summaries
                    .iter()
                    .find(|summary| {
                        summary.id == workflow_id_for_lookup
                            && summary_matches_context(summary, active_project_id.as_ref())
                    })
                    .cloned()
            })
        })
    });

    view! {
        {move || {
            let Some(summary) = summary.get() else {
                return ().into_any();
            };
            let run_state = state.clone();
            let run_summary = summary.clone();
            let on_run = move |_| {
                let Some(host_id) = active_host.get_untracked() else {
                    return;
                };
                let Some(host_stream) = run_state
                    .host_streams
                    .with_untracked(|streams| streams.get(&host_id).cloned())
                else {
                    return;
                };
                let project_id = source_project_id(&run_summary)
                    .or_else(|| active_project.get_untracked());
                let workflow_id = run_summary.id.clone();
                spawn_local(async move {
                    if let Err(error) =
                        send::trigger_workflow(&host_id, host_stream, workflow_id, project_id).await
                    {
                        log::error!("failed to trigger workflow: {error}");
                    }
                });
            };
            view! {
                <article class="workflow-card catalog-card">
                    <div class="workflow-card-main">
                        <div class="workflow-card-title">{summary.name}</div>
                        {summary.description.map(|description| view! {
                            <div class="workflow-card-description">{description}</div>
                        })}
                        <div class="workflow-card-meta">
                            <span>{source_label(&summary.source.scope)}</span>
                            <span class={format!("{} workflow-backend", backend_class(summary.coordinator.backend))}>
                                {backend_label(summary.coordinator.backend)}
                            </span>
                            {(!summary.declared_backends.is_empty()).then(|| view! {
                                <span class="workflow-declared-backends">
                                    "Declares "
                                    {summary.declared_backends.iter().map(|backend| backend_label(*backend)).collect::<Vec<_>>().join(", ")}
                                </span>
                            })}
                        </div>
                    </div>
                    <button type="button" class="primary-button workflow-run-button" on:click=on_run>"Run"</button>
                </article>
            }
            .into_any()
        }}
    }
}

#[component]
fn WorkflowRunCard(
    active_host: Memo<Option<String>>,
    active_project: Memo<Option<ProjectId>>,
    run_id: WorkflowRunId,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let run_id_for_lookup = run_id.clone();
    let state_for_lookup = state.clone();
    let run = Memo::new(move |_| {
        let host_id = active_host.get()?;
        let active_project_id = active_project.get();
        state_for_lookup.workflow_runs.with(|map| {
            map.get(&host_id)
                .and_then(|runs| runs.get(&run_id_for_lookup).cloned())
                .filter(|run| run_matches_context(run, active_project_id.as_ref()))
        })
    });

    view! {
        {move || {
            let Some(host_id) = active_host.get() else {
                return ().into_any();
            };
            let Some(run) = run.get() else {
                return ().into_any();
            };
            let is_running = run.status == WorkflowRunSnapshotStatus::Running;
            let cancel_run_id = run.id.clone();
            let cancel_state = state.clone();
            let cancel_host = active_host;
            let cancel = move |_| {
                if !is_running {
                    return;
                }
                let Some(host_id) = cancel_host.get_untracked() else {
                    return;
                };
                let Some(host_stream) = cancel_state
                    .host_streams
                    .with_untracked(|streams| streams.get(&host_id).cloned())
                else {
                    return;
                };
                let run_id = cancel_run_id.clone();
                spawn_local(async move {
                    if let Err(error) = send::cancel_workflow(&host_id, host_stream, run_id).await {
                        log::error!("failed to cancel workflow: {error}");
                    }
                });
            };

            let agent_rows = agent_rows_for_run(&run)
                .into_iter()
                .map(|(agent_id, label)| agent_button_view(state.clone(), host_id.clone(), agent_id, label))
                .collect::<Vec<_>>();
            let step_tree = step_tree_views(state.clone(), host_id.clone(), &run.steps, None, 0);

            view! {
                <article class="workflow-card run-card">
                    <div class="workflow-run-header">
                        <div>
                            <div class="workflow-card-title">{run.workflow_name.clone()}</div>
                            <div class="workflow-card-meta">
                                <span class={run_status_class(run.status)}>{run_status_label(run.status)}</span>
                                <span class={format!("{} workflow-backend", backend_class(run.coordinator.backend))}>
                                    {backend_label(run.coordinator.backend)}
                                </span>
                            </div>
                        </div>
                        {is_running.then(|| view! {
                            <button type="button" class="filter-toggle workflow-cancel-button" on:click=cancel>"Cancel"</button>
                        })}
                    </div>
                    {run.summary.map(|summary| view! { <div class="workflow-run-summary">{summary}</div> })}
                    {run.error.map(|error| view! { <div class="workflow-run-error">{error}</div> })}
                    <div class="workflow-agent-list">
                        {agent_rows}
                    </div>
                    <details class="workflow-run-tree" open=true>
                        <summary>"Fan-out tree"</summary>
                        {if step_tree.is_empty() {
                            view! { <div class="empty-state small">"No reported steps yet."</div> }.into_any()
                        } else {
                            view! { <div class="workflow-step-tree">{step_tree}</div> }.into_any()
                        }}
                    </details>
                </article>
            }
            .into_any()
        }}
    }
}
