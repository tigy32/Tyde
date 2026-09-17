use std::collections::HashSet;

use leptos::prelude::*;
use protocol::{ToolExecutionMode, ToolProgressUpdate, ToolRequestType, WorkflowRunStatus};

use crate::state::{ActiveAgentRef, AppState};

#[derive(Clone, PartialEq)]
struct ActivityRow {
    id: String,
    name: String,
    status: String,
    target: Option<ActiveAgentRef>,
}

#[derive(Clone, Default, PartialEq)]
struct ActivitySnapshot {
    agents: Vec<ActivityRow>,
    commands: Vec<ActivityRow>,
}

impl ActivitySnapshot {
    fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.commands.is_empty()
    }
}

fn activity_snapshot(state: &AppState) -> ActivitySnapshot {
    let Some(parent) = state.active_agent.get() else {
        return ActivitySnapshot::default();
    };
    let registry = state.agents.get();
    if !registry.iter().any(|agent| {
        agent.local_host_id == parent.local_host_id
            && agent.agent_id == parent.agent_id
            && agent.fatal_error.is_none()
    }) {
        return ActivitySnapshot::default();
    }
    let mut snapshot = ActivitySnapshot::default();
    let mut children = HashSet::new();
    for agent in &registry {
        if agent.local_host_id != parent.local_host_id
            || agent.parent_agent_id.as_ref() != Some(&parent.agent_id)
        {
            continue;
        }
        children.insert(agent.agent_id.clone());
        let agent_ref = agent.agent_ref();
        let running = state
            .agent_turn_active
            .with(|turns| turns.get(&agent_ref).copied().unwrap_or(false));
        let background = state
            .agents_with_background_work
            .with(|agents| agents.contains(&agent_ref));
        let compacting = state.context_compaction_operations.with(|operations| {
            operations.get(&agent_ref).is_some_and(|operation| {
                matches!(
                    operation.status,
                    protocol::ContextCompactionStatus::Started { .. }
                        | protocol::ContextCompactionStatus::Progress { .. }
                )
            })
        }) || state.agent_compactions.with(|operations| {
            operations
                .get(&agent_ref)
                .is_some_and(|operation| operation.status == protocol::AgentCompactStatus::Started)
        });
        if agent.fatal_error.is_some() || (agent.started && !running && !background && !compacting)
        {
            continue;
        }
        snapshot.agents.push(ActivityRow {
            id: agent.agent_id.0.clone(),
            name: agent.name.clone(),
            status: if compacting {
                "Compacting"
            } else if !agent.started {
                "Starting"
            } else if background && !running {
                "Background work"
            } else {
                "Running"
            }
            .to_owned(),
            target: Some(ActiveAgentRef {
                local_host_id: parent.local_host_id.clone(),
                agent_id: agent.agent_id.clone(),
            }),
        });
    }
    let owner = parent.as_agent_ref();
    let progress = state
        .tool_progress
        .with(|map| map.get(&owner).cloned())
        .unwrap_or_default();
    let mut progress: Vec<_> = progress.into_iter().collect();
    progress.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (call_id, progress) in progress {
        match progress.update {
            ToolProgressUpdate::SubAgent(sub)
                if !sub.completed && !children.contains(&sub.agent_id) =>
            {
                if matches!(
                    sub.status,
                    protocol::SubAgentProgressStatus::Completed
                        | protocol::SubAgentProgressStatus::Failed
                        | protocol::SubAgentProgressStatus::Stopped
                ) {
                    continue;
                }
                children.insert(sub.agent_id.clone());
                let target = registry
                    .iter()
                    .find(|agent| {
                        agent.local_host_id == parent.local_host_id
                            && agent.agent_id == sub.agent_id
                    })
                    .map(|agent| ActiveAgentRef {
                        local_host_id: agent.local_host_id.clone(),
                        agent_id: agent.agent_id.clone(),
                    });
                snapshot.agents.push(ActivityRow {
                    id: sub.agent_id.0,
                    name: sub.agent_name,
                    status: sub.last_tool_name.unwrap_or_else(|| "Running".to_owned()),
                    target,
                });
            }
            ToolProgressUpdate::Workflow(run) if run.status == WorkflowRunStatus::Running => {
                snapshot.commands.push(ActivityRow {
                    id: call_id,
                    name: run.workflow_name,
                    status: "Running workflow".to_owned(),
                    target: None,
                });
            }
            ToolProgressUpdate::Other { payload }
                if progress.execution_mode == ToolExecutionMode::Background =>
            {
                let command_name = |tool: &crate::state::ToolRequestEntry| {
                    if tool.request.tool_call_id != call_id {
                        return None;
                    }
                    Some(match &tool.request.tool_type {
                        ToolRequestType::RunCommand { command, .. } => command.clone(),
                        _ => tool.tool_name.clone(),
                    })
                };
                let name = state
                    .streaming_text
                    .with(|streams| {
                        streams.get(&owner).and_then(|stream| {
                            stream
                                .tool_requests
                                .with(|tools| tools.iter().find_map(command_name))
                        })
                    })
                    .or_else(|| {
                        state.chat_messages.with(|messages| {
                            messages.get(&owner).and_then(|messages| {
                                messages
                                    .iter()
                                    .rev()
                                    .flat_map(|message| &message.tool_requests)
                                    .find_map(command_name)
                            })
                        })
                    })
                    .or_else(|| {
                        payload
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "Background command".to_owned());
                snapshot.commands.push(ActivityRow {
                    id: call_id,
                    name,
                    status: "Running".to_owned(),
                    target: None,
                });
            }
            _ => {}
        }
    }
    snapshot.agents.sort_by(|a, b| a.id.cmp(&b.id));
    snapshot.commands.sort_by(|a, b| a.id.cmp(&b.id));
    snapshot
}

#[component]
pub fn ActivityDrawer() -> impl IntoView {
    let state = expect_context::<AppState>();
    let expanded = RwSignal::new(false);
    let toggle_ref = NodeRef::<leptos::html::Button>::new();
    let snapshot = Memo::new({
        let state = state.clone();
        move |_| activity_snapshot(&state)
    });
    let active = state.active_agent;
    Effect::new(move |_| {
        active.track();
        expanded.set(false);
    });
    Effect::new(move |_| {
        if snapshot.with(ActivitySnapshot::is_empty) {
            expanded.set(false);
        }
    });
    let open_agent = Callback::new(move |target: ActiveAgentRef| {
        expanded.set(false);
        state.active_agent.set(Some(target));
        state.viewing_chat.set(true);
    });

    view! {
        <Show when=move || !snapshot.with(ActivitySnapshot::is_empty)>
            <div class="activity-anchor" data-mobile-test="activity-anchor">
                <div
                    class="activity-drawer"
                    class:expanded=move || expanded.get()
                    data-mobile-test="activity-drawer"
                    on:keydown=move |event: web_sys::KeyboardEvent| {
                        if event.key() == "Escape" && expanded.get_untracked() {
                            event.prevent_default();
                            event.stop_propagation();
                            expanded.set(false);
                            if let Some(toggle) = toggle_ref.get() {
                                let _ = toggle.focus();
                            }
                        }
                    }
                >
                    <button
                        type="button"
                        class="activity-toggle"
                        data-mobile-test="activity-toggle"
                        node_ref=toggle_ref
                        aria-expanded=move || expanded.get().to_string()
                        aria-controls="mobile-background-activity"
                        aria-label=move || if expanded.get() { "Collapse background activity" } else { "Expand background activity" }
                        on:click=move |_| expanded.update(|open| *open = !*open)
                    >
                        <span>{move || snapshot.with(|snapshot| {
                            let n = snapshot.agents.len();
                            format!("{n} agent{}", if n == 1 { "" } else { "s" })
                        })}</span>
                        <svg viewBox="0 0 16 16" aria-hidden="true" class="activity-chevron">
                            <path d="M3 10L8 5L13 10" />
                        </svg>
                        <span>{move || snapshot.with(|snapshot| {
                            let n = snapshot.commands.len();
                            format!("{n} command{}", if n == 1 { "" } else { "s" })
                        })}</span>
                    </button>
                    <div
                        class="activity-reveal"
                        id="mobile-background-activity"
                        role="region"
                        aria-label="Background activity"
                        aria-hidden=move || (!expanded.get()).to_string()
                        inert=move || !expanded.get()
                    >
                        <div class="activity-clip">
                            <div class="activity-list" data-mobile-test="activity-list">
                                <Show when=move || snapshot.with(|snapshot| !snapshot.agents.is_empty())>
                                    <h3>"Agents"</h3>
                                    {move || snapshot.get().agents.into_iter().map(|row| {
                                        let label = format!("Open chat with {}", row.name);
                                        view! {
                                            <div class="activity-row" data-mobile-test="activity-agent">
                                                <span class="activity-dot" aria-hidden="true"></span>
                                                <div class="activity-label">
                                                    <div class="activity-name">{row.name}</div>
                                                    <div class="activity-status">{row.status}</div>
                                                </div>
                                                {row.target.map(|target| view! {
                                                    <button
                                                        type="button"
                                                        class="activity-open"
                                                        data-mobile-test="activity-open"
                                                        aria-label=label
                                                        on:click=move |_| open_agent.run(target.clone())
                                                    >"Open "<span aria-hidden="true">"›"</span></button>
                                                })}
                                            </div>
                                        }
                                    }).collect_view()}
                                </Show>
                                <Show when=move || snapshot.with(|snapshot| !snapshot.commands.is_empty())>
                                    <h3>"Commands"</h3>
                                    {move || snapshot.get().commands.into_iter().map(|row| view! {
                                        <div class="activity-row" data-mobile-test="activity-command">
                                            <span class="activity-terminal" aria-hidden="true">">_"</span>
                                            <div class="activity-label">
                                                <div class="activity-name">{row.name}</div>
                                                <div class="activity-status">{row.status}</div>
                                            </div>
                                        </div>
                                    }).collect_view()}
                                </Show>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}
