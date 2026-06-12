use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{
    ActiveProjectRef, AppState, DockVisibility, LeftTab, PendingWorkbenchCreate, TabContent,
    sort_project_infos,
};

use protocol::{
    AgentId, BackendKind, CustomAgentId, FrameKind, GitBranchName, ImageData, ProjectDeletePayload,
    ProjectDeleteRootPayload, ProjectId, ProjectPath, ProjectReadFilePayload, ProjectRenamePayload,
    ProjectReorderPayload, ProjectReorderScope, ProjectRootPath, ProjectSearchCancelPayload,
    ProjectSearchPayload, SessionId, SessionSettingsValues, SetSessionSettingsPayload,
    SpawnAgentParams, SpawnAgentPayload, StreamPath, WorkbenchCreatePayload, WorkbenchRemovePayload,
};

/// Resume a session on the given host. Synchronously switches the active
/// project context (so the resulting `NewAgent` event lands in the user's
/// current view, upgrading the fresh "New Chat" tab into the resumed chat)
/// and then sends the `SpawnAgent::Resume` frame. Sessions without a
/// `project_id` drop the user to the global/home view.
///
/// Shared by `SessionsPanel` and by team manager/report opens so the
/// project-switch step never gets skipped.
pub fn resume_session(
    state: &AppState,
    host_id: String,
    session_id: SessionId,
    project_id: Option<ProjectId>,
) {
    let target_project = project_id.map(|pid| ActiveProjectRef {
        host_id: host_id.clone(),
        project_id: pid,
    });
    state.switch_active_project(target_project);
    let state = state.clone();
    spawn_local(async move {
        let Some(host_stream) = state.host_stream_untracked(&host_id) else {
            log::error!("resume_session: host stream missing for {host_id}");
            return;
        };
        let payload = SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        };
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
        {
            log::error!("failed to send SpawnAgent (resume): {error}");
        }
    });
}

pub fn begin_new_chat(state: &AppState, backend_override: Option<BackendKind>) {
    begin_new_chat_with(state, backend_override, None);
}

pub fn begin_new_chat_with(
    state: &AppState,
    backend_override: Option<BackendKind>,
    custom_agent_id: Option<CustomAgentId>,
) {
    state.draft_backend_override.set(backend_override);
    state.draft_custom_agent_id.set(custom_agent_id);
    state
        .draft_session_settings
        .set(SessionSettingsValues::default());
    // Opening (and activating) the new chat tab drives `active_agent` to None
    // via the Memo derived from `center_zone`.
    state.open_tab(TabContent::empty_chat(), "New Chat".to_string(), true);
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
                Some(project.project.id.clone()),
                project
                    .project
                    .root_paths()
                    .into_iter()
                    .map(|root| root.0)
                    .collect::<Vec<String>>(),
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
                access_mode: Default::default(),
                session_settings,
            },
        };
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
        {
            log::error!("failed to send SpawnAgent: {error}");
        }
    });
}

/// Build the spawn payload for a BTW / side-question fork. Kept pure (no
/// signals, no IO) so the payload shape can be asserted directly in tests.
///
/// A side question is owned by the current agent (`parent_agent_id`) and
/// forks that agent's backend session (`from_session_id`) without mutating
/// it. `access_mode` is left `None` so the server applies its read-only
/// default for forks (see `dev-docs/23-side-questions.md`).
pub fn fork_payload(
    parent_agent_id: AgentId,
    from_session_id: SessionId,
    project_id: Option<ProjectId>,
    prompt: String,
    images: Option<Vec<ImageData>>,
) -> SpawnAgentPayload {
    SpawnAgentPayload {
        name: None,
        custom_agent_id: None,
        parent_agent_id: Some(parent_agent_id),
        project_id,
        params: SpawnAgentParams::Fork {
            from_session_id,
            prompt,
            images,
            access_mode: None,
        },
    }
}

