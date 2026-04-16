use leptos::prelude::*;

use crate::actions::begin_new_chat;
use crate::state::{AgentInfo, AppState};

use protocol::BackendKind;

#[derive(Clone, Copy, PartialEq)]
enum HomeTab {
    Projects,
    Agents,
}

#[component]
pub fn HomeView() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_tab = RwSignal::new(HomeTab::Projects);

    let connected_state = state.clone();
    let connected = Memo::new(move |_| connected_state.active_connection_count() > 0);
    let state_for_hosts = state.clone();

    let tab_class = move |target: HomeTab| {
        move || {
            if active_tab.get() == target {
                "tab active"
            } else {
                "tab"
            }
        }
    };

    let on_new_chat = {
        let state = state.clone();
        move |_| begin_new_chat(&state, None)
    };

    view! {
        <div class="home-view">
            <div class="home-hero">
                <img class="home-logo" src="icon.png" alt="Tyde" />
                <h1 class="home-title">"Tyde"</h1>
                <p class="home-tagline">"Coding Agent Studio"</p>
            </div>

            <div class="home-hints">
                <span class="kbd-hint"><kbd>"⌘ K"</kbd>" Palette"</span>
                <span class="kbd-hint"><kbd>"⌘ N"</kbd>" New Chat"</span>
                <span class="kbd-hint"><kbd>"⌘ ,"</kbd>" Settings"</span>
            </div>

            <div class="home-actions">
                <button
                    class="action-btn primary"
                    on:click=on_new_chat
                    disabled=move || !connected.get()
                >
                    "New Chat"
                </button>
                <button
                    class="action-btn"
                    on:click=move |_| state.settings_open.set(true)
                >
                    "Manage Hosts"
                </button>
            </div>

            <div class="tab-bar home-tab-bar">
                <button class={tab_class(HomeTab::Projects)} on:click=move |_| active_tab.set(HomeTab::Projects)>"Projects"</button>
                <button class={tab_class(HomeTab::Agents)} on:click=move |_| active_tab.set(HomeTab::Agents)>"Agents"</button>
            </div>

            {move || match active_tab.get() {
                HomeTab::Projects => {
                    let state_for_arm = state.clone();
                    let state_for_hosts = state_for_hosts.clone();
                    view! {
                    <section class="home-section">
                        <For
                            each=move || state_for_hosts.configured_hosts.get()
                            key=|host| host.id.clone()
                            let:host
                        >
                            {
                                let state = state_for_arm.clone();
                                let host_id = host.id.clone();
                                let host_label = host.label.clone();

                                let state_for_status = state.clone();
                                let host_id_for_status = host_id.clone();
                                let status = move || {
                                    state_for_status.connection_statuses
                                        .get()
                                        .get(&host_id_for_status)
                                        .cloned()
                                        .unwrap_or(crate::state::ConnectionStatus::Disconnected)
                                };

                                let state_for_projects = state.clone();
                                let host_id_for_projects = host_id.clone();
                                let projects_view = move || {
                                    let state = state_for_projects.clone();
                                    let host_id_filter = host_id_for_projects.clone();
                                    let projects: Vec<_> = state.projects.get()
                                        .into_iter()
                                        .filter(|project| project.host_id == host_id_filter)
                                        .collect();
                                    if projects.is_empty() {
                                        view! { <div class="panel-empty">"No projects"</div> }.into_any()
                                    } else {
                                        let agents = state.agents.get();
                                        view! {
                                            <div class="project-grid">
                                                {projects.into_iter().map(|project| {
                                                    view! { <ProjectCard project=project.project agents=agents.clone() host_id=project.host_id /> }
                                                }).collect_view()}
                                            </div>
                                        }.into_any()
                                    }
                                };

                                view! {
                                    <div class="home-host-section">
                                        <div class="home-host-header">
                                            <h2 class="section-title">{host_label.clone()}</h2>
                                            <span class="home-host-status">
                                                {move || match status() {
                                                    crate::state::ConnectionStatus::Connected => "Connected",
                                                    crate::state::ConnectionStatus::Connecting => "Connecting",
                                                    crate::state::ConnectionStatus::Disconnected => "Disconnected",
                                                    crate::state::ConnectionStatus::Error(_) => "Error",
                                                }}
                                            </span>
                                        </div>
                                        {projects_view}
                                    </div>
                                }
                            }
                        </For>
                    </section>
                    }.into_any()
                },
                HomeTab::Agents => {
                    let state_for_agents = state.clone();
                    view! {
                    <section class="home-section">
                        <div class="agent-list">
                            <For
                                each=move || state_for_agents.agents.get()
                                key=|agent| format!("{}:{}", agent.host_id, agent.agent_id.0)
                                let:agent
                            >
                                <AgentRow host_id=agent.host_id agent_id=agent.agent_id />
                            </For>
                        </div>
                    </section>
                    }.into_any()
                },
            }}
        </div>
    }
}

#[component]
fn ProjectCard(
    project: protocol::Project,
    agents: Vec<AgentInfo>,
    host_id: String,
) -> impl IntoView {
    let project_id = project.id.clone();
    let agent_count = agents
        .iter()
        .filter(|agent| agent.host_id == host_id && agent.project_id.as_ref() == Some(&project_id))
        .count();

    let roots_display = if project.roots.is_empty() {
        "No workspace roots".to_string()
    } else {
        project.roots.join(", ")
    };

    view! {
        <div class="project-card">
            <div class="project-card-name">{project.name}</div>
            <div class="project-card-roots">{roots_display}</div>
            <div class="project-card-agents">
                {match agent_count {
                    0 => "No active agents".to_string(),
                    1 => "1 active agent".to_string(),
                    n => format!("{n} active agents"),
                }}
            </div>
        </div>
    }
}

#[component]
fn AgentRow(host_id: String, agent_id: protocol::AgentId) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Look up the agent reactively so state changes (fatal_error, name, etc.)
    // flow through without relying on the keyed `<For>` to recreate this row.
    let agent = move || {
        state
            .agents
            .get()
            .into_iter()
            .find(|a| a.host_id == host_id && a.agent_id == agent_id)
    };

    let agent_for_name = agent.clone();
    let name = move || agent_for_name().map(|a| a.name).unwrap_or_default();

    let agent_for_backend = agent.clone();
    let backend_class = move || match agent_for_backend().map(|a| a.backend_kind) {
        Some(BackendKind::Tycode) => "backend-badge tycode",
        Some(BackendKind::Kiro) => "backend-badge kiro",
        Some(BackendKind::Claude) => "backend-badge claude",
        Some(BackendKind::Codex) => "backend-badge codex",
        Some(BackendKind::Gemini) => "backend-badge gemini",
        None => "backend-badge",
    };

    let agent_for_label = agent.clone();
    let backend_label = move || match agent_for_label().map(|a| a.backend_kind) {
        Some(BackendKind::Tycode) => "Tycode",
        Some(BackendKind::Kiro) => "Kiro",
        Some(BackendKind::Claude) => "Claude",
        Some(BackendKind::Codex) => "Codex",
        Some(BackendKind::Gemini) => "Gemini",
        None => "",
    };

    let status_text = move || match agent() {
        Some(a) if a.fatal_error.is_some() => "Terminated",
        Some(_) => "Idle",
        None => "",
    };

    view! {
        <div class="agent-row">
            <span class="agent-name">{name}</span>
            <span class=backend_class>{backend_label}</span>
            <span class="agent-status">{status_text}</span>
        </div>
    }
}
