use std::collections::{HashMap, VecDeque};
use std::fmt;

use crate::types::{CloseAgentPayload, NewTerminalPayload};
use crate::{
    AgentClosedPayload, AgentOrigin, AgentStartPayload, BackendKind, BackendSetupPayload,
    ChatEvent, CommandErrorPayload, CustomAgentDeletePayload, CustomAgentNotifyPayload,
    CustomAgentUpsertPayload, DeleteSessionPayload, Envelope, FrameKind, HostBrowseClosePayload,
    HostBrowseListPayload, HostBrowseStartPayload, HostSettingsPayload, ListSessionsPayload,
    McpServerDeletePayload, McpServerNotifyPayload, McpServerUpsertPayload, NewAgentPayload,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectNotifyPayload,
    ProjectRenamePayload, ProjectReorderPayload, RunBackendSetupPayload, SessionListPayload,
    SessionSchemasPayload, SetSettingPayload, SkillNotifyPayload, SkillRefreshPayload,
    SpawnAgentPayload, SteeringDeletePayload, SteeringNotifyPayload, SteeringUpsertPayload,
    StreamPath, TerminalCreatePayload, ToolExecutionCompletedData, ToolRequest,
};

const DEFAULT_HISTORY_LIMIT: usize = 32;

#[derive(Debug, Clone)]
pub struct ProtocolValidator {
    history_limit: usize,
    recent: VecDeque<ObservedFrame>,
    agent_streams: HashMap<StreamPath, AgentStreamState>,
}

impl Default for ProtocolValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolValidator {
    pub fn new() -> Self {
        Self {
            history_limit: DEFAULT_HISTORY_LIMIT,
            recent: VecDeque::with_capacity(DEFAULT_HISTORY_LIMIT),
            agent_streams: HashMap::new(),
        }
    }

    pub fn with_history_limit(history_limit: usize) -> Self {
        Self {
            history_limit: history_limit.max(1),
            recent: VecDeque::with_capacity(history_limit.max(1)),
            agent_streams: HashMap::new(),
        }
    }

    pub fn validate_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        self.record(envelope);

        if envelope.stream.0.starts_with("/host/") {
            return self.validate_host_envelope(envelope);
        }

        if envelope.stream.0.starts_with("/agent/") {
            return self.validate_agent_envelope(envelope);
        }