/// Spawn a BTW / side-question fork from the currently active agent. The
/// child is a first-class interactive agent (`AgentOrigin::SideQuestion`)
/// whose backend session forks the parent's, so the parent transcript is
/// left untouched. No-ops (with a logged reason) when there is no active
/// agent or when its backend session id hasn't been reported yet.
pub fn spawn_side_question(state: &AppState, prompt: String, images: Option<Vec<ImageData>>) {
    let prompt = prompt.trim().to_owned();
    if prompt.is_empty() && images.as_ref().is_none_or(|images| images.is_empty()) {
        log::error!("spawn_side_question: prompt or images required");
        return;
    }

    let Some(active_agent) = state.active_agent.get_untracked() else {
        log::error!("spawn_side_question: no active agent to fork from");
        return;
    };

    let agent_info = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .cloned()
    });
    let Some(agent_info) = agent_info else {
        log::error!("spawn_side_question: active agent not found in registry");
        return;
    };

    let Some(from_session_id) = agent_info.session_id.clone() else {
        log::error!(
            "spawn_side_question: active agent {} has no session id yet; cannot fork",
            agent_info.agent_id
        );
        return;
    };

    let Some(host_stream) = state.host_stream_untracked(&active_agent.host_id) else {
        log::error!("spawn_side_question: host stream missing for active agent host");
        return;
    };

    let host_id = active_agent.host_id;
    let payload = fork_payload(
        agent_info.agent_id.clone(),
        from_session_id,
        agent_info.project_id.clone(),
        prompt,
        images,
    );

    spawn_local(async move {
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
        {
            log::error!("failed to send SpawnAgent (side question fork): {error}");
        }
    });
}

pub fn open_file(state: &AppState, path: ProjectPath) {
    open_project_path(state, path);
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

    let perf_key = format!("file:{}", path.relative_path);
    crate::perf::mark_start(&perf_key);
    crate::perf::log_phase("file_open", "click", &perf_key, "");

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

/// Issue a project-wide search using the current `search_state` parameters.
/// Assigns a fresh `search_id`, clears the previous results, and streams the
/// request to the active project. An empty (whitespace-only) query clears the
/// results and sends nothing.
pub fn start_project_search(state: &AppState) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("start_project_search: no active project");
        return;
    };
    // An empty query clears the results and cancels any still-running walk on
    // the server (the previous `search_id`), rather than leaving it churning.
    if state.search_state.with_untracked(|s| s.query.trim().is_empty()) {
        cancel_project_search(state);
        state.search_state.update(|s| {
            s.results.clear();
            s.total_files = 0;
            s.total_matches = 0;
            s.truncated = false;
            s.error = None;
        });
        return;
    }

    let project_stream = StreamPath(format!("/project/{}", active_project.project_id.0));
    let host_id = active_project.host_id.clone();

    let mut payload: Option<ProjectSearchPayload> = None;
    state.search_state.update(|s| {
        let new_id = s.active_search_id.wrapping_add(1).max(1);
        s.active_search_id = new_id;
        s.results.clear();
        s.total_files = 0;
        s.total_matches = 0;
        s.truncated = false;
        s.error = None;
        s.in_flight = true;
        payload = Some(ProjectSearchPayload {
            search_id: new_id,
            query: s.query.clone(),
            case_sensitive: s.case_sensitive,
            whole_word: s.whole_word,
            use_regex: s.use_regex,
            include_ignored: s.include_ignored,
            roots: s.roots.clone(),
            path_prefix: s.path_prefix.clone(),
            max_results: None,
        });
    });

    let Some(payload) = payload else {
        return;
    };

    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, project_stream, FrameKind::ProjectSearch, &payload).await
        {
            log::error!("failed to send ProjectSearch: {error}");
        }
    });
}

/// Cancel the in-flight project search (if any) for the active project.
///
/// Bumps `active_search_id` to a fresh tombstone id *before* sending the
/// cancel for the old id, so any result frames still in flight from the
/// cancelled walk no longer match the active id and are dropped by dispatch
/// instead of being appended after the UI was cleared.
pub fn cancel_project_search(state: &AppState) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let cancelled_id = state.search_state.with_untracked(|s| s.active_search_id);
    if cancelled_id == 0 {
        return;
    }
    state.search_state.update(|s| {
        // Advance the active id so the cancelled search's late frames are
        // ignored; the next real search advances it again.
        s.active_search_id = s.active_search_id.wrapping_add(1).max(1);
        s.in_flight = false;
    });
    let project_stream = StreamPath(format!("/project/{}", active_project.project_id.0));
    let host_id = active_project.host_id.clone();
    let payload = ProjectSearchCancelPayload {
        search_id: cancelled_id,
    };
    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, project_stream, FrameKind::ProjectSearchCancel, &payload).await
        {
            log::error!("failed to send ProjectSearchCancel: {error}");
        }
    });
}

