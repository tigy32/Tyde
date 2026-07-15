//! Semantic renderer for `tyde_await_agents`.
//!
//! The await card already had a purpose-built live presentation — the
//! **Awaiting agents** rows driven by server progress, with each agent's live
//! name, status, activity summary, token usage, and an **Open agent** action.
//! The raw JSON that used to sit beneath it was not merely redundant, it was
//! strictly *less* informative: `AwaitAgentsResult` carries `{agent_id, status}`
//! and nothing else, and everything else in the envelope was tycode transport
//! metadata. So this renderer emits **no raw JSON in any output mode**,
//! including `Full`. The conscious trade: `durationMs` leaves the UI for this
//! tool. It belongs in logs, not in the conversation.
//!
//! The live card stays the **sole roster**. This renderer must not list the
//! watched agents a second time: the rows directly above already name every one
//! of them, with live status, and repeating them under a "Ready" heading was
//! just the raw-JSON duplication in nicer clothes.
//!
//! What it adds instead is the one thing the live rows genuinely cannot show —
//! they always render *now* — namely the tool's verdict at the moment the wait
//! returned: how many agents were ready, how many were still thinking, and
//! which (if any) came back failed. That is a single concise line.

use leptos::prelude::*;
use protocol::{AgentControlStatus, ToolExecutionResult, ToolRequestType, TydeAgentWaitStatus};

use crate::state::{ActiveAgentRef, AppState, ToolOutputMode};

use super::agent_display_name;

pub(crate) fn render(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    _mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::TydeAwaitAgents { .. } = req else {
        unreachable!("tyde_await_agents::render dispatched on a non-await request");
    };

    // Still waiting: the live progress rows above are the whole presentation.
    // `Error` completions never reach here — the shell short-circuits them.
    match result {
        None => ().into_any(),
        Some(ToolExecutionResult::TydeAwaitAgents {
            ready,
            still_thinking,
        }) => view! {
            <AwaitVerdict
                agent_ref=agent_ref
                ready=ready.clone()
                still_thinking=still_thinking.clone()
            />
        }
        .into_any(),
        // A typed request whose completion is untyped means the request and
        // result normalizers disagree. Surface it rather than rendering an
        // empty, silently-wrong card.
        Some(other) => {
            log::error!("tyde_await_agents completed with an untyped result: {other:?}");
            view! {
                <div class="tool-typed-mismatch" role="alert">
                    "Unexpected result shape for tyde_await_agents. The awaited agents are listed above."
                </div>
            }
            .into_any()
        }
    }
}

