use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{Button, ButtonSize, ButtonVariant};
use crate::state::{AppState, ToolOutputMode, ToolRequestEntry};

use protocol::{
    AskUserQuestion, ExitPlanModeDecision, FrameKind, SendMessagePayload, SendMessageToolResponse,
    StreamPath, ToolRequestType,
};

/// Renders a single tool request inside an assistant message.
///
/// Carries semantic state through the `data-mobile-test` selector
/// (`tool-card-running`, `tool-card-success`, `tool-card-failed`) so
/// tests don't need to guess at color or icon. Failed and running
/// cards always reveal their output detail; successful cards honor
/// the global `ToolOutputMode`.
///
/// Claude's typed `AskUserQuestion` tool is special-cased: instead of the raw
/// result it renders an interactive question card whose answer is sent back
/// through the normal `SendMessage` path (see [`AskUserQuestionCard`]).
#[component]
pub fn ToolCardView(entry: ToolRequestEntry) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tool_output_mode = state.tool_output_mode;

    let tool_name = entry.request.tool_name.clone();

    if let ToolRequestType::AskUserQuestion { questions } = &entry.request.tool_type
        && entry.result.as_ref().is_none_or(|result| result.success)
    {
        let questions = questions.clone();
        return view! {
            <div class="tool-card ask-question" data-mobile-test="tool-card-ask-question">
                <div class="tool-card-header">
                    <span class="tool-name">{tool_name}</span>
                </div>
                <AskUserQuestionCard questions=questions />
            </div>
        }
        .into_any();
    }

    // A pending ExitPlanMode awaits the user's approval: render the plan plus
    // Approve/Reject controls. Once the request succeeds the card renders the
    // plan read-only (no active controls), matching the desktop card. A failed
    // completion falls through to the generic completion body below.
    if let ToolRequestType::ExitPlanMode { plan, plan_path } = &entry.request.tool_type {
        let succeeded = entry.result.as_ref().map(|r| r.success);
        match succeeded {
            // Pending: interactive Approve/Reject card.
            None => {
                let plan = plan.clone();
                let plan_path = plan_path.clone();
                let tool_call_id = entry.request.tool_call_id.clone();
                return view! {
                    <div class="tool-card exit-plan" data-mobile-test="tool-card-exit-plan">
                        <div class="tool-card-header">
                            <span class="tool-name">{tool_name}</span>
                        </div>
                        <ExitPlanModeCard tool_call_id=tool_call_id plan=plan plan_path=plan_path />
                    </div>
                }
                .into_any();
            }
            // Approved/decided: show the plan read-only, no controls.
            Some(true) => {
                let plan = plan.clone();
                let plan_path = plan_path.clone();
                return view! {
                    <div
                        class="tool-card completed success exit-plan"
                        data-mobile-test="tool-card-exit-plan-done"
                        aria-label="Plan reviewed"
                    >
                        <div class="tool-card-header">
                            <span class="tool-status-icon" aria-hidden="true">"\u{2713}"</span>
                            <span class="tool-name">{tool_name}</span>
                        </div>
                        <div class="exit-plan-body exit-plan-readonly" data-mobile-test="exit-plan-readonly">
                            {plan_path.map(|path| view! {
                                <div class="exit-plan-path">{format!("Plan: {path}")}</div>
                            })}
                            {plan
                                .filter(|p| !p.trim().is_empty())
                                .map(|plan| view! {
                                    <div class="exit-plan-text" data-mobile-test="exit-plan-text">{plan}</div>
                                })}
                        </div>
                    </div>
                }
                .into_any();
            }
            // Failed: fall through to the generic failed-completion body.
            Some(false) => {}
        }
    }

    let is_completed = entry.result.is_some();
    let success = entry.result.as_ref().map(|r| r.success).unwrap_or(false);
    let result_summary = entry
        .result
        .as_ref()
        .map(|r| format!("{:?}", r.tool_result))
        .unwrap_or_default();

    let (status_class, status_icon, status_test, aria_label) = if is_completed {
        if success {
            (
                "completed success",
                "\u{2713}",
                "tool-card-success",
                "Tool completed successfully",
            )
        } else {
            (
                "completed failed",
                "\u{2717}",
                "tool-card-failed",
                "Tool failed",
            )
        }
    } else {
        (
            "running",
            "\u{25D4}",
            "tool-card-running",
            "Tool is running",
        )
    };

    // Failed/running cards always show their detail; otherwise honor the mode.
    let force_show = !is_completed || !success;

    view! {
        <div class=format!("tool-card {status_class}") data-mobile-test=status_test aria-label=aria_label>
            <div class="tool-card-header">
                <span class="tool-status-icon" aria-hidden="true">{status_icon}</span>
                <span class="tool-name">{tool_name}</span>
            </div>
            {
                let rs = result_summary.clone();
                let rs2 = result_summary.clone();
                let show = move || {
                    !rs.is_empty()
                        && (force_show || tool_output_mode.get() != ToolOutputMode::Summary)
                };
                view! {
                    <Show when=show>
                        <details class="tool-result" data-mobile-test="tool-card-result" prop:open=move || tool_output_mode.get() == ToolOutputMode::Full>
                            <summary>"Result"</summary>
                            <pre class="tool-output">{rs2.clone()}</pre>
                        </details>
                    </Show>
                }
            }
        </div>
    }
    .into_any()
}

