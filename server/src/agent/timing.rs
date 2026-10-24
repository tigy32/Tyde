use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use protocol::{
    AgentTimeAccount, AgentTimingTrace, ChatEvent, SessionId, TaskTimePhase, TaskTimePhaseEnd,
    TaskTimePhaseSegment, TaskTimingCategory, TaskTimingSpan, TaskTimingState, TimingAvailability,
    ToolExecutionMode, ToolExecutionOutcome, ToolRequestType,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::registry::AgentStatusHandle;
use crate::store::session::SessionStoreHandle;

const SPAN_LIMIT: usize = 64;
const ID_LIMIT: usize = 256;
const PHASE_LIMIT: usize = 256;

/// The lifecycle the status registry reports for timing, stamped where the
/// status is mutated so a busy actor never misdates or skips an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingLifecycle {
    Starting,
    Working,
    AwaitingUser,
    Queued,
    Idle,
    Terminated,
}

#[derive(Debug)]
pub(crate) struct TimingEdge {
    pub at: Instant,
    pub lifecycle: TimingLifecycle,
}

/// One process-wide axis: anchored once to the wall clock, advanced by the
/// monotonic runtime clock, so agents share a timeline and a wall-clock step
/// cannot reorder recorded phases.
fn axis_ms(at: Instant) -> u64 {
    static ANCHOR: OnceLock<(u64, Instant)> = OnceLock::new();
    let (wall, base) = ANCHOR.get_or_init(|| (super::now_ms(), Instant::now()));
    wall.saturating_add(at.saturating_duration_since(*base).as_millis() as u64)
}

fn phase_of(category: &TaskTimingCategory) -> TaskTimePhase {
    match category {
        TaskTimingCategory::Command { .. } => TaskTimePhase::Command,
        TaskTimingCategory::DependencyWait { .. } => TaskTimePhase::DependencyWait,
        TaskTimingCategory::Tool { .. } => TaskTimePhase::Tool,
    }
}

fn precedence(phase: TaskTimePhase) -> u8 {
    match phase {
        TaskTimePhase::DependencyWait => 3,
        TaskTimePhase::Command => 2,
        TaskTimePhase::Tool => 1,
        _ => 0,
    }
}

fn resting(phase: TaskTimePhase) -> bool {
    matches!(
        phase,
        TaskTimePhase::HumanWait
            | TaskTimePhase::Queued
            | TaskTimePhase::Idle
            | TaskTimePhase::NotRunning
    )
}

pub(super) struct TimingTracker {
    trace: AgentTimingTrace,
    active: HashMap<String, Instant>,
    /// In-flight foreground work by tool call, independent of span retention.
    foreground: HashMap<String, TaskTimePhase>,
    background: HashSet<String>,
    lifecycle: Option<TimingLifecycle>,
    edges: Option<mpsc::UnboundedReceiver<TimingEdge>>,
    launched_at: Instant,
    /// Keeps this agent's axis at or after its restored account.
    offset_ms: u64,
    /// The open phase; `trace.account` holds only closed segments and totals.
    current: Option<(TaskTimePhase, u64)>,
    persist_due: bool,
}

impl Default for TimingTracker {
    fn default() -> Self {
        Self {
            trace: AgentTimingTrace::default(),
            active: HashMap::new(),
            foreground: HashMap::new(),
            background: HashSet::new(),
            lifecycle: None,
            edges: None,
            launched_at: Instant::now(),
            offset_ms: 0,
            current: None,
            persist_due: false,
        }
    }
}

impl TimingTracker {
    fn at_ms(&self, at: Instant) -> u64 {
        axis_ms(at).saturating_add(self.offset_ms)
    }

    fn now_ms(&self) -> u64 {
        self.at_ms(Instant::now())
    }

