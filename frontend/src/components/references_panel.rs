use std::collections::HashSet;

use leptos::prelude::*;

use protocol::{CodeIntelReferencesFileResult, ProjectPath};

use crate::actions::{clear_references, open_project_path_at_navigation};
use crate::components::center_zone::{announce, workspace_width};
use crate::components::command_palette::{
    ContextActionId, context_binding, open_to_side_availability,
};
use crate::components::find_bar::render_text_with_highlights;
use crate::state::{AppState, OpenTarget, PendingFileNavigation, ProjectReferencesMode};

/// Find-references results panel (M5), shown as the fourth tab of the left dock
/// and auto-activated by Shift+F12. Mirrors [`SearchPanel`](super::search_panel)'s
/// results-row shape: streamed results grouped by file, each row a line preview
/// with the matched ranges highlighted. Clicking a reference opens the file and
/// scrolls to the line. A header summarizes the query and offers a Clear action.
#[component]
pub fn ReferencesPanel() -> impl IntoView {
    let state = expect_context::<AppState>();

    let title = {
        let state = state.clone();
        move || {
            state.references_state.with(|s| match &s.symbol {
                Some(symbol) => match s.mode {
                    ProjectReferencesMode::References => format!("References to {symbol}"),
                    ProjectReferencesMode::DefinitionTargets => {
                        format!("Definitions for {symbol}")
                    }
                },
                None => match s.mode {
                    ProjectReferencesMode::References => "References".to_owned(),
                    ProjectReferencesMode::DefinitionTargets => "Definition targets".to_owned(),
                },
            })
        }
    };

    let summary_text = {
        let state = state.clone();
        move || {
            state.references_state.with(|s| {
                if s.mode == ProjectReferencesMode::DefinitionTargets {
                    return match s.results.iter().map(|file| file.lines.len()).sum::<usize>() {
                        0 => "No definitions".to_owned(),
                        1 => "Choose one definition target".to_owned(),
                        count => format!("Choose one of {count} definition targets"),
                    };
                }
                if let Some(err) = &s.error {
                    return format!("Error: {err}");
                }
                if s.active_references_id == 0 {
                    return String::new();
                }
                if s.cancelled {
                    return "Cancelled".to_owned();
                }
                if s.results.is_empty() {
                    return if s.in_flight {
                        "Finding references…".to_owned()
                    } else {
                        "No references".to_owned()
                    };
                }
                let suffix = if s.in_flight { " (finding…)" } else { "" };
                format!(
                    "{} reference{} in {} file{}{suffix}",
                    s.total_references,
                    plural(s.total_references),
                    s.total_files,
                    plural(s.total_files),
                )
            })
        }
    };

    let show_truncated = {
        let state = state.clone();
        move || state.references_state.with(|s| s.truncated)
    };

    let on_clear = {
        let state = state.clone();
        move |_| clear_references(&state)
    };

    // Per-file collapse state (keyed by the rendered path string).
    let collapsed: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
    let side_notice: RwSignal<Option<&'static str>> = RwSignal::new(None);

    let results_view = {
        let state = state.clone();
        move || {
            let files = state.references_state.with(|s| {
                let mut row_start = 0usize;
                s.results
                    .clone()
                    .into_iter()
                    .map(|file| {
                        let start = row_start;
                        row_start += file.lines.len();
                        (file, start)
                    })
                    .collect::<Vec<_>>()
            });
            files
                .into_iter()
                .map(|(file, row_start)| {
                    file_group(state.clone(), collapsed, side_notice, file, row_start)
                })
                .collect_view()
        }
    };

    view! {
        <div class="search-panel references-panel">
            <div class="references-header">
                <span class="references-title">{title}</span>
                <button
                    type="button"
                    class="references-clear"
                    title="Clear references"
                    aria-label="Clear references"
                    on:click=on_clear
                >"Clear"</button>
            </div>
            <div class="search-summary">{summary_text}</div>
            <Show when=show_truncated>
                <div class="search-truncated">
                    "Results truncated — some references are not shown."
                </div>
            </Show>
            <Show when=move || side_notice.get().is_some()>
                <div
                    class="cp-notice"
                    role="status"
                    data-testid="references-side-open-notice"
                >
                    {move || side_notice.get().unwrap_or_default()}
                </div>
            </Show>
            <div class="search-results">
                {results_view}
            </div>
        </div>
    }
}