// ── AskUserQuestion ─────────────────────────────────────────────────────
//
// Mirrors the desktop `tool_card::ask_user_question` renderer. The two
// frontends are separate crates with separate component trees, so the small
// view-model / format helpers are duplicated rather than shared.

fn format_answer(questions: &[AskUserQuestion], responses: &[(Vec<usize>, String)]) -> String {
    let mut lines = Vec::new();
    for (q, (selected, custom)) in questions.iter().zip(responses) {
        let mut parts: Vec<String> = selected
            .iter()
            .filter_map(|&idx| q.options.get(idx).map(|o| o.label.clone()))
            .collect();
        let custom = custom.trim();
        if !custom.is_empty() {
            parts.push(custom.to_owned());
        }
        if parts.is_empty() {
            continue;
        }
        let label = q
            .header
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| q.question.trim());
        lines.push(format!("{label}: {}", parts.join(", ")));
    }
    lines.join("\n")
}

#[derive(Clone, Copy)]
struct QuestionState {
    selected: RwSignal<Vec<usize>>,
    custom: RwSignal<String>,
}

#[component]
fn AskUserQuestionCard(questions: Vec<AskUserQuestion>) -> impl IntoView {
    let state = expect_context::<AppState>();

    let questions = std::sync::Arc::new(questions);
    let states: std::sync::Arc<Vec<QuestionState>> = std::sync::Arc::new(
        questions
            .iter()
            .map(|_| QuestionState {
                selected: RwSignal::new(Vec::new()),
                custom: RwSignal::new(String::new()),
            })
            .collect(),
    );
    let submitted = RwSignal::new(false);
    let sending = RwSignal::new(false);
    let send_error = RwSignal::new(None::<String>);

    let all_answered = {
        let states = states.clone();
        move || {
            states
                .iter()
                .all(|qs| !qs.selected.get().is_empty() || !qs.custom.get().trim().is_empty())
        }
    };

    let question_views = questions
        .iter()
        .enumerate()
        .map(|(idx, question)| render_question(question.clone(), states[idx], submitted, sending))
        .collect::<Vec<_>>();

    let on_submit = {
        let questions = questions.clone();
        let states = states.clone();
        move |_| {
            if submitted.get_untracked() || sending.get_untracked() {
                return;
            }
            send_error.set(None);
            let responses: Vec<(Vec<usize>, String)> = states
                .iter()
                .map(|qs| (qs.selected.get_untracked(), qs.custom.get_untracked()))
                .collect();
            let message = format_answer(&questions, &responses);
            if message.is_empty() {
                return;
            }
            let (host_id, stream) = match answer_target(&state) {
                Ok(target) => target,
                Err(message) => {
                    send_error.set(Some(message.to_owned()));
                    return;
                }
            };
            sending.set(true);
            send_answer(host_id, stream, message, submitted, sending, send_error);
        }
    };

    let submit_disabled = {
        let all_answered = all_answered.clone();
        move || submitted.get() || sending.get() || !all_answered()
    };

    view! {
        <div class="ask-question-body" data-mobile-test="ask-question-body">
            {question_views}
            <div class="ask-question-actions">
                <button
                    type="button"
                    class="ui-button ui-button-primary ui-button-compact"
                    data-mobile-test="ask-question-submit"
                    disabled=submit_disabled
                    on:click=on_submit
                >
                    <span class="ui-button-label">
                        {move || {
                            if submitted.get() {
                                "Answer sent"
                            } else if sending.get() {
                                "Sending..."
                            } else {
                                "Submit answer"
                            }
                        }}
                    </span>
                </button>
                <Show when=move || submitted.get()>
                    <span class="ask-question-sent-note" data-mobile-test="ask-question-sent" role="status">
                        "Sent — Claude will continue on the next turn."
                    </span>
                </Show>
                <Show when=move || send_error.get().is_some()>
                    <span class="ask-question-error-note" data-mobile-test="ask-question-error" role="alert">
                        {move || send_error.get().unwrap_or_default()}
                    </span>
                </Show>
            </div>
        </div>
    }
}

