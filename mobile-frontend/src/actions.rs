use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use protocol::types::AgentCompactPayload;

use crate::send::send_frame;
use crate::state::{
    AgentRef, AppState, HostBrowseSession, LocalHostId, ProjectDiffRef, ProjectDiffState,
};

pub async fn spawn_new_chat(
    state: &AppState,
    message: String,
    images: Vec<protocol::ImageData>,
) -> Result<(), String> {
    let host = state
        .active_local_host_id
        .get_untracked()
        .ok_or("no active host")?;
    let host_stream = state.host_stream_untracked(&host).ok_or("no host stream")?;

    let host_settings = state.active_host_settings_untracked();

    let backend_kind = state
        .draft_backend_override
        .get_untracked()
        .or_else(|| host_settings.as_ref().and_then(|s| s.default_backend))
        .or_else(|| {
            host_settings
                .as_ref()
                .and_then(|s| s.enabled_backends.first().copied())
        })
        .ok_or("no backend available")?;

    let custom_agent_id = state.draft_custom_agent_id.get_untracked();
    let session_settings = state.draft_session_settings.get_untracked();

    let active_project = state.active_project.get_untracked();
    let workspace_roots: Vec<String> = if let Some(ref active) = active_project {
        state
            .projects
            .with_untracked(|projects| {
                projects
                    .iter()
                    .find(|p| {
                        p.local_host_id == active.local_host_id && p.project.id == active.project_id
                    })
                    .map(|p| p.project.roots.clone())
            })
            .ok_or("active project not found")?
    } else {
        Vec::new()
    };

    let project_id = active_project.map(|ap| ap.project_id);

    let prompt = if message.is_empty() {
        String::new()
    } else {
        message
    };

    let payload = protocol::SpawnAgentPayload {
        name: None,
        custom_agent_id,
        parent_agent_id: None,
        project_id,
        params: protocol::SpawnAgentParams::New {
            workspace_roots,
            prompt,
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            backend_kind,
            cost_hint: None,
            access_mode: Default::default(),
            session_settings: Some(session_settings),
        },
    };

    send_frame(
        &host,
        host_stream,
        protocol::FrameKind::SpawnAgent,
        &payload,
    )
    .await?;

    state.draft_backend_override.set(None);
    state.draft_custom_agent_id.set(None);
    state
        .draft_session_settings
        .set(protocol::SessionSettingsValues::default());

    Ok(())
}

/// Locate the agent's `instance_stream` so an outbound frame can be sent
/// to that stream. Returns `None` if the agent is gone (e.g. closed
/// out from under us by the host).
fn agent_instance_stream(state: &AppState, agent_ref: &AgentRef) -> Option<protocol::StreamPath> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| {
                a.local_host_id == agent_ref.local_host_id && a.agent_id == agent_ref.agent_id
            })
            .map(|a| a.instance_stream.clone())
    })
}

fn host_stream(state: &AppState, host: &LocalHostId) -> Result<protocol::StreamPath, String> {
    state
        .host_stream_untracked(host)
        .ok_or("no host stream".to_owned())
}

pub fn project_stream(project_id: &protocol::ProjectId) -> protocol::StreamPath {
    protocol::StreamPath(format!("/project/{}", project_id.0))
}

pub fn review_stream(review_id: &protocol::ReviewId) -> protocol::StreamPath {
    protocol::StreamPath(format!("/review/{}", review_id.0))
}

fn new_browse_stream() -> protocol::StreamPath {
    protocol::StreamPath(format!(
        "/browse/mobile-{}",
        js_sys::Math::random().to_string().replace("0.", "")
    ))
}

/// Rename an agent. The server echoes back an `AgentRenamed` event
/// which the dispatcher consumes — the UI doesn't need to optimistically
/// update local state.
pub async fn rename_agent(
    state: &AppState,
    agent_ref: &AgentRef,
    new_name: String,
) -> Result<(), String> {
    let stream = agent_instance_stream(state, agent_ref).ok_or("agent not found")?;
    let payload = protocol::SetAgentNamePayload { name: new_name };
    send_frame(
        &agent_ref.local_host_id,
        stream,
        protocol::FrameKind::SetAgentName,
        &payload,
    )
    .await
}

