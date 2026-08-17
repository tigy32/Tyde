use std::collections::{HashMap, HashSet};

use protocol::{
    ChatEvent, ChatMessage, MessageMetadataUpdateData, MessageSender, ModelRequestTokenUsage,
    TokenUsage, TokenUsageScope, ToolProgressUpdate,
};

use crate::{BackendCapabilities, BackendCapability};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendConformanceError {
    pub observation: u64,
    pub message: String,
}

impl std::fmt::Display for BackendConformanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "backend contract violation at observation {}: {}",
            self.observation, self.message
        )
    }
}

impl std::error::Error for BackendConformanceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceSnapshot {
    pub active_turn: bool,
    pub replaying: bool,
    pub pending_inputs: u32,
    pub completed_turns: u64,
    pub stream_open: bool,
    pub open_tool_count: usize,
    pub running_background_task_count: usize,
}

#[derive(Debug, Default)]
struct TurnEvidence {
    cancelled: bool,
    assistant_output: bool,
    turn_usage: bool,
    model_request_usage: bool,
    context_usage: bool,
    context_breakdown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressPhase {
    Running,
    Terminal,
}

#[derive(Debug, Clone)]
struct TaskSnapshotState {
    description: String,
    status: protocol::TaskStatus,
}

#[derive(Debug, Default)]
struct TaskConformanceState {
    title: Option<String>,
    tasks: HashMap<u64, TaskSnapshotState>,
}

#[derive(Debug, Default)]
struct OrchestrationAgentState {
    terminal: bool,
}

#[derive(Debug)]
struct OrchestrationWorkerState {
    label: String,
    started: bool,
    terminal: bool,
}

#[derive(Debug)]
struct OrchestrationFanOutState {
    owner: protocol::OrchestrationId,
    workers: HashMap<protocol::OrchestrationId, OrchestrationWorkerState>,
    terminal: bool,
}

#[derive(Debug, Default)]
struct OrchestrationConformanceState {
    agents: HashMap<protocol::OrchestrationId, OrchestrationAgentState>,
    fanouts: HashMap<protocol::OrchestrationId, OrchestrationFanOutState>,
    consensus_rounds: HashSet<(protocol::OrchestrationId, u32)>,
    review_rounds: HashSet<(protocol::OrchestrationId, u32)>,
    plan_selected: HashSet<protocol::OrchestrationId>,
}

#[derive(Debug, Default)]
struct CompactionConformanceState {
    marker_ids: HashSet<protocol::CompactionObservationId>,
    operation_ids: HashSet<protocol::CompactionOperationId>,
}

#[derive(Debug, Default)]
struct ExtendedEventState {
    tasks: TaskConformanceState,
    orchestration: OrchestrationConformanceState,
    compaction: CompactionConformanceState,
}

#[derive(Debug)]
pub struct BackendConformanceValidator {
    capabilities: BackendCapabilities,
    observation: u64,
    replaying: bool,
    pending_inputs: u32,
    active_turn: bool,
    completed_turns: u64,
    stream_open: bool,
    open_tools: HashMap<String, String>,
    known_tools: HashMap<String, String>,
    background_tools: HashSet<String>,
    progress_phases: HashMap<(String, &'static str), ProgressPhase>,
    turn: TurnEvidence,
    usage_turn_id: Option<String>,
    last_request_sequence: Option<u32>,
    last_retry_attempt: Option<u64>,
    retry_max_retries: Option<u64>,
    previous_turn_usage: Option<TokenUsage>,
    previous_cumulative_usage: Option<TokenUsage>,
    extended_events: ExtendedEventState,
    replay_extended_events: Option<ExtendedEventState>,
}

impl BackendConformanceValidator {
    pub fn new(capabilities: BackendCapabilities) -> Result<Self, BackendConformanceError> {
        capabilities
            .validate()
            .map_err(|error| BackendConformanceError {
                observation: 0,
                message: error.to_string(),
            })?;
        Ok(Self {
            capabilities,
            observation: 0,
            replaying: false,
            pending_inputs: 0,
            active_turn: false,
            completed_turns: 0,
            stream_open: false,
            open_tools: HashMap::new(),
            known_tools: HashMap::new(),
            background_tools: HashSet::new(),
            progress_phases: HashMap::new(),
            turn: TurnEvidence::default(),
            usage_turn_id: None,
            last_request_sequence: None,
            last_retry_attempt: None,
            retry_max_retries: None,
            previous_turn_usage: None,
            previous_cumulative_usage: None,
            extended_events: ExtendedEventState::default(),
            replay_extended_events: None,
        })
    }

