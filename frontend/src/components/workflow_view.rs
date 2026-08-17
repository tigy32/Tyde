//! Live view of a Claude Code workflow run.
//!
//! `WorkflowRunPanel` renders a `WorkflowRunState` snapshot — phase-grouped
//! agent rows plus an aggregate footer — and is shared between the inline
//! Workflow tool card and the dedicated workflow tab (`WorkflowView`),
//! which adds the workflow script section.

use leptos::prelude::*;
use protocol::{
    ToolProgressUpdate, WorkflowAgentState, WorkflowAgentStatus, WorkflowRunState,
    WorkflowRunStatus,
};

use crate::state::{ActiveAgentRef, AppState, ToolCallId};

pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub fn format_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub fn run_status_label(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Running => "Running",
        WorkflowRunStatus::Completed => "Completed",
        WorkflowRunStatus::Failed => "Failed",
        WorkflowRunStatus::Unknown => "Unknown",
    }
}

fn agent_status_glyph(status: WorkflowAgentStatus) -> (&'static str, &'static str) {
    match status {
        WorkflowAgentStatus::Queued => ("\u{25cb}", "queued"),
        WorkflowAgentStatus::Running => ("\u{27f3}", "running"),
        WorkflowAgentStatus::Done => ("\u{2713}", "done"),
        WorkflowAgentStatus::Error => ("\u{2717}", "error"),
        WorkflowAgentStatus::Unknown => ("\u{00b7}", "unknown"),
    }
}

fn agent_row(agent: &WorkflowAgentState) -> impl IntoView + use<> {
    let (glyph, status_class) = agent_status_glyph(agent.state);
    let label = agent.label.clone();
    let model = agent.model.clone();
    let meta = {
        let mut parts: Vec<String> = Vec::new();
        if agent.tokens > 0 {
            parts.push(format!("{} tokens", format_tokens(agent.tokens)));
        }
        if agent.tool_calls > 0 {
            parts.push(format!("{} tool calls", agent.tool_calls));
        }
        if agent.duration_ms > 0 {
            parts.push(format_duration_ms(agent.duration_ms));
        }
        if agent.attempt > 1 {
            parts.push(format!("attempt {}", agent.attempt));
        }
        parts.join(" \u{b7} ")
    };
    let result_preview = agent
        .result_preview
        .clone()
        .filter(|preview| !preview.trim().is_empty());

    view! {
        <div class="workflow-agent-row">
            <span class=format!(
                "workflow-agent-glyph {status_class}",
            )>{glyph}</span>
            <span class="workflow-agent-label">{label}</span>
            {model
                .map(|model| view! { <span class="workflow-agent-model">{model}</span> })}
            <span class="workflow-agent-meta">{meta}</span>
            {result_preview
                .map(|preview| {
                    let title = preview.clone();
                    view! { <span class="workflow-agent-result" title=title>{preview}</span> }
                })}
        </div>
    }
}

