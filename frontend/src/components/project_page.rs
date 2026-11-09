//! A project's page: its agents by lane, and everything else about the project.
//!
//! Every value is a projection of existing server-owned state. Cards and rows
//! are keyed by id and look their record up reactively.

use std::time::Duration;

use leptos::prelude::*;
use leptos::tachys::dom::event_target_value;
use protocol::{AgentId, AgentOrigin, ProjectId, ProjectRootPath, ProjectSource};
use wasm_bindgen_futures::spawn_local;

use crate::components::agents_panel::{
    DerivedAgentState, derive_agent_state_with_background, open_agent_chat, status_label,
};
use crate::components::card_menu::{CardContextMenu, CardMenuPosition, open_card_menu_below};
use crate::components::file_explorer::confirm_and_remove_root;
use crate::components::header::report_user_error;
use crate::components::host_browser::open_add_root_browser_for;
use crate::components::project_rail::{
    WorkbenchCreateModal, WorkbenchCreatePrompt, failed_workbench_removal, request_delete_project,
    request_remove_workbench,
};
use crate::components::review_view::{open_comments_for_project, pick_workspace_draft};
use crate::components::settings_panel::steering_in_project;
use crate::components::swarms_panel::project_swarm_count;
use crate::components::workflows_panel::effective_summaries;
use crate::sidebar_layout::SidebarTab;
use crate::state::{
    ActiveProjectRef, AgentInfo, AppState, ProjectInfo, TabContent, agent_ui_project_id,
    root_display_name,
};

const THINK: &str = r#"<circle cx="8" cy="8" r="5.5"/><path d="M8 5v3.2l2 1.3"/>"#;
const AWAIT: &str = r#"<circle cx="8" cy="8" r="5.5"/><path d="M6.4 6.5a1.6 1.6 0 1 1 2.4 1.4c-.5.3-.8.6-.8 1.1"/><path d="M8 11h.01"/>"#;
const IDLE: &str = r#"<path d="m3.5 8.5 3 3 6-6.5"/>"#;
const BACKGROUND: &str =
    r#"<path d="M4.5 2.5h7M4.5 13.5h7M5 2.5v2.2L8 8l3-3.3V2.5M5 13.5v-2.2L8 8l3 3.3v2.2"/>"#;
const COMPACT: &str = r#"<path d="M3 5.5V2.8M3 5.5h2.7"/><path d="M3.3 5.3A5.5 5.5 0 1 1 2.5 8"/>"#;
const FAIL: &str = r#"<circle cx="8" cy="8" r="5.5"/><path d="m6 6 4 4M10 6l-4 4"/>"#;
const PLUS: &str = r#"<path d="M8 3.5v9M3.5 8h9"/>"#;
const MORE: &str = r#"<path d="M3.5 8h.01M8 8h.01M12.5 8h.01" stroke-width="2"/>"#;
const CLOSE: &str = r#"<path d="m4.5 4.5 7 7M11.5 4.5l-7 7"/>"#;
const OPEN: &str = r#"<path d="M6 3.5H3.5v9h9V10M9 3.5h3.5V7M12.5 3.5 7.5 8.5"/>"#;

fn glyph(paths: &'static str) -> impl IntoView {
    view! { <svg class="pp-ic" viewBox="0 0 16 16" aria-hidden="true" inner_html=paths></svg> }
}

/// Open `project`'s page. A first visit's untouched starter chat gives way to
/// the page instead of sitting beside it.
pub(crate) fn open_project_page(state: &AppState, project: ActiveProjectRef) {
    let first_visit = state.active_project.get_untracked().as_ref() != Some(&project)
        && !state
            .project_view_memory
            .with_untracked(|memory| memory.contains_key(&project));
    let Some(label) = state.projects.with_untracked(|projects| {
        projects
            .iter()
            .find(|info| info.host_id == project.host_id && info.project.id == project.project_id)
            .map(|info| info.project.name.clone())
    }) else {
        log::error!(
            "open_project_page: project {} is not in the catalog",
            project.project_id.0
        );
        return;
    };
    state.switch_active_project(Some(project.clone()));
    let starter = state.center_zone.with_untracked(|center_zone| {
        first_visit
            .then(|| center_zone.active_tab())
            .flatten()
            .filter(|tab| tab.content == TabContent::empty_chat())
            .map(|tab| tab.id)
    });
    state.open_tab(
        TabContent::Project {
            host_id: project.host_id,
            project_id: project.project_id,
        },
        label,
        true,
    );
    if let Some(starter) = starter {
        state.close_tab(starter);
    }
}

