use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{AgentId, FrameKind, SetAgentNamePayload};

use crate::send::{close_agent, send_frame};
use crate::state::{
    ActiveAgentRef, ActiveProjectRef, AgentInfo, AgentsPanelFilters, AppState, StreamingState,
    TabContent,
};

/// Pure predicate used by the Agents panel filter memo. Extracted so the
/// filter behavior can be unit-tested without a Leptos runtime.
pub fn agent_passes_filters(
    agent: &AgentInfo,
    filters: &AgentsPanelFilters,
    active_project: Option<&ActiveProjectRef>,
    streaming: &HashMap<AgentId, StreamingState>,
    turn_active: &HashMap<AgentId, bool>,
    lowercase_query: &str,
) -> bool {
    if filters.hide_sub_agents && agent.parent_agent_id.is_some() {
        return false;
    }
    if filters.hide_inactive {
        let is_active = !agent.started
            || streaming.contains_key(&agent.agent_id)
            || turn_active.get(&agent.agent_id).copied().unwrap_or(false);
        if !is_active {
            return false;
        }
    }
    if !filters.show_other_projects {
        let matches = match active_project {
            None => agent.project_id.is_none(),
            Some(ap) => {
                agent.host_id == ap.host_id && agent.project_id.as_ref() == Some(&ap.project_id)
            }
        };
        if !matches {
            return false;
        }
    }
    if !lowercase_query.is_empty() && !agent.name.to_lowercase().contains(lowercase_query) {
        return false;
    }
    true
}

fn backend_class(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Tycode => "backend-badge tycode",
        protocol::BackendKind::Kiro => "backend-badge kiro",
        protocol::BackendKind::Claude => "backend-badge claude",
        protocol::BackendKind::Codex => "backend-badge codex",
        protocol::BackendKind::Gemini => "backend-badge gemini",
    }
}

fn backend_label(kind: protocol::BackendKind) -> &'static str {
    match kind {
        protocol::BackendKind::Tycode => "Tycode",
        protocol::BackendKind::Kiro => "Kiro",
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

enum DerivedAgentState {
    Initializing,
    Thinking,
    Idle,
    Terminated,
}

fn status_icon(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "\u{25F7}", // ◷ clock (CSS animates)
        DerivedAgentState::Thinking => "\u{25F7}",     // ◷ clock (CSS animates)
        DerivedAgentState::Idle => "\u{2713}",         // ✓
        DerivedAgentState::Terminated => "\u{2022}",   // •
    }
}

fn status_class(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "agent-card-status running",
        DerivedAgentState::Thinking => "agent-card-status running",
        DerivedAgentState::Idle => "agent-card-status completed",
        DerivedAgentState::Terminated => "agent-card-status error",
    }
}

