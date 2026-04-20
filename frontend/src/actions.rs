use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, TabContent, sort_project_infos};

use protocol::{
    BackendKind, CustomAgentId, FrameKind, ImageData, ProjectDeletePayload, ProjectId, ProjectPath,
    ProjectReadFilePayload, ProjectReorderPayload, ProjectRootPath, SessionSettingsValues,
    SetSessionSettingsPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
};

pub fn begin_new_chat(state: &AppState, backend_override: Option<BackendKind>) {
    begin_new_chat_with(state, backend_override, None);
}

pub fn begin_new_chat_with(
    state: &AppState,
    backend_override: Option<BackendKind>,
    custom_agent_id: Option<CustomAgentId>,
) {
    state.active_agent.set(None);
    state.draft_backend_override.set(backend_override);
    state.draft_custom_agent_id.set(custom_agent_id);
    state
        .draft_session_settings
        .set(SessionSettingsValues::default());
    state.open_tab(
        TabContent::Chat { agent_ref: None },
        "New Chat".to_string(),
        true,
    );
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

    let draft_settings = state.draft_session_settings.get_untracked();
    let session_settings = if draft_settings.0.is_empty() {
        None
    } else {
        Some(draft_settings)
    };

    let custom_agent_id = state.draft_custom_agent_id.get_untracked();

    state.draft_backend_override.set(None);
    state.draft_custom_agent_id.set(None);
    state
        .draft_session_settings
        .set(SessionSettingsValues::default());

    spawn_local(async move {
        let payload = SpawnAgentPayload {
            name: None,
            custom_agent_id,
            parent_agent_id: None,
            project_id,
            params: SpawnAgentParams::New {
                workspace_roots: roots,
                prompt: initial_message,
                images: initial_images,
                backend_kind,
                cost_hint: None,
                session_settings,
            },
        };
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
        {
            log::error!("failed to send SpawnAgent: {error}");
        }
    });
}

pub fn open_file(state: &AppState, relative_path: &str) {
    let Some(project) = state.active_project_info_untracked() else {
        log::error!("open_file: active project not found");
        return;
    };
    let Some(root) = project.project.roots.first().cloned() else {
        log::error!("open_file: project has no roots");
        return;
    };

    open_project_path(
        state,
        ProjectPath {
            root: ProjectRootPath(root),
            relative_path: relative_path.to_owned(),
        },
    );
}

pub fn open_project_path(state: &AppState, path: ProjectPath) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("open_project_path: no active project");
        return;
    };
    let Some(_host_stream) = state.host_stream_untracked(&active_project.host_id) else {
        log::error!("open_project_path: host stream missing");
        return;
    };

    let payload = ProjectReadFilePayload { path };
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

pub fn delete_project(state: &AppState, host_id: String, project_id: ProjectId) {
    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
        log::error!("delete_project: host stream missing for {host_id}");
        return;
    };
    let payload = ProjectDeletePayload { id: project_id };
    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, host_stream, FrameKind::ProjectDelete, &payload).await
        {
            log::error!("failed to send ProjectDelete: {error}");
        }
    });
}

pub fn reorder_projects(
    state: &AppState,
    host_id: String,
    dragged_project_id: ProjectId,
    target_project_id: ProjectId,
    insert_after: bool,
) {
    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
        log::error!("reorder_projects: host stream missing for {host_id}");
        return;
    };

    let current_ids: Vec<_> = state
        .projects
        .get_untracked()
        .into_iter()
        .filter(|project| project.host_id == host_id.as_str())
        .map(|project| project.project.id)
        .collect();

    let Some(dragged_index) = current_ids.iter().position(|id| *id == dragged_project_id) else {
        log::error!(
            "reorder_projects: dragged project {} not found",
            dragged_project_id
        );
        return;
    };
    let Some(target_index) = current_ids.iter().position(|id| *id == target_project_id) else {
        log::error!(
            "reorder_projects: target project {} not found",
            target_project_id
        );
        return;
    };

    if dragged_index == target_index {
        return;
    }

    let mut reordered_ids = current_ids;
    let dragged_id = reordered_ids.remove(dragged_index);
    let Some(mut insert_index) = reordered_ids.iter().position(|id| *id == target_project_id)
    else {
        log::error!(
            "reorder_projects: target project {} disappeared during reorder",
            target_project_id
        );
        return;
    };
    if insert_after {
        insert_index += 1;
    }
    reordered_ids.insert(insert_index, dragged_id);

    state.projects.update(|projects| {
        let mut ordered_projects = Vec::new();
        for project_id in &reordered_ids {
            if let Some(project) = projects
                .iter()
                .find(|project| {
                    project.host_id == host_id.as_str() && project.project.id == *project_id
                })
                .cloned()
            {
                ordered_projects.push(project);
            }
        }

        for (index, project) in ordered_projects.iter_mut().enumerate() {
            project.project.sort_order = index as u64;
        }

        projects.retain(|project| project.host_id != host_id.as_str());
        projects.extend(ordered_projects);
        sort_project_infos(projects);
    });

    let payload = ProjectReorderPayload {
        project_ids: reordered_ids,
    };
    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, host_stream, FrameKind::ProjectReorder, &payload).await
        {
            log::error!("failed to send ProjectReorder: {error}");
        }
    });
}

pub fn send_set_session_settings(state: &AppState, values: SessionSettingsValues) {
    let active_agent = match state.active_agent.get_untracked() {
        Some(agent) => agent,
        None => {
            log::error!("send_set_session_settings: no active agent");
            return;
        }
    };

    let instance_stream = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .map(|a| a.instance_stream.clone())
    });

    let Some(instance_stream) = instance_stream else {
        log::error!("send_set_session_settings: agent not found");
        return;
    };

    let host_id = active_agent.host_id;
    let payload = SetSessionSettingsPayload { values };

    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            instance_stream,
            FrameKind::SetSessionSettings,
            &payload,
        )
        .await
        {
            log::error!("failed to send SetSessionSettings: {error}");
        }
    });
}