fn render_question(
    question: AskUserQuestion,
    qstate: QuestionState,
    submitted: RwSignal<bool>,
    sending: RwSignal<bool>,
) -> AnyView {
    let multi_select = question.multi_select;
    let header = question
        .header
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let prompt = question.question.clone();

    let option_views = question
        .options
        .iter()
        .enumerate()
        .map(|(idx, option)| {
            let selected = qstate.selected;
            let is_selected = move || selected.get().contains(&idx);
            let on_click = move |_| {
                if submitted.get_untracked() || sending.get_untracked() {
                    return;
                }
                selected.update(|current| {
                    if multi_select {
                        if let Some(pos) = current.iter().position(|&i| i == idx) {
                            current.remove(pos);
                        } else {
                            current.push(idx);
                        }
                    } else if current.first() == Some(&idx) {
                        current.clear();
                    } else {
                        current.clear();
                        current.push(idx);
                    }
                });
            };
            let label = option.label.clone();
            let description = option
                .description
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            view! {
                <button
                    class="ask-question-option"
                    data-mobile-test="ask-question-option"
                    class:selected=is_selected
                    prop:disabled=move || submitted.get() || sending.get()
                    aria-pressed=move || if is_selected() { "true" } else { "false" }
                    on:click=on_click
                >
                    <span class="ask-question-option-label">{label}</span>
                    {(!description.is_empty()).then(|| view! {
                        <span class="ask-question-option-desc">{description}</span>
                    })}
                </button>
            }
        })
        .collect::<Vec<_>>();

    let custom = qstate.custom;
    let on_custom_input = move |ev: web_sys::Event| {
        if submitted.get_untracked() || sending.get_untracked() {
            return;
        }
        custom.set(event_target_value(&ev));
    };

    view! {
        <div class="ask-question" data-mobile-test="ask-question">
            {(!header.is_empty()).then(|| view! {
                <div class="ask-question-header">{header}</div>
            })}
            <div class="ask-question-text">{prompt}</div>
            <div class="ask-question-options" class:multi=multi_select>
                {option_views}
            </div>
            <input
                class="ask-question-custom"
                data-mobile-test="ask-question-custom"
                r#type="text"
                placeholder="Or type your own answer"
                prop:disabled=move || submitted.get() || sending.get()
                on:input=on_custom_input
            />
        </div>
    }
    .into_any()
}

fn answer_target(
    state: &AppState,
) -> Result<(crate::state::LocalHostId, StreamPath), &'static str> {
    let Some(active) = state.active_agent.get_untracked() else {
        log::error!("ask_user_question: no active agent to answer");
        return Err("No active agent is available. Reopen the chat and try again.");
    };
    let stream = state.agents.with_untracked(|agents| {
        agents.iter().find_map(|a| {
            (a.local_host_id == active.local_host_id && a.agent_id == active.agent_id)
                .then(|| a.instance_stream.clone())
        })
    });
    let Some(stream) = stream else {
        log::error!("ask_user_question: active agent has no instance stream");
        return Err("The active agent stream is not available yet. Try again after reconnecting.");
    };
    Ok((active.local_host_id, stream))
}