    pub async fn restore(
        &mut self,
        store: &SessionStoreHandle,
        session_id: &SessionId,
        status: &AgentStatusHandle,
    ) {
        let id = session_id.clone();
        match store
            .read(move |store| {
                store
                    .get_result(&id)?
                    .map(|record| record.task_timing)
                    .ok_or_else(|| "timing session record is missing".to_owned())
            })
            .await
        {
            Ok(mut trace) => {
                if trace.spans.len() > SPAN_LIMIT {
                    let excess = trace.spans.len() - SPAN_LIMIT;
                    trace.spans.drain(..excess);
                    trace.dropped_span_count =
                        trace.dropped_span_count.saturating_add(excess as u64);
                }
                for span in &mut trace.spans {
                    if span.state == TaskTimingState::Open {
                        // The last durable observation is the only safe endpoint.
                        span.state = TaskTimingState::Interrupted;
                    }
                }
                let launched = self.at_ms(self.launched_at);
                let account = match trace.account.take() {
                    Some(mut account) => {
                        let gap = match account.phases.last_mut() {
                            Some(last) if last.end == TaskTimePhaseEnd::Open => {
                                last.end = TaskTimePhaseEnd::Interrupted;
                                TaskTimePhase::Unobserved
                            }
                            _ => TaskTimePhase::NotRunning,
                        };
                        let previous = account.observed_through_ms;
                        self.offset_ms = previous.saturating_sub(launched);
                        self.trace.account = Some(account);
                        self.close(gap, previous, self.at_ms(self.launched_at));
                        self.trace.account.take().expect("restored account")
                    }
                    None => AgentTimeAccount {
                        started_at_ms: launched,
                        observed_through_ms: launched,
                        totals: Default::default(),
                        phases: Vec::new(),
                        compacted_phase_count: 0,
                    },
                };
                self.current = Some((TaskTimePhase::Starting, account.observed_through_ms));
                trace.account = Some(account);
                self.trace = trace;
                let (edges, receiver) = mpsc::unbounded_channel();
                self.edges = Some(receiver);
                status.attach_timing(edges);
                self.persist(store, session_id).await;
            }
            Err(error) => self.fail(&error),
        }
    }

    /// Closes `[start, end)` as `phase`, merging into an equal neighbour.
    fn close(&mut self, phase: TaskTimePhase, start: u64, end: u64) {
        let Some(account) = self.trace.account.as_mut() else {
            return;
        };
        if end > start {
            account.totals.add(phase, end - start);
            match account.phases.last_mut() {
                Some(last)
                    if last.phase == phase
                        && last.end == TaskTimePhaseEnd::Ended
                        && last.ended_at_ms == start =>
                {
                    last.ended_at_ms = end;
                }
                _ => {
                    account.phases.push(TaskTimePhaseSegment {
                        phase,
                        started_at_ms: start,
                        ended_at_ms: end,
                        end: TaskTimePhaseEnd::Ended,
                    });
                    if account.phases.len() > PHASE_LIMIT {
                        account.phases.remove(0);
                        account.compacted_phase_count =
                            account.compacted_phase_count.saturating_add(1);
                    }
                }
            }
        }
        account.observed_through_ms = account.observed_through_ms.max(end);
    }

    fn classify(&self) -> TaskTimePhase {
        match self.lifecycle {
            None | Some(TimingLifecycle::Starting) => TaskTimePhase::Starting,
            Some(TimingLifecycle::Terminated) => TaskTimePhase::NotRunning,
            Some(TimingLifecycle::AwaitingUser) => TaskTimePhase::HumanWait,
            Some(TimingLifecycle::Queued) => TaskTimePhase::Queued,
            Some(TimingLifecycle::Idle) => TaskTimePhase::Idle,
            Some(TimingLifecycle::Working) => self
                .foreground
                .iter()
                .filter(|(id, _)| !self.background.contains(*id))
                .map(|(_, phase)| *phase)
                .max_by_key(|phase| precedence(*phase))
                .unwrap_or(TaskTimePhase::ModelResponse),
        }
    }

    fn reclassify(&mut self, at: u64) {
        let Some((phase, start)) = self.current else {
            return;
        };
        let next = self.classify();
        if next == phase {
            return;
        }
        let at = at.max(start);
        self.close(phase, start, at);
        self.current = Some((next, at));
        if resting(next) {
            self.persist_due = true;
        }
    }

    fn drain(&mut self) {
        let Some(receiver) = self.edges.as_mut() else {
            return;
        };
        let mut edges = Vec::new();
        while let Ok(edge) = receiver.try_recv() {
            edges.push(edge);
        }
        for edge in edges {
            if matches!(
                edge.lifecycle,
                TimingLifecycle::Idle | TimingLifecycle::Queued | TimingLifecycle::Terminated
            ) {
                // Typing status is the authority: no foreground tool outlives its turn.
                let background = &self.background;
                self.foreground.retain(|id, _| background.contains(id));
            }
            self.lifecycle = Some(edge.lifecycle);
            let at = self.at_ms(edge.at);
            self.reclassify(at);
        }
    }

