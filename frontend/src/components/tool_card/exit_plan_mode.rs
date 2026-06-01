//! Interactive renderer for Claude's `ExitPlanMode` tool call.
//!
//! Claude emits `ExitPlanMode` when it has finished planning and wants approval
//! before acting. The backend pauses the turn while the request is pending and
//! resumes once the user decides. Rather than dumping the raw plan JSON, this
//! card renders the plan text (and optional plan path) plus one-click Approve /
//! Reject controls. Reject carries optional feedback.
//!
//! The decision is sent back through the normal `SendMessage` path with a
//! [`SendMessageToolResponse::ExitPlanMode`] tool response — there is no
//! dedicated frame kind. Once the request completes (a result arrives) the card
//! drops its controls and renders the plan as a read-only summary, consistent
//! with the other tool cards.

use std::sync::Arc;

use leptos::prelude::*;
use protocol::{
    ExitPlanModeDecision, FrameKind, SendMessagePayload, SendMessageToolResponse, StreamPath,
    ToolExecutionResult, ToolRequestType,
};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, ToolOutputMode};

/// The reactive status signals an `ExitPlanModeCard` shares with its async send
/// task. Bundled into one `Copy` value so the decision/result of a submit can be
/// reported back without threading a long argument list through `send_decision`.
#[derive(Clone, Copy)]
struct DecisionStatus {
    decision_sent: RwSignal<Option<ExitPlanModeDecision>>,
    sending: RwSignal<bool>,
    send_error: RwSignal<Option<String>>,
}

pub(crate) fn render(
    tool_call_id: &str,
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    _mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::ExitPlanMode { plan, plan_path } = req else {
        unreachable!("exit_plan_mode::render dispatched on non-ExitPlanMode request");
    };

    view! {
        <ExitPlanModeCard
            tool_call_id=tool_call_id.to_owned()
            plan=plan.clone()
            plan_path=plan_path.clone()
            completed=result.is_some()
        />
    }
    .into_any()
}

#[component]
fn ExitPlanModeCard(
    tool_call_id: String,
    plan: Option<String>,
    plan_path: Option<String>,
    completed: bool,
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

    let tool_call_id = Arc::new(tool_call_id);

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
            let (host_id, stream) = match decision_target(&state) {
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

    let on_approve = {
        let submit = submit.clone();
        move |_| submit(ExitPlanModeDecision::Approve)
    };
    let on_reject = move |_| submit(ExitPlanModeDecision::Reject);

    let controls_disabled = move || decision_sent.get().is_some() || sending.get();

    // `completed` is a fixed prop for this card instance, so a plain Rust
    // conditional (rather than a reactive `<Show>`) decides whether to render
    // the interactive controls. That also lets the one-shot event handlers move
    // into the buttons without a `Fn` bound.
    let controls = (!completed).then(|| {
        view! {
            <div class="exit-plan-feedback-row">
                <textarea
                    class="exit-plan-feedback"
                    rows="2"
                    placeholder="Optional feedback (sent if you reject)"
                    prop:disabled=controls_disabled
                    on:input=move |ev| {
                        if decision_sent.get_untracked().is_some() || sending.get_untracked() {
                            return;
                        }
                        feedback.set(event_target_value(&ev));
                    }
                />
            </div>
            <div class="exit-plan-actions">
                <button
                    class="exit-plan-approve"
                    prop:disabled=controls_disabled
                    on:click=on_approve
                >
                    "Approve"
                </button>
                <button
                    class="exit-plan-reject"
                    prop:disabled=controls_disabled
                    on:click=on_reject
                >
                    "Reject"
                </button>
                <Show when=move || decision_sent.get().is_some()>
                    <span class="exit-plan-sent-note" role="status">
                        {move || match decision_sent.get() {
                            Some(ExitPlanModeDecision::Approve) => {
                                "Approved — Claude will continue."
                            }
                            Some(ExitPlanModeDecision::Reject) => {
                                "Rejected — Claude will revise."
                            }
                            None => "",
                        }}
                    </span>
                </Show>
                <Show when=move || sending.get() && decision_sent.get().is_none()>
                    <span class="exit-plan-sent-note" role="status">"Sending\u{2026}"</span>
                </Show>
                <Show when=move || send_error.get().is_some()>
                    <span class="exit-plan-error-note" role="alert">
                        {move || send_error.get().unwrap_or_default()}
                    </span>
                </Show>
            </div>
        }
    });

    view! {
        <div class="exit-plan-card">
            {plan_path.map(|path| view! {
                <div class="exit-plan-path">{format!("Plan: {path}")}</div>
            })}
            {plan
                .filter(|p| !p.trim().is_empty())
                .map(|plan| view! {
                    <div class="exit-plan-text">{plan}</div>
                })}
            {controls}
        </div>
    }
}

fn decision_target(state: &AppState) -> Result<(String, StreamPath), &'static str> {
    let Some(active) = state.active_agent.get_untracked() else {
        log::error!("exit_plan_mode: no active agent to respond to");
        return Err("No active agent is available. Reopen the chat and try again.");
    };
    let stream = state.agents.get_untracked().iter().find_map(|a| {
        (a.host_id == active.host_id && a.agent_id == active.agent_id)
            .then(|| a.instance_stream.clone())
    });
    let Some(stream) = stream else {
        log::error!("exit_plan_mode: active agent has no instance stream");
        return Err("The active agent stream is not available yet. Try again after reconnecting.");
    };
    Ok((active.host_id, stream))
}

