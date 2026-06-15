//! Tool-call rendering.
//!
//! `ToolCardView` is the per-tool card mounted by the chat row. Body rendering
//! is dispatched by an exhaustive `match` on `ToolRequestType`, with one module
//! per variant. The `ToolOutputMode` signal (`Summary` / `Compact` / `Full`) is
//! a frontend-only viewing preference; it controls how much of the tool's
//! output the body shows. Each renderer decides what `Summary` means for its
//! variant (usually empty), what `Compact` shows under per-tool caps, and what
//! `Full` lays out without truncation.
//!
//! Errors are special-cased in the shell: any completed tool whose result is
//! `ToolExecutionResult::Error` routes through `error_result::render`, no
//! matter the request kind.

use std::sync::Arc;

use leptos::prelude::*;
use protocol::{
    AgentControlAgentRef, AgentControlProgress, AgentControlProgressKind, SubAgentProgress,
    ToolExecutionResult, ToolProgressData, ToolProgressUpdate, ToolRequestType, WorkflowRunState,
    WorkflowRunStatus,
};
use wasm_bindgen::JsCast;

use crate::components::workflow_view::{WorkflowRunPanel, run_status_label};
use crate::state::{
    ActiveAgentRef, AppState, StreamingToolRequest, TabContent, ToolCallId, ToolOutputMode,
    ToolRequestEntry,
};

mod ask_user_question;
mod error_result;
mod exit_plan_mode;
mod get_type_docs;
mod modify_file;
mod other;
mod read_files;
mod run_command;
mod search_types;

const TOOL_LIST_INLINE_LIMIT: usize = 80;
const TOOL_LIST_HEAD_COUNT: usize = 8;
const TOOL_LIST_TAIL_COUNT: usize = 32;

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod test_utils;

#[component]
pub fn ToolCardListView(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    entries: Vec<ToolRequestEntry>,
) -> impl IntoView {
    let entries = Arc::new(entries);
    let expanded = RwSignal::new(false);
    let total = entries.len();

    view! {
        <div class="chat-card-tools">
            <For
                each={
                    let entries = entries.clone();
                    move || {
                        let expanded = expanded.get();
                        visible_tool_indexes(entries.len(), expanded, |idx| {
                            is_important_tool(&entries[idx])
                        })
                        .into_iter()
                        .map(|idx| entries[idx].clone())
                        .collect::<Vec<_>>()
                    }
                }
                key=|entry| entry.request.tool_call_id.clone()
                let:entry
            >
                <ToolCardView agent_ref=agent_ref entry=entry />
            </For>
            <ToolListSummary
                total=move || total
                hidden_count={
                    let entries = entries.clone();
                    move || {
                        let visible = visible_tool_indexes(entries.len(), expanded.get(), |idx| {
                            is_important_tool(&entries[idx])
                        })
                        .len();
                        entries.len().saturating_sub(visible)
                    }
                }
                expanded=expanded
            />
        </div>
    }
}

#[component]
pub fn StreamingToolCardListView(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    entries: ArcRwSignal<Vec<StreamingToolRequest>>,
) -> impl IntoView {
    let expanded = RwSignal::new(false);

    view! {
        <div class="chat-card-tools">
            <For
                each={
                    let entries = entries.clone();
                    move || {
                        let expanded = expanded.get();
                        entries.with(|tools| {
                            visible_tool_indexes(tools.len(), expanded, |idx| {
                                tools[idx].entry.with_untracked(is_important_tool)
                            })
                            .into_iter()
                            .map(|idx| tools[idx].clone())
                            .collect::<Vec<_>>()
                        })
                    }
                }
                key=|tool| tool.tool_call_id.clone()
                let:tool
            >
                <StreamingToolCardView agent_ref=agent_ref entry=tool.entry />
            </For>
            <ToolListSummary
                total={
                    let entries = entries.clone();
                    move || entries.with(|tools| tools.len())
                }
                hidden_count={
                    let entries = entries.clone();
                    move || {
                        entries.with(|tools| {
                            let visible = visible_tool_indexes(tools.len(), expanded.get(), |idx| {
                                tools[idx].entry.with_untracked(is_important_tool)
                            })
                            .len();
                            tools.len().saturating_sub(visible)
                        })
                    }
                }
                expanded=expanded
            />
        </div>
    }
}

#[component]
fn StreamingToolCardView(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    entry: ArcRwSignal<ToolRequestEntry>,
) -> impl IntoView {
    view! {
        {move || view! { <ToolCardView agent_ref=agent_ref entry=entry.get() /> }}
    }
}

#[component]
fn ToolListSummary(
    total: impl Fn() -> usize + Send + Sync + 'static,
    hidden_count: impl Fn() -> usize + Send + Sync + 'static,
    expanded: RwSignal<bool>,
) -> impl IntoView {
    view! {
        {move || {
            let total = total();
            if total <= TOOL_LIST_INLINE_LIMIT {
                None
            } else {
                let is_expanded = expanded.get();
                let label = if is_expanded {
                    format!("{total} tools")
                } else {
                    format!("{} tools hidden", hidden_count())
                };
                let button_label = if is_expanded { "Show fewer" } else { "Show all" };
                Some(view! {
                    <div class="tool-list-summary">
                        <span class="tool-list-hidden-count">{label}</span>
                        <button
                            type="button"
                            class="tool-list-expand"
                            on:click=move |_| expanded.update(|value| *value = !*value)
                        >
                            {button_label}
                        </button>
                    </div>
                })
            }
        }}
    }
}

fn visible_tool_indexes<F>(len: usize, expanded: bool, mut is_important: F) -> Vec<usize>
where
    F: FnMut(usize) -> bool,
{
    if expanded || len <= TOOL_LIST_INLINE_LIMIT {
        return (0..len).collect();
    }

    let mut visible = vec![false; len];
    for keep in visible.iter_mut().take(TOOL_LIST_HEAD_COUNT) {
        *keep = true;
    }
    for keep in visible
        .iter_mut()
        .skip(len.saturating_sub(TOOL_LIST_TAIL_COUNT))
    {
        *keep = true;
    }
    for (idx, keep) in visible.iter_mut().enumerate() {
        if is_important(idx) {
            *keep = true;
        }
    }

    visible
        .into_iter()
        .enumerate()
        .filter_map(|(idx, keep)| keep.then_some(idx))
        .collect()
}

