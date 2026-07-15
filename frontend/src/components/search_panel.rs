use std::collections::HashSet;
use std::time::Duration;

use leptos::prelude::*;

use protocol::ProjectPath;

use crate::actions::{open_project_path_at_navigation, start_project_search};
use crate::components::find_bar::render_text_with_highlights;
use crate::state::{AppState, OpenTarget, PendingFileNavigation};

/// Project-wide ("global") search panel, shown as the third tab of the left
/// dock. Mirrors the in-file `FindBar` controls (case / whole-word / regex)
/// plus an "include ignored files" toggle, debounces the query, and renders
/// streamed results grouped by file. Clicking a match opens the file and
/// scrolls to the line.
#[component]
pub fn SearchPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let input_ref = NodeRef::<leptos::html::Input>::new();

    // Focus + select the query input whenever a focus is requested (the
    // Cmd/Ctrl+Shift+F shortcut or "search in folder" bump the seq). Deferred a
    // tick so the dock has switched the panel out of `display: none` first —
    // focusing a hidden element is a no-op.
    {
        let state = state.clone();
        Effect::new(move |_| {
            let _ = state.search_focus_seq.get();
            set_timeout(
                move || {
                    if let Some(el) = input_ref.get() {
                        let _ = el.focus();
                        el.select();
                    }
                },
                Duration::from_millis(0),
            );
        });
    }

    // Debounced query input: each keystroke schedules a search ~175ms out and
    // a generation guard ensures only the latest scheduled run actually fires.
    let debounce_gen = RwSignal::new(0u32);
    let on_input = {
        let state = state.clone();
        move |ev| {
            let value = event_target_value(&ev);
            state.search_state.update(|s| s.query = value);
            let my_gen = debounce_gen.get_untracked().wrapping_add(1);
            debounce_gen.set(my_gen);
            let state = state.clone();
            set_timeout(
                move || {
                    if debounce_gen.get_untracked() == my_gen {
                        start_project_search(&state);
                    }
                },
                Duration::from_millis(175),
            );
        }
    };

    // Enter forces an immediate search (skips the debounce).
    let on_keydown = {
        let state = state.clone();
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                ev.prevent_default();
                debounce_gen.update(|g| *g = g.wrapping_add(1));
                start_project_search(&state);
            }
        }
    };

    let toggle = |state: AppState, mutate: fn(&mut crate::state::ProjectSearchUiState)| {
        move |_| {
            state.search_state.update(mutate);
            start_project_search(&state);
        }
    };

    let case_class = {
        let state = state.clone();
        move || toggle_class(state.search_state.with(|s| s.case_sensitive))
    };
    let word_class = {
        let state = state.clone();
        move || toggle_class(state.search_state.with(|s| s.whole_word))
    };
    let regex_class = {
        let state = state.clone();
        move || toggle_class(state.search_state.with(|s| s.use_regex))
    };
    let ignored_class = {
        let state = state.clone();
        move || toggle_class(state.search_state.with(|s| s.include_ignored))
    };

    let query_value = {
        let state = state.clone();
        move || state.search_state.with(|s| s.query.clone())
    };

    let summary_text = {
        let state = state.clone();
        move || {
            state.search_state.with(|s| {
                if let Some(err) = &s.error {
                    return format!("Error: {err}");
                }
                if s.query.trim().is_empty() {
                    return String::new();
                }
                if s.total_matches == 0 {
                    return if s.in_flight {
                        "Searching…".to_owned()
                    } else {
                        "No results".to_owned()
                    };
                }
                let suffix = if s.in_flight { " (searching…)" } else { "" };
                format!(
                    "{} result{} in {} file{}{suffix}",
                    s.total_matches,
                    plural(s.total_matches),
                    s.total_files,
                    plural(s.total_files),
                )
            })
        }
    };

    let show_truncated = {
        let state = state.clone();
        move || state.search_state.with(|s| s.truncated)
    };

    // Per-file collapse state (keyed by the rendered path string).
    let collapsed: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());

    let results_view = {
        let state = state.clone();
        move || {
            let files = state.search_state.with(|s| s.results.clone());
            files
                .into_iter()
                .map(|file| file_group(state.clone(), collapsed, file))
                .collect_view()
        }
    };

    view! {
        <div class="search-panel">
            <div class="search-controls">
                <input
                    type="text"
                    class="search-input find-input"
                    placeholder="Search"
                    node_ref=input_ref
                    prop:value=query_value
                    on:input=on_input
                    on:keydown=on_keydown
                    aria-label="Search project"
                />
                <div class="search-toggles">
                    <button
                        type="button"
                        class=case_class
                        title="Match case"
                        aria-label="Match case"
                        on:click=toggle(state.clone(), |s| s.case_sensitive = !s.case_sensitive)
                    >"Aa"</button>
                    <button
                        type="button"
                        class=word_class
                        title="Whole word"
                        aria-label="Whole word"
                        on:click=toggle(state.clone(), |s| s.whole_word = !s.whole_word)
                    >"W"</button>
                    <button
                        type="button"
                        class=regex_class
                        title="Use regular expression"
                        aria-label="Use regular expression"
                        on:click=toggle(state.clone(), |s| s.use_regex = !s.use_regex)
                    >".*"</button>
                    <button
                        type="button"
                        class=ignored_class
                        title="Include ignored and hidden files"
                        aria-label="Include ignored and hidden files"
                        on:click=toggle(state.clone(), |s| s.include_ignored = !s.include_ignored)
                    >"Ig"</button>
                </div>
            </div>
            <div class="search-summary">{summary_text}</div>
            <Show when=show_truncated>
                <div class="search-truncated">
                    "Results truncated — refine your search."
                </div>
            </Show>
            <div class="search-results">
                {results_view}
            </div>
        </div>
    }
}

