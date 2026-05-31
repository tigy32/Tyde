//! Interactive renderer for Claude's typed `AskUserQuestion` tool call.
//!
//! Claude emits `AskUserQuestion` to ask the user one or more multiple-choice
//! questions (single- or multi-select), each with an optional free-text answer.
//! Rather than dumping the raw JSON, this card renders the questions as
//! selectable options plus a custom-text field and a Submit button.
//!
//! MVP answer flow: submitting composes the chosen answers into a plain message
//! and sends it through the normal `SendMessage` path, resuming Claude on the
//! next turn. There is no live stdin channel.

use std::sync::Arc;

use leptos::prelude::*;
use protocol::{
    AskUserQuestion, FrameKind, SendMessagePayload, StreamPath, ToolExecutionResult,
    ToolRequestType,
};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, ToolOutputMode};

/// Compose the chosen answers into the message sent back to the agent. One line
/// per answered question: `"{header}: {answers}"`, answers comma-joined.
/// Pure function so the message format is unit-tested without a DOM.
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

pub(crate) fn render(
    req: &ToolRequestType,
    _result: Option<&ToolExecutionResult>,
    _mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::AskUserQuestion { questions } = req else {
        unreachable!("ask_user_question::render dispatched on non-AskUserQuestion request");
    };

    view! { <AskUserQuestionCard questions=questions.clone() /> }.into_any()
}

#[derive(Clone, Copy)]
struct QuestionState {
    selected: RwSignal<Vec<usize>>,
    custom: RwSignal<String>,
}

#[component]
fn AskUserQuestionCard(questions: Vec<AskUserQuestion>) -> impl IntoView {
    let state = expect_context::<AppState>();

    let questions = Arc::new(questions);
    let states: Arc<Vec<QuestionState>> = Arc::new(
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
        <div class="ask-question-card">
            {question_views}
            <div class="ask-question-actions">
                <button
                    class="ask-question-submit"
                    prop:disabled=submit_disabled
                    on:click=on_submit
                >
                    {move || {
                        if submitted.get() {
                            "Answer sent"
                        } else if sending.get() {
                            "Sending..."
                        } else {
                            "Submit answer"
                        }
                    }}
                </button>
                <Show when=move || submitted.get()>
                    <span class="ask-question-sent-note" role="status">
                        "Sent — Claude will continue on the next turn."
                    </span>
                </Show>
                <Show when=move || send_error.get().is_some()>
                    <span class="ask-question-error-note" role="alert">
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
    let on_custom_input = move |ev: leptos::ev::Event| {
        if submitted.get_untracked() || sending.get_untracked() {
            return;
        }
        custom.set(event_target_value(&ev));
    };

    view! {
        <div class="ask-question">
            {(!header.is_empty()).then(|| view! {
                <div class="ask-question-header">{header}</div>
            })}
            <div class="ask-question-text">{prompt}</div>
            <div class="ask-question-options" class:multi=multi_select>
                {option_views}
            </div>
            <input
                class="ask-question-custom"
                r#type="text"
                placeholder="Or type your own answer"
                prop:disabled=move || submitted.get() || sending.get()
                on:input=on_custom_input
            />
        </div>
    }
    .into_any()
}

fn answer_target(state: &AppState) -> Result<(String, StreamPath), &'static str> {
    let Some(active) = state.active_agent.get_untracked() else {
        log::error!("ask_user_question: no active agent to answer");
        return Err("No active agent is available. Reopen the chat and try again.");
    };
    let stream = state.agents.get_untracked().iter().find_map(|a| {
        (a.host_id == active.host_id && a.agent_id == active.agent_id)
            .then(|| a.instance_stream.clone())
    });
    let Some(stream) = stream else {
        log::error!("ask_user_question: active agent has no instance stream");
        return Err("The active agent stream is not available yet. Try again after reconnecting.");
    };
    Ok((active.host_id, stream))
}

