use std::cell::Cell;

use leptos::prelude::*;
use protocol::{AgentId, CommandErrorPayload, FrameKind, ProjectId, ReviewId, StreamPath};

use crate::state::{ActiveAgentRef, AppState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeScope {
    Settings(Option<String>),
    Sessions(String),
    Project(String, ProjectId),
    Agent(String, AgentId),
    Review(String, ReviewId),
    Terminal(String, StreamPath),
    Terminals,
    Browser,
    ProjectOperations(String),
    Workflows(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Error,
    Warning,
}

#[derive(Clone)]
struct Notice {
    id: u64,
    scope: NoticeScope,
    operation: Option<FrameKind>,
    severity: NoticeSeverity,
    message: String,
    retry: Option<protocol::ProjectMutation>,
}

thread_local! {
    static NOTICES: ArcRwSignal<Vec<Notice>> = ArcRwSignal::new(Vec::new());
    static NEXT_NOTICE: Cell<u64> = const { Cell::new(0) };
}

pub fn report_error(scope: NoticeScope, message: impl Into<String>) {
    report(scope, None, NoticeSeverity::Error, message.into());
}

pub fn report_settings_error(state: &AppState, message: impl Into<String>) {
    report_error(
        NoticeScope::Settings(state.selected_host_id.get_untracked()),
        message,
    );
}

pub fn report_agent_error(agent: &ActiveAgentRef, message: impl Into<String>) {
    report_error(
        NoticeScope::Agent(agent.host_id.clone(), agent.agent_id.clone()),
        message,
    );
}

fn report(
    scope: NoticeScope,
    operation: Option<FrameKind>,
    severity: NoticeSeverity,
    message: String,
) {
    report_with_retry(scope, operation, severity, message, None);
}

pub fn report_project_send_failure(
    state: &AppState,
    host: String,
    mutation: protocol::ProjectMutation,
    error: &str,
) {
    let label = mutation_label(state, &host, &mutation);
    report_with_retry(
        NoticeScope::ProjectOperations(host),
        None,
        NoticeSeverity::Error,
        format!("{label} could not be sent: {error}"),
        Some(mutation),
    );
}

fn report_with_retry(
    scope: NoticeScope,
    operation: Option<FrameKind>,
    severity: NoticeSeverity,
    message: String,
    retry: Option<protocol::ProjectMutation>,
) {
    log::debug!(
        "error presentation scope={scope:?} operation={operation:?} warning={}: {message}",
        severity == NoticeSeverity::Warning
    );
    let id = NEXT_NOTICE.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1));
        id
    });
    NOTICES.with(|notices| {
        notices.update(|notices| {
            notices.retain(|notice| {
                notice.scope != scope
                    || notice.operation != operation
                    || notice.retry.is_some()
                    || retry.is_some()
            });
            notices.push(Notice {
                id,
                scope,
                operation,
                severity,
                message,
                retry,
            });
        })
    });
}

pub fn has_notice(scope: &NoticeScope) -> bool {
    NOTICES
        .with(|notices| notices.with(|notices| notices.iter().any(|notice| &notice.scope == scope)))
}

pub fn clear_scope(scope: &NoticeScope) {
    NOTICES
        .with(|notices| notices.update(|notices| notices.retain(|notice| &notice.scope != scope)));
}

pub fn clear_request(scope: &NoticeScope, operation: FrameKind) {
    NOTICES.with(|notices| {
        notices.update(|notices| {
            notices.retain(|notice| {
                matches!(notice.scope, NoticeScope::ProjectOperations(_))
                    || &notice.scope != scope
                    || (notice.operation.is_some() && notice.operation != Some(operation))
            })
        })
    });
}

pub fn clear_host(host: &str) {
    NOTICES.with(|notices| {
        notices.update(|notices| {
            notices.retain(|notice| match &notice.scope {
                NoticeScope::Settings(Some(owner))
                | NoticeScope::Sessions(owner)
                | NoticeScope::Project(owner, _)
                | NoticeScope::Agent(owner, _)
                | NoticeScope::Review(owner, _)
                | NoticeScope::Terminal(owner, _)
                | NoticeScope::Workflows(owner) => owner != host,
                _ => true,
            })
        })
    });
}