/// The clearable mark of a project filter on a surface the page opened.
#[component]
pub(crate) fn ProjectFilterChip(name: String, on_clear: Callback<()>) -> impl IntoView {
    view! {
        <button class="pp-filter-chip" aria-label="Clear project filter" on:click=move |_| on_clear.run(())>
            <span class="pp-filter-chip-name">{name}</span>
            {glyph(CLOSE)}
        </button>
    }
}

/// One column of the board. Lanes come from `board_lanes` alone, so a
/// server-owned lane list replaces that function and nothing below it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BoardLane {
    name: String,
    cards: Vec<AgentId>,
}

fn is_board_card(projects: &[ProjectInfo], project: &ActiveProjectRef, agent: &AgentInfo) -> bool {
    agent.host_id == project.host_id
        && agent.parent_agent_id.is_none()
        && agent.origin != AgentOrigin::SwarmMember
        && agent_ui_project_id(projects, &agent.host_id, agent.project_id.as_ref())
            == Some(&project.project_id)
}

fn board_lanes(
    projects: &[ProjectInfo],
    agents: &[AgentInfo],
    project: &ActiveProjectRef,
) -> Vec<BoardLane> {
    let mut members: Vec<&AgentInfo> = agents
        .iter()
        .filter(|agent| is_board_card(projects, project, agent))
        .collect();
    members.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| a.agent_id.0.cmp(&b.agent_id.0))
    });
    vec![BoardLane {
        name: "Active".to_owned(),
        cards: members.iter().map(|agent| agent.agent_id.clone()).collect(),
    }]
}

fn state_glyph(state: DerivedAgentState) -> &'static str {
    match state {
        DerivedAgentState::Initializing
        | DerivedAgentState::Thinking
        | DerivedAgentState::Cancelling => THINK,
        DerivedAgentState::AwaitingUser => AWAIT,
        DerivedAgentState::CompactionQueued | DerivedAgentState::Compacting => COMPACT,
        DerivedAgentState::Idle => IDLE,
        DerivedAgentState::BackgroundWork => BACKGROUND,
        DerivedAgentState::Cancelled => CLOSE,
        DerivedAgentState::Terminated => FAIL,
    }
}

fn state_class(state: DerivedAgentState) -> &'static str {
    match state {
        DerivedAgentState::Thinking
        | DerivedAgentState::CompactionQueued
        | DerivedAgentState::Compacting => "pp-state pp-state-active",
        DerivedAgentState::AwaitingUser => "pp-state pp-state-await",
        DerivedAgentState::Terminated => "pp-state pp-state-fail",
        DerivedAgentState::Initializing
        | DerivedAgentState::Cancelling
        | DerivedAgentState::Idle
        | DerivedAgentState::BackgroundWork
        | DerivedAgentState::Cancelled => "pp-state",
    }
}