fn is_important_tool(entry: &ToolRequestEntry) -> bool {
    // Approval-gated tools (questions, plan approval) stay visible even in
    // collapsed long lists so a successful decision remains discoverable.
    matches!(
        &entry.request.tool_type,
        ToolRequestType::AskUserQuestion { .. } | ToolRequestType::ExitPlanMode { .. }
    ) || entry.result.as_ref().is_none_or(|result| !result.success)
}

#[component]
pub fn ToolCardView(
    /// The chat's agent, plumbed explicitly from the owning view (`None`
    /// only for draft tabs whose agent hasn't spawned yet — no tool
    /// cards exist there). Never inferred from the active tab.
    agent_ref: Signal<Option<ActiveAgentRef>>,
    entry: ToolRequestEntry,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tool_output_mode = state.tool_output_mode;

    let tool_name = entry.request.tool_name.clone();
    let tool_call_id = entry.request.tool_call_id.clone();
    let tool_type = entry.request.tool_type;
    let result = entry.result;

    // Live progress is read reactively from the central store — never
    // from the entry — so the card keeps updating inside keyed `<For>`
    // rows and after the turn ends, without remounting (a remount would
    // also discard the user's collapse state on every snapshot).
    let progress: Signal<Option<ToolProgressData>> = Signal::derive({
        let state = state.clone();
        let tool_call_id = tool_call_id.clone();
        move || {
            let agent_id = agent_ref.get()?.agent_id;
            let key = (agent_id, ToolCallId(tool_call_id.clone()));
            state
                .tool_progress
                .with(|map| map.get(&key).cloned())
                .map(|signal| signal.get())
        }
    });
    let workflow_run: Signal<Option<WorkflowRunState>> =
        Signal::derive(move || match progress.get().map(|data| data.update) {
            Some(ToolProgressUpdate::Workflow(run)) => Some(run),
            _ => None,
        });
    let subagent_progress: Signal<Option<SubAgentProgress>> =
        Signal::derive(move || match progress.get().map(|data| data.update) {
            Some(ToolProgressUpdate::SubAgent(progress)) => Some(progress),
            _ => None,
        });
    let agent_control_progress: Signal<Option<AgentControlProgress>> =
        Signal::derive(move || match progress.get().map(|data| data.update) {
            Some(ToolProgressUpdate::AgentControl(progress)) => Some(progress),
            _ => None,
        });
    // A background task can outlive its tool call: the Workflow tool
    // result is just the run id, the real work keeps going.
    let background_running = Signal::derive({
        let state = state.clone();
        move || {
            workflow_run
                .get()
                .is_some_and(|run| run.status == WorkflowRunStatus::Running)
                || subagent_progress
                    .get()
                    .is_some_and(|progress| !progress.completed)
                || agent_control_progress.get().is_some_and(|progress| {
                    agent_control_progress_has_active_agents(&state, agent_ref.get(), &progress)
                })
        }
    });
    // The body shape (workflow panel vs regular renderer) flips at most
    // once, when the first snapshot arrives — memoized so per-snapshot
    // updates don't recreate the body, only its inner reactive text.
    let is_workflow = Memo::new(move |_| workflow_run.with(|run| run.is_some()));

    let has_result = result.is_some();
    let result_success = result.as_ref().map(|r| r.success).unwrap_or(false);
    let result_failed = has_result && !result_success;

    let status_class = move || {
        if !has_result || (background_running.get() && !result_failed) {
            "tool-status-text pending"
        } else if result_success {
            "tool-status-text success"
        } else {
            "tool-status-text failure"
        }
    };

    let status_label = move || {
        if !has_result || (background_running.get() && !result_failed) {
            "Running\u{2026}".to_owned()
        } else if result_success {
            "Done".to_owned()
        } else {
            "Failed".to_owned()
        }
    };

    let (icon, header_detail) = tool_icon_and_detail(&tool_name, &tool_type);
    let header_detail = move || {
        workflow_run
            .get()
            .map(|run| run.workflow_name)
            .or_else(|| {
                agent_control_progress
                    .get()
                    .map(|progress| agent_control_header_detail(&progress))
            })
            .or_else(|| header_detail.clone())
    };

    let completion_summary = result
        .as_ref()
        .map(|r| completion_header_summary(&tool_type, &r.tool_result));

    let is_ask_user_question = matches!(tool_type, ToolRequestType::AskUserQuestion { .. });
    let body_tool_type = tool_type.clone();
    let body_result = result.as_ref().map(|r| r.tool_result.clone());
    let body_tool_type_slot = StoredValue::new_local(body_tool_type);
    let body_result_slot = StoredValue::new_local(body_result);
    let tool_call_id_slot = StoredValue::new_local(tool_call_id);
    let details_open = RwSignal::new(
        is_ask_user_question
            || !has_result
            || !result_success
            || background_running.get_untracked(),
    );
    let default_open_for_body = move || {
        is_ask_user_question
            || !has_result
            || !result_success
            || background_running.get()
            || tool_output_mode.get() != ToolOutputMode::Summary
    };
    let default_open_for_prop = move || {
        is_ask_user_question
            || !has_result
            || !result_success
            || background_running.get()
            || tool_output_mode.get() != ToolOutputMode::Summary
    };
    let render_body_when = move || default_open_for_body() || details_open.get();

    view! {
        <details
            class="tool-card"
            prop:open=default_open_for_prop
            on:toggle=move |ev: leptos::ev::Event| {
                if let Some(target) = ev.target()
                    && let Ok(el) = target.dyn_into::<web_sys::HtmlDetailsElement>()
                {
                    details_open.set(el.open());
                }
            }
        >
            <summary class="tool-card-header">
                <span class="tool-card-icon">{icon}</span>
                <span class="tool-card-name">{tool_name}</span>
                {move || header_detail().map(|d| view! {
                    <span class="tool-card-detail">{d}</span>
                })}
                {completion_summary.map(|s| view! {
                    <span class="tool-completion-summary">{s}</span>
                })}
                <span class=status_class>{status_label}</span>
                <span class="tool-chevron">"\u{25b6}"</span>
            </summary>
            <Show when=render_body_when>
                <div class="tool-card-body">
                    {move || {
                        // A workflow card's body IS the live run view —
                        // the generic args/result JSON adds nothing.
                        if is_workflow.get() {
                            let tool_call_id = tool_call_id_slot.with_value(Clone::clone);
                            return workflow_card_body(agent_ref, workflow_run, tool_call_id)
                                .into_any();
                        }
                        view! {
                            <div>
                                {move || {
                                    agent_control_progress.get().map(|progress| {
                                        agent_control_status_list(agent_ref, progress)
                                    })
                                }}
                                {move || {
                                    subagent_progress.get().map(|progress| {
                                        subagent_status_line(agent_ref, progress)
                                    })
                                }}
                                {move || {
                                    let mode = tool_output_mode.get();
                                    tool_call_id_slot.with_value(|tool_call_id| {
                                        body_tool_type_slot.with_value(|body_tool_type| {
                                            body_result_slot.with_value(|body_result| {
                                                render_body(
                                                    tool_call_id,
                                                    body_tool_type,
                                                    body_result.as_ref(),
                                                    mode,
                                                    result_failed,
                                                )
                                            })
                                        })
                                    })
                                }}
                            </div>
                        }
                        .into_any()
                    }}
                </div>
            </Show>
        </details>
    }
}

