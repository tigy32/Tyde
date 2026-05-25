use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, EmptyState, Pill, PillTone, Spinner,
};
use crate::state::{AppState, HostBrowseSession, LocalHostId};

/// Full-screen host filesystem browser overlay. Driven by
/// `state.host_browses` for the active browse stream. Lets the user
/// pick a directory; the caller decides what to do with the result via
/// `on_select`.
///
/// Read-only on v1 — we don't expose project add/create from here.
/// Production tightening can route the result through the dispatcher.
#[component]
pub fn HostBrowser(
    host: LocalHostId,
    browse_stream: protocol::StreamPath,
    on_close: Callback<()>,
    on_select: Callback<protocol::HostAbsPath>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    // Current path the user has navigated to. Falls back to opened.root.
    let current_path: RwSignal<Option<protocol::HostAbsPath>> = RwSignal::new(None);
    let include_hidden: RwSignal<bool> = RwSignal::new(false);

    let session_for_render = (host.clone(), browse_stream.clone());
    let state_for_render = state.clone();
    let session = move || {
        state_for_render
            .host_browses
            .with(|browses| browses.get(&session_for_render).cloned())
    };

    let nav = {
        let host = host.clone();
        let browse_stream = browse_stream.clone();
        let state_for_nav = state.clone();
        move |path: protocol::HostAbsPath| {
            current_path.set(Some(path.clone()));
            // Only dispatch the outbound list when we're connected.
            // Headless tests stub by pre-seeding `host_browses` and
            // don't need to reach the bridge.
            let connected = state_for_nav
                .host_streams
                .with_untracked(|streams| streams.contains_key(&host));
            if !connected {
                return;
            }
            let host = host.clone();
            let stream = browse_stream.clone();
            let hidden = include_hidden.get_untracked();
            spawn_local(async move {
                if let Err(e) =
                    crate::actions::list_host_browse_path(&host, stream, path, hidden).await
                {
                    log::error!("host_browse_list failed: {e}");
                }
            });
        }
    };

    let nav_for_initial = nav.clone();
    {
        let state = state.clone();
        let key = (host.clone(), browse_stream.clone());
        Effect::new(move |_| {
            // When the host pushes `HostBrowseOpened` it includes a root.
            // If we haven't navigated yet, jump there.
            let opened = state
                .host_browses
                .with(|browses| browses.get(&key).and_then(|s| s.opened.clone()));
            if current_path.get_untracked().is_none()
                && let Some(opened) = opened
            {
                nav_for_initial(opened.root);
            }
        });
    }

    let on_close_btn = on_close;
    let on_toggle_hidden = {
        let nav = nav.clone();
        Callback::new(move |_: ()| {
            let new_val = !include_hidden.get_untracked();
            include_hidden.set(new_val);
            if let Some(path) = current_path.get_untracked() {
                nav(path);
            }
        })
    };

    let session_for_use = session.clone();
    let on_use_folder = Callback::new(move |_: ()| {
        if let Some(path) = current_path.get_untracked() {
            on_select.run(path);
        }
    });

    view! {
        <div class="host-browser" role="dialog" aria-label="Host filesystem browser" data-mobile-test="host-browser">
            <div class="host-browser-header">
                <div class="host-browser-title">"Browse host"</div>
                <Button
                    label="Cancel"
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Compact
                    data_mobile_test="host-browser-cancel"
                    on_click=on_close_btn
                />
            </div>
            <div class="host-browser-path-bar" data-mobile-test="host-browser-path">
                {move || {
                    let path = current_path.get();
                    let display = path
                        .as_ref()
                        .map(|p| p.0.clone())
                        .unwrap_or_else(|| "(loading…)".to_string());
                    view! { <span class="host-browser-path-text">{display}</span> }
                }}
            </div>
            <div class="host-browser-controls">
                {move || {
                    let label = if include_hidden.get() { "Hide dotfiles" } else { "Show dotfiles" };
                    view! {
                        <Button
                            label=label
                            variant=ButtonVariant::Ghost
                            size=ButtonSize::Compact
                            data_mobile_test="host-browser-toggle-hidden"
                            on_click=on_toggle_hidden
                        />
                    }
                }}
                <Button
                    label="Use this folder"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="host-browser-use-folder"
                    on_click=on_use_folder
                />
            </div>
            <div class="host-browser-body">
                {move || {
                    let session = session_for_use();
                    render_body(session, current_path.get(), nav.clone())
                }}
            </div>
        </div>
    }
}

