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
    AgentControlProgress, AgentControlProgressKind, BackgroundTaskState, BackgroundTaskStatus,
    SubAgentProgress, ToolExecutionCompletedData, ToolExecutionNormalizationFailure,
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
mod tyde_await_agents;
mod tyde_send_agent_message;

const TOOL_LIST_INLINE_LIMIT: usize = 80;
const TOOL_LIST_HEAD_COUNT: usize = 8;
const TOOL_LIST_TAIL_COUNT: usize = 32;
const SANITIZE_MAX_DEPTH: usize = 8;
const EMBEDDED_JSON_WRAPPER_KEYS: &[&str] = &[
    "arguments",
    "args",
    "input",
    "input_data",
    "inputData",
    "tool_input",
    "toolInput",
    "parameters",
    "params",
];

fn malformed_request_payload<'a>(
    request: &'a ToolRequestType,
    completion: Option<&ToolExecutionCompletedData>,
) -> Option<&'a serde_json::Value> {
    let ToolRequestType::Other { args } = request else {
        return None;
    };
    request_normalization_failed(completion?).then_some(args)
}

fn request_normalization_failed(completion: &ToolExecutionCompletedData) -> bool {
    matches!(
        completion.normalization_failure,
        Some(ToolExecutionNormalizationFailure::CanonicalRequest)
            | Some(ToolExecutionNormalizationFailure::CanonicalRequestAndResult)
    )
}

fn sanitized_request_payload_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(&sanitize_request_payload(value, 0))
        .unwrap_or_else(|_| "\"[SANITIZATION FAILED]\"".to_owned())
}