pub fn request_scope(host: &str, stream: &StreamPath, kind: FrameKind) -> Option<NoticeScope> {
    if let Some(id) = stream.0.strip_prefix("/project/") {
        return Some(NoticeScope::Project(
            host.to_owned(),
            ProjectId(id.to_owned()),
        ));
    }
    if let Some(id) = stream
        .0
        .strip_prefix("/agent/")
        .and_then(|tail| tail.split('/').next())
    {
        return Some(NoticeScope::Agent(host.to_owned(), AgentId(id.to_owned())));
    }
    if let Some(id) = stream.0.strip_prefix("/review/") {
        return Some(NoticeScope::Review(
            host.to_owned(),
            ReviewId(id.to_owned()),
        ));
    }
    if stream.0.starts_with("/terminal/") {
        return Some(NoticeScope::Terminal(host.to_owned(), stream.clone()));
    }
    if stream.0.starts_with("/browse/") {
        return Some(NoticeScope::Browser);
    }
    match kind {
        FrameKind::SettingsWrite
        | FrameKind::BackendNativeSettingsWrite
        | FrameKind::InvokeSettingsAction
        | FrameKind::BackendSetupRefresh
        | FrameKind::RunBackendSetup
        | FrameKind::CustomAgentUpsert
        | FrameKind::CustomAgentDelete
        | FrameKind::SteeringUpsert
        | FrameKind::SteeringDelete
        | FrameKind::SkillRefresh
        | FrameKind::BackendSettingsRefresh
        | FrameKind::BackendCapacityRefresh
        | FrameKind::McpServerUpsert
        | FrameKind::McpServerDelete
        | FrameKind::MobilePairingStart
        | FrameKind::MobilePairingCancel
        | FrameKind::MobileDeviceRevoke
        | FrameKind::MobileDeviceRename => Some(NoticeScope::Settings(Some(host.to_owned()))),
        FrameKind::ListSessions | FrameKind::DeleteSession => {
            Some(NoticeScope::Sessions(host.to_owned()))
        }
        FrameKind::HostBrowseStart | FrameKind::HostBrowseList | FrameKind::HostBrowseClose => {
            Some(NoticeScope::Browser)
        }
        FrameKind::ProjectCreate
        | FrameKind::ProjectAddRoot
        | FrameKind::ProjectDeleteRoot
        | FrameKind::ProjectDelete
        | FrameKind::WorkbenchCreate
        | FrameKind::WorkbenchRemove
        | FrameKind::ProjectRename
        | FrameKind::ProjectReorder => Some(NoticeScope::ProjectOperations(host.to_owned())),
        FrameKind::TerminalCreate => Some(NoticeScope::Terminals),
        FrameKind::WorkflowRefresh | FrameKind::TriggerWorkflow | FrameKind::CancelWorkflow => {
            Some(NoticeScope::Workflows(host.to_owned()))
        }
        _ => None,
    }
}

pub fn report_command_error(state: &AppState, host: &str, error: &CommandErrorPayload) {
    let inline_diff = error.request_id.as_ref().is_some_and(|id| {
        state
            .diff_request_ids
            .with_untracked(|requests| requests.values().any(|pending| pending == id))
            || state
                .diff_expand_request_ids
                .with_untracked(|requests| requests.values().any(|pending| pending == id))
    });
    if inline_diff
        || matches!(
            error.request_kind,
            FrameKind::WorkflowRefresh | FrameKind::TriggerWorkflow | FrameKind::CancelWorkflow
        )
    {
        log::debug!(
            "command failure already owned by inline handler: {}",
            error.operation
        );
        return;
    }
    if matches!(
        error.context,
        Some(protocol::CommandErrorContext::ProjectMutationCommitted { .. })
    ) {
        return;
    }
    // The terminal operation and its compatibility command error carry the same
    // diagnostic. Deduplicate presentation only; never settle state from an error.
    let duplicate = matches!(error.request_kind, FrameKind::ProjectCreate | FrameKind::ProjectDelete | FrameKind::ProjectAddRoot | FrameKind::ProjectDeleteRoot | FrameKind::WorkbenchCreate | FrameKind::WorkbenchRemove | FrameKind::ProjectRename | FrameKind::ProjectReorder) && state.project_operations.with_untracked(|operations| operations.iter().any(|((owner, _), operation)| {
        owner == host && matches!(&operation.state, protocol::ProjectOperationState::Failed { message, .. } if message == &error.message)
    }));
    if duplicate {
        return;
    }
    let scope = request_scope(host, &error.stream, error.request_kind);
    if let Some(scope) = scope {
        // ProjectFileList is a server-push kind, not a failed user command. A
        // non-fatal error on that kind reports degraded subscription health.
        let severity = if error.request_kind == FrameKind::ProjectFileList && !error.fatal {
            NoticeSeverity::Warning
        } else {
            NoticeSeverity::Error
        };
        report(
            scope,
            Some(error.request_kind),
            severity,
            error.message.clone(),
        );
    } else {
        crate::components::header::report_user_error(format!(
            "{} failed on host “{host}”: {}",
            error.operation, error.message
        ));
    }
}

