use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{
    ActiveAgentRef, ActiveProjectRef, AppState, DockVisibility, LeftTab, PendingWorkbenchCreate,
    TabContent, sort_project_infos,
};

use protocol::{
    AgentId, BackendKind, ByteRange, CodeIntelCancelReferencesPayload,
    CodeIntelFindReferencesPayload, CodeIntelHoverPayload, CodeIntelNavigatePayload,
    CodeIntelSetVisibleRangePayload, CodeIntelSubscribeFilePayload, CustomAgentId, FrameKind,
    GitBranchName, ImageData, LaunchProfile, LaunchProfileEntry, ProjectDeletePayload,
    ProjectDeleteRootPayload, ProjectFileVersion, ProjectId, ProjectPath, ProjectReadFilePayload,
    ProjectRenamePayload, ProjectReorderPayload, ProjectReorderScope, ProjectRootPath,
    ProjectSearchCancelPayload, ProjectSearchPayload, SessionId, SessionSettingsValues,
    SetSessionSettingsPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
    WorkbenchCreatePayload, WorkbenchRemovePayload,
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
    if open_existing_session_agent(state, &host_id, &session_id, project_id.clone()) {
        return;
    }

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

fn open_existing_session_agent(
    state: &AppState,
    host_id: &str,
    session_id: &SessionId,
    project_id: Option<ProjectId>,
) -> bool {
    let existing = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| {
                agent.host_id == host_id
                    && agent.backend_kind == BackendKind::Antigravity
                    && agent.session_id.as_ref() == Some(session_id)
            })
            .cloned()
    });
    let Some(agent) = existing else {
        return false;
    };

    let target_project = project_id.map(|pid| ActiveProjectRef {
        host_id: host_id.to_owned(),
        project_id: pid,
    });
    state.switch_active_project(target_project);
    state.open_tab(
        TabContent::chat_with_agent(ActiveAgentRef {
            host_id: host_id.to_owned(),
            agent_id: agent.agent_id,
        }),
        agent.name,
        true,
    );
    true
}

pub fn begin_new_chat(state: &AppState, backend_override: Option<BackendKind>) {
    begin_new_chat_with(state, backend_override, None);
}

/// Begin a new chat from the primary "New Chat" button. If the current chat
/// context host's server-owned catalog names a `default_profile_id` that
/// resolves to an exact ready entry, start from that profile (backend +
/// settings come straight from the server). Otherwise open an ordinary draft
/// with no override, letting the server resolve its own default backend at
/// spawn time. No id parsing, no guessing — only an exact catalog match is
/// used.
pub fn begin_new_chat_default(state: &AppState) {
    if let Some(host_id) = state.chat_context_host_id_untracked() {
        let default_profile = state.launch_profile_catalog.with_untracked(|catalogs| {
            let catalog = catalogs.get(&host_id)?;
            let default_id = catalog.default_profile_id.as_ref()?;
            catalog.entries.iter().find_map(|entry| match entry {
                LaunchProfileEntry::Ready { profile } if &profile.id == default_id => {
                    Some(profile.clone())
                }
                _ => None,
            })
        });
        if let Some(profile) = default_profile {
            begin_new_chat_with_profile(state, profile, None);
            return;
        }
    }
    begin_new_chat(state, None);
}

pub fn begin_new_chat_with(
    state: &AppState,
    backend_override: Option<BackendKind>,
    custom_agent_id: Option<CustomAgentId>,
) {
    state.draft_backend_override.set(backend_override);
    state.draft_custom_agent_id.set(custom_agent_id);
    state.draft_launch_profile_id.set(None);
    state
        .draft_session_settings
        .set(SessionSettingsValues::default());
    state.draft_session_settings_dirty.set(false);
    // Opening (and activating) the new chat tab drives `active_agent` to None
    // via the Memo derived from `center_zone`.
    state.open_tab(TabContent::empty_chat(), "New Chat".to_string(), true);
}

/// Begin a new chat from a server-provided ready launch profile. The profile
/// carries the authoritative backend and session settings, so we set the draft
/// directly from it — never by parsing the id. `custom_agent_id` composes a
/// custom agent on top of the selected profile.
pub fn begin_new_chat_with_profile(
    state: &AppState,
    profile: LaunchProfile,
    custom_agent_id: Option<CustomAgentId>,
) {
    state.draft_backend_override.set(Some(profile.backend_kind));
    state.draft_custom_agent_id.set(custom_agent_id);
    state.draft_launch_profile_id.set(Some(profile.id));
    // Show the profile's settings as the effective draft values, but mark them
    // clean so spawn defers to server-owned profile resolution unless the user
    // edits them.
    state.draft_session_settings.set(profile.session_settings);
    state.draft_session_settings_dirty.set(false);
    state.open_tab(TabContent::empty_chat(), "New Chat".to_string(), true);
}

