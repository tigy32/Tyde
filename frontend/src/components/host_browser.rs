use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    FrameKind, HostAbsPath, HostBrowseClosePayload, HostBrowseErrorCode, HostBrowseErrorPayload,
    HostBrowseInitial, HostBrowseListPayload, HostBrowseStartPayload, ProjectAddRootPayload,
    ProjectCreatePayload, ProjectFileKind, StreamPath,
};

use crate::send::send_frame;
use crate::state::{AppState, BrowseDialogState, BrowsePurpose, ConnectionStatus};

fn new_browse_stream() -> StreamPath {
    let id = (js_sys::Math::random() * 1_000_000_000_000.0) as u64;
    StreamPath(format!("/browse/{id}"))
}

/// Pick the host the modal should open against. Prefers the Settings-selected
/// host when it is connected; otherwise falls back to the first configured
/// host with an active stream. Returns `None` when no host is currently
/// usable — callers must surface that to the user.
fn pick_initial_host(state: &AppState) -> Option<(String, StreamPath)> {
    if let Some((host_id, stream)) = state.selected_host_stream_untracked()
        && matches!(
            state.connection_status_for_host(&host_id),
            ConnectionStatus::Connected
        )
    {
        return Some((host_id, stream));
    }
    let statuses = state.connection_statuses.get_untracked();
    let streams = state.host_streams.get_untracked();
    state
        .configured_hosts
        .get_untracked()
        .into_iter()
        .find_map(|host| {
            let connected = matches!(statuses.get(&host.id), Some(ConnectionStatus::Connected));
            let stream = streams.get(&host.id).cloned()?;
            connected.then_some((host.id, stream))
        })
}

pub fn open_project_browser(state: &AppState) {
    let Some((host_id, host_stream)) = pick_initial_host(state) else {
        log::error!("cannot open browser: no connected host available");
        return;
    };
    open_browser_for(state, host_id, host_stream, BrowsePurpose::OpenProject);
}

pub fn open_add_root_browser(state: &AppState) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("cannot add a root without an active project");
        return;
    };
    // §6.5: ProjectAddRoot is invalid on a workbench and on a parent that
    // already has workbench children. The button that opens this browser
    // is gated, but guard here too so a stale click (or a key shortcut
    // that bypasses the disabled state) can't fire a request the server
    // will reject.
    if !state.can_manage_project_roots(&active_project.host_id, &active_project.project_id) {
        log::warn!(
            "cannot add a root for project {}: workbench or parent-of-workbench",
            active_project.project_id
        );
        return;
    }
    let Some(host_stream) = state.host_stream_untracked(&active_project.host_id) else {
        log::error!(
            "cannot add a root: host {} has no active stream",
            active_project.host_id
        );
        return;
    };
    open_browser_for(
        state,
        active_project.host_id,
        host_stream,
        BrowsePurpose::AddRoot {
            project_id: active_project.project_id,
        },
    );
}

fn open_browser_for(
    state: &AppState,
    host_id: String,
    host_stream: StreamPath,
    purpose: BrowsePurpose,
) {
    let browse_stream = new_browse_stream();
    let initial = match &purpose {
        BrowsePurpose::OpenProject => HostBrowseInitial::Home,
        BrowsePurpose::AddRoot { project_id } => HostBrowseInitial::ProjectRoots {
            project_id: project_id.clone(),
        },
    };
    let dialog = BrowseDialogState {
        host_id: ArcRwSignal::new(host_id.clone()),
        browse_stream: ArcRwSignal::new(browse_stream.clone()),
        purpose,
        include_hidden: ArcRwSignal::new(false),
        platform: ArcRwSignal::new(None),
        separator: ArcRwSignal::new('/'),
        home: ArcRwSignal::new(None),
        current_path: ArcRwSignal::new(None),
        parent: ArcRwSignal::new(None),
        entries: ArcRwSignal::new(Vec::new()),
        error: ArcRwSignal::new(None),
        loading: ArcRwSignal::new(true),
    };
    state.browse_dialog.set(Some(dialog));

    let state_for_task = state.clone();
    let host_id_for_err = host_id.clone();
    let browse_stream_for_err = browse_stream.clone();
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            host_stream,
            FrameKind::HostBrowseStart,
            &HostBrowseStartPayload {
                browse_stream: browse_stream.clone(),
                initial,
                include_hidden: false,
            },
        )
        .await
        {
            log::error!("failed to send HostBrowseStart: {error}");
            surface_transport_error(
                &state_for_task,
                &host_id_for_err,
                &browse_stream_for_err,
                format!("failed to start browser: {error}"),
            );
        }
    });
}

