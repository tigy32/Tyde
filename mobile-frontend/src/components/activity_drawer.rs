use std::collections::HashSet;

use leptos::prelude::*;
use protocol::{
    BackendKind, ToolExecutionMode, ToolProgressUpdate, ToolRequestType, WorkflowRunStatus,
};

use crate::components::chat_input::backend_label;

use crate::state::{ActiveAgentRef, AppState};

#[derive(Clone, PartialEq)]
struct ActivityRow {
    id: String,
    name: String,
    status: String,
    backend_kind: Option<BackendKind>,
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
            backend_kind: Some(agent.backend_kind),
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
                let agent = registry.iter().find(|agent| {
                    agent.local_host_id == parent.local_host_id && agent.agent_id == sub.agent_id
                });
                snapshot.agents.push(ActivityRow {
                    id: sub.agent_id.0,
                    name: sub.agent_name,
                    status: sub.last_tool_name.unwrap_or_else(|| "Running".to_owned()),
                    backend_kind: sub.backend_kind,
                    target: agent.map(|agent| ActiveAgentRef {
                        local_host_id: agent.local_host_id.clone(),
                        agent_id: agent.agent_id.clone(),
                    }),
                });
            }
            ToolProgressUpdate::Workflow(run) if run.status == WorkflowRunStatus::Running => {
                snapshot.commands.push(ActivityRow {
                    id: call_id,
                    name: run.workflow_name,
                    status: "Running workflow".to_owned(),
                    backend_kind: None,
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
                    backend_kind: None,
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
    let has_work = move || !snapshot.with(ActivitySnapshot::is_empty);
    let pending_questions: Signal<Vec<protocol::ToolRequest>> = Signal::derive({
        let state = state.clone();
        move || {
            let Some(parent) = state.active_agent.get() else {
                return Vec::new();
            };
            let parent_is_fatal = state.agents.with(|agents| {
                agents.iter().any(|agent| {
                    agent.local_host_id == parent.local_host_id
                        && agent.agent_id == parent.agent_id
                        && agent.fatal_error.is_some()
                })
            });
            if parent_is_fatal {
                return Vec::new();
            }
            let owner = parent.as_agent_ref();
            state
                .pending_user_questions
                .with(|map| map.get(&owner).cloned().unwrap_or_default())
        }
    });
    let has_questions = move || !pending_questions.with(|questions| questions.is_empty());
    let question_open = RwSignal::new(false);
    let active_question_call_id = RwSignal::new(None::<String>);
    let seen_question_ids = RwSignal::new((None::<ActiveAgentRef>, Vec::<String>::new()));

    Effect::new(move |_| {
        active.track();
        expanded.set(false);
    });
    Effect::new(move |_| {
        if !has_work() {
            expanded.set(false);
        }
    });
    Effect::new(move |_| {
        let owner = active.get();
        let questions = pending_questions.get();
        let ids: Vec<String> = questions
            .iter()
            .map(|question| question.tool_call_id.clone())
            .collect();
        if ids.is_empty() {
            question_open.set(false);
            active_question_call_id.set(None);
            seen_question_ids.set((owner, Vec::new()));
            return;
        }
        let (prev_owner, prev_ids) = seen_question_ids.get_untracked();
        let owner_changed = prev_owner != owner;
        let has_new = owner_changed || ids.iter().any(|id| !prev_ids.contains(id));
        if has_new {
            if !question_open.get_untracked() {
                expanded.set(false);
            }
            question_open.set(true);
        }
        seen_question_ids.set((owner, ids.clone()));
        let current = active_question_call_id.get_untracked();
        if current.as_ref().is_none_or(|id| !ids.contains(id)) {
            active_question_call_id.set(ids.first().cloned());
        }
    });
    let collapse_question = Callback::new(move |()| {
        question_open.set(false);
        expanded.set(false);
        if let Some(toggle) = toggle_ref.get_untracked() {
            let _ = toggle.focus();
        }
    });
    let open_agent = Callback::new(move |target: ActiveAgentRef| {
        expanded.set(false);
        state.open_agent_chat(target);
    });

    view! {
        <Show when=move || has_work() || has_questions()>
            <div class="activity-anchor" data-mobile-test="activity-anchor">
                <div
                    class="activity-drawer"
                    class:expanded=move || has_work() && expanded.get() && (!has_questions() || question_open.get())
                    class:question-open=move || has_questions() && question_open.get()
                    data-mobile-test="activity-drawer"
                    on:keydown=move |event: web_sys::KeyboardEvent| {
                        if event.key() == "Escape"
                            && (expanded.get_untracked()
                                || (has_questions() && question_open.get_untracked()))
                        {
                            event.prevent_default();
                            event.stop_propagation();
                            if expanded.get_untracked() {
                                expanded.set(false);
                            } else {
                                question_open.set(false);
                            }
                            if let Some(toggle) = toggle_ref.get() {
                                let _ = toggle.focus();
                            }
                        }
                    }
                >
                    <Show when=move || has_questions() && question_open.get()>
                        <MobilePinnedQuestionSection
                            active_agent=active
                            pending_questions=pending_questions
                            active_question_call_id=active_question_call_id
                            on_collapse=collapse_question
                        />
                    </Show>
                    <Show when=move || has_work() || !question_open.get()>
                        <button
                            type="button"
                            class="activity-toggle"
                            data-mobile-test="activity-toggle"
                            node_ref=toggle_ref
                            aria-expanded=move || {
                                (if has_questions() && !question_open.get() {
                                    false
                                } else {
                                    expanded.get()
                                })
                                .to_string()
                            }
                            aria-controls="mobile-background-activity"
                            aria-label=move || {
                                if expanded.get() {
                                    "Collapse background activity"
                                } else {
                                    "Expand background activity"
                                }
                            }
                            on:click=move |_| {
                                if has_questions() && !question_open.get_untracked() {
                                    question_open.set(true);
                                    expanded.set(false);
                                } else if has_work() {
                                    expanded.update(|open| *open = !*open);
                                } else {
                                    question_open.set(false);
                                }
                            }
                        >
                            {move || {
                                let q = pending_questions.with(|questions| questions.len());
                                (q > 0 && !question_open.get()).then(|| view! {
                                    <span>{format!("{q} question{}", if q == 1 { "" } else { "s" })}</span>
                                })
                            }}
                            <Show when=move || has_work()>
                                <span>{move || snapshot.with(|snapshot| {
                                    let n = snapshot.agents.len();
                                    format!("{n} agent{}", if n == 1 { "" } else { "s" })
                                })}</span>
                            </Show>
                            <Show when=move || has_work()><span aria-hidden="true">"·"</span></Show>
                            <Show when=move || has_work()>
                                <span>{move || snapshot.with(|snapshot| {
                                    let n = snapshot.commands.len();
                                    format!("{n} command{}", if n == 1 { "" } else { "s" })
                                })}</span>
                            </Show>
                            <svg viewBox="0 0 16 16" aria-hidden="true" class="activity-chevron">
                                <path d="M3 10L8 5L13 10" />
                            </svg>
                        </button>
                    </Show>
                    <div
                        class="activity-reveal"
                        id="mobile-background-activity"
                        role="region"
                        aria-label="Background activity"
                        aria-hidden=move || {
                            (!(has_work() && expanded.get() && (!has_questions() || question_open.get()))).to_string()
                        }
                        inert=move || !(has_work() && expanded.get() && (!has_questions() || question_open.get()))
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
                                                    <div class="activity-status">
                                                        {row.status}
                                                        <span class="activity-backend">{
                                                            row.backend_kind.map(backend_label).unwrap_or("Backend unavailable")
                                                        }</span>
                                                    </div>
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

#[component]
fn MobilePinnedQuestionSection(
    active_agent: RwSignal<Option<ActiveAgentRef>>,
    pending_questions: Signal<Vec<protocol::ToolRequest>>,
    active_question_call_id: RwSignal<Option<String>>,
    on_collapse: Callback<()>,
) -> impl IntoView {
    let total = Signal::derive(move || pending_questions.with(|questions| questions.len()));
    let active_index = Signal::derive(move || {
        let current = active_question_call_id.get();
        pending_questions.with(|questions| {
            current
                .as_deref()
                .and_then(|id| questions.iter().position(|q| q.tool_call_id == id))
                .unwrap_or(0)
        })
    });
    let active_request = Signal::derive(move || {
        let idx = active_index.get();
        pending_questions.with(|questions| questions.get(idx).cloned())
    });

    view! {
        <section
            class="pinned-question-section"
            data-mobile-test="pinned-question-section"
            aria-label="Pending question"
        >
            <div class="pinned-question-bar" data-mobile-test="pinned-question-bar">
                <span class="pinned-question-dot" aria-hidden="true"></span>
                <span class="pinned-question-title" data-mobile-test="pinned-question-title">
                    {move || {
                        let count = total.get();
                        format!("{count} question{}", if count == 1 { "" } else { "s" })
                    }}
                </span>
                <Show when={move || total.get() > 1}>
                    <div class="pinned-question-pager" data-mobile-test="pinned-question-pager">
                        <button
                            type="button"
                            class="pinned-question-nav"
                            data-mobile-test="pinned-question-prev"
                            aria-label="Previous question"
                            disabled={move || active_index.get() == 0}
                            on:click=move |ev| {
                                ev.stop_propagation();
                                let idx = active_index.get_untracked();
                                if idx > 0
                                    && let Some(prev_id) =
                                        pending_questions.with_untracked(|questions| {
                                            questions.get(idx - 1).map(|q| q.tool_call_id.clone())
                                        })
                                {
                                    active_question_call_id.set(Some(prev_id));
                                }
                            }
                        >
                            "\u{2039}"
                        </button>
                        <span
                            class="pinned-question-position"
                            data-mobile-test="pinned-question-position"
                        >
                            {move || format!("{} of {}", active_index.get() + 1, total.get())}
                        </span>
                        <button
                            type="button"
                            class="pinned-question-nav"
                            data-mobile-test="pinned-question-next"
                            aria-label="Next question"
                            disabled={move || active_index.get() + 1 >= total.get()}
                            on:click=move |ev| {
                                ev.stop_propagation();
                                let idx = active_index.get_untracked();
                                if let Some(next_id) =
                                    pending_questions.with_untracked(|questions| {
                                        questions.get(idx + 1).map(|q| q.tool_call_id.clone())
                                    })
                                {
                                    active_question_call_id.set(Some(next_id));
                                }
                            }
                        >
                            "\u{203a}"
                        </button>
                    </div>
                </Show>
                <button
                    type="button"
                    class="pinned-question-toggle"
                    data-mobile-test="pinned-question-toggle"
                    aria-label="Collapse question"
                    aria-expanded="true"
                    on:click=move |ev| {
                        ev.stop_propagation();
                        on_collapse.run(());
                    }
                >
                    <svg class="pinned-question-chevron" viewBox="0 0 16 16" aria-hidden="true">
                        <path d="M3 6L8 11L13 6" />
                    </svg>
                </button>
            </div>
            <div class="pinned-question-body" data-mobile-test="pinned-question-body">
                {move || {
                    let parent = active_agent.get()?;
                    let request = active_request.get()?;
                    let ToolRequestType::AskUserQuestion { questions, .. } = request.tool_type else {
                        return None;
                    };
                    Some(view! {
                        <crate::components::tool_card::AskUserQuestionCard
                            owner_agent_ref=parent.as_agent_ref()
                            tool_call_id=request.tool_call_id
                            questions=questions
                            answered=false
                            answers=Vec::new()
                        />
                    })
                }}
            </div>
        </section>
    }
}
