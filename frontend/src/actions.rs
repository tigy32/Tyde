use leptos::prelude::{GetUntracked, Set};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, CenterView};

use protocol::{
    BackendKind, FrameKind, ImageData, ProjectPath, ProjectReadFilePayload, ProjectRootPath,
    SpawnAgentParams, SpawnAgentPayload, StreamPath,
};

pub fn begin_new_chat(state: &AppState, backend_override: Option<BackendKind>) {
    state.active_agent.set(None);
    state.draft_backend_override.set(backend_override);
    state.center_view.set(CenterView::Chat);
}

pub fn resolve_backend(state: &AppState, host_id: &str) -> Option<BackendKind> {
    let draft = state.draft_backend_override.get_untracked();
    draft.or_else(|| {
        state
            .host_settings_by_host
            .get_untracked()
            .get(host_id)
            .and_then(|settings| {
                settings
                    .default_backend
                    .or_else(|| settings.enabled_backends.first().copied())
            })
    })
}

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

    let active_project = state.active_project_ref_untracked();
    let (host_id, host_stream, project_id, roots) = match active_project {
        Some(active_project) => {
            let Some(project) = state.active_project_info_untracked() else {
                log::error!("spawn_new_chat: active project not found");
                return;
            };
            let Some(host_stream) = state.host_stream_untracked(&active_project.host_id) else {
                log::error!("spawn_new_chat: host stream missing for active project host");
                return;
            };
            (
                active_project.host_id,
                host_stream,
                Some(project.project.id),
                project.project.roots,
            )
        }
        None => match state.selected_host_stream_untracked() {
            Some((host_id, host_stream)) => (host_id, host_stream, None, Vec::new()),
            None => {
                log::error!("spawn_new_chat: no selected connected host");
                return;
            }
        },
    };

    let backend_kind = match resolve_backend(state, &host_id) {
        Some(kind) => kind,
        None => {
            log::error!("spawn_new_chat: no backend available — enable one in settings");
            return;
        }
    };

    state.draft_backend_override.set(None);
    state.agent_initializing.set(true);

    spawn_local(async move {
        let payload = SpawnAgentPayload {
            name: "Chat".to_string(),
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
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
        {
            log::error!("failed to send SpawnAgent: {error}");
        }
    });
}

pub fn open_file(state: &AppState, relative_path: &str) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("open_file: no active project");
        return;
    };
    let Some(project) = state.active_project_info_untracked() else {
        log::error!("open_file: active project not found");
        return;
    };
    let Some(_host_stream) = state.host_stream_untracked(&active_project.host_id) else {
        log::error!("open_file: host stream missing");
        return;
    };
    let Some(root) = project.project.roots.first().cloned() else {
        log::error!("open_file: project has no roots");
        return;
    };

    let payload = ProjectReadFilePayload {
        path: ProjectPath {
            root: ProjectRootPath(root),
            relative_path: relative_path.to_owned(),
        },
    };
    let project_stream = StreamPath(format!("/project/{}", active_project.project_id.0));

    spawn_local(async move {
        if let Err(error) = send_frame(
            &active_project.host_id,
            project_stream,
            FrameKind::ProjectReadFile,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectReadFile: {error}");
        }
    });
}