        Ok(())
    }

    fn validate_host_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        match envelope.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse NewAgent payload: {error}"),
                    )
                })?;

                if self.agent_streams.contains_key(&payload.instance_stream) {
                    return Err(self.violation(
                        envelope,
                        Some(payload.backend_kind),
                        format!("duplicate NewAgent for stream {}", payload.instance_stream),
                    ));
                }

                validate_agent_origin(payload.origin, payload.parent_agent_id.as_ref()).map_err(
                    |message| self.violation(envelope, Some(payload.backend_kind), message),
                )?;

                self.agent_streams.insert(
                    payload.instance_stream,
                    AgentStreamState {
                        agent_id: payload.agent_id,
                        backend_kind: payload.backend_kind,
                        saw_agent_start: false,
                        active_stream: None,
                        assistant_turn_open: false,
                        pending_tool_calls: HashMap::new(),
                        cancelled_tool_calls: HashMap::new(),
                    },
                );
                Ok(())
            }
            FrameKind::AgentClosed => {
                let payload: AgentClosedPayload = envelope.parse_payload().map_err(|error| {
                    self.violation(
                        envelope,
                        None,
                        format!("failed to parse AgentClosed payload: {error}"),
                    )
                })?;

                let streams_to_remove = self
                    .agent_streams
                    .iter()
                    .filter_map(|(stream, state)| {
                        (state.agent_id == payload.agent_id).then_some(stream.clone())
                    })
                    .collect::<Vec<_>>();
                let removed = streams_to_remove.len();
                for stream in streams_to_remove {
                    self.agent_streams.remove(&stream);
                }
                if removed == 0 {
                    return Err(self.violation(
                        envelope,
                        None,
                        format!(
                            "AgentClosed referenced unknown agent_id {}",
                            payload.agent_id
                        ),
                    ));
                }
                Ok(())
            }
            FrameKind::HostSettings => {
                parse_host_payload::<HostSettingsPayload>(self, envelope, "HostSettings")
            }
            FrameKind::BackendSetup => {
                parse_host_payload::<BackendSetupPayload>(self, envelope, "BackendSetup")
            }
            FrameKind::SessionSchemas => {
                parse_host_payload::<SessionSchemasPayload>(self, envelope, "SessionSchemas")
            }
            FrameKind::SessionList => {
                parse_host_payload::<SessionListPayload>(self, envelope, "SessionList")
            }
            FrameKind::CommandError => {
                parse_host_payload::<CommandErrorPayload>(self, envelope, "CommandError")
            }
            FrameKind::ProjectNotify => {
                parse_host_payload::<ProjectNotifyPayload>(self, envelope, "ProjectNotify")
            }
            FrameKind::CustomAgentNotify => {
                parse_host_payload::<CustomAgentNotifyPayload>(self, envelope, "CustomAgentNotify")
            }
            FrameKind::SteeringNotify => {
                parse_host_payload::<SteeringNotifyPayload>(self, envelope, "SteeringNotify")
            }
            FrameKind::SkillNotify => {
                parse_host_payload::<SkillNotifyPayload>(self, envelope, "SkillNotify")
            }
            FrameKind::McpServerNotify => {
                parse_host_payload::<McpServerNotifyPayload>(self, envelope, "McpServerNotify")
            }
            FrameKind::SetSetting => {
                parse_host_payload::<SetSettingPayload>(self, envelope, "SetSetting")
            }
            FrameKind::SpawnAgent => {
                parse_host_payload::<SpawnAgentPayload>(self, envelope, "SpawnAgent")
            }
            FrameKind::ListSessions => {
                parse_host_payload::<ListSessionsPayload>(self, envelope, "ListSessions")
            }
            FrameKind::DeleteSession => {
                parse_host_payload::<DeleteSessionPayload>(self, envelope, "DeleteSession")
            }
            FrameKind::ProjectCreate => {
                parse_host_payload::<ProjectCreatePayload>(self, envelope, "ProjectCreate")
            }
            FrameKind::ProjectRename => {
                parse_host_payload::<ProjectRenamePayload>(self, envelope, "ProjectRename")
            }
            FrameKind::ProjectReorder => {
                parse_host_payload::<ProjectReorderPayload>(self, envelope, "ProjectReorder")
            }
            FrameKind::ProjectAddRoot => {
                parse_host_payload::<ProjectAddRootPayload>(self, envelope, "ProjectAddRoot")
            }
            FrameKind::ProjectDelete => {
                parse_host_payload::<ProjectDeletePayload>(self, envelope, "ProjectDelete")
            }
            FrameKind::CustomAgentUpsert => {
                parse_host_payload::<CustomAgentUpsertPayload>(self, envelope, "CustomAgentUpsert")
            }
            FrameKind::CustomAgentDelete => {
                parse_host_payload::<CustomAgentDeletePayload>(self, envelope, "CustomAgentDelete")
            }
            FrameKind::SteeringUpsert => {
                parse_host_payload::<SteeringUpsertPayload>(self, envelope, "SteeringUpsert")
            }
            FrameKind::SteeringDelete => {
                parse_host_payload::<SteeringDeletePayload>(self, envelope, "SteeringDelete")
            }
            FrameKind::SkillRefresh => {
                parse_host_payload::<SkillRefreshPayload>(self, envelope, "SkillRefresh")
            }
            FrameKind::McpServerUpsert => {
                parse_host_payload::<McpServerUpsertPayload>(self, envelope, "McpServerUpsert")
            }
            FrameKind::McpServerDelete => {
                parse_host_payload::<McpServerDeletePayload>(self, envelope, "McpServerDelete")
            }
            FrameKind::HostBrowseStart => {
                parse_host_payload::<HostBrowseStartPayload>(self, envelope, "HostBrowseStart")
            }
            FrameKind::HostBrowseList => {
                parse_host_payload::<HostBrowseListPayload>(self, envelope, "HostBrowseList")
            }
            FrameKind::HostBrowseClose => {
                parse_host_payload::<HostBrowseClosePayload>(self, envelope, "HostBrowseClose")
            }
            FrameKind::TerminalCreate => {
                parse_host_payload::<TerminalCreatePayload>(self, envelope, "TerminalCreate")
            }
            FrameKind::RunBackendSetup => {
                parse_host_payload::<RunBackendSetupPayload>(self, envelope, "RunBackendSetup")
            }
            FrameKind::NewTerminal => {
                parse_host_payload::<NewTerminalPayload>(self, envelope, "NewTerminal")
            }
            _ => Ok(()),
        }
    }

    fn validate_agent_envelope(&mut self, envelope: &Envelope) -> Result<(), ProtocolViolation> {
        let recent_frames: Vec<_> = self.recent.iter().cloned().collect();
        let Some(state) = self.agent_streams.get_mut(&envelope.stream) else {
            return Err(build_violation(
                &recent_frames,
                envelope,
                None,
                format!(
                    "received agent frame {} before NewAgent registered stream {}",
                    envelope.kind, envelope.stream
                ),
            ));
        };

        match envelope.kind {
            FrameKind::AgentStart => {
                if state.saw_agent_start {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("duplicate AgentStart for stream {}", envelope.stream),
                    ));
                }
                let payload: AgentStartPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse AgentStart payload: {error}"),
                    )
                })?;
                if let Err(message) =
                    validate_agent_origin(payload.origin, payload.parent_agent_id.as_ref())
                {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        message,
                    ));
                }
                state.saw_agent_start = true;
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse ChatEvent payload: {error}"),
                    )
                })?;
                validate_chat_event(&recent_frames, envelope, state, &event)?;
            }
            FrameKind::AgentRenamed => {
                if !state.saw_agent_start {
                    return Err(build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!(
                            "AgentRenamed arrived before AgentStart on {}",
                            envelope.stream
                        ),
                    ));
                }
            }
            FrameKind::AgentError => {}
            FrameKind::SessionSettings => {}
            FrameKind::SetSessionSettings => {}
            FrameKind::QueuedMessages => {}
            FrameKind::EditQueuedMessage => {}
            FrameKind::CancelQueuedMessage => {}
            FrameKind::SendQueuedMessageNow => {}
            FrameKind::CloseAgent => {
                let _: CloseAgentPayload = envelope.parse_payload().map_err(|error| {
                    build_violation(
                        &recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        format!("failed to parse CloseAgent payload: {error}"),
                    )
                })?;
            }
            other => {
                return Err(build_violation(
                    &recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!(
                        "unexpected frame kind {other} on agent stream {}",
                        envelope.stream
                    ),
                ));
            }
        }

        Ok(())
    }

    fn record(&mut self, envelope: &Envelope) {
        let observed = ObservedFrame {
            stream: envelope.stream.clone(),
            seq: envelope.seq,
            frame_kind: envelope.kind,
            detail: summarize_envelope(envelope),
        };
        self.recent.push_back(observed);
        while self.recent.len() > self.history_limit {
            self.recent.pop_front();
        }
    }

    fn violation(
        &self,
        envelope: &Envelope,
        backend_kind: Option<BackendKind>,
        message: String,
    ) -> ProtocolViolation {
        build_violation(
            &self.recent.iter().cloned().collect::<Vec<_>>(),
            envelope,
            backend_kind,
            message,
        )
    }
}

