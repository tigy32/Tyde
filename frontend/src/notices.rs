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
    Workflows(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Error,
    Warning,
}

#[derive(Clone, PartialEq, Eq)]
struct Notice {
    id: u64,
    scope: NoticeScope,
    operation: Option<FrameKind>,
    severity: NoticeSeverity,
    message: String,
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
            notices.retain(|notice| notice.scope != scope || notice.operation != operation);
            notices.push(Notice {
                id,
                scope,
                operation,
                severity,
                message,
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
                &notice.scope != scope
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
        FrameKind::HostBrowseStart
        | FrameKind::HostBrowseList
        | FrameKind::HostBrowseClose
        | FrameKind::ProjectCreate
        | FrameKind::ProjectAddRoot => Some(NoticeScope::Browser),
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
    let inline_workbench = match &error.context {
        Some(protocol::CommandErrorContext::WorkbenchCreate {
            parent_project_id,
            branch,
        }) => state.pending_workbench_creates.with_untracked(|pending| {
            pending.iter().any(|entry| {
                entry.host_id == host
                    && !entry.is_stale(crate::state::now_ms())
                    && &entry.parent_project_id == parent_project_id
                    && &entry.branch == branch
            })
        }),
        _ => false,
    };
    if inline_diff
        || inline_workbench
        || matches!(
            error.request_kind,
            FrameKind::WorkbenchRemove
                | FrameKind::WorkflowRefresh
                | FrameKind::TriggerWorkflow
                | FrameKind::CancelWorkflow
        )
    {
        log::debug!(
            "command failure already owned by inline handler: {}",
            error.operation
        );
        return;
    }
    let scope = match &error.context {
        Some(protocol::CommandErrorContext::WorkbenchCreate {
            parent_project_id, ..
        }) => Some(NoticeScope::Project(
            host.to_owned(),
            parent_project_id.clone(),
        )),
        _ => request_scope(host, &error.stream, error.request_kind),
    };
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
pub fn InlineNotices(#[prop(into)] scopes: Signal<Vec<NoticeScope>>) -> impl IntoView {
    let notices = NOTICES.with(Clone::clone);
    let visible = Memo::new(move |_| {
        let scopes = scopes.get();
        notices.with(|notices| {
            notices
                .iter()
                .filter(|notice| scopes.contains(&notice.scope))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    view! {
        <For each=move || visible.get() key=|notice| notice.id let:notice>
            <div class=if notice.severity == NoticeSeverity::Warning { "inline-notice warning" } else { "inline-notice error" }
                role=if notice.severity == NoticeSeverity::Warning { "status" } else { "alert" }>
                <span>{notice.message}</span>
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
