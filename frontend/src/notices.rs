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
    #[prop(optional)] project_operation_target: Option<(String, Option<ProjectId>)>,
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
                        || project_operation_target
                            .as_ref()
                            .is_some_and(|(host, project)| {
                                notice.scope == NoticeScope::ProjectOperations(host.clone())
                                    && notice.retry.as_ref().is_none_or(|mutation| {
                                        mutation_project(mutation) == project.as_ref()
                                    })
                            })
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
pub fn ProjectDialogs() -> impl IntoView {
    let state = expect_context::<AppState>();
    let remove_prompt = state.workbench_remove_prompt;
    let browser = state.browse_dialog;
    let scopes = move || {
        if browser.with(|dialog| dialog.is_none()) {
            vec![NoticeScope::Browser]
        } else {
            Vec::new()
        }
    };
    view! {
        <div class="browser-notices">
            <InlineNotices scopes=Signal::derive(scopes) />
        </div>
        {move || remove_prompt.get().map(|prompt| view! { <crate::components::project_rail::WorkbenchRemoveModal prompt /> })}
    }
}

#[component]
pub fn ProjectOperationDetails(
    host: String,
    #[prop(optional)] project: Option<ProjectId>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let target = (host.clone(), project.clone());
    let notices = Signal::derive(move || {
        project
            .as_ref()
            .map(|project| NoticeScope::Project(host.clone(), project.clone()))
            .into_iter()
            .collect::<Vec<_>>()
    });
    let operation_target = target.clone();
    let details = move || {
        let mut operations = state.project_operations.with(|operations| {
            operations
                .iter()
                .filter(|((host, _), operation)| {
                    host == &operation_target.0
                        && mutation_project(&operation.mutation) == operation_target.1.as_ref()
                })
                .map(|((_, stream), operation)| (stream.clone(), operation.clone()))
                .collect::<Vec<_>>()
        });
        operations.sort_by(|left, right| left.0.0.cmp(&right.0.0));
        operations.into_iter().filter_map(|(_, operation)| {
            let (role, message) = match operation.state {
                protocol::ProjectOperationState::Failed { message, cleanup_may_be_partial, .. } => (
                    "alert",
                    if cleanup_may_be_partial {
                        format!("{message} Cleanup may be incomplete; inspect the workbench before trying again.")
                    } else { message },
                ),
                protocol::ProjectOperationState::Committed { warnings } if !warnings.is_empty() => (
                    "status", warnings.into_iter().map(|warning| warning.message).collect::<Vec<_>>().join("; "),
                ),
                _ => return None,
            };
            let label = mutation_label(&state, &operation_target.0, &operation.mutation);
            Some(view! { <p role=role>{format!("{label}: {message}")}</p> })
        }).collect_view()
    };
    view! {
        <div class="project-operation-details">
            <InlineNotices scopes=notices project_operation_target=target />
            {details}
        </div>
    }
}