fn validate_agent_origin(
    origin: AgentOrigin,
    parent_agent_id: Option<&crate::AgentId>,
) -> Result<(), String> {
    match origin {
        AgentOrigin::BackendNative if parent_agent_id.is_none() => {
            Err("backend_native agents must include parent_agent_id".to_owned())
        }
        AgentOrigin::User | AgentOrigin::AgentControl | AgentOrigin::BackendNative => Ok(()),
    }
}

fn parse_host_payload<T: serde::de::DeserializeOwned>(
    validator: &ProtocolValidator,
    envelope: &Envelope,
    label: &str,
) -> Result<(), ProtocolViolation> {
    let _: T = envelope.parse_payload().map_err(|error| {
        validator.violation(
            envelope,
            None,
            format!("failed to parse {label} payload: {error}"),
        )
    })?;
    Ok(())
}

fn validate_chat_event(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    event: &ChatEvent,
) -> Result<(), ProtocolViolation> {
    match event {
        ChatEvent::MessageAdded(message) => match message.sender {
            crate::MessageSender::Assistant { .. } => {
                if !state.pending_tool_calls.is_empty() {
                    return Err(build_violation(
                        recent_frames,
                        envelope,
                        Some(state.backend_kind),
                        "received assistant MessageAdded while previous tool requests are still unresolved"
                            .to_owned(),
                    ));
                }
                state.assistant_turn_open = true;
                Ok(())
            }
            _ => {
                state.assistant_turn_open = false;
                Ok(())
            }
        },
        ChatEvent::StreamStart(data) => {
            if state.active_stream.is_some() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received StreamStart while previous assistant stream is still open".to_owned(),
                ));
            }
            if !state.pending_tool_calls.is_empty() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received StreamStart while previous tool requests are still unresolved"
                        .to_owned(),
                ));
            }
            state.assistant_turn_open = true;
            state.active_stream = Some(ActiveStreamState {
                message_id: data.message_id.clone(),
            });
            Ok(())
        }
        ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
            let Some(active) = state.active_stream.as_mut() else {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    format!("received {} before StreamStart", chat_event_label(event)),
                ));
            };
            if let Some(actual) = &delta.message_id {
                active.message_id = Some(actual.clone());
            }
            Ok(())
        }
        ChatEvent::StreamEnd(_) => {
            if state.active_stream.is_none() {
                return Err(build_violation(
                    recent_frames,
                    envelope,
                    Some(state.backend_kind),
                    "received StreamEnd before StreamStart".to_owned(),
                ));
            }
            state.active_stream = None;
            Ok(())
        }
        ChatEvent::ToolRequest(request) => {
            validate_tool_request(recent_frames, envelope, state, request)
        }
        ChatEvent::ToolExecutionCompleted(data) => {
            validate_tool_execution_completed(recent_frames, envelope, state, data)
        }
        ChatEvent::OperationCancelled(_) => {
            state
                .cancelled_tool_calls
                .extend(state.pending_tool_calls.drain());
            state.assistant_turn_open = false;
            Ok(())
        }
        ChatEvent::TypingStatusChanged(_)
        | ChatEvent::TaskUpdate(_)
        | ChatEvent::RetryAttempt(_) => Ok(()),
    }
}

