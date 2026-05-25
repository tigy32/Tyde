use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{Button, ButtonSize, ButtonVariant, Pill, PillTone, Spinner};
use crate::state::{ActiveProjectRef, AppState, ProjectFileRef};

const LARGE_FILE_THRESHOLD_BYTES: usize = 1_048_576;

/// Mobile file viewer that backs the Projects file-row tap. Owns its
/// own request lifecycle: on mount, dispatches `ProjectReadFile` if the
/// `(host, project, path)` triple isn't already cached in
/// `state.project_file_contents`. Shows distinct loading / binary /
/// large-file / error states.
///
/// Read-only: there is no edit affordance, no syntax highlighting (the
/// wasm bundle stays small), and no line numbers (they pollute mobile
/// text selection / clipboard copy).
#[component]
pub fn FileViewer(
    project: ActiveProjectRef,
    path: protocol::ProjectPath,
    on_close: Callback<()>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let key = ProjectFileRef {
        local_host_id: project.local_host_id.clone(),
        project_id: project.project_id.clone(),
        path: path.clone(),
    };

    // Kick the request on mount if we don't already have contents.
    {
        let project = project.clone();
        let path = path.clone();
        let key = key.clone();
        let state = state.clone();
        Effect::new(move |_| {
            let already_cached = state
                .project_file_contents
                .with_untracked(|files| files.contains_key(&key));
            if already_cached {
                return;
            }
            // Don't dispatch if we're not connected. This also keeps
            // headless wasm tests from blowing up trying to invoke the
            // absent Tauri bridge.
            let connected = state
                .host_streams
                .with_untracked(|streams| streams.contains_key(&project.local_host_id));
            if !connected {
                return;
            }
            let project = project.clone();
            let path = path.clone();
            spawn_local(async move {
                if let Err(e) = crate::actions::request_project_file(&project, path).await {
                    log::error!("request_project_file failed: {e}");
                }
            });
        });
    }

    let key_for_render = key.clone();
    let entry = move || {
        state
            .project_file_contents
            .with(|files| files.get(&key_for_render).cloned())
    };

    // Refresh action: refetch the same file. Useful for the "we have it
    // but it could be stale" case (mobile users may sit on a file while
    // the host edits it).
    let project_for_refresh = project.clone();
    let path_for_refresh = path.clone();
    let on_refresh = Callback::new(move |_: ()| {
        let project = project_for_refresh.clone();
        let path = path_for_refresh.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::request_project_file(&project, path).await {
                log::error!("request_project_file (refresh) failed: {e}");
            }
        });
    });

    let root_label = short_root(&path.root.0);
    let relative_path_display = path.relative_path.clone();

    view! {
        <div class="project-file-viewer" data-mobile-test="project-file-viewer">
            <div class="project-file-viewer-header">
                <div class="project-file-viewer-pathline">
                    <span
                        class="project-file-viewer-root"
                        data-mobile-test="project-file-viewer-root"
                    >{root_label}</span>
                    <span class="project-file-viewer-sep" aria-hidden="true">"/"</span>
                    <span
                        class="project-file-viewer-path"
                        data-mobile-test="project-file-viewer-path"
                    >{relative_path_display}</span>
                </div>
                <span style="display: flex; gap: var(--space-1);">
                    <Button
                        label="Refresh"
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Compact
                        data_mobile_test="project-file-viewer-refresh"
                        aria_label="Refresh file contents".to_string()
                        on_click=on_refresh
                    />
                    <Button
                        label="Close"
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Compact
                        data_mobile_test="project-file-viewer-close"
                        aria_label="Close file viewer".to_string()
                        on_click=on_close
                    />
                </span>
            </div>
            <div class="project-file-viewer-body" role="region" aria-label="File contents">
                {move || render_body(entry())}
            </div>
        </div>
    }
}

fn render_body(entry: Option<crate::state::ProjectFileState>) -> AnyView {
    let Some(entry) = entry else {
        return view! {
            <div class="project-file-viewer-loading" data-mobile-test="project-file-viewer-loading">
                <Spinner aria_label="Loading file".to_string() />
                <span class="project-file-viewer-loading-text">"Loading file…"</span>
            </div>
        }
        .into_any();
    };

    if entry.is_binary {
        return view! {
            <div class="project-file-viewer-state" data-mobile-test="project-file-viewer-binary">
                <Pill
                    label="Binary file"
                    tone=PillTone::Warning
                    data_mobile_test="project-file-viewer-binary-pill"
                />
                <p class="project-file-viewer-hint">
                    "Binary files don't render on mobile. Open the file on desktop for a hex/preview view."
                </p>
            </div>
        }
        .into_any();
    }

    let Some(contents) = entry.contents else {
        return view! {
            <div class="project-file-viewer-state" data-mobile-test="project-file-viewer-missing">
                <Pill
                    label="No contents"
                    tone=PillTone::Error
                    data_mobile_test="project-file-viewer-missing-pill"
                />
                <p class="project-file-viewer-hint">
                    "The host returned no contents for this file. It may have been deleted, moved,
                    or the host lacks read permission."
                </p>
            </div>
        }
        .into_any();
    };

    if contents.len() > LARGE_FILE_THRESHOLD_BYTES {
        let size = format_bytes(contents.len());
        return view! {
            <div class="project-file-viewer-state" data-mobile-test="project-file-viewer-large">
                <Pill
                    label=format!("Large file · {size}")
                    tone=PillTone::Warning
                    data_mobile_test="project-file-viewer-large-pill"
                />
                <p class="project-file-viewer-hint">
                    "This file is large enough that rendering it on a phone is impractical. Open
                    the file on desktop, or ask the assistant to summarize the parts you need."
                </p>
            </div>
        }
        .into_any();
    }

    let byte_label = format_bytes(contents.len());
    let line_count = contents.lines().count();
    view! {
        <div class="project-file-viewer-ready">
            <div class="project-file-viewer-meta" data-mobile-test="project-file-viewer-meta">
                <Pill
                    label=format!("{line_count} lines")
                    tone=PillTone::Neutral
                />
                <Pill
                    label=byte_label
                    tone=PillTone::Neutral
                />
            </div>
            // <pre> is the mobile-friendliest renderer: it preserves
            // selection/copy on iOS Safari + Android Chrome WebView,
            // and -webkit-user-select keeps highlighting visible.
            <pre
                class="project-file-viewer-pre"
                data-mobile-test="project-file-viewer-contents"
            >{contents}</pre>
        </div>
    }
    .into_any()
}