/// One line: the counts, plus any agent that came back **failed** named
/// explicitly. Counts alone would bury a failure, and the live row beside it
/// shows the agent's status *now*, which may since have changed — so the verdict
/// names the failures rather than assuming the roster still tells that story.
#[component]
fn AwaitVerdict(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    ready: Vec<TydeAgentWaitStatus>,
    still_thinking: Vec<TydeAgentWaitStatus>,
) -> impl IntoView {
    if ready.is_empty() && still_thinking.is_empty() {
        return ().into_any();
    }

    let state = expect_context::<AppState>();
    let failed: Vec<TydeAgentWaitStatus> = ready
        .iter()
        .chain(still_thinking.iter())
        .filter(|agent| agent.status == AgentControlStatus::Failed)
        .cloned()
        .collect();

    let counts = {
        let mut parts = Vec::with_capacity(2);
        if !ready.is_empty() {
            parts.push(format!("{} ready", ready.len()));
        }
        if !still_thinking.is_empty() {
            parts.push(format!("{} still thinking", still_thinking.len()));
        }
        format!("Wait returned \u{b7} {}", parts.join(" \u{b7} "))
    };

    // Names are live server state — an agent can be renamed after the wait
    // returned — so this is resolved reactively, never snapshotted.
    let failed_line = Signal::derive({
        let state = state.clone();
        move || {
            if failed.is_empty() {
                return None;
            }
            let names = failed
                .iter()
                .map(|agent| agent_display_name(&state, agent_ref.get(), &agent.agent_id, None))
                .collect::<Vec<_>>()
                .join(", ");
            Some(format!("Failed: {names}"))
        }
    });

    view! {
        <div class="tool-await-result">
            <div class="tool-await-verdict">{counts}</div>
            {move || failed_line.get().map(|line| view! {
                <div class="tool-await-verdict-failed" role="alert">{line}</div>
            })}
        </div>
    }
    .into_any()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use crate::state::AgentInfo;
    use leptos::mount::mount_to;
    use protocol::{AgentId, AgentOrigin, BackendKind, StreamPath};
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn parent_ref() -> ActiveAgentRef {
        ActiveAgentRef {
            host_id: "host-1".to_owned(),
            agent_id: AgentId("agent-parent".to_owned()),
        }
    }

    fn await_req() -> ToolRequestType {
        ToolRequestType::TydeAwaitAgents {
            agent_ids: vec![AgentId("agent-a".to_owned()), AgentId("agent-b".to_owned())],
        }
    }

    fn wait_status(agent_id: &str, status: AgentControlStatus) -> TydeAgentWaitStatus {
        TydeAgentWaitStatus {
            agent_id: AgentId(agent_id.to_owned()),
            status,
        }
    }

    fn child_agent(agent_id: &str, name: &str) -> AgentInfo {
        AgentInfo {
            host_id: "host-1".to_owned(),
            agent_id: AgentId(agent_id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::AgentControl,
            backend_kind: BackendKind::Codex,
            workspace_roots: vec!["/tmp/work".to_owned()],
            project_id: None,
            parent_agent_id: Some(parent_ref().agent_id),
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 1,
            instance_stream: StreamPath(format!("/agents/{agent_id}")),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn mount_await(
        result: Option<ToolExecutionResult>,
        mode: ToolOutputMode,
        setup: impl FnOnce(&AppState) + 'static,
    ) -> HtmlElement {
        let state = AppState::new();
        setup(&state);
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            provide_context(state);
            let agent_ref = Signal::derive(|| Some(parent_ref()));
            render(agent_ref, &await_req(), result.as_ref(), mode)
        });
        handle.forget();
        container
    }

    fn completed() -> ToolExecutionResult {
        ToolExecutionResult::TydeAwaitAgents {
            ready: vec![wait_status("agent-a", AgentControlStatus::Idle)],
            still_thinking: vec![wait_status("agent-b", AgentControlStatus::Thinking)],
        }
    }

    /// Regression lock for the screenshot's second defect: the await card must
    /// carry no raw JSON in *any* output mode, Full included, while still
    /// reporting the wait's outcome.
    #[wasm_bindgen_test]
    async fn renders_no_raw_json_in_any_mode() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let container = mount_await(Some(completed()), mode, |state| {
                state.agents.update(|agents| {
                    agents.push(child_agent("agent-a", "Awaited Worker"));
                    agents.push(child_agent("agent-b", "Slow Worker"));
                });
            });
            next_tick().await;

            assert_eq!(
                count(&container, "pre.tool-raw-args"),
                0,
                "no raw args in {mode:?}"
            );
            assert_eq!(
                count(&container, "pre.tool-raw-result"),
                0,
                "no raw result in {mode:?}"
            );

            let body = text(&container);
            assert!(
                !body.contains("Result JSON"),
                "no Result JSON panel in {mode:?}: {body}"
            );
            assert!(
                !body.contains("agent_ids"),
                "no raw JSON keys in {mode:?}: {body}"
            );
            assert!(
                body.contains("1 ready") && body.contains("1 still thinking"),
                "the wait's outcome is reported in {mode:?}: {body}"
            );
        }
    }

    /// The live rows above already name every watched agent, with live status.
    /// The verdict must **not** list them again — a second roster is the same
    /// duplication the raw JSON was, just prettier. It reports counts instead:
    /// the one fact the live rows cannot carry, since they always render *now*.
    #[wasm_bindgen_test]
    async fn verdict_does_not_repeat_the_agent_roster() {
        let container = mount_await(Some(completed()), ToolOutputMode::Compact, |state| {
            state.agents.update(|agents| {
                agents.push(child_agent("agent-a", "Awaited Worker"));
                agents.push(child_agent("agent-b", "Slow Worker"));
            });
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            !body.contains("Awaited Worker") && !body.contains("Slow Worker"),
            "the verdict must not re-list agents the live rows already show: {body}"
        );
        assert_eq!(
            count(&container, ".tool-await-agent"),
            0,
            "no duplicate per-agent rows"
        );
        assert!(
            body.contains("Wait returned"),
            "the verdict reports the outcome of the wait: {body}"
        );
        assert!(
            body.contains("1 ready") && body.contains("1 still thinking"),
            "counts carry the verdict: {body}"
        );
    }

    /// A failure is the one case the verdict names an agent: counts alone would
    /// bury it, and the live row beside it shows the agent's status *now*, which
    /// may since have changed.
    #[wasm_bindgen_test]
    async fn failed_agent_is_named_in_the_verdict() {
        let result = ToolExecutionResult::TydeAwaitAgents {
            ready: vec![wait_status("agent-a", AgentControlStatus::Failed)],
            still_thinking: Vec::new(),
        };
        let container = mount_await(Some(result), ToolOutputMode::Summary, |state| {
            state
                .agents
                .update(|agents| agents.push(child_agent("agent-a", "Broken Worker")));
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Failed") && body.contains("Broken Worker"),
            "a failed agent is named, not just counted: {body}"
        );
        assert!(
            !body.contains("still thinking"),
            "an empty group contributes nothing: {body}"
        );
    }

    /// While the wait is pending there is no verdict yet — the live progress rows
    /// above own that state, and this renderer must not invent a second one.
    #[wasm_bindgen_test]
    async fn pending_await_renders_no_verdict() {
        let container = mount_await(None, ToolOutputMode::Full, |_| {});
        next_tick().await;

        assert_eq!(
            count(&container, ".tool-await-result"),
            0,
            "no verdict block before the wait returns"
        );
        assert_eq!(count(&container, "pre.tool-raw-args"), 0);
    }

    /// Protocol drift is surfaced, never silently swallowed.
    #[wasm_bindgen_test]
    async fn unexpected_result_shape_is_surfaced() {
        let container = mount_await(
            Some(ToolExecutionResult::Other {
                result: serde_json::json!({"ready": []}),
            }),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Unexpected result shape"),
            "a mismatched completion is visible: {body}"
        );
    }
}