/// Dispatch table from request kind → renderer module. The compiler enforces
/// exhaustiveness here: adding a new `ToolRequestType` variant fails the build
/// until a renderer is wired in.
///
/// Errors short-circuit the dispatch — any completed tool whose result is
/// `Error` renders via `error_result`, regardless of which request kind issued
/// it.
fn render_body(
    tool_call_id: &str,
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
    result_failed: bool,
) -> AnyView {
    if let Some(ToolExecutionResult::Error { .. }) = result {
        return error_result::render(result.unwrap(), mode).into_any();
    }

    match req {
        ToolRequestType::ModifyFile { .. } => modify_file::render(req, result, mode).into_any(),
        ToolRequestType::RunCommand { .. } => run_command::render(req, result, mode).into_any(),
        ToolRequestType::ReadFiles { .. } => read_files::render(req, result, mode).into_any(),
        ToolRequestType::SearchTypes { .. } => search_types::render(req, result, mode).into_any(),
        ToolRequestType::GetTypeDocs { .. } => get_type_docs::render(req, result, mode).into_any(),
        // A failed completion for a question is no longer answerable: render the
        // raw result instead of the interactive card, mirroring the mobile tool
        // card. The realistic failure carries `ToolExecutionResult::Error`, which
        // the shell short-circuits above; this arm covers a non-`Error` result
        // that still reports `success=false`.
        ToolRequestType::AskUserQuestion { .. } if result_failed => {
            failed_result_body(result).into_any()
        }
        ToolRequestType::AskUserQuestion { .. } => {
            ask_user_question::render(req, result, mode).into_any()
        }
        ToolRequestType::ExitPlanMode { .. } => {
            exit_plan_mode::render(tool_call_id, req, result, mode).into_any()
        }
        ToolRequestType::Other { .. } => other::render(req, result, mode).into_any(),
    }
}

/// Body for a Workflow tool card: the live run view (phase-grouped agent
/// rows + aggregate footer) and a link to the dedicated workflow tab.
fn workflow_card_body(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    run: Signal<Option<WorkflowRunState>>,
    tool_call_id: String,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let header = move || {
        run.get().map(|run| {
            format!(
                "{} \u{b7} {}",
                run.workflow_name,
                run_status_label(run.status)
            )
        })
    };

    let on_open = move |_: web_sys::MouseEvent| {
        let Some(agent_ref) = agent_ref.get_untracked() else {
            log::error!("Open workflow clicked on a card with no resolved agent");
            return;
        };
        let Some(run) = run.get_untracked() else {
            log::error!("Open workflow clicked before any run snapshot");
            return;
        };
        state.open_tab(
            TabContent::Workflow {
                agent_ref,
                tool_call_id: ToolCallId(tool_call_id.clone()),
            },
            format!("Workflow: {}", run.workflow_name),
            true,
        );
    };

    view! {
        <div class="tool-live-workflow">
            <div class="tool-live-header">
                <span class="tool-live-title">{header}</span>
                <button class="tool-live-link" on:click=on_open>"Open workflow"</button>
            </div>
            <WorkflowRunPanel run=run />
        </div>
    }
}

fn agent_control_header_detail(progress: &AgentControlProgress) -> String {
    match (progress.progress_kind, progress.agents.len()) {
        (AgentControlProgressKind::Spawn, 1) => "Spawned agent".to_owned(),
        (AgentControlProgressKind::Spawn, count) => format!("Spawned {count} agents"),
        (AgentControlProgressKind::Await, 1) => "Awaiting agent".to_owned(),
        (AgentControlProgressKind::Await, count) => format!("Awaiting {count} agents"),
    }
}

fn agent_control_progress_has_active_agents(
    state: &AppState,
    parent_ref: Option<ActiveAgentRef>,
    progress: &AgentControlProgress,
) -> bool {
    let Some(parent_ref) = parent_ref else {
        return false;
    };
    if progress.progress_kind == AgentControlProgressKind::Await {
        return false;
    }
    progress.agents.iter().any(|agent| {
        agent_control_agent_is_active(
            state,
            &parent_ref.host_id,
            &agent.agent_id,
            progress.progress_kind,
        )
    })
}

fn agent_control_agent_is_active(
    state: &AppState,
    host_id: &str,
    agent_id: &protocol::AgentId,
    progress_kind: AgentControlProgressKind,
) -> bool {
    let agent = state.agents.with(|agents| {
        agents
            .iter()
            .find(|agent| agent.host_id == host_id && agent.agent_id == *agent_id)
            .cloned()
    });
    match agent {
        Some(agent) if agent.fatal_error.is_some() => false,
        Some(agent) if !agent.started => true,
        Some(_) => {
            let typing = state
                .agent_turn_active
                .with(|map| map.get(agent_id).copied().unwrap_or(false));
            let streaming = state.streaming_text.with(|map| map.contains_key(agent_id));
            typing || streaming
        }
        None => matches!(progress_kind, AgentControlProgressKind::Spawn),
    }
}

