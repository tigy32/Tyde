use leptos::prelude::{GetUntracked, Set};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, CenterView};

use protocol::{
    BackendKind, FrameKind, ImageData, ProjectPath, ProjectReadFilePayload, ProjectRootPath,
    SpawnAgentParams, SpawnAgentPayload, StreamPath,
};

/// Enter draft-chat mode without spawning a backend session yet.
/// If `backend_override` is provided, the first message will use that backend
/// instead of the default.
pub fn begin_new_chat(state: &AppState, backend_override: Option<BackendKind>) {
    state.active_agent_id.set(None);
    state.draft_backend_override.set(backend_override);
    state.center_view.set(CenterView::Chat);
}

/// Resolve which backend to use for a new chat: explicit override first, then
/// the host default, then the first enabled backend.
pub fn resolve_backend(state: &AppState) -> Option<BackendKind> {
    let draft = state.draft_backend_override.get_untracked();
    draft.or_else(|| {
        state.host_settings.get_untracked().and_then(|settings| {
            settings
                .default_backend
                .or_else(|| settings.enabled_backends.first().copied())
        })
    })
}

/// Spawn a new chat agent for the currently active project using the first user message.
pub fn spawn_new_chat(
    state: &AppState,
    initial_message: String,
    initial_images: Option<Vec<ImageData>>,
) {
    let initial_message = initial_message.trim().to_owned();
    if initial_message.is_empty()
        && initial_images
            .as_ref()
            .is_none_or(|images| images.is_empty())
    {
        log::error!("spawn_new_chat: initial input must include text or images");
        return;
    }

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
    let (project_id, roots) = match state.active_project_id.get_untracked() {
        Some(pid) => {
            let projects = state.projects.get_untracked();
            match projects.into_iter().find(|p| p.id == pid) {
                Some(p) => (Some(p.id), p.roots.clone()),
                None => (None, Vec::new()),
            }
        }
        None => (None, Vec::new()),
    };

    let backend_kind = match resolve_backend(state) {
        Some(kind) => kind,
        None => {
            log::error!("spawn_new_chat: no backend available — enable one in settings");
            return;
        }
    };
    // Draft override consumed — clear it.
    state.draft_backend_override.set(None);
    state.agent_initializing.set(true);
    let name = "Chat".to_string();

    spawn_local(async move {
        let payload = SpawnAgentPayload {
            name,
            parent_agent_id: None,
            project_id,
            params: SpawnAgentParams::New {
                workspace_roots: roots,
                prompt: initial_message,
                images: initial_images,
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
