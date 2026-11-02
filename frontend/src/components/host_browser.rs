use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    FrameKind, HostAbsPath, HostBrowseClosePayload, HostBrowseErrorCode, HostBrowseErrorPayload,
    HostBrowseInitial, HostBrowseListPayload, HostBrowseStartPayload, ProjectFileKind, StreamPath,
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
        crate::notices::report_error(
            crate::notices::NoticeScope::Browser,
            "Tyde cannot open the folder browser because no host is connected.",
        );
        return;
    };
    open_browser_for(
        state,
        host_id,
        host_stream,
        BrowsePurpose::OpenProject {
            allow_host_switch: true,
        },
    );
}

pub(super) fn open_project_browser_for_host(state: &AppState, host_id: &str) {
    let stream = state
        .host_streams
        .with_untracked(|streams| streams.get(host_id).cloned());
    let Some(stream) = stream.filter(|_| {
        matches!(
            state.connection_status_for_host(host_id),
            ConnectionStatus::Connected
        )
    }) else {
        crate::notices::report_error(
            crate::notices::NoticeScope::Browser,
            "Connect this host before creating a project.",
        );
        return;
    };
    open_browser_for(
        state,
        host_id.to_owned(),
        stream,
        BrowsePurpose::OpenProject {
            allow_host_switch: false,
        },
    );
}

pub(super) fn open_project_browser_on_selected_host(state: &AppState) {
    let selected = state.selected_host_stream_untracked().filter(|(host, _)| {
        matches!(
            state.connection_status_for_host(host),
            ConnectionStatus::Connected
        )
    });
    let Some((host_id, host_stream)) = selected else {
        crate::notices::report_error(
            crate::notices::NoticeScope::Browser,
            "Connect the selected computer in Host settings before choosing a folder.",
        );
        return;
    };
    open_browser_for(
        state,
        host_id,
        host_stream,
        BrowsePurpose::OpenProject {
            allow_host_switch: false,
        },
    );
}

pub fn open_add_root_browser(state: &AppState) {
    let Some(active_project) = state.active_project_ref_untracked() else {
        log::error!("cannot add a root without an active project");
        crate::notices::report_error(
            crate::notices::NoticeScope::Browser,
            "Tyde cannot add a folder because no project is active.",
        );
        return;
    };
    open_add_root_browser_for(state, active_project);
}