fn agent_control_status_list(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    progress: AgentControlProgress,
) -> impl IntoView {
    let progress_kind = progress.progress_kind;
    let agents = progress.agents;
    let title = match progress_kind {
        AgentControlProgressKind::Spawn => "Spawned agents",
        AgentControlProgressKind::Await => "Awaiting agents",
    };

    view! {
        <div class="tool-live-agent-control">
            <div class="tool-live-agent-control-title">{title}</div>
            <For
                each=move || agents.clone()
                key=|agent| agent.agent_id.0.clone()
                let:agent
            >
                <AgentControlAgentRow
                    parent_ref=parent_ref
                    progress_kind=progress_kind
                    agent=agent
                />
            </For>
        </div>
    }
}

#[derive(Clone)]
enum AgentControlDerivedStatus {
    Starting,
    Running,
    Idle,
    Failed(String),
    Unknown,
}

impl AgentControlDerivedStatus {
    fn label(&self) -> String {
        match self {
            Self::Starting => "Starting".to_owned(),
            Self::Running => "Running".to_owned(),
            Self::Idle => "Idle".to_owned(),
            Self::Failed(message) if message.trim().is_empty() => "Failed".to_owned(),
            Self::Failed(message) => format!("Failed: {}", truncate_inline(message, 72)),
            Self::Unknown => "Unknown".to_owned(),
        }
    }

    fn class(&self) -> &'static str {
        match self {
            Self::Starting | Self::Running => "tool-live-agent-status running",
            Self::Idle => "tool-live-agent-status idle",
            Self::Failed(_) => "tool-live-agent-status failed",
            Self::Unknown => "tool-live-agent-status unknown",
        }
    }
}

#[component]
fn AgentControlAgentRow(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    progress_kind: AgentControlProgressKind,
    agent: AgentControlAgentRef,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let agent_id = agent.agent_id;
    let fallback_name = agent.name;

    let display_name = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        let fallback_name = fallback_name.clone();
        move || {
            let state_name = parent_ref.get().and_then(|parent| {
                state.agents.with(|agents| {
                    agents
                        .iter()
                        .find(|agent| agent.host_id == parent.host_id && agent.agent_id == agent_id)
                        .map(|agent| agent.name.clone())
                })
            });
            state_name
                .or_else(|| fallback_name.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| agent_id.0.clone())
        }
    });

    let derived_status = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || {
            let Some(parent) = parent_ref.get() else {
                return AgentControlDerivedStatus::Unknown;
            };
            let agent = state.agents.with(|agents| {
                agents
                    .iter()
                    .find(|agent| agent.host_id == parent.host_id && agent.agent_id == agent_id)
                    .cloned()
            });
            match agent {
                Some(agent) if agent.fatal_error.is_some() => {
                    AgentControlDerivedStatus::Failed(agent.fatal_error.unwrap_or_default())
                }
                Some(agent) if !agent.started => AgentControlDerivedStatus::Starting,
                Some(_) => {
                    let typing = state
                        .agent_turn_active
                        .with(|map| map.get(&agent_id).copied().unwrap_or(false));
                    let streaming = state.streaming_text.with(|map| map.contains_key(&agent_id));
                    if typing || streaming {
                        AgentControlDerivedStatus::Running
                    } else {
                        AgentControlDerivedStatus::Idle
                    }
                }
                None if progress_kind == AgentControlProgressKind::Spawn => {
                    AgentControlDerivedStatus::Starting
                }
                None => AgentControlDerivedStatus::Unknown,
            }
        }
    });

    let preview = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || {
            let handles = state.streaming_text.with(|map| {
                map.get(&agent_id)
                    .map(|stream| (stream.text.clone(), stream.reasoning.clone()))
            })?;
            let text = handles.0.get();
            let preview_source = if text.trim().is_empty() {
                handles.1.get()
            } else {
                text
            };
            streaming_preview(&preview_source)
        }
    });

    let open_state = state.clone();
    let open_agent_id = agent_id.clone();
    let on_open = move |_: web_sys::MouseEvent| {
        let Some(parent) = parent_ref.get_untracked() else {
            log::error!("Open agent clicked on an agent-control card with no resolved agent");
            return;
        };
        open_state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: parent.host_id,
                agent_id: open_agent_id.clone(),
            }),
            display_name.get_untracked(),
            true,
        );
    };

    view! {
        <div class="tool-live-agent-row">
            <div class="tool-live-agent-main">
                <span class="tool-live-agent-name">{move || display_name.get()}</span>
                <span class=move || derived_status.get().class()>
                    {move || derived_status.get().label()}
                </span>
            </div>
            <button class="tool-live-link" on:click=on_open>"Open agent"</button>
            {move || preview.get().map(|text| view! {
                <div class="tool-live-agent-preview">{text}</div>
            })}
        </div>
    }
}

fn streaming_preview(text: &str) -> Option<String> {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(truncate_inline(&compact, 140))
    }
}

fn truncate_inline(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}\u{2026}")
    } else {
        truncated
    }
}