/// Phase-grouped agent rows plus aggregate footer for one run snapshot.
/// Renders nothing until the first snapshot arrives.
#[component]
pub fn WorkflowRunPanel(
    run: Signal<Option<WorkflowRunState>>,
    #[prop(default = false)] show_script: bool,
) -> impl IntoView {
    view! {
        <div class="workflow-run-panel">
            {move || {
                let snapshot = run.get()?;
                let mut sections: Vec<AnyView> = Vec::new();
                let mut current_phase: Option<String> = None;
                let mut phase_rows: Vec<AnyView> = Vec::new();

                let flush_phase =
                    |phase: &Option<String>, rows: &mut Vec<AnyView>, out: &mut Vec<AnyView>| {
                        if rows.is_empty() {
                            return;
                        }
                        let rows = std::mem::take(rows);
                        let title = phase.clone();
                        out.push(
                            view! {
                                <div class="workflow-phase">
                                    {title
                                        .map(|title| {
                                            view! { <div class="workflow-phase-title">{title}</div> }
                                        })}
                                    {rows}
                                </div>
                            }
                            .into_any(),
                        );
                    };

                for agent in &snapshot.agents {
                    if agent.phase_title != current_phase && !phase_rows.is_empty() {
                        flush_phase(&current_phase, &mut phase_rows, &mut sections);
                    }
                    current_phase = agent.phase_title.clone();
                    phase_rows.push(agent_row(agent).into_any());
                }
                flush_phase(&current_phase, &mut phase_rows, &mut sections);

                let done = snapshot
                    .agents
                    .iter()
                    .filter(|agent| agent.state == WorkflowAgentStatus::Done)
                    .count();
                let total = snapshot.agents.len();
                let mut footer_parts = vec![format!("{done} of {total} agents done")];
                if snapshot.tool_uses > 0 {
                    footer_parts.push(format!("{} tool uses", snapshot.tool_uses));
                }
                if snapshot.total_tokens > 0 {
                    footer_parts.push(format!("{} tokens", format_tokens(snapshot.total_tokens)));
                }
                if snapshot.duration_ms > 0 {
                    footer_parts.push(format_duration_ms(snapshot.duration_ms));
                }
                let footer = footer_parts.join(" \u{b7} ");
                let summary = snapshot
                    .summary
                    .clone()
                    .filter(|summary| !summary.trim().is_empty());
                let script = if show_script {
                    snapshot
                        .script
                        .clone()
                        .filter(|script| !script.trim().is_empty())
                } else {
                    None
                };

                Some(view! {
                    <div>
                        {sections}
                        <div class="workflow-run-footer">{footer}</div>
                        {summary
                            .map(|summary| {
                                view! { <div class="workflow-run-summary">{summary}</div> }
                            })}
                        {script
                            .map(|script| {
                                view! {
                                    <details class="workflow-script">
                                        <summary>"Workflow script"</summary>
                                        <pre class="workflow-script-source">{script}</pre>
                                    </details>
                                }
                            })}
                    </div>
                })
            }}
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn sample_agent(index: u64, label: &str, status: WorkflowAgentStatus) -> WorkflowAgentState {
        WorkflowAgentState {
            index,
            label: label.to_owned(),
            phase_title: Some("Probe".to_owned()),
            model: Some("claude-opus-4-8".to_owned()),
            state: status,
            tokens: 6539,
            tool_calls: 2,
            duration_ms: 1562,
            attempt: 1,
            prompt_preview: Some("Reply with hello".to_owned()),
            result_preview: matches!(status, WorkflowAgentStatus::Done).then(|| "hello".to_owned()),
        }
    }

    fn sample_run() -> WorkflowRunState {
        WorkflowRunState {
            workflow_name: "wfprobe".to_owned(),
            description: Some("Probe: two agents".to_owned()),
            script: Some("export const meta = {}".to_owned()),
            status: WorkflowRunStatus::Running,
            summary: None,
            total_tokens: 13078,
            tool_uses: 4,
            duration_ms: 65_000,
            agents: vec![
                sample_agent(1, "probe-1", WorkflowAgentStatus::Done),
                sample_agent(2, "probe-2", WorkflowAgentStatus::Running),
            ],
        }
    }

    #[wasm_bindgen_test]
    async fn panel_shows_agents_phase_and_aggregate_counts() {
        let run = sample_run();
        let container = mount(move || {
            let signal = Signal::derive(move || Some(run.clone()));
            view! { <WorkflowRunPanel run=signal /> }
        });
        next_tick().await;

        let body = text(&container);
        assert!(body.contains("Probe"), "phase title visible: {body}");
        assert!(body.contains("probe-1"), "agent label visible: {body}");
        assert!(body.contains("probe-2"), "agent label visible: {body}");
        assert!(body.contains("hello"), "result preview visible: {body}");
        assert!(
            body.contains("1 of 2 agents done"),
            "aggregate count visible: {body}"
        );
        assert!(body.contains("4 tool uses"), "tool uses visible: {body}");
        assert!(body.contains("13.1k tokens"), "tokens visible: {body}");
        assert!(body.contains("1m 05s"), "duration visible: {body}");
    }

    #[wasm_bindgen_test]
    async fn panel_updates_when_run_signal_changes() {
        let run = RwSignal::new(sample_run());
        let container = mount(move || {
            let signal = Signal::derive(move || Some(run.get()));
            view! { <WorkflowRunPanel run=signal /> }
        });
        next_tick().await;
        assert!(text(&container).contains("1 of 2 agents done"));

        run.update(|state| {
            state.agents[1].state = WorkflowAgentStatus::Done;
            state.status = WorkflowRunStatus::Completed;
            state.summary = Some("Dynamic workflow completed".to_owned());
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("2 of 2 agents done"),
            "count updated live: {body}"
        );
        assert!(
            body.contains("Dynamic workflow completed"),
            "summary visible after completion: {body}"
        );
    }
}

/// Dedicated tab view for a workflow run, opened from its tool card.
/// Reads the same `AppState::tool_progress` store the card reads.
#[component]
pub fn WorkflowView(agent_ref: ActiveAgentRef, tool_call_id: ToolCallId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let key = (agent_ref.agent_id.clone(), tool_call_id);

    let run: Signal<Option<WorkflowRunState>> = Signal::derive({
        let state = state.clone();
        move || {
            let signal = state.tool_progress.with(|map| map.get(&key).cloned())?;
            match signal.get().update {
                ToolProgressUpdate::Workflow(run) => Some(run),
                ToolProgressUpdate::SubAgent(_)
                | ToolProgressUpdate::AgentControl(_)
                | ToolProgressUpdate::Other { .. } => None,
            }
        }
    });

    view! {
        <div class="center-content-scroll workflow-view">
            {move || match run.get() {
                Some(snapshot) => {
                    let header = format!(
                        "Workflow: {} \u{b7} {}",
                        snapshot.workflow_name,
                        run_status_label(snapshot.status),
                    );
                    view! {
                        <div class="workflow-view-inner">
                            <h2 class="workflow-view-header">{header}</h2>
                            <WorkflowRunPanel run=run show_script=true />
                        </div>
                    }
                    .into_any()
                }
                None => view! {
                    <div class="workflow-view-empty">
                        "No live data for this workflow run."
                    </div>
                }
                .into_any(),
            }}
        </div>
    }
}
