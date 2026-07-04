//! Compact panel rendering Tycode `ChatEvent::Orchestration` progress.
//!
//! The server forwards Tycode's typed orchestration events (sub-agent
//! lifecycle, workflow phases, fan-out/worker progress, consensus/review
//! resolutions) verbatim; it does not aggregate them. This module folds the
//! per-agent event log ([`crate::state::OrchestrationRecord`]) into a small
//! presentation tree at render time — no aggregated state is cached, so live
//! and replayed history fold identically.
//!
//! These are Tycode-internal orchestration nodes, deliberately styled as their
//! own panel rather than surfaced as first-class Tyde agents.

use std::collections::HashMap;

use leptos::prelude::*;
use protocol::{
    OrchestrationAgentOrigin, OrchestrationOutcomeStatus, OrchestrationPayload,
    OrchestrationReviewVerdict, OrchestrationWorkflowPhase,
};

use crate::state::OrchestrationRecord;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrchStatus {
    /// Announced by a fan-out but not yet granted an execution slot.
    Pending,
    Running,
    Succeeded,
    Failed,
    Aborted,
}

impl OrchStatus {
    fn from_outcome(status: OrchestrationOutcomeStatus) -> Self {
        match status {
            OrchestrationOutcomeStatus::Succeeded => Self::Succeeded,
            OrchestrationOutcomeStatus::Failed => Self::Failed,
            OrchestrationOutcomeStatus::Aborted => Self::Aborted,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Pending => "\u{2022}",   // •
            Self::Running => "\u{27f3}",   // ⟳
            Self::Succeeded => "\u{2713}", // ✓
            Self::Failed => "\u{2717}",    // ✗
            Self::Aborted => "\u{2298}",   // ⊘
        }
    }