/// Open a fresh new-chat tab in the given host/project context and pre-fill the
/// composer with an editable `prompt`, in one state transition.
///
/// Used by the Workflows authoring CTA: the active draft (project context +
/// default backend) and the prefilled `chat_input` must be set together so they
/// can never drift apart. This deliberately reuses the ordinary new-chat draft
/// path — no backend is chosen here and no agent is spawned until the user
/// edits and sends. The prompt remains fully editable in the composer.
pub fn open_new_chat_with_prefill(
    state: &AppState,
    host_id: String,
    project_id: Option<ProjectId>,
    prompt: String,
) {
    // Switch to the target project context (or global) so the new chat is
    // created against the same host/project where the workflow file will be
    // saved. A no-op when that context is already active.
    let target_project = project_id.map(|pid| ActiveProjectRef {
        host_id: host_id.clone(),
        project_id: pid,
    });
    state.switch_active_project(target_project);

    // Ordinary new-chat draft: no backend override, no custom agent, default
    // session settings. Backend selection stays on the host/project default.
    state.draft_backend_override.set(None);
    state.draft_custom_agent_id.set(None);
    state.draft_launch_profile_id.set(None);
    state
        .draft_session_settings
        .set(SessionSettingsValues::default());
    state.draft_session_settings_dirty.set(false);

    // Open (and activate) the new chat tab, then seed the composer. The composer
    // mirrors `chat_input` into the textarea reactively, so the prefill appears
    // immediately and the user can edit before sending.
    state.open_tab(TabContent::empty_chat(), "New Chat".to_string(), true);
    state.chat_input.set(prompt);
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
    on_send_error: impl FnOnce(String) + 'static,
) -> bool {
    let initial_message = initial_message.trim().to_owned();
    if initial_message.is_empty()
        && initial_images
            .as_ref()
            .is_none_or(|images| images.is_empty())
    {
        log::error!("spawn_new_chat: initial input must include text or images");
        return false;
    }

    let active_project = state.active_project_ref_untracked();
    let (host_id, host_stream, project_id, roots) = match active_project {
        Some(active_project) => {
            let Some(project) = state.active_project_info_untracked() else {
                log::error!("spawn_new_chat: active project not found");
                return false;
            };
            let Some(host_stream) = state.host_stream_untracked(&active_project.host_id) else {
                log::error!("spawn_new_chat: host stream missing for active project host");
                return false;
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
                return false;
            }
        },
    };

    let backend_kind = match resolve_backend(state, &host_id) {
        Some(kind) => kind,
        None => {
            log::error!("spawn_new_chat: no backend available — enable one in settings");
            return false;
        }
    };

    let custom_agent_id = state.draft_custom_agent_id.get_untracked();
    let launch_profile_id = state.draft_launch_profile_id.get_untracked();

    let draft_settings = state.draft_session_settings.get_untracked();
    let settings_dirty = state.draft_session_settings_dirty.get_untracked();
    // With a launch profile selected and no user edits, defer to server-owned
    // profile resolution rather than echoing a stale copy of the profile's
    // settings back as explicit overrides.
    let defer_to_profile = launch_profile_id.is_some() && !settings_dirty;
    let session_settings = if defer_to_profile || draft_settings.0.is_empty() {
        None
    } else {
        Some(draft_settings)
    };

    state.draft_backend_override.set(None);
    state.draft_custom_agent_id.set(None);
    state.draft_launch_profile_id.set(None);
    state
        .draft_session_settings
        .set(SessionSettingsValues::default());
    state.draft_session_settings_dirty.set(false);

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
                launch_profile_id,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings,
            },
        };
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
        {
            log::error!("failed to send SpawnAgent: {error}");
            on_send_error(error);
        }
    });
    true
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

    send_read_and_subscribe(
        active_project.host_id.clone(),
        active_project.project_id.0.clone(),
        path,
    );
}