fn sanitize_request_payload(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    if depth > SANITIZE_MAX_DEPTH {
        return serde_json::Value::String("[REDACTED: MAX DEPTH]".to_owned());
    }
    match value {
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let sanitized = if is_secret_key(key) {
                        serde_json::Value::String("[REDACTED]".to_owned())
                    } else if is_embedded_json_wrapper(key) {
                        sanitize_embedded_json(value, depth + 1)
                    } else {
                        sanitize_request_payload(value, depth + 1)
                    };
                    (key.clone(), sanitized)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| sanitize_request_payload(value, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn sanitize_embedded_json(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    let serde_json::Value::String(text) = value else {
        return sanitize_request_payload(value, depth);
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return value.clone();
    };
    serde_json::Value::String(
        serde_json::to_string(&sanitize_request_payload(&parsed, depth))
            .unwrap_or_else(|_| "[SANITIZATION FAILED]".to_owned()),
    )
}

fn is_embedded_json_wrapper(key: &str) -> bool {
    EMBEDDED_JSON_WRAPPER_KEYS.contains(&key)
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized == "auth"
        || normalized.contains("authorization")
        || normalized.contains("bearer")
        || normalized.contains("cookie")
        || normalized.contains("apikey")
        || normalized.contains("token")
        || normalized.contains("password")
        || normalized.contains("passwd")
        || normalized.contains("secret")
        || normalized.contains("privatekey")
        || normalized.contains("credential")
        || normalized.contains("psk")
}

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
        || entry
            .result
            .as_ref()
            .is_some_and(|result| result.normalization_failure.is_some())
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
    let malformed_payload = malformed_request_payload(&tool_type, result.as_ref()).cloned();
    let normalization_failure = result
        .as_ref()
        .and_then(|result| result.normalization_failure);
    let normalization_failed = normalization_failure.is_some();

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
    let background_task: Signal<Option<BackgroundTaskState>> =
        Signal::derive(move || match progress.get().map(|data| data.update) {
            Some(ToolProgressUpdate::BackgroundTask(task)) => Some(task),
            _ => None,
        });
    // A background task can outlive its tool call: the Workflow tool
    // result is just the run id, the real work keeps going. Agent-control
    // cards deliberately don't contribute — child-agent liveness renders in
    // the In-flight tray, and a spawn receipt is Done once its result lands.
    let background_running = Signal::derive(move || {
        workflow_run
            .get()
            .is_some_and(|run| run.status == WorkflowRunStatus::Running)
            || subagent_progress
                .get()
                .is_some_and(|progress| !progress.completed)
            || background_task
                .get()
                .is_some_and(|task| task.status == BackgroundTaskStatus::Running)
    });
    // The body shape (workflow panel vs regular renderer) flips at most
    // once, when the first snapshot arrives — memoized so per-snapshot
    // updates don't recreate the body, only its inner reactive text.
    let is_workflow = Memo::new(move |_| workflow_run.with(|run| run.is_some()));

    let has_result = result.is_some();
    let result_success =
        result.as_ref().map(|r| r.success).unwrap_or(false) && !normalization_failed;
    let result_cancelled = result
        .as_ref()
        .is_some_and(|result| matches!(&result.tool_result, ToolExecutionResult::Cancelled { .. }));
    let result_failed = has_result && !result_success;

    let status_class = move || {
        if !has_result || (background_running.get() && !result_failed) {
            "tool-status-text pending"
        } else if result_cancelled {
            "tool-status-text cancelled"
        } else if result_success {
            "tool-status-text success"
        } else {
            "tool-status-text failure"
        }
    };

    let status_label = move || {
        if !has_result || (background_running.get() && !result_failed) {
            "Running\u{2026}".to_owned()
        } else if result_cancelled {
            "Cancelled".to_owned()
        } else if result_success {
            "Done".to_owned()
        } else {
            "Failed".to_owned()
        }
    };

    let (icon, header_detail) = tool_icon_and_detail(&tool_name, &tool_type);

    // A send-message card names its recipient in the header, so "who was
    // messaged" is answerable while the card is collapsed. The name has to be
    // resolved here rather than in `tool_icon_and_detail` because it comes from
    // server-owned agent state; the request alone carries only a uuid.
    let send_recipient_id = match &tool_type {
        ToolRequestType::TydeSendAgentMessage { agent_id, .. } => Some(agent_id.clone()),
        _ => None,
    };
    let recipient_detail: Signal<Option<String>> = Signal::derive({
        let state = state.clone();
        move || {
            let agent_id = send_recipient_id.clone()?;
            Some(agent_display_name(&state, agent_ref.get(), &agent_id, None))
        }
    });

    let header_detail = move || {
        workflow_run
            .get()
            .map(|run| run.workflow_name)
            .or_else(|| {
                agent_control_progress
                    .get()
                    .map(|progress| agent_control_header_detail(&progress))
            })
            .or_else(|| {
                background_task
                    .get()
                    .and_then(|task| task.summary.or(task.description))
            })
            .or_else(|| recipient_detail.get())
            .or_else(|| header_detail.clone())
    };

    let completion_summary = if let Some(failure) = normalization_failure {
        Some(match failure {
            ToolExecutionNormalizationFailure::CanonicalRequest => {
                "request normalization failed".to_owned()
            }
            ToolExecutionNormalizationFailure::CanonicalResult => {
                "result normalization failed".to_owned()
            }
            ToolExecutionNormalizationFailure::CanonicalRequestAndResult => {
                "request and result normalization failed".to_owned()
            }
        })
    } else {
        result
            .as_ref()
            .map(|r| completion_header_summary(&tool_type, &r.tool_result))
    };

    let is_ask_user_question = matches!(tool_type, ToolRequestType::AskUserQuestion { .. });
    let body_tool_type = tool_type.clone();
    let body_result = result.as_ref().map(|r| r.tool_result.clone());
    let body_tool_type_slot = StoredValue::new_local(body_tool_type);
    let body_result_slot = StoredValue::new_local(body_result);
    let malformed_payload_slot = StoredValue::new_local(malformed_payload);
    let tool_call_id_slot = StoredValue::new_local(tool_call_id);
    let details_open = RwSignal::new(
        is_ask_user_question
            || !has_result
            || !result_success
            || normalization_failed
            || background_running.get_untracked(),
    );
    let default_open_for_body = move || {
        is_ask_user_question
            || !has_result
            || !result_success
            || normalization_failed
            || background_running.get()
            || tool_output_mode.get() != ToolOutputMode::Summary
    };
    let default_open_for_prop = move || {
        is_ask_user_question
            || !has_result
            || !result_success
            || normalization_failed
            || background_running.get()
            || tool_output_mode.get() != ToolOutputMode::Summary
    };
    let render_body_when = move || default_open_for_body() || details_open.get();

    view! {
        <details
            class=if normalization_failed { "tool-card tool-card-malformed" } else { "tool-card" }
            aria-label=normalization_failed.then_some("Tool failed: canonical data could not be normalized")
            prop:open=default_open_for_prop
            on:toggle=move |ev: leptos::ev::Event| {
                if let Some(target) = ev.target()
                    && let Ok(el) = target.dyn_into::<web_sys::HtmlDetailsElement>()
                {
                    if normalization_failed && !el.open() {
                        el.set_open(true);
                    }
                    details_open.set(el.open());
                }
            }
        >
            <summary class="tool-card-header">
                // Purely decorative: the tool name beside it is the accessible
                // label, so a screen reader announcing the emoji adds only noise.
                <span class="tool-card-icon" aria-hidden="true">{icon}</span>
                <span class="tool-card-name">{tool_name}</span>
                {move || header_detail().map(|d| view! {
                    <span class="tool-card-detail">{d}</span>
                })}
                {completion_summary.map(|s| view! {
                    <span class="tool-completion-summary">{s}</span>
                })}
                <span class=status_class>{status_label}</span>
                // The native <details>/<summary> already announces its own
                // expanded state; the glyph is decoration on top of that.
                <span class="tool-chevron" aria-hidden="true">"\u{25b6}"</span>
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
                                {(normalization_failure == Some(
                                    ToolExecutionNormalizationFailure::CanonicalResult,
                                )).then(|| view! {
                                    <div class="tool-typed-mismatch" role="alert">
                                        "The canonical tool result could not be normalized."
                                    </div>
                                })}
                                {move || {
                                    agent_control_progress.get().map(|progress| {
                                        agent_control_receipt(agent_ref, progress)
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
                                                malformed_payload_slot.with_value(|malformed_payload| {
                                                    render_body(
                                                        agent_ref,
                                                        tool_call_id,
                                                        body_tool_type,
                                                        body_result.as_ref(),
                                                        malformed_payload.as_ref(),
                                                        mode,
                                                        result_failed,
                                                    )
                                                })
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
/// it. Cancellation is likewise terminal, but is not presented as a failure.
fn render_body(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    tool_call_id: &str,
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    malformed_payload: Option<&serde_json::Value>,
    mode: ToolOutputMode,
    result_failed: bool,
) -> AnyView {
    if let Some(ToolExecutionResult::Cancelled { message }) = result {
        return view! {
            <div class="tool-cancelled" role="status">{message.clone()}</div>
        }
        .into_any();
    }
    if let Some(ToolExecutionResult::Error { .. }) = result {
        return error_result::render(result.unwrap(), malformed_payload, mode).into_any();
    }

    match req {
        ToolRequestType::ModifyFile { .. } => modify_file::render(req, result, mode).into_any(),
        ToolRequestType::RunCommand { .. } => run_command::render(req, result, mode).into_any(),
        ToolRequestType::ReadFiles { .. } => read_files::render(req, result, mode).into_any(),
        ToolRequestType::SearchTypes { .. } => search_types::render(req, result, mode).into_any(),
        ToolRequestType::GetTypeDocs { .. } => get_type_docs::render(req, result, mode).into_any(),
        // The Tyde orchestration tools own their presentation end to end: a sent
        // message renders as Markdown, an await renders its agent list. Because
        // they are typed variants, they never reach `other::render`, so the raw
        // args/`Result JSON` panels that used to duplicate them are gone by
        // construction rather than suppressed by a flag.
        ToolRequestType::TydeSendAgentMessage { .. } => {
            tyde_send_agent_message::render(agent_ref, tool_call_id, req, result, mode)
        }
        ToolRequestType::TydeAwaitAgents { .. } => {
            tyde_await_agents::render(agent_ref, req, result, mode)
        }
        // A failed completion for a question is no longer answerable: render the
        // raw result instead of the interactive card, mirroring the mobile tool
        // card. The realistic failure carries `ToolExecutionResult::Error`, which
        // the shell short-circuits above; this arm covers a non-`Error` result
        // that still reports `success=false`.
        ToolRequestType::AskUserQuestion { .. } if result_failed => {
            failed_result_body(result).into_any()
        }
        ToolRequestType::AskUserQuestion { .. } => {
            ask_user_question::render(agent_ref, req, result, mode).into_any()
        }
        ToolRequestType::ExitPlanMode { .. } => {
            exit_plan_mode::render(agent_ref, tool_call_id, req, result, mode).into_any()
        }
        ToolRequestType::AgentSpawn { prompt, .. } => match prompt {
            Some(prompt) => view! {
                <div class="tool-agent-spawn-prompt">{prompt.clone()}</div>
            }
            .into_any(),
            None => view! {
                <div class="tool-agent-spawn-prompt-unavailable">
                    "Codex did not expose this native sub-agent's prompt."
                </div>
            }
            .into_any(),
        },
        ToolRequestType::GenerateImage { prompt } => view! {
            <div class="tool-image-generation-prompt">
                {prompt.clone().unwrap_or_else(|| "Generating image".to_owned())}
            </div>
        }
        .into_any(),
        ToolRequestType::WebSearch { query } => view! {
            <div class="tool-native-detail">{query.clone()}</div>
        }
        .into_any(),
        ToolRequestType::ViewImage { path } => view! {
            <div class="tool-native-detail">{path.clone()}</div>
        }
        .into_any(),
        ToolRequestType::Sleep { duration_ms } => view! {
            <div class="tool-native-detail">{format!("Waiting {} ms", duration_ms)}</div>
        }
        .into_any(),
        ToolRequestType::Other { .. } => {
            other::render(req, result, malformed_payload, mode).into_any()
        }
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

/// Receipt list for a spawn/await card: one row per referenced agent — its
/// live name with an "Open agent" action — plus a pointer to the In-flight
/// tray. Deliberately **not** a live monitor — per-agent status, previews,
/// and stats render exactly once, in the tray. Rendering them here too is
/// the spawn/await duplication the tray was introduced to remove: both cards
/// derived identical live rows from the same global signals, so every child
/// agent appeared twice on screen.
fn agent_control_receipt(
    parent_ref: Signal<Option<ActiveAgentRef>>,
    progress: AgentControlProgress,
) -> impl IntoView {
    let agents = progress.agents;

    view! {
        <div class="tool-live-agent-control">
            <For
                each=move || agents.clone()
                key=|agent| agent.agent_id.0.clone()
                let:agent
            >
                {
                    let state = expect_context::<AppState>();
                    let agent_id = agent.agent_id.clone();
                    let fallback_name = agent.name.clone();
                    let display_name = Signal::derive({
                        let state = state.clone();
                        let agent_id = agent_id.clone();
                        let fallback_name = fallback_name.clone();
                        move || {
                            agent_display_name(
                                &state,
                                parent_ref.get(),
                                &agent_id,
                                fallback_name.as_deref(),
                            )
                        }
                    });
                    let on_open = move |_: web_sys::MouseEvent| {
                        let Some(parent) = parent_ref.get_untracked() else {
                            log::error!(
                                "Open agent clicked on an agent-control card with no resolved agent"
                            );
                            return;
                        };
                        open_child_agent(&state, &parent.host_id, &agent_id);
                    };
                    view! {
                        <div class="tool-live-agent-receipt-row">
                            <span class="tool-live-agent-name">
                                {move || display_name.get()}
                            </span>
                            <button
                                type="button"
                                class="tool-live-link tool-live-agent-receipt-link"
                                on:click=on_open
                            >
                                "Open agent"
                            </button>
                        </div>
                    }
                }
            </For>
            <span class="tool-live-agent-receipt-hint">
                "live status in the In-flight tray"
            </span>
        </div>
    }
}

/// An agent's live human name, resolved reactively from server-owned state on
/// the parent's host. Falls back to the name the event carried, then to the raw
/// id — never to an invented label. Shared by every card that has to refer to a
/// child agent (agent-control rows, the send-message recipient, the await
/// verdict), so they can't drift apart on what an agent is called.
pub(crate) fn agent_display_name(
    state: &AppState,
    parent_ref: Option<ActiveAgentRef>,
    agent_id: &protocol::AgentId,
    fallback_name: Option<&str>,
) -> String {
    let state_name = parent_ref.and_then(|parent| {
        state.agents.with(|agents| {
            agents
                .iter()
                .find(|agent| agent.host_id == parent.host_id && agent.agent_id == *agent_id)
                .map(|agent| agent.name.clone())
        })
    });
    state_name
        .or_else(|| fallback_name.map(str::to_owned))
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| agent_id.0.clone())
}

/// Open a child agent from a tool card's "Open agent" action.
///
/// The child lives on the parent chat's host, so it is looked up in the
/// server-owned `agents` registry by `(parent_host, agent_id)` — the same record
/// that renders the child's name and status here. Opening then goes through
/// [`agents_panel::open_agent_chat`], which switches to the child's authoritative
/// owning project (and host) *before* opening the tab. That switch is
/// load-bearing: without it a cross-project child's tab lands in the currently
/// active project's `center_zone` and is discarded, so the button appears to do
/// nothing.
///
/// A child with no registry record cannot have its owning project resolved.
/// Because the card's own name and status come from that same record, its
/// absence is a bug, surfaced rather than papered over with a guessed project.
pub(crate) fn open_child_agent(state: &AppState, parent_host: &str, agent_id: &protocol::AgentId) {
    let child = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| agent.host_id.as_str() == parent_host && &agent.agent_id == agent_id)
            .cloned()
    });
    match child {
        Some(child) => crate::components::agents_panel::open_agent_chat(state, &child),
        None => log::error!(
            "Open agent: no registry record for child {agent_id:?} on host {parent_host}; \
             cannot resolve its owning project"
        ),
    }
}

/// Receipt line on a Task tool card for its sub-agent, with a link that opens
/// the sub-agent's own chat tab. While the sub-agent runs this names it and
/// defers to the In-flight tray — the live last-tool/tool-count detail renders
/// there, once. On completion it records the final outcome, which is the one
/// fact a live surface cannot keep: the tray's rows always show *now*.
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
        format!(
            "\u{27f3} {} running \u{b7} live status in the In-flight tray",
            progress.agent_name
        )
    };
    let agent_id = progress.agent_id.clone();

    let on_open = move |_: web_sys::MouseEvent| {
        // The sub-agent lives on the same host as the chat that spawned it; the
        // parent's agent_ref is plumbed in explicitly. `open_child_agent`
        // resolves the child's authoritative owning project and switches to it
        // before opening, so a cross-project sub-agent is not discarded.
        let Some(parent) = agent_ref.get_untracked() else {
            log::error!("Open agent clicked on a card with no resolved agent");
            return;
        };
        open_child_agent(&state, &parent.host_id, &agent_id);
    };

    view! {
        <div class="tool-live-subagent">
            <span class="tool-live-title">{status_text}</span>
            <button type="button" class="tool-live-link" on:click=on_open>"Open agent"</button>
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
        ToolRequestType::AgentSpawn { name, .. } => (
            "\u{1f916}",
            name.clone().or_else(|| Some("Spawning agent".to_owned())),
        ),
        ToolRequestType::GenerateImage { prompt } => (
            "\u{1f5bc}",
            prompt
                .clone()
                .or_else(|| Some("Generating image".to_owned())),
        ),
        ToolRequestType::WebSearch { query } => ("\u{1f50d}", Some(query.clone())),
        ToolRequestType::ViewImage { path } => ("\u{1f5bc}", Some(short_path(path))),
        ToolRequestType::Sleep { duration_ms } => ("\u{23f1}", Some(format!("{} ms", duration_ms))),
        // The recipient's live name needs app state, so the shell resolves it
        // (see `recipient_detail`). The request alone carries only a uuid, which
        // would be a worse header than none.
        ToolRequestType::TydeSendAgentMessage { .. } => ("\u{1f4ac}", None),
        // Mirrors `agent_control_header_detail`, so the label reads the same
        // whether it comes from the typed request or from live progress.
        ToolRequestType::TydeAwaitAgents { agent_ids } => {
            let detail = if agent_ids.len() == 1 {
                "Awaiting agent".to_owned()
            } else {
                format!("Awaiting {} agents", agent_ids.len())
            };
            ("\u{1f916}", Some(detail))
        }
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
        ToolExecutionResult::Cancelled { .. } => "cancelled".to_owned(),
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
        // The send tool's result is a bare ack, so the header carries the whole
        // outcome: the message reached the agent.
        ToolExecutionResult::TydeSendAgentMessage => "delivered".to_owned(),
        ToolExecutionResult::TydeAwaitAgents {
            ready,
            still_thinking,
        } => {
            let mut parts = Vec::with_capacity(2);
            if !ready.is_empty() {
                parts.push(format!("{} ready", ready.len()));
            }
            if !still_thinking.is_empty() {
                parts.push(format!("{} still thinking", still_thinking.len()));
            }
            parts.join(" \u{b7} ")
        }
        ToolExecutionResult::GenerateImage { image_count, .. } => {
            format!(
                "{image_count} image{}",
                if *image_count == 1 { "" } else { "s" }
            )
        }
        ToolExecutionResult::WebSearch => "search complete".to_owned(),
        ToolExecutionResult::ViewImage => "image viewed".to_owned(),
        ToolExecutionResult::Sleep => "wait complete".to_owned(),
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
                normalization_failure: None,
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

#[cfg(test)]
mod open_child_agent_tests {
    use super::*;
    use crate::state::{ActiveProjectRef, AgentInfo};
    use protocol::{AgentId, AgentOrigin, BackendKind, ProjectId, StreamPath};

    fn child_agent(host: &str, id: &str, name: &str, project: Option<&str>) -> AgentInfo {
        AgentInfo {
            host_id: host.to_owned(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: project.map(|p| ProjectId(p.to_owned())),
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath(format!("/agent/{id}/inst")),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn active_on(host: &str, project: &str) -> Option<ActiveProjectRef> {
        Some(ActiveProjectRef {
            host_id: host.to_owned(),
            project_id: ProjectId(project.to_owned()),
        })
    }

    /// A tool card's Open agent for a child in a *different* project resolves the
    /// child's authoritative owning project from the registry and switches to it
    /// before opening — without the switch, the chat tab would land in the active
    /// project's center zone and be discarded, so the button did nothing.
    #[test]
    fn open_child_agent_switches_to_the_childs_project_and_opens_its_chat() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            state.active_project.set(active_on("host-1", "alpha"));
            state.agents.set(vec![child_agent(
                "host-1",
                "child-1",
                "Child",
                Some("beta"),
            )]);

            // The parent host is host-1 (the child lives on the parent's host);
            // the child belongs to project "beta", not the active "alpha".
            open_child_agent(&state, "host-1", &AgentId("child-1".to_owned()));

            let active = state
                .active_project
                .get_untracked()
                .expect("active project stays set");
            assert_eq!(
                active.project_id,
                ProjectId("beta".to_owned()),
                "Open agent must switch to the child's owning project, not stay on the parent's"
            );
            assert_eq!(active.host_id, "host-1");

            let agent = state
                .active_agent
                .get_untracked()
                .expect("the child's chat opened and is active");
            assert_eq!(
                agent.agent_id,
                AgentId("child-1".to_owned()),
                "the exact child agent's chat is the active tab"
            );
            assert_eq!(agent.host_id, "host-1");
        });
    }

    /// A same-project child opens its chat without changing the active project —
    /// the common case must not regress into a spurious switch.
    #[test]
    fn open_child_agent_same_project_opens_without_switching() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            state.active_project.set(active_on("host-1", "alpha"));
            state.agents.set(vec![child_agent(
                "host-1",
                "child-1",
                "Child",
                Some("alpha"),
            )]);

            open_child_agent(&state, "host-1", &AgentId("child-1".to_owned()));

            assert_eq!(
                state.active_project.get_untracked().map(|p| p.project_id),
                Some(ProjectId("alpha".to_owned())),
                "a same-project child leaves the active project unchanged"
            );
            let agent = state
                .active_agent
                .get_untracked()
                .expect("the child's chat opened and is active");
            assert_eq!(agent.agent_id, AgentId("child-1".to_owned()));
        });
    }

    /// A child with no registry record cannot have its owning project resolved.
    /// The action surfaces the error and performs no navigation — no guessed
    /// project, no chat opened in the wrong place, no silent fallback.
    #[test]
    fn open_child_agent_without_a_registry_record_navigates_nowhere() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            let state = AppState::new();
            state.active_project.set(active_on("host-1", "alpha"));
            // The registry has no matching child.

            open_child_agent(&state, "host-1", &AgentId("missing".to_owned()));

            assert_eq!(
                state.active_project.get_untracked().map(|p| p.project_id),
                Some(ProjectId("alpha".to_owned())),
                "an unresolvable child must not switch the active project"
            );
            assert!(
                state.active_agent.get_untracked().is_none(),
                "and must not open a chat: no owning project means no navigation"
            );
        });
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod live_card_wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use crate::state::{AgentInfo, AppState, StreamingState};
    use leptos::mount::mount_to;
    use protocol::{
        AgentActivitySummary, AgentActivitySummaryState, AgentControlAgentRef,
        AgentControlProgress, AgentControlProgressKind, AgentId, AgentOrigin, BackendKind,
        StreamPath, ToolExecutionCompletedData, ToolProgressData, ToolRequest, WorkflowAgentState,
        WorkflowAgentStatus,
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
                normalization_failure: None,
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
            activity_summary: Default::default(),
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

    fn activity_stats(
        last_output_line: Option<&str>,
        tool_calls: u64,
        token_usage: protocol::TokenUsage,
    ) -> protocol::AgentActivityStats {
        protocol::AgentActivityStats {
            last_output_line: last_output_line.map(|s| s.to_owned()),
            tool_calls,
            token_usage,
            token_usage_total_only: None,
            source_through_seq: None,
        }
    }

    fn token_usage(input: u64, cached: u64, output: u64, reasoning: u64) -> protocol::TokenUsage {
        protocol::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_prompt_tokens: (cached > 0).then_some(cached),
            cache_creation_input_tokens: None,
            reasoning_tokens: (reasoning > 0).then_some(reasoning),
        }
    }

    fn seed_stats(state: &AppState, agent_id: &str, stats: protocol::AgentActivityStats) {
        // Child agents in these fixtures live on the parent chat's host.
        seed_stats_on_host(state, "host-1", agent_id, stats);
    }

    fn seed_stats_on_host(
        state: &AppState,
        host_id: &str,
        agent_id: &str,
        stats: protocol::AgentActivityStats,
    ) {
        state.agent_activity_stats.update(|map| {
            map.insert(
                ActiveAgentRef {
                    host_id: host_id.to_owned(),
                    agent_id: AgentId(agent_id.to_owned()),
                },
                stats,
            );
        });
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

    /// The Task card is a receipt while its sub-agent runs: it names the
    /// agent, keeps the open action, and defers live detail (last tool,
    /// tool-call count) to the In-flight tray — that half of the old
    /// `task_card_shows_live_subagent_status_and_open_link` contract is
    /// asserted in `inflight_tray::subagent_row_shows_last_tool_and_count`.
    /// Completion stays on the card (`finished_subagent_line_shows_completion`):
    /// the final outcome is the one fact a live surface cannot keep.
    #[wasm_bindgen_test]
    async fn task_card_defers_live_subagent_detail_to_the_tray() {
        let mut entry = completed_other_request("toolu_task", "Task");
        entry.result = None; // Task tool is still pending while agent runs.
        let (container, _state) = mount_card(entry, Some(subagent_progress_data(12, false)));
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Explore running"),
            "the running sub-agent is named: {body}"
        );
        assert!(
            body.contains("In-flight tray"),
            "the card points at the tray for live status: {body}"
        );
        assert!(body.contains("Open agent"), "open-agent link: {body}");
        assert!(
            !body.contains("last tool") && !body.contains("12 tool calls"),
            "live detail renders in the tray, not the card: {body}"
        );
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

    /// The spawn/await cards are receipts, not live monitors: the referenced
    /// agent renders as an open-agent action under its live AppState name,
    /// and once the tool result lands the card is Done even while the child
    /// keeps working. Liveness — status, previews, summaries, stats — renders
    /// exactly once, in the In-flight tray. The behavior this replaces
    /// (`agent_control_spawn_card_tracks_live_agent_state`) rendered
    /// identical live rows in both the spawn and await cards by
    /// construction; its live-state contract now lives in the tray tests
    /// (`inflight_tray::running_child_shows_streaming_preview_then_idles`,
    /// `unknown_spawned_agent_renders_starting_row`).
    #[wasm_bindgen_test]
    async fn agent_control_card_is_a_receipt_with_open_action() {
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
        assert!(body.contains("Worker Real"), "AppState name wins: {body}");
        assert_eq!(
            tool_header_status(&container),
            "Done",
            "a completed spawn is Done even while the child runs — liveness belongs to the tray"
        );
        assert!(
            !body.contains("Implementing live tool cards"),
            "no streaming preview in the receipt: {body}"
        );
        assert_eq!(
            count(&container, ".tool-live-agent-status"),
            0,
            "no live status badge in the receipt"
        );

        let link = container
            .query_selector(".tool-live-agent-receipt-link")
            .expect("query receipt link")
            .expect("receipt names the agent as an open action")
            .dyn_into::<HtmlElement>()
            .expect("receipt link is an HTML element");
        link.click();
        next_tick().await;
        let opened = state
            .active_agent
            .get_untracked()
            .expect("clicking the receipt opens the child agent");
        assert_eq!(opened.agent_id, agent_id);
        assert_eq!(opened.host_id, "host-1");
    }

    /// The receipt is a vertical list: one row per referenced agent, each
    /// carrying the agent's live name and its own "Open agent" action. This
    /// replaces the inline chip run whose auto-margin buttons scattered
    /// across the full card width.
    #[wasm_bindgen_test]
    async fn receipt_lists_each_agent_on_its_own_row() {
        let entry = completed_other_request("toolu_agent_control", "tyde_await_agents");
        let progress = ToolProgressData {
            tool_call_id: "toolu_agent_control".to_owned(),
            tool_name: "tyde_await_agents".to_owned(),
            update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                progress_kind: AgentControlProgressKind::Await,
                agents: vec![
                    AgentControlAgentRef {
                        agent_id: AgentId("agent-a".to_owned()),
                        name: Some("Codex design".to_owned()),
                    },
                    AgentControlAgentRef {
                        agent_id: AgentId("agent-b".to_owned()),
                        name: Some("Claude design".to_owned()),
                    },
                ],
            }),
        };
        let (container, _state) = mount_card(entry, Some(progress));
        next_tick().await;

        assert_eq!(
            count(&container, ".tool-live-agent-receipt-row"),
            2,
            "one receipt row per referenced agent"
        );
        assert_eq!(
            count(
                &container,
                ".tool-live-agent-receipt-row .tool-live-agent-receipt-link"
            ),
            2,
            "each row carries its own open action"
        );
        let body = text(&container);
        assert!(
            body.contains("Codex design") && body.contains("Claude design"),
            "every agent is named in the list: {body}"
        );
    }

    /// A native backend's wait card still exposes a working open action for
    /// the awaited child. The open contract is unchanged; the action is the
    /// row's "Open agent" button beside the agent's live name.
    #[wasm_bindgen_test]
    async fn native_codex_wait_card_opens_awaited_agent() {
        let entry = completed_other_request("native-wait", "wait");
        let progress = ToolProgressData {
            tool_call_id: "native-wait".to_owned(),
            tool_name: "wait".to_owned(),
            update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                progress_kind: AgentControlProgressKind::Await,
                agents: vec![AgentControlAgentRef {
                    agent_id: AgentId("native-child".to_owned()),
                    name: Some("/root/sleeper".to_owned()),
                }],
            }),
        };
        let (container, state) = mount_card(entry, Some(progress));
        let mut child = agent_info("native-child", "Sleeper", true);
        child.origin = AgentOrigin::BackendNative;
        state.agents.update(|agents| agents.push(child));
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Sleeper"),
            "awaited native child named by its live name: {body}"
        );

        let link = container
            .query_selector(".tool-live-agent-receipt-link")
            .expect("query receipt link")
            .expect("receipt link is rendered")
            .dyn_into::<HtmlElement>()
            .expect("receipt link is an HTML element");
        link.click();
        next_tick().await;

        let opened = state
            .active_agent
            .get_untracked()
            .expect("clicking the rendered action opens the native child");
        assert_eq!(opened.agent_id, AgentId("native-child".to_owned()));
        assert_eq!(opened.host_id, "host-1");
    }

    /// Neither card renders live per-agent detail, even when the server has
    /// all of it — summary, stats, output line, live status. Successor of
    /// `spawn_card_shows_no_summary_or_stats`, widened to the await card:
    /// the await card's summary/stats/output rendering moved to the tray
    /// (see `inflight_tray::summary_states_follow_server_enum` and the
    /// stats-line tests there), so on the cards the detail's *absence* is
    /// now the contract for both kinds.
    #[wasm_bindgen_test]
    async fn cards_render_no_live_agent_detail() {
        for (tool_name, kind) in [
            ("tyde_spawn_agent", AgentControlProgressKind::Spawn),
            ("tyde_await_agents", AgentControlProgressKind::Await),
        ] {
            let entry = completed_other_request("toolu_agent_control", tool_name);
            let (container, state) = mount_card(entry, Some(agent_control_progress_data(kind)));
            let mut info = agent_info("agent-sub", "Worker", true);
            info.activity_summary = AgentActivitySummaryState::Fresh {
                summary: AgentActivitySummary {
                    text: "Summary belongs to the tray".to_owned(),
                    generated_at_ms: js_sys::Date::now() as u64,
                    source_from_seq: Some(1),
                    source_through_seq: Some(9),
                },
            };
            state.agents.update(|agents| agents.push(info));
            seed_stats(
                &state,
                "agent-sub",
                activity_stats(
                    Some("output belongs to the tray"),
                    7,
                    token_usage(10, 0, 5, 0),
                ),
            );
            next_tick().await;

            let body = text(&container);
            assert!(
                !body.contains("Summary belongs to the tray"),
                "{tool_name} card must not render the summary: {body}"
            );
            assert!(
                !body.contains("output belongs to the tray"),
                "{tool_name} card must not render the output line: {body}"
            );
            assert!(
                !body.contains("7 tool calls"),
                "{tool_name} card must not render the stats line: {body}"
            );
            assert_eq!(
                count(&container, ".tool-live-agent-stats"),
                0,
                "{tool_name} card must not contain a stats line element"
            );
            assert_eq!(
                count(&container, ".tool-live-agent-summary"),
                0,
                "{tool_name} card must not contain a summary element"
            );
            assert!(
                body.contains("Worker"),
                "{tool_name} receipt still names the agent: {body}"
            );
        }
    }

    // ── Typed Tyde orchestration cards ──────────────────────────────────
    //
    // These exercise the whole `ToolCardView` shell, not just a renderer,
    // because the raw-JSON duplication they lock out was a property of the
    // shell: the agent-control card rendered and then *fell through* to the
    // generic body.

    fn typed_send_entry(
        message: &str,
        result: Option<ToolExecutionResult>,
        success: bool,
    ) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_send".to_owned(),
                tool_name: "tyde_send_agent_message".to_owned(),
                tool_type: ToolRequestType::TydeSendAgentMessage {
                    agent_id: AgentId("agent-sub".to_owned()),
                    message: message.to_owned(),
                },
            },
            result: result.map(|tool_result| ToolExecutionCompletedData {
                tool_call_id: "toolu_send".to_owned(),
                tool_name: "tyde_send_agent_message".to_owned(),
                tool_result,
                success,
                error: (!success).then(|| "send failed".to_owned()),
                normalization_failure: None,
            }),
        }
    }

    fn typed_await_entry() -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_agent_control".to_owned(),
                tool_name: "tyde_await_agents".to_owned(),
                tool_type: ToolRequestType::TydeAwaitAgents {
                    agent_ids: vec![AgentId("agent-sub".to_owned())],
                },
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_agent_control".to_owned(),
                tool_name: "tyde_await_agents".to_owned(),
                tool_result: ToolExecutionResult::TydeAwaitAgents {
                    ready: vec![protocol::TydeAgentWaitStatus {
                        agent_id: AgentId("agent-sub".to_owned()),
                        status: protocol::AgentControlStatus::Idle,
                    }],
                    still_thinking: Vec::new(),
                },
                success: true,
                error: None,
                normalization_failure: None,
            }),
        }
    }

    /// Force the card's disclosure open. In `Summary` a completed, successful
    /// card is collapsed by default (true of every tool), so the body has to be
    /// opened before its contents can be asserted on.
    fn open_card(container: &HtmlElement) {
        let details = container
            .query_selector("details.tool-card")
            .expect("query card")
            .expect("tool card present")
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .expect("details element");
        details.set_open(true);
        details
            .dispatch_event(&web_sys::Event::new("toggle").expect("toggle event"))
            .expect("dispatch toggle");
    }

    /// Regression lock for the screenshot's defect B. The await card's receipt
    /// and verdict are the complete presentation: no raw JSON below them, in
    /// any output mode — Full included. (The open action moved from a per-row
    /// "Open agent" button onto the receipt's named link when the live rows
    /// moved to the In-flight tray; the reachability contract is unchanged.)
    #[wasm_bindgen_test]
    async fn await_card_renders_no_raw_json_in_any_mode() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let (container, state) = mount_card(
                typed_await_entry(),
                Some(agent_control_progress_data(AgentControlProgressKind::Await)),
            );
            state.tool_output_mode.set(mode);
            state.agents.update(|agents| {
                agents.push(agent_info("agent-sub", "Awaited Worker", true));
            });
            next_tick().await;
            open_card(&container);
            next_tick().await;

            assert_eq!(
                count(&container, "pre.tool-raw-args"),
                0,
                "await card shows no raw args in {mode:?}"
            );
            assert_eq!(
                count(&container, "pre.tool-raw-result"),
                0,
                "await card shows no raw result in {mode:?}"
            );

            let body = text(&container);
            assert!(
                !body.contains("Result JSON"),
                "no Result JSON panel in {mode:?}: {body}"
            );
            // The useful card survives, intact.
            assert!(
                body.contains("Awaited Worker"),
                "agent name still shown in {mode:?}: {body}"
            );
            assert_eq!(
                count(&container, ".tool-live-agent-receipt-link"),
                1,
                "open-agent action still reachable via the receipt link in {mode:?}"
            );
        }
    }

    /// Regression lock for the screenshot's defect A, through the full shell: the
    /// message renders as Markdown exactly once, the recipient is named by their
    /// human name in the header (so a collapsed card still answers "who was
    /// messaged"), and no JSON envelope appears.
    #[wasm_bindgen_test]
    async fn send_message_card_renders_markdown_and_names_recipient() {
        let (container, state) = mount_card(
            typed_send_entry(
                "## Fixing exact rerun behavior\n\n- check `mock.rs`\n- then rerun",
                Some(ToolExecutionResult::TydeSendAgentMessage),
                true,
            ),
            None,
        );
        state.agents.update(|agents| {
            agents.push(agent_info("agent-sub", "Agent state bugs", true));
        });
        next_tick().await;

        let header = container
            .query_selector(".tool-card-detail")
            .expect("query header detail")
            .expect("send card names its recipient in the header")
            .text_content()
            .unwrap_or_default();
        assert_eq!(
            header, "Agent state bugs",
            "collapsed header names the recipient by their live human name"
        );

        open_card(&container);
        next_tick().await;

        assert_eq!(count(&container, "h2"), 1, "message renders as Markdown");
        assert_eq!(count(&container, "li"), 2, "bullets render as list items");
        assert_eq!(count(&container, "pre.tool-raw-args"), 0, "no raw args");
        assert_eq!(count(&container, "pre.tool-raw-result"), 0, "no raw result");

        let body = text(&container);
        assert_eq!(
            body.matches("Fixing exact rerun behavior").count(),
            1,
            "the message appears exactly once: {body}"
        );
        assert!(
            !body.contains("agent_id"),
            "no JSON keys in the default view: {body}"
        );
    }

    /// A failed orchestration call still renders its full error body — a pretty
    /// card must never hide a failure.
    #[wasm_bindgen_test]
    async fn send_message_failure_renders_error_body() {
        let (container, _state) = mount_card(
            typed_send_entry(
                "please pick this up",
                Some(ToolExecutionResult::Error {
                    short_message: "unknown agent_id".to_owned(),
                    detailed_message: "agent-sub is not a direct child".to_owned(),
                }),
                false,
            ),
            None,
        );
        next_tick().await;

        let body = text(&container);
        assert_eq!(tool_header_status(&container), "Failed");
        assert!(
            body.contains("agent-sub is not a direct child"),
            "the error detail stays visible: {body}"
        );
        assert!(
            !body.contains("please pick this up"),
            "a failed send renders the error, not the Markdown card: {body}"
        );
    }

    fn malformed_entry(result: ToolExecutionResult, success: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_send_3".to_owned(),
                tool_name: "mcp__tyde-agent-control__tyde_send_agent_message".to_owned(),
                tool_type: ToolRequestType::Other {
                    args: json!({
                        "tool": "mcp__tyde-agent-control__tyde_send_agent_message",
                        "arguments": { "agent_id": "", "message": "" },
                    }),
                },
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_send_3".to_owned(),
                tool_name: "mcp__tyde-agent-control__tyde_send_agent_message".to_owned(),
                tool_result: result,
                success,
                error: None,
                normalization_failure: Some(ToolExecutionNormalizationFailure::CanonicalRequest),
            }),
        }
    }

    #[wasm_bindgen_test]
    async fn malformed_drift_forces_open_failed_accessible_shell() {
        let (container, _state) = mount_card(
            malformed_entry(ToolExecutionResult::TydeSendAgentMessage, true),
            None,
        );
        next_tick().await;

        let outer = container
            .query_selector("details.tool-card")
            .unwrap()
            .expect("outer tool disclosure")
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .expect("details element");
        assert!(outer.open(), "malformed drift is visibly open");
        assert_eq!(tool_header_status(&container), "Failed");
        assert_eq!(
            outer.get_attribute("aria-label").as_deref(),
            Some("Tool failed: canonical data could not be normalized")
        );
        let alert = container
            .query_selector(".tool-typed-mismatch")
            .unwrap()
            .expect("normalization alert");
        assert_eq!(alert.get_attribute("role").as_deref(), Some("alert"));

        let nested = container
            .query_selector("details.tool-malformed-payload")
            .unwrap()
            .expect("sanitized nested disclosure")
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .expect("details element");
        assert!(!nested.open(), "only the sanitized payload stays closed");

        outer.set_open(false);
        outer
            .dispatch_event(&web_sys::Event::new("toggle").unwrap())
            .unwrap();
        next_tick().await;
        assert!(outer.open(), "malformed outer detail cannot be collapsed");
    }

    #[wasm_bindgen_test]
    async fn result_only_marker_fails_shell_without_request_diagnostic() {
        let mut entry = typed_send_entry(
            "message remains semantic",
            Some(ToolExecutionResult::TydeSendAgentMessage),
            true,
        );
        entry.result.as_mut().unwrap().normalization_failure =
            Some(ToolExecutionNormalizationFailure::CanonicalResult);
        let (container, _state) = mount_card(entry, None);
        next_tick().await;

        let outer = container
            .query_selector("details.tool-card")
            .unwrap()
            .expect("tool shell")
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .expect("details element");
        assert!(outer.open());
        assert_eq!(tool_header_status(&container), "Failed");
        assert!(text(&container).contains("result normalization failed"));
        assert_eq!(count(&container, ".tool-typed-mismatch[role='alert']"), 1);
        assert_eq!(count(&container, ".tool-malformed-payload"), 0);
    }

    #[wasm_bindgen_test]
    async fn combined_marker_keeps_request_diagnostic_and_combined_header() {
        let mut entry = malformed_entry(ToolExecutionResult::Other { result: json!({}) }, true);
        entry.result.as_mut().unwrap().normalization_failure =
            Some(ToolExecutionNormalizationFailure::CanonicalRequestAndResult);
        let (container, _state) = mount_card(entry, None);
        next_tick().await;

        assert_eq!(tool_header_status(&container), "Failed");
        assert!(text(&container).contains("request and result normalization failed"));
        assert_eq!(count(&container, ".tool-malformed-payload"), 1);
    }

    #[wasm_bindgen_test]
    async fn normalization_error_completion_keeps_sanitized_request() {
        let mut entry = malformed_entry(
            ToolExecutionResult::Error {
                short_message: "worker launch failed".to_owned(),
                detailed_message: "request rejected before execution".to_owned(),
            },
            false,
        );
        let ToolRequestType::Other { args } = &mut entry.request.tool_type else {
            unreachable!();
        };
        args["arguments"]["OPENAI_API_KEY"] = json!("sk-never-render");

        let (container, _state) = mount_card(entry, None);
        next_tick().await;

        assert_eq!(tool_header_status(&container), "Failed");
        let text = text(&container);
        assert!(text.contains("Sanitized raw request"));
        assert!(text.contains("OPENAI_API_KEY") && text.contains("[REDACTED]"));
        assert!(!text.contains("sk-never-render"));
    }

    #[wasm_bindgen_test]
    async fn matching_error_prose_without_marker_does_not_trigger_diagnostic() {
        let mut entry = completed_other_request("toolu_spawn_error", "tyde_spawn_agent");
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_spawn_error".to_owned(),
            tool_name: "tyde_spawn_agent".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "worker launch failed".to_owned(),
                detailed_message: "process exited before ready".to_owned(),
            },
            success: false,
            error: Some("Failed to normalize canonical tool request".to_owned()),
            normalization_failure: None,
        });
        let (container, _state) = mount_card(entry, None);
        next_tick().await;

        assert_eq!(count(&container, ".tool-malformed-payload"), 0);
        assert_eq!(count(&container, ".tool-card-malformed"), 0);
        assert_eq!(tool_header_status(&container), "Failed");
        assert!(text(&container).contains("process exited before ready"));
    }

    /// The `tyde_spawn_agent` prompt must stay visible.
    ///
    /// Spawn is deliberately *not* typed in this change, so it still routes to
    /// the generic `Other` renderer and its prompt still shows exactly as it did
    /// before. This test is the guard against a future "simplification" that
    /// turns the orchestration rule into a blanket "agent-control cards hide
    /// raw" — which would silently delete the spawn prompt, the one place the
    /// task brief is visible at all.
    #[wasm_bindgen_test]
    async fn spawn_card_keeps_prompt_visible() {
        const PROMPT: &str = "Implement the typed orchestration cards and lock them with tests";
        let mut entry = completed_other_request("toolu_agent_control", "tyde_spawn_agent");
        entry.request.tool_type = ToolRequestType::Other {
            args: json!({ "prompt": PROMPT, "name": "Worker" }),
        };
        let (container, _state) = mount_card(
            entry,
            Some(agent_control_progress_data(AgentControlProgressKind::Spawn)),
        );
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains(PROMPT),
            "the spawn prompt must remain visible in the default view: {body}"
        );
        assert_eq!(
            count(&container, "pre.tool-raw-args"),
            1,
            "spawn still routes to the generic renderer, unchanged"
        );
    }

    // `await_stats_are_scoped_to_owning_host` moved with the stats line it
    // pinned: the host-scoping contract is asserted on the tray rows in
    // `inflight_tray::stats_are_scoped_to_owning_host`, and the card side —
    // that no stats render here at all, from any host — is pinned by
    // `cards_render_no_live_agent_detail` above.
}