/// Reveal and focus the Search panel in the left dock (Cmd/Ctrl+Shift+F).
pub fn open_search_panel(state: &AppState) {
    state.left_dock.set(DockVisibility::Visible);
    state.left_tab.set(LeftTab::Search);
    state
        .search_focus_seq
        .update(|seq| *seq = seq.wrapping_add(1));
}

/// Scope the Search panel to a folder and reveal it. Prefills the root + path
/// prefix; re-runs the search immediately if a query is already present.
pub fn search_in_folder(state: &AppState, root: ProjectRootPath, relative_path: String) {
    state.search_state.update(|s| {
        s.path_prefix = Some(relative_path);
        s.roots = vec![root];
    });
    open_search_panel(state);
    if state
        .search_state
        .with_untracked(|s| !s.query.trim().is_empty())
    {
        start_project_search(state);
    }
}

pub fn rename_project(state: &AppState, host_id: String, project_id: ProjectId, name: String) {
    let name = name.trim().to_owned();
    if name.is_empty() {
        log::error!("rename_project: name must not be empty");
        return;
    }
    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
        log::error!("rename_project: host stream missing for {host_id}");
        return;
    };
    let payload = ProjectRenamePayload {
        id: project_id,
        name,
    };
    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, host_stream, FrameKind::ProjectRename, &payload).await
        {
            log::error!("failed to send ProjectRename: {error}");
        }
    });
}

