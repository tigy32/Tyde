use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentId, BackendKind, ProjectId, WorkflowCatalogLocation, WorkflowDiagnostic,
    WorkflowDiagnosticSeverity, WorkflowId, WorkflowRunId, WorkflowRunSnapshot,
    WorkflowRunSnapshotStatus, WorkflowSourceScope, WorkflowStepRunId, WorkflowStepRunSnapshot,
    WorkflowStepRunSnapshotStatus, WorkflowSummary,
};

use crate::actions;
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

/// A stable, source-aware key for a catalog row. The same workflow id can be
/// defined once globally and once in a project, so keying `<For>` rows by
/// `workflow_id` alone collides — the two definitions would share one keyed row
/// and the row would render whichever the lookup happened to find first. Keying
/// by `(id, source path)` keeps each definition distinct.
pub(crate) fn summary_row_key(summary: &WorkflowSummary) -> String {
    // `\u{1f}` (unit separator) cannot appear in a workflow id (slug) and is
    // vanishingly unlikely in a path, so it makes an unambiguous composite key.
    format!("{}\u{1f}{}", summary.id.0, summary.source.path)
}

/// Resolve the workflows that are effective for the active context.
///
/// A project workflow *shadows* a same-id global workflow when that project is
/// the active context: the project definition is shown and runs; the global one
/// is hidden in that project only. Outside that project (other projects, or
/// host/global context) the global workflow stays visible and runnable. This is
/// the frontend half of the Phase 1 scoped-shadowing rework: `WorkflowNotify`
/// carries both the global and the project summary, and the panel projects the
/// active-context view here rather than the server pre-hiding one of them.
pub(crate) fn effective_summaries(
    summaries: &[WorkflowSummary],
    active_project: Option<&ProjectId>,
) -> Vec<WorkflowSummary> {
    // Ids that have a project definition in the *active* project shadow the
    // same-id global definition.
    let shadowed_ids: HashSet<&str> = summaries
        .iter()
        .filter(|summary| match &summary.source.scope {
            WorkflowSourceScope::Project { project_id, .. } => active_project == Some(project_id),
            WorkflowSourceScope::Global => false,
        })
        .map(|summary| summary.id.0.as_str())
        .collect();

    summaries
        .iter()
        .filter(|summary| summary_matches_context(summary, active_project))
        .filter(|summary| match &summary.source.scope {
            // Hide a global only when the active project shadows its id.
            WorkflowSourceScope::Global => !shadowed_ids.contains(summary.id.0.as_str()),
            WorkflowSourceScope::Project { .. } => true,
        })
        .cloned()
        .collect()
}

/// Catalog directories relevant to the active context: the global directory
/// plus the active project's directories. Drawn from server-sent locations so
/// the UI never reconstructs `.tyde/workflows` paths by string convention.
pub(crate) fn context_locations(
    locations: &[WorkflowCatalogLocation],
    active_project: Option<&ProjectId>,
) -> Vec<WorkflowCatalogLocation> {
    locations
        .iter()
        .filter(|location| match &location.scope {
            WorkflowSourceScope::Global => true,
            WorkflowSourceScope::Project { project_id, .. } => active_project == Some(project_id),
        })
        .cloned()
        .collect()
}

/// Build the editable composer prompt for the authoring CTA from server-sent
/// catalog locations. Lists the active project's directories first as the
/// preferred target when present, otherwise the global directory; the prompt is
/// a starting point the user edits before sending.
pub(crate) fn build_workflow_authoring_prompt(
    locations: &[WorkflowCatalogLocation],
    active_project: Option<&ProjectId>,
) -> String {
    let context = context_locations(locations, active_project);
    let project_dirs: Vec<&str> = context
        .iter()
        .filter(|location| matches!(location.scope, WorkflowSourceScope::Project { .. }))
        .map(|location| location.directory.as_str())
        .collect();
    let global_dirs: Vec<&str> = context
        .iter()
        .filter(|location| matches!(location.scope, WorkflowSourceScope::Global))
        .map(|location| location.directory.as_str())
        .collect();

    let mut prompt = String::from(
        "Create a Tyde workflow that ...\n\
         \n\
         A Tyde workflow is a Markdown file with YAML frontmatter that a coordinator \
         agent runs.\n",
    );

    if !project_dirs.is_empty() {
        prompt.push_str("\nPreferred workflow target:\n");
        for dir in &project_dirs {
            prompt.push_str(&format!("- Project: {dir}\n"));
        }
        if !global_dirs.is_empty() {
            prompt.push_str("\nOptional global target:\n");
            for dir in &global_dirs {
                prompt.push_str(&format!("- Global: {dir}\n"));
            }
        }
    } else {
        prompt.push_str("\nPreferred workflow target:\n");
        for dir in &global_dirs {
            prompt.push_str(&format!("- Global: {dir}\n"));
        }
    }

    prompt.push_str("\nUse tyde_workflow_targets first, then save it with tyde_workflow_save.\n");
    prompt
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

fn location_scope_label(scope: &WorkflowSourceScope) -> &'static str {
    match scope {
        WorkflowSourceScope::Global => "Global",
        WorkflowSourceScope::Project { .. } => "Project",
    }
}

