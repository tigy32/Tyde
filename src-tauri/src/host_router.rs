use std::sync::Arc;

use tauri::AppHandle;

use crate::host::{Host, RemoteKind};
use crate::remote::{connect_tyde_server_with_progress, parse_remote_path};
use crate::tyde_server_conn::{ConnectionState, TydeServerConnection};
use crate::AppState;

pub(crate) enum WorkspaceRoute {
    Local,
    TydeServer {
        connection: Arc<TydeServerConnection>,
    },
}

pub(crate) enum AgentRoute {
    Local,
    TydeServer {
        connection: Arc<TydeServerConnection>,
    },
}

pub(crate) fn resolve_host_for_roots(
    state: &AppState,
    workspace_roots: &[String],
) -> Result<Host, String> {
    let store = state.host_store.lock();
    let first_root = workspace_roots.first().map(|s| s.as_str()).unwrap_or("");
    if let Some(remote) = parse_remote_path(first_root) {
        for h in store.list() {
            if !h.is_local && h.hostname == remote.host {
                return Ok(h);
            }
        }
        return Err(format!(
            "Remote host '{}' is not registered. Open Settings → Hosts to add it.",
            remote.host
        ));
    }

    store
        .get("local")
        .cloned()
        .ok_or_else(|| "Local host not found in host store".to_string())
}

/// Convert `ssh://user@host/path` roots to local `/path` for the remote server,
/// since the server runs on the remote machine where these are local paths.
pub(crate) fn strip_ssh_roots(roots: &[String]) -> Vec<String> {
    roots
        .iter()
        .map(|root| {
            parse_remote_path(root)
                .map(|remote| remote.path)
                .unwrap_or_else(|| root.clone())
        })
        .collect()
}

pub(crate) async fn route_workspace(
    app: &AppHandle,
    state: &AppState,
    workspace_roots: &[String],
) -> Result<WorkspaceRoute, String> {
    let host = resolve_host_for_roots(state, workspace_roots)?;
    if host.remote_kind == RemoteKind::TydeServer {
        let connection = get_or_create_server_connection(app, state, &host).await?;
        return Ok(WorkspaceRoute::TydeServer { connection });
    }

    Ok(WorkspaceRoute::Local)
}

pub(crate) async fn route_agent(state: &AppState, agent_id: &str) -> Result<AgentRoute, String> {
    // First check local runtime — if it knows the agent, it's local.
    let has_local = {
        let runtime = state.agent_runtime.lock().await;
        runtime.get_agent(agent_id).is_some()
    };

    // Check each TydeServer connection for ownership of this agent.
    let all_conns: Vec<Arc<TydeServerConnection>> = {
        let conns = state.tyde_server_connections.lock();
        conns.values().cloned().collect()
    };
    let mut owners = Vec::new();
    for conn in &all_conns {
        if conn.owns_agent(agent_id).await {
            owners.push(conn.clone());
        }
    }

    // Ownership maps are event-driven; if we don't currently have an owner,
    // refresh from each remote host's authoritative list.
    if !has_local && owners.is_empty() {
        for conn in &all_conns {
            match conn.fetch_remote_agents().await {
                Ok(agents) => {
                    if agents.iter().any(|a| a.agent_id == agent_id) {
                        owners.push(conn.clone());
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "Failed to refresh remote agents for host {} while routing agent {}: {}",
                        conn.host_id,
                        agent_id,
                        err
                    );
                }
            }
        }
    }

    // If we found owners from cached state, verify they still own the agent
    // using an authoritative list refresh to avoid stale ownership after
    // remote termination.
    if !has_local && !owners.is_empty() {
        let mut verified = Vec::new();
        for conn in &owners {
            match conn.fetch_remote_agents().await {
                Ok(agents) => {
                    if agents.iter().any(|a| a.agent_id == agent_id) {
                        verified.push(conn.clone());
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "Failed to verify remote ownership for host {} and agent {}: {}",
                        conn.host_id,
                        agent_id,
                        err
                    );
                }
            }
        }
        owners = verified;
    }

    if has_local && !owners.is_empty() {
        return Err(format!(
            "Agent {agent_id} exists in both local and remote runtimes (ambiguous owner)"
        ));
    }
    if has_local {
        return Ok(AgentRoute::Local);
    }
    if owners.len() > 1 {
        return Err(format!(
            "Agent {agent_id} exists on multiple remote hosts (ambiguous owner)"
        ));
    }

    match owners.into_iter().next() {
        Some(connection) => Ok(AgentRoute::TydeServer { connection }),
        None => Err(format!("Agent {agent_id} not found in any runtime")),
    }
}

pub(crate) async fn get_server_connection_by_id(
    app: &AppHandle,
    state: &AppState,
    host_id: &str,
) -> Result<Arc<TydeServerConnection>, String> {
    if host_id == "local" {
        return Err("Cannot get server connection for local host".to_string());
    }
    let host = state
        .host_store
        .lock()
        .get(host_id)
        .cloned()
        .ok_or_else(|| format!("Host ID '{host_id}' not found"))?;
    get_or_create_server_connection(app, state, &host).await
}

pub(crate) async fn get_or_create_server_connection(
    app: &AppHandle,
    state: &AppState,
    host: &Host,
) -> Result<Arc<TydeServerConnection>, String> {
    // Check for existing healthy connection.
    let existing = {
        let conns = state.tyde_server_connections.lock();
        conns.get(&host.id).cloned()
    };
    if let Some(conn) = existing {
        match conn.connection_state().await {
            ConnectionState::Connected | ConnectionState::Reconnecting { .. } => {
                return Ok(conn);
            }
            _ => {
                // Stale/disconnected — remove and reconnect.
                state.tyde_server_connections.lock().remove(&host.id);
            }
        }
    }

    // Validate the remote and resolve/verify the Tyde remote socket path.
    let remote_socket_path = connect_tyde_server_with_progress(app, &host.hostname).await?;

    // Create new connection.
    let conn = TydeServerConnection::connect(
        app.clone(),
        host.id.clone(),
        host.hostname.clone(),
        remote_socket_path,
    )
    .await?;

    state
        .tyde_server_connections
        .lock()
        .insert(host.id.clone(), conn.clone());

    Ok(conn)
}