fn send_answer(
    host_id: crate::state::LocalHostId,
    stream: StreamPath,
    message: String,
    submitted: RwSignal<bool>,
    sending: RwSignal<bool>,
    send_error: RwSignal<Option<String>>,
) {
    spawn_local(async move {
        let payload = SendMessagePayload {
            message,
            images: None,
            origin: None,
            tool_response: None,
        };
        match crate::send::send_frame(&host_id, stream, FrameKind::SendMessage, &payload).await {
            Ok(()) => {
                submitted.set(true);
                send_error.set(None);
            }
            Err(error) => {
                log::error!("ask_user_question: failed to send answer: {error}");
                send_error.set(Some(format!("Could not send answer: {error}. Try again.")));
            }
        }
        sending.set(false);
    });
}

// ── ExitPlanMode ────────────────────────────────────────────────────────
//
// Mirrors the desktop `tool_card::exit_plan_mode` renderer. The two frontends
// are separate crates, so the small view-model is duplicated rather than
// shared. The decision rides the normal `SendMessage` path as a
// `SendMessageToolResponse::ExitPlanMode` tool response — no dedicated frame.

/// The reactive status signals an `ExitPlanModeCard` shares with its async send
/// task. Bundled into one `Copy` value so the decision/result of a submit can be
/// reported back without threading a long argument list through `send_decision`.
#[derive(Clone, Copy)]
struct DecisionStatus {
    decision_sent: RwSignal<Option<ExitPlanModeDecision>>,
    sending: RwSignal<bool>,
    send_error: RwSignal<Option<String>>,
}

#[component]
fn ExitPlanModeCard(
    tool_call_id: String,
    plan: Option<String>,
    plan_path: Option<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let decision_sent = RwSignal::new(None::<ExitPlanModeDecision>);
    let sending = RwSignal::new(false);
    let send_error = RwSignal::new(None::<String>);
    let feedback = RwSignal::new(String::new());
    let status = DecisionStatus {
        decision_sent,
        sending,
        send_error,
    };

    let tool_call_id = std::sync::Arc::new(tool_call_id);

    let submit = {
        let state = state.clone();
        let tool_call_id = tool_call_id.clone();
        move |decision: ExitPlanModeDecision| {
            if decision_sent.get_untracked().is_some() || sending.get_untracked() {
                return;
            }
            send_error.set(None);
            let trimmed = feedback.get_untracked().trim().to_owned();
            let feedback = match decision {
                ExitPlanModeDecision::Reject if !trimmed.is_empty() => Some(trimmed),
                _ => None,
            };
            let (host_id, stream) = match answer_target(&state) {
                Ok(target) => target,
                Err(message) => {
                    send_error.set(Some(message.to_owned()));
                    return;
                }
            };
            sending.set(true);
            send_decision(
                host_id,
                stream,
                (*tool_call_id).clone(),
                decision,
                feedback,
                status,
            );
        }
    };

    let on_approve = Callback::new({
        let submit = submit.clone();
        move |_: ()| submit(ExitPlanModeDecision::Approve)
    });
    let on_reject = Callback::new(move |_: ()| submit(ExitPlanModeDecision::Reject));

    let controls_disabled = move || decision_sent.get().is_some() || sending.get();

    view! {
        <div class="exit-plan-body" data-mobile-test="exit-plan-body">
            {plan_path.map(|path| view! {
                <div class="exit-plan-path">{format!("Plan: {path}")}</div>
            })}
            {plan
                .filter(|p| !p.trim().is_empty())
                .map(|plan| view! {
                    <div class="exit-plan-text" data-mobile-test="exit-plan-text">{plan}</div>
                })}
            <textarea
                class="exit-plan-feedback"
                data-mobile-test="exit-plan-feedback"
                rows="2"
                placeholder="Optional feedback (sent if you reject)"
                prop:disabled=controls_disabled
                on:input=move |ev: web_sys::Event| {
                    if decision_sent.get_untracked().is_some() || sending.get_untracked() {
                        return;
                    }
                    feedback.set(event_target_value(&ev));
                }
            />
            <div class="exit-plan-actions">
                <Button
                    label="Approve"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="exit-plan-approve"
                    disabled=Signal::derive(controls_disabled)
                    on_click=on_approve
                />
                <Button
                    label="Reject"
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Compact
                    data_mobile_test="exit-plan-reject"
                    disabled=Signal::derive(controls_disabled)
                    on_click=on_reject
                />
                <Show when=move || decision_sent.get().is_some()>
                    <span class="exit-plan-sent-note" data-mobile-test="exit-plan-sent" role="status">
                        {move || match decision_sent.get() {
                            Some(ExitPlanModeDecision::Approve) => "Approved — the agent will continue.",
                            Some(ExitPlanModeDecision::Reject) => "Rejected — the agent will revise.",
                            None => "",
                        }}
                    </span>
                </Show>
                <Show when=move || send_error.get().is_some()>
                    <span class="exit-plan-error-note" data-mobile-test="exit-plan-error" role="alert">
                        {move || send_error.get().unwrap_or_default()}
                    </span>
                </Show>
            </div>
        </div>
    }
}