fn render_body(
    session: Option<HostBrowseSession>,
    current: Option<protocol::HostAbsPath>,
    nav: impl Fn(protocol::HostAbsPath) + Clone + 'static,
) -> AnyView {
    let Some(session) = session else {
        return view! {
            <div class="host-browser-loading" data-mobile-test="host-browser-loading">
                <Spinner aria_label="Opening host browser".to_string() />
            </div>
        }
        .into_any();
    };

    if let Some(error) = session.latest_error {
        return view! {
            <div class="host-browser-error" role="alert" data-mobile-test="host-browser-error">
                <Pill
                    label=format!("{:?}", error.code)
                    tone=PillTone::Error
                />
                <p class="host-browser-error-text">{error.message}</p>
            </div>
        }
        .into_any();
    }

    let Some(current) = current else {
        return view! {
            <div class="host-browser-loading" data-mobile-test="host-browser-loading">
                <Spinner aria_label="Opening host browser".to_string() />
            </div>
        }
        .into_any();
    };

    let Some(entries) = session.entries_by_path.get(&current).cloned() else {
        return view! {
            <div class="host-browser-loading" data-mobile-test="host-browser-loading">
                <Spinner aria_label="Listing directory".to_string() />
            </div>
        }
        .into_any();
    };

    if entries.entries.is_empty() {
        return view! {
            <EmptyState
                title="Empty directory"
                body="Nothing here. Pick a different folder or use this one as-is."
                icon="\u{1F4C2}"
                data_mobile_test="host-browser-empty"
            />
        }
        .into_any();
    }

    let parent = entries.parent.clone();
    let mut rows = entries.entries.clone();
    rows.sort_by(|a, b| {
        let a_dir = matches!(a.kind, protocol::ProjectFileKind::Directory);
        let b_dir = matches!(b.kind, protocol::ProjectFileKind::Directory);
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });

    let nav_for_parent = nav.clone();
    let parent_row = parent.map(|parent| {
        view! {
            <div
                class="host-browser-row host-browser-row-parent"
                role="button"
                tabindex="0"
                data-mobile-test="host-browser-row-parent"
                on:click=move |_| nav_for_parent(parent.clone())
            >
                <span class="host-browser-row-icon" aria-hidden="true">"\u{21B0}"</span>
                <span class="host-browser-row-name">"Up one level"</span>
            </div>
        }
    });

    let current_for_nav = current.clone();
    view! {
        <div class="host-browser-list">
            {parent_row}
            {rows.into_iter().map(|entry| {
                let is_dir = matches!(entry.kind, protocol::ProjectFileKind::Directory);
                let icon = if is_dir { "\u{1F4C1}" } else { "\u{1F4C4}" };
                let class = if is_dir { "host-browser-row host-browser-row-dir" } else { "host-browser-row host-browser-row-file" };
                let test = if is_dir { "host-browser-row-dir" } else { "host-browser-row-file" };
                let nav = nav.clone();
                let current = current_for_nav.clone();
                let name_for_nav = entry.name.clone();
                let perm_pill = entry.entry_error.map(|err| {
                    view! {
                        <Pill
                            label=format!("{:?}", err)
                            tone=PillTone::Warning
                        />
                    }
                });
                view! {
                    <div
                        class=class
                        role=if is_dir { "button" } else { "group" }
                        tabindex=if is_dir { "0" } else { "-1" }
                        data-mobile-test=test
                        on:click=move |_| {
                            if is_dir {
                                let child = join_host_path(&current, &name_for_nav);
                                nav(child);
                            }
                        }
                    >
                        <span class="host-browser-row-icon" aria-hidden="true">{icon}</span>
                        <span class="host-browser-row-name">{entry.name}</span>
                        {perm_pill}
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
    .into_any()
}

fn join_host_path(base: &protocol::HostAbsPath, name: &str) -> protocol::HostAbsPath {
    let joined = if base.0.ends_with('/') || base.0.ends_with('\\') {
        format!("{}{}", base.0, name)
    } else if base.0.contains('\\') && !base.0.contains('/') {
        format!("{}\\{}", base.0, name)
    } else {
        format!("{}/{}", base.0, name)
    };
    protocol::HostAbsPath(joined)
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, HostBrowseSession, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{
        HostAbsPath, HostBrowseEntriesPayload, HostBrowseEntry, HostBrowseErrorCode,
        HostBrowseErrorPayload, HostBrowseOpenedPayload, HostPlatform, ProjectFileKind, StreamPath,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// No session → loading spinner.
    #[wasm_bindgen_test]
    async fn host_browser_shows_loading_initially() {
        let host = LocalHostId("host-1".to_owned());
        let stream = StreamPath("/browse/m1".to_owned());
        let host_for_mount = host.clone();
        let stream_for_mount = stream.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <HostBrowser
                    host=host_for_mount.clone()
                    browse_stream=stream_for_mount.clone()
                    on_close=Callback::new(|_| {})
                    on_select=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='host-browser-loading']")
                .unwrap()
                .is_some(),
            "loading must render before the host pushes Opened/Entries"
        );
    }

    /// Opened + Entries → rows render with the directory entry getting a
    /// dir test selector and file getting the file selector.
    #[wasm_bindgen_test]
    async fn host_browser_renders_rows_after_entries() {
        let host = LocalHostId("host-1".to_owned());
        let stream = StreamPath("/browse/m1".to_owned());
        let host_for_mount = host.clone();
        let stream_for_mount = stream.clone();
        let root = HostAbsPath("/home/dev".to_owned());
        let entries = HostBrowseEntriesPayload {
            path: root.clone(),
            parent: Some(HostAbsPath("/home".to_owned())),
            entries: vec![
                HostBrowseEntry {
                    name: "projects".to_owned(),
                    kind: ProjectFileKind::Directory,
                    size: None,
                    mtime_ms: None,
                    is_hidden: false,
                    symlink_target: None,
                    entry_error: None,
                },
                HostBrowseEntry {
                    name: ".bashrc".to_owned(),
                    kind: ProjectFileKind::File,
                    size: Some(123),
                    mtime_ms: None,
                    is_hidden: true,
                    symlink_target: None,
                    entry_error: None,
                },
            ],
        };
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let session = HostBrowseSession {
                local_host_id: host.clone(),
                stream: stream.clone(),
                opened: Some(HostBrowseOpenedPayload {
                    home: root.clone(),
                    root: root.clone(),
                    separator: '/',
                    platform: HostPlatform::Macos,
                }),
                entries_by_path: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(root.clone(), entries.clone());
                    m
                },
                latest_error: None,
            };
            state.host_browses.update(|map| {
                map.insert((host.clone(), stream.clone()), session);
            });
            provide_context(state);
            view! {
                <HostBrowser
                    host=host_for_mount.clone()
                    browse_stream=stream_for_mount.clone()
                    on_close=Callback::new(|_| {})
                    on_select=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='host-browser-row-dir']")
                .unwrap()
                .is_some(),
            "directory row must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='host-browser-row-file']")
                .unwrap()
                .is_some(),
            "file row must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='host-browser-row-parent']")
                .unwrap()
                .is_some(),
            "parent row must render when entries.parent is Some"
        );
    }

    /// Error payload surfaces inline rather than crashing or leaving
    /// the user stuck on the loading state.
    #[wasm_bindgen_test]
    async fn host_browser_surfaces_error_inline() {
        let host = LocalHostId("host-1".to_owned());
        let stream = StreamPath("/browse/m1".to_owned());
        let host_for_mount = host.clone();
        let stream_for_mount = stream.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let session = HostBrowseSession {
                local_host_id: host.clone(),
                stream: stream.clone(),
                opened: None,
                entries_by_path: Default::default(),
                latest_error: Some(HostBrowseErrorPayload {
                    path: HostAbsPath("/private".to_owned()),
                    code: HostBrowseErrorCode::PermissionDenied,
                    message: "Permission denied".to_owned(),
                }),
            };
            state.host_browses.update(|map| {
                map.insert((host.clone(), stream.clone()), session);
            });
            provide_context(state);
            view! {
                <HostBrowser
                    host=host_for_mount.clone()
                    browse_stream=stream_for_mount.clone()
                    on_close=Callback::new(|_| {})
                    on_select=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        let error = container
            .query_selector("[data-mobile-test='host-browser-error']")
            .unwrap()
            .expect("error must surface inline");
        assert!(
            error
                .text_content()
                .unwrap_or_default()
                .contains("Permission denied"),
            "error message must render"
        );
    }
}