fn validate_tool_request(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    request: &ToolRequest,
) -> Result<(), ProtocolViolation> {
    if !state.assistant_turn_open {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "received ToolRequest {} before any assistant turn",
                request.tool_call_id
            ),
        ));
    }

    if state
        .pending_tool_calls
        .insert(request.tool_call_id.clone(), request.tool_name.clone())
        .is_some()
    {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "duplicate ToolRequest for tool_call_id {}",
                request.tool_call_id
            ),
        ));
    }
    Ok(())
}

fn validate_tool_execution_completed(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    state: &mut AgentStreamState,
    data: &ToolExecutionCompletedData,
) -> Result<(), ProtocolViolation> {
    let expected_tool_name = state
        .pending_tool_calls
        .remove(&data.tool_call_id)
        .or_else(|| state.cancelled_tool_calls.remove(&data.tool_call_id));
    let Some(expected_tool_name) = expected_tool_name else {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "received ToolExecutionCompleted for unknown tool_call_id {}",
                data.tool_call_id
            ),
        ));
    };

    if expected_tool_name != data.tool_name {
        return Err(build_violation(
            recent_frames,
            envelope,
            Some(state.backend_kind),
            format!(
                "tool completion name mismatch for {}: expected {:?}, got {:?}",
                data.tool_call_id, expected_tool_name, data.tool_name
            ),
        ));
    }

    Ok(())
}