/// Re-read an already-open file in the background after the server reports its
/// version advanced (`ProjectEventPayload::FilesChanged`). Marks the path in
/// `pending_file_refreshes` so the `ProjectFileContents` handler updates it in
/// place — without `open_tab` stealing focus — then issues the same
/// read + subscribe an open does. Re-reading (rather than just bumping the
/// tracked version) is required for correctness: code-intel results are
/// computed against byte offsets, so the client must hold the *new* text before
/// its queries can be honored, otherwise offsets would be resolved against
/// stale content.
pub fn refresh_open_file(
    state: &AppState,
    host_id: String,
    project_id: ProjectId,
    path: ProjectPath,
) {
    state.pending_file_refreshes.update(|pending| {
        pending.insert(path.clone());
    });
    send_read_and_subscribe(host_id, project_id.0, path);
}

/// Send `ProjectReadFile` then `CodeIntelSubscribeFile` for `path` on the
/// project stream. Order matters: the server processes them in arrival order,
/// so the read resolves the file version the subscribe then peeks — the pushed
/// semantic model carries the same version as the rendered contents.
fn send_read_and_subscribe(host_id: String, project_id: String, path: ProjectPath) {
    let read_payload = ProjectReadFilePayload { path: path.clone() };
    let subscribe_payload = CodeIntelSubscribeFilePayload { path };
    let project_stream = StreamPath(format!("/project/{project_id}"));

    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            project_stream.clone(),
            FrameKind::ProjectReadFile,
            &read_payload,
        )
        .await
        {
            log::error!("failed to send ProjectReadFile: {error}");
            return;
        }
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::CodeIntelSubscribeFile,
            &subscribe_payload,
        )
        .await
        {
            log::error!("failed to send CodeIntelSubscribeFile: {error}");
        }
    });
}

/// Mint the next code-intel domain id (shared by navigate + hover, like a
/// monotonic `search_id`).
fn next_code_intel_id(state: &AppState) -> u64 {
    let mut id = 0;
    state.code_intel_request_seq.update(|seq| {
        *seq = seq.wrapping_add(1).max(1);
        id = *seq;
    });
    id
}

/// Cmd/Ctrl+click / F12 go-to-definition (M3). First consult the **pushed**
/// whole-file model for a resolved definition under `offset`: if the occurrence
/// there already carries a target (and the model matches the rendered version),
/// jump **locally** with no server round-trip. Only on a miss — the occurrence
/// hasn't resolved yet, or no model has arrived — fall back to the on-demand
/// `code_intel_navigate` miss-fill (M2).
pub fn navigate_to_definition(
    state: &AppState,
    path: ProjectPath,
    version: ProjectFileVersion,
    offset: u32,
) {
    if try_local_definition_jump(state, &path, version, offset) {
        return;
    }
    request_navigate(state, path, version, offset);
}

/// Attempt a local go-to-definition against the pushed model. Returns `true`
/// (and performs the jump) when an occurrence containing `offset` has at least
/// one resolved `definition` target at the rendered version; otherwise `false`,
/// leaving navigation to the on-demand fallback.
fn try_local_definition_jump(
    state: &AppState,
    path: &ProjectPath,
    version: ProjectFileVersion,
    offset: u32,
) -> bool {
    let Some(active) = state.active_project_ref_untracked() else {
        return false;
    };
    let key = crate::state::CodeIntelKey {
        host_id: active.host_id,
        project_id: active.project_id,
        path: path.clone(),
    };
    let target = state.code_intel.with_untracked(|map| {
        let file = map.get(&key)?;
        // Version-equals-rendered: never navigate from a model computed against
        // text the user is no longer viewing.
        if file.rendered_version != Some(version) {
            return None;
        }
        // Multiple targets (overloads / trait impls) take the first for now,
        // matching the M2 on-demand result handling.
        file.resolved_definition_at(version, offset)
            .map(|(_, location)| location)
    });
    match target {
        Some(location) => {
            // Supersede any in-flight M2 miss-fill: an earlier unresolved click
            // may have sent a `code_intel_navigate` and recorded its context. If
            // we jump locally now, a late `code_intel_navigate_result` for that
            // older click must NOT still yank the user to its (stale) target.
            // Clearing the context makes `apply_code_intel_navigate_result` drop
            // any such result on arrival.
            state.code_intel_navigate_ctx.set(None);
            state
                .pending_goto_offset
                .set(Some((location.path.clone(), location.range.start)));
            open_project_path(state, location.path);
            true
        }
        None => false,
    }
}

