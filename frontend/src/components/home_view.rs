use leptos::prelude::*;

use crate::actions::{begin_new_chat, begin_new_chat_with};
use crate::state::{AgentInfo, AppState, ConnectionStatus};

use protocol::{BackendKind, CustomAgent};

#[derive(Clone, Copy, PartialEq)]
enum HomeTab {
    Projects,
    Agents,
}

fn backend_label_for(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

#[component]
pub fn HomeView() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_tab = RwSignal::new(HomeTab::Projects);

    let connected_state = state.clone();
    let connected = Memo::new(move |_| {
        matches!(
            connected_state.chat_context_connection_status(),
            ConnectionStatus::Connected
        )
    });
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
                <NewChatButton connected_sig=connected />
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
        Some(a) if !a.started => "Initializing",
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

#[component]
fn FlyoutBody(
    open_sig: RwSignal<bool>,
    enabled_backends: Memo<Vec<BackendKind>>,
    custom_agents_for_host: Memo<Vec<CustomAgent>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let open_backend = RwSignal::new(None::<BackendKind>);

    move || {
        let backends = enabled_backends.get();
        if backends.is_empty() {
            return view! {
                <div class="panel-empty">"No enabled backends. Enable one in Settings → Backends."</div>
            }
            .into_any();
        }

        let agents = custom_agents_for_host.get();
        let rows = backends
            .into_iter()
            .map(|backend| {
                let on_row_click = {
                    let s = state.clone();
                    move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        open_sig.set(false);
                        begin_new_chat(&s, Some(backend));
                    }
                };

                let agents_for_sub = agents.clone();
                let submenu = move || {
                    if open_backend.get() != Some(backend) {
                        return None;
                    }
                    let state_sub = expect_context::<AppState>();
                    let on_default = {
                        let s = state_sub.clone();
                        move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            open_sig.set(false);
                            begin_new_chat(&s, Some(backend));
                        }
                    };
                    let agent_items = agents_for_sub
                        .clone()
                        .into_iter()
                        .map(|agent| {
                            let s = expect_context::<AppState>();
                            let id = agent.id.clone();
                            let name = agent.name.clone();
                            let desc = agent.description.clone();
                            let on_click = move |ev: web_sys::MouseEvent| {
                                ev.stop_propagation();
                                open_sig.set(false);
                                begin_new_chat_with(&s, Some(backend), Some(id.clone()));
                            };
                            view! {
                                <button
                                    class="new-chat-flyout-item"
                                    style="display:block;width:100%;text-align:left;padding:0.4rem 0.8rem;background:transparent;border:none;color:inherit;cursor:pointer;border-radius:4px;white-space:nowrap;"
                                    title=desc
                                    on:click=on_click
                                >
                                    {name}
                                </button>
                            }
                        })
                        .collect_view();
                    Some(
                        view! {
                            <div
                                class="new-chat-submenu"
                                style="position:absolute;left:100%;top:-0.5rem;background:var(--bg-surface,#1e1e1e);border:1px solid var(--border-subtle,#333);border-radius:6px;padding:0.5rem;z-index:101;box-shadow:0 4px 16px rgba(0,0,0,0.4);white-space:nowrap;"
                            >
                                <button
                                    class="new-chat-flyout-item"
                                    style="display:block;width:100%;text-align:left;padding:0.4rem 0.8rem;background:transparent;border:none;color:inherit;cursor:pointer;border-radius:4px;white-space:nowrap;"
                                    on:click=on_default
                                >
                                    "Default agent"
                                </button>
                                {agent_items}
                            </div>
                        }
                        .into_any(),
                    )
                };

                view! {
                    <div
                        class="new-chat-backend-row-wrap"
                        style="position:relative;"
                        on:mouseenter=move |_| open_backend.set(Some(backend))
                        on:mouseleave=move |_| open_backend.set(None)
                    >
                        <button
                            class="new-chat-flyout-item"
                            style="display:flex;align-items:center;gap:0.5rem;width:100%;text-align:left;padding:0.4rem 0.6rem;background:transparent;border:none;color:inherit;cursor:pointer;border-radius:4px;white-space:nowrap;"
                            on:click=on_row_click
                        >
                            <span>
                                {backend_label_for(backend)}
                            </span>
                            <span style="flex:1;"></span>
                            <span style="opacity:0.5;font-size:0.7rem;">"▶"</span>
                        </button>
                        {submenu}
                    </div>
                }
            })
            .collect_view();

        view! { <>{rows}</> }.into_any()
    }
}

#[component]
fn NewChatButton(connected_sig: Memo<bool>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let open = RwSignal::new(false);

    let state_for_default = state.clone();
    let on_primary_click = move |_| {
        if !connected_sig.get() {
            return;
        }
        begin_new_chat(&state_for_default, None);
    };

    let on_toggle = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        open.update(|v| *v = !*v);
    };

    let state_for_menu = state.clone();
    let enabled_backends = Memo::new(move |_| {
        state_for_menu
            .chat_context_host_settings()
            .map(|s| s.enabled_backends)
            .unwrap_or_default()
    });

    let state_for_agents = state.clone();
    let custom_agents_for_host = Memo::new(move |_| {
        let Some(host_id) = state_for_agents.chat_context_host_id() else {
            return Vec::<CustomAgent>::new();
        };
        let mut agents: Vec<CustomAgent> = state_for_agents
            .custom_agents
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    });

    view! {
        <>
            <Show when=move || open.get()>
                <div
                    style="position:fixed;inset:0;z-index:99;"
                    on:click=move |_| open.set(false)
                />
            </Show>
            <div
                class="new-chat-button-wrap"
                style=move || {
                    let base = "position:relative;display:inline-flex;";
                    if open.get() { format!("{base}z-index:100;") } else { base.to_string() }
                }
            >
                <button
                    class="action-btn primary"
                    style="border-radius:6px 0 0 6px;"
                    on:click=on_primary_click
                    disabled=move || !connected_sig.get()
                >
                    "New Chat"
                </button>
                <button
                    class="action-btn primary"
                    style="padding:0 0.6rem;border-left:1px solid rgba(255,255,255,0.15);border-radius:0 6px 6px 0;"
                    on:click=on_toggle
                    disabled=move || !connected_sig.get()
                    title="Pick backend and custom agent"
                >
                    "▾"
                </button>
                <Show when=move || open.get()>
                    <div
                        class="new-chat-flyout"
                        style="position:absolute;top:calc(100% + 4px);right:0;background:var(--bg-surface, #1e1e1e);border:1px solid var(--border-subtle, #333);border-radius:6px;padding:0.5rem 25px;z-index:100;box-shadow:0 4px 16px rgba(0,0,0,0.4);"
                    >
                        <FlyoutBody
                            open_sig=open
                            enabled_backends=enabled_backends
                            custom_agents_for_host=custom_agents_for_host
                        />
                    </div>
                </Show>
            </div>
        </>
    }
}
