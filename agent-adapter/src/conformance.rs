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
    pub open_stream_id: Option<String>,
    pub open_tool_count: usize,
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

#[derive(Debug)]
pub struct BackendConformanceValidator {
    capabilities: BackendCapabilities,
    observation: u64,
    replaying: bool,
    pending_inputs: u32,
    active_turn: bool,
    completed_turns: u64,
    open_stream_id: Option<String>,
    terminal_stream_ids: HashSet<String>,
    open_tools: HashMap<String, String>,
    known_tools: HashMap<String, String>,
    turn: TurnEvidence,
    usage_turn_id: Option<String>,
    last_request_sequence: Option<u32>,
    previous_turn_usage: Option<TokenUsage>,
    previous_cumulative_usage: Option<TokenUsage>,
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
            open_stream_id: None,
            terminal_stream_ids: HashSet::new(),
            open_tools: HashMap::new(),
            known_tools: HashMap::new(),
            turn: TurnEvidence::default(),
            usage_turn_id: None,
            last_request_sequence: None,
            previous_turn_usage: None,
            previous_cumulative_usage: None,
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
        Ok(())
    }

    pub fn end_replay(&mut self) -> Result<(), BackendConformanceError> {
        self.advance();
        if !self.replaying {
            return Err(self.error("resume replay ended without starting"));
        }
        if self.open_stream_id.is_some() {
            return Err(self.error("resume replay ended with an open assistant stream"));
        }
        if !self.open_tools.is_empty() {
            return Err(self.error("resume replay ended with open tool calls"));
        }
        self.replaying = false;
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
                if self.open_stream_id.is_some() {
                    return Err(
                        self.error("assistant stream started while another stream was open")
                    );
                }
                let message_id = start
                    .message_id
                    .as_deref()
                    .filter(|message_id| !message_id.trim().is_empty())
                    .ok_or_else(|| self.error("assistant stream started without a message id"))?;
                if self.terminal_stream_ids.contains(message_id) {
                    return Err(self.error(format!(
                        "assistant stream reused terminal message id {message_id}"
                    )));
                }
                self.open_stream_id = Some(message_id.to_owned());
                self.turn.assistant_output = true;
                Ok(())
            }
            ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
                let message_id = delta
                    .message_id
                    .as_deref()
                    .filter(|message_id| !message_id.trim().is_empty())
                    .ok_or_else(|| self.error("stream delta did not carry a message id"))?;
                self.require_open_stream(message_id, "stream delta")
            }
            ChatEvent::StreamEnd(end) => {
                let message_id = end
                    .message
                    .message_id
                    .as_ref()
                    .map(|message_id| message_id.0.as_str())
                    .filter(|message_id| !message_id.trim().is_empty())
                    .ok_or_else(|| self.error("assistant stream ended without a message id"))?;
                self.require_open_stream(message_id, "stream end")?;
                self.open_stream_id = None;
                self.terminal_stream_ids.insert(message_id.to_owned());
                self.observe_message(&end.message);
                Ok(())
            }
            ChatEvent::MessageAdded(message) => {
                if matches!(&message.sender, MessageSender::Assistant { .. }) {
                    self.ensure_turn("assistant message was added")?;
                    self.turn.assistant_output = true;
                    self.observe_message(message);
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
                if request.tool_name.trim().is_empty() {
                    return Err(self.error("tool request had an empty name"));
                }
                if self.known_tools.contains_key(&request.tool_call_id) {
                    return Err(
                        self.error(format!("duplicate tool request {}", request.tool_call_id))
                    );
                }
                self.open_tools
                    .insert(request.tool_call_id.clone(), request.tool_name.clone());
                self.known_tools
                    .insert(request.tool_call_id.clone(), request.tool_name.clone());
                Ok(())
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                let Some(tool_name) = self.open_tools.remove(&completion.tool_call_id) else {
                    return Err(self.error(format!(
                        "tool completion referenced unknown or completed tool {}",
                        completion.tool_call_id
                    )));
                };
                if tool_name != completion.tool_name {
                    return Err(self.error(format!(
                        "tool completion name mismatch for {}: expected {tool_name}, got {}",
                        completion.tool_call_id, completion.tool_name
                    )));
                }
                Ok(())
            }
            ChatEvent::ToolProgress(progress) => {
                let Some(tool_name) = self.known_tools.get(&progress.tool_call_id) else {
                    return Err(self.error(format!(
                        "tool progress referenced unknown tool {}",
                        progress.tool_call_id
                    )));
                };
                if tool_name != &progress.tool_name {
                    return Err(self.error(format!(
                        "tool progress name mismatch for {}: expected {tool_name}, got {}",
                        progress.tool_call_id, progress.tool_name
                    )));
                }
                if !self.active_turn
                    && !self.replaying
                    && !self
                        .capabilities
                        .contains(BackendCapability::BackgroundTasks)
                {
                    return Err(self.error(
                        "tool progress arrived while idle without BackgroundTasks capability",
                    ));
                }
                if matches!(&progress.update, ToolProgressUpdate::BackgroundTask(_))
                    && !self
                        .capabilities
                        .contains(BackendCapability::BackgroundTasks)
                {
                    return Err(self.error(
                        "background task progress arrived without BackgroundTasks capability",
                    ));
                }
                Ok(())
            }
            ChatEvent::OperationCancelled(_) => {
                if !self.active_turn {
                    return Err(self.error("operation cancellation arrived while idle"));
                }
                if self.open_stream_id.is_some() {
                    return Err(self.error("operation cancellation arrived before stream end"));
                }
                if !self.open_tools.is_empty() {
                    return Err(self.error("operation cancellation arrived before tool completion"));
                }
                if self.turn.cancelled {
                    return Err(self.error("operation cancellation was emitted twice"));
                }
                self.turn.cancelled = true;
                Ok(())
            }
            ChatEvent::RetryAttempt(_) => {
                if !self.active_turn {
                    return Err(self.error("retry attempt arrived while idle"));
                }
                Ok(())
            }
            ChatEvent::TaskUpdate(_)
            | ChatEvent::Orchestration(_)
            | ChatEvent::ContextCompaction(_) => Ok(()),
        }
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
        if self.open_stream_id.is_some() {
            return Err(self.error("event stream ended with an open assistant stream"));
        }
        if !self.open_tools.is_empty() {
            return Err(self.error("event stream ended with open tool calls"));
        }
        Ok(self.snapshot())
    }

    pub fn snapshot(&self) -> ConformanceSnapshot {
        ConformanceSnapshot {
            active_turn: self.active_turn,
            replaying: self.replaying,
            pending_inputs: self.pending_inputs,
            completed_turns: self.completed_turns,
            open_stream_id: self.open_stream_id.clone(),
            open_tool_count: self.open_tools.len(),
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
        if self.open_stream_id.is_some() {
            return Err(self.error("typing became idle before the assistant stream ended"));
        }
        if !self.open_tools.is_empty() {
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

    fn require_open_stream(
        &self,
        message_id: &str,
        event: &str,
    ) -> Result<(), BackendConformanceError> {
        let Some(open) = self.open_stream_id.as_deref() else {
            return Err(self.error(format!("{event} arrived without an open stream")));
        };
        if open != message_id {
            return Err(self.error(format!(
                "{event} message id {message_id} did not match open stream {open}"
            )));
        }
        Ok(())
    }

    fn observe_message(&mut self, message: &ChatMessage) {
        if let Some(usage) = &message.token_usage
            && matches!(&usage.turn, TokenUsageScope::Known { .. })
        {
            self.turn.turn_usage = true;
        }
        if let Some(context) = &message.context_breakdown {
            self.turn.context_usage = context.context_window > 0;
            self.turn.context_breakdown = true;
        }
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

#[cfg(test)]
mod tests {
    use protocol::{
        BackgroundTaskState, BackgroundTaskStatus, ChatEvent, ChatMessage, ChatMessageId,
        ContextBreakdown, CurrentContextUsage, MessageSender, MessageTokenUsage, ModelRequestId,
        ModelRequestTokenUsage, ModelTurnId, OperationCancelledData, StreamEndData,
        StreamStartData, StreamTextDeltaData, TokenUsage, TokenUsageScope,
        ToolExecutionCompletedData, ToolExecutionResult, ToolProgressData, ToolProgressUpdate,
        ToolRequest, ToolRequestType,
    };

    use super::BackendConformanceValidator;
    use crate::{BackendCapabilities, BackendCapability};

    fn capabilities(values: impl IntoIterator<Item = BackendCapability>) -> BackendCapabilities {
        BackendCapabilities::new(values)
    }

    fn validator(
        values: impl IntoIterator<Item = BackendCapability>,
    ) -> BackendConformanceValidator {
        BackendConformanceValidator::new(capabilities(values)).expect("valid capabilities")
    }

    fn message(message_id: &str, token_usage: Option<MessageTokenUsage>) -> ChatMessage {
        ChatMessage {
            message_id: Some(ChatMessageId(message_id.to_owned())),
            timestamp: 1,
            sender: MessageSender::Assistant {
                agent: "test".to_owned(),
            },
            content: "done".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage,
            context_breakdown: None,
            images: None,
        }
    }

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn known_message_usage(input: u64, output: u64) -> MessageTokenUsage {
        MessageTokenUsage {
            request: TokenUsageScope::Known {
                usage: Box::new(usage(input, output)),
            },
            turn: TokenUsageScope::Known {
                usage: Box::new(usage(input, output)),
            },
            cumulative: TokenUsageScope::Known {
                usage: Box::new(usage(input, output)),
            },
        }
    }

    fn model_usage(turn: &str, sequence: u32, request: TokenUsage) -> ModelRequestTokenUsage {
        ModelRequestTokenUsage {
            request_id: ModelRequestId {
                turn_id: ModelTurnId(turn.to_owned()),
                sequence,
            },
            turn: request.clone(),
            cumulative: request.clone(),
            request,
            model_context_window: Some(100_000),
            current_context_usage: Some(CurrentContextUsage::Known {
                input_tokens: 100,
                context_window: 100_000,
            }),
            estimated_context_breakdown: None,
        }
    }

    fn complete_text_turn(
        validator: &mut BackendConformanceValidator,
        message_id: &str,
        token_usage: Option<MessageTokenUsage>,
    ) -> Result<(), super::BackendConformanceError> {
        validator.observe_chat_event(&ChatEvent::TypingStatusChanged(true))?;
        validator.observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
            message_id: Some(message_id.to_owned()),
            agent: "test".to_owned(),
            model: None,
        }))?;
        validator.observe_chat_event(&ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some(message_id.to_owned()),
            text: "done".to_owned(),
        }))?;
        validator.observe_chat_event(&ChatEvent::StreamEnd(StreamEndData {
            message: message(message_id, token_usage),
        }))?;
        validator.observe_chat_event(&ChatEvent::TypingStatusChanged(false))
    }

    fn start_tool(validator: &mut BackendConformanceValidator, id: &str) {
        validator
            .observe_chat_event(&ChatEvent::ToolRequest(ToolRequest {
                tool_call_id: id.to_owned(),
                tool_name: "terminal".to_owned(),
                tool_type: ToolRequestType::Other {
                    args: serde_json::json!({}),
                },
            }))
            .expect("tool request");
    }

    fn complete_tool(validator: &mut BackendConformanceValidator, id: &str) {
        validator
            .observe_chat_event(&ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id: id.to_owned(),
                    tool_name: "terminal".to_owned(),
                    tool_result: ToolExecutionResult::Other {
                        result: serde_json::json!({}),
                    },
                    success: true,
                    error: None,
                    normalization_failure: None,
                },
            ))
            .expect("tool completion");
    }

    #[test]
    fn accepts_a_well_formed_user_turn() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        complete_text_turn(&mut validator, "message-1", None).expect("valid turn");

        let snapshot = validator.finish().expect("complete stream");
        assert_eq!(snapshot.completed_turns, 1);
        assert!(!snapshot.active_turn);
    }

    #[test]
    fn rejects_an_unprompted_turn_without_agent_initiation() {
        let mut validator = validator([]);

        let error = validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect_err("unprompted turn must fail");

        assert!(error.message.contains("AgentInitiatedTurns"));
    }

    #[test]
    fn accepts_an_agent_initiated_turn_after_idle() {
        let mut validator = validator([BackendCapability::AgentInitiatedTurns]);

        complete_text_turn(&mut validator, "autonomous-1", None).expect("agent-initiated turn");
        assert_eq!(validator.snapshot().completed_turns, 1);
    }

    #[test]
    fn rejects_idle_tool_progress_without_background_tasks() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        start_tool(&mut validator, "tool-1");
        complete_tool(&mut validator, "tool-1");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(false))
            .expect("finish turn");

        let error = validator
            .observe_chat_event(&ChatEvent::ToolProgress(ToolProgressData {
                tool_call_id: "tool-1".to_owned(),
                tool_name: "terminal".to_owned(),
                update: ToolProgressUpdate::Other {
                    payload: serde_json::json!({}),
                },
            }))
            .expect_err("idle progress must require capability");
        assert!(error.message.contains("BackgroundTasks"));
    }

    #[test]
    fn accepts_idle_tool_progress_for_background_tasks() {
        let mut validator = validator([BackendCapability::BackgroundTasks]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        start_tool(&mut validator, "tool-1");
        complete_tool(&mut validator, "tool-1");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(false))
            .expect("finish turn");

        validator
            .observe_chat_event(&ChatEvent::ToolProgress(ToolProgressData {
                tool_call_id: "tool-1".to_owned(),
                tool_name: "terminal".to_owned(),
                update: ToolProgressUpdate::BackgroundTask(BackgroundTaskState {
                    task_id: "background-1".to_owned(),
                    description: None,
                    status: BackgroundTaskStatus::Completed,
                    summary: Some("done".to_owned()),
                    output_unavailable: None,
                }),
            }))
            .expect("background progress while idle");
    }

    #[test]
    fn rejects_mismatched_stream_ids() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        validator
            .observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "test".to_owned(),
                model: None,
            }))
            .expect("start stream");

        let error = validator
            .observe_chat_event(&ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some("message-2".to_owned()),
                text: "wrong".to_owned(),
            }))
            .expect_err("foreign delta must fail");
        assert!(error.message.contains("did not match"));
    }

    #[test]
    fn rejects_reusing_a_terminal_stream_id() {
        let mut validator = validator([]);
        validator.input_accepted().expect("first input");
        complete_text_turn(&mut validator, "message-1", None).expect("first turn");
        validator.input_accepted().expect("second input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("second turn");

        let error = validator
            .observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "test".to_owned(),
                model: None,
            }))
            .expect_err("terminal id reuse must fail");
        assert!(error.message.contains("reused terminal message id"));
    }

    #[test]
    fn rejects_idle_transition_with_an_open_tool() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        start_tool(&mut validator, "tool-1");

        let error = validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(false))
            .expect_err("open tool must block idle");
        assert!(error.message.contains("tool requests"));
    }

    #[test]
    fn enforces_cancellation_ordering_and_uniqueness() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        validator
            .observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "test".to_owned(),
                model: None,
            }))
            .expect("start stream");

        let error = validator
            .observe_chat_event(&ChatEvent::OperationCancelled(OperationCancelledData {
                message: "cancelled".to_owned(),
            }))
            .expect_err("cancel before stream end must fail");
        assert!(error.message.contains("before stream end"));
    }

    #[test]
    fn advertised_turn_usage_requires_evidence() {
        let mut validator = validator([BackendCapability::TurnUsageReported]);
        validator.input_accepted().expect("accept input");

        let error = complete_text_turn(&mut validator, "message-1", None)
            .expect_err("missing advertised usage must fail");
        assert!(error.message.contains("TurnUsageReported"));
    }

    #[test]
    fn advertised_turn_usage_accepts_known_message_usage() {
        let mut validator = validator([BackendCapability::TurnUsageReported]);
        validator.input_accepted().expect("accept input");

        complete_text_turn(
            &mut validator,
            "message-1",
            Some(known_message_usage(10, 2)),
        )
        .expect("turn with usage");
    }

    #[test]
    fn model_request_usage_satisfies_turn_and_context_capabilities() {
        let mut validator = validator([
            BackendCapability::TurnUsageReported,
            BackendCapability::ModelRequestUsageReported,
            BackendCapability::ContextUsageReported,
        ]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        validator
            .observe_model_request_usage(&model_usage("turn-1", 1, usage(10, 2)))
            .expect("request usage");
        validator
            .observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-1".to_owned()),
                agent: "test".to_owned(),
                model: None,
            }))
            .expect("stream start");
        validator
            .observe_chat_event(&ChatEvent::StreamEnd(StreamEndData {
                message: message("message-1", None),
            }))
            .expect("stream end");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(false))
            .expect("finish turn");
    }

    #[test]
    fn rejects_model_request_sequence_gaps() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        validator
            .observe_model_request_usage(&model_usage("turn-1", 1, usage(10, 2)))
            .expect("first usage");

        let error = validator
            .observe_model_request_usage(&model_usage("turn-1", 3, usage(20, 4)))
            .expect_err("sequence gap must fail");
        assert!(error.message.contains("jumped from 1 to 3"));
    }

    #[test]
    fn rejects_decreasing_cumulative_usage() {
        let mut validator = validator([]);
        validator.input_accepted().expect("first input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("first turn");
        validator
            .observe_model_request_usage(&model_usage("turn-1", 1, usage(20, 4)))
            .expect("first usage");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(false))
            .expect("first idle");
        validator.input_accepted().expect("second input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("second turn");

        let error = validator
            .observe_model_request_usage(&model_usage("turn-2", 1, usage(10, 2)))
            .expect_err("cumulative usage regression must fail");
        assert!(error.message.contains("cumulative usage"));
    }

    #[test]
    fn replay_does_not_require_accepted_input() {
        let mut validator = validator([]);
        validator.begin_replay().expect("begin replay");
        validator
            .observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
                message_id: Some("history-1".to_owned()),
                agent: "test".to_owned(),
                model: None,
            }))
            .expect("replayed stream start");
        validator
            .observe_chat_event(&ChatEvent::StreamEnd(StreamEndData {
                message: message("history-1", None),
            }))
            .expect("replayed stream end");
        validator.end_replay().expect("end replay");
        validator.finish().expect("complete stream");
    }

    #[test]
    fn reported_context_breakdown_is_required_when_advertised() {
        let mut missing_context = validator([
            BackendCapability::ContextUsageReported,
            BackendCapability::ContextBreakdownReported,
        ]);
        missing_context.input_accepted().expect("accept input");

        let error = complete_text_turn(&mut missing_context, "message-1", None)
            .expect_err("missing context must fail");
        assert!(error.message.contains("ContextUsageReported"));

        let mut reported_context = validator([
            BackendCapability::ContextUsageReported,
            BackendCapability::ContextBreakdownReported,
        ]);
        reported_context.input_accepted().expect("accept input");
        reported_context
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");
        reported_context
            .observe_chat_event(&ChatEvent::StreamStart(StreamStartData {
                message_id: Some("message-2".to_owned()),
                agent: "test".to_owned(),
                model: None,
            }))
            .expect("stream start");
        let mut final_message = message("message-2", None);
        final_message.context_breakdown = Some(ContextBreakdown {
            system_prompt_bytes: 10,
            tool_io_bytes: 20,
            conversation_history_bytes: 30,
            reasoning_bytes: 0,
            context_injection_bytes: 0,
            input_tokens: 100,
            context_window: 1_000,
        });
        reported_context
            .observe_chat_event(&ChatEvent::StreamEnd(StreamEndData {
                message: final_message,
            }))
            .expect("stream end");
        reported_context
            .observe_chat_event(&ChatEvent::TypingStatusChanged(false))
            .expect("context-bearing turn");
    }

    #[test]
    fn finish_rejects_an_active_turn() {
        let mut validator = validator([]);
        validator.input_accepted().expect("accept input");
        validator
            .observe_chat_event(&ChatEvent::TypingStatusChanged(true))
            .expect("start turn");

        let error = validator
            .finish()
            .expect_err("active stream must not finish");
        assert!(error.message.contains("active turn"));
    }
}
