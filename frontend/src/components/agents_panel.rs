use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{AgentId, FrameKind, SetAgentNamePayload};

use crate::send::{close_agent, send_frame};
use crate::state::{ActiveAgentRef, AgentInfo, AppState, TabContent};

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
    let hide_inactive = RwSignal::new(false);
    let hide_sub_agents = RwSignal::new(false);
    // Per-parent collapse state: parents whose children are hidden.
    let collapsed_parents: RwSignal<HashSet<AgentId>> = RwSignal::new(HashSet::new());
    // Editing state lives here so it survives agent list re-renders caused by
    // streaming / turn-active updates. Only one agent can be renamed at a time.
    let editing_agent: RwSignal<Option<protocol::AgentId>> = RwSignal::new(None);
    let edit_value: RwSignal<String> = RwSignal::new(String::new());

    let filtered_agents = Memo::new(move |_| {
        let agents = state.agents.get();
        let streaming_map = state.streaming_text.get();
        let turn_active_map = state.agent_turn_active.get();
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
                    let is_active = !a.started
                        || streaming_map.contains_key(&a.agent_id)
                        || turn_active_map.get(&a.agent_id).copied().unwrap_or(false);
                    if !is_active {
                        return false;
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
        state_for_click.active_agent.set(Some(agent_ref.clone()));
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
            kd_state.active_agent.set(Some(agent_ref.clone()));
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
        if editing_agent.with(|e| e.as_ref() == Some(&agent_id_for_effect)) {
            if let Some(el) = input_ref.get() {
                let _ = el.focus();
                el.select();
            }
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
                            />
                        }.into_any()
                    } else {
                        view! {
                            <span class="agent-card-name">{name.clone()}</span>
                        }.into_any()
                    }
                }}
                <div>
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
                            <span class="agent-child-count">{child_count}</span>
                            <button
                                type="button"
                                class="filter-toggle"
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
                        }
                    })}
                    <button type="button" class="filter-toggle" on:click=on_rename>
                        "Rename"
                    </button>
                    <button
                        type="button"
                        class="filter-toggle agent-card-close"
                        title="Close agent"
                        on:click=on_close
                        on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                    >
                        "\u{00D7}"
                    </button>
                    <span class={backend_class(backend)}>{backend_label(backend)}</span>
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
            </div>
            {error_msg.map(|msg| view! {
                <div class="agent-card-error">{msg}</div>
            })}
        </div>
    }
}