/// Close (but don't delete) an agent. Server replies with `AgentClosed`,
/// which the dispatcher consumes and clears UI state.
pub async fn close_agent(state: &AppState, agent_ref: &AgentRef) -> Result<(), String> {
    let stream = agent_instance_stream(state, agent_ref).ok_or("agent not found")?;
    send_frame(
        &agent_ref.local_host_id,
        stream,
        protocol::FrameKind::CloseAgent,
        &protocol::CloseAgentPayload {},
    )
    .await
}

/// Cancel a queued (not-yet-sent) message. Server replies with
/// `QueuedMessages` which the dispatcher consumes to update the queue.
pub async fn cancel_queued_message(
    state: &AppState,
    agent_ref: &AgentRef,
    id: protocol::QueuedMessageId,
) -> Result<(), String> {
    let stream = agent_instance_stream(state, agent_ref).ok_or("agent not found")?;
    send_frame(
        &agent_ref.local_host_id,
        stream,
        protocol::FrameKind::CancelQueuedMessage,
        &protocol::CancelQueuedMessagePayload { id },
    )
    .await
}

pub async fn send_queued_message_now(
    state: &AppState,
    agent_ref: &AgentRef,
    id: protocol::QueuedMessageId,
) -> Result<(), String> {
    let stream = agent_instance_stream(state, agent_ref).ok_or("agent not found")?;
    send_frame(
        &agent_ref.local_host_id,
        stream,
        protocol::FrameKind::SendQueuedMessageNow,
        &protocol::SendQueuedMessageNowPayload { id },
    )
    .await
}

pub async fn interrupt_agent(state: &AppState, agent_ref: &AgentRef) -> Result<(), String> {
    let stream = agent_instance_stream(state, agent_ref).ok_or("agent not found")?;
    send_frame(
        &agent_ref.local_host_id,
        stream,
        protocol::FrameKind::Interrupt,
        &protocol::InterruptPayload {},
    )
    .await
}

pub async fn compact_agent(
    state: &AppState,
    agent_ref: &AgentRef,
    summary_prompt: Option<String>,
    max_summary_bytes: Option<u32>,
) -> Result<(), String> {
    let stream = agent_instance_stream(state, agent_ref).ok_or("agent not found")?;
    send_frame(
        &agent_ref.local_host_id,
        stream,
        protocol::FrameKind::AgentCompact,
        &AgentCompactPayload {
            summary_prompt,
            max_summary_bytes,
        },
    )
    .await
}

pub async fn request_project_file(
    project: &crate::state::ActiveProjectRef,
    path: protocol::ProjectPath,
) -> Result<(), String> {
    send_frame(
        &project.local_host_id,
        project_stream(&project.project_id),
        protocol::FrameKind::ProjectReadFile,
        &protocol::ProjectReadFilePayload { path },
    )
    .await
}

pub async fn request_project_diff(
    state: &AppState,
    project: &crate::state::ActiveProjectRef,
    root: protocol::ProjectRootPath,
    scope: protocol::ProjectDiffScope,
    path: Option<String>,
    context_mode: protocol::DiffContextMode,
) -> Result<(), String> {
    let key = ProjectDiffRef {
        local_host_id: project.local_host_id.clone(),
        project_id: project.project_id.clone(),
        root: root.clone(),
        scope,
        path: path.clone(),
    };
    let previous = state
        .project_diffs
        .with_untracked(|diffs| diffs.get(&key).cloned());
    state.project_diffs.update(|diffs| {
        diffs.insert(
            key,
            ProjectDiffState::for_request(
                previous.as_ref(),
                root.clone(),
                scope,
                path.clone(),
                context_mode,
            ),
        );
    });

    send_frame(
        &project.local_host_id,
        project_stream(&project.project_id),
        protocol::FrameKind::ProjectReadDiff,
        &protocol::ProjectReadDiffPayload {
            root,
            scope,
            path,
            context_mode,
        },
    )
    .await
}

