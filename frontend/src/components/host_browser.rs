use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    FrameKind, HostAbsPath, HostBrowseClosePayload, HostBrowseListPayload, HostBrowseStartPayload,
    ProjectCreatePayload, ProjectFileKind, StreamPath,
};

use crate::send::send_frame;
use crate::state::{AppState, BrowseDialogState, BrowsePurpose};

fn new_browse_stream() -> StreamPath {
    let id = (js_sys::Math::random() * 1_000_000_000_000.0) as u64;
    StreamPath(format!("/browse/{id}"))
}

pub fn open_project_browser(state: &AppState) {
    let Some((host_id, _host_stream)) = state.selected_host_stream_untracked() else {
        log::error!("cannot open browser without a selected connected host");
        return;
    };

    let browse_stream = new_browse_stream();
    let dialog = BrowseDialogState {
        host_id: host_id.clone(),
        browse_stream: browse_stream.clone(),
        purpose: BrowsePurpose::OpenProject,
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

    let host_stream_path = state.host_stream_untracked(&host_id);
    let Some(host_stream_path) = host_stream_path else {
        log::error!("host {host_id} has no active host stream");
        state.browse_dialog.set(None);
        return;
    };

    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            host_stream_path,
            FrameKind::HostBrowseStart,
            &HostBrowseStartPayload {
                browse_stream,
                initial: None,
                include_hidden: false,
            },
        )
        .await
        {
            log::error!("failed to send HostBrowseStart: {error}");
        }
    });
}

fn send_list(host_id: &str, browse_stream: StreamPath, path: HostAbsPath, include_hidden: bool) {
    let host_id = host_id.to_owned();
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            browse_stream,
            FrameKind::HostBrowseList,
            &HostBrowseListPayload {
                path,
                include_hidden,
            },
        )
        .await
        {
            log::error!("failed to send HostBrowseList: {error}");
        }
    });
}

fn close_dialog(state: &AppState) {
    let Some(dialog) = state.browse_dialog.get_untracked() else {
        return;
    };
    state.browse_dialog.set(None);

    let host_id = dialog.host_id.clone();
    let browse_stream = dialog.browse_stream.clone();
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

    let current_path = dialog.current_path.clone();
    let parent = dialog.parent.clone();
    let entries = dialog.entries.clone();
    let error = dialog.error.clone();
    let loading = dialog.loading.clone();
    let include_hidden = dialog.include_hidden.clone();

    let host_id = dialog.host_id.clone();
    let browse_stream = dialog.browse_stream.clone();

    let navigate_to = {
        let host_id = host_id.clone();
        let browse_stream = browse_stream.clone();
        let include_hidden = include_hidden.clone();
        let loading = loading.clone();
        move |path: HostAbsPath| {
            loading.set(true);
            send_list(
                &host_id,
                browse_stream.clone(),
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

    // Keep the address bar synced with current_path
    {
        let current_path = current_path.clone();
        Effect::new(move |_| {
            if let Some(path) = current_path.get() {
                address_input.set(path.0);
            }
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
    let on_confirm = move |_| {
        let Some(path) = current_path_for_confirm.get_untracked() else {
            return;
        };
        match dialog_purpose {
            BrowsePurpose::OpenProject => {
                let name = path
                    .0
                    .rsplit('/')
                    .find(|segment| !segment.is_empty())
                    .unwrap_or(&path.0)
                    .to_owned();
                let Some((host_id, host_stream)) =
                    state_for_confirm.selected_host_stream_untracked()
                else {
                    log::error!("cannot create project without a selected connected host");
                    return;
                };
                spawn_local(async move {
                    if let Err(error) = send_frame(
                        &host_id,
                        host_stream,
                        FrameKind::ProjectCreate,
                        &ProjectCreatePayload {
                            name,
                            roots: vec![path.0],
                        },
                    )
                    .await
                    {
                        log::error!("failed to send ProjectCreate: {error}");
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
                    />
                    <button class="browser-btn" on:click=on_toggle_hidden>{hidden_label}</button>
                </div>
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
                    >"Open Project Here"</button>
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
