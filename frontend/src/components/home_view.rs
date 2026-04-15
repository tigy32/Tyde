use leptos::prelude::*;

use crate::actions::begin_new_chat;
use crate::state::{AgentInfo, AppState};

use protocol::{BackendKind, Project};

#[component]
pub fn HomeView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let projects = move || state.projects.get();
    let agents = move || state.agents.get();

    let connected = Memo::new(move |_| state.host_id.get().is_some());

    let state_for_chat = state.clone();
    let new_chat = move |_| {
        begin_new_chat(&state_for_chat, None);
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
                    on:click=new_chat
                    disabled=move || !connected.get()
                    title=move || if connected.get() { "Select a project first".to_owned() } else { "Not connected".to_owned() }
                >
                    "New Chat"
                </button>
                <button
                    class="action-btn"
                    on:click=move |_| state.adding_project.set(true)
                    disabled=move || !connected.get()
                >
                    "Open Workspace"
                </button>
            </div>

            <Show when=move || !projects().is_empty()>
                <section class="home-section">
                    <h2 class="section-title">"Projects"</h2>
                    <div class="project-grid">
                        <For
                            each=projects
                            key=|p| p.id.0.clone()
                            let:project
                        >
                            <ProjectCard project=project agents=agents />
                        </For>
                    </div>
                </section>
            </Show>

            <Show when=move || !agents().is_empty()>
                <section class="home-section">
                    <h2 class="section-title">"Active Agents"</h2>
                    <div class="agent-list">
                        <For
                            each=agents
                            key=|a| a.agent_id.0.clone()
                            let:agent
                        >
                            <AgentRow agent=agent />
                        </For>
                    </div>
                </section>
            </Show>
        </div>
    }
}

#[component]
fn ProjectCard(
    project: Project,
    agents: impl Fn() -> Vec<AgentInfo> + Send + Sync + 'static,
) -> impl IntoView {
    let pid = project.id.clone();
    let agent_count = move || {
        agents()
            .iter()
            .filter(|a| a.project_id.as_ref() == Some(&pid))
            .count()
    };

    let roots_display = if project.roots.is_empty() {
        "No workspace roots".to_owned()
    } else {
        project.roots.join(", ")
    };

    view! {
        <div class="project-card">
            <div class="project-card-name">{project.name}</div>
            <div class="project-card-roots">{roots_display}</div>
            <div class="project-card-agents">
                {move || {
                    let count = agent_count();
                    match count {
                        0 => "No active agents".to_owned(),
                        1 => "1 active agent".to_owned(),
                        n => format!("{n} active agents"),
                    }
                }}
            </div>
        </div>
    }
}

#[component]
fn AgentRow(agent: AgentInfo) -> impl IntoView {
    let backend_class = match agent.backend_kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Kiro => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Gemini => "backend-badge gemini",
    };

    let backend_label = match agent.backend_kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    };

    let status_text = if agent.fatal_error.is_some() {
        "Terminated".to_string()
    } else {
        "Idle".to_string()
    };

    view! {
        <div class="agent-row">
            <span class="agent-name">{agent.name}</span>
            <span class=backend_class>{backend_label}</span>
            <span class="agent-status">{status_text}</span>
        </div>
    }
}