fn build_violation(
    recent_frames: &[ObservedFrame],
    envelope: &Envelope,
    backend_kind: Option<BackendKind>,
    message: String,
) -> ProtocolViolation {
    ProtocolViolation {
        stream: envelope.stream.clone(),
        seq: envelope.seq,
        frame_kind: envelope.kind,
        backend_kind,
        message,
        recent_frames: recent_frames.to_vec(),
    }
}

#[derive(Debug, Clone)]
pub struct ProtocolViolation {
    pub stream: StreamPath,
    pub seq: u64,
    pub frame_kind: FrameKind,
    pub backend_kind: Option<BackendKind>,
    pub message: String,
    pub recent_frames: Vec<ObservedFrame>,
}

impl fmt::Display for ProtocolViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = self
            .backend_kind
            .map(|kind| format!("{kind:?}"))
            .unwrap_or_else(|| "unknown".to_owned());

        writeln!(
            f,
            "{} on stream {} seq {} kind {} backend {}",
            self.message, self.stream, self.seq, self.frame_kind, backend
        )?;
        writeln!(f, "recent frames:")?;
        for frame in &self.recent_frames {
            writeln!(
                f,
                "  seq={} stream={} kind={} {}",
                frame.seq, frame.stream, frame.frame_kind, frame.detail
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ProtocolViolation {}

#[derive(Debug, Clone)]
pub struct ObservedFrame {
    pub stream: StreamPath,
    pub seq: u64,
    pub frame_kind: FrameKind,
    pub detail: String,
}

#[derive(Debug, Clone)]
struct AgentStreamState {
    agent_id: crate::AgentId,
    backend_kind: BackendKind,
    saw_agent_start: bool,
    active_stream: Option<ActiveStreamState>,
    assistant_turn_open: bool,
    pending_tool_calls: HashMap<String, String>,
    cancelled_tool_calls: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct ActiveStreamState {
    message_id: Option<String>,
}

fn summarize_envelope(envelope: &Envelope) -> String {
    if envelope.kind != FrameKind::ChatEvent {
        return String::new();
    }

    match envelope.parse_payload::<ChatEvent>() {
        Ok(event) => summarize_chat_event(&event),
        Err(error) => format!("payload_parse_error={error}"),
    }
}

fn summarize_chat_event(event: &ChatEvent) -> String {
    match event {
        ChatEvent::TypingStatusChanged(typing) => {
            format!("event=typing_status_changed typing={typing}")
        }
        ChatEvent::MessageAdded(message) => {
            format!("event=message_added sender={:?}", message.sender)
        }
        ChatEvent::StreamStart(data) => format!(
            "event=stream_start message_id={:?} agent={:?}",
            data.message_id, data.agent
        ),
        ChatEvent::StreamDelta(data) => format!(
            "event=stream_delta message_id={:?} text_len={}",
            data.message_id,
            data.text.len()
        ),
        ChatEvent::StreamReasoningDelta(data) => format!(
            "event=stream_reasoning_delta message_id={:?} text_len={}",
            data.message_id,
            data.text.len()
        ),
        ChatEvent::StreamEnd(data) => format!(
            "event=stream_end sender={:?} text_len={}",
            data.message.sender,
            data.message.content.len()
        ),
        ChatEvent::ToolRequest(data) => format!(
            "event=tool_request tool_call_id={} tool_name={}",
            data.tool_call_id, data.tool_name
        ),
        ChatEvent::ToolExecutionCompleted(data) => format!(
            "event=tool_execution_completed tool_call_id={} tool_name={} success={}",
            data.tool_call_id, data.tool_name, data.success
        ),
        ChatEvent::TaskUpdate(tasks) => {
            format!(
                "event=task_update title={:?} tasks={}",
                tasks.title,
                tasks.tasks.len()
            )
        }
        ChatEvent::OperationCancelled(data) => {
            format!("event=operation_cancelled message={:?}", data.message)
        }
        ChatEvent::RetryAttempt(data) => {
            format!(
                "event=retry_attempt attempt={} max={}",
                data.attempt, data.max_retries
            )
        }
    }
}

fn chat_event_label(event: &ChatEvent) -> &'static str {
    match event {
        ChatEvent::TypingStatusChanged(_) => "TypingStatusChanged",
        ChatEvent::MessageAdded(_) => "MessageAdded",
        ChatEvent::StreamStart(_) => "StreamStart",
        ChatEvent::StreamDelta(_) => "StreamDelta",
        ChatEvent::StreamReasoningDelta(_) => "StreamReasoningDelta",
        ChatEvent::StreamEnd(_) => "StreamEnd",
        ChatEvent::ToolRequest(_) => "ToolRequest",
        ChatEvent::ToolExecutionCompleted(_) => "ToolExecutionCompleted",
        ChatEvent::TaskUpdate(_) => "TaskUpdate",
        ChatEvent::OperationCancelled(_) => "OperationCancelled",
        ChatEvent::RetryAttempt(_) => "RetryAttempt",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ChatMessage, MessageSender, StreamEndData, StreamStartData, StreamTextDeltaData};

    fn host_stream() -> StreamPath {
        StreamPath("/host/test".to_owned())
    }

    fn agent_stream() -> StreamPath {
        StreamPath("/agent/test-agent".to_owned())
    }

    fn new_agent_envelope() -> Envelope {
        Envelope::from_payload(
            host_stream(),
            FrameKind::NewAgent,
            0,
            &NewAgentPayload {
                agent_id: crate::AgentId("test-agent".to_owned()),
                name: "test".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: vec![],
                custom_agent_id: None,
                project_id: None,
                parent_agent_id: None,
                created_at_ms: 0,
                instance_stream: agent_stream(),
            },
        )
        .expect("serialize NewAgent")
    }

    fn agent_start_envelope(seq: u64) -> Envelope {
        Envelope::from_payload(
            agent_stream(),
            FrameKind::AgentStart,
            seq,
            &crate::AgentStartPayload {
                agent_id: crate::AgentId("test-agent".to_owned()),
                name: "test".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: vec![],
                custom_agent_id: None,
                project_id: None,
                parent_agent_id: None,
                created_at_ms: 0,
            },
        )
        .expect("serialize AgentStart")
    }

    fn chat_envelope(seq: u64, event: &ChatEvent) -> Envelope {
        Envelope::from_payload(agent_stream(), FrameKind::ChatEvent, seq, event)
            .expect("serialize ChatEvent")
    }

    fn assistant_message(content: &str) -> ChatMessage {
        ChatMessage {
            timestamp: 0,
            sender: MessageSender::Assistant {
                agent: "assistant".to_owned(),
            },
            content: content.to_owned(),
            reasoning: None,
            tool_calls: vec![],
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn assistant_message_added(content: &str) -> ChatEvent {
        ChatEvent::MessageAdded(assistant_message(content))
    }

    fn user_message(content: &str) -> ChatMessage {
        ChatMessage {
            timestamp: 0,
            sender: MessageSender::User,
            content: content.to_owned(),
            reasoning: None,
            tool_calls: vec![],
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn user_message_added(content: &str) -> ChatEvent {
        ChatEvent::MessageAdded(user_message(content))
    }

    fn tool_request(call_id: &str) -> ChatEvent {
        ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: call_id.to_owned(),
            tool_name: "run_command".to_owned(),
            tool_type: crate::ToolRequestType::Other { args: json!({}) },
        })
    }

    fn tool_completed(call_id: &str) -> ChatEvent {
        ChatEvent::ToolExecutionCompleted(ToolExecutionCompletedData {
            tool_call_id: call_id.to_owned(),
            tool_name: "run_command".to_owned(),
            tool_result: crate::ToolExecutionResult::Other { result: json!({}) },
            success: true,
            error: None,
        })
    }

    #[test]
    fn accepts_turn_with_tools_after_stream_end() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("msg-1".to_owned()),
                    text: "hi".to_owned(),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                3,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(4, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(5, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn accepts_non_streaming_turn_with_tools_after_assistant_message() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(1, &assistant_message_added("hi")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn accepts_streaming_turn_with_tool_request_before_stream_end() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                3,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(4, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn rejects_tool_request_before_assistant_turn() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(1, &tool_request("call-1")))
            .expect_err("tool request before assistant turn should be invalid");

        assert!(violation.to_string().contains("ToolRequest"));
        assert_eq!(violation.backend_kind, Some(BackendKind::Claude));
    }

    #[test]
    fn rejects_stream_delta_before_stream_start() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("msg-1".to_owned()),
                    text: "hi".to_owned(),
                }),
            ))
            .expect_err("delta before stream start should be invalid");

        assert!(
            violation
                .to_string()
                .contains("StreamDelta before StreamStart")
        );
    }

    #[test]
    fn rejects_next_turn_when_tool_request_is_unresolved() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_request("call-1")))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(
                4,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-2".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .expect_err("next turn should not start while tool request is unresolved");

        assert!(
            violation
                .to_string()
                .contains("previous tool requests are still unresolved")
        );
    }

    #[test]
    fn operation_cancelled_clears_unresolved_tool_requests() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                4,
                &ChatEvent::OperationCancelled(crate::OperationCancelledData {
                    message: "cancelled".to_owned(),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                5,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-2".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
    }

    #[test]
    fn accepts_late_tool_completion_after_operation_cancelled() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                1,
                &ChatEvent::StreamStart(StreamStartData {
                    message_id: Some("msg-1".to_owned()),
                    agent: "assistant".to_owned(),
                    model: None,
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                2,
                &ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("hi"),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(
                4,
                &ChatEvent::OperationCancelled(crate::OperationCancelledData {
                    message: "cancelled".to_owned(),
                }),
            ))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(5, &tool_completed("call-1")))
            .unwrap();
    }

    #[test]
    fn rejects_unknown_tool_completion_even_after_assistant_turn() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(1, &assistant_message_added("hi")))
            .unwrap();
        let violation = validator
            .validate_envelope(&chat_envelope(2, &tool_completed("call-unknown")))
            .expect_err("unknown tool completion should be invalid");

        assert!(violation.to_string().contains("unknown tool_call_id"));
        assert_eq!(violation.backend_kind, Some(BackendKind::Claude));
    }

    #[test]
    fn accepts_mixed_non_streaming_sequence_across_multiple_turns() {
        let mut validator = ProtocolValidator::new();

        validator.validate_envelope(&new_agent_envelope()).unwrap();
        validator
            .validate_envelope(&agent_start_envelope(0))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(1, &assistant_message_added("")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(2, &tool_request("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(3, &tool_completed("call-1")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(4, &user_message_added("next turn")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(5, &assistant_message_added("")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(6, &tool_request("call-2")))
            .unwrap();
        validator
            .validate_envelope(&chat_envelope(7, &tool_completed("call-2")))
            .unwrap();
    }
}
