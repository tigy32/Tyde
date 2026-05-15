use std::collections::HashSet;

use anyhow::anyhow;
use protocol::types::CloseAgentPayload;
use protocol::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, CancelQueuedMessagePayload,
    CustomAgentDeletePayload, CustomAgentUpsertPayload, DeleteSessionPayload,
    EditQueuedMessagePayload, Envelope, FrameKind, HostBrowseClosePayload, HostBrowseListPayload,
    HostBrowseStartPayload, InterruptPayload, ListSessionsPayload, McpServerDeletePayload,
    McpServerUpsertPayload, ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload,
    ProjectDeleteRootPayload, ProjectDiscardFilePayload, ProjectGitCommitPayload, ProjectId,
    ProjectListDirPayload, ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRenamePayload,
    ProjectReorderPayload, ProjectStageFilePayload, ProjectStageHunkPayload,
    ProjectUnstageFilePayload, ReviewActionPayload, ReviewCreatePayload, ReviewId,
    ReviewSubscribePayload, RunBackendSetupPayload, SendMessagePayload,
    SendQueuedMessageNowPayload, SetAgentNamePayload, SetSessionSettingsPayload, SetSettingPayload,
    SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload, SteeringDeletePayload,
    SteeringUpsertPayload, StreamPath, TeamCreatePayload, TeamDeletePayload,
    TeamDraftApplyTemplatePayload, TeamDraftCommitPayload, TeamDraftCreatePayload,
    TeamDraftDiscardPayload, TeamDraftShufflePayload, TeamDraftUpdatePayload,
    TeamMemberActivatePayload, TeamMemberCreatePayload, TeamMemberDeletePayload,
    TeamMemberShufflePayload, TeamMemberUpdatePayload, TeamRenamePayload, TeamSetManagerPayload,
    TerminalClosePayload, TerminalCreatePayload, TerminalId, TerminalResizePayload,
    TerminalSendPayload,
};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::host::HostHandle;
use crate::stream::Stream;