fn plural(count: u32) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn path_key(path: &ProjectPath) -> String {
    format!("{}\u{0}{}", path.root.0, path.relative_path)
}

/// Render one file's group: a collapsible header (path + reference-count badge)
/// followed by one row per matching line.
fn file_group(
    state: AppState,
    collapsed: RwSignal<HashSet<String>>,
    side_notice: RwSignal<Option<&'static str>>,
    file: CodeIntelReferencesFileResult,
    row_start: usize,
) -> impl IntoView {
    let width = workspace_width();
    let key = path_key(&file.path);
    let display_name = file.path.relative_path.clone();
    let ref_count: usize = file.lines.iter().map(|line| line.ranges.len()).sum();
    let truncated = file.truncated;

    let key_for_chevron = key.clone();
    let key_for_lines = key.clone();
    let chevron_collapsed = move || collapsed.with(|set| set.contains(&key_for_chevron));
    let lines_style = move || {
        if collapsed.with(|set| set.contains(&key_for_lines)) {
            "display: none;"
        } else {
            ""
        }
    };

    let on_toggle = {
        let key = key.clone();
        move |_| {
            collapsed.update(|set| {
                if !set.remove(&key) {
                    set.insert(key.clone());
                }
            });
        }
    };

    let file_path = file.path.clone();
    let rows = file
        .lines
        .into_iter()
        .enumerate()
        .map(|(line_index, line)| {
            let ranges: Vec<(usize, usize)> = line
                .ranges
                .iter()
                .map(|range| (range.start as usize, range.end as usize))
                .collect();
            let highlighted = render_text_with_highlights(&line.line_text, &ranges);
            let line_number = line.line_number;
            let row_index = row_start + line_index;
            let availability = {
                let state = state.clone();
                Memo::new(move |_| open_to_side_availability(&state, width.get()))
            };
            let open_target = {
                let state = state.clone();
                let path = file_path.clone();
                move |open_target| {
                    let (path, navigation) = if let Some(target) = state
                        .references_state
                        .with_untracked(|s| s.row_targets.get(row_index).cloned())
                    {
                        (
                            target.path,
                            PendingFileNavigation::Offset(target.range.start),
                        )
                    } else {
                        (path.clone(), PendingFileNavigation::Line(line_number))
                    };
                    let _ = open_project_path_at_navigation(&state, path, open_target, navigation);
                }
            };
            let on_click = {
                let open_target = open_target.clone();
                move |_| {
                    side_notice.set(None);
                    open_target(OpenTarget::Focused);
                }
            };
            let open_to_side = {
                let open_target = open_target.clone();
                move || {
                    if let Some(reason) = availability.get_untracked().reason() {
                        side_notice.set(Some(reason));
                        announce(reason);
                        return;
                    }
                    side_notice.set(None);
                    open_target(OpenTarget::Beside);
                }
            };
            let on_side_click = {
                let open_to_side = open_to_side.clone();
                move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    open_to_side();
                }
            };
            let open_to_side_for_key = open_to_side.clone();
            let on_keydown = move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) {
                    ev.prevent_default();
                    ev.stop_propagation();
                    open_to_side_for_key();
                }
            };
            let side_hint = context_binding(ContextActionId::OpenToSide).chord().hint();
            let row_side_hint = side_hint.clone();
            let row_title = move || match availability.get().reason() {
                Some(reason) => {
                    format!("Open; {row_side_hint} Open to the Side unavailable: {reason}")
                }
                None => format!("Open ({row_side_hint} opens to the side)"),
            };
            let side_title = move || match availability.get().reason() {
                Some(reason) => reason.to_owned(),
                None => format!("Open to the side ({side_hint})"),
            };
            let side_label = format!(
                "Open {} at line {} to the side",
                file_path.relative_path.as_str(),
                line_number
            );
            view! {
                <div class="fe-row">
                    <button
                        class="search-match-row fe-item"
                        title=row_title
                        aria-keyshortcuts="Enter Control+Enter Meta+Enter"
                        on:click=on_click
                        on:keydown=on_keydown
                    >
                        <span class="search-match-line">{line_number.to_string()}</span>
                        <span class="search-match-text">{highlighted}</span>
                    </button>
                    <button
                        class="fe-open-side search-result-open-side"
                        class:disabled=move || !availability.get().is_enabled()
                        aria-label=side_label
                        aria-keyshortcuts="Control+Enter Meta+Enter"
                        aria-disabled=move || {
                            (!availability.get().is_enabled()).then_some("true")
                        }
                        title=side_title
                        on:click=on_side_click
                    >
                        <span class="fe-open-side-icon" aria-hidden="true">"\u{29c9}"</span>
                    </button>
                </div>
            }
        })
        .collect_view();

    let badge = if truncated {
        format!("{ref_count}+")
    } else {
        ref_count.to_string()
    };

    view! {
        <div class="search-file-group">
            <button class="search-file-header" on:click=on_toggle>
                <span class="search-file-chevron">
                    {move || if chevron_collapsed() { "\u{25b8}" } else { "\u{25be}" }}
                </span>
                <span class="search-file-name">{display_name}</span>
                <span class="search-file-badge">{badge}</span>
            </button>
            <div class="search-file-matches" style=lines_style>{rows}</div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{
        ActiveProjectRef, AppState, FileResourceKey, OpenFile, PaneId, ProjectReferencesUiState,
        TabContent,
    };
    use leptos::mount::mount_to;
    use protocol::{
        ByteRange, CodeIntelReferenceLine, CodeIntelReferencesFileResult, ProjectFileVersion,
        ProjectId, ProjectPath, ProjectRootPath,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-references")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-references");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 320px; height: 600px; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
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

    fn file_result(
        relative: &str,
        lines: &[(u32, &str, (u32, u32))],
        truncated: bool,
    ) -> CodeIntelReferencesFileResult {
        CodeIntelReferencesFileResult {
            path: ProjectPath {
                root: ProjectRootPath("test-root".to_owned()),
                relative_path: relative.to_owned(),
            },
            lines: lines
                .iter()
                .map(|(line_number, text, range)| CodeIntelReferenceLine {
                    line_number: *line_number,
                    line_text: (*text).to_owned(),
                    ranges: vec![ByteRange {
                        start: range.0,
                        end: range.1,
                    }],
                })
                .collect(),
            truncated,
        }
    }

    fn query_count(container: &HtmlElement, selector: &str) -> usize {
        container.query_selector_all(selector).unwrap().length() as usize
    }

    fn file_key(project: &str, path: ProjectPath) -> FileResourceKey {
        FileResourceKey {
            host_id: "host".to_owned(),
            project_id: ProjectId(project.to_owned()),
            path,
        }
    }

    fn seed_loaded_file(state: &AppState, key: &FileResourceKey, contents: &str) {
        state.open_files.update(|files| {
            files.insert(
                key.clone(),
                OpenFile {
                    path: key.path.clone(),
                    version: ProjectFileVersion(1),
                    contents: Some(contents.to_owned()),
                    is_binary: false,
                },
            );
        });
    }

    fn dispatch_side_enter(target: &HtmlElement) {
        let init = web_sys::KeyboardEventInit::new();
        init.set_bubbles(true);
        init.set_key("Enter");
        init.set_meta_key(true);
        let event =
            web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap();
        target.dispatch_event(&event).unwrap();
    }

    fn mount_panel(setup: impl Fn(&AppState) + 'static) -> (HtmlElement, AppState) {
        workspace_width().set(None);
        let container = make_container();
        let captured: Rc<RefCell<Option<AppState>>> = Rc::new(RefCell::new(None));
        let captured_for_mount = captured.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.pending_goto_line.set(None);
            state.pending_goto_offset.set(None);
            setup(&state);
            *captured_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state.clone());
            view! { <ReferencesPanel /> }
        });
        let state = captured.borrow().clone().unwrap();
        std::mem::forget(_handle);
        (container, state)
    }

    /// CodeIntelReferencesResults frames populate the panel: one group per file,
    /// one row per matching line, and a summary that reflects the totals.
    #[wasm_bindgen_test]
    async fn references_results_populate_panel() {
        ensure_styles_loaded();
        let (container, _state) = mount_panel(|state| {
            state.references_state.set(ProjectReferencesUiState {
                active_references_id: 1,
                in_flight: false,
                symbol: Some("foo".to_owned()),
                results: vec![
                    file_result(
                        "src/a.rs",
                        &[(3, "fn foo() {}", (3, 6)), (8, "foo();", (0, 3))],
                        false,
                    ),
                    file_result("src/b.rs", &[(12, "foo();", (0, 3))], false),
                ],
                total_files: 2,
                total_references: 3,
                truncated: false,
                cancelled: false,
                error: None,
                ..Default::default()
            });
        });

        next_tick().await;

        assert_eq!(
            query_count(&container, ".search-file-group"),
            2,
            "one group per matching file"
        );
        assert_eq!(
            query_count(&container, ".search-match-row"),
            3,
            "one row per matching line"
        );

        let summary = container
            .query_selector(".search-summary")
            .unwrap()
            .expect("summary present")
            .text_content()
            .unwrap_or_default();
        assert!(
            summary.contains("3 references in 2 files"),
            "summary was: {summary:?}"
        );

        let title = container
            .query_selector(".references-title")
            .unwrap()
            .expect("title present")
            .text_content()
            .unwrap_or_default();
        assert!(title.contains("foo"), "title was: {title:?}");
    }

    /// Clicking a reference row requests a goto to that file + line (the same
    /// navigation the search panel uses), so the click drives the editor.
    #[wasm_bindgen_test]
    async fn clicking_reference_targets_focused_exact_project_occurrence() {
        ensure_styles_loaded();
        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "src/a.rs".to_owned(),
        };
        let current_key = file_key("current", path.clone());
        let other_key = file_key("other", path);
        let current_for_setup = current_key.clone();
        let other_for_setup = other_key.clone();
        let (container, state) = mount_panel(move |state| {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host".to_owned(),
                project_id: ProjectId("current".to_owned()),
            }));
            seed_loaded_file(state, &current_for_setup, "foo();");
            seed_loaded_file(state, &other_for_setup, "other foo();");
            let current_tab = state
                .open_tab_in(
                    PaneId::Primary,
                    TabContent::File {
                        key: current_for_setup.clone(),
                    },
                    "a.rs · current".to_owned(),
                    true,
                )
                .expect("current-project occurrence");
            state
                .open_tab_in(
                    PaneId::Secondary,
                    TabContent::File {
                        key: other_for_setup.clone(),
                    },
                    "a.rs · other".to_owned(),
                    true,
                )
                .expect("other-project occurrence");
            state.activate_tab(current_tab);
            state.references_state.set(ProjectReferencesUiState {
                source_tab: Some(current_tab),
                source_key: Some(current_for_setup.clone()),
                source_version: Some(ProjectFileVersion(1)),
                active_references_id: 1,
                in_flight: false,
                symbol: None,
                results: vec![file_result("src/a.rs", &[(42, "foo();", (0, 3))], false)],
                total_files: 1,
                total_references: 1,
                truncated: false,
                cancelled: false,
                error: None,
                ..Default::default()
            });
        });

        next_tick().await;

        assert!(
            state.pending_goto_line.get_untracked().is_none(),
            "no pending goto before clicking"
        );

        let row = container
            .query_selector(".search-match-row")
            .unwrap()
            .expect("reference row present")
            .dyn_into::<Element>()
            .unwrap();
        let row: HtmlElement = row.dyn_into().unwrap();
        row.click();

        next_tick().await;

        let current_tab = state
            .resolve_file_occurrence(&current_key, PaneId::Primary)
            .expect("current occurrence")
            .1;
        let other_tab = state
            .resolve_file_occurrence(&other_key, PaneId::Primary)
            .expect("other occurrence")
            .1;
        assert_eq!(
            state.pending_goto_line.get_untracked(),
            Some((current_tab, 42)),
            "reference navigation must not cross into the same-path other-project occurrence"
        );
        assert_ne!(current_tab, other_tab);
        assert_eq!(
            state.references_state.with_untracked(|references| {
                references
                    .source()
                    .map(|(tab, key, version)| (tab, key.clone(), version))
            }),
            Some((current_tab, current_key, ProjectFileVersion(1)))
        );
    }

    #[wasm_bindgen_test]
    async fn side_enter_is_row_scoped_and_targets_only_the_duplicate() {
        ensure_styles_loaded();
        workspace_width().set(Some(1.0));
        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "src/a.rs".to_owned(),
        };
        let key = file_key("current", path);
        let key_for_setup = key.clone();
        let (container, state) = mount_panel(move |state| {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host".to_owned(),
                project_id: ProjectId("current".to_owned()),
            }));
            seed_loaded_file(state, &key_for_setup, "foo();");
            let source_tab = state
                .open_tab_in(
                    PaneId::Primary,
                    TabContent::File {
                        key: key_for_setup.clone(),
                    },
                    "a.rs".to_owned(),
                    true,
                )
                .expect("primary occurrence");
            state.references_state.set(ProjectReferencesUiState {
                source_tab: Some(source_tab),
                source_key: Some(key_for_setup.clone()),
                source_version: Some(ProjectFileVersion(1)),
                active_references_id: 1,
                results: vec![file_result("src/a.rs", &[(42, "foo();", (0, 3))], false)],
                total_files: 1,
                total_references: 1,
                ..Default::default()
            });
        });
        assert_eq!(
            workspace_width().get_untracked(),
            None,
            "a references-panel mount must discard a stale prior workspace measurement"
        );
        next_tick().await;

        let init = web_sys::KeyboardEventInit::new();
        init.set_bubbles(true);
        init.set_key("Enter");
        init.set_meta_key(true);
        web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            state.center_zone.with_untracked(|center| {
                center
                    .occurrences(&TabContent::File { key: key.clone() })
                    .len()
            }),
            1,
            "a global Open to the Side chord must not open a reference result"
        );

        let row = container
            .query_selector(".search-match-row")
            .unwrap()
            .expect("reference row")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            row.get_attribute("aria-keyshortcuts").as_deref(),
            Some("Enter Control+Enter Meta+Enter")
        );
        let side_hint = crate::components::command_palette::context_binding(
            crate::components::command_palette::ContextActionId::OpenToSide,
        )
        .chord()
        .hint();
        assert_eq!(
            row.get_attribute("title").as_deref(),
            Some(format!("Open ({side_hint} opens to the side)").as_str()),
            "the row title must show the exact platform chord that opens to the side"
        );
        let side_action = container
            .query_selector(".search-result-open-side")
            .unwrap()
            .expect("visible side-open action")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            side_action.get_attribute("aria-label").as_deref(),
            Some("Open src/a.rs at line 42 to the side")
        );
        assert_eq!(
            side_action.get_attribute("aria-keyshortcuts").as_deref(),
            Some("Control+Enter Meta+Enter")
        );
        state.tabs_enabled.set(false);
        next_tick().await;
        assert_eq!(
            side_action.get_attribute("aria-disabled").as_deref(),
            Some("true")
        );
        assert_eq!(
            side_action.get_attribute("title").as_deref(),
            Some("Enable tabs to use split view.")
        );
        let disabled_row_title = row.get_attribute("title").unwrap_or_default();
        assert_eq!(
            disabled_row_title,
            format!(
                "Open; {side_hint} Open to the Side unavailable: Enable tabs to use split view."
            ),
            "the disabled row title must preserve the exact platform chord and refusal"
        );
        dispatch_side_enter(&row);
        next_tick().await;
        assert_eq!(
            state.center_zone.with_untracked(|center| {
                center
                    .occurrences(&TabContent::File { key: key.clone() })
                    .len()
            }),
            1,
            "the shared unavailable policy must refuse without a focused-pane consolation open"
        );
        assert!(state.pending_goto_line.get_untracked().is_none());
        let notice = container
            .query_selector("[data-testid=\"references-side-open-notice\"]")
            .unwrap()
            .expect("visible side-open refusal notice");
        assert_eq!(notice.get_attribute("role").as_deref(), Some("status"));
        assert_eq!(
            notice.text_content().unwrap_or_default().trim(),
            "Enable tabs to use split view."
        );

        state.tabs_enabled.set(true);
        next_tick().await;
        dispatch_side_enter(&row);
        next_tick().await;

        let occurrences = state
            .center_zone
            .with_untracked(|center| center.occurrences(&TabContent::File { key: key.clone() }));
        assert_eq!(occurrences.len(), 2);
        let secondary = occurrences
            .iter()
            .find_map(|(pane, tab)| (*pane == PaneId::Secondary).then_some(*tab))
            .expect("secondary duplicate");
        let primary = occurrences
            .iter()
            .find_map(|(pane, tab)| (*pane == PaneId::Primary).then_some(*tab))
            .expect("primary occurrence");
        assert_ne!(primary, secondary);
        assert_eq!(
            state.pending_goto_line.get_untracked(),
            Some((secondary, 42))
        );
        assert!(
            container
                .query_selector("[data-testid=\"references-side-open-notice\"]")
                .unwrap()
                .is_none(),
            "a successful retry clears the visible refusal"
        );

        assert!(state.reveal_tab(primary));
        state.pending_goto_line.set(None);
        side_action.click();
        next_tick().await;
        assert_eq!(
            state.pending_goto_line.get_untracked(),
            Some((secondary, 42)),
            "the visible affordance must target the exact side occurrence"
        );
    }

    /// A multi-target go-to-definition result reuses the references panel as a
    /// chooser instead of silently taking the first target; clicking the second
    /// row jumps to the second target's byte offset.
    #[wasm_bindgen_test]
    async fn navigate_result_with_two_targets_populates_chooser_and_clicks_second() {
        use crate::state::{CodeIntelKey, CodeIntelNavigateContext, ProjectReferencesMode};
        use protocol::{CodeIntelLocation, CodeIntelNavigateResultPayload};

        ensure_styles_loaded();
        let root = ProjectRootPath("test-root".to_owned());
        let source = ProjectPath {
            root: root.clone(),
            relative_path: "src/main.rs".to_owned(),
        };
        let first = ProjectPath {
            root: root.clone(),
            relative_path: "src/first.rs".to_owned(),
        };
        let second = ProjectPath {
            root,
            relative_path: "src/second.rs".to_owned(),
        };
        let source_key = FileResourceKey {
            host_id: "chooser-host".to_owned(),
            project_id: ProjectId("chooser-project".to_owned()),
            path: source.clone(),
        };
        let first_key = FileResourceKey {
            host_id: "chooser-host".to_owned(),
            project_id: ProjectId("chooser-project".to_owned()),
            path: first.clone(),
        };
        let second_key = FileResourceKey {
            host_id: "chooser-host".to_owned(),
            project_id: ProjectId("chooser-project".to_owned()),
            path: second.clone(),
        };
        let source_for_setup = source_key.clone();
        let first_key_for_setup = first_key.clone();
        let second_key_for_setup = second_key.clone();
        let first_for_setup = first.clone();
        let second_for_setup = second.clone();
        let (container, state) = mount_panel(move |state| {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "chooser-host".to_owned(),
                project_id: ProjectId("chooser-project".to_owned()),
            }));
            seed_loaded_file(state, &source_for_setup, "fn main() { thing(); }");
            seed_loaded_file(state, &first_key_for_setup, "pub fn first() {}\n");
            seed_loaded_file(state, &second_key_for_setup, "pub fn second() {}\n");
            let source_tab = state
                .open_tab_in(
                    PaneId::Primary,
                    TabContent::File {
                        key: source_for_setup.clone(),
                    },
                    "main.rs".to_owned(),
                    true,
                )
                .expect("definition source tab");
            state
                .code_intel_navigate_ctx
                .set(Some(CodeIntelNavigateContext {
                    navigate_id: 77,
                    tab: source_tab,
                    key: source_for_setup.clone(),
                    version: ProjectFileVersion(1),
                }));
            state.code_intel.update(|map| {
                map.entry(CodeIntelKey {
                    host_id: "chooser-host".to_owned(),
                    project_id: ProjectId("chooser-project".to_owned()),
                    path: source.clone(),
                })
                .or_default()
                .set_rendered_version(ProjectFileVersion(1));
            });
            crate::dispatch::apply_code_intel_navigate_result(
                state,
                CodeIntelNavigateResultPayload {
                    navigate_id: 77,
                    path: source.clone(),
                    version: ProjectFileVersion(1),
                    targets: vec![
                        CodeIntelLocation {
                            path: first_for_setup.clone(),
                            range: ByteRange { start: 7, end: 12 },
                        },
                        CodeIntelLocation {
                            path: second_for_setup.clone(),
                            range: ByteRange { start: 7, end: 13 },
                        },
                    ],
                },
            );
        });

        next_tick().await;

        state.references_state.with_untracked(|s| {
            assert_eq!(s.mode, ProjectReferencesMode::DefinitionTargets);
            assert_eq!(s.results.len(), 2, "both targets appear in the chooser");
            assert_eq!(s.row_targets.len(), 2, "each chooser row has a target");
            assert_eq!(
                s.source()
                    .map(|(tab, key, version)| (tab, key.clone(), version)),
                state
                    .resolve_file_occurrence(&source_key, PaneId::Primary)
                    .map(|(_, tab)| (tab, source_key.clone(), ProjectFileVersion(1)))
            );
        });
        assert_eq!(
            query_count(&container, ".search-match-row"),
            2,
            "one chooser row per definition target"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("first"), "chooser text was: {text:?}");
        assert!(text.contains("second"), "chooser text was: {text:?}");

        let rows = container.query_selector_all(".search-match-row").unwrap();
        let second_row = rows
            .item(1)
            .expect("second chooser row present")
            .dyn_into::<HtmlElement>()
            .unwrap();
        second_row.click();

        next_tick().await;

        let second_tab = state
            .resolve_file_occurrence(&second_key, PaneId::Primary)
            .expect("second target occurrence")
            .1;
        assert_eq!(
            state.pending_goto_offset.get_untracked(),
            Some((second_tab, 7))
        );
        assert!(
            state.pending_goto_line.get_untracked().is_none(),
            "definition chooser rows navigate by byte offset, not by line"
        );
    }

    /// The truncation banner appears when a file's references were capped.
    #[wasm_bindgen_test]
    async fn shows_truncation_banner_when_truncated() {
        ensure_styles_loaded();
        let (container, _state) = mount_panel(|state| {
            state.references_state.set(ProjectReferencesUiState {
                active_references_id: 1,
                in_flight: false,
                symbol: None,
                results: vec![file_result("a.rs", &[(1, "x", (0, 1))], true)],
                total_files: 1,
                total_references: 1,
                truncated: true,
                cancelled: false,
                error: None,
                ..Default::default()
            });
        });

        next_tick().await;

        let banner = container
            .query_selector(".search-truncated")
            .unwrap()
            .expect("truncation banner present")
            .text_content()
            .unwrap_or_default();
        assert!(
            banner.to_lowercase().contains("truncated"),
            "banner text was: {banner:?}"
        );
    }
}