/// Send a `code_intel_set_visible_range` hint so the server prioritizes
/// resolving the on-screen occurrences first (M3). Pure prioritization — it
/// never changes which identifiers are clickable. Debounced at the call site so
/// scrolling doesn't flood the stream.
pub fn send_visible_range(
    state: &AppState,
    path: ProjectPath,
    version: ProjectFileVersion,
    range: ByteRange,
) {
    let Some(active) = state.active_project_ref_untracked() else {
        return;
    };
    let payload = CodeIntelSetVisibleRangePayload {
        path,
        version,
        range,
    };
    let project_stream = StreamPath(format!("/project/{}", active.project_id.0));
    let host_id = active.host_id;
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::CodeIntelSetVisibleRange,
            &payload,
        )
        .await
        {
            log::error!("failed to send CodeIntelSetVisibleRange: {error}");
        }
    });
}

/// On-demand go-to-definition (M2 miss-fill): resolve the definition at
/// `offset` bytes into `path`. Mints a fresh `navigate_id`, records it as the
/// active one (so a superseded result is ignored), and streams the request to
/// the active project. The jump happens later, when the correlated
/// `code_intel_navigate_result` arrives (see `dispatch.rs`).
pub fn request_navigate(
    state: &AppState,
    path: ProjectPath,
    version: ProjectFileVersion,
    offset: u32,
) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("request_navigate: no active project");
        return;
    };
    let navigate_id = next_code_intel_id(state);
    // Record the full context so the result is only acted on while it still
    // applies (same host/project, source file still open at this version).
    state
        .code_intel_navigate_ctx
        .set(Some(crate::state::CodeIntelNavigateContext {
            navigate_id,
            host_id: active_project.host_id.clone(),
            project_id: active_project.project_id.clone(),
            path: path.clone(),
            version,
        }));
    let payload = CodeIntelNavigatePayload {
        navigate_id,
        path,
        version,
        offset,
    };
    let project_stream = StreamPath(format!("/project/{}", active_project.project_id.0));
    let host_id = active_project.host_id;
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::CodeIntelNavigate,
            &payload,
        )
        .await
        {
            log::error!("failed to send CodeIntelNavigate: {error}");
        }
    });
}

/// On-demand hover: request type/doc markdown at `offset` bytes into `path`.
/// Mints a fresh `hover_id`, records it active (superseding older hovers), and
/// seeds the popover with the captured anchor rect and `None` contents — the
/// popover renders nothing until the correlated result fills the markdown in.
#[allow(clippy::too_many_arguments)]
pub fn request_hover(
    state: &AppState,
    path: ProjectPath,
    version: ProjectFileVersion,
    offset: u32,
    anchor_left: f64,
    anchor_top: f64,
    anchor_bottom: f64,
) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        return;
    };
    let hover_id = next_code_intel_id(state);
    state.code_intel_active_hover.set(hover_id);
    state.code_intel_hover.set(Some(crate::state::HoverPopover {
        hover_id,
        path: path.clone(),
        version,
        offset,
        anchor_left,
        anchor_top,
        anchor_bottom,
        contents: None,
    }));
    let payload = CodeIntelHoverPayload {
        hover_id,
        path,
        version,
        offset,
    };
    let project_stream = StreamPath(format!("/project/{}", active_project.project_id.0));
    let host_id = active_project.host_id;
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::CodeIntelHover,
            &payload,
        )
        .await
        {
            log::error!("failed to send CodeIntelHover: {error}");
        }
    });
}

/// Dismiss the hover popover and supersede any in-flight hover so its late
/// result is ignored. Called on mouseleave / scroll. Supersedes the active
/// hover id **even when no popover is currently visible**, so a request that is
/// still in flight (debounce fired, result not yet back) is dropped on arrival.
pub fn dismiss_hover(state: &AppState) {
    // Mint a fresh id as the active hover so any in-flight result (which carries
    // an older id) is dropped by `apply_code_intel_hover_result`.
    let superseded = next_code_intel_id(state);
    state.code_intel_active_hover.set(superseded);
    if state.code_intel_hover.with_untracked(|h| h.is_some()) {
        state.code_intel_hover.set(None);
    }
}