fn send_answer(
    host_id: String,
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
        };
        match send_frame(&host_id, stream, FrameKind::SendMessage, &payload).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::AskUserQuestionOption;

    fn opt(label: &str) -> AskUserQuestionOption {
        AskUserQuestionOption {
            label: label.to_owned(),
            description: None,
        }
    }

    fn question(
        header: Option<&str>,
        prompt: &str,
        labels: &[&str],
        multi: bool,
    ) -> AskUserQuestion {
        AskUserQuestion {
            id: None,
            question: prompt.to_owned(),
            header: header.map(str::to_owned),
            options: labels.iter().map(|l| opt(l)).collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn format_answer_single_select_uses_header() {
        let qs = vec![question(
            Some("Language"),
            "Which language?",
            &["Rust", "Python"],
            false,
        )];
        let responses = vec![(vec![0usize], String::new())];
        assert_eq!(format_answer(&qs, &responses), "Language: Rust");
    }

    #[test]
    fn format_answer_multi_select_joins_with_comma() {
        let qs = vec![question(
            Some("Language"),
            "Which?",
            &["Rust", "Python"],
            true,
        )];
        let responses = vec![(vec![0usize, 1usize], String::new())];
        assert_eq!(format_answer(&qs, &responses), "Language: Rust, Python");
    }

    #[test]
    fn format_answer_appends_custom_text() {
        let qs = vec![question(Some("Language"), "Which?", &["Rust"], false)];
        let responses = vec![(vec![0usize], "  Go  ".to_owned())];
        assert_eq!(format_answer(&qs, &responses), "Language: Rust, Go");
    }

    #[test]
    fn format_answer_custom_only() {
        let qs = vec![question(Some("Language"), "Which?", &["Rust"], false)];
        let responses = vec![(vec![], "Zig".to_owned())];
        assert_eq!(format_answer(&qs, &responses), "Language: Zig");
    }

    #[test]
    fn format_answer_falls_back_to_question_without_header() {
        let qs = vec![question(None, "Which language?", &["Rust"], false)];
        let responses = vec![(vec![0usize], String::new())];
        assert_eq!(format_answer(&qs, &responses), "Which language?: Rust");
    }

    #[test]
    fn format_answer_multiple_questions_one_line_each() {
        let qs = vec![
            question(Some("Language"), "Lang?", &["Rust"], false),
            question(Some("Framework"), "Framework?", &["Leptos", "Axum"], true),
        ];
        let responses = vec![
            (vec![0usize], String::new()),
            (vec![0usize, 1usize], String::new()),
        ];
        assert_eq!(
            format_answer(&qs, &responses),
            "Language: Rust\nFramework: Leptos, Axum"
        );
    }

    #[test]
    fn format_answer_skips_unanswered_questions() {
        let qs = vec![
            question(Some("Language"), "Lang?", &["Rust"], false),
            question(Some("Framework"), "Framework?", &["Leptos"], false),
        ];
        let responses = vec![(vec![0usize], String::new()), (vec![], String::new())];
        assert_eq!(format_answer(&qs, &responses), "Language: Rust");
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::{ToolCardView, test_utils::*};
    use crate::state::{ActiveAgentRef, AgentInfo, AppState, TabContent, ToolRequestEntry};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, AskUserQuestionOption, BackendKind, StreamPath,
        ToolExecutionCompletedData, ToolRequest,
    };
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_test::*;
    use web_sys::{HtmlButtonElement, HtmlDetailsElement, HtmlElement, HtmlInputElement};

    wasm_bindgen_test_configure!(run_in_browser);

    fn opt(label: &str, desc: &str) -> AskUserQuestionOption {
        AskUserQuestionOption {
            label: label.to_owned(),
            description: (!desc.is_empty()).then(|| desc.to_owned()),
        }
    }

    fn single_select_req() -> ToolRequestType {
        ToolRequestType::AskUserQuestion {
            questions: vec![AskUserQuestion {
                id: None,
                question: "Which language?".to_owned(),
                header: Some("Language".to_owned()),
                options: vec![opt("Rust", "Systems lang"), opt("Python", "Scripting lang")],
                multi_select: false,
            }],
        }
    }

    fn multi_select_req() -> ToolRequestType {
        ToolRequestType::AskUserQuestion {
            questions: vec![AskUserQuestion {
                id: None,
                question: "Which frameworks?".to_owned(),
                header: Some("Frameworks".to_owned()),
                options: vec![opt("Leptos", ""), opt("Axum", ""), opt("Tauri", "")],
                multi_select: true,
            }],
        }
    }

    /// Mount a renderer fn with an `AppState` in context so the card can read
    /// the active agent on submit.
    fn mount_with_state<F>(view_fn: F) -> HtmlElement
    where
        F: FnOnce() -> AnyView + 'static,
    {
        mount_with_state_setup(|_| {}, view_fn)
    }

    fn mount_with_state_setup<F, S>(setup: S, view_fn: F) -> HtmlElement
    where
        F: FnOnce() -> AnyView + 'static,
        S: FnOnce(&AppState) + 'static,
    {
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            setup(&state);
            provide_context(state);
            view_fn()
        });
        handle.forget();
        container
    }

    fn configure_active_agent(state: &AppState) {
        let host_id = "host-1".to_owned();
        let agent_id = AgentId("agent-1".to_owned());
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                host_id: host_id.clone(),
                agent_id: agent_id.clone(),
                name: "Claude".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            });
        });
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef { host_id, agent_id }),
            "Claude".to_owned(),
            true,
        );
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
            .query_selector_all(".ask-question-option")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.get(i))
            .filter_map(|n| n.dyn_into::<HtmlButtonElement>().ok())
            .collect()
    }

    fn submit_button(container: &HtmlElement) -> HtmlButtonElement {
        container
            .query_selector(".ask-question-submit")
            .unwrap()
            .expect("submit button")
            .dyn_into::<HtmlButtonElement>()
            .unwrap()
    }

    fn custom_input(container: &HtmlElement) -> HtmlInputElement {
        container
            .query_selector(".ask-question-custom")
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
            .query_selector(".ask-question-sent-note")
            .unwrap()
            .is_some()
    }

    fn error_note_text(container: &HtmlElement) -> String {
        container
            .query_selector(".ask-question-error-note")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn renders_question_text_and_all_options() {
        let req = single_select_req();
        let container = mount_with_state(move || render(&req, None, ToolOutputMode::Summary));
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Which language?"), "question text: {body}");
        assert!(body.contains("Rust"), "option Rust: {body}");
        assert!(body.contains("Python"), "option Python: {body}");
        assert_eq!(option_buttons(&container).len(), 2);
    }

    #[wasm_bindgen_test]
    async fn completed_tool_card_stays_open_for_answering() {
        let entry = ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_ask".to_owned(),
                tool_name: "AskUserQuestion".to_owned(),
                tool_type: single_select_req(),
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_ask".to_owned(),
                tool_name: "AskUserQuestion".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({}),
                },
                success: true,
                error: None,
            }),
        };
        let container = mount_with_state(move || view! { <ToolCardView entry=entry /> }.into_any());
        next_tick().await;

        let details = container
            .query_selector("details.tool-card")
            .unwrap()
            .expect("tool card details")
            .dyn_into::<HtmlDetailsElement>()
            .unwrap();
        assert!(details.open(), "completed question card should stay open");
        assert!(text(&container).contains("Which language?"));
    }

    #[wasm_bindgen_test]
    async fn failed_completion_renders_non_interactive_card() {
        // A question that completes with `success=false` (and a non-`Error`
        // result, so the dispatcher's `Error` short-circuit does not fire) is no
        // longer answerable. Mirror mobile: render the normal completion body,
        // not the interactive card.
        let entry = ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_ask".to_owned(),
                tool_name: "AskUserQuestion".to_owned(),
                tool_type: single_select_req(),
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_ask".to_owned(),
                tool_name: "AskUserQuestion".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({ "error": "question failed" }),
                },
                success: false,
                error: Some("question failed".to_owned()),
            }),
        };
        let container = mount_with_state(move || view! { <ToolCardView entry=entry /> }.into_any());
        next_tick().await;

        assert!(
            container
                .query_selector(".ask-question-option")
                .unwrap()
                .is_none(),
            "failed question must not render interactive options"
        );
        assert!(
            container
                .query_selector(".ask-question-submit")
                .unwrap()
                .is_none(),
            "failed question must not render the submit button"
        );
        assert!(
            text(&container).contains("Failed"),
            "failed status should remain visible in the header"
        );
    }

    #[wasm_bindgen_test]
    async fn submit_disabled_until_an_option_is_chosen() {
        let req = single_select_req();
        let container = mount_with_state(move || render(&req, None, ToolOutputMode::Summary));
        next_tick().await;

        assert!(
            submit_button(&container).disabled(),
            "disabled before choice"
        );

        option_buttons(&container)[0].click();
        next_tick().await;

        assert!(
            !submit_button(&container).disabled(),
            "enabled after choice"
        );
    }

    #[wasm_bindgen_test]
    async fn single_select_replaces_previous_choice() {
        let req = single_select_req();
        let container = mount_with_state(move || render(&req, None, ToolOutputMode::Summary));
        next_tick().await;

        let buttons = option_buttons(&container);
        buttons[0].click();
        next_tick().await;
        assert!(is_pressed(&buttons[0]));
        assert!(!is_pressed(&buttons[1]));

        buttons[1].click();
        next_tick().await;
        assert!(!is_pressed(&buttons[0]), "first deselected");
        assert!(is_pressed(&buttons[1]), "second selected");
    }

    #[wasm_bindgen_test]
    async fn multi_select_keeps_multiple_choices() {
        let req = multi_select_req();
        let container = mount_with_state(move || render(&req, None, ToolOutputMode::Summary));
        next_tick().await;

        let buttons = option_buttons(&container);
        buttons[0].click();
        buttons[2].click();
        next_tick().await;
        assert!(is_pressed(&buttons[0]));
        assert!(!is_pressed(&buttons[1]));
        assert!(is_pressed(&buttons[2]));

        // Clicking an already-selected option toggles it off.
        buttons[0].click();
        next_tick().await;
        assert!(!is_pressed(&buttons[0]));
        assert!(is_pressed(&buttons[2]));
    }

    #[wasm_bindgen_test]
    async fn submit_without_active_agent_shows_retryable_error() {
        let req = single_select_req();
        let container = mount_with_state(move || render(&req, None, ToolOutputMode::Summary));
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
    async fn submit_waits_for_successful_send_before_showing_sent() {
        let calls = install_deferred_send_stub();
        let req = single_select_req();
        let container = mount_with_state_setup(configure_active_agent, move || {
            render(&req, None, ToolOutputMode::Summary)
        });
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
        assert!(text(&container).contains("Sending..."));
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
        assert!(text(&container).contains("Answer sent"));
        assert_answer_controls_disabled(&container);
    }

    #[wasm_bindgen_test]
    async fn send_failure_leaves_answer_retryable() {
        install_failing_send_stub();
        let req = single_select_req();
        let container = mount_with_state_setup(configure_active_agent, move || {
            render(&req, None, ToolOutputMode::Summary)
        });
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
}