fn toggle_class(active: bool) -> &'static str {
    if active {
        "find-toggle-btn active"
    } else {
        "find-toggle-btn"
    }
}

fn plural(count: u32) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn path_key(path: &ProjectPath) -> String {
    format!("{}\u{0}{}", path.root.0, path.relative_path)
}

/// Render one file's group: a collapsible header (path + match-count badge)
/// followed by one row per matching line.
fn file_group(
    state: AppState,
    collapsed: RwSignal<HashSet<String>>,
    file: protocol::ProjectSearchFileResult,
) -> impl IntoView {
    let key = path_key(&file.path);
    let display_name = file.path.relative_path.clone();
    let match_count = file.matches.len();
    let truncated = file.truncated;

    let key_for_chevron = key.clone();
    let key_for_matches = key.clone();
    let chevron_collapsed = move || collapsed.with(|set| set.contains(&key_for_chevron));
    let matches_style = move || {
        if collapsed.with(|set| set.contains(&key_for_matches)) {
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
        .matches
        .into_iter()
        .map(|m| {
            let ranges: Vec<(usize, usize)> = m
                .ranges
                .iter()
                .map(|(start, end)| (*start as usize, *end as usize))
                .collect();
            let highlighted = render_text_with_highlights(&m.line_text, &ranges);
            let line_number = m.line_number;
            let on_click = {
                let state = state.clone();
                let path = file_path.clone();
                move |_| {
                    let _ = open_project_path_at_navigation(
                        &state,
                        path.clone(),
                        OpenTarget::Focused,
                        PendingFileNavigation::Line(line_number),
                    );
                }
            };
            view! {
                <div class="fe-row">
                    <button
                        class="search-match-row fe-item"
                        title="Open"
                        aria-keyshortcuts="Enter"
                        on:click=on_click
                    >
                        <span class="search-match-line">{line_number.to_string()}</span>
                        <span class="search-match-text">{highlighted}</span>
                    </button>
                </div>
            }
        })
        .collect_view();

    let badge = if truncated {
        format!("{match_count}+")
    } else {
        match_count.to_string()
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
            <div class="search-file-matches" style=matches_style>{rows}</div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::center_zone::workspace_width;
    use crate::state::{ActiveProjectRef, AppState, FileResourceKey, OpenFile, PaneId, TabContent};
    use leptos::mount::mount_to;
    use protocol::{
        ProjectFileVersion, ProjectId, ProjectPath, ProjectRootPath, ProjectSearchFileResult,
        ProjectSearchMatch,
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
            .get_element_by_id("test-prod-styles-search")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-search");
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
    ) -> ProjectSearchFileResult {
        ProjectSearchFileResult {
            path: ProjectPath {
                root: ProjectRootPath("test-root".to_owned()),
                relative_path: relative.to_owned(),
            },
            matches: lines
                .iter()
                .map(|(line_number, text, range)| ProjectSearchMatch {
                    line_number: *line_number,
                    line_text: (*text).to_owned(),
                    ranges: vec![*range],
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

    fn seed_loaded_file(state: &AppState, key: &FileResourceKey) {
        state.open_files.update(|files| {
            files.insert(
                key.clone(),
                OpenFile {
                    path: key.path.clone(),
                    version: ProjectFileVersion(1),
                    contents: Some("the needle".to_owned()),
                    is_binary: false,
                },
            );
        });
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
            view! { <SearchPanel /> }
        });
        let state = captured.borrow().clone().unwrap();
        // Leak the handle so the reactive owner (and its signals) stay alive
        // for the duration of the test's assertions.
        std::mem::forget(_handle);
        (container, state)
    }

    #[wasm_bindgen_test]
    async fn renders_streamed_results_grouped_by_file() {
        ensure_styles_loaded();
        let (container, _state) = mount_panel(|state| {
            state.search_state.update(|s| {
                s.query = "needle".to_owned();
                s.results = vec![
                    file_result(
                        "src/a.rs",
                        &[(3, "let needle = 1;", (4, 10)), (8, "needle again", (0, 6))],
                        false,
                    ),
                    file_result("src/b.rs", &[(12, "a needle here", (2, 8))], false),
                ];
                s.total_files = 2;
                s.total_matches = 3;
            });
        });

        next_tick().await;

        // Two file groups, three match rows total.
        assert_eq!(
            query_count(&container, ".search-file-group"),
            2,
            "expected one group per matching file"
        );
        assert_eq!(
            query_count(&container, ".search-match-row"),
            3,
            "expected one row per matching line"
        );

        // Summary reflects the totals.
        let summary = container
            .query_selector(".search-summary")
            .unwrap()
            .expect("summary present")
            .text_content()
            .unwrap_or_default();
        assert!(
            summary.contains("3 results in 2 files"),
            "summary was: {summary:?}"
        );

        // No truncation banner when nothing was truncated.
        assert!(
            container
                .query_selector(".search-truncated")
                .unwrap()
                .is_none()
                || container
                    .query_selector(".search-truncated")
                    .unwrap()
                    .and_then(|el| el.dyn_into::<HtmlElement>().ok())
                    .map(|el| el.offset_parent().is_none())
                    .unwrap_or(true),
            "truncation banner should be absent/hidden"
        );
    }

    #[wasm_bindgen_test]
    async fn shows_truncation_banner_when_truncated() {
        ensure_styles_loaded();
        let (container, _state) = mount_panel(|state| {
            state.search_state.update(|s| {
                s.query = "x".to_owned();
                s.results = vec![file_result("a.rs", &[(1, "x", (0, 1))], false)];
                s.total_files = 1;
                s.total_matches = 1;
                s.truncated = true;
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

    #[wasm_bindgen_test]
    async fn per_file_match_badge_shows_count() {
        ensure_styles_loaded();
        let (container, _state) = mount_panel(|state| {
            state.search_state.update(|s| {
                s.query = "needle".to_owned();
                s.results = vec![file_result(
                    "src/a.rs",
                    &[(3, "needle one", (0, 6)), (8, "needle two", (0, 6))],
                    false,
                )];
                s.total_files = 1;
                s.total_matches = 2;
            });
        });

        next_tick().await;

        let badge = container
            .query_selector(".search-file-badge")
            .unwrap()
            .expect("badge present")
            .text_content()
            .unwrap_or_default();
        assert_eq!(badge.trim(), "2", "badge should show the match count");
    }

    #[wasm_bindgen_test]
    async fn toggle_button_changes_appearance_on_click() {
        ensure_styles_loaded();
        let (container, _state) = mount_panel(|_state| {});

        next_tick().await;

        let window = web_sys::window().unwrap();
        let buttons = container
            .query_selector_all(".search-toggles button")
            .unwrap();
        assert_eq!(buttons.length(), 4, "expected four toggle buttons");
        let case_btn = buttons.item(0).unwrap().dyn_into::<HtmlElement>().unwrap();

        let bg_before = window
            .get_computed_style(&case_btn)
            .unwrap()
            .unwrap()
            .get_property_value("background-color")
            .unwrap();

        case_btn.click();
        next_tick().await;

        let bg_after = window
            .get_computed_style(&case_btn)
            .unwrap()
            .unwrap()
            .get_property_value("background-color")
            .unwrap();

        assert_ne!(
            bg_before, bg_after,
            "toggling 'Match case' should visibly change the button background"
        );
    }

    #[wasm_bindgen_test]
    async fn clicking_match_row_targets_focused_exact_project_occurrence() {
        ensure_styles_loaded();
        let path = ProjectPath {
            root: ProjectRootPath("test-root".to_owned()),
            relative_path: "src/a.rs".to_owned(),
        };
        let current_key = file_key("current", path.clone());
        let other_key = file_key("other", path.clone());
        let current_for_setup = current_key.clone();
        let other_for_setup = other_key.clone();
        let (container, state) = mount_panel(move |state| {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host".to_owned(),
                project_id: ProjectId("current".to_owned()),
            }));
            seed_loaded_file(state, &current_for_setup);
            seed_loaded_file(state, &other_for_setup);
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
            state.search_state.update(|s| {
                s.query = "needle".to_owned();
                s.results = vec![file_result(
                    "src/a.rs",
                    &[(42, "the needle", (4, 10))],
                    false,
                )];
                s.total_files = 1;
                s.total_matches = 1;
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
            .expect("match row present")
            .dyn_into::<Element>()
            .unwrap();
        let row: HtmlElement = row.dyn_into().unwrap();
        row.click();

        next_tick().await;

        let current_tab = state
            .resolve_file_occurrence(&current_key, PaneId::Primary)
            .expect("current project occurrence")
            .1;
        let other_tab = state
            .resolve_file_occurrence(&other_key, PaneId::Primary)
            .expect("other project occurrence")
            .1;
        assert_eq!(
            state.pending_goto_line.get_untracked(),
            Some((current_tab, 42)),
            "ordinary search navigation must target the focused exact-project occurrence"
        );
        assert_ne!(current_tab, other_tab);
    }

    /// Splits are created by dragging tabs: a search result row is a plain
    /// open-on-click control with no side-open action or chord.
    #[wasm_bindgen_test]
    async fn search_rows_have_no_side_open_control() {
        let (container, _state) = mount_panel(move |state| {
            state.active_project.set(Some(ActiveProjectRef {
                host_id: "host".to_owned(),
                project_id: ProjectId("current".to_owned()),
            }));
            state.search_state.update(|search| {
                search.query = "needle".to_owned();
                search.results = vec![file_result(
                    "src/a.rs",
                    &[(42, "the needle", (4, 10))],
                    false,
                )];
                search.total_files = 1;
                search.total_matches = 1;
            });
        });
        next_tick().await;

        assert!(
            container
                .query_selector(".search-result-open-side")
                .unwrap()
                .is_none(),
            "no side-open action renders on a search result row"
        );
        let row = container
            .query_selector(".search-match-row")
            .unwrap()
            .expect("match row")
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert_eq!(
            row.get_attribute("aria-keyshortcuts").as_deref(),
            Some("Enter"),
            "the row advertises only its ordinary Enter activation"
        );
    }
}