    fn spans(&self) -> Vec<TaskTimingSpan> {
        let mut spans = self.trace.spans.clone();
        for span in &mut spans {
            if let Some(start) = self.active.get(&span.tool_call_id) {
                span.observed_duration_ms =
                    start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                span.observed_through_ms =
                    span.started_at_ms.saturating_add(span.observed_duration_ms);
            }
        }
        spans
    }

    pub fn snapshot(&mut self) -> AgentTimingTrace {
        self.drain();
        let mut trace = self.trace.clone();
        trace.spans = self.spans();
        if let (Some(account), Some((phase, start))) = (trace.account.as_mut(), self.current) {
            let now = self.now_ms().max(start);
            account.totals.add(phase, now - start);
            account.phases.push(TaskTimePhaseSegment {
                phase,
                started_at_ms: start,
                ended_at_ms: now,
                end: TaskTimePhaseEnd::Open,
            });
            account.observed_through_ms = now;
        }
        trace
    }

    /// Lifecycle edges are already stamped; this applies them and makes a
    /// resting transition durable.
    pub async fn observe_status(&mut self, store: &SessionStoreHandle, session: &SessionId) {
        self.drain();
        if self.persist_due {
            self.persist(store, session).await;
        }
    }

    pub fn interrupt(&mut self) {
        self.drain();
        self.trace.spans = self.spans();
        self.active.clear();
        self.foreground.clear();
        self.background.clear();
        for span in &mut self.trace.spans {
            if span.state == TaskTimingState::Open {
                span.state = TaskTimingState::Interrupted;
            }
        }
        let now = self.now_ms();
        self.reclassify(now);
    }

    /// Ends the account cleanly; a later restore records not-running time.
    pub fn stop(&mut self) {
        self.drain();
        if let Some((phase, start)) = self.current.take() {
            let now = self.now_ms().max(start);
            self.close(phase, start, now);
        }
        self.edges = None;
    }