/// Teaching empty state shown when the active context has no runnable
/// workflows. Explains the agent-authored model, lists the real catalog
/// directories from server-sent locations, and offers a CTA that opens a new
/// chat pre-filled with an editable authoring prompt.
fn workflow_empty_state(
    state: AppState,
    active_host: Memo<Option<String>>,
    active_project: Memo<Option<ProjectId>>,
    locations: Memo<Vec<WorkflowCatalogLocation>>,
) -> AnyView {
    let on_create = move |_| {
        let Some(host_id) = active_host.get_untracked() else {
            return;
        };
        let project_id = active_project.get_untracked();
        let prompt =
            build_workflow_authoring_prompt(&locations.get_untracked(), project_id.as_ref());
        actions::open_new_chat_with_prefill(&state, host_id, project_id, prompt);
    };

    view! {
        <div class="empty-state workflow-empty-state">
            <div class="empty-state-title">"No workflows yet"</div>
            <div class="empty-state-body">
                "Workflows are runnable playbooks an agent writes for you — Markdown files \
                 with YAML frontmatter saved under a .tyde/workflows directory. Ask an agent \
                 to author one and Tyde discovers and runs it; you don't hand-write the file."
            </div>
            <div class="empty-state-paths">
                <div class="empty-state-paths-label">"Catalog directories"</div>
                <For
                    each=move || locations.get()
                    key=|location| location.directory.clone()
                    children=move |location| {
                        let exists_hint = if location.exists { "" } else { " · not created yet" };
                        view! {
                            <div class="empty-state-path">
                                <span class="empty-state-path-scope">
                                    {location_scope_label(&location.scope)}
                                </span>
                                <span class="empty-state-path-dir">{location.directory}</span>
                                <span class="empty-state-path-hint">{exists_hint}</span>
                            </div>
                        }
                    }
                />
            </div>
            <button type="button" class="primary-button" on:click=on_create>
                "Ask an agent to create a workflow"
            </button>
        </div>
    }
    .into_any()
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

    // Source-aware catalog rows for the active context. Each row carries its
    // stable key plus the `(workflow_id, source path)` needed to look the
    // summary back up reactively in the card. Computed via `effective_summaries`
    // so a project workflow shadows the same-id global in its own project.
    let summary_rows = {
        let state = state.clone();
        Memo::new(move |_| {
            let Some(host_id) = active_host.get() else {
                return Vec::new();
            };
            let active_project_id = active_project.get();
            let summaries = state
                .workflow_summaries
                .with(|map| map.get(&host_id).cloned().unwrap_or_default());
            effective_summaries(&summaries, active_project_id.as_ref())
                .into_iter()
                .map(|summary| {
                    (
                        summary_row_key(&summary),
                        summary.id.clone(),
                        summary.source.path.clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
    };

    // Catalog directories for the active context, used by the teaching empty
    // state and the authoring CTA. Read straight from server-sent locations.
    let context_locations_memo = {
        let state = state.clone();
        Memo::new(move |_| {
            let Some(host_id) = active_host.get() else {
                return Vec::new();
            };
            let active_project_id = active_project.get();
            let locations = state
                .workflow_locations
                .with(|map| map.get(&host_id).cloned().unwrap_or_default());
            context_locations(&locations, active_project_id.as_ref())
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

    let refresh = {
        let state = state.clone();
        move |_| {
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
        }
    };

    view! {
        <div class="workflows-panel">
            <div class="panel-header workflows-panel-header">
                <div>
                    <div class="panel-title">"Workflows"</div>
                    <div class="panel-subtitle">"Runnable playbooks your agents write for this project"</div>
                </div>
                <button type="button" class="filter-toggle" on:click=refresh>"Refresh"</button>
            </div>

            {move || if active_host.get().is_none() {
                view! { <div class="empty-state">"Connect to a host to use Workflows."</div> }.into_any()
            } else {
                // Per-render clone so the empty-state closure can own its own
                // `AppState` handle without making this outer reactive closure
                // `FnOnce` (it must stay `FnMut` to re-render on signal changes).
                let state = state.clone();
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
                                each=move || summary_rows.get()
                                key=|(row_key, _, _)| row_key.clone()
                                children=move |(_, workflow_id, source_path)| view! {
                                    <WorkflowSummaryCard
                                        active_host=active_host
                                        active_project=active_project
                                        workflow_id=workflow_id
                                        source_path=source_path
                                    />
                                }
                            />
                            {
                                let state = state.clone();
                                move || summary_rows.get().is_empty().then(|| {
                                    workflow_empty_state(
                                        state.clone(),
                                        active_host,
                                        active_project,
                                        context_locations_memo,
                                    )
                                })
                            }
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
    /// Source file path of the *effective* definition for this row. Combined
    /// with `workflow_id` it identifies the exact summary so a shadowed global
    /// and its shadowing project workflow never resolve to each other.
    source_path: String,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let workflow_id_for_lookup = workflow_id.clone();
    let source_path_for_lookup = source_path.clone();
    let state_for_lookup = state.clone();
    let summary = Memo::new(move |_| {
        let host_id = active_host.get()?;
        state_for_lookup.workflow_summaries.with(|map| {
            map.get(&host_id).and_then(|summaries| {
                summaries
                    .iter()
                    .find(|summary| {
                        summary.id == workflow_id_for_lookup
                            && summary.source.path == source_path_for_lookup
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::ActiveProjectRef;
    use leptos::mount::mount_to;
    use protocol::{ProjectRootPath, StreamPath, WorkflowCoordinatorSpec, WorkflowSource};
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-workflows")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-workflows");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 420px; height: 600px; \
                 z-index: 2147483647; background: white; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

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

    /// Pull the `(workflow_id, project_id)` out of the first `trigger_workflow`
    /// frame the stub captured, as `"id|project"` (project empty when absent).
    /// Returns `""` when no trigger frame was sent.
    fn captured_trigger() -> String {
        js_sys::eval(
            r#"
            (function() {
                var calls = window.__test_send_calls || [];
                for (var i = 0; i < calls.length; i++) {
                    if (calls[i][0] !== 'send_host_line') continue;
                    var args = JSON.parse(calls[i][1]);
                    var env = JSON.parse(args.line);
                    if (env.kind === 'trigger_workflow') {
                        return (env.payload.workflow_id || '') + '|' + (env.payload.project_id || '');
                    }
                }
                return '';
            })()
            "#,
        )
        .expect("read captured trigger")
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

    fn coordinator() -> WorkflowCoordinatorSpec {
        WorkflowCoordinatorSpec {
            backend: BackendKind::Codex,
            access_mode: Default::default(),
        }
    }

    fn global_summary(id: &str, name: &str) -> WorkflowSummary {
        WorkflowSummary {
            id: WorkflowId(id.to_owned()),
            name: name.to_owned(),
            description: None,
            triggers: Vec::new(),
            inputs: Vec::new(),
            coordinator: coordinator(),
            declared_backends: Vec::new(),
            tags: Vec::new(),
            source: WorkflowSource {
                scope: WorkflowSourceScope::Global,
                path: format!("/global/.tyde/workflows/{id}.md"),
            },
        }
    }

    fn project_summary(id: &str, name: &str, project_id: &str, root: &str) -> WorkflowSummary {
        WorkflowSummary {
            id: WorkflowId(id.to_owned()),
            name: name.to_owned(),
            description: None,
            triggers: Vec::new(),
            inputs: Vec::new(),
            coordinator: coordinator(),
            declared_backends: Vec::new(),
            tags: Vec::new(),
            source: WorkflowSource {
                scope: WorkflowSourceScope::Project {
                    project_id: ProjectId(project_id.to_owned()),
                    root: ProjectRootPath(root.to_owned()),
                },
                path: format!("{root}/.tyde/workflows/{id}.md"),
            },
        }
    }

    fn buttons_with_text(container: &HtmlElement, label: &str) -> usize {
        let nodes = container.query_selector_all("button").unwrap();
        let mut count = 0;
        for i in 0..nodes.length() {
            let node = nodes.item(i).unwrap();
            if node.text_content().unwrap_or_default().trim() == label {
                count += 1;
            }
        }
        count
    }

    fn click_button_with_text(container: &HtmlElement, label: &str) {
        let nodes = container.query_selector_all("button").unwrap();
        for i in 0..nodes.length() {
            let node = nodes.item(i).unwrap();
            if node.text_content().unwrap_or_default().trim() == label {
                node.dyn_into::<HtmlElement>().unwrap().click();
                return;
            }
        }
        panic!("button with text {label:?} not found");
    }

    // ---- Pure projection logic (the shadowing rework) ----

    #[wasm_bindgen_test]
    fn project_workflow_shadows_same_id_global_in_its_project() {
        let summaries = vec![
            global_summary("deploy", "Deploy (global)"),
            project_summary("deploy", "Deploy (project)", "proj-a", "/repo"),
            global_summary("lint", "Lint"),
        ];
        let proj_a = ProjectId("proj-a".to_owned());

        // In project A: the project "deploy" shadows the global "deploy"; the
        // unrelated global "lint" still shows. Two rows, not three.
        let effective = effective_summaries(&summaries, Some(&proj_a));
        assert_eq!(effective.len(), 2, "shadowed global must be hidden");
        let deploy = effective
            .iter()
            .find(|s| s.id.0 == "deploy")
            .expect("deploy present");
        assert!(
            matches!(deploy.source.scope, WorkflowSourceScope::Project { .. }),
            "the effective deploy must be the project definition"
        );

        // In a different project B (no project deploy): the global deploy wins.
        let proj_b = ProjectId("proj-b".to_owned());
        let effective_b = effective_summaries(&summaries, Some(&proj_b));
        let deploy_b = effective_b
            .iter()
            .find(|s| s.id.0 == "deploy")
            .expect("deploy present for project B");
        assert!(
            matches!(deploy_b.source.scope, WorkflowSourceScope::Global),
            "project B sees the global deploy"
        );

        // Host/global context: only globals, deploy resolves to the global.
        let effective_global = effective_summaries(&summaries, None);
        assert_eq!(effective_global.len(), 2);
        assert!(
            effective_global
                .iter()
                .all(|s| matches!(s.source.scope, WorkflowSourceScope::Global))
        );
    }

    #[wasm_bindgen_test]
    fn authoring_prompt_lists_project_target_first_then_global() {
        let locations = vec![
            WorkflowCatalogLocation {
                scope: WorkflowSourceScope::Global,
                directory: "/Users/me/.tyde/workflows".to_owned(),
                exists: false,
            },
            WorkflowCatalogLocation {
                scope: WorkflowSourceScope::Project {
                    project_id: ProjectId("proj-a".to_owned()),
                    root: ProjectRootPath("/repo".to_owned()),
                },
                directory: "/repo/.tyde/workflows".to_owned(),
                exists: false,
            },
        ];
        let proj_a = ProjectId("proj-a".to_owned());
        let prompt = build_workflow_authoring_prompt(&locations, Some(&proj_a));

        let project_pos = prompt
            .find("/repo/.tyde/workflows")
            .expect("project dir present");
        let global_pos = prompt
            .find("/Users/me/.tyde/workflows")
            .expect("global dir present");
        assert!(
            project_pos < global_pos,
            "active project target must be listed before global"
        );
        assert!(prompt.contains("tyde_workflow_save"));
    }

    // ---- Rendered panel (user-perceived) ----

    /// (a) Empty catalog → teaching copy, real server-sent paths, and a CTA
    /// that opens a new chat pre-filling the composer (not auto-sent).
    #[wasm_bindgen_test]
    async fn empty_state_teaches_and_cta_prefills_editable_prompt() {
        ensure_styles_loaded();
        install_send_stub();

        let container = make_container();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let captured_for_mount = captured.clone();

        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.selected_host_id.set(Some("host-a".to_owned()));
            state.host_streams.update(|m| {
                m.insert("host-a".to_owned(), StreamPath("/host/host-a".to_owned()));
            });
            // No workflows for this host → empty catalog.
            state.workflow_summaries.update(|m| {
                m.insert("host-a".to_owned(), Vec::new());
            });
            state.workflow_locations.update(|m| {
                m.insert(
                    "host-a".to_owned(),
                    vec![WorkflowCatalogLocation {
                        scope: WorkflowSourceScope::Global,
                        directory: "/Users/me/.tyde/workflows".to_owned(),
                        exists: false,
                    }],
                );
            });
            *captured_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <WorkflowsPanel /> }
        });

        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        // Header relabel names the authoring model.
        assert!(
            text.contains("Runnable playbooks your agents write for this project"),
            "panel subtitle must name the authoring model; text was {text:?}"
        );
        // Teaching copy explains agents author the Markdown files.
        assert!(
            text.contains("runnable playbooks an agent writes"),
            "empty state must explain workflows are agent-authored playbooks; text was {text:?}"
        );
        assert!(
            text.contains(".tyde/workflows"),
            "empty state must mention the workflows directory; text was {text:?}"
        );
        // The real server-sent path appears (not a hardcoded convention).
        assert!(
            text.contains("/Users/me/.tyde/workflows"),
            "empty state must show the server-sent global directory; text was {text:?}"
        );

        // The CTA button is present.
        assert_eq!(
            buttons_with_text(&container, "Ask an agent to create a workflow"),
            1,
            "exactly one authoring CTA button must render"
        );

        // Click the CTA: it must pre-fill an editable composer prompt and must
        // NOT send anything (no agent spawned until the user sends).
        click_button_with_text(&container, "Ask an agent to create a workflow");
        next_tick().await;

        let state = captured.borrow().clone().expect("state captured");
        let prefill = state.chat_input.get_untracked();
        assert!(
            prefill.contains("Create a Tyde workflow"),
            "composer must be pre-filled with the authoring prompt; was {prefill:?}"
        );
        assert!(
            prefill.contains("/Users/me/.tyde/workflows"),
            "prefill must include the server-sent target path; was {prefill:?}"
        );
        assert!(
            prefill.contains("tyde_workflow_save"),
            "prefill must instruct the agent to save via the MCP tool; was {prefill:?}"
        );
        // Nothing was sent: the CTA opens a draft, it does not spawn an agent.
        assert_eq!(
            captured_trigger(),
            "",
            "CTA must not trigger/send any frame; the prompt is editable and unsent"
        );
    }

    /// (b) Same-id global + project summary in a project context: the panel
    /// shows the PROJECT definition (one row, identified by its source label),
    /// hides the global, and Run triggers the project-scoped definition.
    #[wasm_bindgen_test]
    async fn project_context_shows_and_runs_shadowing_project_workflow() {
        ensure_styles_loaded();
        install_send_stub();

        let container = make_container();

        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host-a".to_owned(),
                project_id: ProjectId("proj-a".to_owned()),
            }));
            state.host_streams.update(|m| {
                m.insert("host-a".to_owned(), StreamPath("/host/host-a".to_owned()));
            });
            state.workflow_summaries.update(|m| {
                m.insert(
                    "host-a".to_owned(),
                    vec![
                        global_summary("deploy", "Deploy GLOBAL"),
                        project_summary("deploy", "Deploy PROJECT", "proj-a", "/repo"),
                    ],
                );
            });
            provide_context(state);
            view! { <WorkflowsPanel /> }
        });

        next_tick().await;

        // Exactly one catalog card renders (one Run button), not two.
        assert_eq!(
            buttons_with_text(&container, "Run"),
            1,
            "shadowed global must not produce a second catalog row"
        );

        let text = container.text_content().unwrap_or_default();
        // The visible card is the PROJECT one (its name + project source label).
        assert!(
            text.contains("Deploy PROJECT"),
            "the project definition must be the one shown; text was {text:?}"
        );
        assert!(
            !text.contains("Deploy GLOBAL"),
            "the shadowed global definition must be hidden; text was {text:?}"
        );
        assert!(
            text.contains("Project · /repo"),
            "the shown card must carry the project source label; text was {text:?}"
        );

        // Run triggers the project-scoped definition: the frame carries the
        // workflow id plus the active project, so the server resolves the
        // project workflow (not the global).
        click_button_with_text(&container, "Run");
        next_tick().await;

        assert_eq!(
            captured_trigger(),
            "deploy|proj-a",
            "Run must trigger the deploy workflow scoped to the active project"
        );
    }
}