fn send_decision(
    host_id: String,
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
        match send_frame(&host_id, stream, FrameKind::SendMessage, &payload).await {
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::{ToolCardView, test_utils::*};
    use crate::state::{ActiveAgentRef, AgentInfo, AppState, TabContent, ToolRequestEntry};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, StreamPath, ToolExecutionCompletedData, ToolRequest,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlButtonElement, HtmlElement, HtmlTextAreaElement};

    wasm_bindgen_test_configure!(run_in_browser);

    fn exit_plan_req() -> ToolRequestType {
        ToolRequestType::ExitPlanMode {
            plan: Some("Step 1: do the thing\nStep 2: verify it".to_owned()),
            plan_path: Some("docs/plan.md".to_owned()),
        }
    }

    fn completed_entry(success: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_type: exit_plan_req(),
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({ "decision": "approve" }),
                },
                success,
                error: (!success).then(|| "rejected".to_owned()),
            }),
        }
    }

    fn mount_with_state<F>(setup: impl FnOnce(&AppState) + 'static, view_fn: F) -> HtmlElement
    where
        F: FnOnce() -> AnyView + 'static,
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

    fn install_send_capture_stub() -> js_sys::Array {
        let code = r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
        "#;
        js_sys::eval(code)
            .expect("install send capture stub")
            .dyn_into::<js_sys::Array>()
            .expect("array")
    }

    fn approve_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector(".exit-plan-approve")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlButtonElement>().ok())
    }

    fn reject_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector(".exit-plan-reject")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlButtonElement>().ok())
    }

    fn feedback_area(container: &HtmlElement) -> HtmlTextAreaElement {
        container
            .query_selector(".exit-plan-feedback")
            .unwrap()
            .expect("feedback area")
            .dyn_into::<HtmlTextAreaElement>()
            .unwrap()
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

    fn install_error_capture() -> js_sys::Array {
        let code = r#"
            (function() {
                window.__captured_errors = [];
                window.addEventListener('error', function(e) {
                    window.__captured_errors.push(String(e.message || e.error || e));
                });
                var origErr = console.error.bind(console);
                console.error = function() {
                    try { window.__captured_errors.push(Array.from(arguments).map(String).join(' ')); } catch (_) {}
                    return origErr.apply(console, arguments);
                };
                return window.__captured_errors;
            })();
        "#;
        js_sys::eval(code)
            .expect("install error capture")
            .dyn_into::<js_sys::Array>()
            .expect("array")
    }

    fn captured_errors_text(errors: &js_sys::Array) -> String {
        (0..errors.length())
            .filter_map(|i| errors.get(i).as_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pending_entry() -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_type: exit_plan_req(),
            },
            result: None,
        }
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

    fn resolve_deferred_send() {
        let window = wasm_bindgen::JsValue::from(web_sys::window().unwrap());
        let resolver = js_sys::Reflect::get(
            &window,
            &wasm_bindgen::JsValue::from_str("__test_send_resolve"),
        )
        .expect("read resolver")
        .dyn_into::<js_sys::Function>()
        .expect("resolver function");
        resolver
            .call0(&wasm_bindgen::JsValue::NULL)
            .expect("resolve send");
    }

    /// Approve while the send is still in-flight, then let the backend result
    /// arrive (disposing the pending card) before the send resolves. The async
    /// task then writes its status signals on the disposed card. Guards against
    /// the "closure invoked recursively or after being dropped" class of error.
    #[wasm_bindgen_test]
    async fn no_closure_error_when_card_disposed_mid_send() {
        let errors = install_error_capture();
        // Deferred send: the decision stays in-flight so we can dispose the card
        // (result arrives) *before* the send resolves, then fire the setter.
        let _calls = install_deferred_send_stub();
        let entry_sig = ArcRwSignal::new(pending_entry());
        let container = {
            let entry_sig = entry_sig.clone();
            mount_with_state(configure_active_agent, move || {
                view! { {move || view! { <ToolCardView entry=entry_sig.get() /> }} }.into_any()
            })
        };
        next_tick().await;

        approve_button(&container).unwrap().click();
        next_tick().await;

        // Backend result arrives while the send is still pending: the pending
        // card (and its signals) is disposed.
        entry_sig.set(completed_entry(true));
        next_tick().await;

        // Now the in-flight send resolves and the async task calls
        // `decision_sent.set(...)` / `sending.set(false)` on the disposed card.
        resolve_deferred_send();
        next_tick().await;

        let errs = captured_errors_text(&errors);
        assert!(
            !errs.contains("recursively") && !errs.contains("after being dropped"),
            "setter after disposal produced closure error: {errs}"
        );
    }

    /// Exercise the full lifecycle through the production `ToolCardView` shell
    /// (reactive entry signal, streaming churn, feedback input, approve, then
    /// completion) and assert no closure-lifetime errors surface at any step.
    #[wasm_bindgen_test]
    async fn no_closure_error_through_render_approve_and_completion() {
        let errors = install_error_capture();
        let calls = install_send_capture_stub();
        // Mount through the full ToolCardView shell, behind a reactive entry
        // signal, mirroring StreamingToolCardView in production. The pending →
        // completed transition recreates the card and is where closure-lifetime
        // bugs surface.
        let entry_sig = ArcRwSignal::new(pending_entry());
        let container = {
            let entry_sig = entry_sig.clone();
            mount_with_state(configure_active_agent, move || {
                view! {
                    {move || view! { <ToolCardView entry=entry_sig.get() /> }}
                }
                .into_any()
            })
        };
        next_tick().await;

        let after_render = captured_errors_text(&errors);
        assert!(
            !after_render.contains("recursively") && !after_render.contains("after being dropped"),
            "render produced closure error: {after_render}"
        );

        // Simulate streaming churn: the entry signal updates repeatedly while
        // pending, recreating the whole ToolCardView each time.
        for _ in 0..3 {
            entry_sig.set(pending_entry());
            next_tick().await;
        }

        // Type feedback (textarea on:input) before deciding.
        if let Some(area) = container
            .query_selector(".exit-plan-feedback")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlTextAreaElement>().ok())
        {
            area.focus().ok();
            area.set_value("try another approach");
            area.dispatch_event(&web_sys::Event::new("input").unwrap())
                .unwrap();
            next_tick().await;
        }

        approve_button(&container).unwrap().click();
        next_tick().await;
        let _ = calls;

        // Simulate the backend result arriving, which recreates the card with
        // `completed=true` and disposes the pending card (textarea still focused).
        entry_sig.set(completed_entry(true));
        next_tick().await;

        let after_approve = captured_errors_text(&errors);
        assert!(
            !after_approve.contains("recursively")
                && !after_approve.contains("after being dropped"),
            "approve/completion produced closure error: {after_approve}"
        );
    }

    #[wasm_bindgen_test]
    async fn renders_plan_text_and_path_without_pre() {
        let container = mount_with_state(
            |_| {},
            || render("toolu_plan", &exit_plan_req(), None, ToolOutputMode::Full),
        );
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Step 1: do the thing"), "plan text: {body}");
        assert!(body.contains("Step 2: verify it"), "plan text: {body}");
        assert!(body.contains("docs/plan.md"), "plan path: {body}");
        // Readable plain-text block, not a horizontally scrolling <pre>.
        assert!(
            container
                .query_selector(".exit-plan-text")
                .unwrap()
                .is_some(),
            "plan should render in a wrapping text block"
        );
    }

    #[wasm_bindgen_test]
    async fn pending_shows_approve_and_reject() {
        let container = mount_with_state(
            |_| {},
            || render("toolu_plan", &exit_plan_req(), None, ToolOutputMode::Full),
        );
        next_tick().await;

        assert!(
            approve_button(&container).is_some(),
            "approve control present"
        );
        assert!(
            reject_button(&container).is_some(),
            "reject control present"
        );
    }

    #[wasm_bindgen_test]
    async fn completed_drops_active_controls() {
        let container = mount_with_state(
            |_| {},
            || view! { <ToolCardView entry=completed_entry(true) /> }.into_any(),
        );
        next_tick().await;

        assert!(
            approve_button(&container).is_none(),
            "completed plan must not keep an active approve control"
        );
        assert!(
            reject_button(&container).is_none(),
            "completed plan must not keep an active reject control"
        );
    }

    #[wasm_bindgen_test]
    async fn approve_sends_decision_one_click() {
        let calls = install_send_capture_stub();
        let container = mount_with_state(configure_active_agent, || {
            render("toolu_plan", &exit_plan_req(), None, ToolOutputMode::Full)
        });
        next_tick().await;

        approve_button(&container).unwrap().click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "one send for one click");
        let payload = last_send_payload(&calls);
        assert!(payload.contains("ExitPlanMode"), "payload: {payload}");
        assert!(
            payload.contains("\\\"decision\\\":\\\"approve\\\"") || payload.contains("approve"),
            "payload: {payload}"
        );
        assert!(
            payload.contains("toolu_plan"),
            "payload carries tool_call_id: {payload}"
        );
        assert!(
            approve_button(&container).unwrap().disabled(),
            "controls disabled after a decision is sent"
        );
    }

    #[wasm_bindgen_test]
    async fn reject_includes_feedback() {
        let calls = install_send_capture_stub();
        let container = mount_with_state(configure_active_agent, || {
            render("toolu_plan", &exit_plan_req(), None, ToolOutputMode::Full)
        });
        next_tick().await;

        let area = feedback_area(&container);
        area.set_value("please use a different approach");
        area.dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        reject_button(&container).unwrap().click();
        next_tick().await;

        let payload = last_send_payload(&calls);
        assert!(
            payload.contains("reject"),
            "payload should carry reject: {payload}"
        );
        assert!(
            payload.contains("please use a different approach"),
            "reject payload should carry feedback: {payload}"
        );
    }

    #[wasm_bindgen_test]
    async fn decision_without_active_agent_shows_retryable_error() {
        let container = mount_with_state(
            |_| {},
            || render("toolu_plan", &exit_plan_req(), None, ToolOutputMode::Full),
        );
        next_tick().await;

        approve_button(&container).unwrap().click();
        next_tick().await;

        let err = container
            .query_selector(".exit-plan-error-note")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default();
        assert!(err.contains("No active agent"), "error note: {err}");
        assert!(
            !approve_button(&container).unwrap().disabled(),
            "decision should remain retryable after a failure"
        );
    }
}
