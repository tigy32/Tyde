use leptos::html::Div;
use leptos::prelude::*;

use super::agents_view::{persist_bool_pref, restore_bool_pref};
use crate::actions::{begin_new_chat, clear_active_project, open_new_chat, select_project};
use crate::state::{ActiveProjectRef, AppState, ProjectInfo, sort_project_infos};
use protocol::ProjectId;

/// The project sheet that stands between "New chat" and the composer.
///
/// Mobile carries no project rail, so an agent spawned here used to go out with
/// `project_id: None` and no workspace roots no matter what the user wanted —
/// there was no surface left that set `active_project`. This asks the question
/// once, at the only moment it decides anything.
///
/// **"No project" is a row, not a fallback.** A phone is where quick questions
/// get asked, and forcing every one of them into a workspace would be the wrong
/// default; the row is listed first.
///
/// Workbenches stay hidden unless the remembered "Show workbenches" switch is
/// on: a new chat almost always belongs in the parent project.
///
/// Selection only, deliberately: this lists projects to scope a spawn and never
/// opens one for browsing. Files, Git, diffs, and reviews stay off the mobile
/// surface.
#[component]
pub fn ProjectPicker() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let open = state.project_picker_open;
    crate::native::track_modal(open);

    let query = RwSignal::new(String::new());
    let show_workbenches = RwSignal::new(restore_bool_pref(STORAGE_SHOW_WORKBENCHES, false));
    Effect::new(move |_| {
        if open.get() {
            query.set(String::new());
        }
    });

    let projects_state = state.clone();
    let projects = Memo::new(move |_| {
        let Some(host) = projects_state.active_local_host_id.get() else {
            return Vec::<ProjectInfo>::new();
        };
        let mut projects: Vec<ProjectInfo> = projects_state
            .projects
            .get()
            .into_iter()
            .filter(|info| info.local_host_id == host)
            .collect();
        sort_project_infos(&mut projects);
        let query = query.get().trim().to_lowercase();
        let show_workbenches = show_workbenches.get();
        // The current scope stays listed so its selection is never hidden.
        let selected = projects_state.active_project.get();
        let matches = |info: &ProjectInfo| info.project.name.to_lowercase().contains(&query);
        let parent_matches = |parent: &ProjectId| {
            projects
                .iter()
                .any(|info| &info.project.id == parent && matches(info))
        };
        let visible_children: Vec<&ProjectInfo> = projects
            .iter()
            .filter(|info| {
                info.project.parent_project_id().is_some_and(|parent| {
                    let is_selected = selected.as_ref().is_some_and(|selected| {
                        selected.local_host_id == info.local_host_id
                            && selected.project_id == info.project.id
                    });
                    (show_workbenches || is_selected) && (matches(info) || parent_matches(parent))
                })
            })
            .collect();
        projects
            .iter()
            .filter(|info| match info.project.parent_project_id() {
                Some(_) => visible_children
                    .iter()
                    .any(|child| child.project.id == info.project.id),
                None => {
                    matches(info)
                        || visible_children.iter().any(|child| {
                            child.project.parent_project_id() == Some(&info.project.id)
                        })
                }
            })
            .cloned()
            .collect()
    });

    let toggle_workbenches = move |event: web_sys::Event| {
        let shown = event_target_checked(&event);
        persist_bool_pref(STORAGE_SHOW_WORKBENCHES, shown);
        show_workbenches.set(shown);
    };

    let selected_state = state.clone();
    let selected = move || selected_state.active_project.get();

    let cancel_state = state.clone();
    let on_cancel = Callback::new(move |_: ()| cancel_state.project_picker_open.set(false));

    let none_state = state.clone();
    let on_none = Callback::new(move |_: ()| {
        clear_active_project(&none_state);
        open_new_chat(&none_state);
    });

    // Escape-to-cancel on the focused backdrop, matching `ConfirmModal`: the
    // listener is scoped to the sheet and torn down with it, so nothing leaks
    // onto the document.
    let backdrop_ref: NodeRef<Div> = NodeRef::new();
    Effect::new(move |_| {
        if open.get()
            && let Some(element) = backdrop_ref.get()
        {
            let _ = element.focus();
        }
    });

    view! {
        <Show when=move || open.get()>
            {
                let state = state.clone();
                view! {
                    <div
                        node_ref=backdrop_ref
                        class="project-sheet-backdrop"
                        role="dialog"
                        aria-modal="true"
                        aria-label="Choose a project for this chat"
                        tabindex="-1"
                        data-mobile-test="project-picker"
                        on:click=move |_| on_cancel.run(())
                        on:keydown=move |event: web_sys::KeyboardEvent| {
                            if event.key() == "Escape" {
                                on_cancel.run(());
                            }
                        }
                    >
                        <div
                            class="project-sheet"
                            on:click=|event: web_sys::MouseEvent| event.stop_propagation()
                        >
                            <h2 class="project-sheet-title">"Start a chat in"</h2>
                            <div class="search-bar project-sheet-search">
                                <input type="search" placeholder="Find a project"
                                    aria-label="Find a project"
                                    autocapitalize="none" autocomplete="off" spellcheck="false"
                                    data-mobile-test="project-picker-search"
                                    prop:value=move || query.get()
                                    on:input=move |event| query.set(event_target_value(&event)) />
                            </div>
                            <label class="project-sheet-toggle">
                                <span>"Show workbenches"</span>
                                <input
                                    class="settings-toggle"
                                    type="checkbox"
                                    role="switch"
                                    data-mobile-test="project-picker-show-workbenches"
                                    prop:checked=move || show_workbenches.get()
                                    on:change=toggle_workbenches
                                />
                            </label>
                            <div class="project-sheet-list" role="listbox">
                                <button
                                    type="button"
                                    class="project-sheet-row"
                                    class:selected=move || selected().is_none()
                                    role="option"
                                    aria-selected=move || selected().is_none().to_string()
                                    data-mobile-test="project-picker-none"
                                    on:click=move |_| on_none.run(())
                                >
                                    <span class="project-sheet-row-name">"No project"</span>
                                </button>
                                {move || {
                                    projects
                                        .get()
                                        .into_iter()
                                        .map(|info| project_row(&state, info))
                                        .collect::<Vec<_>>()
                                }}
                            </div>
                        </div>
                    </div>
                }
            }
        </Show>
    }
}

