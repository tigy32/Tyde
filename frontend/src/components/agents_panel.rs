use leptos::prelude::*;

use crate::state::{AgentInfo, AgentStatus, AppState, CenterView};

fn backend_class(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Claude => "backend-badge claude",
        protocol::BackendKind::Codex => "backend-badge codex",
        protocol::BackendKind::Gemini => "backend-badge gemini",
    }
}

fn backend_label(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Claude => "Claude",
        protocol::BackendKind::Codex => "Codex",
        protocol::BackendKind::Gemini => "Gemini",
    }
}

fn relative_time(created_at_ms: u64) -> String {
    let now = js_sys::Date::now() as u64;
    let diff_secs = now.saturating_sub(created_at_ms) / 1000;

    if diff_secs < 60 {
        "just now".to_string()
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86400)
    }
}

fn status_icon(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "\u{25F7}",  // ◷ clock
        AgentStatus::Running => "\u{25F7}",   // ◷ clock (CSS animates)
        AgentStatus::Completed => "\u{2713}", // ✓
        AgentStatus::Error(_) => "\u{2022}",  // •
    }
}

fn status_class(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "agent-card-status starting",
        AgentStatus::Running => "agent-card-status running",
        AgentStatus::Completed => "agent-card-status completed",
        AgentStatus::Error(_) => "agent-card-status error",
    }
}

#[component]
pub fn AgentsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());
    let hide_inactive = RwSignal::new(false);
    let hide_sub_agents = RwSignal::new(false);

    let filtered_agents = Memo::new(move |_| {
        let agents = state.agents.get();
        let query = search.get().to_lowercase();
        let hide_sub = hide_sub_agents.get();
        let hide_done = hide_inactive.get();

        agents
            .into_iter()
            .filter(|a| {
                if hide_sub && a.parent_agent_id.is_some() {
                    return false;
                }
                if hide_done {
                    match &a.status {
                        AgentStatus::Completed | AgentStatus::Error(_) => return false,
                        _ => {}
                    }
                }
                if !query.is_empty() && !a.name.to_lowercase().contains(&query) {
                    return false;
                }
                true
            })
            .collect::<Vec<_>>()
    });

    // Build parent-children grouping
    let grouped = Memo::new(move |_| {
        let agents = filtered_agents.get();
        // Parents: no parent_agent_id
        let parents: Vec<&AgentInfo> = agents.iter().filter(|a| a.parent_agent_id.is_none()).collect();
        let mut result: Vec<(AgentInfo, Vec<AgentInfo>)> = Vec::new();
        for parent in parents {
            let children: Vec<AgentInfo> = agents
                .iter()
                .filter(|a| a.parent_agent_id.as_ref() == Some(&parent.agent_id))
                .cloned()
                .collect();
            result.push((parent.clone(), children));
        }
        // Orphans: agents whose parent is filtered out
        let parent_ids: Vec<_> = result.iter().map(|(p, _)| p.agent_id.clone()).collect();
        for agent in &agents {
            if agent.parent_agent_id.is_some()
                && !parent_ids.contains(&agent.parent_agent_id.as_ref().unwrap().clone())
                && !result.iter().any(|(_, children)| children.iter().any(|c| c.agent_id == agent.agent_id))
            {
                result.push((agent.clone(), Vec::new()));
            }
        }
        result
    });

    let on_search = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        search.set(val);
    };

    let toggle_inactive = move |_| {
        hide_inactive.set(!hide_inactive.get());
    };

    let toggle_sub = move |_| {
        hide_sub_agents.set(!hide_sub_agents.get());
    };

    view! {
        <div class="panel agents-panel">
            <div class="panel-search">
                <input
                    type="text"
                    class="panel-search-input"
                    placeholder="Filter agents..."
                    prop:value=search
                    on:input=on_search
                />
            </div>
            <div class="panel-filters">
                <button
                    class=move || if hide_inactive.get() { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_inactive
                >
                    "Hide inactive"
                </button>
                <button
                    class=move || if hide_sub_agents.get() { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_sub
                >
                    "Hide sub-agents"
                </button>
            </div>
            <div class="panel-content">
                {move || {
                    let groups = grouped.get();
                    if groups.is_empty() {
                        view! {
                            <div class="panel-empty">"No agents yet"</div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="agent-card-list">
                                {groups.into_iter().map(|(parent, children)| {
                                    let parent_id = parent.agent_id.clone();
                                    let parent_view = agent_card(state.clone(), parent);
                                    let children_view = children.into_iter().map(|child| {
                                        view! {
                                            <div class="agent-card-child">
                                                {agent_card(state.clone(), child)}
                                            </div>
                                        }
                                    }).collect_view();
                                    view! {
                                        <div class="agent-card-group" data-agent-id=parent_id.0>
                                            {parent_view}
                                            {children_view}
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}

fn agent_card(state: AppState, agent: AgentInfo) -> impl IntoView {
    let agent_id = agent.agent_id.clone();
    let name = agent.name.clone();
    let backend = agent.backend_kind;
    let created = agent.created_at_ms;
    let status = agent.status.clone();
    let error_msg = match &status {
        AgentStatus::Error(msg) => {
            let truncated: String = msg.chars().take(80).collect();
            Some(truncated)
        }
        _ => None,
    };

    let click_id = agent_id.clone();
    let on_click = move |_| {
        state.active_agent_id.set(Some(click_id.clone()));
        state.center_view.set(CenterView::Chat);
    };

    view! {
        <button class="agent-card" on:click=on_click>
            <div class="agent-card-top">
                <span class="agent-card-name">{name}</span>
                <span class={backend_class(backend)}>{backend_label(backend)}</span>
            </div>
            <div class="agent-card-bottom">
                <span class={status_class(&status)}>{status_icon(&status)}</span>
                <span class="agent-card-time">{relative_time(created)}</span>
            </div>
            {error_msg.map(|msg| view! {
                <div class="agent-card-error">{msg}</div>
            })}
        </button>
    }
}