    pub fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    pub fn input_accepted(&mut self) -> Result<(), BackendConformanceError> {
        self.advance();
        self.pending_inputs = self
            .pending_inputs
            .checked_add(1)
            .ok_or_else(|| self.error("accepted input count overflowed"))?;
        Ok(())
    }

    pub fn begin_replay(&mut self) -> Result<(), BackendConformanceError> {
        self.advance();
        if self.replaying {
            return Err(self.error("resume replay started twice"));
        }
        if self.active_turn {
            return Err(self.error("resume replay started during an active turn"));
        }
        self.replaying = true;
        self.replay_extended_events = Some(ExtendedEventState::default());
        Ok(())
    }

    pub fn end_replay(&mut self) -> Result<(), BackendConformanceError> {
        self.advance();
        if !self.replaying {
            return Err(self.error("resume replay ended without starting"));
        }
        if self.stream_open {
            return Err(self.error("resume replay ended with an open assistant stream"));
        }
        if !self.open_tools.is_empty() {
            return Err(self.error("resume replay ended with open tool calls"));
        }
        self.replaying = false;
        self.extended_events = self
            .replay_extended_events
            .take()
            .expect("replay state exists while replaying");
        self.active_turn = false;
        self.turn = TurnEvidence::default();
        Ok(())
    }