pub fn delete_project_root(
    state: &AppState,
    host_id: String,
    project_id: ProjectId,
    root: ProjectRootPath,
) {
    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
        log::error!("delete_project_root: host stream missing for {host_id}");
        return;
    };
    let payload = ProjectDeleteRootPayload {
        id: project_id,
        root,
    };
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            host_stream,
            FrameKind::ProjectDeleteRoot,
            &payload,
        )
        .await
        {
            log::error!("failed to send ProjectDeleteRoot: {error}");
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

    let host_projects: Vec<_> = state
        .projects
        .get_untracked()
        .into_iter()
        .filter(|project| project.host_id == host_id.as_str())
        .collect();

    let Some(dragged) = host_projects
        .iter()
        .find(|project| project.project.id == dragged_project_id)
    else {
        log::error!(
            "reorder_projects: dragged project {} not found",
            dragged_project_id
        );
        return;
    };

    // Reorder is scoped: dragging a top-level project reorders top-level only;
    // dragging a workbench reorders that parent's children only. Cross-scope
    // drags are rejected — the protocol does not represent moving a workbench
    // out from under its parent.
    let scope = match dragged.project.parent_project_id().cloned() {
        Some(parent_project_id) => ProjectReorderScope::WorkbenchChildren { parent_project_id },
        None => ProjectReorderScope::TopLevel,
    };

    let current_ids: Vec<ProjectId> = host_projects
        .iter()
        .filter(|project| match &scope {
            ProjectReorderScope::TopLevel => !project.project.is_workbench(),
            ProjectReorderScope::WorkbenchChildren { parent_project_id } => {
                project.project.parent_project_id() == Some(parent_project_id)
            }
        })
        .map(|project| project.project.id.clone())
        .collect();

    let Some(dragged_index) = current_ids.iter().position(|id| *id == dragged_project_id) else {
        log::error!(
            "reorder_projects: dragged project {} not found in scope",
            dragged_project_id
        );
        return;
    };
    let Some(target_index) = current_ids.iter().position(|id| *id == target_project_id) else {
        log::error!(
            "reorder_projects: target project {} not found in scope",
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

    let scope_for_update = scope.clone();
    state.projects.update(|projects| {
        for project in projects.iter_mut() {
            if project.host_id != host_id.as_str() {
                continue;
            }
            let in_scope = match &scope_for_update {
                ProjectReorderScope::TopLevel => !project.project.is_workbench(),
                ProjectReorderScope::WorkbenchChildren { parent_project_id } => {
                    project.project.parent_project_id() == Some(parent_project_id)
                }
            };
            if !in_scope {
                continue;
            }
            if let Some(index) = reordered_ids
                .iter()
                .position(|id| *id == project.project.id)
            {
                project.project.sort_order = index as u64;
            }
        }
        sort_project_infos(projects);
    });

    let payload = ProjectReorderPayload {
        scope,
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

pub fn create_workbench(
    state: &AppState,
    host_id: String,
    parent_project_id: ProjectId,
    branch: String,
) {
    let trimmed = branch.trim().to_owned();
    if trimmed.is_empty() {
        log::error!("create_workbench: branch must not be empty");
        return;
    }
    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
        log::error!("create_workbench: host stream missing for {host_id}");
        return;
    };
    let branch_name = GitBranchName(trimmed.clone());
    // Record the pending create so dispatch can correlate the resulting
    // `ProjectNotify::Upsert` (per §3.3) and switch active to the new
    // workbench. Purge stale entries while we're here so an old orphaned
    // create can never trigger a spurious switch.
    let now = crate::state::now_ms();
    state.pending_workbench_creates.update(|pending| {
        pending.retain(|entry| !entry.is_stale(now));
        pending.push(PendingWorkbenchCreate {
            host_id: host_id.clone(),
            parent_project_id: parent_project_id.clone(),
            branch: branch_name.clone(),
            requested_at_ms: now,
            error: None,
        });
    });
    let payload = WorkbenchCreatePayload {
        parent_project_id: parent_project_id.clone(),
        branch: branch_name.clone(),
        name: trimmed,
    };
    let state = state.clone();
    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, host_stream, FrameKind::WorkbenchCreate, &payload).await
        {
            log::error!("failed to send WorkbenchCreate: {error}");
            // The request never reached the host: drop the pending entry so
            // it can't match a later Upsert and cause a spurious
            // active-project switch. The create modal notices the entry
            // vanishing (without a matching workbench appearing) and shows a
            // generic failure.
            state.pending_workbench_creates.update(|pending| {
                if let Some(idx) = pending.iter().position(|entry| {
                    entry.host_id == host_id
                        && entry.parent_project_id == parent_project_id
                        && entry.branch == branch_name
                        && entry.error.is_none()
                }) {
                    pending.remove(idx);
                }
            });
        }
    });
}

pub fn remove_workbench(state: &AppState, host_id: String, workbench_id: ProjectId) {
    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
        log::error!("remove_workbench: host stream missing for {host_id}");
        return;
    };
    let payload = WorkbenchRemovePayload { id: workbench_id };
    spawn_local(async move {
        if let Err(error) =
            send_frame(&host_id, host_stream, FrameKind::WorkbenchRemove, &payload).await
        {
            log::error!("failed to send WorkbenchRemove: {error}");
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// A BTW fork must be owned by the parent agent, fork the parent's
    /// backend session, and leave `access_mode` unset so the server applies
    /// its read-only default — otherwise a side question could mutate the
    /// workspace or land on the wrong session.
    #[wasm_bindgen_test]
    fn fork_payload_targets_parent_and_source_session_read_only() {
        let payload = fork_payload(
            AgentId("agent-parent".to_owned()),
            SessionId("session-parent".to_owned()),
            Some(ProjectId("proj-1".to_owned())),
            "why is this slow?".to_owned(),
            None,
        );

        assert_eq!(
            payload.parent_agent_id,
            Some(AgentId("agent-parent".to_owned()))
        );
        assert_eq!(payload.project_id, Some(ProjectId("proj-1".to_owned())));
        assert!(payload.custom_agent_id.is_none());

        match payload.params {
            SpawnAgentParams::Fork {
                from_session_id,
                prompt,
                images,
                access_mode,
            } => {
                assert_eq!(from_session_id, SessionId("session-parent".to_owned()));
                assert_eq!(prompt, "why is this slow?");
                assert!(images.is_none());
                // Unset → server's read-only fork default applies.
                assert!(access_mode.is_none());
            }
            other => panic!("expected Fork params, got {other:?}"),
        }
    }
}