fn project_row(state: &AppState, info: ProjectInfo) -> AnyView {
    let reference = ActiveProjectRef {
        local_host_id: info.local_host_id.clone(),
        project_id: info.project.id.clone(),
    };
    let is_workbench = info.project.is_workbench();
    let name = info.project.name.clone();

    let selected_reference = reference.clone();
    let class_state = state.clone();
    let class_reference = selected_reference.clone();
    let is_selected = move || class_state.active_project.get() == Some(class_reference.clone());
    let aria_state = state.clone();
    let aria_selected =
        move || (aria_state.active_project.get() == Some(selected_reference.clone())).to_string();

    let click_state = state.clone();
    let on_click = move |_| {
        select_project(&click_state, reference.clone());
        open_new_chat(&click_state);
    };

    view! {
        <button
            type="button"
            class="project-sheet-row"
            class:workbench=is_workbench
            class:selected=is_selected
            role="option"
            aria-selected=aria_selected
            data-mobile-test="project-picker-project"
            on:click=on_click
        >
            <span class="project-sheet-row-name">{name}</span>
        </button>
    }
    .into_any()
}

const STORAGE_SHOW_WORKBENCHES: &str = "tyde-mobile-new-chat-show-workbenches";

/// The "New chat" affordance every surface shares: it opens the project sheet
/// rather than the composer, so no caller can spawn an unscoped agent by
/// forgetting a step.
pub fn new_chat_callback(state: &AppState) -> Callback<()> {
    let state = state.clone();
    Callback::new(move |_: ()| begin_new_chat(&state))
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppState;
    use leptos::mount::mount_to;
    use protocol::{Project, ProjectId, ProjectRootPath, ProjectSource};
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

    fn state_with_projects() -> AppState {
        let state = AppState::new();
        let host = crate::state::LocalHostId("picker-host".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        state.projects.set(vec![
            ProjectInfo {
                local_host_id: host.clone(),
                project: Project {
                    id: ProjectId("tyde".to_owned()),
                    name: "Tyde".to_owned(),
                    sort_order: 0,
                    source: ProjectSource::Standalone {
                        roots: vec![ProjectRootPath("/Users/mike/Tyggs/Tyde".to_owned())],
                    },
                },
            },
            ProjectInfo {
                local_host_id: host.clone(),
                project: Project {
                    id: ProjectId("tyde-wb".to_owned()),
                    name: "Picker polish".to_owned(),
                    sort_order: 0,
                    source: ProjectSource::GitWorkbench {
                        environment: Default::default(),
                        parent_project_id: ProjectId("tyde".to_owned()),
                        branch: protocol::GitBranchName("picker-polish".to_owned()),
                        roots: Vec::new(),
                    },
                },
            },
            ProjectInfo {
                local_host_id: host,
                project: Project {
                    id: ProjectId("tychat".to_owned()),
                    name: "Tychat".to_owned(),
                    sort_order: 1,
                    source: ProjectSource::Standalone {
                        roots: vec![ProjectRootPath("/Users/mike/Tyggs/Tychat".to_owned())],
                    },
                },
            },
        ]);
        state
    }

    /// **A new chat cannot reach the composer without answering the project
    /// question, and "No project" is one of the answers.**
    ///
    /// The regression this guards is bug-for-bug what shipped: every mobile
    /// new-chat route set `viewing_chat` directly, `active_project` had no
    /// writer left, and every agent spawned from a phone went out unscoped with
    /// no way to say otherwise.
    #[wasm_bindgen_test]
    async fn the_sheet_scopes_a_new_chat_and_offers_no_project() {
        persist_bool_pref(STORAGE_SHOW_WORKBENCHES, false);
        let container = make_container();
        let state = state_with_projects();
        let state_for_mount = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <ProjectPicker /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='project-picker']")
                .unwrap()
                .is_none(),
            "the sheet stays closed until a new chat asks for it"
        );

        crate::actions::begin_new_chat(&state);
        next_tick().await;
        assert!(
            !state.viewing_chat.get(),
            "opening the sheet must not open the composer behind it"
        );

        let row_names = || {
            let rows = container
                .query_selector_all("[data-mobile-test='project-picker-project']")
                .unwrap();
            (0..rows.length())
                .map(|index| rows.item(index).unwrap().text_content().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            row_names(),
            ["Tyde", "Tychat"],
            "workbenches stay hidden until the switch is turned on"
        );

        let switch: web_sys::HtmlInputElement = container
            .query_selector("[data-mobile-test='project-picker-show-workbenches']")
            .unwrap()
            .expect("the sheet offers a Show workbenches switch")
            .dyn_into()
            .unwrap();
        assert!(!switch.checked(), "Show workbenches is off by default");
        switch.click();
        next_tick().await;
        assert_eq!(
            row_names(),
            ["Tyde", "Picker polish", "Tychat"],
            "turning the switch on nests workbenches under their parent"
        );
        assert!(
            restore_bool_pref(STORAGE_SHOW_WORKBENCHES, false),
            "the switch is remembered for the next new chat"
        );

        let search: web_sys::HtmlInputElement = container
            .query_selector("[data-mobile-test='project-picker-search']")
            .unwrap()
            .expect("the sheet offers project search")
            .dyn_into()
            .unwrap();
        search.set_value("polish");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(
            row_names(),
            ["Tyde", "Picker polish"],
            "a matching workbench keeps its parent for context"
        );
        search.set_value("chat");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(row_names(), ["Tychat"], "search narrows the projects");
        search.set_value("");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(row_names(), ["Tyde", "Picker polish", "Tychat"]);
        let polish: HtmlElement = container
            .query_selector_all("[data-mobile-test='project-picker-project']")
            .unwrap()
            .item(1)
            .unwrap()
            .dyn_into()
            .unwrap();
        polish.click();
        next_tick().await;
        crate::actions::begin_new_chat(&state);
        next_tick().await;
        let switch: web_sys::HtmlInputElement = container
            .query_selector("[data-mobile-test='project-picker-show-workbenches']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        switch.click();
        next_tick().await;
        assert_eq!(
            row_names(),
            ["Tyde", "Picker polish", "Tychat"],
            "the chosen workbench stays listed while workbenches are hidden"
        );
        clear_active_project(&state);
        next_tick().await;
        assert_eq!(row_names(), ["Tyde", "Tychat"]);

        let rows = container
            .query_selector_all("[data-mobile-test='project-picker-project']")
            .unwrap();

        // Pick a project: it becomes the scope the spawn will read, and the
        // composer opens.
        let tychat: HtmlElement = rows.item(1).unwrap().dyn_into().unwrap();
        assert!(
            tychat.text_content().unwrap_or_default().contains("Tychat"),
            "the second row is the second project in the list"
        );
        tychat.click();
        next_tick().await;
        assert_eq!(
            state.active_project.get().map(|p| p.project_id.0),
            Some("tychat".to_owned()),
            "the chosen project must become the active scope"
        );
        assert!(
            state.viewing_chat.get(),
            "choosing a project opens the composer"
        );
        assert!(
            !state.project_picker_open.get(),
            "the sheet closes behind the choice"
        );

        // "No project" is a real choice, and it clears a scope set earlier.
        crate::actions::begin_new_chat(&state);
        next_tick().await;
        let none: HtmlElement = container
            .query_selector("[data-mobile-test='project-picker-none']")
            .unwrap()
            .expect("'No project' must always be offered")
            .dyn_into()
            .unwrap();
        none.click();
        next_tick().await;
        assert!(
            state.active_project.get().is_none(),
            "'No project' must clear a previously chosen scope"
        );
        assert!(
            state.viewing_chat.get(),
            "'No project' reaches the composer like any other choice"
        );

        drop(mount);
        container.remove();
        persist_bool_pref(STORAGE_SHOW_WORKBENCHES, false);
    }
}