pub fn report_send_failure(host: &str, stream: &StreamPath, kind: FrameKind, error: &str) {
    if is_background_send(kind) {
        log::debug!("background send failed host={host} kind={kind}: {error}");
        return;
    }
    let message = format!(
        "Tyde could not send “{}” to host “{host}”. {error}",
        kind.to_string().replace('_', " ")
    );
    if let Some(scope) = request_scope(host, stream, kind) {
        report(scope, Some(kind), NoticeSeverity::Error, message);
    } else {
        crate::components::header::report_user_error(message);
    }
}

pub fn is_background_send(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::TerminalResize
            | FrameKind::HostBrowseClose
            | FrameKind::CodeIntelUnsubscribeFile
            | FrameKind::CodeIntelSetVisibleRange
            | FrameKind::CodeIntelCancelReferences
            | FrameKind::ProjectSearchCancel
            | FrameKind::ProjectAccessed
    )
}

#[component]
pub fn InlineNotices(
    #[prop(into)] scopes: Signal<Vec<NoticeScope>>,
    #[prop(optional)] include_operation: Option<FrameKind>,
) -> impl IntoView {
    let notices = NOTICES.with(Clone::clone);
    let state = expect_context::<AppState>();
    let visible = Signal::derive(move || {
        let scopes = scopes.get();
        notices.with(|notices| {
            notices
                .iter()
                .filter(|notice| {
                    scopes.contains(&notice.scope)
                        || include_operation
                            .is_some_and(|operation| notice.operation == Some(operation))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    view! {
        <For each=move || visible.get() key=|notice| notice.id let:notice>
            <div class=if notice.severity == NoticeSeverity::Warning { "inline-notice warning" } else { "inline-notice error" }
                role=if notice.severity == NoticeSeverity::Warning { "status" } else { "alert" }>
                <span>{notice.message}</span>
                {notice.retry.clone().map(|mutation| {
                    let state = state.clone();
                    let scope = notice.scope.clone();
                    view! { <button type="button" on:click=move |_| {
                        let NoticeScope::ProjectOperations(host) = &scope else { return; };
                        NOTICES.with(|notices| notices.update(|notices| notices.retain(|current| current.id != notice.id)));
                        crate::actions::submit_project_mutation(&state, host.clone(), mutation.clone());
                    }>"Retry"</button> }
                })}
                <button type="button" aria-label="Dismiss notice" on:click=move |_| {
                    NOTICES.with(|notices| notices.update(|notices| notices.retain(|current| current.id != notice.id)));
                }>"×"</button>
            </div>
        </For>
    }
}

#[component]
pub fn ActionError(error: ArcRwSignal<Option<String>>) -> impl IntoView {
    let shown = error.clone();
    let message = error.clone();
    view! {
        <Show when=move || shown.get().is_some()>
            <div class="inline-notice error" role="alert">
                <span>{let message = message.clone(); move || message.get().unwrap_or_default()}</span>
                <button type="button" aria-label="Dismiss notice" on:click={let error = error.clone(); move |_| error.set(None)}>"×"</button>
            </div>
        </Show>
    }
}

pub fn project_mutation_pending(state: &AppState, host: &str, project: &ProjectId) -> bool {
    let family = |id: &ProjectId| {
        state.projects.with(|projects| {
            projects
                .iter()
                .find(|entry| entry.host_id == host && &entry.project.id == id)
                .and_then(|entry| entry.project.parent_project_id().cloned())
                .unwrap_or_else(|| id.clone())
        })
    };
    let target = family(project);
    state.project_operations.with(|operations| {
        operations.iter().any(|((owner, _), operation)| {
            owner == host
                && matches!(
                    operation.state,
                    protocol::ProjectOperationState::Admitted
                        | protocol::ProjectOperationState::Running { .. }
                )
                && mutation_project(&operation.mutation).is_some_and(|id| family(id) == target)
        })
    })
}

fn mutation_project(mutation: &protocol::ProjectMutation) -> Option<&ProjectId> {
    use protocol::ProjectMutation;
    match mutation {
        ProjectMutation::Create(_) | ProjectMutation::Reorder(_) => None,
        ProjectMutation::Delete(payload) => Some(&payload.id),
        ProjectMutation::AddRoot(payload) => Some(&payload.id),
        ProjectMutation::DeleteRoot(payload) => Some(&payload.id),
        ProjectMutation::CreateWorkbench(payload) => Some(&payload.parent_project_id),
        ProjectMutation::RemoveWorkbench(payload) => Some(&payload.id),
        ProjectMutation::Rename(payload) => Some(&payload.id),
    }
}

pub(crate) fn project_label(state: &AppState, host: &str, id: &ProjectId) -> String {
    state.projects.with(|projects| {
        match projects
            .iter()
            .find(|entry| entry.host_id == host && &entry.project.id == id)
        {
            Some(entry) => entry.project.name.clone(),
            None => format!("project {id} (not in catalog)"),
        }
    })
}

pub(crate) fn mutation_label(
    state: &AppState,
    host: &str,
    mutation: &protocol::ProjectMutation,
) -> String {
    use protocol::ProjectMutation;
    match mutation {
        ProjectMutation::Create(payload) => format!("Create project {}", payload.name),
        ProjectMutation::Delete(payload) => {
            format!("Delete {}", project_label(state, host, &payload.id))
        }
        ProjectMutation::AddRoot(payload) => format!(
            "Add {} to {}",
            payload.root,
            project_label(state, host, &payload.id)
        ),
        ProjectMutation::DeleteRoot(payload) => format!(
            "Remove {} from {}",
            payload.root,
            project_label(state, host, &payload.id)
        ),
        ProjectMutation::CreateWorkbench(payload) => format!(
            "Create workbench {} in {}",
            payload.branch,
            project_label(state, host, &payload.parent_project_id)
        ),
        ProjectMutation::RemoveWorkbench(payload) => format!(
            "Remove workbench {}",
            project_label(state, host, &payload.id)
        ),
        ProjectMutation::Rename(payload) => format!("Rename project to {}", payload.name),
        ProjectMutation::Reorder(_) => "Reorder projects".to_owned(),
    }
}

#[component]
pub fn ProjectOperationNotices() -> impl IntoView {
    use protocol::{ProjectOperationStage, ProjectOperationState};
    let state = expect_context::<AppState>();
    let operations = state.project_operations;
    let hosts = state.configured_hosts;
    let remove_prompt = state.workbench_remove_prompt;
    let browser = state.browse_dialog;
    let scopes = move || {
        let mut scopes = Vec::new();
        if browser.with(|dialog| dialog.is_none()) {
            scopes.push(NoticeScope::Browser);
        }
        scopes.extend(
            hosts
                .get()
                .into_iter()
                .map(|host| NoticeScope::ProjectOperations(host.id)),
        );
        scopes
    };
    view! {
        <div class="project-operation-notices">
            <InlineNotices scopes=Signal::derive(scopes) />
            <For each=move || {
                let mut keys = operations.with(|operations| operations.iter().filter(|(_, operation)| !matches!(&operation.state, ProjectOperationState::Committed { warnings } if warnings.is_empty())).map(|(key, _)| key.clone()).collect::<Vec<_>>());
                keys.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
                keys
            } key=|key| key.clone() let:key>
                {
                    let state = state.clone();
                    // Operation identity is server-owned; a keyed row reads the current payload.
                    let current = Signal::derive({ let key = key.clone(); move || operations.with(|operations| operations.get(&key).cloned()) });
                    let label = { let host = key.0.clone(); let state = state.clone(); move || current.get().map(|operation| {
                        let host_label = hosts.with(|hosts| hosts.iter().find(|entry| entry.id == host).map(|entry| entry.label.clone()));
                        format!("{} · {}", host_label.unwrap_or_else(|| host.clone()), mutation_label(&state, &host, &operation.mutation))
                    }) };
                    let detail = move || current.get().map(|operation| match operation.state {
                        ProjectOperationState::Admitted => "Queued".to_owned(),
                        ProjectOperationState::Running { stage } => match stage {
                            ProjectOperationStage::Queue => "Queued",
                            ProjectOperationStage::Git => "Working in Git…",
                            ProjectOperationStage::Filesystem => "Updating files…",
                            ProjectOperationStage::Persistence => "Saving…",
                            ProjectOperationStage::Cleanup => "Cleaning up…",
                            ProjectOperationStage::Fanout => "Updating project views…",
                            ProjectOperationStage::Workflow => "Updating workflows…",
                        }.to_owned(),
                        ProjectOperationState::Committed { warnings } => format!("Saved, with warnings: {}. Do not repeat this operation.", warnings.iter().map(|warning| warning.message.as_str()).collect::<Vec<_>>().join("; ")),
                        ProjectOperationState::Failed { message, cleanup_may_be_partial, .. } => if cleanup_may_be_partial { format!("{message} Cleanup may be incomplete; inspect the workbench before trying again.") } else { message },
                    });
                    let settled = move || current.get().is_some_and(|operation| matches!(operation.state, ProjectOperationState::Committed { .. } | ProjectOperationState::Failed { .. }));
                    let state_for_retry = state.clone();
                    let host_for_retry = key.0.clone();
                    let key_for_retry = key.clone();
                    let retry = move |_| {
                        let Some(operation) = current.get_untracked() else { return; };
                        let ProjectOperationState::Failed { message, cleanup_may_be_partial, .. } = operation.state else { return; };
                        if let protocol::ProjectMutation::RemoveWorkbench(payload) = &operation.mutation {
                            state_for_retry.workbench_remove_prompt.set(Some(crate::state::WorkbenchRemovePrompt {
                                host_id: host_for_retry.clone(), project_id: payload.id.clone(),
                                operation_stream: operation.stream,
                                project_name: project_label(&state_for_retry, &host_for_retry, &payload.id),
                                message: if cleanup_may_be_partial { format!("{message} Cleanup may be incomplete; inspect the workbench before trying again.") } else { message },
                            }));
                        } else {
                            crate::actions::submit_project_mutation(&state_for_retry, host_for_retry.clone(), operation.mutation);
                            operations.update(|operations| { operations.remove(&key_for_retry); });
                        }
                    };
                    view! {
                        <div class=move || if current.get().is_some_and(|operation| matches!(operation.state, ProjectOperationState::Failed { .. })) { "inline-notice error" } else if current.get().is_some_and(|operation| matches!(operation.state, ProjectOperationState::Committed { .. })) { "inline-notice warning" } else { "inline-notice" } role="status">
                            <span><strong>{label}</strong>{" — "}{detail}</span>
                            <Show when=move || current.get().is_some_and(|operation| matches!(operation.state, ProjectOperationState::Failed { cleanup_may_be_partial: false, .. }) || (matches!(operation.state, ProjectOperationState::Failed { .. }) && matches!(operation.mutation, protocol::ProjectMutation::RemoveWorkbench(_))))>
                                <button type="button" on:click=retry.clone()>{move || if current.get().is_some_and(|operation| matches!(operation.mutation, protocol::ProjectMutation::RemoveWorkbench(_))) { "Review removal…" } else { "Retry" }}</button>
                            </Show>
                            <Show when=settled>
                                <button type="button" aria-label="Dismiss project operation" on:click={let key = key.clone(); move |_| operations.update(|operations| { operations.remove(&key); })}>"×"</button>
                            </Show>
                        </div>
                    }
                }
            </For>
            {move || remove_prompt.get().map(|prompt| view! { <crate::components::project_rail::WorkbenchRemoveModal prompt /> })}
        </div>
    }
}
