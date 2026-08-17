use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::{
    AgentId, ChatEvent, ChatMessage, ChatMessageId, CompactionMethod, CompactionMetrics,
    CompactionTrigger, MessageMetadataUpdateData, MessageSender, MessageTokenUsage, ModelInfo,
    OperationCancelledData, OrchestrationAgentOrigin, OrchestrationAgentType, OrchestrationEvent,
    OrchestrationId, OrchestrationPayload, SessionId, StreamEndData, StreamStartData,
    StreamTextDeltaData, TokenUsage, ToolExecutionCompletedData, ToolExecutionOutcome,
    ToolExecutionResult, ToolProgressData, ToolRequest, ToolRequestType,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::backend::agent_control_progress::{await_progress_data_for_tool, tyde_tool_result};
use crate::backend::compaction::stable_observation_id;
use crate::backend::{
    BackendCompactionEvent, BackendCompactionObservationSource, BackendEvent,
    BackendObservedCompaction, StartupMcpServer, StartupMcpTransport,
};
use crate::sub_agent::{SubAgentEmitter, SubAgentHandle};

use super::script::{MockAgentControlAwait, MockChildLifecycle, MockNativeChild};
use super::{MOCK_MODEL, now_ms};

#[derive(Clone)]
pub(super) struct MockEventSender {
    tx: mpsc::UnboundedSender<BackendEvent>,
    active_turn: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(super) struct WeakMockEventSender {
    tx: mpsc::WeakUnboundedSender<BackendEvent>,
    active_turn: Arc<AtomicBool>,
}

impl MockEventSender {
    pub(super) fn new(tx: mpsc::UnboundedSender<BackendEvent>) -> Self {
        Self {
            tx,
            active_turn: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn downgrade(&self) -> WeakMockEventSender {
        WeakMockEventSender {
            tx: self.tx.downgrade(),
            active_turn: Arc::clone(&self.active_turn),
        }
    }

    pub(super) fn send_event(&self, event: BackendEvent) -> bool {
        if let BackendEvent::Chat(ChatEvent::TypingStatusChanged(active)) = &event {
            self.active_turn.store(*active, Ordering::SeqCst);
        }
        self.tx.send(event).is_ok()
    }

    pub(super) fn send_compaction(&self, event: BackendCompactionEvent) -> bool {
        self.send_event(BackendEvent::Compaction(event))
    }

    pub(super) fn is_active(&self) -> bool {
        self.active_turn.load(Ordering::SeqCst)
    }
}

impl WeakMockEventSender {
    pub(super) fn upgrade(&self) -> Option<MockEventSender> {
        Some(MockEventSender {
            tx: self.tx.upgrade()?,
            active_turn: Arc::clone(&self.active_turn),
        })
    }
}

fn chat(event: ChatEvent) -> BackendEvent {
    BackendEvent::Chat(event)
}

pub(super) fn typing(active: bool) -> BackendEvent {
    chat(ChatEvent::TypingStatusChanged(active))
}

pub(super) fn stream_start(agent: &str, model: Option<String>) -> BackendEvent {
    chat(ChatEvent::StreamStart(StreamStartData {
        agent: agent.to_owned(),
        model,
    }))
}

pub(super) fn stream_delta(text: String) -> BackendEvent {
    chat(ChatEvent::StreamDelta(StreamTextDeltaData { text }))
}

pub(super) fn stream_end(message: ChatMessage) -> BackendEvent {
    chat(ChatEvent::StreamEnd(StreamEndData { message }))
}

pub(super) fn message_added(message: ChatMessage) -> BackendEvent {
    chat(ChatEvent::MessageAdded(message))
}

pub(super) fn cancelled(message: String) -> BackendEvent {
    chat(ChatEvent::OperationCancelled(OperationCancelledData {
        message,
    }))
}

pub(super) fn metadata_update(data: MessageMetadataUpdateData) -> BackendEvent {
    chat(ChatEvent::MessageMetadataUpdated(data))
}

pub(super) fn tool_request(request: ToolRequest) -> BackendEvent {
    chat(ChatEvent::ToolRequest(request))
}

pub(super) fn tool_completed(data: ToolExecutionCompletedData) -> BackendEvent {
    chat(ChatEvent::ToolExecutionCompleted(data))
}

pub(super) fn tool_progress(progress: ToolProgressData) -> BackendEvent {
    chat(ChatEvent::ToolProgress(progress))
}

pub(super) fn empty_agent_control_output_frames() -> Vec<BackendEvent> {
    let empty_output_message_id = ChatMessageId(Uuid::new_v4().to_string());
    vec![
        typing(true),
        stream_start("mock", None),
        stream_end(ChatMessage {
            reasoning: Some(protocol::ReasoningData {
                text: "hidden reasoning".to_owned(),
                tokens: None,
                signature: None,
                blob: None,
            }),
            model_info: None,
            ..mock_assistant_message(Some(empty_output_message_id), String::new())
        }),
        typing(false),
    ]
}

pub(super) fn orchestration_frames() -> Vec<BackendEvent> {
    vec![
        chat(ChatEvent::Orchestration(OrchestrationEvent {
            agent_id: OrchestrationId("mock-root".to_owned()),
            agent_type: OrchestrationAgentType("swarm".to_owned()),
            payload: OrchestrationPayload::AgentStarted {
                parent_agent_id: None,
                task_preview: "mock orchestration".to_owned(),
                origin: OrchestrationAgentOrigin::Root,
                depth: 1,
                interactive: true,
                model: None,
            },
        })),
        typing(false),
    ]
}

pub(super) fn mock_assistant_message(
    message_id: Option<ChatMessageId>,
    content: String,
) -> ChatMessage {
    ChatMessage {
        message_id,
        timestamp: now_ms(),
        sender: MessageSender::Assistant {
            agent: "mock".to_owned(),
        },
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: Some(ModelInfo {
            model: MOCK_MODEL.to_owned(),
        }),
        token_usage: None,
        context_breakdown: None,
        images: None,
    }
}

pub(super) fn user_bubble(message: &str) -> BackendEvent {
    message_added(ChatMessage {
        message_id: Some(ChatMessageId(Uuid::new_v4().to_string())),
        timestamp: now_ms(),
        sender: MessageSender::User,
        content: message.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

pub(super) fn error_card(message: &str) -> BackendEvent {
    card(MessageSender::Error, message)
}

pub(super) fn warning_card(message: &str) -> BackendEvent {
    card(MessageSender::Warning, message)
}

fn card(sender: MessageSender, message: &str) -> BackendEvent {
    message_added(ChatMessage {
        message_id: None,
        timestamp: now_ms(),
        sender,
        content: message.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

pub(super) fn emit_error_card(events_tx: &MockEventSender, message: &str) {
    let _ = events_tx.send_event(error_card(message));
}

pub(super) fn compaction_observation(
    session_id: &SessionId,
    prompt_index: usize,
    trigger: CompactionTrigger,
    method: CompactionMethod,
) -> BackendEvent {
    let event_id = format!("prompt-{prompt_index}-compact");
    BackendEvent::Compaction(BackendCompactionEvent::Observed(Box::new(
        BackendObservedCompaction {
            observation_id: stable_observation_id("mock", &session_id.0, &event_id),
            trigger,
            method,
            provider_session_id: Some(session_id.clone()),
            metrics: CompactionMetrics {
                before_tokens: Some(12_000),
                after_tokens: Some(3_000),
                ..CompactionMetrics::default()
            },
            source: BackendCompactionObservationSource::MockEvent { event_id },
            user_focus: None,
        },
    )))
}

pub(super) fn compact_turn_frames(
    session_id: &SessionId,
    prompt_index: usize,
    prompt: &str,
) -> Vec<BackendEvent> {
    let message_id = ChatMessageId(Uuid::new_v4().to_string());
    vec![
        typing(true),
        user_bubble(prompt),
        stream_start("mock", Some(MOCK_MODEL.to_owned())),
        compaction_observation(
            session_id,
            prompt_index,
            CompactionTrigger::UserTyped,
            CompactionMethod::NativeTextCommand,
        ),
        stream_end(ChatMessage {
            message_id: Some(message_id),
            timestamp: now_ms(),
            sender: MessageSender::Assistant {
                agent: "mock".to_owned(),
            },
            content: String::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: Some(ModelInfo {
                model: MOCK_MODEL.to_owned(),
            }),
            token_usage: None,
            context_breakdown: None,
            images: None,
        }),
        typing(false),
    ]
}

pub(super) fn mock_turn_token_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1250,
        output_tokens: 340,
        total_tokens: 1590,
        cached_prompt_tokens: Some(800),
        cache_creation_input_tokens: Some(50),
        reasoning_tokens: Some(120),
    }
}

pub(super) fn agent_control_send_message_frames(
    agent_id: AgentId,
    message: String,
) -> Vec<BackendEvent> {
    let tool_call_id = format!("mock-agent-control-send-message-{}", Uuid::new_v4());
    let response_text = "mock agent-control message delivered".to_owned();

    vec![
        typing(true),
        stream_start("mock", Some(MOCK_MODEL.to_owned())),
        tool_request(ToolRequest {
            tool_call_id: tool_call_id.clone(),
            tool_type: ToolRequestType::TydeSendAgentMessage { agent_id, message },
        }),
        tool_completed(ToolExecutionCompletedData {
            tool_call_id,
            outcome: ToolExecutionOutcome::Succeeded {
                result: ToolExecutionResult::TydeSendAgentMessage,
            },
        }),
        stream_delta(response_text.clone()),
        stream_end(mock_assistant_message(None, response_text)),
        typing(false),
    ]
}

pub(super) fn exit_plan_mode_request_frames(
    stream_end_before_request: bool,
    tool_call_id: &str,
    plan: &str,
    plan_path: &str,
) -> Vec<BackendEvent> {
    const WAITING_TEXT: &str = "mock ExitPlanMode waiting for approval";
    let mut frames = vec![typing(true)];
    if stream_end_before_request {
        let message_id = Some(Uuid::new_v4().to_string());
        frames.push(stream_start("mock", Some(MOCK_MODEL.to_owned())));
        frames.push(stream_delta(WAITING_TEXT.to_owned()));
        frames.push(stream_end(mock_assistant_message(
            message_id.map(ChatMessageId),
            WAITING_TEXT.to_owned(),
        )));
    } else {
        frames.push(message_added(mock_assistant_message(
            Some(ChatMessageId(Uuid::new_v4().to_string())),
            WAITING_TEXT.to_owned(),
        )));
    }
    frames.push(tool_request(ToolRequest {
        tool_call_id: tool_call_id.to_owned(),
        tool_type: ToolRequestType::ExitPlanMode {
            plan: Some(plan.to_owned()),
            plan_path: Some(plan_path.to_owned()),
        },
    }));
    frames.push(typing(false));
    frames
}

pub(super) fn exit_plan_mode_completion(
    tool_call_id: &str,
    decision: protocol::ExitPlanModeDecision,
    feedback: Option<String>,
) -> (BackendEvent, String) {
    let approved = decision == protocol::ExitPlanModeDecision::Approve;
    let decision_label = if approved { "approved" } else { "rejected" };
    let message = if approved {
        "mock ExitPlanMode approved".to_owned()
    } else {
        format!(
            "mock ExitPlanMode rejected: {}",
            feedback.as_deref().unwrap_or("Plan rejected by user.")
        )
    };

    let completion = tool_completed(ToolExecutionCompletedData {
        tool_call_id: tool_call_id.to_owned(),
        outcome: ToolExecutionOutcome::Succeeded {
            result: ToolExecutionResult::Other {
                result: serde_json::json!({
                    "decision": decision_label,
                    "feedback": feedback,
                }),
            },
        },
    });
    (completion, message)
}

pub(super) fn codex_internal_error_tail_frames() -> Vec<BackendEvent> {
    const TOOL_CALL_ID: &str = "mock-codex-successful-tool";
    vec![
        typing(true),
        tool_request(ToolRequest {
            tool_call_id: TOOL_CALL_ID.to_owned(),
            tool_type: ToolRequestType::RunCommand {
                command: "printf done".to_owned(),
                working_directory: "/tmp/test".to_owned(),
            },
        }),
        tool_completed(ToolExecutionCompletedData {
            tool_call_id: TOOL_CALL_ID.to_owned(),
            outcome: ToolExecutionOutcome::Succeeded {
                result: ToolExecutionResult::RunCommand {
                    exit_code: 0,
                    stdout: "done".to_owned(),
                    stderr: String::new(),
                },
            },
        }),
        warning_card("Codex warning: Internal server error"),
        typing(false),
        error_card("Internal server error"),
    ]
}

pub(super) fn tool_failure_without_idle_frames(tool_call_id: &str) -> Vec<BackendEvent> {
    vec![
        typing(true),
        stream_start("mock", Some(MOCK_MODEL.to_owned())),
        tool_request(ToolRequest {
            tool_call_id: tool_call_id.to_owned(),
            tool_type: ToolRequestType::RunCommand {
                command: "mock interrupted command".to_owned(),
                working_directory: "/tmp/test".to_owned(),
            },
        }),
        tool_completed(ToolExecutionCompletedData {
            tool_call_id: tool_call_id.to_owned(),
            outcome: ToolExecutionOutcome::Failed {
                message: "Tool execution was interrupted".to_owned(),
                details: Some("Claude history did not contain a tool_result before the conversation advanced; treating the tool as interrupted.".to_owned()),
                normalization_failure: None,
            },
        }),
    ]
}

pub(super) async fn spawn_native_child(
    events_tx: &MockEventSender,
    subagent_emitter_rx: &mut watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    active_subagents: &mut Vec<SubAgentHandle>,
    child: MockNativeChild,
) {
    let live = child.lifecycle == MockChildLifecycle::LiveUntilInterrupt;

    let emitter = wait_for_subagent_emitter(subagent_emitter_rx).await;
    let tool_use_id = if live {
        format!("mock-live-tool-use-{}", Uuid::new_v4())
    } else {
        format!("mock-tool-use-{}", Uuid::new_v4())
    };
    let _ = events_tx.send_event(tool_request(ToolRequest {
        tool_call_id: tool_use_id.clone(),
        tool_type: ToolRequestType::AgentSpawn {
            prompt: Some(child.prompt.clone()),
            name: Some(child.name.clone()),
            execution_mode: protocol::AgentExecutionMode::Background,
        },
    }));

    let handle = match emitter
        .on_subagent_spawned(
            tool_use_id,
            child.name.clone(),
            child.prompt.clone(),
            "mock".to_owned(),
            Some(SessionId(Uuid::new_v4().to_string())),
        )
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            if live {
                tracing::error!(%error, "mock live native child relay registration failed");
            } else {
                tracing::error!(%error, "mock native child relay registration failed");
            }
            return;
        }
    };

    match child.lifecycle {
        MockChildLifecycle::LiveUntilInterrupt => {
            emit_live_native_child_stream(&handle);
            active_subagents.push(handle);
        }
        MockChildLifecycle::CompleteAndRetain => {
            emit_native_child_turn(&handle.event_tx, &child.prompt);
            active_subagents.push(handle);
        }
        MockChildLifecycle::CompleteAndDrop => {
            emit_native_child_turn(&handle.event_tx, &child.prompt);
            // Dropping the handle closes event_tx, so the relay actor sees
            // events.recv() == None and must park instead of exiting.
            drop(handle);
        }
    }
}

async fn wait_for_subagent_emitter(
    subagent_emitter_rx: &mut watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
) -> Arc<dyn SubAgentEmitter> {
    loop {
        if let Some(emitter) = subagent_emitter_rx.borrow().clone() {
            return emitter;
        }
        subagent_emitter_rx
            .changed()
            .await
            .expect("mock sub-agent emitter sender dropped before registration");
    }
}

fn live_native_child_message_id(handle: &SubAgentHandle) -> String {
    format!("mock-live-{}", handle.agent_id.0)
}

const LIVE_NATIVE_CHILD_TEXT: &str = "mock live native child working";
const LIVE_NATIVE_CHILD_AGENT: &str = "mock-live-native-child";

fn emit_live_native_child_stream(handle: &SubAgentHandle) {
    let _ = handle.event_tx.send(ChatEvent::TypingStatusChanged(true));
    let _ = handle
        .event_tx
        .send(ChatEvent::StreamStart(StreamStartData {
            agent: LIVE_NATIVE_CHILD_AGENT.to_owned(),
            model: Some(MOCK_MODEL.to_owned()),
        }));
    let _ = handle
        .event_tx
        .send(ChatEvent::StreamDelta(StreamTextDeltaData {
            text: LIVE_NATIVE_CHILD_TEXT.to_owned(),
        }));
}

pub(super) fn cancel_live_native_children(active_subagents: &[SubAgentHandle]) {
    for child in active_subagents {
        let _ = child.event_tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: Some(ChatMessageId(live_native_child_message_id(child))),
                timestamp: now_ms(),
                sender: MessageSender::Assistant {
                    agent: LIVE_NATIVE_CHILD_AGENT.to_owned(),
                },
                content: LIVE_NATIVE_CHILD_TEXT.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: Some(ModelInfo {
                    model: MOCK_MODEL.to_owned(),
                }),
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        }));
        let _ = child
            .event_tx
            .send(ChatEvent::OperationCancelled(OperationCancelledData {
                message: "Parent agent turn ended before the sub-agent completed".to_owned(),
            }));
        let _ = child.event_tx.send(ChatEvent::TypingStatusChanged(false));
    }
}

fn emit_native_child_turn(event_tx: &mpsc::UnboundedSender<ChatEvent>, prompt: &str) {
    let message_id = Some(Uuid::new_v4().to_string());
    let response_text = format!("mock native child response to: {prompt}");

    let _ = event_tx.send(ChatEvent::TypingStatusChanged(true));
    let _ = event_tx.send(ChatEvent::StreamStart(StreamStartData {
        agent: "mock-native-child".to_owned(),
        model: Some(MOCK_MODEL.to_owned()),
    }));
    let _ = event_tx.send(ChatEvent::StreamDelta(StreamTextDeltaData {
        text: response_text.clone(),
    }));
    let _ = event_tx.send(ChatEvent::StreamEnd(StreamEndData {
        message: ChatMessage {
            message_id: message_id.map(ChatMessageId),
            timestamp: now_ms(),
            sender: MessageSender::Assistant {
                agent: "mock-native-child".to_owned(),
            },
            content: response_text,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: Some(ModelInfo {
                model: MOCK_MODEL.to_owned(),
            }),
            token_usage: Some(MessageTokenUsage::request_and_turn_known(
                TokenUsage {
                    input_tokens: 250,
                    output_tokens: 80,
                    total_tokens: 330,
                    cached_prompt_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    reasoning_tokens: Some(0),
                },
                TokenUsage {
                    input_tokens: 250,
                    output_tokens: 80,
                    total_tokens: 330,
                    cached_prompt_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    reasoning_tokens: Some(0),
                },
            )),
            context_breakdown: None,
            images: None,
        },
    }));
    let _ = event_tx.send(ChatEvent::TypingStatusChanged(false));
}

#[derive(Clone)]
pub(super) struct MockAgentControlAwaitMcp {
    url: String,
    authorization: Option<String>,
}

pub(super) fn agent_control_await_mcp(
    startup_mcp_servers: &[StartupMcpServer],
) -> Option<MockAgentControlAwaitMcp> {
    startup_mcp_servers
        .iter()
        .find(|server| server.name == crate::agent_control_mcp::AGENT_CONTROL_AWAIT_MCP_SERVER_NAME)
        .and_then(|server| match &server.transport {
            StartupMcpTransport::Http { url, headers, .. } => Some(MockAgentControlAwaitMcp {
                url: url.clone(),
                authorization: headers
                    .get(axum::http::header::AUTHORIZATION.as_str())
                    .cloned(),
            }),
            StartupMcpTransport::Stdio { .. } => None,
        })
}

pub(super) async fn agent_control_await(
    events_tx: &MockEventSender,
    config: Option<&MockAgentControlAwaitMcp>,
    step: MockAgentControlAwait,
) -> bool {
    let MockAgentControlAwait {
        message_id,
        agent_ids,
    } = step;
    let tool_call_id = format!("mock-agent-control-await-{}", Uuid::new_v4());
    let tool_name = "tyde_await_agents";
    let typed_agent_ids = agent_ids.iter().cloned().map(AgentId).collect::<Vec<_>>();
    let arguments = json!({ "agent_ids": agent_ids.clone() });

    if !events_tx.send_event(tool_request(ToolRequest {
        tool_call_id: tool_call_id.clone(),
        tool_type: ToolRequestType::TydeAwaitAgents {
            agent_ids: typed_agent_ids,
        },
    })) {
        return false;
    }
    if let Some(progress) = await_progress_data_for_tool(&tool_call_id, tool_name, &arguments)
        && !events_tx.send_event(tool_progress(progress))
    {
        return false;
    }

    let result = match config {
        Some(config) => call_agent_control_await_mcp(config, &agent_ids).await,
        None => Err("mock backend has no tyde-agent-await MCP server".to_owned()),
    };
    let (outcome, response_text) = match result {
        Ok(body) => match tyde_tool_result(tool_name, &body) {
            Ok(Some(tool_result)) => (
                ToolExecutionOutcome::Succeeded {
                    result: tool_result,
                },
                format!("mock agent-control await completed: {body}"),
            ),
            Ok(None) => unreachable!("canonical await tool must normalize"),
            Err(error) => (
                ToolExecutionOutcome::Failed {
                    message: "tyde_await_agents result normalization failed".to_owned(),
                    details: Some(error.to_string()),
                    normalization_failure: Some(error.normalization_failure),
                },
                format!("mock agent-control await normalization failed: {error}"),
            ),
        },
        Err(error) => (
            ToolExecutionOutcome::Failed {
                message: "tyde_await_agents failed".to_owned(),
                details: Some(error.clone()),
                normalization_failure: None,
            },
            format!("mock agent-control await failed: {error}"),
        ),
    };

    if !events_tx.send_event(tool_completed(ToolExecutionCompletedData {
        tool_call_id,
        outcome,
    })) {
        return false;
    }
    if !events_tx.send_event(stream_delta(response_text.clone())) {
        return false;
    }
    events_tx.send_event(stream_end(mock_assistant_message(
        message_id.map(ChatMessageId),
        response_text,
    )))
}

async fn call_agent_control_await_mcp(
    config: &MockAgentControlAwaitMcp,
    agent_ids: &[String],
) -> Result<Value, String> {
    let response = post_mcp_json(
        config,
        &json!({
            "jsonrpc": "2.0",
            "id": "mock-agent-control-await",
            "method": "tools/call",
            "params": {
                "name": "tyde_await_agents",
                "arguments": {
                    "agent_ids": agent_ids,
                }
            }
        }),
    )
    .await?;
    let result = response
        .get("result")
        .ok_or_else(|| format!("MCP response missing result: {response}"))?;
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("MCP result missing isError: {response}"))?;
    if is_error {
        return Err(format!("MCP tool call failed: {response}"));
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("MCP response missing content text: {response}"))?;
    serde_json::from_str(text).map_err(|err| format!("failed to parse MCP result JSON: {err}"))
}

async fn post_mcp_json(config: &MockAgentControlAwaitMcp, body: &Value) -> Result<Value, String> {
    let (addr, target) = parse_http_url(&config.url)?;
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|err| format!("connect {addr} failed: {err}"))?;
    let body_bytes =
        serde_json::to_vec(body).map_err(|err| format!("serialize MCP request failed: {err}"))?;
    let authorization = config
        .authorization
        .as_ref()
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {addr}\r\n{authorization}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| format!("write MCP HTTP headers failed: {err}"))?;
    stream
        .write_all(&body_bytes)
        .await
        .map_err(|err| format!("write MCP HTTP body failed: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("flush MCP HTTP request failed: {err}"))?;

    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .await
        .map_err(|err| format!("read MCP HTTP response failed: {err}"))?;
    let header_end = response_bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "MCP HTTP response missing header terminator".to_owned())?;
    let header = std::str::from_utf8(&response_bytes[..header_end])
        .map_err(|err| format!("MCP HTTP response header was not UTF-8: {err}"))?;
    if !header.starts_with("HTTP/1.1 200") {
        return Err(format!("unexpected MCP HTTP response header: {header}"));
    }
    let response_body = std::str::from_utf8(&response_bytes[header_end + 4..])
        .map_err(|err| format!("MCP HTTP response body was not UTF-8: {err}"))?;
    let json_str = response_body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .ok_or_else(|| format!("no SSE data line in MCP response body: {response_body}"))?;
    serde_json::from_str(json_str)
        .map_err(|err| format!("failed to parse MCP SSE JSON response: {err}"))
}

fn parse_http_url(url: &str) -> Result<(&str, &str), String> {
    let without_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("expected http:// URL, got {url}"))?;
    let slash = without_scheme
        .find('/')
        .ok_or_else(|| format!("expected path in URL {url}"))?;
    Ok((&without_scheme[..slash], &without_scheme[slash..]))
}