    pub fn observe_chat_event(&mut self, event: &ChatEvent) -> Result<(), BackendConformanceError> {
        self.advance();
        match event {
            ChatEvent::TypingStatusChanged(true) => self.start_turn("typing became active"),
            ChatEvent::TypingStatusChanged(false) => self.finish_turn(),
            ChatEvent::StreamStart(start) => {
                self.ensure_turn("assistant stream started")?;
                if self.stream_open {
                    return Err(
                        self.error("assistant stream started while another stream was open")
                    );
                }
                if start.agent.trim().is_empty() {
                    return Err(self.error("assistant stream started without an agent name"));
                }
                self.stream_open = true;
                self.turn.assistant_output = true;
                Ok(())
            }
            ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
                let _ = delta;
                self.require_open_stream("stream delta")
            }
            ChatEvent::StreamEnd(end) => {
                self.require_open_stream("stream end")?;
                self.stream_open = false;
                self.observe_message(&end.message)?;
                Ok(())
            }
            ChatEvent::MessageAdded(message) => {
                if matches!(&message.sender, MessageSender::Assistant { .. }) {
                    self.ensure_turn("assistant message was added")?;
                    self.turn.assistant_output = true;
                    self.observe_message(message)?;
                }
                Ok(())
            }
            ChatEvent::MessageMetadataUpdated(metadata) => {
                self.observe_metadata(metadata);
                Ok(())
            }
            ChatEvent::ToolRequest(request) => {
                self.ensure_turn("tool request was emitted")?;
                if request.tool_call_id.trim().is_empty() {
                    return Err(self.error("tool request had an empty id"));
                }
                if self.open_tools.contains_key(&request.tool_call_id) {
                    return Err(
                        self.error(format!("duplicate tool request {}", request.tool_call_id))
                    );
                }
                let Some(tool_name) = self.known_tools.get(&request.tool_call_id).cloned() else {
                    return Err(self.error(format!(
                        "tool request {} had no owning assistant declaration",
                        request.tool_call_id
                    )));
                };
                self.open_tools
                    .insert(request.tool_call_id.clone(), tool_name);
                Ok(())
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                let Some(_) = self.open_tools.remove(&completion.tool_call_id) else {
                    return Err(self.error(format!(
                        "tool completion referenced unknown or completed tool {}",
                        completion.tool_call_id
                    )));
                };
                self.background_tools.remove(&completion.tool_call_id);
                Ok(())
            }
            ChatEvent::ToolProgress(progress) => {
                if !self.open_tools.contains_key(&progress.tool_call_id) {
                    return Err(self.error(format!(
                        "tool progress referenced unknown or completed tool {}",
                        progress.tool_call_id
                    )));
                }
                if self.background_tools.contains(&progress.tool_call_id)
                    && progress.execution_mode != protocol::ToolExecutionMode::Background
                {
                    return Err(self.error(format!(
                        "background tool {} moved back to foreground",
                        progress.tool_call_id
                    )));
                }
                if progress.execution_mode == protocol::ToolExecutionMode::Background {
                    if !self
                        .capabilities
                        .contains(BackendCapability::BackgroundTasks)
                    {
                        return Err(self.error(
                            "background tool progress arrived without BackgroundTasks capability",
                        ));
                    }
                    self.background_tools.insert(progress.tool_call_id.clone());
                }
                if !self.active_turn
                    && !self.replaying
                    && progress.execution_mode != protocol::ToolExecutionMode::Background
                {
                    return Err(self.error("foreground tool progress arrived while idle"));
                }
                self.observe_progress_phase(&progress.tool_call_id, &progress.update)?;
                Ok(())
            }
            ChatEvent::OperationCancelled(_) => {
                if !self.active_turn {
                    return Err(self.error("operation cancellation arrived while idle"));
                }
                self.stream_open = false;
                if self
                    .open_tools
                    .keys()
                    .any(|tool_call_id| !self.background_tools.contains(tool_call_id))
                {
                    return Err(self.error("operation cancellation arrived before tool completion"));
                }
                if self.turn.cancelled {
                    return Err(self.error("operation cancellation was emitted twice"));
                }
                self.turn.cancelled = true;
                Ok(())
            }
            ChatEvent::RetryAttempt(retry) => {
                if !self
                    .capabilities
                    .contains(BackendCapability::RetryTelemetry)
                {
                    return Err(
                        self.error("retry attempt arrived without RetryTelemetry capability")
                    );
                }
                if !self.active_turn {
                    return Err(self.error("retry attempt arrived while idle"));
                }
                if retry.attempt == 0 {
                    return Err(self.error("retry attempt numbering started below one"));
                }
                if retry.attempt > retry.max_retries {
                    return Err(self.error("retry attempt exceeded the declared retry limit"));
                }
                if retry.max_retries == 0 {
                    return Err(self.error("retry attempt declared a zero retry limit"));
                }
                if retry.backoff_ms == 0 {
                    return Err(self.error("retry attempt declared a zero backoff"));
                }
                if retry.error.trim().is_empty() {
                    return Err(self.error("retry attempt omitted its provider error"));
                }
                if let Some(max_retries) = self.retry_max_retries
                    && retry.max_retries != max_retries
                {
                    return Err(self.error(format!(
                        "retry limit changed within one turn from {max_retries} to {}",
                        retry.max_retries
                    )));
                }
                let expected = self.last_retry_attempt.map_or(1, |attempt| attempt + 1);
                if retry.attempt != expected {
                    return Err(self.error(format!(
                        "retry attempt sequence expected {expected}, got {}",
                        retry.attempt
                    )));
                }
                self.retry_max_retries = Some(retry.max_retries);
                self.last_retry_attempt = Some(retry.attempt);
                Ok(())
            }
            ChatEvent::TaskUpdate(tasks) => {
                if !self.capabilities.contains(BackendCapability::TaskUpdates) {
                    return Err(self.error("task update arrived without TaskUpdates capability"));
                }
                if !self.active_turn && !self.replaying {
                    return Err(self.error("task update arrived while idle"));
                }
                let capabilities = self.capabilities.clone();
                let result =
                    validate_task_update(self.extended_event_state(), &capabilities, tasks);
                result.map_err(|message| self.error(message))
            }
            ChatEvent::Orchestration(event) => {
                if !self
                    .capabilities
                    .contains(BackendCapability::OrchestrationEvents)
                {
                    return Err(self.error(
                        "orchestration event arrived without OrchestrationEvents capability",
                    ));
                }
                let result = validate_orchestration_event(
                    &mut self.extended_event_state().orchestration,
                    event,
                );
                result.map_err(|message| self.error(message))
            }
            ChatEvent::ContextCompaction(event) => {
                if !self
                    .capabilities
                    .contains(BackendCapability::CompactionReported)
                {
                    return Err(self
                        .error("compaction event arrived without CompactionReported capability"));
                }
                let result =
                    validate_compaction_event(&mut self.extended_event_state().compaction, event);
                result.map_err(|message| self.error(message))
            }
        }
    }

    fn extended_event_state(&mut self) -> &mut ExtendedEventState {
        if self.replaying {
            self.replay_extended_events
                .as_mut()
                .expect("replay state exists while replaying")
        } else {
            &mut self.extended_events
        }
    }

