use protocol::{
    AgentId, ChatMessageId, ContextBreakdown, MessageMetadataUpdateData, MessageTokenUsage,
    SessionId,
};
use uuid::Uuid;

use crate::backend::BackendEvent;

use super::MOCK_MODEL;
use super::emit;
use super::gate::{MockGate, MockGateHandle};
use super::{mock_prompt_history, startup_mcp_response_prefix};

#[derive(Debug, Clone, Default)]
pub struct MockScript {
    pub(super) turns: Vec<MockTurn>,
    pub(super) unbounded_echo: bool,
    pub(super) user_bubbles: bool,
    pub(super) busy_self_turn_once: bool,
}

impl MockScript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn one(turn: MockTurn) -> Self {
        Self {
            turns: vec![turn],
            ..Self::default()
        }
    }

    pub fn then(mut self, turn: MockTurn) -> Self {
        self.turns.push(turn);
        self
    }

    pub fn with_unbounded_echo(mut self) -> Self {
        self.unbounded_echo = true;
        self
    }

    pub fn with_user_bubbles(mut self) -> Self {
        self.user_bubbles = true;
        self
    }

    pub fn with_busy_self_turn_once(mut self) -> Self {
        self.busy_self_turn_once = true;
        self
    }
}

#[derive(Debug, Clone)]
pub enum MockLaunch {
    Script(MockScript),
    CloseBeforeResumeBarrier,
}

#[derive(Debug, Clone)]
pub struct MockTurn {
    pub(super) body: MockTurnBody,
    pub(super) finish: MockTurnFinish,
}

#[derive(Debug, Clone)]
pub(super) enum MockTurnBody {
    Steps(Vec<MockStep>),
    RenderedEcho,
    RenderedHistoryJoin,
}

