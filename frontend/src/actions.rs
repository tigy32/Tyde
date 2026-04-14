use leptos::prelude::GetUntracked;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::AppState;

use protocol::{
    FrameKind, ProjectPath, ProjectReadFilePayload, ProjectRootPath, SpawnAgentParams,
    SpawnAgentPayload, StreamPath,
};

/// Spawn a new chat agent for the currently active project.
/// Requires an active project with workspace roots, a host connection, and a host stream.
pub fn spawn_new_chat(state: &AppState) {
    let host_id = match state.host_id.get_untracked() {
        Some(id) => id,
        None => {
            log::error!("spawn_new_chat: not connected");
            return;
        }
    };
    let host_stream = match state.host_stream.get_untracked() {
        Some(s) => s,
        None => {
            log::error!("spawn_new_chat: no host stream");
            return;
        }
    };
    let project = match state.active_project_id.get_untracked() {
        Some(pid) => {
            let projects = state.projects.get_untracked();
            match projects.into_iter().find(|p| p.id == pid) {
                Some(p) => p,
                None => {
                    log::error!("spawn_new_chat: active project not found");
                    return;
                }
            }
        }
        None => {
            log::warn!("spawn_new_chat: no active project — select a project first");
            return;
        }
    };

    let roots = project.roots.clone();
    if roots.is_empty() {
        log::error!("spawn_new_chat: project has no workspace roots");
        return;
    }

    let backend_kind = match state.host_settings.get_untracked() {
        Some(settings) => settings.default_backend,
        None => {
            log::error!("spawn_new_chat: host settings not loaded");
            return;
        }
    };
    let project_id = Some(project.id);
    let name = format!("Chat");

    spawn_local(async move {
        let payload = SpawnAgentPayload {
            name,
            parent_agent_id: None,
            project_id,
            params: SpawnAgentParams::New {
                workspace_roots: roots,
                prompt: None,
                backend_kind,
                cost_hint: None,
            },
        };
        if let Err(e) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await {
            log::error!("failed to send SpawnAgent: {e}");
        }
    });
}

/// Request file contents from the server for the given relative path in the active project.
pub fn open_file(state: &AppState, relative_path: &str) {
    let host_id = match state.host_id.get_untracked() {
        Some(id) => id,
        None => {
            log::error!("open_file: not connected");
            return;
        }
    };
    let project_id = match state.active_project_id.get_untracked() {
        Some(pid) => pid,
        None => {
            log::error!("open_file: no active project");
            return;
        }
    };
    let root = {
        let projects = state.projects.get_untracked();
        match projects.iter().find(|p| p.id == project_id) {
            Some(p) => match p.roots.first() {
                Some(r) => ProjectRootPath(r.clone()),
                None => {
                    log::error!("open_file: project has no roots");
                    return;
                }
            },
            None => {
                log::error!("open_file: project not found");
                return;
            }
        }
    };

    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = ProjectReadFilePayload {
        path: ProjectPath {
            root,
            relative_path: relative_path.to_owned(),
        },
    };

    spawn_local(async move {
        if let Err(e) = send_frame(&host_id, stream, FrameKind::ProjectReadFile, &payload).await {
            log::error!("failed to send ProjectReadFile: {e}");
        }
    });
}