    fn observe_progress_phase(
        &mut self,
        tool_call_id: &str,
        update: &ToolProgressUpdate,
    ) -> Result<(), BackendConformanceError> {
        let (family, phase) = match update {
            ToolProgressUpdate::SubAgent(progress) => (
                "subagent",
                if progress.completed {
                    ProgressPhase::Terminal
                } else {
                    ProgressPhase::Running
                },
            ),
            ToolProgressUpdate::Workflow(progress) => (
                "workflow",
                if progress.status == protocol::WorkflowRunStatus::Running {
                    ProgressPhase::Running
                } else {
                    ProgressPhase::Terminal
                },
            ),
            ToolProgressUpdate::AgentControl(progress) => (
                "agent-control",
                if progress.status == protocol::AgentControlProgressStatus::Running {
                    ProgressPhase::Running
                } else {
                    ProgressPhase::Terminal
                },
            ),
            ToolProgressUpdate::Other { .. } => ("other", ProgressPhase::Running),
        };
        let key = (tool_call_id.to_owned(), family);
        if self.progress_phases.get(&key) == Some(&ProgressPhase::Terminal) {
            return Err(self.error(format!(
                "{family} progress for {tool_call_id} emitted after its terminal snapshot"
            )));
        }
        self.progress_phases.insert(key, phase);
        Ok(())
    }

    pub fn observe_model_request_usage(
        &mut self,
        usage: &ModelRequestTokenUsage,
    ) -> Result<(), BackendConformanceError> {
        self.advance();
        if !self.active_turn && !self.replaying {
            return Err(self.error("model request usage arrived while idle"));
        }
        if usage.request_id.turn_id.0.trim().is_empty() {
            return Err(self.error("model request usage had an empty turn id"));
        }
        if usage.request_id.sequence == 0 {
            return Err(self.error("model request sequence must start at one"));
        }
        match self.usage_turn_id.as_deref() {
            Some(turn_id) if turn_id != usage.request_id.turn_id.0 => {
                return Err(self.error(format!(
                    "model request turn id changed from {turn_id} to {} during one turn",
                    usage.request_id.turn_id.0
                )));
            }
            None => self.usage_turn_id = Some(usage.request_id.turn_id.0.clone()),
            Some(_) => {}
        }
        if let Some(previous) = self.last_request_sequence
            && usage.request_id.sequence != previous + 1
        {
            return Err(self.error(format!(
                "model request sequence jumped from {previous} to {}",
                usage.request_id.sequence
            )));
        }
        if self.last_request_sequence.is_none() && usage.request_id.sequence != 1 {
            return Err(self.error(format!(
                "first model request sequence was {}, expected 1",
                usage.request_id.sequence
            )));
        }
        validate_usage_total(&usage.request).map_err(|message| self.error(message))?;
        validate_usage_total(&usage.turn).map_err(|message| self.error(message))?;
        validate_usage_total(&usage.cumulative).map_err(|message| self.error(message))?;
        if let Some(previous) = &self.previous_turn_usage {
            require_usage_not_decreased(previous, &usage.turn, "turn usage")
                .map_err(|message| self.error(message))?;
        }
        if let Some(previous) = &self.previous_cumulative_usage {
            require_usage_not_decreased(previous, &usage.cumulative, "cumulative usage")
                .map_err(|message| self.error(message))?;
        }
        if let Some(protocol::CurrentContextUsage::Known {
            input_tokens,
            context_window,
        }) = &usage.current_context_usage
        {
            if *context_window == 0 {
                return Err(self.error("reported context window was zero"));
            }
            if *input_tokens > *context_window {
                return Err(self.error(format!(
                    "reported context usage {input_tokens} exceeded context window {context_window}"
                )));
            }
            self.turn.context_usage = true;
        }
        self.last_request_sequence = Some(usage.request_id.sequence);
        self.previous_turn_usage = Some(usage.turn.clone());
        self.previous_cumulative_usage = Some(usage.cumulative.clone());
        self.turn.model_request_usage = true;
        self.turn.turn_usage = true;
        Ok(())
    }

    pub fn finish(mut self) -> Result<ConformanceSnapshot, BackendConformanceError> {
        self.advance();
        if self.replaying {
            return Err(self.error("event stream ended during resume replay"));
        }
        if self.active_turn {
            return Err(self.error("event stream ended during an active turn"));
        }
        if self.stream_open {
            return Err(self.error("event stream ended with an open assistant stream"));
        }
        if !self.open_tools.is_empty() {
            return Err(self.error("event stream ended with open tool calls"));
        }
        if self.pending_inputs != 0 {
            return Err(self.error(format!(
                "event stream ended with {} accepted inputs not consumed",
                self.pending_inputs
            )));
        }
        let running_background_tasks = self.background_tools.len();
        if running_background_tasks != 0 {
            return Err(self.error(format!(
                "event stream ended with {running_background_tasks} running background tasks"
            )));
        }
        Ok(self.snapshot())
    }