fn send_list(
    state: AppState,
    host_id: String,
    browse_stream: StreamPath,
    path: HostAbsPath,
    include_hidden: bool,
) {
    let host_id_for_err = host_id.clone();
    let browse_stream_for_err = browse_stream.clone();
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            browse_stream.clone(),
            FrameKind::HostBrowseList,
            &HostBrowseListPayload {
                path,
                include_hidden,
            },
        )
        .await
        {
            log::error!("failed to send HostBrowseList: {error}");
            surface_transport_error(
                &state,
                &host_id_for_err,
                &browse_stream_for_err,
                format!("failed to list directory: {error}"),
            );
        }
    });
}

/// Show a transport-level (not server-protocol) error in the dialog, but only
/// if the dialog is still pointing at the same (host, stream) — otherwise the
/// user has already moved on and we'd be tainting a fresh stream's state.
fn surface_transport_error(
    state: &AppState,
    host_id: &str,
    browse_stream: &StreamPath,
    message: String,
) {
    let Some(dialog) = state.browse_dialog.with_untracked(|d| {
        d.as_ref()
            .filter(|d| {
                d.host_id.get_untracked() == host_id
                    && d.browse_stream.get_untracked() == *browse_stream
            })
            .cloned()
    }) else {
        return;
    };
    dialog.error.set(Some(HostBrowseErrorPayload {
        path: HostAbsPath(String::new()),
        code: HostBrowseErrorCode::Internal,
        message,
    }));
    dialog.loading.set(false);
}

fn close_dialog(state: &AppState) {
    let Some(dialog) = state.browse_dialog.get_untracked() else {
        return;
    };
    state.browse_dialog.set(None);

    let host_id = dialog.host_id.get_untracked();
    let browse_stream = dialog.browse_stream.get_untracked();
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            browse_stream,
            FrameKind::HostBrowseClose,
            &HostBrowseClosePayload::default(),
        )
        .await
        {
            log::error!("failed to send HostBrowseClose: {error}");
        }
    });
}

/// Switch the still-open dialog to a different host without remounting the
/// modal. Validates the new host first so a failed switch leaves the old
/// stream alive and the dialog usable.
fn switch_host(state: &AppState, dialog: &BrowseDialogState, new_host_id: String) {
    let old_host_id = dialog.host_id.get_untracked();
    if old_host_id == new_host_id {
        return;
    }

    let Some(new_host_stream) = state.host_stream_untracked(&new_host_id) else {
        dialog.error.set(Some(HostBrowseErrorPayload {
            path: HostAbsPath(String::new()),
            code: HostBrowseErrorCode::Internal,
            message: format!("host {new_host_id} has no active stream"),
        }));
        // Force the host_id signal to fire so the dropdown's prop:value
        // re-binds back to the old host even though the value didn't change.
        dialog.host_id.update(|_| {});
        return;
    };

    let old_browse_stream = dialog.browse_stream.get_untracked();
    let new_browse_stream = new_browse_stream();
    let include_hidden = dialog.include_hidden.get_untracked();

    // Atomically swap the dialog onto the new (host, stream) BEFORE issuing
    // any frames. Late events from the old stream then fail the dispatcher's
    // (host_id, browse_stream) match check.
    let dialog_for_batch = dialog.clone();
    let new_host_id_for_batch = new_host_id.clone();
    let new_browse_stream_for_batch = new_browse_stream.clone();
    leptos::prelude::batch(move || {
        dialog_for_batch.host_id.set(new_host_id_for_batch);
        dialog_for_batch
            .browse_stream
            .set(new_browse_stream_for_batch);
        dialog_for_batch.platform.set(None);
        dialog_for_batch.separator.set('/');
        dialog_for_batch.home.set(None);
        dialog_for_batch.current_path.set(None);
        dialog_for_batch.parent.set(None);
        dialog_for_batch.entries.set(Vec::new());
        dialog_for_batch.error.set(None);
        dialog_for_batch.loading.set(true);
    });

    // Sequence Start-on-new before Close-on-old inside one task. If Close
    // arrived at the server before its matching Start, the server would
    // no-op the close, then later register a stream nobody will close — a
    // leak.
    let state_for_task = state.clone();
    let new_host_id_for_task = new_host_id;
    let new_browse_stream_for_task = new_browse_stream;
    spawn_local(async move {
        if let Err(error) = send_frame(
            &new_host_id_for_task,
            new_host_stream,
            FrameKind::HostBrowseStart,
            &HostBrowseStartPayload {
                browse_stream: new_browse_stream_for_task.clone(),
                initial: HostBrowseInitial::Home,
                include_hidden,
            },
        )
        .await
        {
            log::error!("failed to send HostBrowseStart on switch: {error}");
            surface_transport_error(
                &state_for_task,
                &new_host_id_for_task,
                &new_browse_stream_for_task,
                format!("failed to start browser on {new_host_id_for_task}: {error}"),
            );
            // Fall through and still close the old stream — the dialog has
            // already moved off it.
        }

        if let Err(error) = send_frame(
            &old_host_id,
            old_browse_stream,
            FrameKind::HostBrowseClose,
            &HostBrowseClosePayload::default(),
        )
        .await
        {
            log::error!("failed to send HostBrowseClose on old stream: {error}");
        }
    });
}