fn short_root(root: &str) -> String {
    root.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(root)
        .to_owned()
}

fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1_048_576 {
        format!("{:.1} KB", (n as f64) / 1024.0)
    } else {
        format!("{:.1} MB", (n as f64) / 1_048_576.0)
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId, ProjectFileState};
    use leptos::mount::mount_to;
    use protocol::{ProjectId, ProjectPath, ProjectRootPath};
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

    fn make_project_ref(host: &LocalHostId, project: &str) -> ActiveProjectRef {
        ActiveProjectRef {
            local_host_id: host.clone(),
            project_id: ProjectId(project.to_owned()),
        }
    }

    fn make_path(root: &str, rel: &str) -> ProjectPath {
        ProjectPath {
            root: ProjectRootPath(root.to_owned()),
            relative_path: rel.to_owned(),
        }
    }

    /// Empty state cache → loading spinner appears.
    #[wasm_bindgen_test]
    async fn file_viewer_shows_loading_before_contents() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project_ref(&host, "p-1");
        let path = make_path("/x", "src/main.rs");
        let project_for_mount = project.clone();
        let path_for_mount = path.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <FileViewer
                    project=project_for_mount.clone()
                    path=path_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer-loading']")
                .unwrap()
                .is_some(),
            "loading state must render before contents arrive"
        );
    }

    /// Cached contents render in a <pre> with a meta row containing line count + size.
    #[wasm_bindgen_test]
    async fn file_viewer_renders_text_contents_with_meta() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project_ref(&host, "p-1");
        let path = make_path("/x", "README.md");
        let key = ProjectFileRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            path: path.clone(),
        };
        let project_for_mount = project.clone();
        let path_for_mount = path.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_file_contents.update(|files| {
                files.insert(
                    key.clone(),
                    ProjectFileState {
                        path: path.clone(),
                        contents: Some("alpha\nbeta\ngamma\n".to_owned()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! {
                <FileViewer
                    project=project_for_mount.clone()
                    path=path_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        let pre = container
            .query_selector("[data-mobile-test='project-file-viewer-contents']")
            .unwrap()
            .expect("contents pre must render");
        let text = pre.text_content().unwrap_or_default();
        assert!(
            text.contains("alpha") && text.contains("beta") && text.contains("gamma"),
            "contents must render verbatim, got: {text}"
        );
        let meta = container
            .query_selector("[data-mobile-test='project-file-viewer-meta']")
            .unwrap()
            .expect("meta row must render");
        let meta_text = meta.text_content().unwrap_or_default();
        assert!(
            meta_text.contains("3 lines"),
            "meta must surface line count, got: {meta_text}"
        );
    }

    /// `is_binary` payload → binary state, not the pre.
    #[wasm_bindgen_test]
    async fn file_viewer_shows_binary_notice_for_binary_files() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project_ref(&host, "p-1");
        let path = make_path("/x", "image.png");
        let key = ProjectFileRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            path: path.clone(),
        };
        let project_for_mount = project.clone();
        let path_for_mount = path.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_file_contents.update(|files| {
                files.insert(
                    key.clone(),
                    ProjectFileState {
                        path: path.clone(),
                        contents: None,
                        is_binary: true,
                    },
                );
            });
            provide_context(state);
            view! {
                <FileViewer
                    project=project_for_mount.clone()
                    path=path_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer-binary']")
                .unwrap()
                .is_some(),
            "binary state must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer-contents']")
                .unwrap()
                .is_none(),
            "the <pre> must not render for binary files"
        );
    }

    /// Files over the LARGE threshold render the large-file callout
    /// rather than blasting the DOM with the bytes.
    #[wasm_bindgen_test]
    async fn file_viewer_shows_large_file_notice() {
        let host = LocalHostId("host-1".to_owned());
        let project = make_project_ref(&host, "p-1");
        let path = make_path("/x", "huge.log");
        let key = ProjectFileRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
            path: path.clone(),
        };
        let project_for_mount = project.clone();
        let path_for_mount = path.clone();
        let container = make_container();
        let huge = "x".repeat(LARGE_FILE_THRESHOLD_BYTES + 1);
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.project_file_contents.update(|files| {
                files.insert(
                    key.clone(),
                    ProjectFileState {
                        path: path.clone(),
                        contents: Some(huge.clone()),
                        is_binary: false,
                    },
                );
            });
            provide_context(state);
            view! {
                <FileViewer
                    project=project_for_mount.clone()
                    path=path_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer-large']")
                .unwrap()
                .is_some(),
            "large-file state must render for >1 MB contents"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='project-file-viewer-contents']")
                .unwrap()
                .is_none(),
            "the <pre> must not render for large files"
        );
    }
}
