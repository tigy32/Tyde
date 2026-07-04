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
    /// Stable identity for keyed rendering: the sub-agent id, fan-out id, or a
    /// per-turn note index. Stable across re-folds so `<For>` reuses the row.
    pub key: String,
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
    /// Show the panel only when the current turn is actually orchestrating:
    /// it has body nodes or a live root phase. The root label alone (retained
    /// across turns) must not keep an empty panel on screen during a plain
    /// chat turn.
    pub fn is_visible(&self) -> bool {
        !self.nodes.is_empty() || self.root_phase.is_some()
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
    key: String,
    owner_id: &str,
    agent_depth: &HashMap<String, usize>,
    text: String,
    status: OrchStatus,
) {
    let owner_depth = agent_depth.get(owner_id).copied().unwrap_or(1);
    nodes.push(OrchNode {
        key,
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
    let mut note_seq: usize = 0;

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
                        key: owner_id.clone(),
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
                fanout_node.insert(fid.clone(), nodes.len());
                nodes.push(OrchNode {
                    key: fid,
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
                    format!("note-{note_seq}"),
                    &owner_id,
                    &agent_depth,
                    text,
                    OrchStatus::Succeeded,
                );
                note_seq += 1;
            }
            OrchestrationPayload::PlanSelected { candidate } => {
                let text = match candidate {
                    Some(candidate) => format!("Plan selected: {}", candidate.label),
                    None => "Plan selected".to_owned(),
                };
                push_note(
                    &mut nodes,
                    format!("note-{note_seq}"),
                    &owner_id,
                    &agent_depth,
                    text,
                    OrchStatus::Succeeded,
                );
                note_seq += 1;
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
                    format!("note-{note_seq}"),
                    &owner_id,
                    &agent_depth,
                    format!("Review round {round}: {label}"),
                    status,
                );
                note_seq += 1;
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

/// Renders the current turn's orchestration progress.
///
/// `records` is the live per-agent log (already bounded to the current turn
/// plus the retained root announcement — see `dispatch`). The fold runs once
/// per change through a `Memo`, and rows are keyed so `<For>` reuses DOM and
/// only touches nodes that actually changed rather than rebuilding the list on
/// every event.
#[component]
pub fn OrchestrationView(records: Signal<Vec<OrchestrationRecord>>) -> impl IntoView {
    let model = Memo::new(move |_| records.with(|r| build_orchestration_panel(r)));

    view! {
        <div class=move || {
            if model.with(|m| m.is_visible()) {
                "orchestration-panel"
            } else {
                "orchestration-panel hidden"
            }
        }>
            <div class="orchestration-header">
                <span class="orchestration-title">"Orchestration"</span>
                {move || {
                    model
                        .with(|m| m.root_label.clone())
                        .map(|label| view! { <span class="orchestration-root">{label}</span> })
                }}
                {move || {
                    model
                        .with(|m| m.root_phase.clone())
                        .map(|phase| view! { <span class="orchestration-phase">{phase}</span> })
                }}
            </div>
            <div class="orchestration-nodes">
                <For
                    each=move || model.with(|m| m.nodes.iter().map(|n| n.key.clone()).collect::<Vec<_>>())
                    key=|key| key.clone()
                    let:key
                >
                    {
                        // Look the node up reactively by its stable key so a
                        // same-key update (e.g. a worker completing inside a
                        // fan-out) re-renders this row instead of being frozen
                        // by the keyed `<For>`.
                        move || model.with(|m| m.nodes.iter().find(|n| n.key == key).cloned()).map(node_view)
                    }
                </For>
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppState;
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

    // Status glyphs the user actually sees, mirroring `OrchStatus::icon`.
    const ICON_RUNNING: &str = "\u{27f3}";
    const ICON_SUCCEEDED: &str = "\u{2713}";
    const ICON_ABORTED: &str = "\u{2298}";

    fn mount(records: Vec<OrchestrationRecord>) -> HtmlElement {
        let container = make_container();
        mount_to(container.clone(), move || {
            let signal = RwSignal::new(records.clone());
            view! { <OrchestrationView records=signal.into() /> }
        })
        .forget();
        container
    }

    /// The status glyph shown next to the worker row whose label contains
    /// `label` — what a human reads to see whether that worker is running,
    /// done, or aborted.
    fn worker_icon(container: &HtmlElement, label: &str) -> Option<String> {
        let rows = container.query_selector_all(".orch-worker").unwrap();
        for i in 0..rows.length() {
            let row = rows
                .item(i)
                .unwrap()
                .dyn_into::<web_sys::Element>()
                .unwrap();
            if row.text_content().unwrap_or_default().contains(label) {
                let icon = row.query_selector(".orch-icon").unwrap()?;
                return icon.text_content();
            }
        }
        None
    }

    fn worker_labels(container: &HtmlElement) -> Vec<String> {
        let els = container.query_selector_all(".orch-worker-label").unwrap();
        (0..els.length())
            .filter_map(|i| els.item(i).and_then(|n| n.text_content()))
            .collect()
    }

    /// A fan-out renders the root workflow in the header and each worker with a
    /// status glyph that tracks its own lifecycle: a completed worker shows the
    /// success glyph while a still-running sibling shows the running glyph.
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

        assert_eq!(worker_labels(&container).len(), 2, "both workers rendered");
        assert_eq!(
            worker_icon(&container, "src/a.rs").as_deref(),
            Some(ICON_SUCCEEDED),
            "completed worker must show the success glyph"
        );
        assert_eq!(
            worker_icon(&container, "src/b.rs").as_deref(),
            Some(ICON_RUNNING),
            "in-flight worker must show the running glyph"
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

        assert_eq!(
            worker_icon(&container, "src/b.rs").as_deref(),
            Some(ICON_ABORTED),
            "cancelled in-flight worker must show the aborted glyph, not running"
        );
        assert_eq!(
            worker_icon(&container, "src/a.rs").as_deref(),
            Some(ICON_SUCCEEDED),
            "already-completed worker keeps its outcome across cancel"
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

    /// Driven end-to-end through the live reducer: a new user turn segments the
    /// panel. The prior turn's workers disappear (no stale prior-turn workers
    /// shown as if current) while the once-announced root workflow header is
    /// retained, and the new turn's worker appears.
    #[wasm_bindgen_test]
    async fn new_turn_segments_and_drops_prior_workers() {
        use crate::dispatch::apply_chat_event;
        use protocol::{AgentId, ChatEvent, ChatMessage, MessageSender};
        use std::cell::RefCell;
        use std::rc::Rc;

        let container = make_container();
        let stash: Rc<RefCell<Option<(AppState, AgentId)>>> = Rc::new(RefCell::new(None));
        let stash_for_mount = stash.clone();
        mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_id = AgentId("agent-1".to_owned());
            let records: Signal<Vec<OrchestrationRecord>> = Signal::derive({
                let state = state.clone();
                let agent_id = agent_id.clone();
                move || {
                    state
                        .orchestration
                        .with(|m| m.get(&agent_id).cloned().unwrap_or_default())
                }
            });
            *stash_for_mount.borrow_mut() = Some((state.clone(), agent_id.clone()));
            view! { <OrchestrationView records=records /> }
        })
        .forget();

        let (state, agent_id) = stash.borrow().clone().unwrap();
        let host = "host";
        let user_message = || {
            ChatEvent::MessageAdded(ChatMessage {
                message_id: None,
                timestamp: 0,
                sender: MessageSender::User,
                content: "go".to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            })
        };
        let orch = |payload| {
            ChatEvent::Orchestration(OrchestrationEvent {
                agent_id: OrchestrationId("root".to_owned()),
                agent_type: OrchestrationAgentType("swarm".to_owned()),
                payload,
            })
        };

        // Turn 1: user message, root announced, one worker started.
        apply_chat_event(&state, host, &agent_id, user_message());
        apply_chat_event(
            &state,
            host,
            &agent_id,
            orch(OrchestrationPayload::AgentStarted {
                parent_agent_id: None,
                task_preview: "run".to_owned(),
                origin: OrchestrationAgentOrigin::Root,
                depth: 1,
                interactive: true,
                model: None,
            }),
        );
        apply_chat_event(
            &state,
            host,
            &agent_id,
            orch(OrchestrationPayload::FanOutStarted {
                fanout_id: OrchestrationId("f1".to_owned()),
                total: 1,
                concurrency: 1,
                workers: vec![worker_info("w1", "turn1/a.rs")],
            }),
        );
        apply_chat_event(
            &state,
            host,
            &agent_id,
            orch(OrchestrationPayload::WorkerStarted {
                fanout_id: OrchestrationId("f1".to_owned()),
                worker_id: OrchestrationId("w1".to_owned()),
                label: "turn1/a.rs".to_owned(),
            }),
        );
        next_tick().await;
        assert!(
            worker_labels(&container)
                .iter()
                .any(|l| l.contains("turn1/a.rs")),
            "turn 1 worker should be visible during turn 1"
        );

        // Turn 2: a new user message is the segment boundary.
        apply_chat_event(&state, host, &agent_id, user_message());
        next_tick().await;
        assert!(
            worker_labels(&container)
                .iter()
                .all(|l| !l.contains("turn1/a.rs")),
            "prior-turn worker must be dropped at the new turn boundary"
        );

        apply_chat_event(
            &state,
            host,
            &agent_id,
            orch(OrchestrationPayload::FanOutStarted {
                fanout_id: OrchestrationId("f2".to_owned()),
                total: 1,
                concurrency: 1,
                workers: vec![worker_info("w2", "turn2/z.rs")],
            }),
        );
        apply_chat_event(
            &state,
            host,
            &agent_id,
            orch(OrchestrationPayload::WorkerStarted {
                fanout_id: OrchestrationId("f2".to_owned()),
                worker_id: OrchestrationId("w2".to_owned()),
                label: "turn2/z.rs".to_owned(),
            }),
        );
        next_tick().await;

        let labels = worker_labels(&container);
        assert!(
            labels.iter().any(|l| l.contains("turn2/z.rs")),
            "current-turn worker must be visible: {labels:?}"
        );
        assert!(
            labels.iter().all(|l| !l.contains("turn1/a.rs")),
            "stale prior-turn worker must not reappear: {labels:?}"
        );
        let root = container
            .query_selector(".orchestration-root")
            .unwrap()
            .expect("root workflow header retained across turns");
        assert!(
            root.text_content().unwrap_or_default().contains("swarm"),
            "root workflow header must persist across the turn boundary"
        );
    }
}