pub async fn create_review(
    project: &crate::state::ActiveProjectRef,
    origin_agent_id: protocol::AgentId,
    selection: protocol::ReviewDiffSelection,
) -> Result<(), String> {
    send_frame(
        &project.local_host_id,
        project_stream(&project.project_id),
        protocol::FrameKind::ReviewCreate,
        &protocol::ReviewCreatePayload {
            origin_agent_id,
            selection,
        },
    )
    .await
}

pub async fn subscribe_review(
    state: &AppState,
    host: &LocalHostId,
    review_id: protocol::ReviewId,
) -> Result<(), String> {
    let stream = review_stream(&review_id);
    state.review_streams.update(|streams| {
        streams.insert(
            crate::state::ReviewRef {
                local_host_id: host.clone(),
                review_id,
            },
            stream.clone(),
        );
    });
    send_frame(
        host,
        stream,
        protocol::FrameKind::ReviewSubscribe,
        &protocol::ReviewSubscribePayload::default(),
    )
    .await
}

pub async fn send_review_action(
    state: &AppState,
    host: &LocalHostId,
    review_id: protocol::ReviewId,
    action: protocol::ReviewActionPayload,
) -> Result<(), String> {
    let stream = state
        .review_streams
        .with_untracked(|streams| {
            streams
                .get(&crate::state::ReviewRef {
                    local_host_id: host.clone(),
                    review_id: review_id.clone(),
                })
                .cloned()
        })
        .unwrap_or_else(|| review_stream(&review_id));
    send_frame(host, stream, protocol::FrameKind::ReviewAction, &action).await
}

pub async fn create_team(
    state: &AppState,
    host: &LocalHostId,
    name: String,
    manager: protocol::TeamMemberCreateSpec,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamCreate,
        &protocol::TeamCreatePayload { name, manager },
    )
    .await
}

pub async fn rename_team(
    state: &AppState,
    host: &LocalHostId,
    id: protocol::TeamId,
    name: String,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamRename,
        &protocol::TeamRenamePayload { id, name },
    )
    .await
}

pub async fn delete_team(
    state: &AppState,
    host: &LocalHostId,
    id: protocol::TeamId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDelete,
        &protocol::TeamDeletePayload { id },
    )
    .await
}

pub async fn set_team_manager(
    state: &AppState,
    host: &LocalHostId,
    team_id: protocol::TeamId,
    new_manager_member_id: protocol::TeamMemberId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamSetManager,
        &protocol::TeamSetManagerPayload {
            team_id,
            new_manager_member_id,
        },
    )
    .await
}

pub async fn create_team_member(
    state: &AppState,
    host: &LocalHostId,
    team_id: protocol::TeamId,
    member: protocol::TeamMemberCreateSpec,
    session_id: Option<protocol::SessionId>,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamMemberCreate,
        &protocol::TeamMemberCreatePayload {
            team_id,
            member,
            session_id,
        },
    )
    .await
}

pub async fn update_team_member(
    state: &AppState,
    host: &LocalHostId,
    payload: protocol::TeamMemberUpdatePayload,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamMemberUpdate,
        &payload,
    )
    .await
}

pub async fn delete_team_member(
    state: &AppState,
    host: &LocalHostId,
    id: protocol::TeamMemberId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamMemberDelete,
        &protocol::TeamMemberDeletePayload { id },
    )
    .await
}

pub async fn activate_team_member(
    state: &AppState,
    host: &LocalHostId,
    member_id: protocol::TeamMemberId,
    prompt: Option<String>,
    images: Option<Vec<protocol::ImageData>>,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamMemberActivate,
        &protocol::TeamMemberActivatePayload {
            member_id,
            prompt,
            images,
        },
    )
    .await
}