    pub fn snapshot(&self) -> ConformanceSnapshot {
        ConformanceSnapshot {
            active_turn: self.active_turn,
            replaying: self.replaying,
            pending_inputs: self.pending_inputs,
            completed_turns: self.completed_turns,
            stream_open: self.stream_open,
            open_tool_count: self.open_tools.len(),
            running_background_task_count: self.background_tools.len(),
        }
    }

    fn start_turn(&mut self, cause: &str) -> Result<(), BackendConformanceError> {
        if self.active_turn {
            return Err(self.error(format!("{cause} while another turn was active")));
        }
        if !self.replaying {
            if self.pending_inputs > 0 {
                self.pending_inputs -= 1;
            } else if !self
                .capabilities
                .contains(BackendCapability::AgentInitiatedTurns)
            {
                return Err(self.error(format!(
                    "{cause} without accepted input or AgentInitiatedTurns capability"
                )));
            }
        }
        self.active_turn = true;
        self.turn = TurnEvidence::default();
        self.usage_turn_id = None;
        self.last_request_sequence = None;
        self.last_retry_attempt = None;
        self.retry_max_retries = None;
        self.previous_turn_usage = None;
        Ok(())
    }

    fn ensure_turn(&mut self, cause: &str) -> Result<(), BackendConformanceError> {
        if self.active_turn {
            return Ok(());
        }
        self.start_turn(cause)
    }

    fn finish_turn(&mut self) -> Result<(), BackendConformanceError> {
        if !self.active_turn {
            return Err(self.error("typing became idle without an active turn"));
        }
        if self.stream_open {
            return Err(self.error("typing became idle before the assistant stream ended"));
        }
        if self
            .open_tools
            .keys()
            .any(|tool_call_id| !self.background_tools.contains(tool_call_id))
        {
            return Err(self.error("typing became idle before all tool requests completed"));
        }
        if !self.replaying && !self.turn.cancelled && self.turn.assistant_output {
            self.require_turn_evidence(
                BackendCapability::TurnUsageReported,
                self.turn.turn_usage,
                "turn token usage",
            )?;
            self.require_turn_evidence(
                BackendCapability::ModelRequestUsageReported,
                self.turn.model_request_usage,
                "model request token usage",
            )?;
            self.require_turn_evidence(
                BackendCapability::ContextUsageReported,
                self.turn.context_usage,
                "reported context usage",
            )?;
            self.require_turn_evidence(
                BackendCapability::ContextBreakdownReported,
                self.turn.context_breakdown,
                "reported context breakdown",
            )?;
        }
        self.active_turn = false;
        self.completed_turns += 1;
        self.turn = TurnEvidence::default();
        self.usage_turn_id = None;
        self.last_request_sequence = None;
        self.last_retry_attempt = None;
        self.retry_max_retries = None;
        self.previous_turn_usage = None;
        Ok(())
    }

    fn require_turn_evidence(
        &self,
        capability: BackendCapability,
        observed: bool,
        label: &str,
    ) -> Result<(), BackendConformanceError> {
        if self.capabilities.contains(capability) && !observed {
            return Err(self.error(format!(
                "backend advertised {capability:?} but emitted no {label}"
            )));
        }
        Ok(())
    }

    fn require_open_stream(&self, event: &str) -> Result<(), BackendConformanceError> {
        if !self.stream_open {
            return Err(self.error(format!("{event} arrived without an open stream")));
        }
        Ok(())
    }

    fn observe_message(&mut self, message: &ChatMessage) -> Result<(), BackendConformanceError> {
        for tool in &message.tool_calls {
            if tool.tool_call_id.trim().is_empty() {
                return Err(self.error("assistant declared a tool with an empty id"));
            }
            if tool.name.trim().is_empty() {
                return Err(self.error(format!(
                    "assistant tool {} had an empty name",
                    tool.tool_call_id
                )));
            }
            if self
                .known_tools
                .insert(tool.tool_call_id.clone(), tool.name.clone())
                .is_some()
            {
                return Err(self.error(format!(
                    "assistant declared duplicate tool {}",
                    tool.tool_call_id
                )));
            }
        }
        if let Some(usage) = &message.token_usage
            && matches!(&usage.turn, TokenUsageScope::Known { .. })
        {
            self.turn.turn_usage = true;
        }
        if let Some(context) = &message.context_breakdown {
            self.turn.context_usage = context.context_window > 0;
            self.turn.context_breakdown = true;
        }
        Ok(())
    }

    fn observe_metadata(&mut self, metadata: &MessageMetadataUpdateData) {
        if let Some(usage) = &metadata.token_usage
            && matches!(&usage.turn, TokenUsageScope::Known { .. })
        {
            self.turn.turn_usage = true;
        }
        if let Some(context) = &metadata.context_breakdown {
            self.turn.context_usage = context.context_window > 0;
            self.turn.context_breakdown = true;
        }
    }