#[component]
pub fn HostBrowser() -> impl IntoView {
    let state = expect_context::<AppState>();

    view! {
        {move || {
            state.browse_dialog.with(|dialog| {
                dialog.as_ref().map(|d| {
                    let dialog = d.clone();
                    view! { <HostBrowserModal dialog=dialog/> }
                })
            })
        }}
    }
}

#[component]
fn HostBrowserModal(dialog: BrowseDialogState) -> impl IntoView {
    let state = expect_context::<AppState>();

    let host_id_signal = dialog.host_id.clone();
    let browse_stream_signal = dialog.browse_stream.clone();
    let current_path = dialog.current_path.clone();
    let parent = dialog.parent.clone();
    let entries = dialog.entries.clone();
    let error = dialog.error.clone();
    let loading = dialog.loading.clone();
    let include_hidden = dialog.include_hidden.clone();

    let navigate_to = {
        let state = state.clone();
        let host_id_signal = host_id_signal.clone();
        let browse_stream_signal = browse_stream_signal.clone();
        let include_hidden = include_hidden.clone();
        let loading = loading.clone();
        move |path: HostAbsPath| {
            loading.set(true);
            send_list(
                state.clone(),
                host_id_signal.get_untracked(),
                browse_stream_signal.get_untracked(),
                path,
                include_hidden.get_untracked(),
            );
        }
    };

    let address_input: RwSignal<String> = RwSignal::new(String::new());

    let path_display = {
        let current_path = current_path.clone();
        move || {
            current_path
                .get()
                .map(|p| p.0)
                .unwrap_or_else(|| "…".to_owned())
        }
    };

    // Sync the address bar with current_path. When switching hosts resets
    // current_path to None, also clear the bar so the previous host's path
    // doesn't linger while the new host loads.
    {
        let current_path = current_path.clone();
        Effect::new(move |_| match current_path.get() {
            Some(path) => address_input.set(path.0),
            None => address_input.set(String::new()),
        });
    }

    let state_for_close = state.clone();
    let on_cancel = move |_| close_dialog(&state_for_close);
    let state_for_backdrop = state.clone();
    let on_backdrop = move |ev: web_sys::MouseEvent| {
        if let Some(target) = ev.target()
            && let Some(cur) = ev.current_target()
            && target == cur
        {
            close_dialog(&state_for_backdrop);
        }
    };

    let on_up = {
        let parent = parent.clone();
        let navigate_to = navigate_to.clone();
        move |_| {
            if let Some(p) = parent.get_untracked() {
                navigate_to(p);
            }
        }
    };

    let on_toggle_hidden = {
        let include_hidden = include_hidden.clone();
        let current_path = current_path.clone();
        let navigate_to = navigate_to.clone();
        move |_| {
            include_hidden.update(|v| *v = !*v);
            if let Some(p) = current_path.get_untracked() {
                navigate_to(p);
            }
        }
    };

    let on_address_keydown = {
        let navigate_to = navigate_to.clone();
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                let typed = address_input.get_untracked();
                let trimmed = typed.trim();
                if trimmed.starts_with('/') {
                    navigate_to(HostAbsPath(trimmed.to_owned()));
                } else {
                    log::warn!("browser address must be absolute: {trimmed}");
                }
            }
        }
    };

    let state_for_confirm = state.clone();
    let current_path_for_confirm = current_path.clone();
    let dialog_purpose = dialog.purpose.clone();
    let host_id_for_confirm = host_id_signal.clone();
    let on_confirm = move |_| {
        let Some(path) = current_path_for_confirm.get_untracked() else {
            return;
        };
        match dialog_purpose.clone() {
            BrowsePurpose::OpenProject => {
                let name = path
                    .0
                    .rsplit('/')
                    .find(|segment| !segment.is_empty())
                    .unwrap_or(&path.0)
                    .to_owned();
                let host_id = host_id_for_confirm.get_untracked();
                let Some(host_stream) = state_for_confirm.host_stream_untracked(&host_id) else {
                    log::error!("cannot create project without a connected dialog host");
                    return;
                };
                spawn_local(async move {
                    if let Err(error) = send_frame(
                        &host_id,
                        host_stream,
                        FrameKind::ProjectCreate,
                        &ProjectCreatePayload {
                            name,
                            roots: vec![protocol::ProjectRootPath(path.0)],
                        },
                    )
                    .await
                    {
                        log::error!("failed to send ProjectCreate: {error}");
                    }
                });
                close_dialog(&state_for_confirm);
            }
            BrowsePurpose::AddRoot { project_id } => {
                let host_id = host_id_for_confirm.get_untracked();
                let Some(host_stream) = state_for_confirm.host_stream_untracked(&host_id) else {
                    log::error!("cannot add root without a connected dialog host");
                    return;
                };
                spawn_local(async move {
                    if let Err(error) = send_frame(
                        &host_id,
                        host_stream,
                        FrameKind::ProjectAddRoot,
                        &ProjectAddRootPayload {
                            id: project_id,
                            root: protocol::ProjectRootPath(path.0),
                        },
                    )
                    .await
                    {
                        log::error!("failed to send ProjectAddRoot: {error}");
                    }
                });
                close_dialog(&state_for_confirm);
            }
        }
    };

    let state_for_esc = state.clone();
    let on_modal_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" {
            close_dialog(&state_for_esc);
        }
    };

    // Host dropdown is meaningless for AddRoot — the project lives on a
    // single host. Disable the control entirely in that mode.
    let allow_host_switch = matches!(dialog.purpose, BrowsePurpose::OpenProject);

    let dropdown_view = {
        let state = state.clone();
        let host_id_signal = host_id_signal.clone();
        let dialog = dialog.clone();
        move || {
            let current_host = host_id_signal.get();
            let configured = state.configured_hosts.get();
            let statuses = state.connection_statuses.get();
            let streams = state.host_streams.get();
            let options = configured
                .into_iter()
                .map(|host| {
                    let connected =
                        matches!(statuses.get(&host.id), Some(ConnectionStatus::Connected))
                            && streams.contains_key(&host.id);
                    let selected = host.id == current_host;
                    let label = if connected {
                        host.label.clone()
                    } else {
                        format!("{} (disconnected)", host.label)
                    };
                    // Disable disconnected hosts unless this is the one
                    // currently shown — we still need to be able to render
                    // its option as selected.
                    let disabled = !connected && !selected;
                    view! {
                        <option value=host.id.clone() disabled=disabled selected=selected>
                            {label}
                        </option>
                    }
                })
                .collect_view();
            let on_change = {
                let state = state.clone();
                let dialog = dialog.clone();
                move |ev: web_sys::Event| {
                    let new_host_id = event_target_value(&ev);
                    switch_host(&state, &dialog, new_host_id);
                }
            };
            let host_id_for_value = host_id_signal.clone();
            view! {
                <select
                    class="browser-host-select"
                    prop:value=move || host_id_for_value.get()
                    on:change=on_change
                    disabled=!allow_host_switch
                    title="Choose host"
                >
                    {options}
                </select>
            }
        }
    };

    let entries_view = {
        let entries = entries.clone();
        let navigate_to = navigate_to.clone();
        let current_for_entries = current_path.clone();
        move || {
            let rows = entries.get();
            rows.into_iter()
                .map(|entry| {
                    let is_dir = matches!(
                        entry.kind,
                        ProjectFileKind::Directory | ProjectFileKind::Symlink
                    );
                    let has_error = entry.entry_error.is_some();
                    let class = if has_error {
                        "browser-row browser-row-error"
                    } else if is_dir {
                        "browser-row browser-row-dir"
                    } else {
                        "browser-row browser-row-file"
                    };
                    let current = current_for_entries.clone();
                    let navigate_to = navigate_to.clone();
                    let entry_name = entry.name.clone();
                    let on_click = move |_| {
                        if !is_dir || has_error {
                            return;
                        }
                        let Some(base) = current.get_untracked() else {
                            return;
                        };
                        let joined = if base.0.ends_with('/') {
                            format!("{}{}", base.0, entry_name)
                        } else {
                            format!("{}/{}", base.0, entry_name)
                        };
                        navigate_to(HostAbsPath(joined));
                    };
                    let icon = match entry.kind {
                        ProjectFileKind::Directory => "📁",
                        ProjectFileKind::Symlink => "🔗",
                        ProjectFileKind::File => "📄",
                    };
                    let size_text = entry.size.map(human_size).unwrap_or_else(String::new);
                    view! {
                        <div class=class on:click=on_click>
                            <span class="browser-icon">{icon}</span>
                            <span class="browser-name">{entry.name.clone()}</span>
                            <span class="browser-size">{size_text}</span>
                        </div>
                    }
                })
                .collect_view()
        }
    };

    let error_view = {
        let error = error.clone();
        move || {
            error.get().map(|e| {
                view! {
                    <div class="browser-error">
                        {format!("{}: {}", format_error_code(&e.code), e.message)}
                    </div>
                }
            })
        }
    };

    let loading_view = {
        let loading = loading.clone();
        move || {
            loading.get().then(|| {
                view! { <div class="browser-loading">"Loading…"</div> }
            })
        }
    };

    let hidden_label = {
        let include_hidden = include_hidden.clone();
        move || {
            if include_hidden.get() {
                "Hide hidden"
            } else {
                "Show hidden"
            }
        }
    };

    let can_go_up = {
        let parent = parent.clone();
        move || parent.get().is_some()
    };

    let can_confirm = {
        let current_path = current_path.clone();
        move || current_path.get().is_some()
    };

    let confirm_label = match &dialog.purpose {
        BrowsePurpose::OpenProject => "Open Project Here",
        BrowsePurpose::AddRoot { .. } => "Add Root",
    };

    view! {
        <div class="browser-backdrop" on:click=on_backdrop on:keydown=on_modal_keydown tabindex="0">
            <div class="browser-modal" on:click=|ev| ev.stop_propagation()>
                <div class="browser-header">
                    <button
                        class="browser-btn"
                        on:click=on_up
                        disabled=move || !can_go_up()
                        title="Parent directory"
                    >"↑"</button>
                    <input
                        class="browser-address"
                        type="text"
                        prop:value=move || address_input.get()
                        on:input=move |ev| address_input.set(event_target_value(&ev))
                        on:keydown=on_address_keydown
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                    <button class="browser-btn" on:click=on_toggle_hidden>{hidden_label}</button>
                </div>
                <div class="browser-host-row">{dropdown_view}</div>
                <div class="browser-path">{path_display}</div>
                {error_view}
                {loading_view}
                <div class="browser-entries">{entries_view}</div>
                <div class="browser-footer">
                    <button class="browser-btn" on:click=on_cancel>"Cancel"</button>
                    <button
                        class="browser-btn browser-btn-primary"
                        on:click=on_confirm
                        disabled=move || !can_confirm()
                    >{confirm_label}</button>
                </div>
            </div>
        </div>
    }
}

fn human_size(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.1}G", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1}M", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}K", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

fn format_error_code(code: &protocol::HostBrowseErrorCode) -> &'static str {
    use protocol::HostBrowseErrorCode::*;
    match code {
        NotFound => "Not found",
        NotADirectory => "Not a directory",
        PermissionDenied => "Permission denied",
        SymlinkLoop => "Symlink loop",
        TooLarge => "Directory too large",
        Internal => "Internal error",
    }
}
