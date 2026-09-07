use leptos::html::Div;
use leptos::prelude::*;

use crate::actions::{begin_new_chat, clear_active_project, open_new_chat, select_project};
use crate::state::{ActiveProjectRef, AppState, ProjectInfo, sort_project_infos};

/// The project sheet that stands between "New chat" and the composer.
///
/// Mobile carries no project rail, so an agent spawned here used to go out with
/// `project_id: None` and no workspace roots no matter what the user wanted —
/// there was no surface left that set `active_project`. This asks the question
/// once, at the only moment it decides anything.
///
/// **"No project" is a row, not a fallback.** A phone is where quick questions
/// get asked, and forcing every one of them into a workspace would be the wrong
/// default; the row is listed first and says what it does.
///
/// Selection only, deliberately: this lists projects to scope a spawn and never
/// opens one for browsing. Files, Git, diffs, and reviews stay off the mobile
/// surface.
#[component]
pub fn ProjectPicker() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let open = state.project_picker_open;

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
        projects
    });

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
                                    <span class="project-sheet-row-hint">
                                        "Ask anything without a workspace"
                                    </span>
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
    // A workbench's roots are its worktree paths, which is the one thing that
    // distinguishes two same-named branches of the same repository.
    let hint = info
        .project
        .root_paths()
        .first()
        .map(|root| root.0.clone())
        .unwrap_or_default();

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
            <span class="project-sheet-row-hint">{hint}</span>
        </button>
    }
    .into_any()
}

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

        let rows = container
            .query_selector_all("[data-mobile-test='project-picker-project']")
            .unwrap();
        assert_eq!(rows.length(), 2, "both of the host's projects are listed");

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
    }
}
