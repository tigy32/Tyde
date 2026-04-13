use crate::agent::AgentInfo;
use crate::backends::types::SessionCommand;
use crate::server_state::ServerState;

pub struct AgentCommandFailure {
    pub error: String,
    pub agent_snapshot: Option<(AgentInfo, Option<u64>)>,
}

pub async fn execute_agent_command(
    server: &ServerState,
    agent_id: &str,
    command: SessionCommand,
) -> Result<(), AgentCommandFailure> {
    let handle = server
        .agent_handle(agent_id)
        .await
        .ok_or_else(|| AgentCommandFailure {
            error: "Agent not found".to_string(),
            agent_snapshot: None,
        })?;

    match handle.execute(command).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(backend) = server.remove_agent(agent_id).await {
                backend.shutdown().await;
            }
            let agent_snapshot = server
                .mark_agent_failed_snapshot(agent_id, error.clone())
                .await;
            server.clear_agent_session(agent_id);
            Err(AgentCommandFailure {
                error,
                agent_snapshot,
            })
        }
    }
}

pub async fn close_agent(
    server: &ServerState,
    agent_id: &str,
) -> Result<Option<(AgentInfo, Option<u64>)>, String> {
    let backend = server
        .remove_agent(agent_id)
        .await
        .ok_or_else(|| "Agent not found".to_string())?;
    backend.shutdown().await;
    Ok(server
        .mark_agent_closed_snapshot(agent_id, Some("Agent closed".to_string()))
        .await)
}

pub async fn resume_session(
    server: &ServerState,
    agent_id: &str,
    session_id: String,
) -> Result<(), AgentCommandFailure> {
    let track_local = {
        let reg = server.agent_registry.lock().await;
        reg.tracks_local_session_store(agent_id)
    };
    let backend_kind = server.agent_backend_kind(agent_id).await;
    let workspace_roots = server
        .agent_workspace_roots(agent_id)
        .await
        .unwrap_or_default();
    let workspace_root = workspace_roots.first().cloned();
    let agent_sessions = server.agent_sessions();
    let mut created_record_id = None::<String>;

    if track_local {
        if let Some(ref backend_kind) = backend_kind {
            created_record_id = agent_sessions
                .bind_resumed_session(
                    agent_id,
                    backend_kind,
                    workspace_root.as_deref(),
                    &workspace_roots,
                    &session_id,
                )
                .map_err(|error| AgentCommandFailure {
                    error,
                    agent_snapshot: None,
                })?
                .created_record_id;
        }
    } else {
        agent_sessions.clear_agent_session(agent_id);
    }

    let result = execute_agent_command(
        server,
        agent_id,
        SessionCommand::ResumeSession {
            session_id: session_id.clone(),
        },
    )
    .await;
    if result.is_ok() {
        return Ok(());
    }

    if track_local {
        agent_sessions.clear_agent_session(agent_id);
    }
    if let Some(record_id) = created_record_id {
        if let Err(err) = agent_sessions.delete_session_record(&record_id) {
            tracing::error!(
                "Failed to clean up failed-resume session record '{}': {err}",
                record_id
            );
        }
    }
    result
}

pub async fn delete_session(
    server: &ServerState,
    agent_id: &str,
    session_id: String,
) -> Result<(), AgentCommandFailure> {
    let backend_kind = server.agent_backend_kind(agent_id).await;
    execute_agent_command(
        server,
        agent_id,
        SessionCommand::DeleteSession {
            session_id: session_id.clone(),
        },
    )
    .await?;
    if let Some(backend_kind) = backend_kind {
        server
            .delete_session_record_by_backend_session(&backend_kind, &session_id)
            .map_err(|error| AgentCommandFailure {
                error,
                agent_snapshot: None,
            })?;
    }
    Ok(())
}

pub async fn execute_admin_command(
    server: &ServerState,
    admin_id: u64,
    command: SessionCommand,
) -> Result<(), String> {
    let handle = server
        .admin_handle(admin_id)
        .await
        .ok_or_else(|| "Admin subprocess not found".to_string())?;
    match handle.execute(command).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(backend) = server.remove_admin_session(admin_id).await {
                backend.shutdown().await;
            }
            Err(error)
        }
    }
}

pub async fn close_admin_session(server: &ServerState, admin_id: u64) -> Result<(), String> {
    let backend = server
        .remove_admin_session(admin_id)
        .await
        .ok_or_else(|| "Admin subprocess not found".to_string())?;
    backend.shutdown().await;
    Ok(())
}

pub async fn delete_admin_session(
    server: &ServerState,
    admin_id: u64,
    session_id: String,
) -> Result<(), String> {
    let backend_kind = server.admin_kind_str(admin_id).await;
    execute_admin_command(
        server,
        admin_id,
        SessionCommand::DeleteSession {
            session_id: session_id.clone(),
        },
    )
    .await?;
    if let Some(backend_kind) = backend_kind {
        server.delete_session_record_by_backend_session(&backend_kind, &session_id)?;
    }
    Ok(())
}