/// Start a streamed find-references query (Shift+F12 / M5). Mints a fresh
/// `references_id` (which supersedes any prior query — late frames for an older
/// id are dropped by `dispatch`), resets the panel to an in-flight empty state,
/// switches the left dock to the References tab, and streams the request to the
/// active project. Results arrive incrementally on
/// `code_intel_references_results` frames and finish with a
/// `code_intel_references_complete`.
pub fn start_find_references(
    state: &AppState,
    path: ProjectPath,
    version: ProjectFileVersion,
    offset: u32,
    symbol: Option<String>,
) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("start_find_references: no active project");
        return;
    };
    let references_id = next_code_intel_id(state);
    state
        .references_state
        .set(crate::state::ProjectReferencesUiState {
            host_id: Some(active_project.host_id.clone()),
            project_id: Some(active_project.project_id.clone()),
            source_path: Some(path.clone()),
            source_version: Some(version),
            active_references_id: references_id,
            in_flight: true,
            symbol,
            ..Default::default()
        });
    state.left_tab.set(LeftTab::References);

    let payload = CodeIntelFindReferencesPayload {
        references_id,
        path,
        version,
        offset,
        include_declaration: true,
    };
    let project_stream = StreamPath(format!("/project/{}", active_project.project_id.0));
    let host_id = active_project.host_id;
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::CodeIntelFindReferences,
            &payload,
        )
        .await
        {
            log::error!("failed to send CodeIntelFindReferences: {error}");
        }
    });
}

/// Cancel the in-flight find-references query (if any). Marks the panel not
/// in-flight and sends a `code_intel_cancel_references` for the active id; the
/// server terminates the query with a `cancelled` completion. A no-op when no
/// query is active.
pub fn cancel_find_references(state: &AppState) {
    let (mode, references_id) = state
        .references_state
        .with_untracked(|s| (s.mode, s.active_references_id));
    if mode != crate::state::ProjectReferencesMode::References {
        return;
    }
    if references_id == 0 {
        return;
    }
    let Some((host_id, project_id)) = state
        .references_state
        .with_untracked(|s| Some((s.host_id.clone()?, s.project_id.clone()?)))
    else {
        return;
    };
    state.references_state.update(|s| s.in_flight = false);
    let payload = CodeIntelCancelReferencesPayload { references_id };
    let project_stream = StreamPath(format!("/project/{}", project_id.0));
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::CodeIntelCancelReferences,
            &payload,
        )
        .await
        {
            log::error!("failed to send CodeIntelCancelReferences: {error}");
        }
    });
}