pub(crate) async fn route_client_envelope(
    host: &HostHandle,
    connection_host_stream: &StreamPath,
    host_output_stream: &Stream,
    envelope: Envelope,
) -> AppResult<()> {
    if envelope.stream == *connection_host_stream {
        match envelope.kind {
            FrameKind::SetSetting => {
                let payload: SetSettingPayload = parse_payload(&envelope, "set_setting")?;
                host.set_setting(payload).await?;
            }
            FrameKind::SpawnAgent => {
                let payload: SpawnAgentPayload = parse_payload(&envelope, "spawn_agent")?;
                validate_spawn_agent(&payload)?;
                host.spawn_agent(payload).await;
            }
            FrameKind::ListSessions => {
                let _: ListSessionsPayload = parse_payload(&envelope, "list_sessions")?;
                host.list_sessions(host_output_stream).await?;
            }
            FrameKind::DeleteSession => {
                let payload: DeleteSessionPayload = parse_payload(&envelope, "delete_session")?;
                ensure_non_empty(
                    "delete_session",
                    "session_id",
                    payload.session_id.0.as_str(),
                )?;
                host.delete_session(payload.session_id).await?;
            }
            FrameKind::ProjectCreate => {
                let payload: ProjectCreatePayload = parse_payload(&envelope, "project_create")?;
                ensure_non_empty("project_create", "name", payload.name.as_str())?;
                validate_project_roots(&payload.roots)?;
                host.create_project(payload).await?;
            }
            FrameKind::ProjectRename => {
                let payload: ProjectRenamePayload = parse_payload(&envelope, "project_rename")?;
                ensure_non_empty("project_rename", "id", payload.id.0.as_str())?;
                ensure_non_empty("project_rename", "name", payload.name.as_str())?;
                host.rename_project(payload).await?;
            }
            FrameKind::ProjectReorder => {
                let payload: ProjectReorderPayload = parse_payload(&envelope, "project_reorder")?;
                validate_project_reorder(&payload)?;
                host.reorder_projects(payload).await?;
            }
            FrameKind::ProjectAddRoot => {
                let payload: ProjectAddRootPayload = parse_payload(&envelope, "project_add_root")?;
                ensure_non_empty("project_add_root", "id", payload.id.0.as_str())?;
                ensure_non_empty("project_add_root", "root", payload.root.as_str())?;
                host.add_project_root(payload).await?;
            }
            FrameKind::ProjectDeleteRoot => {
                let payload: ProjectDeleteRootPayload =
                    parse_payload(&envelope, "project_delete_root")?;
                ensure_non_empty("project_delete_root", "id", payload.id.0.as_str())?;
                ensure_non_empty("project_delete_root", "root", payload.root.as_str())?;
                host.delete_project_root(payload).await?;
            }
            FrameKind::ProjectDelete => {
                let payload: ProjectDeletePayload = parse_payload(&envelope, "project_delete")?;
                ensure_non_empty("project_delete", "id", payload.id.0.as_str())?;
                host.delete_project(payload).await?;
            }
            FrameKind::CustomAgentUpsert => {
                let payload: CustomAgentUpsertPayload =
                    parse_payload(&envelope, "custom_agent_upsert")?;
                host.upsert_custom_agent(payload).await?;
            }
            FrameKind::CustomAgentDelete => {
                let payload: CustomAgentDeletePayload =
                    parse_payload(&envelope, "custom_agent_delete")?;
                host.delete_custom_agent(payload).await?;
            }
            FrameKind::SteeringUpsert => {
                let payload: SteeringUpsertPayload = parse_payload(&envelope, "steering_upsert")?;
                host.upsert_steering(payload).await?;
            }
            FrameKind::SteeringDelete => {
                let payload: SteeringDeletePayload = parse_payload(&envelope, "steering_delete")?;
                host.delete_steering(payload).await?;
            }
            FrameKind::SkillRefresh => {
                let payload: SkillRefreshPayload = parse_payload(&envelope, "skill_refresh")?;
                host.refresh_skills(payload).await?;
            }
            FrameKind::McpServerUpsert => {
                let payload: McpServerUpsertPayload =
                    parse_payload(&envelope, "mcp_server_upsert")?;
                host.upsert_mcp_server(payload).await?;
            }
            FrameKind::McpServerDelete => {
                let payload: McpServerDeletePayload =
                    parse_payload(&envelope, "mcp_server_delete")?;
                host.delete_mcp_server(payload).await?;
            }
            FrameKind::TeamCreate => {
                let payload: TeamCreatePayload = parse_payload(&envelope, "team_create")?;
                ensure_non_empty("team_create", "name", payload.name.as_str())?;
                validate_team_member_create_spec("team_create", &payload.manager)?;
                host.create_team(payload).await?;
            }
            FrameKind::TeamRename => {
                let payload: TeamRenamePayload = parse_payload(&envelope, "team_rename")?;
                ensure_non_empty("team_rename", "id", payload.id.0.as_str())?;
                ensure_non_empty("team_rename", "name", payload.name.as_str())?;
                host.rename_team(payload).await?;
            }
            FrameKind::TeamDelete => {
                let payload: TeamDeletePayload = parse_payload(&envelope, "team_delete")?;
                ensure_non_empty("team_delete", "id", payload.id.0.as_str())?;
                host.delete_team(payload).await?;
            }
            FrameKind::TeamSetManager => {
                let payload: TeamSetManagerPayload = parse_payload(&envelope, "team_set_manager")?;
                ensure_non_empty("team_set_manager", "team_id", payload.team_id.0.as_str())?;
                ensure_non_empty(
                    "team_set_manager",
                    "new_manager_member_id",
                    payload.new_manager_member_id.0.as_str(),
                )?;
                host.set_team_manager(payload).await?;
            }
            FrameKind::TeamMemberCreate => {
                let payload: TeamMemberCreatePayload =
                    parse_payload(&envelope, "team_member_create")?;
                ensure_non_empty("team_member_create", "team_id", payload.team_id.0.as_str())?;
                if payload.session_id.is_some() {
                    return Err(AppError::invalid(
                        "team_member_create",
                        "session_id must be absent",
                    ));
                }
                validate_team_member_create_spec("team_member_create", &payload.member)?;
                host.create_team_member(payload).await?;
            }
            FrameKind::TeamMemberUpdate => {
                let payload: TeamMemberUpdatePayload =
                    parse_payload(&envelope, "team_member_update")?;
                ensure_non_empty("team_member_update", "id", payload.id.0.as_str())?;
                ensure_non_empty("team_member_update", "name", payload.name.as_str())?;
                ensure_non_empty(
                    "team_member_update",
                    "description",
                    payload.description.as_str(),
                )?;
                validate_team_profile("team_member_update", payload.profile.as_ref())?;
                validate_team_project_ids("team_member_update", &payload.project_ids)?;
                host.update_team_member(payload).await?;
            }
            FrameKind::TeamMemberDelete => {
                let payload: TeamMemberDeletePayload =
                    parse_payload(&envelope, "team_member_delete")?;
                ensure_non_empty("team_member_delete", "id", payload.id.0.as_str())?;
                host.delete_team_member(payload).await?;
            }
            FrameKind::TeamMemberActivate => {
                let payload: TeamMemberActivatePayload =
                    parse_payload(&envelope, "team_member_activate")?;
                ensure_non_empty(
                    "team_member_activate",
                    "member_id",
                    payload.member_id.0.as_str(),
                )?;
                host.activate_team_member(payload.member_id, payload.prompt, payload.images)
                    .await?;
            }
            FrameKind::TeamDraftCreate => {
                let payload: TeamDraftCreatePayload =
                    parse_payload(&envelope, "team_draft_create")?;
                host.create_team_draft(payload).await?;
            }
            FrameKind::TeamDraftUpdate => {
                let payload: TeamDraftUpdatePayload =
                    parse_payload(&envelope, "team_draft_update")?;
                validate_team_draft_update(&payload)?;
                host.update_team_draft(payload).await?;
            }
            FrameKind::TeamDraftShuffle => {
                let payload: TeamDraftShufflePayload =
                    parse_payload(&envelope, "team_draft_shuffle")?;
                ensure_non_empty(
                    "team_draft_shuffle",
                    "draft_id",
                    payload.draft_id.0.as_str(),
                )?;
                if let Some(member_id) = payload.member_id.as_ref() {
                    ensure_non_empty("team_draft_shuffle", "member_id", member_id.0.as_str())?;
                }
                host.shuffle_team_draft(payload).await?;
            }
            FrameKind::TeamMemberShuffle => {
                let payload: TeamMemberShufflePayload =
                    parse_payload(&envelope, "team_member_shuffle")?;
                ensure_non_empty("team_member_shuffle", "team_id", payload.team_id.0.as_str())?;
                host.shuffle_team_member(payload).await?;
            }
            FrameKind::TeamDraftApplyTemplate => {
                let payload: TeamDraftApplyTemplatePayload =
                    parse_payload(&envelope, "team_draft_apply_template")?;
                ensure_non_empty(
                    "team_draft_apply_template",
                    "draft_id",
                    payload.draft_id.0.as_str(),
                )?;
                ensure_non_empty(
                    "team_draft_apply_template",
                    "template_id",
                    payload.template_id.0.as_str(),
                )?;
                host.apply_team_draft_template(payload).await?;
            }
            FrameKind::TeamDraftCommit => {
                let payload: TeamDraftCommitPayload =
                    parse_payload(&envelope, "team_draft_commit")?;
                ensure_non_empty("team_draft_commit", "draft_id", payload.draft_id.0.as_str())?;
                host.commit_team_draft(payload).await?;
            }
            FrameKind::TeamDraftDiscard => {
                let payload: TeamDraftDiscardPayload =
                    parse_payload(&envelope, "team_draft_discard")?;
                ensure_non_empty(
                    "team_draft_discard",
                    "draft_id",
                    payload.draft_id.0.as_str(),
                )?;
                host.discard_team_draft(payload).await?;
            }
            FrameKind::HostBrowseStart => {
                let payload: HostBrowseStartPayload =
                    parse_payload(&envelope, "host_browse_start")?;
                if !payload.browse_stream.0.starts_with("/browse/") {
                    return Err(AppError::invalid(
                        "host_browse_start",
                        format!(
                            "browse_stream must start with /browse/, got {}",
                            payload.browse_stream
                        ),
                    ));
                }
                if let Some(initial) = payload.initial.as_ref() {
                    ensure_non_empty("host_browse_start", "initial", initial.0.as_str())?;
                }
                host.open_browse_stream(connection_host_stream, host_output_stream, payload)
                    .await?;
            }
            FrameKind::TerminalCreate => {
                let payload: TerminalCreatePayload = parse_payload(&envelope, "terminal_create")?;
                validate_terminal_dimensions("terminal_create", payload.cols, payload.rows)?;
                host.create_terminal(connection_host_stream, host_output_stream, payload)
                    .await?;
            }
            FrameKind::RunBackendSetup => {
                let payload: RunBackendSetupPayload =
                    parse_payload(&envelope, "run_backend_setup")?;
                host.run_backend_setup(connection_host_stream, host_output_stream, payload)
                    .await?;
            }
            other => {
                return Err(AppError::protocol(
                    "route_client_envelope",
                    format!(
                        "unexpected client frame kind {} on host stream {}",
                        other, envelope.stream
                    ),
                ));
            }
        }
        return Ok(());
    }

    if envelope.stream.0.starts_with("/agent/") {
        match envelope.kind {
            FrameKind::SendMessage => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let payload: SendMessagePayload = parse_payload(&envelope, "send_message")?;

                let sent = if let Some(agent) = host.agent_handle(&agent_id).await {
                    agent.send_input(AgentInput::SendMessage(payload)).await
                } else {
                    false
                };

                if !sent {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            FrameKind::EditQueuedMessage => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let payload: EditQueuedMessagePayload =
                    parse_payload(&envelope, "edit_queued_message")?;

                let sent = if let Some(agent) = host.agent_handle(&agent_id).await {
                    agent
                        .send_input(AgentInput::EditQueuedMessage(payload))
                        .await
                } else {
                    false
                };

                if !sent {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            FrameKind::CancelQueuedMessage => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let payload: CancelQueuedMessagePayload =
                    parse_payload(&envelope, "cancel_queued_message")?;

                let sent = if let Some(agent) = host.agent_handle(&agent_id).await {
                    agent
                        .send_input(AgentInput::CancelQueuedMessage(payload))
                        .await
                } else {
                    false
                };

                if !sent {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            FrameKind::SendQueuedMessageNow => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let payload: SendQueuedMessageNowPayload =
                    parse_payload(&envelope, "send_queued_message_now")?;

                let sent = if let Some(agent) = host.agent_handle(&agent_id).await {
                    agent
                        .send_input(AgentInput::SendQueuedMessageNow(payload))
                        .await
                } else {
                    false
                };

                if !sent {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            FrameKind::SetAgentName => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let payload: SetAgentNamePayload = parse_payload(&envelope, "set_agent_name")?;
                ensure_non_empty("set_agent_name", "name", payload.name.as_str())?;

                match host.agent_handle(&agent_id).await {
                    Some(agent) => match agent.set_name(payload.name).await {
                        Some(true) => host.fan_out_session_lists().await,
                        Some(false) => {}
                        None => {
                            let stream = host_output_stream.with_path(stream_path);
                            send_agent_not_running_error(stream, agent_id).await;
                        }
                    },
                    None => {
                        let stream = host_output_stream.with_path(stream_path);
                        send_agent_not_running_error(stream, agent_id).await;
                    }
                }
            }
            FrameKind::Interrupt => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let _: InterruptPayload = parse_payload(&envelope, "interrupt")?;

                let interrupted = host.interrupt_agent(&agent_id).await;

                if !interrupted {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            FrameKind::CloseAgent => {
                let agent_id = parse_agent_id(&envelope.stream)?;
                let _: CloseAgentPayload = parse_payload(&envelope, "close_agent")?;
                host.close_agent(&agent_id).await;
            }
            FrameKind::SetSessionSettings => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path)?;
                let payload: SetSessionSettingsPayload =
                    parse_payload(&envelope, "set_session_settings")?;

                let sent = if let Some(agent) = host.agent_handle(&agent_id).await {
                    agent
                        .send_input(AgentInput::UpdateSessionSettings(payload))
                        .await
                } else {
                    false
                };

                if !sent {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            other => {
                return Err(AppError::protocol(
                    "route_client_envelope",
                    format!(
                        "unexpected client frame kind {} on agent stream {}",
                        other, envelope.stream
                    ),
                ));
            }
        }
        return Ok(());
    }

    if envelope.stream.0.starts_with("/terminal/") {
        let stream_path = envelope.stream.clone();
        let terminal_id = parse_terminal_id(&stream_path)?;

        match envelope.kind {
            FrameKind::TerminalSend => {
                let payload: TerminalSendPayload = parse_payload(&envelope, "terminal_send")?;
                host.send_terminal_input(connection_host_stream, &terminal_id, payload)
                    .await?;
            }
            FrameKind::TerminalResize => {
                let payload: TerminalResizePayload = parse_payload(&envelope, "terminal_resize")?;
                validate_terminal_dimensions("terminal_resize", payload.cols, payload.rows)?;
                host.resize_terminal(connection_host_stream, &terminal_id, payload)
                    .await?;
            }
            FrameKind::TerminalClose => {
                let _: TerminalClosePayload = parse_payload(&envelope, "terminal_close")?;
                host.close_terminal(connection_host_stream, &terminal_id)
                    .await?;
            }
            other => {
                return Err(AppError::protocol(
                    "route_client_envelope",
                    format!(
                        "unexpected client frame kind {} on terminal stream {}",
                        other, envelope.stream
                    ),
                ));
            }
        }
        return Ok(());
    }

    if envelope.stream.0.starts_with("/project/") {
        let stream_path = envelope.stream.clone();
        let project_id = parse_project_id(&stream_path)?;
        let project_output_stream = host_output_stream.with_path(stream_path.clone());

        match envelope.kind {
            FrameKind::ProjectListDir => {
                let payload: ProjectListDirPayload = parse_payload(&envelope, "project_list_dir")?;
                ensure_non_empty("project_list_dir", "root", payload.root.0.as_str())?;
                host.list_project_dir(
                    connection_host_stream,
                    &project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectReadFile => {
                let payload: ProjectReadFilePayload =
                    parse_payload(&envelope, "project_read_file")?;
                ensure_non_empty("project_read_file", "root", payload.path.root.0.as_str())?;
                ensure_non_empty(
                    "project_read_file",
                    "relative_path",
                    payload.path.relative_path.as_str(),
                )?;
                host.read_project_file(
                    connection_host_stream,
                    &project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectReadDiff => {
                let payload: ProjectReadDiffPayload =
                    parse_payload(&envelope, "project_read_diff")?;
                ensure_non_empty("project_read_diff", "root", payload.root.0.as_str())?;
                if let Some(path) = &payload.path {
                    ensure_non_empty("project_read_diff", "path", path.as_str())?;
                }
                host.read_project_diff(
                    connection_host_stream,
                    &project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ReviewCreate => {
                let payload: ReviewCreatePayload = parse_payload(&envelope, "review_create")?;
                ensure_non_empty(
                    "review_create",
                    "origin_agent_id",
                    payload.origin_agent_id.0.as_str(),
                )?;
                host.create_review(
                    connection_host_stream,
                    &project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectStageFile => {
                let payload: ProjectStageFilePayload =
                    parse_payload(&envelope, "project_stage_file")?;
                ensure_non_empty("project_stage_file", "root", payload.path.root.0.as_str())?;
                ensure_non_empty(
                    "project_stage_file",
                    "relative_path",
                    payload.path.relative_path.as_str(),
                )?;
                host.stage_project_file(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectStageHunk => {
                let payload: ProjectStageHunkPayload =
                    parse_payload(&envelope, "project_stage_hunk")?;
                ensure_non_empty("project_stage_hunk", "root", payload.path.root.0.as_str())?;
                ensure_non_empty(
                    "project_stage_hunk",
                    "relative_path",
                    payload.path.relative_path.as_str(),
                )?;
                ensure_non_empty("project_stage_hunk", "hunk_id", payload.hunk_id.as_str())?;
                host.stage_project_hunk(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectUnstageFile => {
                let payload: ProjectUnstageFilePayload =
                    parse_payload(&envelope, "project_unstage_file")?;
                ensure_non_empty("project_unstage_file", "root", payload.path.root.0.as_str())?;
                ensure_non_empty(
                    "project_unstage_file",
                    "relative_path",
                    payload.path.relative_path.as_str(),
                )?;
                host.unstage_project_file(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectDiscardFile => {
                let payload: ProjectDiscardFilePayload =
                    parse_payload(&envelope, "project_discard_file")?;
                ensure_non_empty("project_discard_file", "root", payload.path.root.0.as_str())?;
                ensure_non_empty(
                    "project_discard_file",
                    "relative_path",
                    payload.path.relative_path.as_str(),
                )?;
                host.discard_project_file(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ProjectGitCommit => {
                let payload: ProjectGitCommitPayload =
                    parse_payload(&envelope, "project_git_commit")?;
                ensure_non_empty("project_git_commit", "root", payload.root.0.as_str())?;
                ensure_non_empty("project_git_commit", "message", payload.message.as_str())?;
                host.commit_project(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await?;
            }
            other => {
                return Err(AppError::protocol(
                    "route_client_envelope",
                    format!(
                        "unexpected client frame kind {} on project stream {}",
                        other, envelope.stream
                    ),
                ));
            }
        }
        return Ok(());
    }

    if envelope.stream.0.starts_with("/browse/") {
        let stream_path = envelope.stream.clone();
        match envelope.kind {
            FrameKind::HostBrowseList => {
                let payload: HostBrowseListPayload = parse_payload(&envelope, "host_browse_list")?;
                ensure_non_empty("host_browse_list", "path", payload.path.0.as_str())?;
                host.list_browse_dir(connection_host_stream, &stream_path, payload)
                    .await?;
            }
            FrameKind::HostBrowseClose => {
                let _: HostBrowseClosePayload = parse_payload(&envelope, "host_browse_close")?;
                host.close_browse_stream(connection_host_stream, &stream_path)
                    .await;
            }
            other => {
                return Err(AppError::protocol(
                    "route_client_envelope",
                    format!(
                        "unexpected client frame kind {} on browse stream {}",
                        other, envelope.stream
                    ),
                ));
            }
        }
        return Ok(());
    }

    if envelope.stream.0.starts_with("/review/") {
        let stream_path = envelope.stream.clone();
        let review_id = parse_review_id(&stream_path)?;
        let review_output_stream = host_output_stream.with_path(stream_path.clone());

        match envelope.kind {
            FrameKind::ReviewAction => {
                let payload: ReviewActionPayload = parse_payload(&envelope, "review_action")?;
                host.review_action(
                    connection_host_stream,
                    review_output_stream,
                    review_id,
                    payload,
                )
                .await?;
            }
            FrameKind::ReviewSubscribe => {
                let _: ReviewSubscribePayload = parse_payload(&envelope, "review_subscribe")?;
                host.review_subscribe(connection_host_stream, review_output_stream, review_id)
                    .await?;
            }
            other => {
                return Err(AppError::protocol(
                    "route_client_envelope",
                    format!(
                        "unexpected client frame kind {} on review stream {}",
                        other, envelope.stream
                    ),
                ));
            }
        }
        return Ok(());
    }

    Err(AppError::protocol(
        "route_client_envelope",
        format!("unknown stream {} from client", envelope.stream),
    ))
}

fn parse_payload<T: DeserializeOwned>(
    envelope: &Envelope,
    operation: &'static str,
) -> AppResult<T> {
    envelope.parse_payload().map_err(|error| {
        AppError::invalid_with_source(
            operation,
            format!("invalid {operation} payload: {error}"),
            error,
        )
    })
}

fn validate_spawn_agent(payload: &SpawnAgentPayload) -> AppResult<()> {
    if let Some(name) = payload.name.as_ref() {
        ensure_non_empty("spawn_agent", "name", name)?;
    }

    match &payload.params {
        SpawnAgentParams::New {
            workspace_roots,
            prompt,
            images,
            ..
        } => {
            for root in workspace_roots {
                ensure_non_empty("spawn_agent", "workspace_root", root)?;
            }
            if prompt.trim().is_empty() && images.as_ref().is_none_or(|images| images.is_empty()) {
                return Err(AppError::invalid(
                    "spawn_agent",
                    "new prompt must not be empty unless images are attached",
                ));
            }
        }
        SpawnAgentParams::Resume { session_id, .. } => {
            ensure_non_empty("spawn_agent", "session_id", session_id.0.as_str())?;
        }
    }

    Ok(())
}

fn validate_team_member_create_spec(
    operation: &'static str,
    spec: &protocol::TeamMemberCreateSpec,
) -> AppResult<()> {
    ensure_non_empty(operation, "name", spec.name.as_str())?;
    ensure_non_empty(operation, "description", spec.description.as_str())?;
    validate_team_profile(operation, spec.profile.as_ref())?;
    if let Some(custom_agent_id) = spec.custom_agent_id.as_ref() {
        ensure_non_empty(operation, "custom_agent_id", custom_agent_id.0.as_str())?;
    }
    validate_team_project_ids(operation, &spec.project_ids)
}

fn validate_team_draft_update(payload: &TeamDraftUpdatePayload) -> AppResult<()> {
    match payload {
        TeamDraftUpdatePayload::SetName { draft_id, .. } => {
            ensure_non_empty("team_draft_update", "draft_id", draft_id.0.as_str())
        }
        TeamDraftUpdatePayload::ReplaceMember { draft_id, member } => {
            ensure_non_empty("team_draft_update", "draft_id", draft_id.0.as_str())?;
            ensure_non_empty("team_draft_update", "member_id", member.id.0.as_str())?;
            if let Some(custom_agent_id) = member.custom_agent_id.as_ref() {
                ensure_non_empty(
                    "team_draft_update",
                    "custom_agent_id",
                    custom_agent_id.0.as_str(),
                )?;
            }
            validate_optional_team_project_ids("team_draft_update", &member.project_ids)
        }
        TeamDraftUpdatePayload::AddReport { draft_id } => {
            ensure_non_empty("team_draft_update", "draft_id", draft_id.0.as_str())
        }
        TeamDraftUpdatePayload::RemoveMember {
            draft_id,
            member_id,
        } => {
            ensure_non_empty("team_draft_update", "draft_id", draft_id.0.as_str())?;
            ensure_non_empty("team_draft_update", "member_id", member_id.0.as_str())
        }
        TeamDraftUpdatePayload::SetMemberProfile {
            draft_id,
            member_id,
            role_preset_id,
            personality_preset_id,
            ..
        } => {
            ensure_non_empty("team_draft_update", "draft_id", draft_id.0.as_str())?;
            ensure_non_empty("team_draft_update", "member_id", member_id.0.as_str())?;
            if let Some(role_preset_id) = role_preset_id.as_ref() {
                ensure_non_empty(
                    "team_draft_update",
                    "role_preset_id",
                    role_preset_id.0.as_str(),
                )?;
            }
            if let Some(personality_preset_id) = personality_preset_id.as_ref() {
                ensure_non_empty(
                    "team_draft_update",
                    "personality_preset_id",
                    personality_preset_id.0.as_str(),
                )?;
            }
            Ok(())
        }
    }
}

fn validate_team_profile(
    operation: &'static str,
    profile: Option<&protocol::TeamMemberPresetProfile>,
) -> AppResult<()> {
    let Some(profile) = profile else {
        return Ok(());
    };
    if let Some(role_preset_id) = profile.role_preset_id.as_ref() {
        ensure_non_empty(operation, "role_preset_id", role_preset_id.0.as_str())?;
    }
    if let Some(personality_preset_id) = profile.personality_preset_id.as_ref() {
        ensure_non_empty(
            operation,
            "personality_preset_id",
            personality_preset_id.0.as_str(),
        )?;
    }
    Ok(())
}

fn validate_team_project_ids(operation: &'static str, project_ids: &[ProjectId]) -> AppResult<()> {
    if project_ids.is_empty() {
        return Err(AppError::invalid(
            operation,
            "project_ids must not be empty",
        ));
    }
    let mut seen = HashSet::new();
    for project_id in project_ids {
        ensure_non_empty(operation, "project_id", project_id.0.as_str())?;
        if !seen.insert(project_id.0.as_str()) {
            return Err(AppError::invalid(
                operation,
                format!("project_ids contains duplicate id {}", project_id),
            ));
        }
    }
    Ok(())
}

fn validate_optional_team_project_ids(
    operation: &'static str,
    project_ids: &[ProjectId],
) -> AppResult<()> {
    if project_ids.is_empty() {
        return Ok(());
    }
    validate_team_project_ids(operation, project_ids)
}

fn validate_project_reorder(payload: &ProjectReorderPayload) -> AppResult<()> {
    let mut seen_ids = HashSet::new();
    for project_id in &payload.project_ids {
        ensure_non_empty("project_reorder", "project_id", project_id.0.as_str())?;
        if !seen_ids.insert(project_id.0.clone()) {
            return Err(AppError::invalid(
                "project_reorder",
                format!("contains duplicate id {}", project_id),
            ));
        }
    }
    Ok(())
}

fn validate_terminal_dimensions(operation: &'static str, cols: u16, rows: u16) -> AppResult<()> {
    if cols < 2 {
        return Err(AppError::invalid(
            operation,
            format!("cols must be at least 2, got {cols}"),
        ));
    }
    if rows < 1 {
        return Err(AppError::invalid(
            operation,
            format!("rows must be at least 1, got {rows}"),
        ));
    }
    Ok(())
}

fn ensure_non_empty(operation: &'static str, field: &'static str, value: &str) -> AppResult<()> {
    if value.trim().is_empty() {
        return Err(AppError::invalid(
            operation,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
}

fn parse_agent_id(stream: &StreamPath) -> AppResult<AgentId> {
    let segments: Vec<&str> = stream.0.split('/').collect();
    if segments.len() != 4 {
        return Err(AppError::protocol(
            "parse_agent_stream",
            format!(
                "agent stream must have format /agent/<agent_id>/<instance_id>, got {}",
                stream
            ),
        ));
    }
    if segments.first() != Some(&"") {
        return Err(AppError::protocol(
            "parse_agent_stream",
            format!("agent stream must be absolute path, got {}", stream),
        ));
    }
    if segments[1] != "agent" {
        return Err(AppError::protocol(
            "parse_agent_stream",
            format!("expected /agent/<agent_id>/<instance_id>, got {}", stream),
        ));
    }

    Uuid::parse_str(segments[2]).map_err(|error| {
        AppError::protocol(
            "parse_agent_stream",
            format!(
                "agent stream contains invalid agent_id UUID {} in {}",
                segments[2], stream
            ),
        )
        .with_source(anyhow!(error))
    })?;
    Uuid::parse_str(segments[3]).map_err(|error| {
        AppError::protocol(
            "parse_agent_stream",
            format!(
                "agent stream contains invalid instance_id UUID {} in {}",
                segments[3], stream
            ),
        )
        .with_source(anyhow!(error))
    })?;

    Ok(AgentId(segments[2].to_owned()))
}

fn parse_project_id(stream: &StreamPath) -> AppResult<ProjectId> {
    let segments: Vec<&str> = stream.0.split('/').collect();
    if segments.len() != 3 {
        return Err(AppError::protocol(
            "parse_project_stream",
            format!(
                "project stream must have format /project/<project_id>, got {}",
                stream
            ),
        ));
    }
    if segments.first() != Some(&"") {
        return Err(AppError::protocol(
            "parse_project_stream",
            format!("project stream must be absolute path, got {}", stream),
        ));
    }
    if segments[1] != "project" {
        return Err(AppError::protocol(
            "parse_project_stream",
            format!("expected /project/<project_id> stream, got {}", stream),
        ));
    }

    Uuid::parse_str(segments[2]).map_err(|error| {
        AppError::protocol(
            "parse_project_stream",
            format!(
                "project stream contains invalid project_id UUID {} in {}",
                segments[2], stream
            ),
        )
        .with_source(anyhow!(error))
    })?;

    Ok(ProjectId(segments[2].to_owned()))
}

fn parse_terminal_id(stream: &StreamPath) -> AppResult<TerminalId> {
    let segments: Vec<&str> = stream.0.split('/').collect();
    if segments.len() != 3 {
        return Err(AppError::protocol(
            "parse_terminal_stream",
            format!(
                "terminal stream must have format /terminal/<terminal_id>, got {}",
                stream
            ),
        ));
    }
    if segments.first() != Some(&"") {
        return Err(AppError::protocol(
            "parse_terminal_stream",
            format!("terminal stream must be absolute path, got {}", stream),
        ));
    }
    if segments[1] != "terminal" {
        return Err(AppError::protocol(
            "parse_terminal_stream",
            format!("expected /terminal/<terminal_id> stream, got {}", stream),
        ));
    }

    Uuid::parse_str(segments[2]).map_err(|error| {
        AppError::protocol(
            "parse_terminal_stream",
            format!(
                "terminal stream contains invalid terminal_id UUID {} in {}",
                segments[2], stream
            ),
        )
        .with_source(anyhow!(error))
    })?;

    Ok(TerminalId(segments[2].to_owned()))
}

fn parse_review_id(stream: &StreamPath) -> AppResult<ReviewId> {
    let segments: Vec<&str> = stream.0.split('/').collect();
    if segments.len() != 3 {
        return Err(AppError::protocol(
            "parse_review_stream",
            format!(
                "review stream must have format /review/<review_id>, got {}",
                stream
            ),
        ));
    }
    if segments.first() != Some(&"") {
        return Err(AppError::protocol(
            "parse_review_stream",
            format!("review stream must be absolute path, got {}", stream),
        ));
    }
    if segments[1] != "review" {
        return Err(AppError::protocol(
            "parse_review_stream",
            format!("expected /review/<review_id> stream, got {}", stream),
        ));
    }
    Uuid::parse_str(segments[2]).map_err(|error| {
        AppError::protocol(
            "parse_review_stream",
            format!(
                "review stream contains invalid review_id UUID {} in {}",
                segments[2], stream
            ),
        )
        .with_source(anyhow!(error))
    })?;
    Ok(ReviewId(segments[2].to_owned()))
}

async fn send_agent_not_running_error(stream: Stream, agent_id: AgentId) {
    let payload = AgentErrorPayload {
        agent_id,
        code: AgentErrorCode::Internal,
        message: "agent not running".to_owned(),
        fatal: false,
    };
    match serde_json::to_value(&payload) {
        Ok(payload) => {
            let _ = stream.send_value(FrameKind::AgentError, payload);
        }
        Err(error) => {
            tracing::error!(
                agent_id = %payload.agent_id,
                error = %error,
                "failed to serialize AgentError payload for stream error emission"
            );
        }
    }
}

fn validate_project_roots(roots: &[String]) -> AppResult<()> {
    if roots.is_empty() {
        return Err(AppError::invalid(
            "project_create",
            "requires at least one root",
        ));
    }

    let mut seen = HashSet::new();
    for root in roots {
        ensure_non_empty("project_create", "root", root)?;
        if !seen.insert(root.as_str()) {
            return Err(AppError::invalid(
                "project_create",
                format!("roots must be unique: {}", root),
            ));
        }
    }

    Ok(())
}