    fn css(self) -> &'static str {
        match self {
            Self::Pending => "orch-status-pending",
            Self::Running => "orch-status-running",
            Self::Succeeded => "orch-status-succeeded",
            Self::Failed => "orch-status-failed",
            Self::Aborted => "orch-status-aborted",
        }
    }

    /// A cancellation closes anything still in flight. Terminal outcomes keep
    /// their real status.
    fn abort_if_active(&mut self) {
        if matches!(self, Self::Pending | Self::Running) {
            *self = Self::Aborted;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrchWorker {
    /// Worker id, used to match `WorkerStarted`/`WorkerCompleted` to the slot
    /// announced in `FanOutStarted`; not rendered.
    pub id: String,
    pub label: String,
    pub status: OrchStatus,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrchNodeDetail {
    Agent {
        agent_type: String,
        task_preview: String,
        phase: Option<String>,
        result: Option<String>,
    },
    FanOut {
        total: usize,
        concurrency: usize,
        workers: Vec<OrchWorker>,
    },
    /// A one-line domain resolution (consensus round, plan selection, review
    /// verdict).
    Note { text: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrchNode {
    pub indent: usize,
    pub status: OrchStatus,
    pub detail: OrchNodeDetail,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrchestrationPanelModel {
    /// The root interactive agent's catalog type. The root never emits a
    /// terminal event, so it heads the panel rather than appearing as a status
    /// row that would spin forever.
    pub root_label: Option<String>,
    pub root_phase: Option<String>,
    pub root_status: OrchStatus,
    pub nodes: Vec<OrchNode>,
}

impl OrchestrationPanelModel {
    pub fn is_visible(&self) -> bool {
        self.root_label.is_some() || !self.nodes.is_empty()
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn phase_label(phase: &OrchestrationWorkflowPhase) -> String {
    use OrchestrationWorkflowPhase::*;
    match phase {
        Reviewing { round } | BuilderReviewing { round } => format!("Reviewing (round {round})"),
        Fixing { round } | BuilderFixing { round } | SwarmFixing { round } => {
            format!("Fixing (round {round})")
        }
        BuilderPlanning | SwarmPlanning => "Planning".to_owned(),
        BuilderImplementing | SwarmImplementing { .. } => "Implementing".to_owned(),
        SwarmPlanFanOut { models } => {
            format!("Planning ({} model{})", models.len(), plural(models.len()))
        }
        SwarmConsensus { round, candidates } => format!(
            "Consensus (round {round}, {} candidate{})",
            candidates.len(),
            plural(candidates.len())
        ),
        SwarmFanOut { .. } => "Fan-out".to_owned(),
        SwarmIntegration { round, models } => format!(
            "Integration (round {round}, {} model{})",
            models.len(),
            plural(models.len())
        ),
    }
}

fn set_worker(
    nodes: &mut [OrchNode],
    fanout_node: &HashMap<String, usize>,
    fanout_id: &str,
    worker_id: &str,
    label: &str,
    status: OrchStatus,
    summary: Option<String>,
) {
    let Some(&index) = fanout_node.get(fanout_id) else {
        return;
    };
    let OrchNodeDetail::FanOut { workers, .. } = &mut nodes[index].detail else {
        return;
    };
    if let Some(worker) = workers.iter_mut().find(|w| w.id == worker_id) {
        worker.status = status;
        if summary.is_some() {
            worker.summary = summary;
        }
    } else {
        workers.push(OrchWorker {
            id: worker_id.to_owned(),
            label: label.to_owned(),
            status,
            summary,
        });
    }
}

fn push_note(
    nodes: &mut Vec<OrchNode>,
    owner_id: &str,
    agent_depth: &HashMap<String, usize>,
    text: String,
    status: OrchStatus,
) {
    let owner_depth = agent_depth.get(owner_id).copied().unwrap_or(1);
    nodes.push(OrchNode {
        indent: (owner_depth + 1).saturating_sub(2),
        status,
        detail: OrchNodeDetail::Note { text },
    });
}

/// Fold an ordered orchestration log into the panel model. Pure and
/// order-sensitive: the same records produce the same tree whether they came
/// from the live stream or a history replay.
pub fn build_orchestration_panel(records: &[OrchestrationRecord]) -> OrchestrationPanelModel {
    let mut root_label: Option<String> = None;
    let mut root_phase: Option<String> = None;
    let mut root_status = OrchStatus::Running;
    let mut root_id: Option<String> = None;

    let mut nodes: Vec<OrchNode> = Vec::new();
    let mut agent_node: HashMap<String, usize> = HashMap::new();
    let mut fanout_node: HashMap<String, usize> = HashMap::new();
    let mut agent_depth: HashMap<String, usize> = HashMap::new();

    for record in records {
        let ev = match record {
            OrchestrationRecord::Cancelled => {
                root_status.abort_if_active();
                for node in nodes.iter_mut() {
                    node.status.abort_if_active();
                    if let OrchNodeDetail::FanOut { workers, .. } = &mut node.detail {
                        for worker in workers.iter_mut() {
                            worker.status.abort_if_active();
                        }
                    }
                }
                continue;
            }
            OrchestrationRecord::Event(ev) => ev,
        };

        let owner_id = ev.agent_id.to_string();
        let owner_type = ev.agent_type.to_string();
        match &ev.payload {
            OrchestrationPayload::AgentStarted {
                origin,
                task_preview,
                depth,
                ..
            } => {
                agent_depth.insert(owner_id.clone(), *depth);
                let is_root = matches!(origin, OrchestrationAgentOrigin::Root) || *depth <= 1;
                if is_root {
                    root_id = Some(owner_id.clone());
                    root_label = Some(owner_type);
                } else if !agent_node.contains_key(&owner_id) {
                    agent_node.insert(owner_id.clone(), nodes.len());
                    nodes.push(OrchNode {
                        indent: depth.saturating_sub(2),
                        status: OrchStatus::Running,
                        detail: OrchNodeDetail::Agent {
                            agent_type: owner_type,
                            task_preview: task_preview.clone(),
                            phase: None,
                            result: None,
                        },
                    });
                }
            }
            OrchestrationPayload::AgentCompleted { status, result } => {
                if let Some(&index) = agent_node.get(&owner_id) {
                    nodes[index].status = OrchStatus::from_outcome(*status);
                    if let OrchNodeDetail::Agent { result: slot, .. } = &mut nodes[index].detail
                        && !result.trim().is_empty()
                    {
                        *slot = Some(result.clone());
                    }
                } else if root_id.as_deref() == Some(owner_id.as_str()) {
                    root_status = OrchStatus::from_outcome(*status);
                }
            }
            OrchestrationPayload::PhaseChanged { phase } => {
                let label = phase_label(phase);
                if let Some(&index) = agent_node.get(&owner_id) {
                    if let OrchNodeDetail::Agent { phase: slot, .. } = &mut nodes[index].detail {
                        *slot = Some(label);
                    }
                } else {
                    root_phase = Some(label);
                }
            }
            OrchestrationPayload::FanOutStarted {
                fanout_id,
                total,
                concurrency,
                workers,
            } => {
                let fid = fanout_id.to_string();
                let owner_depth = agent_depth.get(&owner_id).copied().unwrap_or(1);
                let announced = workers
                    .iter()
                    .map(|w| OrchWorker {
                        id: w.worker_id.to_string(),
                        label: w.label.clone(),
                        status: OrchStatus::Pending,
                        summary: None,
                    })
                    .collect();
                fanout_node.insert(fid, nodes.len());
                nodes.push(OrchNode {
                    indent: (owner_depth + 1).saturating_sub(2),
                    status: OrchStatus::Running,
                    detail: OrchNodeDetail::FanOut {
                        total: *total,
                        concurrency: *concurrency,
                        workers: announced,
                    },
                });
            }
            OrchestrationPayload::WorkerStarted {
                fanout_id,
                worker_id,
                label,
            } => set_worker(
                &mut nodes,
                &fanout_node,
                &fanout_id.to_string(),
                &worker_id.to_string(),
                label,
                OrchStatus::Running,
                None,
            ),
            OrchestrationPayload::WorkerCompleted {
                fanout_id,
                worker_id,
                label,
                status,
                summary,
            } => set_worker(
                &mut nodes,
                &fanout_node,
                &fanout_id.to_string(),
                &worker_id.to_string(),
                label,
                OrchStatus::from_outcome(*status),
                (!summary.trim().is_empty()).then(|| summary.clone()),
            ),
            OrchestrationPayload::FanOutCompleted { fanout_id, status } => {
                if let Some(&index) = fanout_node.get(&fanout_id.to_string()) {
                    nodes[index].status = OrchStatus::from_outcome(*status);
                }
            }
            OrchestrationPayload::ConsensusRoundResolved {
                round,
                verdicts,
                eliminated,
                remaining,
            } => {
                let mut text = format!("Consensus round {round}");
                if let Some(candidate) = eliminated {
                    text.push_str(&format!(" \u{2014} eliminated {}", candidate.label));
                }
                text.push_str(&format!(
                    " ({} verdict{}, {} remaining)",
                    verdicts.len(),
                    plural(verdicts.len()),
                    remaining.len()
                ));
                push_note(
                    &mut nodes,
                    &owner_id,
                    &agent_depth,
                    text,
                    OrchStatus::Succeeded,
                );
            }
            OrchestrationPayload::PlanSelected { candidate } => {
                let text = match candidate {
                    Some(candidate) => format!("Plan selected: {}", candidate.label),
                    None => "Plan selected".to_owned(),
                };
                push_note(
                    &mut nodes,
                    &owner_id,
                    &agent_depth,
                    text,
                    OrchStatus::Succeeded,
                );
            }
            OrchestrationPayload::ReviewRoundResolved { round, verdict, .. } => {
                let (label, status) = match verdict {
                    OrchestrationReviewVerdict::Approved => ("approved", OrchStatus::Succeeded),
                    OrchestrationReviewVerdict::Rejected => {
                        ("changes requested", OrchStatus::Running)
                    }
                    OrchestrationReviewVerdict::RoundLimitReached => {
                        ("round limit reached", OrchStatus::Failed)
                    }
                };
                push_note(
                    &mut nodes,
                    &owner_id,
                    &agent_depth,
                    format!("Review round {round}: {label}"),
                    status,
                );
            }
        }
    }

    OrchestrationPanelModel {
        root_label,
        root_phase,
        root_status,
        nodes,
    }
}

fn indent_style(indent: usize) -> String {
    format!("padding-left: {}px", 8 + indent * 16)
}

fn node_view(node: OrchNode) -> AnyView {
    let status = node.status;
    let indent = indent_style(node.indent);
    match node.detail {
        OrchNodeDetail::Agent {
            agent_type,
            task_preview,
            phase,
            result,
        } => view! {
            <div class=format!("orch-row orch-agent {}", status.css()) style=indent>
                <span class="orch-icon">{status.icon()}</span>
                <span class="orch-agent-type">{agent_type}</span>
                {(!task_preview.is_empty())
                    .then(|| view! { <span class="orch-task">{task_preview}</span> })}
                {phase.map(|p| view! { <span class="orch-phase-chip">{p}</span> })}
                {result.map(|r| view! { <span class="orch-result">{r}</span> })}
            </div>
        }
        .into_any(),
        OrchNodeDetail::FanOut {
            total,
            concurrency,
            workers,
        } => view! {
            <div class="orch-fanout" style=indent>
                <div class=format!("orch-row orch-fanout-head {}", status.css())>
                    <span class="orch-icon">{status.icon()}</span>
                    <span class="orch-fanout-title">
                        {format!("Fan-out \u{00b7} {total} worker{}", plural(total))}
                    </span>
                    <span class="orch-fanout-meta">{format!("concurrency {concurrency}")}</span>
                </div>
                <div class="orch-workers">
                    {workers
                        .into_iter()
                        .map(|worker| {
                            view! {
                                <div class=format!("orch-worker {}", worker.status.css())>
                                    <span class="orch-icon">{worker.status.icon()}</span>
                                    <span class="orch-worker-label">{worker.label}</span>
                                    {worker
                                        .summary
                                        .map(|s| {
                                            view! { <span class="orch-worker-summary">{s}</span> }
                                        })}
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()}
                </div>
            </div>
        }
        .into_any(),
        OrchNodeDetail::Note { text } => view! {
            <div class=format!("orch-row orch-note {}", status.css()) style=indent>
                <span class="orch-icon">{status.icon()}</span>
                <span class="orch-note-text">{text}</span>
            </div>
        }
        .into_any(),
    }
}

#[component]
pub fn OrchestrationView(records: Vec<OrchestrationRecord>) -> impl IntoView {
    let model = build_orchestration_panel(&records);
    let panel_class = if model.is_visible() {
        "orchestration-panel"
    } else {
        "orchestration-panel hidden"
    };

    view! {
        <div class=panel_class>
            <div class="orchestration-header">
                <span class="orchestration-title">"Orchestration"</span>
                {model
                    .root_label
                    .clone()
                    .map(|label| view! { <span class="orchestration-root">{label}</span> })}
                {model
                    .root_phase
                    .clone()
                    .map(|phase| view! { <span class="orchestration-phase">{phase}</span> })}
            </div>
            <div class="orchestration-nodes">
                {model.nodes.into_iter().map(node_view).collect::<Vec<_>>()}
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use protocol::{
        OrchestrationAgentType, OrchestrationEvent, OrchestrationId, OrchestrationWorkerInfo,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 800px; height: 600px; \
                 z-index: 2147483647; background: white; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn event(
        agent_id: &str,
        agent_type: &str,
        payload: OrchestrationPayload,
    ) -> OrchestrationRecord {
        OrchestrationRecord::Event(OrchestrationEvent {
            agent_id: OrchestrationId(agent_id.to_owned()),
            agent_type: OrchestrationAgentType(agent_type.to_owned()),
            payload,
        })
    }

    fn root_started(id: &str, agent_type: &str) -> OrchestrationRecord {
        event(
            id,
            agent_type,
            OrchestrationPayload::AgentStarted {
                parent_agent_id: None,
                task_preview: "run the swarm".to_owned(),
                origin: OrchestrationAgentOrigin::Root,
                depth: 1,
                interactive: true,
                model: None,
            },
        )
    }

    fn worker_info(id: &str, label: &str) -> OrchestrationWorkerInfo {
        OrchestrationWorkerInfo {
            worker_id: OrchestrationId(id.to_owned()),
            label: label.to_owned(),
            agent_type: OrchestrationAgentType("coder".to_owned()),
            model: None,
            reviewed: false,
            task_preview: label.to_owned(),
        }
    }

    fn mount(records: Vec<OrchestrationRecord>) -> HtmlElement {
        let container = make_container();
        mount_to(container.clone(), move || {
            let records = records.clone();
            view! { <OrchestrationView records=records /> }
        })
        .forget();
        container
    }

    fn worker_class(container: &HtmlElement, label: &str) -> Option<String> {
        let nodes = container.query_selector_all(".orch-worker").unwrap();
        for i in 0..nodes.length() {
            let el = nodes
                .item(i)
                .unwrap()
                .dyn_into::<web_sys::Element>()
                .unwrap();
            if el.text_content().unwrap_or_default().contains(label) {
                return el.get_attribute("class");
            }
        }
        None
    }

    /// A fan-out renders the root workflow in the header and each worker with a
    /// status that tracks its own lifecycle: a completed worker reads as
    /// succeeded while a still-running sibling reads as running.
    #[wasm_bindgen_test]
    async fn fanout_reflects_per_worker_progress() {
        let container = mount(vec![
            root_started("root", "swarm"),
            event(
                "root",
                "swarm",
                OrchestrationPayload::FanOutStarted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    total: 2,
                    concurrency: 2,
                    workers: vec![worker_info("w1", "src/a.rs"), worker_info("w2", "src/b.rs")],
                },
            ),
            event(
                "root",
                "swarm",
                OrchestrationPayload::WorkerStarted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    worker_id: OrchestrationId("w1".to_owned()),
                    label: "src/a.rs".to_owned(),
                },
            ),
            event(
                "root",
                "swarm",
                OrchestrationPayload::WorkerCompleted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    worker_id: OrchestrationId("w1".to_owned()),
                    label: "src/a.rs".to_owned(),
                    status: OrchestrationOutcomeStatus::Succeeded,
                    summary: "done".to_owned(),
                },
            ),
            event(
                "root",
                "swarm",
                OrchestrationPayload::WorkerStarted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    worker_id: OrchestrationId("w2".to_owned()),
                    label: "src/b.rs".to_owned(),
                },
            ),
        ]);
        next_tick().await;

        let root = container
            .query_selector(".orchestration-root")
            .unwrap()
            .expect("root workflow label present");
        assert!(
            root.text_content().unwrap_or_default().contains("swarm"),
            "header must name the root workflow, got: {:?}",
            root.text_content()
        );

        let a = worker_class(&container, "src/a.rs").expect("worker a rendered");
        assert!(
            a.contains("orch-status-succeeded"),
            "completed worker must read succeeded: {a}"
        );
        let b = worker_class(&container, "src/b.rs").expect("worker b rendered");
        assert!(
            b.contains("orch-status-running"),
            "in-flight worker must read running: {b}"
        );
    }

    /// A cancellation must not leave in-flight workers stuck "running": every
    /// worker that had not reached a terminal status is closed as aborted,
    /// while an already-completed worker keeps its real outcome.
    #[wasm_bindgen_test]
    async fn cancellation_closes_in_flight_workers() {
        let container = mount(vec![
            root_started("root", "swarm"),
            event(
                "root",
                "swarm",
                OrchestrationPayload::FanOutStarted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    total: 2,
                    concurrency: 2,
                    workers: vec![worker_info("w1", "src/a.rs"), worker_info("w2", "src/b.rs")],
                },
            ),
            event(
                "root",
                "swarm",
                OrchestrationPayload::WorkerCompleted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    worker_id: OrchestrationId("w1".to_owned()),
                    label: "src/a.rs".to_owned(),
                    status: OrchestrationOutcomeStatus::Succeeded,
                    summary: "done".to_owned(),
                },
            ),
            event(
                "root",
                "swarm",
                OrchestrationPayload::WorkerStarted {
                    fanout_id: OrchestrationId("f1".to_owned()),
                    worker_id: OrchestrationId("w2".to_owned()),
                    label: "src/b.rs".to_owned(),
                },
            ),
            OrchestrationRecord::Cancelled,
        ]);
        next_tick().await;

        let b = worker_class(&container, "src/b.rs").expect("worker b rendered");
        assert!(
            b.contains("orch-status-aborted"),
            "cancelled in-flight worker must read aborted, not running: {b}"
        );
        assert!(
            !b.contains("orch-status-running"),
            "cancelled worker must not remain running: {b}"
        );
        let a = worker_class(&container, "src/a.rs").expect("worker a rendered");
        assert!(
            a.contains("orch-status-succeeded"),
            "already-completed worker keeps its outcome across cancel: {a}"
        );
    }

    /// A workflow phase change on the root agent surfaces in the panel header,
    /// so the root reads as an evolving workflow rather than a frozen row.
    #[wasm_bindgen_test]
    async fn root_phase_shows_in_header() {
        let container = mount(vec![
            root_started("root", "swarm"),
            event(
                "root",
                "swarm",
                OrchestrationPayload::PhaseChanged {
                    phase: OrchestrationWorkflowPhase::SwarmConsensus {
                        round: 2,
                        candidates: Vec::new(),
                    },
                },
            ),
        ]);
        next_tick().await;

        let phase = container
            .query_selector(".orchestration-phase")
            .unwrap()
            .expect("phase chip present");
        let text = phase.text_content().unwrap_or_default();
        assert!(
            text.contains("Consensus") && text.contains("round 2"),
            "header must reflect the current workflow phase, got: {text}"
        );
    }
}
