use std::collections::HashSet;

use protocol::types::CloseAgentPayload;
use protocol::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, CancelQueuedMessagePayload,
    CustomAgentDeletePayload, CustomAgentUpsertPayload, DeleteSessionPayload,
    EditQueuedMessagePayload, Envelope, FrameKind, HostBrowseClosePayload, HostBrowseListPayload,
    HostBrowseStartPayload, InterruptPayload, ListSessionsPayload, McpServerDeletePayload,
    McpServerUpsertPayload, ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload,
    ProjectId, ProjectListDirPayload, ProjectReadDiffPayload, ProjectReadFilePayload,
    ProjectRefreshPayload, ProjectRenamePayload, ProjectReorderPayload, ProjectStageFilePayload,
    ProjectStageHunkPayload, RunBackendSetupPayload, SendMessagePayload,
    SendQueuedMessageNowPayload, SetAgentNamePayload, SetSessionSettingsPayload, SetSettingPayload,
    SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload, SteeringDeletePayload,
    SteeringUpsertPayload, StreamPath, TerminalClosePayload, TerminalCreatePayload, TerminalId,
    TerminalResizePayload, TerminalSendPayload,
};
use uuid::Uuid;

use crate::host::HostHandle;
use crate::stream::Stream;

pub(crate) async fn route_client_envelope(
    host: &HostHandle,
    connection_host_stream: &StreamPath,
    host_output_stream: &Stream,
    envelope: Envelope,
) {
    if envelope.stream == *connection_host_stream {
        match envelope.kind {
            FrameKind::SetSetting => {
                let payload: SetSettingPayload = envelope
                    .parse_payload()
                    .expect("invalid set_setting payload");
                host.set_setting(payload).await;
            }
            FrameKind::SpawnAgent => {
                let payload: SpawnAgentPayload = envelope
                    .parse_payload()
                    .expect("invalid spawn_agent payload");
                if let Some(name) = payload.name.as_ref() {
                    assert!(
                        !name.trim().is_empty(),
                        "spawn_agent name must not be empty when provided"
                    );
                }
                match &payload.params {
                    SpawnAgentParams::New {
                        workspace_roots,
                        prompt,
                        images,
                        ..
                    } => {
                        assert!(
                            workspace_roots.iter().all(|root| !root.trim().is_empty()),
                            "spawn_agent workspace roots must not contain empty values"
                        );
                        assert!(
                            !prompt.trim().is_empty()
                                || images.as_ref().is_some_and(|images| !images.is_empty()),
                            "spawn_agent new prompt must not be empty unless images are attached"
                        );
                    }
                    SpawnAgentParams::Resume { session_id, .. } => {
                        assert!(
                            !session_id.0.trim().is_empty(),
                            "resume session_id must not be empty"
                        );
                    }
                }
                host.spawn_agent(payload).await;
            }
            FrameKind::ListSessions => {
                let _: ListSessionsPayload = envelope
                    .parse_payload()
                    .expect("invalid list_sessions payload");
                host.list_sessions(host_output_stream).await;
            }
            FrameKind::DeleteSession => {
                let payload: DeleteSessionPayload = envelope
                    .parse_payload()
                    .expect("invalid delete_session payload");
                host.delete_session(payload.session_id).await;
            }
            FrameKind::ProjectCreate => {
                let payload: ProjectCreatePayload = envelope
                    .parse_payload()
                    .expect("invalid project_create payload");
                assert!(
                    !payload.name.trim().is_empty(),
                    "project_create name must not be empty"
                );
                validate_project_roots(&payload.roots);
                host.create_project(payload).await;
            }
            FrameKind::ProjectRename => {
                let payload: ProjectRenamePayload = envelope
                    .parse_payload()
                    .expect("invalid project_rename payload");
                assert!(
                    !payload.id.0.trim().is_empty(),
                    "project_rename id must not be empty"
                );
                assert!(
                    !payload.name.trim().is_empty(),
                    "project_rename name must not be empty"
                );
                host.rename_project(payload).await;
            }
            FrameKind::ProjectReorder => {
                let payload: ProjectReorderPayload = envelope
                    .parse_payload()
                    .expect("invalid project_reorder payload");
                let mut seen_ids = HashSet::new();
                for project_id in &payload.project_ids {
                    assert!(
                        !project_id.0.trim().is_empty(),
                        "project_reorder ids must not be empty"
                    );
                    assert!(
                        seen_ids.insert(project_id.0.clone()),
                        "project_reorder contains duplicate id {}",
                        project_id
                    );
                }
                host.reorder_projects(payload).await;
            }
            FrameKind::ProjectAddRoot => {
                let payload: ProjectAddRootPayload = envelope
                    .parse_payload()
                    .expect("invalid project_add_root payload");
                assert!(
                    !payload.id.0.trim().is_empty(),
                    "project_add_root id must not be empty"
                );
                assert!(
                    !payload.root.trim().is_empty(),
                    "project_add_root root must not be empty"
                );
                host.add_project_root(payload).await;
            }
            FrameKind::ProjectDelete => {
                let payload: ProjectDeletePayload = envelope
                    .parse_payload()
                    .expect("invalid project_delete payload");
                assert!(
                    !payload.id.0.trim().is_empty(),
                    "project_delete id must not be empty"
                );
                host.delete_project(payload).await;
            }
            FrameKind::CustomAgentUpsert => {
                let payload: CustomAgentUpsertPayload = envelope
                    .parse_payload()
                    .expect("invalid custom_agent_upsert payload");
                host.upsert_custom_agent(payload).await;
            }
            FrameKind::CustomAgentDelete => {
                let payload: CustomAgentDeletePayload = envelope
                    .parse_payload()
                    .expect("invalid custom_agent_delete payload");
                host.delete_custom_agent(payload).await;
            }
            FrameKind::SteeringUpsert => {
                let payload: SteeringUpsertPayload = envelope
                    .parse_payload()
                    .expect("invalid steering_upsert payload");
                host.upsert_steering(payload).await;
            }
            FrameKind::SteeringDelete => {
                let payload: SteeringDeletePayload = envelope
                    .parse_payload()
                    .expect("invalid steering_delete payload");
                host.delete_steering(payload).await;
            }
            FrameKind::SkillRefresh => {
                let payload: SkillRefreshPayload = envelope
                    .parse_payload()
                    .expect("invalid skill_refresh payload");
                host.refresh_skills(payload).await;
            }
            FrameKind::McpServerUpsert => {
                let payload: McpServerUpsertPayload = envelope
                    .parse_payload()
                    .expect("invalid mcp_server_upsert payload");
                host.upsert_mcp_server(payload).await;
            }
            FrameKind::McpServerDelete => {
                let payload: McpServerDeletePayload = envelope
                    .parse_payload()
                    .expect("invalid mcp_server_delete payload");
                host.delete_mcp_server(payload).await;
            }
            FrameKind::HostBrowseStart => {
                let payload: HostBrowseStartPayload = envelope
                    .parse_payload()
                    .expect("invalid host_browse_start payload");
                assert!(
                    payload.browse_stream.0.starts_with("/browse/"),
                    "host_browse_start browse_stream must start with /browse/, got {}",
                    payload.browse_stream
                );
                if let Some(initial) = payload.initial.as_ref() {
                    assert!(
                        !initial.0.trim().is_empty(),
                        "host_browse_start initial must not be empty when provided"
                    );
                }
                host.open_browse_stream(connection_host_stream, host_output_stream, payload)
                    .await;
            }
            FrameKind::TerminalCreate => {
                let payload: TerminalCreatePayload = envelope
                    .parse_payload()
                    .expect("invalid terminal_create payload");
                assert!(
                    payload.cols >= 2,
                    "terminal_create cols must be at least 2, got {}",
                    payload.cols
                );
                assert!(
                    payload.rows >= 1,
                    "terminal_create rows must be at least 1, got {}",
                    payload.rows
                );
                host.create_terminal(connection_host_stream, host_output_stream, payload)
                    .await;
            }
            FrameKind::RunBackendSetup => {
                let payload: RunBackendSetupPayload = envelope
                    .parse_payload()
                    .expect("invalid run_backend_setup payload");
                host.run_backend_setup(connection_host_stream, host_output_stream, payload)
                    .await;
            }
            other => {
                panic!(
                    "protocol violation: unexpected client frame kind {} on host stream {}",
                    other, envelope.stream
                );
            }
        }
        return;
    }

    if envelope.stream.0.starts_with("/agent/") {
        match envelope.kind {
            FrameKind::SendMessage => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path);
                let payload: SendMessagePayload = envelope
                    .parse_payload()
                    .expect("invalid send_message payload");

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
                let agent_id = parse_agent_id(&stream_path);
                let payload: EditQueuedMessagePayload = envelope
                    .parse_payload()
                    .expect("invalid edit_queued_message payload");

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
                let agent_id = parse_agent_id(&stream_path);
                let payload: CancelQueuedMessagePayload = envelope
                    .parse_payload()
                    .expect("invalid cancel_queued_message payload");

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
                let agent_id = parse_agent_id(&stream_path);
                let payload: SendQueuedMessageNowPayload = envelope
                    .parse_payload()
                    .expect("invalid send_queued_message_now payload");

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
                let agent_id = parse_agent_id(&stream_path);
                let payload: SetAgentNamePayload = envelope
                    .parse_payload()
                    .expect("invalid set_agent_name payload");
                assert!(
                    !payload.name.trim().is_empty(),
                    "set_agent_name name must not be empty"
                );

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
                let agent_id = parse_agent_id(&stream_path);
                let _: InterruptPayload =
                    envelope.parse_payload().expect("invalid interrupt payload");

                let interrupted = host.interrupt_agent(&agent_id).await;

                if !interrupted {
                    let stream = host_output_stream.with_path(stream_path);
                    send_agent_not_running_error(stream, agent_id).await;
                }
            }
            FrameKind::CloseAgent => {
                let agent_id = parse_agent_id(&envelope.stream);
                let _: CloseAgentPayload = envelope
                    .parse_payload()
                    .expect("invalid close_agent payload");
                host.close_agent(&agent_id).await;
            }
            FrameKind::SetSessionSettings => {
                let stream_path = envelope.stream.clone();
                let agent_id = parse_agent_id(&stream_path);
                let payload: SetSessionSettingsPayload = envelope
                    .parse_payload()
                    .expect("invalid set_session_settings payload");

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
                panic!(
                    "protocol violation: unexpected client frame kind {} on agent stream {}",
                    other, envelope.stream
                );
            }
        }
        return;
    }

    if envelope.stream.0.starts_with("/terminal/") {
        let stream_path = envelope.stream.clone();
        let terminal_id = parse_terminal_id(&stream_path);

        match envelope.kind {
            FrameKind::TerminalSend => {
                let payload: TerminalSendPayload = envelope
                    .parse_payload()
                    .expect("invalid terminal_send payload");
                host.send_terminal_input(connection_host_stream, &terminal_id, payload)
                    .await;
            }
            FrameKind::TerminalResize => {
                let payload: TerminalResizePayload = envelope
                    .parse_payload()
                    .expect("invalid terminal_resize payload");
                assert!(
                    payload.cols >= 2,
                    "terminal_resize cols must be at least 2, got {}",
                    payload.cols
                );
                assert!(
                    payload.rows >= 1,
                    "terminal_resize rows must be at least 1, got {}",
                    payload.rows
                );
                host.resize_terminal(connection_host_stream, &terminal_id, payload)
                    .await;
            }
            FrameKind::TerminalClose => {
                let _: TerminalClosePayload = envelope
                    .parse_payload()
                    .expect("invalid terminal_close payload");
                host.close_terminal(connection_host_stream, &terminal_id)
                    .await;
            }
            other => {
                panic!(
                    "protocol violation: unexpected client frame kind {} on terminal stream {}",
                    other, envelope.stream
                );
            }
        }
        return;
    }

    if envelope.stream.0.starts_with("/project/") {
        let stream_path = envelope.stream.clone();
        let project_id = parse_project_id(&stream_path);
        let project_output_stream = host_output_stream.with_path(stream_path.clone());

        match envelope.kind {
            FrameKind::ProjectRefresh => {
                let payload: ProjectRefreshPayload = envelope
                    .parse_payload()
                    .expect("invalid project_refresh payload");
                host.refresh_project(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await;
            }
            FrameKind::ProjectListDir => {
                let payload: ProjectListDirPayload = envelope
                    .parse_payload()
                    .expect("invalid project_list_dir payload");
                assert!(
                    !payload.root.0.trim().is_empty(),
                    "project_list_dir root must not be empty"
                );
                host.list_project_dir(&project_output_stream, project_id, payload)
                    .await;
            }
            FrameKind::ProjectReadFile => {
                let payload: ProjectReadFilePayload = envelope
                    .parse_payload()
                    .expect("invalid project_read_file payload");
                assert!(
                    !payload.path.root.0.trim().is_empty(),
                    "project_read_file root must not be empty"
                );
                assert!(
                    !payload.path.relative_path.trim().is_empty(),
                    "project_read_file relative_path must not be empty"
                );
                host.read_project_file(&project_output_stream, project_id, payload)
                    .await;
            }
            FrameKind::ProjectReadDiff => {
                let payload: ProjectReadDiffPayload = envelope
                    .parse_payload()
                    .expect("invalid project_read_diff payload");
                assert!(
                    !payload.root.0.trim().is_empty(),
                    "project_read_diff root must not be empty"
                );
                if let Some(path) = &payload.path {
                    assert!(
                        !path.trim().is_empty(),
                        "project_read_diff path must not be empty when provided"
                    );
                }
                host.read_project_diff(&project_output_stream, project_id, payload)
                    .await;
            }
            FrameKind::ProjectStageFile => {
                let payload: ProjectStageFilePayload = envelope
                    .parse_payload()
                    .expect("invalid project_stage_file payload");
                assert!(
                    !payload.path.root.0.trim().is_empty(),
                    "project_stage_file root must not be empty"
                );
                assert!(
                    !payload.path.relative_path.trim().is_empty(),
                    "project_stage_file relative_path must not be empty"
                );
                host.stage_project_file(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await;
            }
            FrameKind::ProjectStageHunk => {
                let payload: ProjectStageHunkPayload = envelope
                    .parse_payload()
                    .expect("invalid project_stage_hunk payload");
                assert!(
                    !payload.path.root.0.trim().is_empty(),
                    "project_stage_hunk root must not be empty"
                );
                assert!(
                    !payload.path.relative_path.trim().is_empty(),
                    "project_stage_hunk relative_path must not be empty"
                );
                assert!(
                    !payload.hunk_id.trim().is_empty(),
                    "project_stage_hunk hunk_id must not be empty"
                );
                host.stage_project_hunk(
                    connection_host_stream,
                    project_output_stream,
                    project_id,
                    payload,
                )
                .await;
            }
            other => {
                panic!(
                    "protocol violation: unexpected client frame kind {} on project stream {}",
                    other, envelope.stream
                );
            }
        }
        return;
    }

    if envelope.stream.0.starts_with("/browse/") {
        let stream_path = envelope.stream.clone();
        match envelope.kind {
            FrameKind::HostBrowseList => {
                let payload: HostBrowseListPayload = envelope
                    .parse_payload()
                    .expect("invalid host_browse_list payload");
                assert!(
                    !payload.path.0.trim().is_empty(),
                    "host_browse_list path must not be empty"
                );
                host.list_browse_dir(connection_host_stream, &stream_path, payload)
                    .await;
            }
            FrameKind::HostBrowseClose => {
                let _: HostBrowseClosePayload = envelope
                    .parse_payload()
                    .expect("invalid host_browse_close payload");
                host.close_browse_stream(connection_host_stream, &stream_path)
                    .await;
            }
            other => {
                panic!(
                    "protocol violation: unexpected client frame kind {} on browse stream {}",
                    other, envelope.stream
                );
            }
        }
        return;
    }

    panic!(
        "protocol violation: unknown stream {} from client",
        envelope.stream
    );
}

fn parse_agent_id(stream: &StreamPath) -> AgentId {
    let segments: Vec<&str> = stream.0.split('/').collect();
    assert_eq!(
        segments.len(),
        4,
        "agent stream must have format /agent/<agent_id>/<instance_id>, got {}",
        stream
    );
    assert!(
        segments.first() == Some(&""),
        "agent stream must be absolute path, got {}",
        stream
    );
    assert_eq!(
        segments[1], "agent",
        "send_message must target /agent/<agent_id>/<instance_id>, got {}",
        stream
    );

    Uuid::parse_str(segments[2]).unwrap_or_else(|err| {
        panic!(
            "agent stream contains invalid agent_id UUID {} in {}: {}",
            segments[2], stream, err
        )
    });
    Uuid::parse_str(segments[3]).unwrap_or_else(|err| {
        panic!(
            "agent stream contains invalid instance_id UUID {} in {}: {}",
            segments[3], stream, err
        )
    });

    AgentId(segments[2].to_owned())
}

fn parse_project_id(stream: &StreamPath) -> ProjectId {
    let segments: Vec<&str> = stream.0.split('/').collect();
    assert_eq!(
        segments.len(),
        3,
        "project stream must have format /project/<project_id>, got {}",
        stream
    );
    assert!(
        segments.first() == Some(&""),
        "project stream must be absolute path, got {}",
        stream
    );
    assert_eq!(
        segments[1], "project",
        "expected /project/<project_id> stream, got {}",
        stream
    );

    Uuid::parse_str(segments[2]).unwrap_or_else(|err| {
        panic!(
            "project stream contains invalid project_id UUID {} in {}: {}",
            segments[2], stream, err
        )
    });

    ProjectId(segments[2].to_owned())
}

fn parse_terminal_id(stream: &StreamPath) -> TerminalId {
    let segments: Vec<&str> = stream.0.split('/').collect();
    assert_eq!(
        segments.len(),
        3,
        "terminal stream must have format /terminal/<terminal_id>, got {}",
        stream
    );
    assert!(
        segments.first() == Some(&""),
        "terminal stream must be absolute path, got {}",
        stream
    );
    assert_eq!(
        segments[1], "terminal",
        "expected /terminal/<terminal_id> stream, got {}",
        stream
    );

    Uuid::parse_str(segments[2]).unwrap_or_else(|err| {
        panic!(
            "terminal stream contains invalid terminal_id UUID {} in {}: {}",
            segments[2], stream, err
        )
    });

    TerminalId(segments[2].to_owned())
}

async fn send_agent_not_running_error(stream: Stream, agent_id: AgentId) {
    let payload = AgentErrorPayload {
        agent_id,
        code: AgentErrorCode::Internal,
        message: "agent not running".to_owned(),
        fatal: false,
    };
    let payload = serde_json::to_value(&payload)
        .expect("failed to serialize AgentError payload for stream error emission");
    let _ = stream.send_value(FrameKind::AgentError, payload).await;
}

fn validate_project_roots(roots: &[String]) {
    assert!(
        !roots.is_empty(),
        "project_create requires at least one root"
    );
    assert!(
        roots.iter().all(|root| !root.trim().is_empty()),
        "project roots must not contain empty values"
    );

    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let inserted = seen.insert(root.as_str());
        assert!(inserted, "project roots must be unique: {}", root);
    }
}
