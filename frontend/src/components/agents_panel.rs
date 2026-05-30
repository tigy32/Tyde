use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{AgentId, FrameKind, SetAgentNamePayload};

use crate::send::{close_agent, compact_agent, send_frame};
use crate::state::{
    ActiveAgentRef, ActiveProjectRef, AgentInfo, AgentsPanelFilters, AppState, ConnectionStatus,
    StreamingState, TabContent,
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
    Compacting,
    Terminated,
}

fn status_icon(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "\u{25F7}", // ◷ clock (CSS animates)
        DerivedAgentState::Thinking => "\u{25F7}",     // ◷ clock (CSS animates)
        DerivedAgentState::Compacting => "\u{27F2}",   // ⟲ counter-clockwise gapped circle
        DerivedAgentState::Idle => "\u{2713}",         // ✓
        DerivedAgentState::Terminated => "\u{2022}",   // •
    }
}

fn status_class(derived: &DerivedAgentState) -> &'static str {
    match derived {
        DerivedAgentState::Initializing => "agent-card-status running",
        DerivedAgentState::Thinking => "agent-card-status running",
        DerivedAgentState::Compacting => "agent-card-status running",
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
        let active_project = filter_state.active_project.get();
        let query = search.get().to_lowercase();
        let filters = current_filters.get();

        // Read the noisy maps in place via `with` rather than cloning
        // them up-front. The Memo re-runs on every keystroke in the
        // panel-search input, and cloning the streaming/turn-active
        // HashMaps + the full agents Vec on each keystroke was the
        // dominant per-keystroke cost in the audit.
        filter_state.streaming_text.with(|streaming_map| {
            filter_state.agent_turn_active.with(|turn_active_map| {
                filter_state.agents.with(|agents| {
                    agents
                        .iter()
                        .filter(|a| {
                            agent_passes_filters(
                                a,
                                &filters,
                                active_project.as_ref(),
                                streaming_map,
                                turn_active_map,
                                &query,
                            )
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
        })
    });

    // Build parent-children grouping in O(N). The previous version
    // did O(N×M) parent-lookup per parent and a second O(N) pass to
    // detect orphans, which the agents panel re-runs on every chat
    // streaming delta because `filtered_agents` subscribes to
    // `streaming_text` and `agent_turn_active`. With dozens of agents
    // and a fast model this would dominate per-delta main-thread time.
    let grouped = Memo::new(move |_| {
        use std::collections::{HashMap, HashSet};
        let agents = filtered_agents.get();

        // Index every visible agent's id once; lets us tell orphans
        // (parent filtered out) from real children in a single pass.
        let visible_ids: HashSet<AgentId> = agents.iter().map(|a| a.agent_id.clone()).collect();

        // Bucket children by parent_agent_id. One alloc per parent
        // group; orphans land in a synthetic "orphan" bucket whose
        // entries we later promote to top-level rows.
        let mut children_by_parent: HashMap<AgentId, Vec<AgentInfo>> = HashMap::new();
        let mut top_level: Vec<AgentInfo> = Vec::new();
        let mut orphans: Vec<AgentInfo> = Vec::new();
        for agent in agents {
            match &agent.parent_agent_id {
                Some(pid) if visible_ids.contains(pid) => {
                    children_by_parent
                        .entry(pid.clone())
                        .or_default()
                        .push(agent);
                }
                Some(_) => orphans.push(agent),
                None => top_level.push(agent),
            }
        }

        let mut result: Vec<(AgentInfo, Vec<AgentInfo>)> =
            Vec::with_capacity(top_level.len() + orphans.len());
        for parent in top_level {
            let children = children_by_parent
                .remove(&parent.agent_id)
                .unwrap_or_default();
            result.push((parent, children));
        }
        for orphan in orphans {
            result.push((orphan, Vec::new()));
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
            TabContent::chat_with_agent(agent_ref),
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
                TabContent::chat_with_agent(agent_ref),
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
        let host_id = close_host_id.clone();
        let stream = close_stream.clone();
        spawn_local(async move {
            if !crate::bridge::confirm_dialog("Close agent", &message).await {
                return;
            }
            if let Err(e) = close_agent(&host_id, stream).await {
                log::error!("failed to send CloseAgent: {e}");
            }
        });
    };

    let derived = {
        let agent_id = agent_id.clone();
        let streaming = state.streaming_text;
        let turn_active = state.agent_turn_active;
        let compaction = state.compaction_in_progress;
        move || {
            if has_fatal {
                return DerivedAgentState::Terminated;
            }
            if !started {
                return DerivedAgentState::Initializing;
            }
            if compaction.with(|map| map.contains_key(&agent_id)) {
                return DerivedAgentState::Compacting;
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
    let status_icon_sig = {
        let derived = derived.clone();
        move || status_icon(&derived())
    };

    // Compact (Compact/Rotate) action — gated on the agent being idle on a
    // connected host with at least one chat row, and not already mid-
    // compaction. Hidden when gating fails so the button surface mirrors
    // the existing hover-revealed Close (`agent-card-action`) UX.
    let can_compact = {
        let host_id = agent.host_id.clone();
        let agent_id = agent_id.clone();
        let derived = derived.clone();
        let state = state.clone();
        move || {
            if !matches!(
                state.connection_status_for_host(&host_id),
                ConnectionStatus::Connected
            ) {
                return false;
            }
            if state
                .chat_rows
                .with(|map| map.get(&agent_id).is_none_or(|rows| rows.is_empty()))
            {
                return false;
            }
            matches!(derived(), DerivedAgentState::Idle)
        }
    };
    let compact_host_id = agent.host_id.clone();
    let compact_agent_id = agent_id.clone();
    let compact_agent_stream = agent.instance_stream.clone();
    let compact_name = name.clone();
    let compact_state = state.clone();
    let on_compact = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let host_id = compact_host_id.clone();
        let aid = compact_agent_id.clone();
        let agent_stream = compact_agent_stream.clone();
        // The server marks the predecessor session non-resumable as
        // part of the compaction protocol, so don't promise the user
        // they can pick it back up. The summary remains visible in
        // Sessions as a read-only record of what was kept.
        let message = format!(
            "Compact agent \"{}\"?\n\nThe agent will write a summary of context worth keeping and a fresh replacement will start from that summary. The original session is closed and kept in Sessions as a read-only record — you can view it, but it can't be resumed.",
            compact_name
        );
        let state = compact_state.clone();
        spawn_local(async move {
            if !crate::bridge::confirm_dialog("Compact agent", &message).await {
                return;
            }
            state.mark_compaction_started(&host_id, aid.clone());
            if let Err(e) = compact_agent(&host_id, agent_stream).await {
                log::error!("failed to send AgentCompact: {e}");
                state.finish_compaction_failure(aid, e);
            }
        });
    };

    let compaction_error_msg = {
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || state.compaction_errors.with(|m| m.get(&agent_id).cloned())
    };

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
                    {move || can_compact().then(|| view! {
                        <button
                            type="button"
                            class="filter-toggle agent-card-compact agent-card-action"
                            title="Compact agent"
                            aria-label="Compact agent"
                            on:click=on_compact.clone()
                            on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                        >
                            "\u{27F2}"
                        </button>
                    })}
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
            {move || compaction_error_msg().map(|msg| view! {
                <div class="agent-card-error agent-card-error-compaction">{msg}</div>
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::dispatch::dispatch_envelope;
    use crate::state::{ChatMessageEntry, ChatRowHandle};
    use leptos::mount::mount_to;
    use protocol::types::{
        AgentCompactNotifyPayload, AgentCompactStatus, TeamCompactNotifyPayload, TeamCompactStatus,
    };
    use protocol::{
        AgentOrigin, BackendKind, ChatMessage, Envelope, MessageSender, NewAgentPayload,
        StreamPath, TeamId, TeamMemberId,
    };
    use serde_json::Value as JsonValue;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 600px; height: 800px;",
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

    /// Stub `window.__TAURI__.core.invoke` so every call is recorded into
    /// `window.__test_send_calls`, `plugin:dialog|message` resolves to
    /// `"Ok"` (the user clicked OK on the native confirm), and everything
    /// else resolves to undefined. The recorded JS array is returned so
    /// tests can read it after triggering UI actions.
    fn install_send_stub_with_dialog_ok() -> js_sys::Array {
        let calls = js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    if (cmd === 'plugin:dialog|message') {
                        return Promise.resolve('Ok');
                    }
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
            "#,
        )
        .expect("install tauri stub");
        calls.dyn_into::<js_sys::Array>().expect("array")
    }

    /// Walk `window.__test_send_calls` and return `(frame_kind, payload)`
    /// tuples for every `send_host_line` invoke. Mirrors the
    /// `recorded_frames` helper in teams_panel's tests so the assertion
    /// shape stays consistent across the crate.
    fn recorded_frames(calls: &js_sys::Array) -> Vec<(String, JsonValue, String)> {
        let mut out = Vec::new();
        for entry in calls.iter() {
            let arr = entry.dyn_into::<js_sys::Array>().expect("entry array");
            let cmd = arr.get(0).as_string().expect("cmd is string");
            if cmd != "send_host_line" {
                continue;
            }
            let args_json = arr.get(1).as_string().expect("args json string");
            let args: JsonValue = serde_json::from_str(&args_json).expect("args parse");
            let line = args
                .get("line")
                .and_then(|v| v.as_str())
                .expect("line present");
            let envelope: JsonValue = serde_json::from_str(line).expect("envelope parse");
            let kind = envelope
                .get("kind")
                .and_then(|v| v.as_str())
                .expect("kind present")
                .to_string();
            let stream = envelope
                .get("stream")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let payload = envelope.get("payload").cloned().unwrap_or(JsonValue::Null);
            out.push((kind, payload, stream));
        }
        out
    }

    /// Synthesize an `Envelope` and feed it through `dispatch_envelope`
    /// for the tests that drive the AgentCompactNotify state machine.
    /// Sequence is advanced per (host, stream) so the seq validator
    /// doesn't reject subsequent frames in the same test.
    fn dispatch_frame<T: serde::Serialize>(
        state: &AppState,
        host_id: &str,
        stream: StreamPath,
        kind: FrameKind,
        seq: u64,
        payload: &T,
    ) {
        let envelope =
            Envelope::from_payload(stream, kind, seq, payload).expect("envelope serialize");
        dispatch_envelope(state, host_id, envelope);
    }

    fn make_app_state(host_id: &str) -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|map| {
            map.insert(host_id.to_owned(), StreamPath(format!("/host/{host_id}")));
        });
        state.connection_statuses.update(|map| {
            map.insert(
                host_id.to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        state
    }

    fn push_agent(state: &AppState, host_id: &str, agent_id: &str, name: &str, started: bool) {
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                // Mirror the real backend format `/agent/<id>/<uuid>`.
                // Using a stable suffix keeps tests deterministic; the
                // protocol validator only cares about the registered
                // path equality, not the uuid value.
                instance_stream: StreamPath(format!("/agent/{agent_id}/inst")),
                started,
                fatal_error: None,
            });
        });
    }

    fn seed_chat_row(state: &AppState, agent_id: &str) {
        state.chat_rows.update(|m| {
            m.insert(
                AgentId(agent_id.to_owned()),
                vec![ChatRowHandle::new(ChatMessageEntry {
                    message: ChatMessage {
                        message_id: None,
                        timestamp: 0,
                        sender: MessageSender::User,
                        content: "hi".to_owned(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                    tool_requests: Vec::new(),
                })],
            );
        });
    }

    fn compact_btn(container: &HtmlElement) -> Option<HtmlElement> {
        container
            .query_selector(".agent-card-compact")
            .unwrap()
            .map(|e| e.dyn_into::<HtmlElement>().unwrap())
    }

    /// Mount `AgentsPanel` and return the handle. Caller MUST bind the
    /// handle to a local (e.g. `_handle`) — dropping it tears down the
    /// Leptos root, which empties the container and makes any DOM probe
    /// trivially fail.
    fn mount_panel(container: &HtmlElement, state: AppState) -> impl Sized {
        let state_for_mount = state;
        mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <AgentsPanel /> }
        })
    }

    /// Idle agent on a connected host with at least one chat row should
    /// expose the Compact action.
    #[wasm_bindgen_test]
    async fn compact_button_visible_when_idle_with_history_and_connected() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-idle", "Agent", true);
        seed_chat_row(&state, "a-idle");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let btn = compact_btn(&container).expect("compact button should render for idle agent");
        assert_eq!(
            btn.get_attribute("aria-label").as_deref(),
            Some("Compact agent"),
            "compact button must keep a labelled affordance"
        );
    }

    /// Initializing (server hasn't echoed AgentStart) — Compact must be
    /// hidden so the user can't fire a rotation before the agent is even
    /// ready.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_initializing() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-init", "Agent", false);
        seed_chat_row(&state, "a-init");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render for the initializing agent"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden while the agent is still initializing"
        );
    }

    /// Thinking (turn active or streaming open) — Compact must be hidden.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_thinking() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-thinking", "Agent", true);
        seed_chat_row(&state, "a-thinking");
        state.agent_turn_active.update(|m| {
            m.insert(AgentId("a-thinking".to_owned()), true);
        });
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render for the thinking agent"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden while the agent is taking a turn"
        );
    }

    /// No chat rows yet — compaction is wasted spend on an unused agent.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_no_chat_history() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-blank", "Agent", true);
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render even with no chat rows"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden for agents that have no chat rows yet"
        );
    }

    /// Disconnected host — Compact must be hidden because the request
    /// can't reach the server.
    #[wasm_bindgen_test]
    async fn compact_button_hidden_when_host_disconnected() {
        let container = make_container();
        let state = make_app_state("h");
        state.connection_statuses.update(|m| {
            m.insert("h".to_owned(), crate::state::ConnectionStatus::Disconnected);
        });
        push_agent(&state, "h", "a-disc", "Agent", true);
        seed_chat_row(&state, "a-disc");
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            container.query_selector(".agent-card").unwrap().is_some(),
            "agent card itself should render even when host is disconnected"
        );
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden when the host is disconnected"
        );
    }

    /// Already compacting — Compact button must be hidden so the user
    /// can't double-fire, and the status pill must render the running-
    /// blue style we use elsewhere for in-flight work.
    #[wasm_bindgen_test]
    async fn compacting_state_hides_button_and_shows_running_pill() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-busy", "Agent", true);
        seed_chat_row(&state, "a-busy");
        state.mark_compaction_started("h", AgentId("a-busy".to_owned()));
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            compact_btn(&container).is_none(),
            "compact button must be hidden once a compaction is in flight"
        );
        let status_pill: HtmlElement = container
            .query_selector(".agent-card-status")
            .unwrap()
            .expect("status pill present")
            .dyn_into()
            .unwrap();
        let class = status_pill.get_attribute("class").unwrap_or_default();
        assert!(
            class.contains("running"),
            "compacting status pill should use the running class for the blue pulse, got: {class}"
        );
    }

    /// Compaction failure surfaces a non-fatal inline error and the
    /// predecessor agent is back to idle (Compact button is offered
    /// again).
    #[wasm_bindgen_test]
    async fn compaction_failure_shows_inline_error_and_reenables_button() {
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-fail", "Agent", true);
        seed_chat_row(&state, "a-fail");
        state.finish_compaction_failure(
            AgentId("a-fail".to_owned()),
            "summary backend returned an error".to_owned(),
        );
        let _handle = mount_panel(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let error_row: HtmlElement = container
            .query_selector(".agent-card-error-compaction")
            .unwrap()
            .expect("compaction error footer present")
            .dyn_into()
            .unwrap();
        assert!(
            error_row
                .text_content()
                .unwrap_or_default()
                .contains("summary backend"),
            "error row should display the server-reported reason"
        );
        assert!(
            compact_btn(&container).is_some(),
            "compact button should be offered again after a non-fatal failure"
        );
    }

    /// Clicking Compact through the OK-stubbed confirm dialog actually
    /// sends an `AgentCompact` frame on the *agent's* instance stream
    /// (not the host stream), with a `Default::default()` payload as
    /// per the Backend contract. The local state also flips to
    /// in-flight so the next render shows the running pill.
    #[wasm_bindgen_test]
    async fn clicking_compact_sends_agent_compact_frame_on_agent_stream() {
        let calls = install_send_stub_with_dialog_ok();
        let container = make_container();
        let state = make_app_state("h");
        push_agent(&state, "h", "a-click", "Agent", true);
        seed_chat_row(&state, "a-click");
        let _handle = mount_panel(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let btn = compact_btn(&container).expect("compact button should render");
        btn.click();
        for _ in 0..8 {
            next_tick().await;
        }

        let frames = recorded_frames(&calls);
        let compact_frames: Vec<_> = frames
            .iter()
            .filter(|(kind, _, _)| kind == &FrameKind::AgentCompact.to_string())
            .collect();
        assert_eq!(
            compact_frames.len(),
            1,
            "exactly one AgentCompact frame should be sent, all frames: {frames:?}"
        );
        let (_, payload, stream) = compact_frames[0];
        assert_eq!(
            stream, "/agent/a-click/inst",
            "AgentCompact must target the agent's instance stream, not the host stream"
        );
        assert_eq!(
            payload,
            &serde_json::json!({}),
            "default AgentCompactPayload omits the optional tuning fields"
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-click".to_owned()))),
            "agent should be flagged as in-flight while the server processes"
        );
    }

    /// The dispatcher's INBOUND_SEQ and INBOUND_PROTOCOL validators are
    /// process-wide thread-locals that persist across wasm tests. Each
    /// compaction test dispatches a fresh `(host_id, stream)` pair, so we
    /// reset that host's seq state AND wipe the protocol validator's
    /// stream registry at the top of every test. Without the protocol
    /// reset, a NewAgent for `/agent/a-new/inst` in one test would trip
    /// the duplicate-stream check in the next test that uses the same
    /// path.
    fn reset_inbound_seqs(state: &AppState, host_id: &str) {
        crate::dispatch::prime_host_for_tests(state, host_id);
    }

    /// Real backend stream format for an agent instance. The protocol
    /// validator rejects agent-stream traffic on streams that were
    /// never registered via NewAgent, so tests that send AgentCompact*
    /// or AgentClosed frames must use stream paths that match the
    /// `/agent/<agent_id>/<uuid>` pattern the server actually emits.
    fn agent_stream(agent_id: &str) -> StreamPath {
        StreamPath(format!("/agent/{agent_id}/inst"))
    }

    /// Dispatch a NewAgent frame so the protocol validator registers
    /// the agent's `/agent/<id>/inst` instance stream. Without this,
    /// subsequent AgentCompactNotify / AgentClosed frames on the agent
    /// stream are rejected as "unknown agent_id". The seq returned is
    /// the next free seq on the `/host/<host_id>` stream so callers
    /// can chain further host-stream frames.
    fn register_agent_via_new_agent(
        state: &AppState,
        host_id: &str,
        agent_id: &str,
        name: &str,
        host_seq: u64,
        created_at_ms: u64,
    ) {
        dispatch_frame(
            state,
            host_id,
            StreamPath(format!("/host/{host_id}")),
            FrameKind::NewAgent,
            host_seq,
            &NewAgentPayload {
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                custom_agent_id: None,
                team_id: None,
                team_member_id: None,
                project_id: None,
                parent_agent_id: None,
                created_at_ms,
                instance_stream: agent_stream(agent_id),
            },
        );
        // Prime the agent's instance stream so subsequent
        // AgentCompactNotify / AgentClosed / ChatEvent frames pass the
        // bootstrap-first check the protocol validator now enforces.
        crate::dispatch::prime_agent_stream_for_tests(
            state,
            host_id,
            &agent_stream(agent_id),
            &protocol::AgentStartPayload {
                agent_id: AgentId(agent_id.to_owned()),
                name: name.to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                custom_agent_id: None,
                team_id: None,
                team_member_id: None,
                project_id: None,
                parent_agent_id: None,
                created_at_ms,
            },
        );
    }

    /// `AgentCompactNotify` with status `Started` flips the agent into
    /// `compaction_in_progress` even if the user never clicked Compact
    /// (e.g. compaction was kicked off by a server-side rule). Uses a
    /// real `/agent/<id>/<uuid>` stream so the protocol validator
    /// path is exercised, not bypassed.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_started_marks_in_progress() {
        let state = make_app_state("h-started");
        reset_inbound_seqs(&state, "h-started");
        register_agent_via_new_agent(&state, "h-started", "a-old", "Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        dispatch_frame(
            &state,
            "h-started",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Started,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: None,
                new_session_id: None,
                summary_preview: None,
                message: None,
            },
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "Started notify must mark the old agent in-flight"
        );
    }

    /// `Failed` notify clears the in-flight flag and stores the
    /// server-reported reason as a non-fatal error so the card surfaces
    /// it inline without flipping the agent to Terminated.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_failed_clears_in_progress_and_stores_error() {
        let state = make_app_state("h-failed");
        reset_inbound_seqs(&state, "h-failed");
        register_agent_via_new_agent(&state, "h-failed", "a-old", "Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        state.mark_compaction_started("h-failed", AgentId("a-old".to_owned()));
        dispatch_frame(
            &state,
            "h-failed",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Failed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: None,
                new_session_id: None,
                summary_preview: None,
                message: Some("summary backend returned an error".to_owned()),
            },
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "Failed notify must clear the in-flight flag"
        );
        let err = state
            .compaction_errors
            .with(|m| m.get(&AgentId("a-old".to_owned())).cloned())
            .expect("error message stored");
        assert!(err.contains("summary backend"), "got error {err:?}");
    }

    /// `Completed` notify when the replacement's `NewAgent` echo is
    /// already in state retargets every chat tab pointing at the old
    /// agent over to the new one — same TabId / scroll / focus, just a
    /// new agent_ref. Mirrors the `upgrade_pending_team_member_tab`
    /// contract. Uses real `/agent/<id>/<uuid>` streams.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_completed_after_new_agent_retargets_tab() {
        let state = make_app_state("h-after");
        reset_inbound_seqs(&state, "h-after");
        // Register `a-old` via a real NewAgent frame. For User-origin
        // agents this also auto-opens a chat tab — that's the very
        // user-perceived tab the retarget needs to preserve.
        register_agent_via_new_agent(&state, "h-after", "a-old", "Old Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        let tab_id_before = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id)
            .expect("NewAgent should have auto-opened a chat tab for a-old");
        let tabs_before = state.center_zone.with_untracked(|cz| cz.tabs.len());
        // User clicks Compact: the fingerprint is captured now. When
        // NewAgent for the replacement arrives next, the fingerprint
        // suppression keeps it from stealing focus / opening a duplicate.
        state.mark_compaction_started("h-after", AgentId("a-old".to_owned()));
        register_agent_via_new_agent(&state, "h-after", "a-new", "Compacted Agent", 1, 1);
        assert_eq!(
            state.center_zone.with_untracked(|cz| cz.tabs.len()),
            tabs_before,
            "replacement NewAgent must not open a duplicate tab while compaction is in flight"
        );

        dispatch_frame(
            &state,
            "h-after",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: Some(AgentId("a-new".to_owned())),
                new_session_id: None,
                summary_preview: Some("Worked on the wizard.".to_owned()),
                message: None,
            },
        );

        let (label, ar, tab_id_after) = state.center_zone.with_untracked(|cz| {
            let tab = cz.active_tab().expect("active tab still present");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => ar.clone(),
                _ => panic!("active tab should still be a Chat after retarget"),
            };
            (tab.label.clone(), agent_ref, tab.id)
        });
        assert_eq!(
            tab_id_after, tab_id_before,
            "retarget must preserve the TabId so the tab does not remount"
        );
        assert_eq!(ar.agent_id, AgentId("a-new".to_owned()));
        assert_eq!(label, "Compacted Agent");
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "in-flight flag cleared on Completed"
        );
        assert!(
            state.compaction_pending_completion.with(|m| m.is_empty()),
            "no pending mapping should linger when NewAgent is already in state"
        );
    }

    /// `Completed` notify can race ahead of the replacement's
    /// `NewAgent` echo. When that happens the dispatcher stashes the
    /// (host, new) → old mapping in `compaction_pending_completion`,
    /// and the `NewAgent` arm later flushes it to do the retarget.
    /// This test exercises that ordering using real `/agent/<id>/<uuid>`
    /// streams so the protocol validator path is exercised.
    #[wasm_bindgen_test]
    async fn dispatch_compact_notify_completed_before_new_agent_defers_then_flushes() {
        let state = make_app_state("h-defer");
        reset_inbound_seqs(&state, "h-defer");
        register_agent_via_new_agent(&state, "h-defer", "a-old", "Old Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        let tab_id_before = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id)
            .expect("NewAgent should have auto-opened a chat tab for a-old");
        state.mark_compaction_started("h-defer", AgentId("a-old".to_owned()));

        // Completed arrives FIRST, while the replacement isn't in
        // state.agents yet. Note we send on a-old's REAL agent stream
        // — the backend's new contract is that Completed lands while
        // the old stream is still valid (i.e. before AgentClosed
        // invalidates it).
        dispatch_frame(
            &state,
            "h-defer",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: Some(AgentId("a-new".to_owned())),
                new_session_id: None,
                summary_preview: None,
                message: None,
            },
        );
        // The retarget is deferred; the tab still points at the old
        // agent, but the pending mapping is recorded.
        let still_old = state.center_zone.with_untracked(|cz| {
            cz.active_tab().and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => Some(ar.agent_id.clone()),
                _ => None,
            })
        });
        assert_eq!(still_old, Some(AgentId("a-old".to_owned())));
        assert!(
            state
                .compaction_pending_completion
                .with(|m| m.contains_key(&("h-defer".to_owned(), AgentId("a-new".to_owned())))),
            "pending mapping should be recorded until NewAgent arrives"
        );

        // Now the replacement's NewAgent echo lands on the host stream
        // (seq=1 since a-old's NewAgent occupied seq=0). The NewAgent
        // dispatch arm should flush the pending mapping and call
        // finish_compaction_success.
        register_agent_via_new_agent(&state, "h-defer", "a-new", "Compacted Agent", 1, 1);

        let (label, ar, tab_id_after) = state.center_zone.with_untracked(|cz| {
            let tab = cz.active_tab().expect("active tab still present");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => ar.clone(),
                _ => panic!("active tab should still be a Chat after retarget"),
            };
            (tab.label.clone(), agent_ref, tab.id)
        });
        assert_eq!(tab_id_after, tab_id_before, "TabId preserved across flush");
        assert_eq!(ar.agent_id, AgentId("a-new".to_owned()));
        assert_eq!(label, "Compacted Agent");
        assert!(
            state.compaction_pending_completion.with(|m| m.is_empty()),
            "pending mapping must be drained after the NewAgent flush"
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "in-flight flag cleared once retarget finalizes"
        );
    }

    /// Fixed backend contract regression: `NewAgent` (replacement) →
    /// `AgentCompactNotify::Completed` on the old agent's still-valid
    /// stream → `AgentClosed` (old). All frames use real
    /// `/agent/<id>/<uuid>` stream paths so the protocol validator
    /// path is exercised (the validator rejects agent-stream traffic
    /// after `AgentClosed` removes the stream, which is exactly why
    /// the backend must deliver `Completed` BEFORE `AgentClosed`).
    ///
    /// Asserts the user-visible contract:
    ///   1. Replacement `NewAgent` does NOT open a duplicate chat tab.
    ///   2. `Completed` retargets the existing tab to the replacement
    ///      in place — same `TabId`, new `agent_ref`, new label.
    ///   3. The subsequent `AgentClosed` for old does NOT close the
    ///      retargeted tab.
    ///   4. Once `AgentClosed` runs the old agent's transient state
    ///      (agents row, chat_rows, etc.) is gone.
    #[wasm_bindgen_test]
    async fn qa_ordering_new_then_completed_then_close_preserves_tab() {
        let state = make_app_state("h-qa");
        reset_inbound_seqs(&state, "h-qa");
        // Register a-old via a real NewAgent. For User-origin agents
        // this also auto-opens the user's chat tab.
        register_agent_via_new_agent(&state, "h-qa", "a-old", "Old Agent", 0, 0);
        seed_chat_row(&state, "a-old");
        let tab_id_before = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id)
            .expect("NewAgent should have auto-opened a chat tab for a-old");
        let tabs_before = state.center_zone.with_untracked(|cz| cz.tabs.len());

        // User clicks Compact — fingerprint captured. Replacement
        // NewAgent arrives next; without the dispatcher's fingerprint
        // suppression it would steal focus into a duplicate tab.
        state.mark_compaction_started("h-qa", AgentId("a-old".to_owned()));

        // 1. Replacement NewAgent arrives on /host/h-qa (seq=1 because
        //    a-old's NewAgent occupied seq=0).
        register_agent_via_new_agent(&state, "h-qa", "a-new", "Compacted Agent", 1, 1);
        let after_new_agent_tab_count = state.center_zone.with_untracked(|cz| cz.tabs.len());
        let after_new_agent_active = state.center_zone.with_untracked(|cz| {
            cz.active_tab().and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => Some(ar.agent_id.clone()),
                _ => None,
            })
        });
        assert_eq!(
            after_new_agent_tab_count, tabs_before,
            "replacement NewAgent must not open a duplicate chat tab"
        );
        assert_eq!(
            after_new_agent_active,
            Some(AgentId("a-old".to_owned())),
            "active tab must still point at the old agent until Completed retargets it"
        );

        // 2. Completed arrives on the OLD agent's instance stream,
        //    while that stream is still valid (the protocol validator
        //    would reject this frame if it arrived after AgentClosed).
        dispatch_frame(
            &state,
            "h-qa",
            agent_stream("a-old"),
            FrameKind::AgentCompactNotify,
            0,
            &AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: AgentId("a-old".to_owned()),
                old_session_id: None,
                new_agent_id: Some(AgentId("a-new".to_owned())),
                new_session_id: None,
                summary_preview: Some("Worked on the wizard.".to_owned()),
                message: None,
            },
        );
        let (label, ar, tab_id_after, tabs_after) = state.center_zone.with_untracked(|cz| {
            let tab = cz.active_tab().expect("active tab still present");
            let agent_ref = match &tab.content {
                TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } => ar.clone(),
                _ => panic!("active tab should be a Chat after retarget"),
            };
            (tab.label.clone(), agent_ref, tab.id, cz.tabs.len())
        });
        assert_eq!(
            tab_id_after, tab_id_before,
            "Completed retarget must preserve the TabId so the tab does not remount"
        );
        assert_eq!(
            ar.agent_id,
            AgentId("a-new".to_owned()),
            "tab agent_ref should now point at the replacement"
        );
        assert_eq!(label, "Compacted Agent");
        assert_eq!(
            tabs_after, tabs_before,
            "no duplicate tab introduced through retarget"
        );
        assert!(
            !state
                .compaction_in_progress
                .with_untracked(|map| map.contains_key(&AgentId("a-old".to_owned()))),
            "in-flight flag cleared on Completed"
        );

        // 3. AgentClosed for old arrives last. This is the "normal"
        //    close path (compaction_in_progress no longer has a-old),
        //    so we expect transient state for a-old to be cleaned up.
        //    The retargeted tab now points at a-new, so the close
        //    sweep finds no matching Chat tab and must leave it alone.
        // seq=2 on /host/h-qa: a-old=0, a-new=1, AgentClosed=2.
        dispatch_frame(
            &state,
            "h-qa",
            StreamPath("/host/h-qa".to_owned()),
            FrameKind::AgentClosed,
            2,
            &protocol::AgentClosedPayload {
                agent_id: AgentId("a-old".to_owned()),
            },
        );

        // The retargeted tab is still here, still pointing at a-new.
        let (final_label, final_ar, final_tab_id, final_tab_count) =
            state.center_zone.with_untracked(|cz| {
                let tab = cz.active_tab().expect("active tab still present");
                let agent_ref = match &tab.content {
                    TabContent::Chat {
                        agent_ref: Some(ar),
                        ..
                    } => ar.clone(),
                    _ => panic!("active tab should still be a Chat after AgentClosed"),
                };
                (tab.label.clone(), agent_ref, tab.id, cz.tabs.len())
            });
        assert_eq!(
            final_tab_id, tab_id_before,
            "AgentClosed must not remount or replace the retargeted tab"
        );
        assert_eq!(
            final_ar.agent_id,
            AgentId("a-new".to_owned()),
            "AgentClosed for the old agent must not flip agent_ref back"
        );
        assert_eq!(final_label, "Compacted Agent");
        assert_eq!(
            final_tab_count, tabs_before,
            "AgentClosed for the old agent must not close the retargeted tab"
        );

        // 4. Old agent transient state cleaned up by the normal
        //    apply_agent_closed path (compaction_in_progress was
        //    empty so no defer; teardown ran immediately).
        assert!(
            state.agents.with_untracked(|agents| agents
                .iter()
                .all(|a| a.agent_id != AgentId("a-old".to_owned()))),
            "old AgentInfo must be cleaned up after AgentClosed"
        );
        assert!(
            !state
                .chat_rows
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "old chat_rows must be cleaned up after AgentClosed"
        );
        assert!(
            !state
                .agent_session_settings
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "old agent_session_settings must be cleaned up after AgentClosed"
        );
        assert!(
            state.compaction_pending_close.with(|set| set.is_empty()),
            "pending-close set must remain empty under the new contract"
        );
    }

    /// Defensive belt: `finalize_compaction_close` cleans up the same
    /// transient maps `apply_agent_closed` does. The new backend
    /// contract delivers `Completed` before `AgentClosed`, so the
    /// deferred-close path normally isn't exercised — but we still
    /// want the cleanup parity intact in case ordering ever inverts.
    /// This drives `finalize_compaction_close` directly via the
    /// state API to keep the assertion narrow and protocol-free.
    #[wasm_bindgen_test]
    async fn finalize_compaction_close_clears_agent_session_settings() {
        let state = make_app_state("h-clean");
        push_agent(&state, "h-clean", "a-old", "Old Agent", true);
        seed_chat_row(&state, "a-old");
        state.agent_session_settings.update(|map| {
            map.insert(
                AgentId("a-old".to_owned()),
                protocol::SessionSettingsValues::default(),
            );
        });
        // Drive the same code path finish_compaction_success calls
        // after retargeting: drop the deferred-close entry's transient
        // state for the old agent.
        state.finish_compaction_success(
            &AgentId("a-old".to_owned()),
            &AgentInfo {
                host_id: "h-clean".to_owned(),
                agent_id: AgentId("a-new".to_owned()),
                name: "New".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                custom_agent_id: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-new/inst".to_owned()),
                started: true,
                fatal_error: None,
            },
        );
        // Without an entry in compaction_pending_close,
        // finish_compaction_success does NOT call finalize — that's
        // intentional. Add one and re-trigger by calling
        // defer_compaction_close + a synthetic
        // finish_compaction_success.
        state.defer_compaction_close("h-clean", AgentId("a-old".to_owned()));
        state.finish_compaction_success(
            &AgentId("a-old".to_owned()),
            &AgentInfo {
                host_id: "h-clean".to_owned(),
                agent_id: AgentId("a-new".to_owned()),
                name: "New".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                custom_agent_id: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-new/inst".to_owned()),
                started: true,
                fatal_error: None,
            },
        );
        assert!(
            !state
                .agent_session_settings
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "finalize_compaction_close must drop agent_session_settings for the old agent"
        );
        assert!(
            !state
                .chat_rows
                .with_untracked(|m| m.contains_key(&AgentId("a-old".to_owned()))),
            "finalize_compaction_close must drop chat_rows for the old agent"
        );
        assert!(
            state.agents.with_untracked(|agents| agents
                .iter()
                .all(|a| a.agent_id != AgentId("a-old".to_owned()))),
            "finalize_compaction_close must drop the old AgentInfo"
        );
    }

    /// `TeamCompactNotify::Started` flips every targeted agent into
    /// `compaction_in_progress` even when the user never clicked Compact
    /// in this client (a team compact may have been initiated from
    /// another client / server-side rule). Idempotent if the local
    /// click handler had already marked them.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_started_marks_all_targets_in_progress() {
        let state = make_app_state("h-team-started");
        reset_inbound_seqs(&state, "h-team-started");
        register_agent_via_new_agent(&state, "h-team-started", "a-mgr", "Manager", 0, 0);
        register_agent_via_new_agent(&state, "h-team-started", "a-rep", "Reporter", 1, 1);
        seed_chat_row(&state, "a-mgr");
        seed_chat_row(&state, "a-rep");
        dispatch_frame(
            &state,
            "h-team-started",
            StreamPath("/host/h-team-started".to_owned()),
            FrameKind::TeamCompactNotify,
            2,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Started,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![
                    TeamMemberId("m-mgr".to_owned()),
                    TeamMemberId("m-rep".to_owned()),
                ],
                agent_ids: vec![AgentId("a-mgr".to_owned()), AgentId("a-rep".to_owned())],
                results: Vec::new(),
                message: None,
            },
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-mgr".to_owned()))),
            "Started team notify must mark every targeted agent in-flight (a-mgr)"
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-rep".to_owned()))),
            "Started team notify must mark every targeted agent in-flight (a-rep)"
        );
    }

    /// `TeamCompactNotify::Completed` carries one
    /// `AgentCompactNotifyPayload` per target. The dispatcher must
    /// drive each through the same per-agent state machine: chat tabs
    /// retarget to the new agent, `compaction_in_progress` clears.
    /// Per-agent `AgentCompactNotify` frames are NOT emitted to the
    /// client during a team compact, so this aggregated path is the
    /// only place the UI learns of completion.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_completed_retargets_each_member_tab() {
        let state = make_app_state("h-team-completed");
        reset_inbound_seqs(&state, "h-team-completed");
        register_agent_via_new_agent(&state, "h-team-completed", "a-mgr-old", "Manager", 0, 0);
        register_agent_via_new_agent(&state, "h-team-completed", "a-rep-old", "Reporter", 1, 1);
        seed_chat_row(&state, "a-mgr-old");
        seed_chat_row(&state, "a-rep-old");
        state.mark_compaction_started("h-team-completed", AgentId("a-mgr-old".to_owned()));
        state.mark_compaction_started("h-team-completed", AgentId("a-rep-old".to_owned()));
        // Replacement agents land first (server emits them on the host
        // stream, then sends TeamCompactNotify on the host stream).
        register_agent_via_new_agent(
            &state,
            "h-team-completed",
            "a-mgr-new",
            "Manager (compacted)",
            2,
            2,
        );
        register_agent_via_new_agent(
            &state,
            "h-team-completed",
            "a-rep-new",
            "Reporter (compacted)",
            3,
            3,
        );

        dispatch_frame(
            &state,
            "h-team-completed",
            StreamPath("/host/h-team-completed".to_owned()),
            FrameKind::TeamCompactNotify,
            4,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Completed,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![
                    TeamMemberId("m-mgr".to_owned()),
                    TeamMemberId("m-rep".to_owned()),
                ],
                agent_ids: vec![
                    AgentId("a-mgr-old".to_owned()),
                    AgentId("a-rep-old".to_owned()),
                ],
                results: vec![
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Completed,
                        old_agent_id: AgentId("a-mgr-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: Some(AgentId("a-mgr-new".to_owned())),
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    },
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Completed,
                        old_agent_id: AgentId("a-rep-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: Some(AgentId("a-rep-new".to_owned())),
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    },
                ],
                message: None,
            },
        );

        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-mgr-old".to_owned()))),
            "team Completed must clear in-flight for a-mgr-old"
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-rep-old".to_owned()))),
            "team Completed must clear in-flight for a-rep-old"
        );
        // Each per-agent result drives the same retarget path as a
        // solo compaction. Both old→new mappings must finalize without
        // anything left behind in `compaction_pending_completion`.
        assert!(
            state.compaction_pending_completion.with(|m| m.is_empty()),
            "all per-agent retargets must finalize since both replacements are in state"
        );
    }

    /// Partial `TeamCompactNotify::Failed` — one agent succeeded, one
    /// failed. Each per-agent result must drive its own state path:
    /// the successful one retargets and clears in-flight, the failed
    /// one clears in-flight and surfaces the error message inline so
    /// the per-agent Compact button re-enables.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_failed_applies_per_agent_results() {
        let state = make_app_state("h-team-mixed");
        reset_inbound_seqs(&state, "h-team-mixed");
        register_agent_via_new_agent(&state, "h-team-mixed", "a-ok-old", "OK", 0, 0);
        register_agent_via_new_agent(&state, "h-team-mixed", "a-bad-old", "Bad", 1, 1);
        seed_chat_row(&state, "a-ok-old");
        seed_chat_row(&state, "a-bad-old");
        state.mark_compaction_started("h-team-mixed", AgentId("a-ok-old".to_owned()));
        state.mark_compaction_started("h-team-mixed", AgentId("a-bad-old".to_owned()));
        register_agent_via_new_agent(&state, "h-team-mixed", "a-ok-new", "OK (compacted)", 2, 2);

        dispatch_frame(
            &state,
            "h-team-mixed",
            StreamPath("/host/h-team-mixed".to_owned()),
            FrameKind::TeamCompactNotify,
            3,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Failed,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![
                    TeamMemberId("m-ok".to_owned()),
                    TeamMemberId("m-bad".to_owned()),
                ],
                agent_ids: vec![
                    AgentId("a-ok-old".to_owned()),
                    AgentId("a-bad-old".to_owned()),
                ],
                results: vec![
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Completed,
                        old_agent_id: AgentId("a-ok-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: Some(AgentId("a-ok-new".to_owned())),
                        new_session_id: None,
                        summary_preview: None,
                        message: None,
                    },
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: AgentId("a-bad-old".to_owned()),
                        old_session_id: None,
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("summary backend exploded".to_owned()),
                    },
                ],
                message: Some("1 of 2 team agents failed to compact".to_owned()),
            },
        );

        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-ok-old".to_owned()))),
            "successful per-agent result must clear in-flight"
        );
        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-bad-old".to_owned()))),
            "failed per-agent result must also clear in-flight (re-enable Compact button)"
        );
        let err = state
            .compaction_errors
            .with(|m| m.get(&AgentId("a-bad-old".to_owned())).cloned())
            .expect("per-agent failure must surface an error for the failed agent");
        assert!(
            err.contains("summary backend"),
            "per-agent error message must come from the result's message, got {err:?}"
        );
        assert!(
            state
                .compaction_errors
                .with(|m| !m.contains_key(&AgentId("a-ok-old".to_owned()))),
            "successful per-agent result must NOT record an error"
        );
    }

    /// Defensive: if the server's `Failed` notify lists an agent in
    /// `agent_ids` but provides no matching `results` entry (e.g. the
    /// per-agent task aborted before producing a payload), the
    /// dispatcher must still clear that agent's in-flight flag using
    /// the team-level message — otherwise the per-agent Compact button
    /// would remain disabled forever.
    #[wasm_bindgen_test]
    async fn dispatch_team_compact_notify_missing_result_falls_back_to_team_message() {
        let state = make_app_state("h-team-missing");
        reset_inbound_seqs(&state, "h-team-missing");
        register_agent_via_new_agent(&state, "h-team-missing", "a-orphan", "Orphan", 0, 0);
        seed_chat_row(&state, "a-orphan");
        state.mark_compaction_started("h-team-missing", AgentId("a-orphan".to_owned()));

        dispatch_frame(
            &state,
            "h-team-missing",
            StreamPath("/host/h-team-missing".to_owned()),
            FrameKind::TeamCompactNotify,
            1,
            &TeamCompactNotifyPayload {
                status: TeamCompactStatus::Failed,
                team_id: TeamId("t-1".to_owned()),
                member_ids: vec![TeamMemberId("m-orphan".to_owned())],
                agent_ids: vec![AgentId("a-orphan".to_owned())],
                results: Vec::new(),
                message: Some("team compaction aborted".to_owned()),
            },
        );

        assert!(
            !state
                .compaction_in_progress
                .with(|map| map.contains_key(&AgentId("a-orphan".to_owned()))),
            "missing per-agent result must still clear in-flight via team-level fallback"
        );
        let err = state
            .compaction_errors
            .with(|m| m.get(&AgentId("a-orphan".to_owned())).cloned())
            .expect("team-level message must be surfaced when no per-agent result was emitted");
        assert!(
            err.contains("team compaction aborted"),
            "fallback must use the team-level message, got {err:?}"
        );
    }
}