    fn advance(&mut self) {
        self.observation += 1;
    }

    fn error(&self, message: impl Into<String>) -> BackendConformanceError {
        BackendConformanceError {
            observation: self.observation,
            message: message.into(),
        }
    }
}

fn task_status_rank(status: &protocol::TaskStatus) -> u8 {
    match status {
        protocol::TaskStatus::Pending => 0,
        protocol::TaskStatus::InProgress => 1,
        protocol::TaskStatus::Completed | protocol::TaskStatus::Failed => 2,
    }
}

fn task_status_is_terminal(status: &protocol::TaskStatus) -> bool {
    matches!(
        status,
        protocol::TaskStatus::Completed | protocol::TaskStatus::Failed
    )
}

fn validate_task_update(
    state: &mut ExtendedEventState,
    capabilities: &BackendCapabilities,
    update: &protocol::TaskList,
) -> Result<(), String> {
    if update.title.trim().is_empty() {
        return Err("task update had an empty title".to_owned());
    }
    if update.tasks.is_empty()
        && !state.tasks.tasks.is_empty()
        && !capabilities.contains(BackendCapability::TaskListClear)
    {
        return Err("task list cleared without TaskListClear capability".to_owned());
    }
    let mut next = HashMap::new();
    let mut changed = state.tasks.title.as_deref() != Some(update.title.as_str());
    for task in &update.tasks {
        if task.description.trim().is_empty() {
            return Err(format!("task {} had an empty description", task.id));
        }
        if next
            .insert(
                task.id,
                TaskSnapshotState {
                    description: task.description.clone(),
                    status: task.status.clone(),
                },
            )
            .is_some()
        {
            return Err(format!("task update duplicated task id {}", task.id));
        }
        if let Some(previous) = state.tasks.tasks.get(&task.id) {
            if previous.description != task.description {
                return Err(format!("task {} changed its description", task.id));
            }
            if task_status_is_terminal(&previous.status)
                && std::mem::discriminant(&previous.status) != std::mem::discriminant(&task.status)
            {
                return Err(format!("terminal task {} changed status", task.id));
            }
            if task_status_rank(&task.status) < task_status_rank(&previous.status) {
                return Err(format!("task {} status regressed", task.id));
            }
            changed |=
                std::mem::discriminant(&previous.status) != std::mem::discriminant(&task.status);
        } else {
            changed = true;
        }
    }
    let removed = state
        .tasks
        .tasks
        .keys()
        .any(|task_id| !next.contains_key(task_id));
    if removed && !capabilities.contains(BackendCapability::TaskListReplacement) {
        return Err("task list removed tasks without TaskListReplacement capability".to_owned());
    }
    changed |= removed;
    if !state.tasks.tasks.is_empty() && !changed {
        return Err("task update duplicated the previous snapshot".to_owned());
    }
    state.tasks.title = Some(update.title.clone());
    state.tasks.tasks = next;
    Ok(())
}

fn require_orchestration_agent_active(
    state: &OrchestrationConformanceState,
    agent_id: &protocol::OrchestrationId,
) -> Result<(), String> {
    match state.agents.get(agent_id) {
        Some(agent) if !agent.terminal => Ok(()),
        Some(_) => Err(format!(
            "orchestration event arrived after agent {agent_id} completed"
        )),
        None => Err(format!(
            "orchestration event for agent {agent_id} arrived before AgentStarted"
        )),
    }
}

fn validate_candidate(candidate: &protocol::OrchestrationCandidateInfo) -> Result<(), String> {
    if candidate.label.trim().is_empty() {
        return Err("orchestration candidate had an empty label".to_owned());
    }
    Ok(())
}

fn validate_orchestration_event(
    state: &mut OrchestrationConformanceState,
    event: &protocol::OrchestrationEvent,
) -> Result<(), String> {
    use protocol::OrchestrationPayload as Payload;

    if event.agent_id.0.trim().is_empty() {
        return Err("orchestration event had an empty agent id".to_owned());
    }
    if event.agent_type.0.trim().is_empty() {
        return Err("orchestration event had an empty agent type".to_owned());
    }
    match &event.payload {
        Payload::AgentStarted {
            parent_agent_id,
            task_preview,
            origin,
            ..
        } => {
            let _ = task_preview;
            if parent_agent_id
                .as_ref()
                .is_some_and(|parent| parent.0.trim().is_empty())
            {
                return Err("orchestration agent had an empty parent id".to_owned());
            }
            if let protocol::OrchestrationAgentOrigin::Tool { tool_call_id } = origin
                && tool_call_id.trim().is_empty()
            {
                return Err("tool-origin orchestration agent had an empty tool call id".to_owned());
            }
            if state
                .agents
                .insert(
                    event.agent_id.clone(),
                    OrchestrationAgentState { terminal: false },
                )
                .is_some()
            {
                return Err(format!(
                    "orchestration agent {} started more than once",
                    event.agent_id
                ));
            }
        }
        Payload::AgentCompleted { status, .. } => {
            let Some(agent) = state.agents.get_mut(&event.agent_id) else {
                return Err(format!(
                    "orchestration agent {} completed before it started",
                    event.agent_id
                ));
            };
            if agent.terminal {
                return Err(format!(
                    "orchestration agent {} completed more than once",
                    event.agent_id
                ));
            }
            let has_open_fanout = state
                .fanouts
                .values()
                .any(|fanout| fanout.owner == event.agent_id && !fanout.terminal);
            if has_open_fanout && *status != protocol::OrchestrationOutcomeStatus::Aborted {
                return Err(format!(
                    "orchestration agent {} completed with an open fan-out",
                    event.agent_id
                ));
            }
            if *status == protocol::OrchestrationOutcomeStatus::Aborted {
                for fanout in state
                    .fanouts
                    .values_mut()
                    .filter(|fanout| fanout.owner == event.agent_id)
                {
                    fanout.terminal = true;
                }
            }
            agent.terminal = true;
        }
        Payload::PhaseChanged { .. } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
        }
        Payload::FanOutStarted {
            fanout_id,
            total,
            concurrency,
            workers,
        } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            if fanout_id.0.trim().is_empty() {
                return Err("fan-out had an empty id".to_owned());
            }
            if *total == 0 || *total != workers.len() {
                return Err(format!(
                    "fan-out {fanout_id} total {total} did not match {} workers",
                    workers.len()
                ));
            }
            if *concurrency == 0 || *concurrency > *total {
                return Err(format!(
                    "fan-out {fanout_id} had invalid concurrency {concurrency}"
                ));
            }
            let mut worker_states = HashMap::new();
            for worker in workers {
                if worker.worker_id.0.trim().is_empty()
                    || worker.label.trim().is_empty()
                    || worker.agent_type.0.trim().is_empty()
                {
                    return Err(format!("fan-out {fanout_id} declared an incomplete worker"));
                }
                if worker_states
                    .insert(
                        worker.worker_id.clone(),
                        OrchestrationWorkerState {
                            label: worker.label.clone(),
                            started: false,
                            terminal: false,
                        },
                    )
                    .is_some()
                {
                    return Err(format!("fan-out {fanout_id} duplicated a worker id"));
                }
            }
            if state
                .fanouts
                .insert(
                    fanout_id.clone(),
                    OrchestrationFanOutState {
                        owner: event.agent_id.clone(),
                        workers: worker_states,
                        terminal: false,
                    },
                )
                .is_some()
            {
                return Err(format!("fan-out {fanout_id} started more than once"));
            }
        }
        Payload::WorkerStarted {
            fanout_id,
            worker_id,
            label,
        } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            let fanout = state
                .fanouts
                .get_mut(fanout_id)
                .ok_or_else(|| format!("worker started before fan-out {fanout_id}"))?;
            if fanout.owner != event.agent_id || fanout.terminal {
                return Err(format!("worker started for inactive fan-out {fanout_id}"));
            }
            let worker = fanout
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| format!("fan-out {fanout_id} started an undeclared worker"))?;
            if worker.label != *label || label.trim().is_empty() {
                return Err(format!(
                    "fan-out {fanout_id} worker label did not correlate"
                ));
            }
            if worker.started {
                return Err(format!("fan-out {fanout_id} worker started more than once"));
            }
            worker.started = true;
        }
        Payload::WorkerCompleted {
            fanout_id,
            worker_id,
            label,
            ..
        } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            let fanout = state
                .fanouts
                .get_mut(fanout_id)
                .ok_or_else(|| format!("worker completed before fan-out {fanout_id}"))?;
            if fanout.owner != event.agent_id || fanout.terminal {
                return Err(format!("worker completed for inactive fan-out {fanout_id}"));
            }
            let worker = fanout
                .workers
                .get_mut(worker_id)
                .ok_or_else(|| format!("fan-out {fanout_id} completed an undeclared worker"))?;
            if worker.label != *label || label.trim().is_empty() {
                return Err(format!(
                    "fan-out {fanout_id} worker label did not correlate"
                ));
            }
            if !worker.started {
                return Err(format!(
                    "fan-out {fanout_id} worker completed before starting"
                ));
            }
            if worker.terminal {
                return Err(format!(
                    "fan-out {fanout_id} worker completed more than once"
                ));
            }
            worker.terminal = true;
        }
        Payload::FanOutCompleted { fanout_id, status } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            let fanout = state
                .fanouts
                .get_mut(fanout_id)
                .ok_or_else(|| format!("fan-out {fanout_id} completed before starting"))?;
            if fanout.owner != event.agent_id || fanout.terminal {
                return Err(format!(
                    "fan-out {fanout_id} completed more than once or under the wrong owner"
                ));
            }
            if *status != protocol::OrchestrationOutcomeStatus::Aborted
                && fanout.workers.values().any(|worker| !worker.terminal)
            {
                return Err(format!(
                    "fan-out {fanout_id} completed with unfinished workers"
                ));
            }
            fanout.terminal = true;
        }
        Payload::ConsensusRoundResolved {
            round,
            verdicts,
            eliminated,
            remaining,
        } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            let _ = verdicts;
            if !state
                .consensus_rounds
                .insert((event.agent_id.clone(), *round))
            {
                return Err(format!(
                    "consensus round {round} was resolved more than once"
                ));
            }
            if let Some(candidate) = eliminated {
                validate_candidate(candidate)?;
            }
            let mut labels = HashSet::new();
            for candidate in remaining {
                validate_candidate(candidate)?;
                if !labels.insert(candidate.label.as_str()) {
                    return Err("consensus remaining candidates contained duplicates".to_owned());
                }
            }
        }
        Payload::PlanSelected { candidate } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            if let Some(candidate) = candidate {
                validate_candidate(candidate)?;
            }
            if !state.plan_selected.insert(event.agent_id.clone()) {
                return Err("orchestration plan was selected more than once".to_owned());
            }
        }
        Payload::ReviewRoundResolved { round, .. } => {
            require_orchestration_agent_active(state, &event.agent_id)?;
            if !state.review_rounds.insert((event.agent_id.clone(), *round)) {
                return Err(format!("review round {round} was resolved more than once"));
            }
        }
    }
    Ok(())
}

