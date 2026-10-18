use std::collections::HashMap;

use protocol::{
    AgentTimingTrace, ChatEvent, SessionId, TaskTimingCategory, TaskTimingSpan, TaskTimingState,
    TimingAvailability, ToolExecutionOutcome, ToolRequestType,
};
use tokio::time::Instant;

use crate::store::session::SessionStoreHandle;

const SPAN_LIMIT: usize = 64;
const ID_LIMIT: usize = 256;

#[derive(Default)]
pub(super) struct TimingTracker {
    trace: AgentTimingTrace,
    active: HashMap<String, Instant>,
}

impl TimingTracker {
    pub async fn restore(&mut self, store: &SessionStoreHandle, session_id: &SessionId) {
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
                self.trace = trace;
                self.persist(store, session_id).await;
            }
            Err(error) => self.fail(&error),
        }
    }

    pub fn snapshot(&self) -> AgentTimingTrace {
        let mut trace = self.trace.clone();
        for span in &mut trace.spans {
            if let Some(start) = self.active.get(&span.tool_call_id) {
                span.observed_duration_ms =
                    start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                span.observed_through_ms =
                    span.started_at_ms.saturating_add(span.observed_duration_ms);
            }
        }
        trace
    }

    pub fn interrupt(&mut self) {
        self.trace = self.snapshot();
        self.active.clear();
        for span in &mut self.trace.spans {
            if span.state == TaskTimingState::Open {
                span.state = TaskTimingState::Interrupted;
            }
        }
    }

    pub async fn observe(
        &mut self,
        event: &ChatEvent,
        store: &SessionStoreHandle,
        session: &SessionId,
    ) -> bool {
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
                if request.tool_call_id.len() > ID_LIMIT {
                    self.trace.dropped_span_count = self.trace.dropped_span_count.saturating_add(1);
                    self.persist(store, session).await;
                    return true;
                }
                if self.trace.availability == TimingAvailability::PersistenceFailed {
                    self.trace.dropped_span_count = self.trace.dropped_span_count.saturating_add(1);
                    return true;
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
                if self.trace.spans.len() == SPAN_LIMIT {
                    let removed = self.trace.spans.remove(0);
                    self.active.remove(&removed.tool_call_id);
                    self.trace.dropped_span_count = self.trace.dropped_span_count.saturating_add(1);
                }
                let now = super::now_ms();
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
        if changed {
            self.persist(store, session).await;
        }
        changed
    }

    pub(super) async fn persist(&mut self, store: &SessionStoreHandle, session: &SessionId) {
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
    }
}