/// Live status line on a Task tool card while its sub-agent runs, with a
/// link that opens the sub-agent's own chat tab.
fn subagent_status_line(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    progress: SubAgentProgress,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let status_text = if progress.completed {
        format!(
            "\u{2713} {} finished \u{b7} {} tool calls",
            progress.agent_name, progress.tool_calls
        )
    } else {
        let last_tool = progress
            .last_tool_name
            .clone()
            .map(|name| format!(" \u{b7} last tool: {name}"))
            .unwrap_or_default();
        format!(
            "\u{27f3} {} running{last_tool} \u{b7} {} tool calls",
            progress.agent_name, progress.tool_calls
        )
    };
    let agent_id = progress.agent_id.clone();
    let agent_name = progress.agent_name.clone();

    let on_open = move |_: web_sys::MouseEvent| {
        // The sub-agent lives on the same host as the chat that spawned
        // it; the parent's agent_ref is plumbed in explicitly.
        let Some(parent) = agent_ref.get_untracked() else {
            log::error!("Open agent clicked on a card with no resolved agent");
            return;
        };
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: parent.host_id,
                agent_id: agent_id.clone(),
            }),
            agent_name.clone(),
            true,
        );
    };

    view! {
        <div class="tool-live-subagent">
            <span class="tool-live-title">{status_text}</span>
            <button class="tool-live-link" on:click=on_open>"Open agent"</button>
        </div>
    }
}

/// Body for a tool whose request kind has an interactive renderer (currently
/// only `AskUserQuestion`) but whose completion failed, so the interactive UI
/// must not show. `Error` results are handled by the shell's short-circuit; this
/// renders any other non-success result as raw JSON.
fn failed_result_body(result: Option<&ToolExecutionResult>) -> AnyView {
    let Some(result) = result else {
        return ().into_any();
    };
    let pretty = serde_json::to_string_pretty(result).unwrap_or_else(|_| format!("{result:?}"));
    view! { <pre class="tool-raw-result">{pretty}</pre> }.into_any()
}

// ── Header bits ─────────────────────────────────────────────────────────

fn tool_icon_and_detail(name: &str, tool_type: &ToolRequestType) -> (&'static str, Option<String>) {
    match tool_type {
        ToolRequestType::ModifyFile { file_path, .. } => ("\u{270f}", Some(short_path(file_path))),
        ToolRequestType::RunCommand { command, .. } => ("\u{25b6}", Some(command.clone())),
        ToolRequestType::ReadFiles { file_paths } => {
            let label = if file_paths.len() == 1 {
                short_path(&file_paths[0])
            } else {
                format!("{} files", file_paths.len())
            };
            ("\u{1f4c4}", Some(label))
        }
        ToolRequestType::SearchTypes { type_name, .. } => ("\u{1f50d}", Some(type_name.clone())),
        ToolRequestType::GetTypeDocs { type_path, .. } => ("\u{1f4d6}", Some(type_path.clone())),
        ToolRequestType::AskUserQuestion { questions } => {
            let detail = match questions.len() {
                0 => None,
                1 => questions[0].header.clone(),
                n => Some(format!("{n} questions")),
            };
            ("\u{2753}", detail)
        }
        ToolRequestType::ExitPlanMode { plan_path, .. } => (
            "\u{1f4dd}",
            plan_path.clone().or_else(|| Some("Plan".to_owned())),
        ),
        ToolRequestType::Other { .. } => {
            let icon = match name {
                n if n.contains("spawn") || n.contains("agent") => "\u{1f916}",
                n if n.contains("ask") || n.contains("question") || n.contains("input") => {
                    "\u{2753}"
                }
                _ => "\u{2699}",
            };
            (icon, None)
        }
    }
}

/// Short header line attached next to the status — legacy parity with
/// `completionHeaderDetail` in tools.ts. Renderer modules don't use this; the
/// shell does.
pub(crate) fn completion_header_summary(
    req: &ToolRequestType,
    result: &ToolExecutionResult,
) -> String {
    match result {
        ToolExecutionResult::ModifyFile {
            lines_added,
            lines_removed,
        } => format!("+{lines_added} -{lines_removed}"),
        ToolExecutionResult::RunCommand {
            exit_code,
            stdout,
            stderr,
        } => {
            let mut parts = Vec::with_capacity(3);
            parts.push(format!("exit {exit_code}"));
            let stdout_lines = count_summary_lines(stdout);
            let stderr_lines = count_summary_lines(stderr);
            if stdout_lines > 0 {
                parts.push(format!("out {stdout_lines}L"));
            }
            if stderr_lines > 0 {
                parts.push(format!("err {stderr_lines}L"));
            }
            // Suppress request-side info — the request's command is already in
            // the header detail. Keep this concise.
            let _ = req;
            parts.join(" \u{b7} ")
        }
        ToolExecutionResult::ReadFiles { files } => {
            let total_bytes: u64 = files.iter().map(|f| f.bytes).sum();
            if files.len() == 1 {
                format_bytes(total_bytes)
            } else {
                format!("{} files \u{b7} {}", files.len(), format_bytes(total_bytes))
            }
        }
        ToolExecutionResult::SearchTypes { types } => {
            if types.is_empty() {
                "no matches".to_owned()
            } else {
                format!(
                    "{} matching {}",
                    types.len(),
                    if types.len() == 1 { "type" } else { "types" }
                )
            }
        }
        ToolExecutionResult::GetTypeDocs { documentation } => {
            let lines = count_summary_lines(documentation);
            if lines == 0 {
                "no documentation".to_owned()
            } else {
                format!("documentation \u{b7} {lines}L")
            }
        }
        ToolExecutionResult::Error { short_message, .. } => {
            let trimmed = short_message.replace('\n', " ");
            if trimmed.len() > 90 {
                format!("error \u{b7} {}\u{2026}", &trimmed[..87])
            } else {
                format!("error \u{b7} {trimmed}")
            }
        }
        ToolExecutionResult::Other { .. } => String::new(),
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────

pub(crate) fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        path.to_owned()
    } else {
        format!("\u{2026}/{}", parts[parts.len() - 2..].join("/"))
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(crate) fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Line count for completion summaries. Empty/whitespace-only strings count
/// as 0 lines — matches legacy `countSummaryLines`.
pub(crate) fn count_summary_lines(text: &str) -> usize {
    if text.trim().is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

#[cfg(test)]
mod tool_visibility_tests {
    use super::*;
    use protocol::{
        AskUserQuestion, AskUserQuestionOption, ToolExecutionCompletedData, ToolRequest,
    };
    use serde_json::json;

    fn completed_entry(idx: usize, tool_type: ToolRequestType) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: format!("toolu_{idx}"),
                tool_name: match &tool_type {
                    ToolRequestType::AskUserQuestion { .. } => "AskUserQuestion".to_owned(),
                    _ => "OtherTool".to_owned(),
                },
                tool_type,
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: format!("toolu_{idx}"),
                tool_name: "OtherTool".to_owned(),
                tool_result: ToolExecutionResult::Other { result: json!({}) },
                success: true,
                error: None,
            }),
        }
    }

    fn completed_other_entry(idx: usize) -> ToolRequestEntry {
        completed_entry(idx, ToolRequestType::Other { args: json!({}) })
    }

    fn completed_ask_entry(idx: usize) -> ToolRequestEntry {
        completed_entry(
            idx,
            ToolRequestType::AskUserQuestion {
                questions: vec![AskUserQuestion {
                    id: None,
                    question: "Which language?".to_owned(),
                    header: Some("Language".to_owned()),
                    options: vec![AskUserQuestionOption {
                        label: "Rust".to_owned(),
                        description: None,
                    }],
                    multi_select: false,
                }],
            },
        )
    }

    fn completed_exit_plan_entry(idx: usize) -> ToolRequestEntry {
        completed_entry(
            idx,
            ToolRequestType::ExitPlanMode {
                plan: Some("Step 1".to_owned()),
                plan_path: Some("docs/plan.md".to_owned()),
            },
        )
    }

    #[test]
    fn collapsed_large_lists_keep_successful_ask_questions_visible() {
        let mut entries: Vec<_> = (0..100).map(completed_other_entry).collect();
        entries[40] = completed_ask_entry(40);

        let visible =
            visible_tool_indexes(entries.len(), false, |idx| is_important_tool(&entries[idx]));

        assert!(
            visible.contains(&40),
            "successful AskUserQuestion should remain visible in collapsed lists"
        );
        assert!(
            !visible.contains(&41),
            "nearby successful non-important tool should stay hidden"
        );
    }

    #[test]
    fn collapsed_large_lists_keep_successful_exit_plan_mode_visible() {
        let mut entries: Vec<_> = (0..100).map(completed_other_entry).collect();
        entries[40] = completed_exit_plan_entry(40);

        let visible =
            visible_tool_indexes(entries.len(), false, |idx| is_important_tool(&entries[idx]));

        assert!(
            visible.contains(&40),
            "successful ExitPlanMode should remain discoverable in collapsed lists"
        );
        assert!(
            !visible.contains(&41),
            "nearby successful non-important tool should stay hidden"
        );
    }
}