fn send_decision(
    host_id: crate::state::LocalHostId,
    stream: StreamPath,
    tool_call_id: String,
    decision: ExitPlanModeDecision,
    feedback: Option<String>,
    status: DecisionStatus,
) {
    spawn_local(async move {
        let payload = SendMessagePayload {
            message: String::new(),
            images: None,
            origin: None,
            tool_response: Some(SendMessageToolResponse::ExitPlanMode {
                tool_call_id,
                decision,
                feedback,
            }),
        };
        match crate::send::send_frame(&host_id, stream, FrameKind::SendMessage, &payload).await {
            Ok(()) => {
                status.decision_sent.set(Some(decision));
                status.send_error.set(None);
            }
            Err(error) => {
                log::error!("exit_plan_mode: failed to send decision: {error}");
                status.send_error.set(Some(format!(
                    "Could not send decision: {error}. Try again."
                )));
            }
        }
        status.sending.set(false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::AskUserQuestionOption;

    fn question() -> AskUserQuestion {
        AskUserQuestion {
            id: None,
            question: "Which language?".to_owned(),
            header: Some("Language".to_owned()),
            options: vec![
                AskUserQuestionOption {
                    label: "Rust".to_owned(),
                    description: None,
                },
                AskUserQuestionOption {
                    label: "Python".to_owned(),
                    description: None,
                },
            ],
            multi_select: true,
        }
    }

    #[test]
    fn format_answer_joins_selected_and_custom() {
        let qs = vec![question()];
        let responses = vec![(vec![0usize, 1usize], "Go".to_owned())];
        assert_eq!(format_answer(&qs, &responses), "Language: Rust, Python, Go");
    }

    #[test]
    fn format_answer_falls_back_to_question_without_header() {
        let mut q = question();
        q.header = None;
        let responses = vec![(vec![0usize], String::new())];
        assert_eq!(format_answer(&[q], &responses), "Which language?: Rust");
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AgentInfo, AppState, LocalHostId, ToolRequestEntry};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, AskUserQuestion, AskUserQuestionOption, BackendKind, StreamPath,
        ToolExecutionCompletedData, ToolExecutionResult, ToolRequest, ToolRequestType,
    };
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_test::*;
    use web_sys::{HtmlButtonElement, HtmlElement, HtmlInputElement};

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
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

    fn opt(label: &str, desc: &str) -> AskUserQuestionOption {
        AskUserQuestionOption {
            label: label.to_owned(),
            description: (!desc.is_empty()).then(|| desc.to_owned()),
        }
    }

    fn ask_entry(multi_select: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_ask".to_owned(),
                tool_name: "AskUserQuestion".to_owned(),
                tool_type: ToolRequestType::AskUserQuestion {
                    questions: vec![AskUserQuestion {
                        id: None,
                        question: "Which language?".to_owned(),
                        header: Some("Language".to_owned()),
                        options: vec![
                            opt("Rust", "Systems lang"),
                            opt("Python", "Scripting lang"),
                            opt("Go", ""),
                        ],
                        multi_select,
                    }],
                },
            },
            result: None,
        }
    }

    fn mount_card(entry: ToolRequestEntry) -> HtmlElement {
        mount_card_with_setup(entry, |_| {})
    }

    fn mount_card_with_setup<S>(entry: ToolRequestEntry, setup: S) -> HtmlElement
    where
        S: FnOnce(&AppState) + 'static,
    {
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            setup(&state);
            provide_context(state);
            view! { <ToolCardView entry=entry.clone() /> }
        });
        handle.forget();
        container
    }

    fn configure_active_agent(state: &AppState) {
        let local_host_id = LocalHostId("host-1".to_owned());
        let agent_id = AgentId("agent-1".to_owned());
        state.active_agent.set(Some(ActiveAgentRef {
            local_host_id: local_host_id.clone(),
            agent_id: agent_id.clone(),
        }));
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                local_host_id,
                agent_id,
                name: "Claude".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            });
        });
    }

    fn install_deferred_send_stub() -> js_sys::Array {
        let code = r#"
            (function() {
                window.__test_send_calls = [];
                window.__test_send_resolve = null;
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    if (cmd === "send_host_line") {
                        return new Promise(function(resolve) {
                            window.__test_send_resolve = resolve;
                        });
                    }
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
        "#;
        js_sys::eval(code)
            .expect("install deferred send stub")
            .dyn_into::<js_sys::Array>()
            .expect("array")
    }

    fn install_failing_send_stub() -> js_sys::Array {
        let code = r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    if (cmd === "send_host_line") {
                        return Promise.reject("send failed");
                    }
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
        "#;
        js_sys::eval(code)
            .expect("install failing send stub")
            .dyn_into::<js_sys::Array>()
            .expect("array")
    }

    fn resolve_deferred_send() {
        let window = JsValue::from(web_sys::window().unwrap());
        let resolver = js_sys::Reflect::get(&window, &JsValue::from_str("__test_send_resolve"))
            .expect("read resolver")
            .dyn_into::<js_sys::Function>()
            .expect("resolver function");
        resolver.call0(&JsValue::NULL).expect("resolve send");
    }

    fn option_buttons(container: &HtmlElement) -> Vec<HtmlButtonElement> {
        let nodes = container
            .query_selector_all("[data-mobile-test='ask-question-option']")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.get(i))
            .filter_map(|n| n.dyn_into::<HtmlButtonElement>().ok())
            .collect()
    }

    fn submit_button(container: &HtmlElement) -> HtmlButtonElement {
        container
            .query_selector("[data-mobile-test='ask-question-submit']")
            .unwrap()
            .expect("submit button")
            .dyn_into::<HtmlButtonElement>()
            .unwrap()
    }

    fn custom_input(container: &HtmlElement) -> HtmlInputElement {
        container
            .query_selector("[data-mobile-test='ask-question-custom']")
            .unwrap()
            .expect("custom input")
            .dyn_into::<HtmlInputElement>()
            .unwrap()
    }

    fn assert_answer_controls_disabled(container: &HtmlElement) {
        assert!(
            option_buttons(container)
                .iter()
                .all(HtmlButtonElement::disabled),
            "option controls should be disabled"
        );
        assert!(
            custom_input(container).disabled(),
            "custom answer input should be disabled"
        );
    }

    fn is_pressed(btn: &HtmlButtonElement) -> bool {
        btn.get_attribute("aria-pressed").as_deref() == Some("true")
    }

    fn has_sent_note(container: &HtmlElement) -> bool {
        container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .is_some()
    }

    fn error_note_text(container: &HtmlElement) -> String {
        container
            .query_selector("[data-mobile-test='ask-question-error']")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn renders_interactive_card_instead_of_raw_result() {
        let container = mount_card(ask_entry(false));
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Which language?"), "question: {body}");
        assert!(body.contains("Rust") && body.contains("Python") && body.contains("Go"));
        assert_eq!(option_buttons(&container).len(), 3);
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-result']")
                .unwrap()
                .is_none(),
            "should not render the generic result block"
        );
    }

    #[wasm_bindgen_test]
    async fn successful_ask_user_question_still_renders_interactive_card() {
        let mut entry = ask_entry(false);
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_ask".to_owned(),
            tool_name: "AskUserQuestion".to_owned(),
            tool_result: ToolExecutionResult::Other {
                result: serde_json::json!({}),
            },
            success: true,
            error: None,
        });
        let container = mount_card(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-ask-question']")
                .unwrap()
                .is_some(),
            "successful AskUserQuestion should remain interactive"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='ask-question-body']")
                .unwrap()
                .is_some(),
            "interactive body should be visible"
        );
    }

    #[wasm_bindgen_test]
    async fn failed_ask_user_question_renders_error_completion() {
        let mut entry = ask_entry(false);
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_ask".to_owned(),
            tool_name: "AskUserQuestion".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "ask failed".to_owned(),
                detailed_message: "AskUserQuestion failed".to_owned(),
            },
            success: false,
            error: Some("ask failed".to_owned()),
        });
        let container = mount_card(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-failed']")
                .unwrap()
                .is_some(),
            "failed AskUserQuestion should render the failed completion card"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='ask-question-body']")
                .unwrap()
                .is_none(),
            "failed AskUserQuestion should not render the interactive body"
        );
    }

    #[wasm_bindgen_test]
    async fn submit_enables_after_selection() {
        let container = mount_card(ask_entry(false));
        next_tick().await;
        assert!(submit_button(&container).disabled());

        option_buttons(&container)[0].click();
        next_tick().await;
        assert!(!submit_button(&container).disabled());
    }

    #[wasm_bindgen_test]
    async fn multi_select_allows_multiple() {
        let container = mount_card(ask_entry(true));
        next_tick().await;

        let buttons = option_buttons(&container);
        buttons[0].click();
        buttons[2].click();
        next_tick().await;
        assert!(is_pressed(&buttons[0]));
        assert!(!is_pressed(&buttons[1]));
        assert!(is_pressed(&buttons[2]));
    }

    #[wasm_bindgen_test]
    async fn submit_waits_for_successful_send_before_showing_sent() {
        let calls = install_deferred_send_stub();
        let container = mount_card_with_setup(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "send_frame should be invoked once");
        assert!(
            submit_button(&container).disabled(),
            "disabled while sending"
        );
        assert_answer_controls_disabled(&container);
        assert!(
            !has_sent_note(&container),
            "not marked sent before send resolves"
        );

        resolve_deferred_send();
        next_tick().await;

        assert!(
            has_sent_note(&container),
            "sent note appears after send resolves"
        );
        assert_answer_controls_disabled(&container);
    }

    #[wasm_bindgen_test]
    async fn submit_without_active_agent_shows_retryable_error() {
        let container = mount_card(ask_entry(false));
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert!(!has_sent_note(&container));
        assert!(
            error_note_text(&container).contains("No active agent"),
            "missing active-agent note should be visible"
        );
        assert!(
            !submit_button(&container).disabled(),
            "answer should remain retryable"
        );
    }

    #[wasm_bindgen_test]
    async fn send_failure_leaves_answer_retryable() {
        install_failing_send_stub();
        let container = mount_card_with_setup(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert!(!has_sent_note(&container));
        assert!(
            error_note_text(&container).contains("Could not send answer"),
            "send failure note should be visible"
        );
        assert!(
            !submit_button(&container).disabled(),
            "answer should remain retryable after send failure"
        );
    }

    // ── ExitPlanMode ────────────────────────────────────────────────────

    fn exit_plan_entry(completed: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_type: ToolRequestType::ExitPlanMode {
                    plan: Some("Step 1: do the thing\nStep 2: verify it".to_owned()),
                    plan_path: Some("docs/plan.md".to_owned()),
                },
            },
            result: completed.then(|| ToolExecutionCompletedData {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({ "decision": "approve" }),
                },
                success: true,
                error: None,
            }),
        }
    }

    fn exit_approve_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector("[data-mobile-test='exit-plan-approve']")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlButtonElement>().ok())
    }

    fn exit_reject_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector("[data-mobile-test='exit-plan-reject']")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlButtonElement>().ok())
    }

    fn last_send_payload(calls: &js_sys::Array) -> String {
        let len = calls.length();
        assert!(len > 0, "expected at least one send call");
        let entry = calls
            .get(len - 1)
            .dyn_into::<js_sys::Array>()
            .expect("call entry");
        entry.get(1).as_string().unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn pending_exit_plan_renders_plan_and_controls() {
        let container = mount_card(exit_plan_entry(false));
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Step 1: do the thing"), "plan text: {body}");
        assert!(body.contains("docs/plan.md"), "plan path: {body}");
        assert!(exit_approve_button(&container).is_some(), "approve present");
        assert!(exit_reject_button(&container).is_some(), "reject present");
    }

    #[wasm_bindgen_test]
    async fn completed_exit_plan_drops_controls() {
        let container = mount_card(exit_plan_entry(true));
        next_tick().await;

        assert!(
            exit_approve_button(&container).is_none(),
            "completed plan must not keep active approve control"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-body']")
                .unwrap()
                .is_none(),
            "completed plan must not render the interactive body"
        );
    }

    #[wasm_bindgen_test]
    async fn completed_exit_plan_shows_plan_readonly() {
        let container = mount_card(exit_plan_entry(true));
        next_tick().await;

        // The plan and path remain visible, read-only, after completion.
        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Step 1: do the thing"), "plan text: {body}");
        assert!(body.contains("docs/plan.md"), "plan path: {body}");
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-readonly']")
                .unwrap()
                .is_some(),
            "completed plan should render the read-only summary"
        );
        // No interactive controls and no raw-JSON result dump.
        assert!(
            exit_approve_button(&container).is_none(),
            "read-only plan must not keep an approve control"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-feedback']")
                .unwrap()
                .is_none(),
            "read-only plan must not keep the feedback textarea"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-result']")
                .unwrap()
                .is_none(),
            "read-only plan must not dump the generic raw result"
        );
    }

    #[wasm_bindgen_test]
    async fn approve_sends_decision_and_disables() {
        let calls = install_deferred_send_stub();
        let container = mount_card_with_setup(exit_plan_entry(false), configure_active_agent);
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "one send for one click");
        let payload = last_send_payload(&calls);
        assert!(payload.contains("ExitPlanMode"), "payload: {payload}");
        assert!(
            payload.contains("approve"),
            "payload carries approve: {payload}"
        );
        assert!(
            payload.contains("toolu_plan"),
            "payload carries id: {payload}"
        );
        assert!(
            exit_approve_button(&container).unwrap().disabled(),
            "controls disabled while sending"
        );

        resolve_deferred_send();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-sent']")
                .unwrap()
                .is_some(),
            "sent note appears after send resolves"
        );
    }

    #[wasm_bindgen_test]
    async fn reject_includes_feedback() {
        let calls = install_deferred_send_stub();
        let container = mount_card_with_setup(exit_plan_entry(false), configure_active_agent);
        next_tick().await;

        let area = container
            .query_selector("[data-mobile-test='exit-plan-feedback']")
            .unwrap()
            .expect("feedback area")
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        area.set_value("use a different approach");
        area.dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        exit_reject_button(&container).unwrap().click();
        next_tick().await;

        let payload = last_send_payload(&calls);
        assert!(
            payload.contains("reject"),
            "payload carries reject: {payload}"
        );
        assert!(
            payload.contains("use a different approach"),
            "reject payload carries feedback: {payload}"
        );
    }

    #[wasm_bindgen_test]
    async fn decision_without_active_agent_shows_retryable_error() {
        let container = mount_card(exit_plan_entry(false));
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;

        let err = container
            .query_selector("[data-mobile-test='exit-plan-error']")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default();
        assert!(err.contains("No active agent"), "error note: {err}");
        assert!(
            !exit_approve_button(&container).unwrap().disabled(),
            "decision should remain retryable"
        );
    }
}