impl MockTurn {
    pub(super) fn compact(session_id: &SessionId, prompt_index: usize, prompt: &str) -> Self {
        Self::done(
            emit::compact_turn_frames(session_id, prompt_index, prompt)
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::done(text_steps(text.into(), TextShape::default()))
    }

    pub fn gated_text(text: impl Into<String>, gate: &MockGateHandle) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                gate: Some(gate.gate()),
                ..TextShape::default()
            },
        ))
    }

    /// [`MockTurn::gated_text`] that spawns a backend-native child **mid
    /// turn** — after the parent's stream end but before the gate and the
    /// trailing idle. Gate entry is therefore causally after the child's
    /// full lifecycle has been emitted, while the parent is still provably
    /// busy: the shape for busy-parent child-completion tests.
    /// ([`MockTurn::with_native_child`] runs the child after the parent
    /// idles, matching the legacy sentinel placement.)
    pub fn gated_text_with_busy_native_child(
        text: impl Into<String>,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
        gate: &MockGateHandle,
    ) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                busy_native_child: Some(MockNativeChild {
                    name: child_name.into(),
                    prompt: child_prompt.into(),
                    lifecycle: MockChildLifecycle::CompleteAndRetain,
                }),
                gate: Some(gate.gate()),
                ..TextShape::default()
            },
        ))
    }

    pub fn text_without_usage(text: impl Into<String>) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                usage: TextUsage::Omitted,
                ..TextShape::default()
            },
        ))
    }

    pub fn text_with_late_usage(text: impl Into<String>) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                usage: TextUsage::Late,
                ..TextShape::default()
            },
        ))
    }

    pub fn text_with_context_250k(text: impl Into<String>) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                context_250k: true,
                ..TextShape::default()
            },
        ))
    }

    pub fn text_with_mid_turn_error(
        text: impl Into<String>,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                mid_turn_error: Some(diagnostic.into()),
                ..TextShape::default()
            },
        ))
    }

    pub fn echo() -> Self {
        Self {
            body: MockTurnBody::RenderedEcho,
            finish: MockTurnFinish::Done,
        }
    }

    pub fn history_join() -> Self {
        Self {
            body: MockTurnBody::RenderedHistoryJoin,
            finish: MockTurnFinish::Done,
        }
    }

    pub fn held_text(text: impl Into<String>) -> Self {
        Self::held(held_steps(text.into()), true)
    }

    pub fn held_text_uninterruptible(text: impl Into<String>) -> Self {
        Self::held(held_steps(text.into()), false)
    }

    pub fn held_text_with_live_native_child(
        text: impl Into<String>,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
    ) -> Self {
        let mut steps = held_steps(text.into());
        steps.push(MockStep::SpawnNativeChild(MockNativeChild {
            name: child_name.into(),
            prompt: child_prompt.into(),
            lifecycle: MockChildLifecycle::LiveUntilInterrupt,
        }));
        Self::held(steps, true)
    }

    pub fn exit_plan_request(tool_call_id: impl Into<String>, plan: impl Into<String>) -> Self {
        let tool_call_id = tool_call_id.into();
        let steps = emit::exit_plan_mode_request_frames(
            false,
            &tool_call_id,
            &plan.into(),
            "/tmp/mock/mock-plan.md",
        )
        .into_iter()
        .map(MockStep::emit)
        .collect();
        Self::await_tool_response(
            steps,
            MockPendingTool {
                tool_call_id,
                gate_before_completion: None,
                gate_after_completion: None,
            },
        )
    }

    pub fn exit_plan_request_stream_end_first(
        tool_call_id: impl Into<String>,
        plan: impl Into<String>,
        gate_before: &MockGateHandle,
        gate_after: &MockGateHandle,
    ) -> Self {
        let tool_call_id = tool_call_id.into();
        let steps = emit::exit_plan_mode_request_frames(
            true,
            &tool_call_id,
            &plan.into(),
            "/tmp/mock/mock-plan.md",
        )
        .into_iter()
        .map(MockStep::emit)
        .collect();
        Self::await_tool_response(
            steps,
            MockPendingTool {
                tool_call_id,
                gate_before_completion: Some(gate_before.gate()),
                gate_after_completion: Some(gate_after.gate()),
            },
        )
    }

    pub fn agent_control_await(agent_ids: impl IntoIterator<Item = AgentId>) -> Self {
        let message_id = Some(Uuid::new_v4().to_string());
        let steps = vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::stream_start("mock", Some(MOCK_MODEL.to_owned()))),
            MockStep::AgentControlAwait(MockAgentControlAwait {
                message_id,
                agent_ids: agent_ids.into_iter().map(|id| id.0).collect(),
            }),
            MockStep::emit(emit::typing(false)),
        ];
        Self::done(steps)
    }

    pub fn agent_control_send_message(agent_id: AgentId, message: impl Into<String>) -> Self {
        Self::done(
            emit::agent_control_send_message_frames(agent_id, message.into())
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn error_card(message: impl Into<String>) -> Self {
        Self::done(vec![MockStep::emit(emit::error_card(&message.into()))])
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::done(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::cancelled(message.into())),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn tool_failure_without_idle() -> Self {
        Self::done(
            emit::tool_failure_without_idle_frames("mock-failed-tool")
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn codex_internal_error_tail() -> Self {
        Self::done(
            emit::codex_internal_error_tail_frames()
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn empty_agent_control_output() -> Self {
        Self::done(
            emit::empty_agent_control_output_frames()
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn orchestration_started() -> Self {
        Self::done(
            emit::orchestration_frames()
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn busy_then_close_stream(gate: &MockGateHandle) -> Self {
        Self::close_stream(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::Gate(gate.gate()),
        ])
    }

    pub fn with_duplicate_idle(self) -> Self {
        self.with_appended_steps(vec![MockStep::emit(emit::typing(false))])
    }

    pub fn with_active_idle_cycle(self) -> Self {
        self.with_appended_steps(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn with_native_child(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            name: child_name.into(),
            prompt: child_prompt.into(),
            lifecycle: MockChildLifecycle::CompleteAndRetain,
        })])
    }

    pub fn with_dropped_native_child(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            name: child_name.into(),
            prompt: child_prompt.into(),
            lifecycle: MockChildLifecycle::CompleteAndDrop,
        })])
    }

    fn with_appended_steps(mut self, extra: Vec<MockStep>) -> Self {
        match &mut self.body {
            MockTurnBody::Steps(steps) => steps.extend(extra),
            MockTurnBody::RenderedEcho | MockTurnBody::RenderedHistoryJoin => {
                panic!("post-idle step builders apply to frozen turns, not rendered ones")
            }
        }
        self
    }

    pub(super) fn done(steps: Vec<MockStep>) -> Self {
        Self {
            body: MockTurnBody::Steps(steps),
            finish: MockTurnFinish::Done,
        }
    }

    pub(super) fn held(steps: Vec<MockStep>, interruptible: bool) -> Self {
        Self {
            body: MockTurnBody::Steps(steps),
            finish: MockTurnFinish::Held { interruptible },
        }
    }

    pub(super) fn close_stream(steps: Vec<MockStep>) -> Self {
        Self {
            body: MockTurnBody::Steps(steps),
            finish: MockTurnFinish::CloseStream,
        }
    }

    pub(super) fn await_tool_response(steps: Vec<MockStep>, pending: MockPendingTool) -> Self {
        Self {
            body: MockTurnBody::Steps(steps),
            finish: MockTurnFinish::AwaitToolResponse(pending),
        }
    }

    pub(super) fn realize(
        self,
        session_id: &SessionId,
        input: &str,
    ) -> (Vec<MockStep>, MockTurnFinish) {
        let steps = match self.body {
            MockTurnBody::Steps(steps) => steps,
            MockTurnBody::RenderedEcho => text_steps(
                format!(
                    "{}mock backend response to: {input}",
                    startup_mcp_response_prefix(session_id)
                ),
                TextShape::default(),
            ),
            MockTurnBody::RenderedHistoryJoin => text_steps(
                format!(
                    "mock history: {}",
                    mock_prompt_history(session_id).join(" | ")
                ),
                TextShape::default(),
            ),
        };
        (steps, self.finish)
    }
}

#[derive(Default)]
struct TextShape {
    usage: TextUsage,
    context_250k: bool,
    mid_turn_error: Option<String>,
    /// Spawn a native child after the stream end (and any late usage), before
    /// the gate and the trailing idle — i.e. while the turn is still busy.
    busy_native_child: Option<MockNativeChild>,
    gate: Option<MockGate>,
}

#[derive(Default, PartialEq)]
enum TextUsage {
    #[default]
    Normal,
    Omitted,
    Late,
}

fn text_steps(text: String, shape: TextShape) -> Vec<MockStep> {
    let message_id = Some(Uuid::new_v4().to_string());
    let mut steps = vec![
        MockStep::emit(emit::typing(true)),
        MockStep::emit(emit::stream_start("mock", Some(MOCK_MODEL.to_owned()))),
        MockStep::emit(emit::stream_delta(text.clone())),
    ];
    if let Some(diagnostic) = &shape.mid_turn_error {
        steps.push(MockStep::emit(emit::error_card(diagnostic)));
    }
    steps.push(MockStep::emit(emit::stream_end(protocol::ChatMessage {
        token_usage: (shape.usage == TextUsage::Normal).then(|| {
            MessageTokenUsage::request_and_turn_known(
                emit::mock_turn_token_usage(),
                emit::mock_turn_token_usage(),
            )
        }),
        context_breakdown: shape.context_250k.then_some(ContextBreakdown {
            system_prompt_bytes: 100_000,
            tool_io_bytes: 40_000,
            conversation_history_bytes: 60_000,
            reasoning_bytes: 20_000,
            context_injection_bytes: 30_000,
            input_tokens: 250_000,
            context_window: 300_000,
        }),
        ..emit::mock_assistant_message(message_id.clone().map(ChatMessageId), text)
    })));
    if shape.usage == TextUsage::Late
        && let Some(message_id) = message_id
    {
        steps.push(MockStep::emit(emit::metadata_update(
            MessageMetadataUpdateData {
                message_id: ChatMessageId(message_id),
                model_info: None,
                token_usage: Some(MessageTokenUsage::request_and_turn_known(
                    emit::mock_turn_token_usage(),
                    emit::mock_turn_token_usage(),
                )),
                context_breakdown: None,
            },
        )));
    }
    if let Some(child) = shape.busy_native_child {
        steps.push(MockStep::SpawnNativeChild(child));
    }
    if let Some(gate) = shape.gate {
        steps.push(MockStep::Gate(gate));
    }
    steps.push(MockStep::emit(emit::typing(false)));
    steps
}

fn held_steps(text: String) -> Vec<MockStep> {
    let message_id = Some(Uuid::new_v4().to_string());
    vec![
        MockStep::emit(emit::typing(true)),
        MockStep::emit(emit::stream_start("mock", Some(MOCK_MODEL.to_owned()))),
        MockStep::emit(emit::stream_delta(text.clone())),
        MockStep::emit(emit::stream_end(emit::mock_assistant_message(
            message_id.map(ChatMessageId),
            text,
        ))),
    ]
}

#[derive(Debug, Clone)]
pub(super) enum MockStep {
    Emit(Box<BackendEvent>),
    Gate(MockGate),
    SpawnNativeChild(MockNativeChild),
    AgentControlAwait(MockAgentControlAwait),
}

impl MockStep {
    pub(super) fn emit(event: BackendEvent) -> Self {
        Self::Emit(Box::new(event))
    }
}

#[derive(Debug, Clone)]
pub(super) enum MockTurnFinish {
    Done,
    Held { interruptible: bool },
    CloseStream,
    AwaitToolResponse(MockPendingTool),
}

#[derive(Debug, Clone)]
pub(super) struct MockPendingTool {
    pub(super) tool_call_id: String,
    pub(super) gate_before_completion: Option<MockGate>,
    pub(super) gate_after_completion: Option<MockGate>,
}

#[derive(Debug, Clone)]
pub(super) struct MockNativeChild {
    pub(super) name: String,
    pub(super) prompt: String,
    pub(super) lifecycle: MockChildLifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MockChildLifecycle {
    CompleteAndRetain,
    CompleteAndDrop,
    LiveUntilInterrupt,
}

#[derive(Debug, Clone)]
pub(super) struct MockAgentControlAwait {
    pub(super) message_id: Option<String>,
    pub(super) agent_ids: Vec<String>,
}