fn compact_age(created_at_ms: u64, now_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(created_at_ms) / 60_000;
    match minutes {
        0 => "now".to_owned(),
        1..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

fn workbench_branch(
    projects: &[ProjectInfo],
    host_id: &str,
    project_id: &ProjectId,
) -> Option<String> {
    projects.iter().find_map(|info| {
        if info.host_id != host_id || &info.project.id != project_id {
            return None;
        }
        match &info.project.source {
            ProjectSource::GitWorkbench { branch, .. } => Some(branch.0.clone()),
            ProjectSource::Standalone { .. } => None,
        }
    })
}

#[component]
fn AgentCard(host_id: String, agent_id: AgentId, now_ms: ReadSignal<u64>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let agent = {
        let agents = state.agents;
        Memo::new(move |_| {
            agents.with(|agents| {
                agents
                    .iter()
                    .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
                    .cloned()
            })
        })
    };
    let derived = {
        let state = state.clone();
        Memo::new(move |_| {
            agent
                .get()
                .map(|agent| derive_agent_state_with_background(&state, &agent))
        })
    };
    let branch = {
        let projects = state.projects;
        Memo::new(move |_| {
            let agent = agent.get()?;
            let project_id = agent.project_id?;
            projects.with(|projects| workbench_branch(projects, &agent.host_id, &project_id))
        })
    };
    let on_click = move |_| {
        if let Some(agent) = agent.get_untracked() {
            open_agent_chat(&state, &agent);
        }
    };
    view! {
        <button class="pp-card" on:click=on_click>
            <span class="pp-card-title" title=move || agent.get().map(|agent| agent.name)>
                {move || agent.get().map(|agent| agent.name)}
            </span>
            <span class="pp-card-meta">
                {move || derived.get().map(|derived| view! {
                    <span class=state_class(derived)>
                        {glyph(state_glyph(derived))}
                        {status_label(&derived)}
                    </span>
                })}
                <span>
                    {move || agent.get().map(|agent| compact_age(agent.created_at_ms, now_ms.get()))}
                </span>
            </span>
            {move || branch.get().map(|branch| view! {
                <span class="pp-branch" title=branch.clone()>{branch.clone()}</span>
            })}
        </button>
    }
}

#[component]
fn LaneColumn(
    name: String,
    host_id: String,
    cards: Signal<Vec<AgentId>>,
    now_ms: ReadSignal<u64>,
) -> impl IntoView {
    let lane_label = name.clone();
    view! {
        <section class="pp-lane" aria-label=lane_label>
            <div class="pp-lane-head">
                <span class="pp-lane-name">{name}</span>
                <span class="pp-count">{move || cards.with(Vec::len)}</span>
            </div>
            <For
                each=move || cards.get()
                key=|agent_id| agent_id.clone()
                let:agent_id
            >
                <AgentCard host_id=host_id.clone() agent_id=agent_id now_ms=now_ms />
            </For>
        </section>
    }
}

#[component]
fn KvRow(
    #[prop(into)] label: String,
    #[prop(into)] value: Signal<String>,
    on_open: Callback<()>,
) -> impl IntoView {
    view! {
        <button class="pp-kv" on:click=move |_| on_open.run(())>
            <span>{label}</span>
            <span class="pp-kv-value">{move || value.get()}</span>
        </button>
    }
}

fn root_meta(git: &protocol::ProjectRootGitStatus) -> Option<(String, Option<String>)> {
    if git.branch.is_none() && git.head_oid.is_none() {
        return None;
    }
    let branch = git
        .branch
        .as_ref()
        .map_or_else(String::new, |branch| format!("{branch} · "));
    Some(if git.clean {
        (format!("{branch}clean"), None)
    } else {
        (branch, Some(format!("{} changed", git.files.len())))
    })
}

#[component]
fn RootRow(
    project: ActiveProjectRef,
    root: ProjectRootPath,
    can_remove: Signal<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let meta = {
        let git_status = state.git_status;
        let project_id = project.project_id.clone();
        let root = root.clone();
        Memo::new(move |_| {
            git_status.with(|map| {
                map.get(&project_id)?
                    .iter()
                    .find(|status| status.root == root)
                    .and_then(root_meta)
            })
        })
    };
    let name = root_display_name(&root);
    let on_remove = {
        let root = root.clone();
        move |_| {
            confirm_and_remove_root(
                state.clone(),
                project.host_id.clone(),
                project.project_id.clone(),
                root.clone(),
            );
        }
    };
    view! {
        <div class="pp-row" class:pp-row-swap=move || can_remove.get()>
            <span class="pp-row-main" title=root.0.clone()>{name}</span>
            {move || meta.get().map(|(text, dirty)| view! {
                <span class="pp-row-meta">
                    {text}
                    {dirty.map(|dirty| view! { <span class="pp-dirty">{dirty}</span> })}
                </span>
            })}
            <Show when=move || can_remove.get()>
                <button class="pp-row-act" aria-label="Remove root" title="Remove root" on:click=on_remove.clone()>
                    {glyph(CLOSE)}
                </button>
            </Show>
        </div>
    }
}

#[component]
fn WorkbenchRow(host_id: String, workbench_id: ProjectId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let branch = {
        let projects = state.projects;
        let host_id = host_id.clone();
        let workbench_id = workbench_id.clone();
        Memo::new(move |_| {
            projects.with(|projects| workbench_branch(projects, &host_id, &workbench_id))
        })
    };
    let has_agents = {
        let agents = state.agents;
        let host_id = host_id.clone();
        let workbench_id = workbench_id.clone();
        Memo::new(move |_| {
            agents.with(|agents| {
                agents.iter().any(|agent| {
                    agent.host_id == host_id && agent.project_id.as_ref() == Some(&workbench_id)
                })
            })
        })
    };
    let remove_disabled = {
        let state = state.clone();
        let host_id = host_id.clone();
        let workbench_id = workbench_id.clone();
        Memo::new(move |_| {
            crate::notices::project_mutation_pending(&state, &host_id, &workbench_id)
                && failed_workbench_removal(&state, &host_id, &workbench_id).is_none()
        })
    };
    let on_open = {
        let state = state.clone();
        let project = ActiveProjectRef {
            host_id: host_id.clone(),
            project_id: workbench_id.clone(),
        };
        move |_| state.switch_active_project(Some(project.clone()))
    };
    let on_remove = move |_| {
        request_remove_workbench(state.clone(), host_id.clone(), workbench_id.clone());
    };
    view! {
        <div class="pp-row pp-row-swap">
            <span class="pp-row-main pp-branch-primary" title=move || branch.get()>{move || branch.get()}</span>
            <Show when=move || !has_agents.get()>
                <span class="pp-row-meta">"No agents"</span>
            </Show>
            <button class="pp-row-act" aria-label="Open workbench" title="Open workbench" on:click=on_open>
                {glyph(OPEN)}
            </button>
            <button class="pp-row-act" aria-label="Remove workbench" title="Remove workbench" disabled=move || remove_disabled.get() on:click=on_remove>
                {glyph(CLOSE)}
            </button>
        </div>
    }
}

#[component]
pub fn ProjectPage(host_id: String, project_id: ProjectId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let target = ActiveProjectRef {
        host_id: host_id.clone(),
        project_id: project_id.clone(),
    };
    let target = StoredValue::new(target);
    let state_sv = StoredValue::new(state.clone());

    let project = {
        let projects = state.projects;
        Memo::new(move |_| {
            target.with_value(|target| {
                projects.with(|projects| {
                    projects
                        .iter()
                        .find(|info| {
                            info.host_id == target.host_id && info.project.id == target.project_id
                        })
                        .map(|info| info.project.clone())
                })
            })
        })
    };
    let name = Memo::new(move |_| {
        project
            .get()
            .map(|project| project.name)
            .unwrap_or_default()
    });

    let lanes = {
        let projects = state.projects;
        let agents = state.agents;
        Memo::new(move |_| {
            target.with_value(|target| {
                projects
                    .with(|projects| agents.with(|agents| board_lanes(projects, agents, target)))
            })
        })
    };

    // Display-only: ages advance with the wall clock, not with server events.
    let (now_ms, set_now_ms) = signal(js_sys::Date::now() as u64);
    let ticker = set_interval_with_handle(
        move || set_now_ms.set(js_sys::Date::now() as u64),
        Duration::from_secs(30),
    );
    on_cleanup(move || {
        if let Ok(handle) = ticker {
            handle.clear();
        }
    });

    let review_count = {
        let review_summaries = state.review_summaries;
        Signal::derive(move || {
            let open = target.with_value(|target| {
                review_summaries.with(|map| {
                    map.get(&target.project_id)
                        .and_then(|summaries| pick_workspace_draft(summaries))
                        .map_or(0, |draft| {
                            draft.user_comment_count + draft.pending_suggestion_count
                        })
                })
            });
            format!("{open} open")
        })
    };
    let steering_count = {
        let steering = state.steering;
        Signal::derive(move || {
            target
                .with_value(|target| {
                    steering.with(|map| {
                        map.get(&target.host_id).map_or(0, |items| {
                            items
                                .values()
                                .filter(|item| steering_in_project(item, &target.project_id))
                                .count()
                        })
                    })
                })
                .to_string()
        })
    };
    let workflow_count = {
        let workflow_summaries = state.workflow_summaries;
        Signal::derive(move || {
            target
                .with_value(|target| {
                    workflow_summaries.with(|map| {
                        map.get(&target.host_id).map_or(0, |summaries| {
                            effective_summaries(summaries, Some(&target.project_id)).len()
                        })
                    })
                })
                .to_string()
        })
    };
    let swarm_count = Signal::derive(move || {
        state_sv
            .with_value(|state| target.with_value(|target| project_swarm_count(state, target)))
            .to_string()
    });

    let roots = Memo::new(move |_| {
        project
            .get()
            .map(|project| project.root_paths())
            .unwrap_or_default()
    });
    let can_manage_roots = {
        let state = state.clone();
        Memo::new(move |_| {
            target.with_value(|target| {
                state.can_manage_project_roots(&target.host_id, &target.project_id)
            })
        })
    };
    let workbenches = {
        let projects = state.projects;
        Memo::new(move |_| {
            target.with_value(|target| {
                projects.with(|projects| {
                    projects
                        .iter()
                        .filter(|info| {
                            info.host_id == target.host_id
                                && info.project.parent_project_id() == Some(&target.project_id)
                        })
                        .map(|info| info.project.id.clone())
                        .collect::<Vec<_>>()
                })
            })
        })
    };

    let busy = {
        let state = state.clone();
        Memo::new(move |_| {
            target.with_value(|target| {
                crate::notices::project_mutation_pending(
                    &state,
                    &target.host_id,
                    &target.project_id,
                )
            })
        })
    };
    let has_workbenches = Memo::new(move |_| workbenches.with(|ids| !ids.is_empty()));

    let menu: CardMenuPosition = RwSignal::new(None);
    let menu_anchor = NodeRef::<leptos::html::Div>::new();
    let renaming = RwSignal::new(false);
    let rename_input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if renaming.get()
            && let Some(input) = rename_input.get()
        {
            let _ = input.focus();
            input.select();
        }
    });
    let commit_rename = move |value: String| {
        if !renaming.get_untracked() {
            return;
        }
        renaming.set(false);
        if value.trim().is_empty() || value.trim() == name.get_untracked() {
            return;
        }
        state_sv.with_value(|state| {
            target.with_value(|target| {
                crate::actions::rename_project(
                    state,
                    target.host_id.clone(),
                    target.project_id.clone(),
                    value,
                )
            })
        });
    };

    let new_agent = move |_| {
        let (state, target) = (state_sv.get_value(), target.get_value());
        spawn_local(async move {
            if let Err(error) = crate::actions::begin_new_chat_default_at(
                &state,
                target.host_id.clone(),
                Some(target),
            )
            .await
            {
                log::error!("failed to start a new agent from the project page: {error}");
            }
        });
    };

    let workbench_prompt = RwSignal::new(None::<WorkbenchCreatePrompt>);
    let on_new_workbench = move |_| {
        target.with_value(|target| {
            workbench_prompt.set(Some(WorkbenchCreatePrompt {
                host_id: target.host_id.clone(),
                parent_project_id: target.project_id.clone(),
                parent_name: name.get_untracked(),
            }))
        });
    };
    let on_add_root = move |_| {
        state_sv.with_value(|state| {
            target.with_value(|target| open_add_root_browser_for(state, target.clone()))
        });
    };

    let open_review = Callback::new(move |_| {
        state_sv.with_value(|state| {
            target.with_value(|target| {
                open_comments_for_project(state, &target.host_id, &target.project_id)
            })
        });
    });
    let open_steering = Callback::new(move |_| {
        let (state, target) = (state_sv.get_value(), target.get_value());
        spawn_local(async move {
            if let Err(error) = crate::actions::select_new_chat_host(&state, &target.host_id).await
            {
                report_user_error(format!("Could not switch host: {error}"));
                return;
            }
            state.steering_project_filter.set(Some(target));
            state.settings_tab_request.set(Some("Steering"));
            state.settings_open.set(true);
        });
    });
    let open_workflows = Callback::new(move |_| {
        state_sv.with_value(|state| state.activate_sidebar_tab(SidebarTab::Workflows));
    });
    let open_swarms = Callback::new(move |_| {
        state_sv.with_value(|state| {
            state.swarm_project_filter.set(Some(target.get_value()));
            state.activate_sidebar_tab(SidebarTab::Swarms);
        });
    });

    let lane_host = host_id;
    view! {
        <div class="pp-page">
            <header class="pp-head">
                {move || if renaming.get() {
                    view! {
                        <input
                            class="pp-title-input"
                            aria-label="Project name"
                            node_ref=rename_input
                            prop:value=name.get_untracked()
                            on:keydown=move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                                "Enter" => commit_rename(event_target_value(&ev)),
                                "Escape" => renaming.set(false),
                                _ => {}
                            }
                            on:blur=move |ev| commit_rename(event_target_value(&ev))
                        />
                    }.into_any()
                } else {
                    view! { <h1 class="pp-title" title=move || name.get()>{move || name.get()}</h1> }.into_any()
                }}
                <div class="pp-actions">
                    <div node_ref=menu_anchor>
                        <button
                            class="pp-btn pp-btn-icon"
                            aria-label="Project menu"
                            on:click=move |ev| open_card_menu_below(menu, &ev)
                        >
                            {glyph(MORE)}
                        </button>
                    </div>
                    <button class="pp-btn pp-btn-primary" on:click=new_agent>
                        {glyph(PLUS)}
                        "New agent"
                    </button>
                </div>
            </header>
            <CardContextMenu menu=menu label="Project actions" anchor=menu_anchor>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    disabled=move || busy.get()
                    on:click=move |_| {
                        menu.set(None);
                        renaming.set(true);
                    }
                >
                    "Rename"
                </button>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    disabled=move || has_workbenches.get() || busy.get()
                    title=move || if has_workbenches.get() { "Remove its workbenches first" } else { "" }
                    on:click=move |_| {
                        menu.set(None);
                        target.with_value(|target| {
                            request_delete_project(
                                state_sv.get_value(),
                                target.host_id.clone(),
                                target.project_id.clone(),
                            )
                        });
                    }
                >
                    "Delete Project"
                </button>
            </CardContextMenu>
            <div class="pp-body">
                <div class="pp-board">
                    <div class="pp-lanes">
                        <For
                            each=move || lanes.with(|lanes| lanes.iter().map(|lane| lane.name.clone()).collect::<Vec<_>>())
                            key=|name| name.clone()
                            let:lane_name
                        >
                            {
                                let name_for_cards = lane_name.clone();
                                let cards = Signal::derive(move || {
                                    lanes.with(|lanes| {
                                        lanes
                                            .iter()
                                            .find(|lane| lane.name == name_for_cards)
                                            .map(|lane| lane.cards.clone())
                                            .unwrap_or_default()
                                    })
                                });
                                view! { <LaneColumn name=lane_name host_id=lane_host.clone() cards=cards now_ms=now_ms /> }
                            }
                        </For>
                    </div>
                </div>
                <aside class="pp-details">
                    <KvRow label="Review" value=review_count on_open=open_review />
                    <div class="pp-details-scroll">
                        <section>
                            <div class="pp-sec-head">
                                "Roots"
                                <span class="pp-count">{move || roots.with(Vec::len)}</span>
                                <Show when=move || can_manage_roots.get()>
                                    <button class="pp-sec-add" aria-label="Add root" title="Add root" on:click=on_add_root>
                                        {glyph(PLUS)}
                                    </button>
                                </Show>
                            </div>
                            <div class="pp-list">
                                <For
                                    each=move || roots.get()
                                    key=|root| root.0.clone()
                                    let:root
                                >
                                    <RootRow
                                        project=target.get_value()
                                        root=root
                                        can_remove=Signal::derive(move || can_manage_roots.get() && roots.with(|roots| roots.len() > 1))
                                    />
                                </For>
                            </div>
                        </section>
                        <section>
                            <div class="pp-sec-head">
                                "Workbenches"
                                <span class="pp-count">{move || workbenches.with(Vec::len)}</span>
                                <button
                                    class="pp-sec-add"
                                    aria-label="New workbench"
                                    title="New workbench"
                                    disabled=move || busy.get()
                                    on:click=on_new_workbench
                                >
                                    {glyph(PLUS)}
                                </button>
                            </div>
                            <div class="pp-list">
                                <For
                                    each=move || workbenches.get()
                                    key=|id| id.clone()
                                    let:workbench_id
                                >
                                    <WorkbenchRow host_id=target.with_value(|target| target.host_id.clone()) workbench_id=workbench_id />
                                </For>
                            </div>
                        </section>
                    </div>
                    <div class="pp-details-foot">
                        <KvRow label="Steering" value=steering_count on_open=open_steering />
                        <KvRow label="Workflows" value=workflow_count on_open=open_workflows />
                        <KvRow label="Swarms" value=swarm_count on_open=open_swarms />
                    </div>
                </aside>
            </div>
            {move || workbench_prompt.get().map(|prompt| view! {
                <WorkbenchCreateModal prompt=prompt workbench_prompt=workbench_prompt />
            })}
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::settings_panel::SteeringTab;
    use crate::components::swarms_panel::SwarmsPanel;
    use crate::state::ActiveAgentRef;
    use crate::wasm_test_support::mount_to;
    use protocol::{
        BackendKind, GitBranchName, Project, ProjectGitChangeKind, ProjectGitFileStatus,
        ProjectRootGitStatus, Steering, SteeringId, SteeringScope, StreamPath, SwarmConstraints,
        SwarmDraft, SwarmDraftGeneration, SwarmDraftId,
    };
    use std::collections::HashMap;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    const HOST: &str = "project-page-host";

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("project-page-test-css")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("project-page-test-css");
            style.set_text_content(Some(include_str!("../../styles.css")));
            document.head().unwrap().append_child(&style).unwrap();
        }
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container
            .set_attribute("style", "display:flex;width:1200px;height:680px;")
            .unwrap();
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

    fn agent(id: &str, name: &str, project: &str, created_at_ms: u64) -> AgentInfo {
        AgentInfo {
            tyde_permissions: Default::default(),
            host_id: HOST.to_owned(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: Some(ProjectId(project.to_owned())),
            parent_agent_id: None,
            team_member_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms,
            instance_stream: StreamPath(format!("/agent/{id}/inst")),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
            review_recipient: false,
        }
    }

    fn workbench(id: &str, branch: &str) -> ProjectInfo {
        ProjectInfo {
            host_id: HOST.to_owned(),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: branch.to_owned(),
                sort_order: 0,
                source: ProjectSource::GitWorkbench {
                    parent_project_id: ProjectId("page".to_owned()),
                    branch: GitBranchName(branch.to_owned()),
                    roots: Vec::new(),
                    environment: Default::default(),
                },
            },
        }
    }

    fn root_status(root: &str, files: usize) -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath(root.to_owned()),
            branch: Some("main".to_owned()),
            head_oid: None,
            empty_tree_oid: None,
            ahead: 0,
            behind: 0,
            clean: files == 0,
            files: (0..files)
                .map(|index| ProjectGitFileStatus {
                    relative_path: format!("src/{index}.rs"),
                    staged: None,
                    unstaged: Some(ProjectGitChangeKind::Modified),
                    untracked: false,
                })
                .collect(),
            recent_commits: Vec::new(),
            history_has_more: false,
        }
    }

    fn seed() -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(HOST.to_owned()));
        state.projects.set(vec![
            ProjectInfo {
                host_id: HOST.to_owned(),
                project: Project {
                    id: ProjectId("page".to_owned()),
                    name: "Tyde".to_owned(),
                    sort_order: 0,
                    source: ProjectSource::Standalone {
                        roots: vec![
                            ProjectRootPath("/work/tyde".to_owned()),
                            ProjectRootPath("/work/docs".to_owned()),
                        ],
                    },
                },
            },
            workbench("wb-busy", "feat/busy"),
            workbench("wb-empty", "feat/empty"),
        ]);
        let mut sub_agent = agent("sub", "Sub agent", "page", 40);
        sub_agent.parent_agent_id = Some(AgentId("older".to_owned()));
        let mut swarm_member = agent("member", "Swarm member", "page", 50);
        swarm_member.origin = AgentOrigin::SwarmMember;
        state.agents.set(vec![
            agent("older", "Older card", "page", 10),
            agent("newer", "Workbench card", "wb-busy", 20),
            agent("elsewhere", "Other project card", "other", 30),
            sub_agent,
            swarm_member,
        ]);
        state.agent_turn_active.update(|turns| {
            turns.insert(AgentId("newer".to_owned()), true);
        });
        state.git_status.set(HashMap::from([(
            ProjectId("page".to_owned()),
            vec![root_status("/work/tyde", 3), root_status("/work/docs", 0)],
        )]));
        state.steering.update(|steering| {
            let items = [
                (
                    "Project rule",
                    SteeringScope::Project(ProjectId("page".to_owned())),
                ),
                ("Host rule", SteeringScope::Host),
            ]
            .into_iter()
            .map(|(id, scope)| {
                let id = SteeringId(id.to_owned());
                let item = Steering {
                    id: id.clone(),
                    scope,
                    title: id.0.clone(),
                    content: id.0.clone(),
                };
                (id, item)
            })
            .collect();
            steering.insert(HOST.to_owned(), items);
        });
        state.active_project.set(Some(ActiveProjectRef {
            host_id: HOST.to_owned(),
            project_id: ProjectId("page".to_owned()),
        }));
        state.swarm_drafts.update(|drafts| {
            let items = [
                ("Page crew", "page"),
                ("Workbench crew", "wb-busy"),
                ("Other crew", "other"),
            ]
            .into_iter()
            .map(|(name, project)| {
                let id = SwarmDraftId(name.to_owned());
                let draft = SwarmDraft {
                    legacy_source: None,
                    retained_sessions: Default::default(),
                    id: id.clone(),
                    revision: 1,
                    name: name.to_owned(),
                    constraints: SwarmConstraints {
                        project_id: ProjectId(project.to_owned()),
                        workspace_policy: Default::default(),
                        max_live_agents: 1,
                        allocations: Vec::new(),
                        shared_guidance: String::new(),
                        agent_wake_budget: None,
                    },
                    members: Vec::new(),
                    conflicts: Vec::new(),
                    generation: SwarmDraftGeneration::DeterministicGeneralists,
                    legacy_team_id: None,
                };
                (id, draft)
            })
            .collect();
            drafts.insert(HOST.to_owned(), items);
        });
        state
    }

    fn elements(container: &HtmlElement, selector: &str) -> Vec<Element> {
        let nodes = container.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .filter_map(|index| nodes.item(index)?.dyn_into::<Element>().ok())
            .collect()
    }

    fn text(element: &Element) -> String {
        element.text_content().unwrap_or_default()
    }

    fn row_with(container: &HtmlElement, label: &str) -> Element {
        elements(container, "aside button, aside section > div > div")
            .into_iter()
            .find(|row| text(row).starts_with(label))
            .unwrap_or_else(|| panic!("no details row starts with {label}"))
    }

    fn click(element: &Element) {
        element.dyn_ref::<HtmlElement>().unwrap().click();
    }

    #[wasm_bindgen_test]
    async fn page_projects_board_and_details_and_routes_clicks() {
        let container = make_container();
        let state = seed();
        let mounted = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mounted.clone());
            view! { <ProjectPage host_id=HOST.to_owned() project_id=ProjectId("page".to_owned()) /> }
        });
        let surfaces = make_container();
        let mounted = state.clone();
        let _surfaces = mount_to(surfaces.clone(), move || {
            provide_context(mounted.clone());
            view! { <SteeringTab /> <SwarmsPanel /> }
        });
        next_tick().await;

        let page_text = container.text_content().unwrap_or_default();
        assert!(page_text.contains("Tyde"), "header names the project");
        assert!(page_text.contains("New agent"), "header offers a new agent");
        for hidden in ["Sub agent", "Swarm member", "Other project card"] {
            assert!(!page_text.contains(hidden), "board must exclude {hidden}");
        }

        let lane = container
            .query_selector("section[aria-label='Active']")
            .unwrap()
            .unwrap();
        let cards: Vec<Element> = elements(&container, "section[aria-label='Active'] button");
        assert_eq!(cards.len(), 2, "one card per top-level agent");
        assert!(
            text(&lane).starts_with("Active2"),
            "lane shows its count: {}",
            text(&lane)
        );
        assert!(
            text(&cards[0]).contains("Workbench card"),
            "newest card leads"
        );
        assert!(
            text(&cards[0]).contains("Thinking"),
            "card shows server-derived state"
        );
        assert!(
            text(&cards[0]).contains("feat/busy"),
            "workbench card shows its branch"
        );
        assert!(text(&cards[1]).contains("Older card"));
        assert!(text(&cards[1]).contains("Idle"));
        assert!(
            !text(&cards[1]).contains("feat/"),
            "plain card shows no branch"
        );

        assert_eq!(text(&row_with(&container, "Review")), "Review0 open");
        let aside = container.query_selector("aside").unwrap().unwrap();
        let aside_text = text(&aside);
        let positions: Vec<usize> = [
            "Review",
            "Roots2",
            "Workbenches2",
            "Steering1",
            "Workflows0",
            "Swarms2",
        ]
        .iter()
        .map(|label| {
            aside_text
                .find(label)
                .unwrap_or_else(|| panic!("details show {label}: {aside_text}"))
        })
        .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "details order: {aside_text}"
        );

        let dirty = row_with(&container, "tyde");
        assert_eq!(text(&dirty), "tydemain · 3 changed");
        assert_eq!(text(&row_with(&container, "docs")), "docsmain · clean");
        assert_eq!(text(&row_with(&container, "feat/busy")), "feat/busy");
        let empty = row_with(&container, "feat/empty");
        assert_eq!(text(&empty), "feat/emptyNo agents");

        for row in [&dirty, &empty] {
            let row_rect = row.get_bounding_client_rect();
            let meta = elements(&container, "aside span")
                .into_iter()
                .filter(|span| {
                    row.contains(Some(span)) && span.get_bounding_client_rect().width() > 0.0
                })
                .last()
                .unwrap();
            let gap = row_rect.right() - meta.get_bounding_client_rect().right();
            assert!(
                (gap - 8.0).abs() < 0.5,
                "metadata is right-aligned, gap {gap}"
            );
        }
        let actions = elements(&container, "aside button[aria-label='Remove workbench']");
        assert_eq!(actions.len(), 2);
        assert!(
            actions
                .iter()
                .all(|action| action.get_bounding_client_rect().width() == 0.0),
            "row actions stay hidden until hover"
        );

        let swarms = row_with(&container, "Swarms").get_bounding_client_rect();
        let aside_rect = aside.get_bounding_client_rect();
        assert!(
            (aside_rect.bottom() - 16.0 - swarms.bottom()).abs() < 0.5,
            "Swarms is pinned to the bottom of the details column"
        );

        click(&cards[1]);
        next_tick().await;
        assert_eq!(
            state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                host_id: HOST.to_owned(),
                agent_id: AgentId("older".to_owned()),
            }),
            "clicking a card opens that agent"
        );

        click(&row_with(&container, "Steering"));
        next_tick().await;
        assert_eq!(state.settings_tab_request.get_untracked(), Some("Steering"));
        assert!(state.settings_open.get_untracked());
        let steering_row = row_with(&container, "Steering");
        assert_eq!(
            text(&steering_row).trim_start_matches("Steering").trim(),
            "1"
        );
        let surface_text = text(&surfaces);
        assert!(surface_text.contains("Project rule"));
        assert!(
            !surface_text.contains("Host rule"),
            "Steering shows only what the page counted"
        );

        click(&row_with(&container, "Swarms"));
        next_tick().await;
        let swarms_row = row_with(&container, "Swarms");
        assert_eq!(text(&swarms_row).trim_start_matches("Swarms").trim(), "2");
        let surface_text = text(&surfaces);
        assert!(surface_text.contains("Page crew"));
        assert!(surface_text.contains("Workbench crew"));
        assert!(
            !surface_text.contains("Other crew"),
            "Swarms shows only what the page counted"
        );
        let chips = elements(&surfaces, "button[aria-label='Clear project filter']");
        assert_eq!(chips.iter().map(text).collect::<Vec<_>>(), ["Tyde", "Tyde"]);

        state.selected_host_id.set(Some("another-host".to_owned()));
        next_tick().await;
        assert_eq!(
            elements(&surfaces, "button[aria-label='Clear project filter']").len(),
            1,
            "changing the Settings host drops the Steering project filter"
        );
        assert!(!text(&surfaces).contains("No steering on this project"));
    }
}