#[cfg(test)]
mod completion_summary_tests {
    use super::*;
    use protocol::FileInfo;

    fn req_run_command() -> ToolRequestType {
        ToolRequestType::RunCommand {
            command: "ls".to_owned(),
            working_directory: String::new(),
        }
    }

    #[test]
    fn modify_file_summary() {
        let req = ToolRequestType::ModifyFile {
            file_path: "x".into(),
            before: String::new(),
            after: String::new(),
        };
        let res = ToolExecutionResult::ModifyFile {
            lines_added: 5,
            lines_removed: 3,
        };
        assert_eq!(completion_header_summary(&req, &res), "+5 -3");
    }

    #[test]
    fn run_command_summary_includes_streams() {
        let res = ToolExecutionResult::RunCommand {
            exit_code: 0,
            stdout: "a\nb\nc".to_owned(),
            stderr: "err1".to_owned(),
        };
        assert_eq!(
            completion_header_summary(&req_run_command(), &res),
            "exit 0 \u{b7} out 3L \u{b7} err 1L"
        );
    }

    #[test]
    fn run_command_summary_no_streams_omits_them() {
        let res = ToolExecutionResult::RunCommand {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(
            completion_header_summary(&req_run_command(), &res),
            "exit 0"
        );
    }

    #[test]
    fn run_command_summary_nonzero_exit() {
        let res = ToolExecutionResult::RunCommand {
            exit_code: 2,
            stdout: String::new(),
            stderr: "boom".to_owned(),
        };
        assert_eq!(
            completion_header_summary(&req_run_command(), &res),
            "exit 2 \u{b7} err 1L"
        );
    }

    #[test]
    fn read_files_single_shows_bytes() {
        let req = ToolRequestType::ReadFiles {
            file_paths: vec!["a".into()],
        };
        let res = ToolExecutionResult::ReadFiles {
            files: vec![FileInfo {
                path: "a".into(),
                bytes: 1234,
            }],
        };
        assert_eq!(completion_header_summary(&req, &res), "1.2KB");
    }

    #[test]
    fn read_files_multi_shows_count_and_total() {
        let req = ToolRequestType::ReadFiles {
            file_paths: vec!["a".into(), "b".into()],
        };
        let res = ToolExecutionResult::ReadFiles {
            files: vec![
                FileInfo {
                    path: "a".into(),
                    bytes: 500,
                },
                FileInfo {
                    path: "b".into(),
                    bytes: 1500,
                },
            ],
        };
        assert_eq!(
            completion_header_summary(&req, &res),
            "2 files \u{b7} 2.0KB"
        );
    }

    #[test]
    fn search_types_zero() {
        let req = ToolRequestType::SearchTypes {
            language: "rust".into(),
            workspace_root: "/r".into(),
            type_name: "Foo".into(),
        };
        let res = ToolExecutionResult::SearchTypes { types: vec![] };
        assert_eq!(completion_header_summary(&req, &res), "no matches");
    }

    #[test]
    fn search_types_singular_vs_plural() {
        let req = ToolRequestType::SearchTypes {
            language: "rust".into(),
            workspace_root: "/r".into(),
            type_name: "Foo".into(),
        };
        let one = ToolExecutionResult::SearchTypes {
            types: vec!["A".into()],
        };
        assert_eq!(completion_header_summary(&req, &one), "1 matching type");
        let many = ToolExecutionResult::SearchTypes {
            types: vec!["A".into(), "B".into()],
        };
        assert_eq!(completion_header_summary(&req, &many), "2 matching types");
    }

    #[test]
    fn get_type_docs_summary() {
        let req = ToolRequestType::GetTypeDocs {
            language: "rust".into(),
            workspace_root: "/r".into(),
            type_path: "std::vec::Vec".into(),
        };
        let empty = ToolExecutionResult::GetTypeDocs {
            documentation: "  \n  ".into(),
        };
        assert_eq!(completion_header_summary(&req, &empty), "no documentation");
        let docs = ToolExecutionResult::GetTypeDocs {
            documentation: "line1\nline2\nline3".into(),
        };
        assert_eq!(
            completion_header_summary(&req, &docs),
            "documentation \u{b7} 3L"
        );
    }

    #[test]
    fn error_summary_truncates_long_messages() {
        let req = req_run_command();
        let res = ToolExecutionResult::Error {
            short_message: "x".repeat(100),
            detailed_message: String::new(),
        };
        let out = completion_header_summary(&req, &res);
        assert!(out.starts_with("error \u{b7} "));
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn error_summary_short_passes_through() {
        let req = req_run_command();
        let res = ToolExecutionResult::Error {
            short_message: "boom".into(),
            detailed_message: String::new(),
        };
        assert_eq!(completion_header_summary(&req, &res), "error \u{b7} boom");
    }

    #[test]
    fn count_summary_lines_handles_blank_and_text() {
        assert_eq!(count_summary_lines(""), 0);
        assert_eq!(count_summary_lines("   "), 0);
        assert_eq!(count_summary_lines("\n\n"), 0);
        assert_eq!(count_summary_lines("a"), 1);
        assert_eq!(count_summary_lines("a\nb"), 2);
        assert_eq!(count_summary_lines("a\nb\n"), 3);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod live_card_wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use crate::state::{AgentInfo, AppState, StreamingState};
    use leptos::mount::mount_to;
    use protocol::{
        AgentControlAgentRef, AgentControlProgress, AgentControlProgressKind, AgentId, AgentOrigin,
        BackendKind, StreamPath, ToolExecutionCompletedData, ToolProgressData, ToolRequest,
        WorkflowAgentState, WorkflowAgentStatus,
    };
    use serde_json::json;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn chat_agent_ref() -> ActiveAgentRef {
        ActiveAgentRef {
            host_id: "host-1".to_owned(),
            agent_id: AgentId("agent-1".to_owned()),
        }
    }

    /// Mount a card the way the app does: progress lives in
    /// `AppState::tool_progress`, never on the entry, and the chat's
    /// agent_ref is plumbed in explicitly. Returns the progress store so
    /// tests can update it after mount and assert the live re-render.
    fn mount_card(
        entry: ToolRequestEntry,
        progress: Option<ToolProgressData>,
    ) -> (HtmlElement, AppState) {
        let state = AppState::new();
        if let Some(progress) = progress {
            state.tool_progress.update(|map| {
                map.insert(
                    (
                        chat_agent_ref().agent_id,
                        ToolCallId(progress.tool_call_id.clone()),
                    ),
                    ArcRwSignal::new(progress),
                );
            });
        }
        let container = make_container();
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            let agent_ref = Signal::derive(|| Some(chat_agent_ref()));
            view! { <ToolCardView agent_ref=agent_ref entry=entry /> }
        });
        handle.forget();
        (container, state)
    }

    fn completed_other_request(tool_call_id: &str, tool_name: &str) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                tool_type: ToolRequestType::Other { args: json!({}) },
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: json!({"runId": "wf_123"}),
                },
                success: true,
                error: None,
            }),
        }
    }

    fn workflow_progress(status: WorkflowRunStatus) -> ToolProgressData {
        ToolProgressData {
            tool_call_id: "toolu_wf".to_owned(),
            tool_name: "Workflow".to_owned(),
            update: ToolProgressUpdate::Workflow(workflow_state(status)),
        }
    }

    fn subagent_progress_data(tool_calls: u64, completed: bool) -> ToolProgressData {
        ToolProgressData {
            tool_call_id: "toolu_task".to_owned(),
            tool_name: "Task".to_owned(),
            update: ToolProgressUpdate::SubAgent(SubAgentProgress {
                agent_id: AgentId("agent-sub".to_owned()),
                agent_name: "Explore".to_owned(),
                last_tool_name: Some("Read".to_owned()),
                tool_calls,
                completed,
            }),
        }
    }

    fn agent_control_progress_data(progress_kind: AgentControlProgressKind) -> ToolProgressData {
        ToolProgressData {
            tool_call_id: "toolu_agent_control".to_owned(),
            tool_name: match progress_kind {
                AgentControlProgressKind::Spawn => "tyde_spawn_agent",
                AgentControlProgressKind::Await => "tyde_await_agents",
            }
            .to_owned(),
            update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                progress_kind,
                agents: vec![AgentControlAgentRef {
                    agent_id: AgentId("agent-sub".to_owned()),
                    name: Some("Worker".to_owned()),
                }],
            }),
        }
    }

    fn agent_info(id: &str, name: &str, started: bool) -> AgentInfo {
        AgentInfo {
            host_id: "host-1".to_owned(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::AgentControl,
            backend_kind: BackendKind::Codex,
            workspace_roots: vec!["/tmp/work".to_owned()],
            project_id: None,
            parent_agent_id: Some(chat_agent_ref().agent_id),
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 1,
            instance_stream: StreamPath(format!("/agents/{id}")),
            started,
            fatal_error: None,
        }
    }

    fn streaming_state(text: &str) -> StreamingState {
        StreamingState {
            agent_name: "codex".to_owned(),
            model: None,
            text: ArcRwSignal::new(text.to_owned()),
            reasoning: ArcRwSignal::new(String::new()),
            tool_requests: ArcRwSignal::new(Vec::new()),
        }
    }

    fn tool_header_status(container: &HtmlElement) -> String {
        container
            .query_selector(".tool-status-text")
            .expect("query status")
            .expect("status element")
            .text_content()
            .unwrap_or_default()
    }

    fn workflow_state(status: WorkflowRunStatus) -> WorkflowRunState {
        WorkflowRunState {
            workflow_name: "wfprobe".to_owned(),
            description: None,
            script: None,
            status,
            summary: None,
            total_tokens: 13078,
            tool_uses: 0,
            duration_ms: 3000,
            agents: vec![WorkflowAgentState {
                index: 1,
                label: "probe-1".to_owned(),
                phase_title: Some("Probe".to_owned()),
                model: None,
                state: WorkflowAgentStatus::Running,
                tokens: 100,
                tool_calls: 0,
                duration_ms: 0,
                attempt: 1,
                prompt_preview: None,
                result_preview: None,
            }],
        }
    }

    #[wasm_bindgen_test]
    async fn completed_workflow_tool_with_running_run_shows_running_status() {
        let entry = completed_other_request("toolu_wf", "Workflow");
        let (container, state) =
            mount_card(entry, Some(workflow_progress(WorkflowRunStatus::Running)));
        next_tick().await;

        let body = text(&container);
        // The tool call itself succeeded, but the run is still going —
        // the user must see it as running, not done.
        assert!(
            body.contains("Running\u{2026}"),
            "status is running: {body}"
        );
        assert!(!body.contains("Done"), "no Done while run active: {body}");
        assert!(body.contains("probe-1"), "live agent row visible: {body}");
        assert!(
            body.contains("Open workflow"),
            "open-workflow link present: {body}"
        );

        // A later snapshot in the store must re-render the mounted card
        // in place — this is the post-turn update path.
        let key = (chat_agent_ref().agent_id, ToolCallId("toolu_wf".to_owned()));
        let signal = state
            .tool_progress
            .with_untracked(|map| map.get(&key).cloned())
            .expect("progress signal");
        signal.set(workflow_progress(WorkflowRunStatus::Completed));
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Done"), "card flips to Done live: {body}");
        assert!(body.contains("Completed"), "header updates live: {body}");
    }

    #[wasm_bindgen_test]
    async fn completed_workflow_run_shows_done_status() {
        let entry = completed_other_request("toolu_wf", "Workflow");
        let (container, _state) =
            mount_card(entry, Some(workflow_progress(WorkflowRunStatus::Completed)));
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Done"), "completed run shows Done: {body}");
    }

    #[wasm_bindgen_test]
    async fn task_card_shows_live_subagent_status_and_open_link() {
        let mut entry = completed_other_request("toolu_task", "Task");
        entry.result = None; // Task tool is still pending while agent runs.
        let (container, _state) = mount_card(entry, Some(subagent_progress_data(12, false)));
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Explore running"),
            "live status line visible: {body}"
        );
        assert!(
            body.contains("last tool: Read"),
            "last tool visible: {body}"
        );
        assert!(body.contains("12 tool calls"), "tool count visible: {body}");
        assert!(body.contains("Open agent"), "open-agent link: {body}");
    }

    #[wasm_bindgen_test]
    async fn finished_subagent_line_shows_completion() {
        let entry = completed_other_request("toolu_task", "Task");
        let (container, _state) = mount_card(entry, Some(subagent_progress_data(30, true)));
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Explore finished"),
            "finished line visible: {body}"
        );
        assert!(body.contains("Done"), "tool status is Done: {body}");
    }

    #[wasm_bindgen_test]
    async fn agent_control_spawn_card_tracks_live_agent_state() {
        let entry = completed_other_request("toolu_agent_control", "tyde_spawn_agent");
        let (container, state) = mount_card(
            entry,
            Some(agent_control_progress_data(AgentControlProgressKind::Spawn)),
        );
        let agent_id = AgentId("agent-sub".to_owned());
        state.agents.update(|agents| {
            agents.push(agent_info("agent-sub", "Worker Real", true));
        });
        state.agent_turn_active.update(|map| {
            map.insert(agent_id.clone(), true);
        });
        state.streaming_text.update(|map| {
            map.insert(
                agent_id.clone(),
                streaming_state("Implementing live tool cards"),
            );
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Running\u{2026}"),
            "header stays live: {body}"
        );
        assert!(body.contains("Worker Real"), "AppState name wins: {body}");
        assert!(body.contains("Running"), "agent status visible: {body}");
        assert!(
            body.contains("Implementing live tool cards"),
            "streaming preview visible: {body}"
        );
        assert!(body.contains("Open agent"), "open-agent affordance: {body}");

        state.agent_turn_active.update(|map| {
            map.remove(&agent_id);
        });
        state.streaming_text.update(|map| {
            map.remove(&agent_id);
        });
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Idle"), "agent row goes idle: {body}");
        assert!(
            body.contains("Done"),
            "card completes when child idle: {body}"
        );
    }

    #[wasm_bindgen_test]
    async fn agent_control_spawn_card_treats_unknown_agent_as_starting() {
        let entry = completed_other_request("toolu_agent_control", "tyde_spawn_agent");
        let (container, _state) = mount_card(
            entry,
            Some(agent_control_progress_data(AgentControlProgressKind::Spawn)),
        );
        next_tick().await;

        let body = text(&container);
        assert_eq!(
            tool_header_status(&container),
            "Running\u{2026}",
            "unknown spawned agent keeps header live"
        );
        assert!(
            body.contains("Starting"),
            "unknown spawned agent row starts optimistic: {body}"
        );
    }

    #[wasm_bindgen_test]
    async fn agent_control_await_card_header_follows_tool_lifecycle() {
        let entry = completed_other_request("toolu_agent_control", "tyde_await_agents");
        let (container, state) = mount_card(
            entry,
            Some(agent_control_progress_data(AgentControlProgressKind::Await)),
        );
        let agent_id = AgentId("agent-sub".to_owned());
        state.agents.update(|agents| {
            agents.push(agent_info("agent-sub", "Awaited Worker", true));
        });
        state.agent_turn_active.update(|map| {
            map.insert(agent_id.clone(), true);
        });
        state.streaming_text.update(|map| {
            map.insert(agent_id, streaming_state("Still finishing follow-up work"));
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Awaited Worker"),
            "awaited agent row visible: {body}"
        );
        assert!(
            body.contains("Still finishing follow-up work"),
            "awaited agent preview remains live: {body}"
        );
        assert!(
            body.contains("Running"),
            "row status can still show running: {body}"
        );
        assert_eq!(
            tool_header_status(&container),
            "Done",
            "completed await tool header should not stay running"
        );
    }
}