pub async fn compact_team(
    state: &AppState,
    host: &LocalHostId,
    team_id: protocol::TeamId,
    summary_prompt: Option<String>,
    max_summary_bytes: Option<u32>,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamCompact,
        &protocol::TeamCompactPayload {
            team_id,
            summary_prompt,
            max_summary_bytes,
        },
    )
    .await
}

pub async fn shuffle_team_member(
    state: &AppState,
    host: &LocalHostId,
    team_id: protocol::TeamId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamMemberShuffle,
        &protocol::TeamMemberShufflePayload { team_id },
    )
    .await
}

pub async fn create_team_draft(
    state: &AppState,
    host: &LocalHostId,
    template_id: Option<protocol::TeamTemplateId>,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDraftCreate,
        &protocol::TeamDraftCreatePayload { template_id },
    )
    .await
}

pub async fn update_team_draft(
    state: &AppState,
    host: &LocalHostId,
    update: protocol::TeamDraftUpdatePayload,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDraftUpdate,
        &update,
    )
    .await
}

pub async fn shuffle_team_draft(
    state: &AppState,
    host: &LocalHostId,
    payload: protocol::TeamDraftShufflePayload,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDraftShuffle,
        &payload,
    )
    .await
}

pub async fn apply_team_draft_template(
    state: &AppState,
    host: &LocalHostId,
    draft_id: protocol::TeamDraftId,
    template_id: protocol::TeamTemplateId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDraftApplyTemplate,
        &protocol::TeamDraftApplyTemplatePayload {
            draft_id,
            template_id,
        },
    )
    .await
}

pub async fn commit_team_draft(
    state: &AppState,
    host: &LocalHostId,
    draft_id: protocol::TeamDraftId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDraftCommit,
        &protocol::TeamDraftCommitPayload { draft_id },
    )
    .await
}

pub async fn discard_team_draft(
    state: &AppState,
    host: &LocalHostId,
    draft_id: protocol::TeamDraftId,
) -> Result<(), String> {
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::TeamDraftDiscard,
        &protocol::TeamDraftDiscardPayload { draft_id },
    )
    .await
}

pub async fn start_host_browse(
    state: &AppState,
    host: &LocalHostId,
    initial: Option<protocol::HostAbsPath>,
    include_hidden: bool,
) -> Result<protocol::StreamPath, String> {
    let browse_stream = new_browse_stream();
    state.host_browses.update(|browses| {
        browses.insert(
            (host.clone(), browse_stream.clone()),
            HostBrowseSession {
                local_host_id: host.clone(),
                stream: browse_stream.clone(),
                opened: None,
                entries_by_path: Default::default(),
                latest_error: None,
            },
        );
    });
    send_frame(
        host,
        host_stream(state, host)?,
        protocol::FrameKind::HostBrowseStart,
        &protocol::HostBrowseStartPayload {
            browse_stream: browse_stream.clone(),
            initial,
            include_hidden,
        },
    )
    .await?;
    Ok(browse_stream)
}

pub async fn list_host_browse_path(
    host: &LocalHostId,
    browse_stream: protocol::StreamPath,
    path: protocol::HostAbsPath,
    include_hidden: bool,
) -> Result<(), String> {
    send_frame(
        host,
        browse_stream,
        protocol::FrameKind::HostBrowseList,
        &protocol::HostBrowseListPayload {
            path,
            include_hidden,
        },
    )
    .await
}

pub async fn close_host_browse(
    state: &AppState,
    host: &LocalHostId,
    browse_stream: protocol::StreamPath,
) -> Result<(), String> {
    send_frame(
        host,
        browse_stream.clone(),
        protocol::FrameKind::HostBrowseClose,
        &protocol::HostBrowseClosePayload::default(),
    )
    .await?;
    state.host_browses.update(|browses| {
        browses.remove(&(host.clone(), browse_stream));
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_and_review_stream_helpers_match_protocol_paths() {
        assert_eq!(
            project_stream(&protocol::ProjectId("p1".to_owned())).0,
            "/project/p1"
        );
        assert_eq!(
            review_stream(&protocol::ReviewId("r1".to_owned())).0,
            "/review/r1"
        );
    }
}