fn validate_compaction_event(
    state: &mut CompactionConformanceState,
    event: &protocol::ContextCompactionTimelineEvent,
) -> Result<(), String> {
    if event.marker_id.0.trim().is_empty() {
        return Err("compaction event had an empty marker id".to_owned());
    }
    if !state.marker_ids.insert(event.marker_id.clone()) {
        return Err(format!(
            "compaction marker {} was emitted more than once",
            event.marker_id.0
        ));
    }
    if let Some(operation_id) = &event.operation_id {
        if operation_id.0.trim().is_empty() {
            return Err("compaction event had an empty operation id".to_owned());
        }
        if !state.operation_ids.insert(operation_id.clone()) {
            return Err(format!(
                "compaction operation {} terminalized more than once",
                operation_id.0
            ));
        }
    }
    match (event.status, event.mutation) {
        (
            protocol::ContextCompactionTimelineStatus::Completed,
            protocol::CompactionMutation::Completed,
        )
        | (
            protocol::ContextCompactionTimelineStatus::Failed,
            protocol::CompactionMutation::NotObserved
            | protocol::CompactionMutation::Completed
            | protocol::CompactionMutation::MayHaveMutated,
        ) => Ok(()),
        _ => Err("compaction terminal status and mutation disagreed".to_owned()),
    }
}

fn validate_usage_total(usage: &TokenUsage) -> Result<(), String> {
    let minimum = usage
        .input_tokens
        .checked_add(usage.output_tokens)
        .ok_or_else(|| "token usage input/output sum overflowed".to_owned())?;
    if usage.total_tokens < minimum {
        return Err(format!(
            "token usage total {} was smaller than input {} plus output {}",
            usage.total_tokens, usage.input_tokens, usage.output_tokens
        ));
    }
    Ok(())
}

fn require_usage_not_decreased(
    previous: &TokenUsage,
    current: &TokenUsage,
    label: &str,
) -> Result<(), String> {
    let fields = [
        ("input_tokens", previous.input_tokens, current.input_tokens),
        (
            "output_tokens",
            previous.output_tokens,
            current.output_tokens,
        ),
        ("total_tokens", previous.total_tokens, current.total_tokens),
    ];
    for (field, previous, current) in fields {
        if current < previous {
            return Err(format!(
                "{label} {field} decreased from {previous} to {current}"
            ));
        }
    }
    Ok(())
}