#[component]
pub fn AgentsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());
    // Per-parent collapse state: parents whose children are hidden.
    let collapsed_parents: RwSignal<HashSet<AgentId>> = RwSignal::new(HashSet::new());
    // Editing state lives here so it survives agent list re-renders caused by
    // streaming / turn-active updates. Only one agent can be renamed at a time.
    let editing_agent: RwSignal<Option<protocol::AgentId>> = RwSignal::new(None);
    let edit_value: RwSignal<String> = RwSignal::new(String::new());

    // Current filter values for the active project. Falls back to
    // context-aware defaults when the user hasn't toggled anything yet for
    // this project.
    let filters_state = state.clone();
    let current_filters = Memo::new(move |_| {
        let active = filters_state.active_project.get();
        let overrides = filters_state.agents_panel_filters.get();
        overrides
            .get(&active)
            .cloned()
            .unwrap_or_else(|| AgentsPanelFilters::defaults_for(active.as_ref()))
    });

    let update_filters = {
        let state = state.clone();
        move |mutate: Box<dyn FnOnce(&mut AgentsPanelFilters)>| {
            let active = state.active_project.get_untracked();
            state.agents_panel_filters.update(|map| {
                let entry = map
                    .entry(active.clone())
                    .or_insert_with(|| AgentsPanelFilters::defaults_for(active.as_ref()));
                mutate(entry);
            });
        }
    };

    let filter_state = state.clone();
    let filtered_agents = Memo::new(move |_| {
        let agents = filter_state.agents.get();
        let streaming_map = filter_state.streaming_text.get();
        let turn_active_map = filter_state.agent_turn_active.get();
        let active_project = filter_state.active_project.get();
        let query = search.get().to_lowercase();
        let filters = current_filters.get();

        agents
            .into_iter()
            .filter(|a| {
                agent_passes_filters(
                    a,
                    &filters,
                    active_project.as_ref(),
                    &streaming_map,
                    &turn_active_map,
                    &query,
                )
            })
            .collect::<Vec<_>>()
    });

    // Build parent-children grouping
    let grouped = Memo::new(move |_| {
        let agents = filtered_agents.get();
        // Parents: no parent_agent_id
        let parents: Vec<&AgentInfo> = agents
            .iter()
            .filter(|a| a.parent_agent_id.is_none())
            .collect();
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
                && !result
                    .iter()
                    .any(|(_, children)| children.iter().any(|c| c.agent_id == agent.agent_id))
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
        update_filters(Box::new(|f: &mut AgentsPanelFilters| {
            f.hide_inactive = !f.hide_inactive;
        }));
    };

    let toggle_sub = move |_| {
        update_filters(Box::new(|f: &mut AgentsPanelFilters| {
            f.hide_sub_agents = !f.hide_sub_agents;
        }));
    };

    let toggle_other_projects = move |_| {
        update_filters(Box::new(|f: &mut AgentsPanelFilters| {
            f.show_other_projects = !f.show_other_projects;
        }));
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
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                />
            </div>
            <div class="panel-filters">
                <button
                    class=move || if current_filters.get().hide_inactive { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_inactive
                >
                    "Hide inactive"
                </button>
                <button
                    class=move || if current_filters.get().hide_sub_agents { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_sub
                >
                    "Hide sub-agents"
                </button>
                <button
                    class=move || if current_filters.get().show_other_projects { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_other_projects
                >
                    "Show other projects"
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
                                    let group_id = parent_id.0.clone();
                                    let child_count = children.len();
                                    let parent_view = agent_card(state.clone(), parent, editing_agent, edit_value, child_count, collapsed_parents);
                                    let children_view = children.into_iter().map(|child| {
                                        let pid = parent_id.clone();
                                        view! {
                                            <div
                                                class=move || {
                                                    if collapsed_parents.with(|s| s.contains(&pid)) {
                                                        "agent-card-child agent-card-child-hidden"
                                                    } else {
                                                        "agent-card-child"
                                                    }
                                                }
                                            >
                                                {agent_card(state.clone(), child, editing_agent, edit_value, 0, collapsed_parents)}
                                            </div>
                                        }
                                    }).collect_view();
                                    view! {
                                        <div class="agent-card-group" data-agent-id=group_id>
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

fn agent_card(
    state: AppState,
    agent: AgentInfo,
    editing_agent: RwSignal<Option<protocol::AgentId>>,
    edit_value: RwSignal<String>,
    child_count: usize,
    collapsed_parents: RwSignal<HashSet<AgentId>>,
) -> impl IntoView {
    let agent_id = agent.agent_id.clone();
    let name = agent.name.clone();
    let backend = agent.backend_kind;
    let created = agent.created_at_ms;
    let started = agent.started;
    let has_fatal = agent.fatal_error.is_some();
    let custom_agent_id = agent.custom_agent_id.clone();
    let custom_agent_host_id = agent.host_id.clone();
    let custom_agent_state = state.clone();
    let custom_agent_name = move || {
        custom_agent_id.as_ref().and_then(|id| {
            custom_agent_state
                .custom_agents
                .get()
                .get(&custom_agent_host_id)
                .and_then(|map| map.get(id).map(|a| a.name.clone()))
        })
    };

    let error_msg = agent.fatal_error.as_ref().map(|msg| {
        let truncated: String = msg.chars().take(80).collect();
        truncated
    });

    let click_id = agent_id.clone();
    let click_host_id = agent.host_id.clone();
    let state_for_click = state.clone();
    let click_name = name.clone();
    let on_click = move |_: web_sys::MouseEvent| {
        let agent_ref = ActiveAgentRef {
            host_id: click_host_id.clone(),
            agent_id: click_id.clone(),
        };
        // Opening (and activating) the chat tab drives `active_agent` to this
        // agent via the Memo derived from `center_zone`.
        state_for_click.open_tab(
            TabContent::Chat {
                agent_ref: Some(agent_ref),
            },
            click_name.clone(),
            true,
        );
    };

    let kd_id = agent_id.clone();
    let kd_host = agent.host_id.clone();
    let kd_state = state.clone();
    let kd_name = name.clone();
    let on_keydown_card = move |ev: web_sys::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            let agent_ref = ActiveAgentRef {
                host_id: kd_host.clone(),
                agent_id: kd_id.clone(),
            };
            // active_agent is a Memo over center_zone; opening the tab drives
            // it.
            kd_state.open_tab(
                TabContent::Chat {
                    agent_ref: Some(agent_ref),
                },
                kd_name.clone(),
                true,
            );
        }
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();

    let agent_id_for_effect = agent_id.clone();
    // Auto-focus and select-all when editing mode activates.
    Effect::new(move |_| {
        if editing_agent.with(|e| e.as_ref() == Some(&agent_id_for_effect))
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
            el.select();
        }
    });

    let rename_name = name.clone();
    let agent_id_for_rename = agent_id.clone();
    let on_rename = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        edit_value.set(rename_name.clone());
        editing_agent.set(Some(agent_id_for_rename.clone()));
    };

    let host_id_for_edit = agent.host_id.clone();
    let stream_for_edit = agent.instance_stream.clone();

    let close_host_id = agent.host_id.clone();
    let close_stream = agent.instance_stream.clone();
    let close_name = name.clone();
    let close_agent_id = agent_id.clone();
    let close_state = state.clone();
    let on_close = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let window = web_sys::window().expect("window");
        let is_active = close_state
            .active_agent
            .with_untracked(|a| a.as_ref().is_some_and(|a| a.agent_id == close_agent_id));
        let has_draft = is_active
            && !close_state
                .chat_input
                .with_untracked(|s| s.trim().is_empty());
        let message = if has_draft {
            format!(
                "Close agent \"{}\"?\n\nYou have unsent input — it will be discarded. Continue?",
                close_name
            )
        } else {
            format!("Close agent \"{}\"?", close_name)
        };
        match window.confirm_with_message(&message) {
            Ok(true) => {}
            _ => return,
        }
        let host_id = close_host_id.clone();
        let stream = close_stream.clone();
        spawn_local(async move {
            if let Err(e) = close_agent(&host_id, stream).await {
                log::error!("failed to send CloseAgent: {e}");
            }
        });
    };

    let derived = {
        let agent_id = agent_id.clone();
        let streaming = state.streaming_text;
        let turn_active = state.agent_turn_active;
        move || {
            if has_fatal {
                return DerivedAgentState::Terminated;
            }
            if !started {
                return DerivedAgentState::Initializing;
            }
            let typing = turn_active.with(|map| map.get(&agent_id).copied().unwrap_or(false));
            let streaming_open = streaming.with(|map| map.contains_key(&agent_id));
            if typing || streaming_open {
                DerivedAgentState::Thinking
            } else {
                DerivedAgentState::Idle
            }
        }
    };

    let status_class_sig = {
        let derived = derived.clone();
        move || status_class(&derived())
    };
    let status_icon_sig = move || status_icon(&derived());
    let agent_id_for_editing_block = agent_id.clone();

    view! {
        <div
            class="agent-card"
            tabindex="0"
            role="button"
            on:click=on_click
            on:keydown=on_keydown_card
        >
            <div class="agent-card-top">
                <div class="agent-card-top-main">
                {move || {
                    if editing_agent.with(|e| e.as_ref() == Some(&agent_id_for_editing_block)) {
                        let host_id = host_id_for_edit.clone();
                        let stream = stream_for_edit.clone();
                        let compare = name.clone();
                        let agent_id_for_blur = agent_id_for_editing_block.clone();
                        let on_keydown = move |ev: web_sys::KeyboardEvent| {
                            ev.stop_propagation();
                            match ev.key().as_str() {
                                "Enter" => {
                                    let new_name = edit_value.get_untracked().trim().to_string();
                                    editing_agent.set(None);
                                    if !new_name.is_empty() && new_name != compare {
                                        let host_id = host_id.clone();
                                        let stream = stream.clone();
                                        spawn_local(async move {
                                            if let Err(e) = send_frame(
                                                &host_id,
                                                stream,
                                                FrameKind::SetAgentName,
                                                &SetAgentNamePayload { name: new_name },
                                            )
                                            .await
                                            {
                                                log::error!("failed to send SetAgentName: {e}");
                                            }
                                        });
                                    }
                                }
                                "Escape" => editing_agent.set(None),
                                _ => {}
                            }
                        };
                        let on_blur = {
                            let host_id = host_id_for_edit.clone();
                            let stream = stream_for_edit.clone();
                            let compare = name.clone();
                            move |_: web_sys::FocusEvent| {
                                // Guard against double-send when Enter already committed.
                                if editing_agent.with_untracked(|e| e.as_ref() != Some(&agent_id_for_blur)) {
                                    return;
                                }
                                let new_name = edit_value.get_untracked().trim().to_string();
                                editing_agent.set(None);
                                if !new_name.is_empty() && new_name != compare {
                                    let host_id = host_id.clone();
                                    let stream = stream.clone();
                                    spawn_local(async move {
                                        if let Err(e) = send_frame(
                                            &host_id,
                                            stream,
                                            FrameKind::SetAgentName,
                                            &SetAgentNamePayload { name: new_name },
                                        )
                                        .await
                                        {
                                            log::error!("failed to send SetAgentName: {e}");
                                        }
                                    });
                                }
                            }
                        };
                        view! {
                            <input
                                type="text"
                                class="agent-card-name-input"
                                node_ref=input_ref
                                prop:value=move || edit_value.get()
                                on:input=move |ev| edit_value.set(event_target_value(&ev))
                                on:keydown=on_keydown
                                on:blur=on_blur
                                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                                spellcheck="false"
                                {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                                autocapitalize="none"
                                autocomplete="off"
                            />
                        }.into_any()
                    } else {
                        view! {
                            <span class="agent-card-name">{name.clone()}</span>
                        }.into_any()
                    }
                }}
                {(child_count > 0).then(|| {
                    let agent_id_col = agent_id.clone();
                    let agent_id_icon = agent_id.clone();
                    let toggle = move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        let id = agent_id_col.clone();
                        collapsed_parents.update(|set| {
                            if set.contains(&id) {
                                set.remove(&id);
                            } else {
                                set.insert(id);
                            }
                        });
                    };
                    view! {
                        <span class="agent-card-child-badge">
                            <span class="agent-child-count">{child_count}</span>
                            <button
                                type="button"
                                class="agent-card-collapse-toggle"
                                title="Toggle sub-agents"
                                on:click=toggle
                                on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                            >
                                {move || if collapsed_parents.with(|s| s.contains(&agent_id_icon)) {
                                    "\u{25B6}"
                                } else {
                                    "\u{25BC}"
                                }}
                            </button>
                        </span>
                    }
                })}
                </div>
                <div class="agent-card-top-actions">
                    <button
                        type="button"
                        class="filter-toggle agent-card-action"
                        title="Rename agent"
                        aria-label="Rename agent"
                        on:click=on_rename
                    >
                        "\u{270E}"
                    </button>
                    <button
                        type="button"
                        class="filter-toggle agent-card-close agent-card-action"
                        title="Close agent"
                        aria-label="Close agent"
                        on:click=on_close
                        on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                    >
                        "\u{00D7}"
                    </button>
                </div>
            </div>
            <div class="agent-card-bottom">
                <span class=status_class_sig>{status_icon_sig}</span>
                <span class="agent-card-time">{relative_time(created)}</span>
                {move || custom_agent_name().map(|n| {
                    let title = format!("Custom agent: {n}");
                    view! {
                        <span class="agent-card-custom-agent" title=title>{n}</span>
                    }
                })}
                <span class={format!("{} agent-card-backend", backend_class(backend))}>{backend_label(backend)}</span>
            </div>
            {error_msg.map(|msg| view! {
                <div class="agent-card-error">{msg}</div>
            })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AgentOrigin, BackendKind, ProjectId, StreamPath};

    fn mk_agent(
        name: &str,
        host: &str,
        project_id: Option<&str>,
        parent: Option<&str>,
        started: bool,
    ) -> AgentInfo {
        AgentInfo {
            host_id: host.to_string(),
            agent_id: AgentId(format!("agent-{name}")),
            name: name.to_string(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Tycode,
            workspace_roots: vec![],
            project_id: project_id.map(|s| ProjectId(s.to_string())),
            parent_agent_id: parent.map(|p| AgentId(p.to_string())),
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath("s".to_string()),
            started,
            fatal_error: None,
        }
    }

    fn active(host: &str, project: &str) -> ActiveProjectRef {
        ActiveProjectRef {
            host_id: host.to_string(),
            project_id: ProjectId(project.to_string()),
        }
    }

    fn no_runtime() -> (HashMap<AgentId, StreamingState>, HashMap<AgentId, bool>) {
        (HashMap::new(), HashMap::new())
    }

    #[test]
    fn hide_sub_agents_drops_children_keeps_parents() {
        let parent = mk_agent("p", "h", Some("proj"), None, true);
        let child = mk_agent("c", "h", Some("proj"), Some("agent-p"), true);
        let (s, t) = no_runtime();
        let filters = AgentsPanelFilters {
            hide_sub_agents: true,
            hide_inactive: false,
            show_other_projects: true,
        };
        assert!(agent_passes_filters(
            &parent,
            &filters,
            Some(&active("h", "proj")),
            &s,
            &t,
            "",
        ));
        assert!(!agent_passes_filters(
            &child,
            &filters,
            Some(&active("h", "proj")),
            &s,
            &t,
            "",
        ));
    }

    #[test]
    fn hide_inactive_keeps_starting_streaming_and_turn_active() {
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: true,
            show_other_projects: true,
        };

        // Not yet started → treated as active (initializing).
        let starting = mk_agent("starting", "h", None, None, false);
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(&starting, &filters, None, &s, &t, ""));

        // Started + streaming.
        let streaming_agent = mk_agent("streaming", "h", None, None, true);
        let mut stream_map: HashMap<AgentId, StreamingState> = HashMap::new();
        stream_map.insert(
            streaming_agent.agent_id.clone(),
            StreamingState {
                agent_name: "streaming".to_string(),
                model: None,
                text: leptos::prelude::ArcRwSignal::new(String::new()),
                reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
            },
        );
        assert!(agent_passes_filters(
            &streaming_agent,
            &filters,
            None,
            &stream_map,
            &t,
            "",
        ));

        // Started + turn active.
        let turn_agent = mk_agent("turn", "h", None, None, true);
        let mut turn_map: HashMap<AgentId, bool> = HashMap::new();
        turn_map.insert(turn_agent.agent_id.clone(), true);
        let (s, _) = no_runtime();
        assert!(agent_passes_filters(
            &turn_agent,
            &filters,
            None,
            &s,
            &turn_map,
            "",
        ));

        // Started, idle, not streaming → hidden.
        let idle = mk_agent("idle", "h", None, None, true);
        let (s, t) = no_runtime();
        assert!(!agent_passes_filters(&idle, &filters, None, &s, &t, ""));
    }

    #[test]
    fn show_other_projects_off_on_home_keeps_only_none_project() {
        assert!(AgentsPanelFilters::defaults_for(None).show_other_projects);
        // Override to simulate user turning it off on Home.
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: false,
        };
        let home_agent = mk_agent("home", "h", None, None, true);
        let project_agent = mk_agent("proj", "h", Some("p1"), None, true);
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(
            &home_agent,
            &filters,
            None,
            &s,
            &t,
            ""
        ));
        assert!(!agent_passes_filters(
            &project_agent,
            &filters,
            None,
            &s,
            &t,
            ""
        ));
    }

    #[test]
    fn show_other_projects_off_in_project_requires_host_and_project_match() {
        let filters = AgentsPanelFilters::defaults_for(Some(&active("h1", "p1")));
        // Specific-project default is false.
        assert!(!filters.show_other_projects);

        let same = mk_agent("same", "h1", Some("p1"), None, true);
        let other_project = mk_agent("other_p", "h1", Some("p2"), None, true);
        let other_host = mk_agent("other_h", "h2", Some("p1"), None, true);
        let home_agent = mk_agent("home", "h1", None, None, true);
        let active_ref = active("h1", "p1");
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(
            &same,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            ""
        ));
        assert!(!agent_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(!agent_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(!agent_passes_filters(
            &home_agent,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
    }

    #[test]
    fn show_other_projects_on_bypasses_project_check() {
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: true,
        };
        let other_project = mk_agent("other_p", "h1", Some("p2"), None, true);
        let other_host = mk_agent("other_h", "h2", Some("p1"), None, true);
        let home_agent = mk_agent("home", "h1", None, None, true);
        let active_ref = active("h1", "p1");
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(agent_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
        assert!(agent_passes_filters(
            &home_agent,
            &filters,
            Some(&active_ref),
            &s,
            &t,
            "",
        ));
    }

    #[test]
    fn search_matches_case_insensitively() {
        let filters = AgentsPanelFilters {
            hide_sub_agents: false,
            hide_inactive: false,
            show_other_projects: true,
        };
        let agent = mk_agent("Foo Bar", "h", None, None, true);
        let (s, t) = no_runtime();
        assert!(agent_passes_filters(&agent, &filters, None, &s, &t, "foo"));
        assert!(agent_passes_filters(&agent, &filters, None, &s, &t, "bar"));
        assert!(!agent_passes_filters(&agent, &filters, None, &s, &t, "baz"));
        // Empty query passes all.
        assert!(agent_passes_filters(&agent, &filters, None, &s, &t, ""));
    }

    #[test]
    fn defaults_for_home_shows_other_projects_true() {
        assert!(AgentsPanelFilters::defaults_for(None).show_other_projects);
    }

    #[test]
    fn defaults_for_specific_project_shows_other_projects_false() {
        let ap = active("h", "p");
        assert!(!AgentsPanelFilters::defaults_for(Some(&ap)).show_other_projects);
    }
}