    pub async fn observe(
        &mut self,
        event: &ChatEvent,
        store: &SessionStoreHandle,
        session: &SessionId,
    ) -> bool {
        self.drain();
        let changed = match event {
            ChatEvent::ToolRequest(request) => {
                if self
                    .trace
                    .spans
                    .iter()
                    .any(|span| span.tool_call_id == request.tool_call_id)
                {
                    return false;
                }
                let category = match &request.tool_type {
                    ToolRequestType::RunCommand { command, .. } => TaskTimingCategory::Command {
                        command: command.chars().take(240).collect(),
                        check: command.trim() == "./dev.sh check",
                    },
                    ToolRequestType::TydeAwaitAgents { agent_ids } => {
                        TaskTimingCategory::DependencyWait {
                            agent_ids: agent_ids.iter().take(128).cloned().collect(),
                            targets_observed: true,
                            omitted_link_count: Some(
                                agent_ids.len().saturating_sub(128).min(u32::MAX as usize) as u32,
                            ),
                        }
                    }
                    ToolRequestType::AgentSpawn {
                        execution_mode: protocol::AgentExecutionMode::Foreground,
                        ..
                    } => TaskTimingCategory::DependencyWait {
                        agent_ids: Vec::new(),
                        omitted_link_count: Some(0),
                        targets_observed: false,
                    },
                    _ => TaskTimingCategory::Tool {
                        name: request.tool_name.chars().take(120).collect(),
                        classified: matches!(
                            request.tool_type,
                            ToolRequestType::ModifyFile { .. }
                                | ToolRequestType::ReadFiles { .. }
                                | ToolRequestType::SearchTypes { .. }
                                | ToolRequestType::GetTypeDocs { .. }
                                | ToolRequestType::GenerateImage { .. }
                                | ToolRequestType::WebSearch { .. }
                                | ToolRequestType::ViewImage { .. }
                        ),
                    },
                };
                // A user's answer is accounted by the agent's status, not as a tool.
                if !matches!(
                    request.tool_type,
                    ToolRequestType::AskUserQuestion { .. } | ToolRequestType::ExitPlanMode { .. }
                ) && self.current.is_some()
                {
                    self.foreground
                        .insert(request.tool_call_id.clone(), phase_of(&category));
                    let now = self.now_ms();
                    self.reclassify(now);
                }
                if request.tool_call_id.len() > ID_LIMIT {
                    self.trace.dropped_span_count = self.trace.dropped_span_count.saturating_add(1);
                    self.persist(store, session).await;
                    return true;
                }
                if self.trace.availability == TimingAvailability::PersistenceFailed {
                    self.trace.dropped_span_count = self.trace.dropped_span_count.saturating_add(1);
                    return true;
                }
                if self.trace.spans.len() == SPAN_LIMIT {
                    let removed = self.trace.spans.remove(0);
                    self.active.remove(&removed.tool_call_id);
                    self.trace.dropped_span_count = self.trace.dropped_span_count.saturating_add(1);
                }
                let now = self.now_ms();
                self.trace.spans.push(TaskTimingSpan {
                    tool_call_id: request.tool_call_id.clone(),
                    category,
                    started_at_ms: now,
                    observed_through_ms: now,
                    observed_duration_ms: 0,
                    state: TaskTimingState::Open,
                });
                self.active
                    .insert(request.tool_call_id.clone(), Instant::now());
                self.trace.availability = TimingAvailability::Observed;
                true
            }
            ChatEvent::ToolProgress(progress) => {
                if self.foreground.contains_key(&progress.tool_call_id) {
                    let moved = match progress.execution_mode {
                        ToolExecutionMode::Background => {
                            self.background.insert(progress.tool_call_id.clone())
                        }
                        ToolExecutionMode::Foreground => {
                            self.background.remove(&progress.tool_call_id)
                        }
                    };
                    if moved {
                        let now = self.now_ms();
                        self.reclassify(now);
                    }
                }
                let protocol::ToolProgressUpdate::SubAgent(child) = &progress.update else {
                    return false;
                };
                let Some(span) = self.trace.spans.iter_mut().find(|span| {
                    span.tool_call_id == progress.tool_call_id
                        && span.state == TaskTimingState::Open
                }) else {
                    return false;
                };
                let TaskTimingCategory::DependencyWait {
                    agent_ids,
                    omitted_link_count,
                    targets_observed,
                } = &mut span.category
                else {
                    return false;
                };
                if agent_ids.contains(&child.agent_id) {
                    return false;
                }
                *targets_observed = true;
                if agent_ids.len() < 128 {
                    agent_ids.push(child.agent_id.clone());
                } else {
                    if omitted_link_count.is_none() {
                        return false;
                    }
                    // Progress may repeat omitted children; their distinct count is unknown.
                    *omitted_link_count = None;
                }
                true
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                self.background.remove(&completion.tool_call_id);
                if self.foreground.remove(&completion.tool_call_id).is_some() {
                    let now = self.now_ms();
                    self.reclassify(now);
                }
                let Some(start) = self.active.remove(&completion.tool_call_id) else {
                    return false;
                };
                let Some(span) = self
                    .trace
                    .spans
                    .iter_mut()
                    .find(|span| span.tool_call_id == completion.tool_call_id)
                else {
                    return false;
                };
                span.observed_duration_ms =
                    start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                span.observed_through_ms =
                    span.started_at_ms.saturating_add(span.observed_duration_ms);
                span.state = match completion.outcome {
                    ToolExecutionOutcome::Succeeded { .. } => TaskTimingState::Completed,
                    ToolExecutionOutcome::Cancelled { .. } => TaskTimingState::Cancelled,
                    ToolExecutionOutcome::Failed { .. } => TaskTimingState::Failed,
                };
                true
            }
            _ => false,
        };
        if changed || self.persist_due {
            self.persist(store, session).await;
        }
        changed
    }

    pub(super) async fn persist(&mut self, store: &SessionStoreHandle, session: &SessionId) {
        self.persist_due = false;
        let trace = self.snapshot();
        if let Err(error) = store
            .update(session, move |record| record.task_timing = trace)
            .await
        {
            self.fail(&error);
        }
    }

    fn fail(&mut self, error: &str) {
        tracing::error!(
            error_bytes = error.len(),
            "task timing persistence failed; timing coverage is unavailable"
        );
        self.trace.availability = TimingAvailability::PersistenceFailed;
        self.active.clear();
        for span in &mut self.trace.spans {
            if span.state == TaskTimingState::Open {
                span.state = TaskTimingState::Interrupted;
            }
        }
        // Accounting stops with durability; later time is reported unobserved.
        if let Some((phase, start)) = self.current.take() {
            let now = self.now_ms().max(start);
            self.close(phase, start, now);
        }
        self.edges = None;
    }
}