/// Dismiss the references panel: cancel any in-flight query and clear the
/// results. Resetting `active_references_id` to `0` means any late frame for the
/// old id is dropped by `dispatch`.
pub fn clear_references(state: &AppState) {
    cancel_find_references(state);
    state
        .references_state
        .set(crate::state::ProjectReferencesUiState::default());
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
    if state
        .search_state
        .with_untracked(|s| s.query.trim().is_empty())
    {
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
        if let Err(error) = send_frame(
            &host_id,
            project_stream,
            FrameKind::ProjectSearchCancel,
            &payload,
        )
        .await
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
    use protocol::LaunchProfileId;
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

    #[wasm_bindgen_test]
    fn resume_session_opens_current_agent_locally() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            let session_id = SessionId("antigravity-session".to_owned());
            let agent_id = AgentId("antigravity-agent".to_owned());
            state.agents.update(|agents| {
                agents.push(crate::state::AgentInfo {
                    host_id: "host-a".to_owned(),
                    agent_id: agent_id.clone(),
                    name: "Antigravity chat".to_owned(),
                    origin: protocol::AgentOrigin::User,
                    backend_kind: BackendKind::Antigravity,
                    workspace_roots: vec!["/repo".to_owned()],
                    project_id: Some(ProjectId("project-a".to_owned())),
                    parent_agent_id: None,
                    session_id: Some(session_id.clone()),
                    custom_agent_id: None,
                    workflow: None,
                    created_at_ms: 0,
                    instance_stream: StreamPath("/agent/antigravity-agent".to_owned()),
                    started: true,
                    fatal_error: None,
                    activity_summary: Default::default(),
                });
            });

            resume_session(
                &state,
                "host-a".to_owned(),
                session_id,
                Some(ProjectId("project-a".to_owned())),
            );

            assert_eq!(
                state.active_project.get_untracked(),
                Some(ActiveProjectRef {
                    host_id: "host-a".to_owned(),
                    project_id: ProjectId("project-a".to_owned()),
                })
            );
            assert_eq!(
                state.active_agent.get_untracked(),
                Some(ActiveAgentRef {
                    host_id: "host-a".to_owned(),
                    agent_id,
                })
            );
        });
    }

    fn install_send_stub() -> js_sys::Array {
        use wasm_bindgen::JsCast;
        let code = r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
        "#;
        js_sys::eval(code)
            .expect("install tauri stub")
            .dyn_into::<js_sys::Array>()
            .expect("array")
    }

    async fn tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Parse the `params` object of every `spawn_agent` frame recorded against
    /// the send stub.
    fn recorded_spawn_params(calls: &js_sys::Array) -> Vec<serde_json::Value> {
        use wasm_bindgen::JsCast;
        let mut out = Vec::new();
        for entry in calls.iter() {
            let Ok(arr) = entry.dyn_into::<js_sys::Array>() else {
                continue;
            };
            if arr.get(0).as_string().as_deref() != Some("send_host_line") {
                continue;
            }
            let Some(args_json) = arr.get(1).as_string() else {
                continue;
            };
            let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_json) else {
                continue;
            };
            let Some(line) = args.get("line").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(envelope) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if envelope.get("kind").and_then(|v| v.as_str()) != Some("spawn_agent") {
                continue;
            }
            if let Some(params) = envelope
                .get("payload")
                .and_then(|p| p.get("params"))
                .cloned()
            {
                out.push(params);
            }
        }
        out
    }

    fn install_profile_host(state: &AppState) {
        state.selected_host_id.set(Some("host-a".to_owned()));
        state.host_streams.update(|m| {
            m.insert("host-a".to_owned(), StreamPath("/host/host-a".to_owned()));
        });
        state.host_settings_by_host.update(|m| {
            m.insert(
                "host-a".to_owned(),
                protocol::HostSettings {
                    enabled_backends: vec![BackendKind::Hermes],
                    default_backend: Some(BackendKind::Hermes),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
    }

    fn profile_with_model(model: &str) -> LaunchProfile {
        let mut values = std::collections::HashMap::new();
        values.insert(
            "model".to_owned(),
            protocol::SessionSettingValue::String(model.to_owned()),
        );
        LaunchProfile {
            id: LaunchProfileId("hermes:claude".to_owned()),
            kind: protocol::LaunchProfileKind::Custom,
            label: "Hermes · Claude".to_owned(),
            description: None,
            backend_kind: BackendKind::Hermes,
            session_settings: SessionSettingsValues(values),
        }
    }

    /// A launch profile selected but not edited must NOT re-send its settings as
    /// explicit `session_settings` overrides — the server owns profile
    /// resolution. The spawn still carries the `launch_profile_id`.
    #[wasm_bindgen_test]
    async fn spawn_omits_session_settings_for_unedited_profile() {
        let calls = install_send_stub();
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            install_profile_host(&state);
            begin_new_chat_with_profile(&state, profile_with_model("opus"), None);
            assert!(spawn_new_chat(&state, "hello".to_owned(), None, |_| {}));
        });
        for _ in 0..4 {
            tick().await;
        }

        let params = recorded_spawn_params(&calls);
        let new = params
            .iter()
            .find(|p| p.get("kind").and_then(|k| k.as_str()) == Some("new"))
            .expect("a spawn_agent New frame must be emitted");
        assert_eq!(
            new.get("launch_profile_id").and_then(|v| v.as_str()),
            Some("hermes:claude"),
            "spawn must carry the selected launch profile id"
        );
        assert!(
            new.get("session_settings").is_none(),
            "unedited profile settings must not be sent as explicit overrides: {new:?}"
        );
    }

    /// Editing the draft settings after selecting a profile marks them dirty, so
    /// the spawn sends them as explicit `session_settings`.
    #[wasm_bindgen_test]
    async fn spawn_sends_session_settings_when_profile_edited() {
        let calls = install_send_stub();
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            install_profile_host(&state);
            begin_new_chat_with_profile(&state, profile_with_model("opus"), None);
            // Simulate a user edit in the session-settings bar.
            let mut edited = std::collections::HashMap::new();
            edited.insert(
                "model".to_owned(),
                protocol::SessionSettingValue::String("sonnet".to_owned()),
            );
            state
                .draft_session_settings
                .set(SessionSettingsValues(edited));
            state.draft_session_settings_dirty.set(true);
            assert!(spawn_new_chat(&state, "hello".to_owned(), None, |_| {}));
        });
        for _ in 0..4 {
            tick().await;
        }

        let params = recorded_spawn_params(&calls);
        let new = params
            .iter()
            .find(|p| p.get("kind").and_then(|k| k.as_str()) == Some("new"))
            .expect("a spawn_agent New frame must be emitted");
        let model = new
            .get("session_settings")
            .and_then(|s| s.get("model"))
            .and_then(|m| m.get("string"))
            .and_then(|v| v.as_str());
        assert_eq!(
            model,
            Some("sonnet"),
            "user-edited settings must be sent as explicit overrides: {new:?}"
        );
    }
}