pub fn open_add_root_browser_for(state: &AppState, active_project: crate::state::ActiveProjectRef) {
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
        crate::notices::report_error(
            crate::notices::NoticeScope::Browser,
            "Folders cannot be changed for a workbench or for a project that currently has workbenches.",
        );
        return;
    }
    let Some(host_stream) = state.host_stream_untracked(&active_project.host_id) else {
        log::error!(
            "cannot add a root: host {} has no active stream",
            active_project.host_id
        );
        crate::notices::report_error(
            crate::notices::NoticeScope::Browser,
            format!(
                "Tyde cannot add a folder because host “{}” is not connected.",
                active_project.host_id
            ),
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
    crate::notices::clear_scope(&crate::notices::NoticeScope::Browser);
    let browse_stream = new_browse_stream();
    let initial = match &purpose {
        BrowsePurpose::OpenProject { .. } => HostBrowseInitial::Home,
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
        operation: ArcRwSignal::new(None),
        awaiting_submission: ArcRwSignal::new(false),
    };
    state.browse_dialog.set(Some(dialog));

    let state_for_task = state.clone();
    let host_id_for_err = host_id.clone();
    let browse_stream_for_err = browse_stream.clone();
    spawn_local(async move {
        if let Err(error) = crate::send::send_frame_unreported(
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
        if let Err(error) = crate::send::send_frame_unreported(
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
    crate::notices::clear_scope(&crate::notices::NoticeScope::Browser);
    let Some(dialog) = state.browse_dialog.get_untracked() else {
        return;
    };
    state.browse_dialog.set(None);
    if let Some(operation) = dialog.operation.get_untracked() {
        let key = (dialog.host_id.get_untracked(), operation);
        state.project_operations.update(|operations| {
            if operations.get(&key).is_some_and(|operation| {
                matches!(&operation.state,
                protocol::ProjectOperationState::Committed { warnings } if warnings.is_empty())
            }) {
                operations.remove(&key);
            }
        });
    }

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
        dialog_for_batch.operation.set(None);
        dialog_for_batch.awaiting_submission.set(false);
    });

    // Sequence Start-on-new before Close-on-old inside one task. If Close
    // arrived at the server before its matching Start, the server would
    // no-op the close, then later register a stream nobody will close — a
    // leak.
    let state_for_task = state.clone();
    let new_host_id_for_task = new_host_id;
    let new_browse_stream_for_task = new_browse_stream;
    spawn_local(async move {
        if let Err(error) = crate::send::send_frame_unreported(
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
    let address_input = RwSignal::new(String::new());
    let submission = {
        let state = state.clone();
        let host = host_id_signal.clone();
        let operation = dialog.operation.clone();
        Signal::derive(move || {
            let host = host.get();
            operation.get().and_then(|stream| {
                state.project_operations.with(|operations| {
                    operations
                        .get(&(host, stream))
                        .map(|operation| operation.state.clone())
                })
            })
        })
    };
    let target_locked = {
        let awaiting = dialog.awaiting_submission.clone();
        Signal::derive(move || {
            awaiting.get()
                || matches!(
                    submission.get(),
                    Some(
                        protocol::ProjectOperationState::Admitted
                            | protocol::ProjectOperationState::Running { .. }
                            | protocol::ProjectOperationState::Committed { .. }
                    )
                )
        })
    };
    {
        let state = state.clone();
        let awaiting = dialog.awaiting_submission.clone();
        Effect::new(move |_| {
            if submission.get().is_some() {
                awaiting.set(false);
            }
            if matches!(
                submission.get(),
                Some(protocol::ProjectOperationState::Committed { warnings }) if warnings.is_empty()
            ) {
                close_dialog(&state);
            }
        });
    }
    {
        let host = host_id_signal.clone();
        Effect::new(move |_| {
            host.track();
            address_input.set(String::new());
        });
    }

    let navigate_to = {
        let state = state.clone();
        let host_id_signal = host_id_signal.clone();
        let browse_stream_signal = browse_stream_signal.clone();
        let include_hidden = include_hidden.clone();
        let loading = loading.clone();
        let error = error.clone();
        let operation = dialog.operation.clone();
        move |path: HostAbsPath, select: bool| {
            if loading.get_untracked() || target_locked.get_untracked() {
                return;
            }
            if select {
                address_input.set(path.0.clone());
            }
            error.set(None);
            operation.set(None);
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
                navigate_to(p, true);
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
                navigate_to(p, false);
            }
        }
    };

    let on_address_keydown = {
        let navigate_to = navigate_to.clone();
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                let typed = address_input.get_untracked();
                let trimmed = typed.trim();
                if !trimmed.is_empty() {
                    navigate_to(HostAbsPath(typed), true);
                }
            }
        }
    };

    let duplicate = {
        let state = state.clone();
        let host = host_id_signal.clone();
        Memo::new(move |_| {
            let target = address_input.get();
            let host = host.get();
            state.projects.with(|projects| {
                projects
                    .iter()
                    .find(|entry| {
                        entry.host_id == host
                            && entry
                                .project
                                .root_paths()
                                .iter()
                                .any(|root| root.0 == target)
                    })
                    .map(|entry| format!("Already registered in project “{}”.", entry.project.name))
            })
        })
    };
    let on_confirm = {
        let state = state.clone();
        let dialog = dialog.clone();
        move |_| {
            let target = address_input.get_untracked();
            if target_locked.get_untracked()
                || dialog.loading.get_untracked()
                || target.trim().is_empty()
                || duplicate.get_untracked().is_some()
            {
                return;
            }
            dialog.awaiting_submission.set(true);
            dialog.error.set(None);
            dialog.operation.set(None);
            let intent = match &dialog.purpose {
                BrowsePurpose::OpenProject { .. } => protocol::HostBrowseIntent::CreateProject,
                BrowsePurpose::AddRoot { project_id } => protocol::HostBrowseIntent::AddRoot {
                    project_id: project_id.clone(),
                },
            };
            let state = state.clone();
            let host = dialog.host_id.get_untracked();
            let stream = dialog.browse_stream.get_untracked();
            let awaiting_submission = dialog.awaiting_submission.clone();
            spawn_local(async move {
                if let Err(error) = crate::send::send_frame_unreported(
                    &host,
                    stream.clone(),
                    FrameKind::HostBrowseSubmit,
                    &protocol::HostBrowseSubmitPayload {
                        path: HostAbsPath(target),
                        intent,
                    },
                )
                .await
                {
                    awaiting_submission.set(false);
                    surface_transport_error(
                        &state,
                        &host,
                        &stream,
                        format!("Cannot submit folder: {error}"),
                    );
                }
            });
        }
    };

    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    let address_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if let Some(address) = address_ref.get() {
            let _ = address.focus();
        }
    });
    let state_for_esc = state.clone();
    let on_modal_keydown = move |ev: web_sys::KeyboardEvent| {
        ev.stop_propagation();
        match ev.key().as_str() {
            "Escape" => {
                ev.prevent_default();
                close_dialog(&state_for_esc);
            }
            "Tab" => {
                let Some(dialog) = dialog_ref.get_untracked() else {
                    return;
                };
                let Ok(nodes) = dialog.query_selector_all(
                    "button:not([disabled]), input:not([disabled]), select:not([disabled])",
                ) else {
                    return;
                };
                let (Some(first), Some(last)) = (
                    nodes
                        .item(0)
                        .and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok()),
                    nodes
                        .item(nodes.length().saturating_sub(1))
                        .and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok()),
                ) else {
                    return;
                };
                let active = web_sys::window()
                    .and_then(|window| window.document())
                    .and_then(|document| document.active_element());
                if ev.shift_key()
                    && active
                        .as_ref()
                        .is_some_and(|element| element.is_same_node(Some(&first)))
                {
                    ev.prevent_default();
                    let _ = last.focus();
                } else if !ev.shift_key()
                    && active
                        .as_ref()
                        .is_some_and(|element| element.is_same_node(Some(&last)))
                {
                    ev.prevent_default();
                    let _ = first.focus();
                }
            }
            _ => {}
        }
    };

    let allow_host_switch = matches!(
        dialog.purpose,
        BrowsePurpose::OpenProject {
            allow_host_switch: true
        }
    );

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
                    disabled=move || !allow_host_switch || target_locked.get()
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
                        navigate_to(HostAbsPath(joined), true);
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
                        {e.message}
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
        {
            let loading = loading.clone();
            move || parent.get().is_some() && !loading.get() && !target_locked.get()
        }
    };

    let can_confirm = {
        let loading = loading.clone();
        move || {
            !address_input.get().trim().is_empty()
                && !loading.get()
                && !target_locked.get()
                && duplicate.get().is_none()
        }
    };
    let title = {
        let state = state.clone();
        let host = host_id_signal.clone();
        let purpose = dialog.purpose.clone();
        move || match &purpose {
            BrowsePurpose::OpenProject { .. } => "Create Project".to_owned(),
            BrowsePurpose::AddRoot { project_id } => {
                let host = host.get();
                state
                    .projects
                    .with(|projects| {
                        projects
                            .iter()
                            .find(|entry| entry.host_id == host && &entry.project.id == project_id)
                            .map(|entry| format!("Add Root to “{}”", entry.project.name))
                    })
                    .unwrap_or_else(|| format!("Add Root to {project_id}"))
            }
        }
    };
    let confirm_label = match &dialog.purpose {
        BrowsePurpose::OpenProject { .. } => "Create Project",
        BrowsePurpose::AddRoot { .. } => "Add Root",
    };
    let current_for_select = current_path.clone();
    let error_for_select = error.clone();
    let operation_for_select = dialog.operation.clone();
    let select_current = move |_| {
        if let Some(path) = current_for_select.get_untracked() {
            address_input.set(path.0);
            error_for_select.set(None);
            operation_for_select.set(None);
        }
    };
    let current_for_label = current_path.clone();
    let current_for_disable = current_path.clone();
    let operation_for_edit = dialog.operation.clone();
    let error_for_edit = error.clone();
    let loading_for_hidden = loading.clone();
    let loading_for_select = loading.clone();

    view! {
        <div class="browser-backdrop" on:click=on_backdrop on:keydown=on_modal_keydown tabindex="0">
            <div class="browser-modal" role="dialog" aria-modal="true" aria-label="Folder browser" node_ref=dialog_ref on:click=|ev| ev.stop_propagation()>
                <h2>{title}</h2>
                <div class="browser-header">
                    <button
                        class="browser-btn"
                        on:click=on_up
                        disabled=move || !can_go_up()
                        title="Parent directory"
                    >"↑"</button>
                    <input
                        class="browser-address"
                        aria-label="Folder path"
                        node_ref=address_ref
                        type="text"
                        prop:value=move || address_input.get()
                        on:input=move |ev| {
                            address_input.set(event_target_value(&ev));
                            error_for_edit.set(None);
                            operation_for_edit.set(None);
                        }
                        disabled=move || target_locked.get()
                        on:keydown=on_address_keydown
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                    <button class="browser-btn" on:click=on_toggle_hidden disabled=move || loading_for_hidden.get() || target_locked.get()>{hidden_label}</button>
                </div>
                <div class="browser-host-row">{dropdown_view}</div>
                <button class="browser-btn browser-path" on:click=select_current
                    disabled=move || current_for_disable.get().is_none() || loading_for_select.get() || target_locked.get()>
                    {move || current_for_label.get().map(|path| format!("Select {}", path.0))}
                </button>
                <output class="browser-path" aria-label="Selected folder">{move || address_input.get()}</output>
                {move || (!target_locked.get()).then(|| duplicate.get()).flatten().map(|message| view! { <p role="alert">{message}</p> })}
                <crate::notices::InlineNotices scopes=vec![crate::notices::NoticeScope::Browser] />
                {move || submission.get().and_then(|state| match state {
                    protocol::ProjectOperationState::Failed { message, .. } => Some(view! { <p role="alert">{message}</p> }.into_any()),
                    protocol::ProjectOperationState::Admitted | protocol::ProjectOperationState::Running { .. } => Some(view! { <p role="status">"Saving…"</p> }.into_any()),
                    protocol::ProjectOperationState::Committed { warnings } => (!warnings.is_empty()).then(|| view! {
                        <div role="alert">
                            <p>"Saved with warnings. Do not repeat this action."</p>
                            {warnings.into_iter().map(|warning| view! { <p>{warning.message}</p> }).collect_view()}
                        </div>
                    }.into_any()),
                })}
                {error_view}
                {loading_view}
                <div class="browser-entries">{entries_view}</div>
                <div class="browser-footer">
                    <button class="browser-btn" on:click=on_cancel>{move || if target_locked.get() { "Close" } else { "Cancel" }}</button>
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::wasm_test_support::mount_to;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlInputElement};

    wasm_bindgen_test_configure!(run_in_browser);

    async fn tick() {
        leptos::task::tick().await;
    }

    fn button(container: &HtmlElement, label: &str) -> HtmlElement {
        let nodes = container.query_selector_all("button").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|button| button.text_content().unwrap_or_default() == label)
            .unwrap_or_else(|| panic!("missing button: {label}"))
    }

    fn input(container: &HtmlElement, value: &str) {
        let input: HtmlInputElement = container
            .query_selector("input[aria-label='Folder path']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value(value);
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    fn frames() -> Vec<protocol::Envelope> {
        serde_json::from_str(
            &js_sys::eval("JSON.stringify(window.__picker_frames)")
                .unwrap()
                .as_string()
                .unwrap(),
        )
        .unwrap()
    }

    fn fixture(purpose: BrowsePurpose) -> (AppState, BrowseDialogState, HtmlElement) {
        js_sys::eval(
            r#"
            window.__picker_frames = [];
            window.__TAURI__ = { core: { invoke: (cmd, args) => {
                if (cmd === 'send_host_line') window.__picker_frames.push(JSON.parse(args.line));
                return Promise.resolve();
            } } };
        "#,
        )
        .unwrap();
        let state = AppState::new();
        state.host_streams.update(|streams| {
            streams.insert("picker-host".into(), StreamPath("/host/picker".into()));
        });
        let dialog = BrowseDialogState {
            host_id: ArcRwSignal::new("picker-host".into()),
            browse_stream: ArcRwSignal::new(StreamPath("/browse/picker-test".into())),
            purpose,
            include_hidden: ArcRwSignal::new(false),
            platform: ArcRwSignal::new(Some(protocol::HostPlatform::Linux)),
            separator: ArcRwSignal::new('/'),
            home: ArcRwSignal::new(Some(HostAbsPath("/isolated/home".into()))),
            current_path: ArcRwSignal::new(Some(HostAbsPath("/isolated/home".into()))),
            parent: ArcRwSignal::new(Some(HostAbsPath("/isolated".into()))),
            entries: ArcRwSignal::new(Vec::new()),
            error: ArcRwSignal::new(None),
            loading: ArcRwSignal::new(false),
            operation: ArcRwSignal::new(None),
            awaiting_submission: ArcRwSignal::new(false),
        };
        state.browse_dialog.set(Some(dialog.clone()));
        let document = web_sys::window().unwrap().document().unwrap();
        let container: HtmlElement = document.create_element("div").unwrap().dyn_into().unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        (state, dialog, container)
    }

    #[wasm_bindgen_test]
    async fn typed_target_without_enter_never_submits_opening_home() {
        let (state, _, container) = fixture(BrowsePurpose::OpenProject {
            allow_host_switch: false,
        });
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <HostBrowser /> }
        });
        tick().await;
        input(&container, "/isolated/typed-project");
        tick().await;
        let buttons = container.query_selector_all("button").unwrap();
        let confirm = (0..buttons.length())
            .filter_map(|i| buttons.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|b| {
                b.text_content().unwrap_or_default().contains("Project")
                    && b.text_content().unwrap_or_default() != "Cancel"
            })
            .unwrap();
        confirm.click();
        tick().await;
        crate::send::settle_stream("picker-host", &StreamPath("/browse/picker-test".into())).await;
        crate::send::settle_stream("picker-host", &StreamPath("/host/picker".into())).await;
        let sent = frames();
        assert!(
            !sent
                .iter()
                .any(|frame| frame.kind == FrameKind::ProjectCreate),
            "confirmation must not bypass browse target validation"
        );
        assert!(
            sent.iter()
                .any(|frame| frame.kind.to_string() == "host_browse_submit"
                    && frame
                        .payload
                        .to_string()
                        .contains("/isolated/typed-project")),
            "typed target must reach the browse submission boundary without Enter"
        );
        drop(handle);
        container.remove();
    }
    fn receive<T: serde::Serialize>(state: &AppState, seq: u64, kind: FrameKind, payload: &T) {
        crate::dispatch::dispatch_envelope(
            state,
            "picker-host",
            protocol::Envelope::from_payload(
                StreamPath("/browse/picker-test".into()),
                kind,
                seq,
                payload,
            )
            .unwrap(),
        );
    }

    fn operation(
        state: &AppState,
        host_seq: u64,
        browse_seq: Option<u64>,
        stream: &str,
        status: protocol::ProjectOperationState,
    ) {
        let stream = StreamPath(stream.into());
        if let Some(seq) = browse_seq {
            receive(state, seq, FrameKind::HostBrowseOperation, &stream);
        }
        crate::dispatch::dispatch_envelope(
            state,
            "picker-host",
            protocol::Envelope::from_payload(
                StreamPath("/host/picker".into()),
                FrameKind::ProjectOperation,
                host_seq,
                &protocol::ProjectOperationPayload {
                    host_id: protocol::HostFilterId("local".into()),
                    stream,
                    mutation: protocol::ProjectMutation::Create(protocol::ProjectCreatePayload {
                        name: "Target".into(),
                        roots: vec![protocol::ProjectRootPath("/isolated/pasted-target".into())],
                    }),
                    state: status,
                },
            )
            .unwrap(),
        );
    }

    fn enabled(container: &HtmlElement, label: &str) -> bool {
        !button(container, label).has_attribute("disabled")
    }

    #[wasm_bindgen_test]
    async fn draft_survives_initial_listing_and_failed_attempt_can_succeed() {
        use protocol::ProjectOperationState;
        let (state, dialog, container) = fixture(BrowsePurpose::OpenProject {
            allow_host_switch: false,
        });
        crate::dispatch::clear_host_seqs("picker-host");
        dialog.current_path.set(None);
        dialog.loading.set(true);
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <HostBrowser /> }
        });
        tick().await;
        assert!(
            !enabled(&container, "Create Project"),
            "opening is not a selection"
        );
        input(&container, "/isolated/pasted-target");
        receive(
            &state,
            0,
            FrameKind::BrowseBootstrap,
            &protocol::BrowseBootstrapPayload {
                opened: protocol::HostBrowseOpenedPayload {
                    home: HostAbsPath("/isolated/home".into()),
                    root: HostAbsPath("/".into()),
                    separator: '/',
                    platform: protocol::HostPlatform::Linux,
                },
                listing: protocol::BrowseBootstrapListing::Entries {
                    entries: protocol::HostBrowseEntriesPayload {
                        path: HostAbsPath("/isolated/home".into()),
                        parent: Some(HostAbsPath("/isolated".into())),
                        entries: vec![],
                    },
                },
            },
        );
        tick().await;
        let field: HtmlInputElement = container
            .query_selector("input")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert_eq!(
            field.value(),
            "/isolated/pasted-target",
            "late bootstrap must not overwrite a draft"
        );
        button(&container, "Create Project").click();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        assert!(
            state.browse_dialog.get_untracked().is_some(),
            "bridge send is not commitment"
        );
        operation(
            &state,
            0,
            Some(1),
            "/project-operation/attempt-a",
            ProjectOperationState::Admitted,
        );
        tick().await;
        assert!(
            state.browse_dialog.get_untracked().is_some(),
            "admission is not commitment"
        );
        assert!(!enabled(&container, "Create Project"));
        operation(
            &state,
            1,
            None,
            "/project-operation/attempt-a",
            ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Conflict,
                message: "Attempt failed at persistence".into(),
                cleanup_may_be_partial: false,
            },
        );
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Attempt failed at persistence")
        );
        assert!(enabled(&container, "Create Project"));
        input(&container, "");
        tick().await;
        assert!(
            !enabled(&container, "Create Project"),
            "clearing cannot select the opening directory"
        );
        input(&container, "/isolated/corrected-target");
        tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Attempt failed at persistence")
        );
        button(&container, "Create Project").click();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        let sent = frames();
        let targets: Vec<_> = sent
            .iter()
            .filter(|frame| frame.kind == FrameKind::HostBrowseSubmit)
            .map(|frame| {
                frame
                    .parse_payload::<protocol::HostBrowseSubmitPayload>()
                    .unwrap()
                    .path
                    .0
            })
            .collect();
        assert_eq!(
            targets,
            vec!["/isolated/pasted-target", "/isolated/corrected-target"]
        );
        operation(
            &state,
            2,
            Some(2),
            "/project-operation/attempt-b",
            ProjectOperationState::Committed { warnings: vec![] },
        );
        tick().await;
        assert!(state.browse_dialog.get_untracked().is_none());
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        drop(handle);
        container.remove();

        for purpose in [
            BrowsePurpose::OpenProject {
                allow_host_switch: false,
            },
            BrowsePurpose::AddRoot {
                project_id: protocol::ProjectId("picker-project".into()),
            },
        ] {
            let label = match &purpose {
                BrowsePurpose::OpenProject { .. } => "Create Project",
                BrowsePurpose::AddRoot { .. } => "Add Root",
            };
            let (state, dialog, container) = fixture(purpose);
            crate::dispatch::clear_host_seqs("picker-host");
            let mount_state = state.clone();
            let handle = mount_to(container.clone(), move || {
                provide_context(mount_state.clone());
                view! { <HostBrowser /> }
            });
            tick().await;
            input(&container, "/isolated/committed-target");
            tick().await;
            button(&container, label).click();
            tick().await;
            crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
            let confirm = button(&container, label);
            let dismiss = button(&container, "Close");
            let field: HtmlInputElement = container
                .query_selector("input[aria-label='Folder path']")
                .unwrap()
                .unwrap()
                .dyn_into()
                .unwrap();
            operation(
                &state,
                0,
                Some(0),
                "/project-operation/committed-warning",
                ProjectOperationState::Committed {
                    warnings: vec![protocol::ProjectOperationWarning {
                        stage: protocol::ProjectOperationStage::Fanout,
                        message: "Project saved, but subscription publication failed".into(),
                    }],
                },
            );
            tick().await;
            let text = container.text_content().unwrap();
            let retained = state.browse_dialog.get_untracked().is_some();
            let disabled = confirm.has_attribute("disabled") && field.disabled();
            confirm.click();
            tick().await;
            crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
            let submissions = frames()
                .iter()
                .filter(|frame| frame.kind == FrameKind::HostBrowseSubmit)
                .count();
            dismiss.click();
            tick().await;
            crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
            let dismissed = state.browse_dialog.get_untracked().is_none();
            drop(handle);
            container.remove();
            assert!(dismissed, "Close must dismiss the committed warning");
            assert!(
                retained,
                "committed warnings must remain visible until dismissed"
            );
            assert!(text.contains("Project saved, but subscription publication failed"));
            assert!(text.contains("Close") && !text.contains("Cancel"));
            assert!(disabled, "a committed mutation must never offer retry");
            assert_eq!(
                submissions, 1,
                "committed warnings must not resubmit the mutation"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn add_root_preflight_browse_invalid_correction_home_and_cancel() {
        let project_id = protocol::ProjectId("picker-project".into());
        let (state, dialog, container) = fixture(BrowsePurpose::AddRoot {
            project_id: project_id.clone(),
        });
        crate::dispatch::clear_host_seqs("picker-host");
        state.projects.update(|projects| {
            projects.push(crate::state::ProjectInfo {
                host_id: "picker-host".into(),
                project: protocol::Project {
                    id: project_id.clone(),
                    name: "Selected project".into(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone {
                        roots: vec![protocol::ProjectRootPath("/isolated/registered".into())],
                    },
                },
            })
        });
        // A historical failure on the same host is not this dialog's attempt.
        crate::dispatch::dispatch_envelope(
            &state,
            "picker-host",
            protocol::Envelope::from_payload(
                StreamPath("/host/picker".into()),
                FrameKind::ProjectOperation,
                0,
                &protocol::ProjectOperationPayload {
                    host_id: protocol::HostFilterId("local".into()),
                    stream: StreamPath("/project-operation/old".into()),
                    mutation: protocol::ProjectMutation::Create(protocol::ProjectCreatePayload {
                        name: "Old".into(),
                        roots: vec![protocol::ProjectRootPath("/isolated/other".into())],
                    }),
                    state: protocol::ProjectOperationState::Failed {
                        code: protocol::CommandErrorCode::Conflict,
                        message: "Unrelated historical error".into(),
                        cleanup_may_be_partial: false,
                    },
                },
            )
            .unwrap(),
        );
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <HostBrowser /> }
        });
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Add Root to “Selected project”")
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Unrelated historical error")
        );
        input(&container, "/isolated/registered");
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Already registered")
        );
        assert!(!enabled(&container, "Add Root"));
        input(&container, "/isolated/missing");
        let field: HtmlInputElement = container
            .query_selector("input")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        let init = web_sys::KeyboardEventInit::new();
        init.set_key("Enter");
        field
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        assert!(
            frames()
                .iter()
                .all(|frame| frame.kind != FrameKind::HostBrowseSubmit),
            "Enter only browses"
        );
        receive(
            &state,
            0,
            FrameKind::HostBrowseError,
            &HostBrowseErrorPayload {
                path: HostAbsPath("/isolated/missing".into()),
                code: HostBrowseErrorCode::NotFound,
                message: "Missing directory".into(),
            },
        );
        tick().await;
        assert_eq!(
            field.value(),
            "/isolated/missing",
            "a failed browse cannot select the prior directory"
        );
        input(&container, "/isolated/pasted-root");
        tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Missing directory")
        );
        button(&container, "Add Root").click();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        let sent = frames();
        let submitted = sent
            .iter()
            .find(|frame| frame.kind == FrameKind::HostBrowseSubmit)
            .unwrap()
            .parse_payload::<protocol::HostBrowseSubmitPayload>()
            .unwrap();
        assert_eq!(submitted.path.0, "/isolated/pasted-root");
        assert!(
            matches!(submitted.intent, protocol::HostBrowseIntent::AddRoot { project_id: id } if id == project_id)
        );
        crate::dispatch::dispatch_envelope(
            &state,
            "picker-host",
            protocol::Envelope::from_payload(
                StreamPath("/host/picker".into()),
                FrameKind::CommandError,
                1,
                &protocol::CommandErrorPayload {
                    stream: dialog.browse_stream.get_untracked(),
                    request_kind: FrameKind::HostBrowseSubmit,
                    operation: "host_browse_submit".into(),
                    code: protocol::CommandErrorCode::Internal,
                    message: "Unknown submission failure".into(),
                    workbench_reason: None,
                    fatal: false,
                    request_id: None,
                    context: None,
                },
            )
            .unwrap(),
        );
        tick().await;
        assert!(
            enabled(&container, "Add Root"),
            "typed command errors must release pending presentation"
        );
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Unknown submission failure")
        );
        input(&container, "/isolated/corrected-root");
        tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Unknown submission failure")
        );
        button(&container, "Add Root").click();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        button(&container, "Close").click();
        tick().await;
        operation(
            &state,
            2,
            Some(1),
            "/project-operation/closed-attempt",
            protocol::ProjectOperationState::Failed {
                code: protocol::CommandErrorCode::Conflict,
                message: "Late closed-attempt error".into(),
                cleanup_may_be_partial: false,
            },
        );
        assert!(state.browse_dialog.get_untracked().is_none());
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        drop(handle);
        container.remove();

        let (state, dialog, container) = fixture(BrowsePurpose::OpenProject {
            allow_host_switch: false,
        });
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <HostBrowser /> }
        });
        tick().await;
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Late closed-attempt error")
        );
        button(&container, "Select /isolated/home").click();
        tick().await;
        assert!(
            enabled(&container, "Create Project"),
            "intentional home remains selectable"
        );
        button(&container, "Create Project").click();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        assert!(frames().iter().any(|frame| {
            frame.kind == FrameKind::HostBrowseSubmit
                && frame
                    .parse_payload::<protocol::HostBrowseSubmitPayload>()
                    .unwrap()
                    .path
                    .0
                    == "/isolated/home"
        }));
        button(&container, "Close").click();
        tick().await;
        crate::send::settle_stream("picker-host", &dialog.browse_stream.get_untracked()).await;
        drop(handle);
        container.remove();
    }
}
