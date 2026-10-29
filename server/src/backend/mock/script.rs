use protocol::{
    AgentId, ChatMessageId, ContextBreakdown, MessageMetadataUpdateData, MessageTokenUsage,
    SessionId, SlashCommand,
};
use uuid::Uuid;

use crate::backend::BackendEvent;

use super::MOCK_MODEL;
use super::emit;
use super::gate::{MockGate, MockGateHandle};
use super::{mock_prompt_history, startup_mcp_response_prefix};

#[derive(Debug, Clone, Copy)]
pub enum MockCompactionFailure {
    NotDispatched,
    Rejected,
    QuotaExceeded,
    QuotaExceededOnce,
}

/// Where a running command's request lands relative to the response that
/// issued it.
#[derive(Debug, Clone, Copy)]
pub enum RunningCommandShape {
    RequestAfterResponse,
    RequestInsideEndedResponse,
    RequestInsideOpenResponse,
}

/// How the backend abandons a response that issued commands.
#[derive(Debug, Clone, Copy)]
pub enum AbandonedResponse {
    Interrupted,
    Replaced,
}

#[derive(Debug, Clone, Default)]
pub struct MockScript {
    #[cfg(all(feature = "test-support", target_os = "linux"))]
    pub(super) native_process: Option<Box<crate::backend::subprocess::test_support::Fixture>>,
    pub(super) compaction_coordinator: Option<crate::backend::BackendCompactionCoordinator>,
    pub(super) compaction_availability: Option<crate::backend::BackendCompactionAvailability>,
    pub(super) compaction_failure: Option<MockCompactionFailure>,
    pub(super) turns: Vec<MockTurn>,
    pub(super) unbounded_echo: bool,
    pub(super) user_bubbles: bool,
    pub(super) busy_self_turn_once: bool,
    pub(super) resume_continuation: Option<(MockGate, MockGate)>,
    pub(super) resume_replay: Option<MockResumeReplay>,
    pub(super) mid_turn_steering: bool,
    pub(super) shutdown_gate: Option<MockGate>,
    pub(super) native_child_idle_gate: Option<MockGate>,
    /// Parks every follow-up message before the provider accepts it.
    pub(super) send_gate: Option<MockGate>,
    pub(super) compaction_observation_gates: Option<(MockGate, MockGate)>,
    /// Advertised as the session's slash-command set before the launch turn.
    pub(super) slash_commands: Option<Vec<SlashCommand>>,
}

impl MockScript {
    #[cfg(all(feature = "test-support", target_os = "linux"))]
    pub fn with_native_process(
        mut self,
        fixture: crate::backend::subprocess::test_support::Fixture,
    ) -> Self {
        self.native_process = Some(Box::new(fixture));
        self
    }
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

    pub fn with_compaction_coordinator(
        mut self,
        coordinator: crate::backend::BackendCompactionCoordinator,
    ) -> Self {
        self.compaction_coordinator = Some(coordinator);
        self
    }

    pub fn with_compaction_availability(
        mut self,
        availability: crate::backend::BackendCompactionAvailability,
    ) -> Self {
        self.compaction_availability = Some(availability);
        self
    }

    pub fn with_compaction_failure(mut self, failure: MockCompactionFailure) -> Self {
        self.compaction_failure = Some(failure);
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

    pub fn with_resume_continuation(
        mut self,
        replay: &MockGateHandle,
        finish: &MockGateHandle,
    ) -> Self {
        self.resume_continuation = Some((replay.gate(), finish.gate()));
        self
    }

    pub fn with_controlled_resume_replay(mut self, replay: &MockResumeReplay) -> Self {
        self.resume_replay = Some(replay.clone());
        self
    }

    pub fn with_busy_self_turn_once(mut self) -> Self {
        self.busy_self_turn_once = true;
        self
    }

    /// Take `Backend::steer` into the running turn instead of reporting it
    /// unsupported.
    pub fn with_mid_turn_steering(mut self) -> Self {
        self.mid_turn_steering = true;
        self
    }

    pub fn with_slash_commands(mut self, commands: Vec<SlashCommand>) -> Self {
        self.slash_commands = Some(commands);
        self
    }

    /// Park the backend inside `Backend::shutdown` until the gate is released,
    /// so a test can observe what a close does while teardown is still running.
    pub fn with_late_compaction_observation(
        mut self,
        release: &MockGateHandle,
        sent: &MockGateHandle,
    ) -> Self {
        self.compaction_observation_gates = Some((release.gate(), sent.gate()));
        self
    }

    pub fn with_shutdown_gate(mut self, gate: &MockGateHandle) -> Self {
        self.shutdown_gate = Some(gate.gate());
        self
    }

    pub fn with_native_child_idle_gate(mut self, gate: &MockGateHandle) -> Self {
        self.native_child_idle_gate = Some(gate.gate());
        self
    }

    pub fn with_send_gate(mut self, gate: &MockGateHandle) -> Self {
        self.send_gate = Some(gate.gate());
        self
    }
}

/// Synchronous replay production lets a sim queue history before advancing its
/// paused clock, without scheduling the server between those two operations.
#[derive(Debug, Clone, Default)]
pub struct MockResumeReplay {
    live_turn_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ready: std::sync::Arc<tokio::sync::Notify>,
    sender:
        std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<BackendEvent>>>>,
    send_gate: std::sync::Arc<std::sync::Mutex<Option<MockGate>>>,
}

impl MockResumeReplay {
    /// Parks every message sent to the resumed session on `gate`.
    pub fn gate_sends(&self, gate: &MockGateHandle) {
        *self.send_gate.lock().expect("mock replay send gate mutex") = Some(gate.gate());
    }

    pub(super) fn send_gate(&self) -> Option<MockGate> {
        self.send_gate
            .lock()
            .expect("mock replay send gate mutex")
            .clone()
    }

    pub(super) fn bind(&self, sender: tokio::sync::mpsc::UnboundedSender<BackendEvent>) {
        *self.sender.lock().expect("mock replay sender mutex") = Some(sender);
        self.ready.notify_one();
    }

    pub async fn wait_until_started(&self) {
        self.ready.notified().await;
    }

    pub fn history_batch(&self, count: usize) {
        let sender = self.sender.lock().expect("mock replay sender mutex");
        let sender = sender.as_ref().expect("mock resume has not started");
        for _ in 0..count {
            sender
                .send(emit::message_added(emit::mock_assistant_message(
                    Some(ChatMessageId(Uuid::new_v4().to_string())),
                    "replayed history".to_owned(),
                )))
                .expect("mock replay stream closed");
        }
    }

    pub(super) fn is_turn_active(&self) -> bool {
        self.live_turn_active
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn live_turn(&self, text: &str) {
        self.start_live_turn(text);
        self.finish_live_turn(text);
    }

    pub fn start_live_turn(&self, text: &str) {
        let sender = self.sender.lock().expect("mock replay sender mutex");
        let sender = sender.as_ref().expect("mock resume has not started");
        self.live_turn_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for event in [
            emit::typing(true),
            emit::stream_start("mock", Some(MOCK_MODEL.to_owned())),
            emit::stream_delta(text.to_owned()),
        ] {
            sender.send(event).expect("mock replay stream closed");
        }
    }

    pub fn finish_live_turn(&self, text: &str) {
        let sender = self.sender.lock().expect("mock replay sender mutex");
        let sender = sender.as_ref().expect("mock resume has not started");
        self.live_turn_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        for event in [
            emit::stream_end(emit::mock_assistant_message(
                Some(ChatMessageId(Uuid::new_v4().to_string())),
                text.to_owned(),
            )),
            emit::typing(false),
        ] {
            sender.send(event).expect("mock replay stream closed");
        }
    }

    pub fn goal_state(&self, goal: Option<protocol::NativeGoal>) {
        self.sender
            .lock()
            .expect("mock replay sender mutex")
            .as_ref()
            .expect("mock resume has not started")
            .send(BackendEvent::Chat(protocol::ChatEvent::GoalChanged(goal)))
            .expect("mock replay stream closed");
    }

    pub fn complete(&self) {
        self.sender
            .lock()
            .expect("mock replay sender mutex")
            .as_ref()
            .expect("mock resume has not started")
            .send(BackendEvent::ResumeReplayComplete(Ok(())))
            .expect("mock replay stream closed");
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
    RenderedEcho(Option<MockGate>),
    RenderedTextForInput {
        text: String,
        required: String,
        gate: MockGate,
    },
    RenderedHistoryJoin,
    RenderedWorkspaceRoots,
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

    pub fn chat_events(events: Vec<protocol::ChatEvent>) -> Self {
        Self::done(
            std::iter::once(emit::typing(true))
                .chain(events.into_iter().map(BackendEvent::Chat))
                .chain(std::iter::once(emit::typing(false)))
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::done(text_steps(text.into(), TextShape::default()))
    }

    pub fn overlapping_usage(start: &MockGateHandle) -> Self {
        let requests = [
            protocol::TokenUsage {
                input_tokens: 16_127,
                output_tokens: 857,
                total_tokens: 16_984,
                cached_prompt_tokens: Some(0),
                cache_creation_input_tokens: None,
                reasoning_tokens: Some(321),
            },
            protocol::TokenUsage {
                input_tokens: 7_918,
                output_tokens: 7_266,
                total_tokens: 15_184,
                cached_prompt_tokens: Some(16_128),
                cache_creation_input_tokens: None,
                reasoning_tokens: Some(4_200),
            },
        ];
        let turn_id = protocol::ModelTurnId(Uuid::new_v4().to_string());
        let mut total = protocol::TokenUsage::default();
        let mut steps = vec![
            MockStep::Gate(start.gate()),
            MockStep::emit(emit::typing(true)),
        ];
        let mut updates = Vec::new();
        for (index, request) in requests.into_iter().enumerate() {
            total.input_tokens += request.input_tokens;
            total.output_tokens += request.output_tokens;
            total.total_tokens += request.total_tokens;
            total.cached_prompt_tokens = Some(
                total.cached_prompt_tokens.unwrap_or(0) + request.cached_prompt_tokens.unwrap_or(0),
            );
            total.reasoning_tokens =
                Some(total.reasoning_tokens.unwrap_or(0) + request.reasoning_tokens.unwrap_or(0));
            let message_id = ChatMessageId(Uuid::new_v4().to_string());
            let usage = MessageTokenUsage::request_and_turn_known(request.clone(), total.clone());
            steps.extend([
                MockStep::emit(emit::stream_start("mock", Some(MOCK_MODEL.to_owned()))),
                MockStep::emit(emit::stream_delta(format!("usage response {}", index + 1))),
                MockStep::emit(BackendEvent::ModelRequestTokenUsage(
                    protocol::ModelRequestTokenUsage {
                        request_id: protocol::ModelRequestId {
                            turn_id: turn_id.clone(),
                            sequence: index as u32 + 1,
                        },
                        request,
                        turn: total.clone(),
                        cumulative: total.clone(),
                        model_context_window: None,
                        current_context_usage: None,
                        estimated_context_breakdown: None,
                    },
                )),
                MockStep::emit(emit::stream_end(protocol::ChatMessage {
                    token_usage: Some(usage.clone()),
                    ..emit::mock_assistant_message(
                        Some(message_id.clone()),
                        format!("usage response {}", index + 1),
                    )
                })),
            ]);
            updates.push(MessageMetadataUpdateData {
                message_id,
                model_info: None,
                token_usage: Some(usage),
                context_breakdown: None,
            });
        }
        // Repeat the latest snapshot, then deliver an older response's metadata
        // after idle. Neither changes the generation's measured total.
        steps.push(MockStep::emit(emit::metadata_update(updates[1].clone())));
        steps.push(MockStep::emit(emit::typing(false)));
        steps.push(MockStep::emit(emit::metadata_update(updates[0].clone())));
        Self::done(steps)
    }

    pub fn generation_usage_coverage(checkpoint: &MockGateHandle) -> Self {
        let measured = |count| {
            let usage = emit::mock_turn_token_usage();
            protocol::TokenUsage {
                input_tokens: usage.input_tokens * count,
                output_tokens: usage.output_tokens * count,
                total_tokens: usage.total_tokens * count,
                cached_prompt_tokens: usage.cached_prompt_tokens.map(|value| value * count),
                cache_creation_input_tokens: usage
                    .cache_creation_input_tokens
                    .map(|value| value * count),
                reasoning_tokens: usage.reasoning_tokens.map(|value| value * count),
            }
        };
        let missing_optionals = |mut usage: protocol::TokenUsage| {
            usage.cached_prompt_tokens = None;
            usage.cache_creation_input_tokens = None;
            usage.reasoning_tokens = None;
            usage
        };
        let unavailable = || {
            MessageTokenUsage::unavailable(
                protocol::TokenUsageUnavailableReason::BackendDidNotReport,
            )
        };
        let ids = (0..4)
            .map(|_| ChatMessageId(Uuid::new_v4().to_string()))
            .collect::<Vec<_>>();
        let metadata = |index: usize, usage| {
            MockStep::emit(emit::metadata_update(MessageMetadataUpdateData {
                message_id: ids[index].clone(),
                model_info: None,
                token_usage: Some(usage),
                context_breakdown: None,
            }))
        };
        let mut steps = vec![MockStep::emit(emit::typing(true))];
        for (index, usage) in [
            unavailable(),
            MessageTokenUsage::request_and_turn_known(measured(1), missing_optionals(measured(2))),
            unavailable(),
            MessageTokenUsage::request_known(missing_optionals(measured(1))),
        ]
        .into_iter()
        .enumerate()
        {
            steps.extend([
                MockStep::emit(emit::stream_start("mock", Some(MOCK_MODEL.to_owned()))),
                MockStep::emit(emit::stream_delta(format!(
                    "coverage response {}",
                    index + 1
                ))),
                MockStep::emit(emit::stream_end(protocol::ChatMessage {
                    token_usage: Some(usage),
                    ..emit::mock_assistant_message(
                        Some(ids[index].clone()),
                        format!("coverage response {}", index + 1),
                    )
                })),
            ]);
            if index == 1 {
                steps.extend([
                    MockStep::Gate(checkpoint.gate()),
                    metadata(
                        1,
                        MessageTokenUsage::request_and_turn_known(measured(1), measured(2)),
                    ),
                    metadata(
                        0,
                        MessageTokenUsage::request_and_turn_known(
                            measured(1),
                            missing_optionals(measured(1)),
                        ),
                    ),
                    metadata(0, unavailable()),
                    MockStep::Gate(checkpoint.gate()),
                ]);
            }
        }
        steps.extend([
            MockStep::Gate(checkpoint.gate()),
            metadata(
                3,
                MessageTokenUsage::request_and_turn_known(measured(1), measured(4)),
            ),
            MockStep::emit(emit::typing(false)),
        ]);
        Self::done(steps)
    }

    pub fn gated_text_for_input(
        text: impl Into<String>,
        required: impl Into<String>,
        gate: &MockGateHandle,
    ) -> Self {
        Self {
            body: MockTurnBody::RenderedTextForInput {
                text: text.into(),
                required: required.into(),
                gate: gate.gate(),
            },
            finish: MockTurnFinish::Done,
        }
    }

    pub fn workspace_roots() -> Self {
        Self {
            body: MockTurnBody::RenderedWorkspaceRoots,
            finish: MockTurnFinish::Done,
        }
    }

    pub fn background_task_wait(tool_call_id: &str, drain: &MockGateHandle) -> Self {
        let mut steps = vec![MockStep::emit(emit::typing(true))];
        steps.extend(
            emit::background_task_started_frames(tool_call_id, false)
                .into_iter()
                .map(MockStep::emit),
        );
        steps.push(MockStep::emit(emit::typing(false)));
        steps.push(MockStep::Gate(drain.gate()));
        steps.extend(
            emit::background_task_finished_frames(tool_call_id)
                .into_iter()
                .map(MockStep::emit),
        );
        Self::done(steps)
    }

    /// Sends `message` through the real `tyde-tychat` MCP mid-turn, parks at
    /// `gate`, then finishes with `text` as the final reply.
    pub fn tychat_send_then_text(
        message: impl Into<String>,
        gate: &MockGateHandle,
        text: impl Into<String>,
    ) -> Self {
        Self::tychat_send_then_text_with_work(message, gate, false, text)
    }

    /// Like [`Self::tychat_send_then_text`], but calls another tool after the
    /// send and before the final reply when `then_work` is set.
    pub fn tychat_send_then_text_with_work(
        message: impl Into<String>,
        gate: &MockGateHandle,
        then_work: bool,
        text: impl Into<String>,
    ) -> Self {
        let mut steps = text_steps(text.into(), TextShape::default());
        let mut inserted = vec![MockStep::TychatSend(message.into())];
        // The provider's view of the send call lands after the MCP request.
        inserted.extend(
            emit::mcp_tool_frames("mock-tychat-send", "mcp__tyde-tychat__tychat_send_message")
                .into_iter()
                .map(MockStep::emit),
        );
        inserted.push(MockStep::Gate(gate.gate()));
        if then_work {
            inserted.extend(
                emit::mcp_tool_frames(
                    "mock-tychat-work",
                    "mcp__tyde-agent-control__tyde_list_agents",
                )
                .into_iter()
                .map(MockStep::emit),
            );
        }
        steps.splice(1..1, inserted);
        Self::done(steps)
    }

    pub fn with_tyde_permissions_probe(mut self) -> Self {
        let MockTurnBody::Steps(steps) = &mut self.body else {
            panic!("permission probe requires a scripted text turn");
        };
        steps.insert(1, MockStep::TydePermissions);
        self
    }

    pub fn text_after_gate(text: impl Into<String>, gate: &MockGateHandle) -> Self {
        let mut steps = vec![MockStep::Gate(gate.gate())];
        steps.extend(text_steps(text.into(), TextShape::default()));
        Self::done(steps)
    }

    pub fn text_completion_after_gate(text: impl Into<String>, gate: &MockGateHandle) -> Self {
        let mut steps = text_steps(text.into(), TextShape::default());
        let position = steps.iter().position(|step| matches!(step, MockStep::Emit(event) if matches!(&**event, BackendEvent::Chat(protocol::ChatEvent::StreamEnd(_))))).expect("text completion boundary");
        steps.insert(position, MockStep::Gate(gate.gate()));
        Self::done(steps)
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

    /// A gated text turn the provider has not started until `start` is
    /// released; each steer held before then enters `steer_waiting`.
    pub fn gated_text_after_turn_start(
        text: impl Into<String>,
        start: &MockGateHandle,
        steer_waiting: &MockGateHandle,
        gate: &MockGateHandle,
    ) -> Self {
        let mut steps = vec![MockStep::TurnStartGate {
            start: start.gate(),
            steer_waiting: steer_waiting.gate(),
        }];
        steps.extend(text_steps(
            text.into(),
            TextShape {
                gate: Some(gate.gate()),
                ..TextShape::default()
            },
        ));
        Self::done(steps)
    }

    pub fn gated_held_text(text: impl Into<String>, gate: &MockGateHandle) -> Self {
        let mut steps = held_steps(text.into());
        steps.push(MockStep::Gate(gate.gate()));
        Self::held(steps, true)
    }

    /// Spawns a backend-native child mid-turn, before the gate and idle.
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
                    registration_gate: None,
                    usage: MockNativeChildUsage::Split,
                    emit_timing_tool: false,
                    name: child_name.into(),
                    prompt: child_prompt.into(),
                    grandchild: None,
                    lifecycle: MockChildLifecycle::CompleteAndRetain,
                }),
                gate: Some(gate.gate()),
                ..TextShape::default()
            },
        ))
    }

    /// One delta per character, the way Codex actually streams.
    ///
    /// The provider's chunking is not a detail the client should inherit: a
    /// 1,500-character answer arrives from Codex as ~1,000 events and from
    /// Claude as ~34, and it is the event count — not the text — that decides
    /// whether a client can keep up with the server it is rendering.
    pub fn text_streamed_per_character(text: impl Into<String>) -> Self {
        Self::done(text_steps(
            text.into(),
            TextShape {
                per_character: true,
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

    pub fn with_gate_before_usage(mut self, gate: &MockGateHandle) -> Self {
        let MockTurnBody::Steps(steps) = &mut self.body else {
            panic!("usage gate requires a scripted turn");
        };
        let position = steps.iter().position(|step| matches!(step, MockStep::Emit(event) if matches!(&**event, BackendEvent::Chat(protocol::ChatEvent::MessageMetadataUpdated(_))))).expect("late usage metadata");
        steps.insert(position, MockStep::Gate(gate.gate()));
        self
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

    pub fn with_context_usage(mut self, input_tokens: u64, context_window: u64) -> Self {
        let MockTurnBody::Steps(steps) = &mut self.body else {
            panic!("context usage requires a scripted turn");
        };
        steps.insert(
            steps.len().saturating_sub(1),
            MockStep::emit(BackendEvent::ModelRequestTokenUsage(
                protocol::ModelRequestTokenUsage {
                    request_id: protocol::ModelRequestId {
                        turn_id: protocol::ModelTurnId(Uuid::new_v4().to_string()),
                        sequence: 1,
                    },
                    request: emit::mock_turn_token_usage(),
                    turn: emit::mock_turn_token_usage(),
                    cumulative: emit::mock_turn_token_usage(),
                    model_context_window: Some(context_window),
                    current_context_usage: Some(protocol::CurrentContextUsage::Known {
                        input_tokens,
                        context_window,
                    }),
                    estimated_context_breakdown: None,
                },
            )),
        );
        self
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
            body: MockTurnBody::RenderedEcho(None),
            finish: MockTurnFinish::Done,
        }
    }

    pub fn held_echo() -> Self {
        Self {
            body: MockTurnBody::RenderedEcho(None),
            finish: MockTurnFinish::Held {
                interruptible: true,
            },
        }
    }

    pub fn gated_echo(gate: &MockGateHandle) -> Self {
        Self {
            body: MockTurnBody::RenderedEcho(Some(gate.gate())),
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

    /// A turn parked inside a command the backend never reports finishing.
    pub fn held_running_command(tool_call_id: &str, shape: RunningCommandShape) -> Self {
        Self::held(
            emit::running_command_frames(tool_call_id, shape)
                .into_iter()
                .map(MockStep::emit)
                .collect(),
            true,
        )
    }

    /// A turn whose response issues two commands and is then abandoned
    /// without ending; only `finished_id` is ever reported finishing.
    pub fn abandoned_response_commands(
        running_id: &str,
        finished_id: &str,
        abandon: AbandonedResponse,
    ) -> Self {
        Self::done(
            emit::abandoned_response_command_frames(running_id, finished_id, abandon)
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
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
            registration_gate: None,
            usage: MockNativeChildUsage::Split,
            emit_timing_tool: false,
            name: child_name.into(),
            prompt: child_prompt.into(),
            grandchild: None,
            lifecycle: MockChildLifecycle::LiveUntilInterrupt,
        }));
        Self::held(steps, true)
    }

    pub fn async_question_request(tool_call_id: &str, gate: &MockGateHandle) -> Self {
        Self::done(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::tool_request(protocol::ToolRequest {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: "request_user_input_async".to_owned(),
                tool_type: protocol::ToolRequestType::AskUserQuestion {
                    mode: protocol::UserQuestionMode::NonBlocking,
                    questions: vec![protocol::AskUserQuestion {
                        id: None,
                        question: "Choose a color".to_owned(),
                        header: None,
                        options: Vec::new(),
                        answer_schema: None,
                        multi_select: false,
                    }],
                },
            })),
            MockStep::Gate(gate.gate()),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn blocking_question_request(tool_call_id: &str, gate: &MockGateHandle) -> Self {
        let mut turn = Self::async_question_request(tool_call_id, gate);
        if let MockTurnBody::Steps(steps) = &mut turn.body {
            for step in steps {
                if let MockStep::Emit(event) = step
                    && let BackendEvent::Chat(protocol::ChatEvent::ToolRequest(request)) =
                        &mut **event
                    && let protocol::ToolRequestType::AskUserQuestion { mode, .. } =
                        &mut request.tool_type
                {
                    *mode = protocol::UserQuestionMode::Blocking;
                }
            }
        }
        turn.finish = MockTurnFinish::AwaitToolResponse(MockPendingTool {
            tool_call_id: tool_call_id.to_owned(),
            gate_before_completion: None,
            gate_after_completion: None,
        });
        turn
    }

    pub fn structured_question_request(
        tool_call_id: &str,
        questions: Vec<protocol::AskUserQuestion>,
        gate: &MockGateHandle,
    ) -> Self {
        let mut turn = Self::blocking_question_request(tool_call_id, gate);
        let MockTurnBody::Steps(steps) = &mut turn.body else {
            unreachable!("blocking question turn has steps");
        };
        let request = steps
            .iter_mut()
            .find_map(|step| match step {
                MockStep::Emit(event) => match &mut **event {
                    BackendEvent::Chat(protocol::ChatEvent::ToolRequest(request)) => Some(request),
                    _ => None,
                },
                _ => None,
            })
            .expect("blocking question turn has a request");
        let protocol::ToolRequestType::AskUserQuestion {
            questions: pending, ..
        } = &mut request.tool_type
        else {
            unreachable!("blocking question turn asks a question");
        };
        *pending = questions;
        turn
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

    /// Protocol-server fixture: overlapping command and dependency wait,
    /// followed by background completion, without a second execution.
    pub fn timing_flow(
        child: AgentId,
        foreground: &MockGateHandle,
        background: &MockGateHandle,
    ) -> Self {
        let request = |id: &str, name: &str, tool_type| {
            MockStep::emit(emit::tool_request(protocol::ToolRequest {
                tool_call_id: id.to_owned(),
                tool_name: name.to_owned(),
                tool_type,
            }))
        };
        let mut steps = vec![
            MockStep::emit(emit::typing(true)),
            request(
                "timing-check",
                "run_command",
                protocol::ToolRequestType::RunCommand {
                    command: "./dev.sh check".to_owned(),
                    working_directory: "/tmp/test".to_owned(),
                },
            ),
            request(
                "timing-await",
                "tyde_await_agents",
                protocol::ToolRequestType::TydeAwaitAgents {
                    agent_ids: vec![child],
                },
            ),
            MockStep::Gate(foreground.gate()),
            MockStep::emit(emit::tool_completed(protocol::ToolExecutionCompletedData {
                tool_call_id: "timing-await".to_owned(),
                outcome: protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::TydeAwaitAgents {
                        ready: Vec::new(),
                        still_thinking: Vec::new(),
                    },
                },
            })),
            MockStep::emit(emit::tool_progress(protocol::ToolProgressData {
                tool_call_id: "timing-check".to_owned(),
                execution_mode: protocol::ToolExecutionMode::Background,
                cancellable: false,
                update: protocol::ToolProgressUpdate::Other {
                    payload: serde_json::json!({"status":"running"}),
                },
            })),
            MockStep::emit(emit::typing(false)),
            MockStep::Gate(background.gate()),
        ];
        steps.extend(
            emit::background_task_finished_frames("timing-check")
                .into_iter()
                .map(MockStep::emit),
        );
        Self::done(steps)
    }

    /// Responds, then ends the turn while a foreground read never completes.
    pub fn timing_model_then_unfinished_tool(
        tool_call_id: &str,
        model: &MockGateHandle,
        tool: &MockGateHandle,
    ) -> Self {
        Self::done(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::Gate(model.gate()),
            MockStep::emit(emit::tool_request(protocol::ToolRequest {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: "read_files".to_owned(),
                tool_type: protocol::ToolRequestType::ReadFiles {
                    file_paths: vec!["/tmp/test/notes.md".to_owned()],
                },
            })),
            MockStep::Gate(tool.gate()),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn text_with_late_zero_native_usage(
        text: impl Into<String>,
        gate: &MockGateHandle,
    ) -> Self {
        Self::text_with_late_native_usage(text, gate, 0)
    }

    pub fn text_with_late_native_usage(
        text: impl Into<String>,
        gate: &MockGateHandle,
        tokens: u64,
    ) -> Self {
        let mut steps = text_steps(
            text.into(),
            TextShape {
                usage: TextUsage::Omitted,
                ..TextShape::default()
            },
        );
        steps.push(MockStep::Gate(gate.gate()));
        steps.push(MockStep::emit(BackendEvent::ModelRequestTokenUsage(
            protocol::ModelRequestTokenUsage {
                request_id: protocol::ModelRequestId {
                    turn_id: protocol::ModelTurnId(format!("observed-native-{tokens}")),
                    sequence: 1,
                },
                request: protocol::TokenUsage {
                    output_tokens: tokens,
                    total_tokens: tokens,
                    ..Default::default()
                },
                turn: protocol::TokenUsage {
                    output_tokens: tokens,
                    total_tokens: tokens,
                    ..Default::default()
                },
                cumulative: protocol::TokenUsage {
                    output_tokens: tokens,
                    total_tokens: tokens,
                    ..Default::default()
                },
                model_context_window: None,
                current_context_usage: None,
                estimated_context_breakdown: None,
            },
        )));
        Self::done(steps)
    }

    pub fn text_with_cache_counter(
        text: impl Into<String>,
        cached_prompt_tokens: Option<u64>,
    ) -> Self {
        let mut steps = text_steps(text.into(), TextShape::default());
        for step in &mut steps {
            if let MockStep::Emit(event) = step
                && let BackendEvent::Chat(protocol::ChatEvent::StreamEnd(end)) = &mut **event
            {
                let mut usage = emit::mock_turn_token_usage();
                usage.cached_prompt_tokens = cached_prompt_tokens;
                end.message.token_usage = Some(MessageTokenUsage::request_and_turn_known(
                    usage.clone(),
                    usage,
                ));
            }
        }
        Self::done(steps)
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

    pub fn agent_control_send_message(
        agent_id: AgentId,
        message: impl Into<String>,
        interrupt: bool,
    ) -> Self {
        Self::done(
            emit::agent_control_send_message_frames(agent_id, message.into(), interrupt)
                .into_iter()
                .map(MockStep::emit)
                .collect(),
        )
    }

    pub fn error_card(message: impl Into<String>) -> Self {
        Self::done(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::error_card(&message.into())),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn error_before_typing(
        message: impl Into<String>,
        text: impl Into<String>,
        gate: &MockGateHandle,
    ) -> Self {
        let mut steps = vec![
            MockStep::emit(emit::error_card(&message.into())),
            MockStep::Gate(gate.gate()),
        ];
        steps.extend(text_steps(text.into(), TextShape::default()));
        Self::done(steps)
    }

    pub fn cancelled_before_typing(message: impl Into<String>) -> Self {
        Self::done(vec![
            MockStep::emit(emit::cancelled(message.into())),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::done(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::cancelled(message.into())),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn tool_failure_then_text(text: impl Into<String>, gate: &MockGateHandle) -> Self {
        let mut steps = emit::failed_tool_frames("mock-failed-tool")
            .into_iter()
            .map(MockStep::emit)
            .collect::<Vec<_>>();
        steps.push(MockStep::Gate(gate.gate()));
        let text = text.into();
        steps.extend([
            MockStep::emit(emit::stream_delta(text.clone())),
            MockStep::emit(emit::stream_end(emit::mock_assistant_message(
                Some(ChatMessageId(Uuid::new_v4().to_string())),
                text,
            ))),
            MockStep::emit(emit::typing(false)),
        ]);
        Self::done(steps)
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

    pub fn starting_with_goal(mut self, goal: protocol::NativeGoal) -> Self {
        match &mut self.body {
            MockTurnBody::Steps(steps) => steps.insert(
                0,
                MockStep::emit(crate::backend::BackendEvent::Chat(
                    protocol::ChatEvent::GoalChanged(Some(goal)),
                )),
            ),
            MockTurnBody::RenderedEcho(_)
            | MockTurnBody::RenderedTextForInput { .. }
            | MockTurnBody::RenderedHistoryJoin
            | MockTurnBody::RenderedWorkspaceRoots => {
                panic!("goal state requires a frozen turn")
            }
        }
        self
    }

    pub fn with_goal_state(self, goal: Option<protocol::NativeGoal>) -> Self {
        self.with_appended_steps(vec![MockStep::emit(crate::backend::BackendEvent::Chat(
            protocol::ChatEvent::GoalChanged(goal),
        ))])
    }

    pub fn with_duplicate_idle(self) -> Self {
        self.with_appended_steps(vec![MockStep::emit(emit::typing(false))])
    }

    pub fn with_gate_before_idle(mut self, gate: &MockGateHandle) -> Self {
        let MockTurnBody::Steps(steps) = &mut self.body else {
            panic!("idle gate requires a frozen turn");
        };
        let position = steps
            .iter()
            .rposition(|step| matches!(step, MockStep::Emit(event)
                if matches!(&**event, BackendEvent::Chat(protocol::ChatEvent::TypingStatusChanged(false)))))
            .expect("idle gate requires a backend idle marker");
        steps.insert(position, MockStep::Gate(gate.gate()));
        self
    }

    pub fn with_active_idle_cycle(self) -> Self {
        self.with_appended_steps(vec![
            MockStep::emit(emit::typing(true)),
            MockStep::emit(emit::typing(false)),
        ])
    }

    pub fn with_gated_native_child_and_timing_tool(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
        gate: &MockGateHandle,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            usage: MockNativeChildUsage::Split,
            registration_gate: Some(gate.gate()),
            emit_timing_tool: true,
            name: child_name.into(),
            prompt: child_prompt.into(),
            grandchild: None,
            lifecycle: MockChildLifecycle::CompleteAndRetain,
        })])
    }

    pub fn with_running_native_child_and_timing(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            registration_gate: None,
            usage: MockNativeChildUsage::Split,
            emit_timing_tool: true,
            name: child_name.into(),
            prompt: child_prompt.into(),
            grandchild: None,
            lifecycle: MockChildLifecycle::LiveUntilInterrupt,
        })])
    }

    pub fn with_native_child(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            registration_gate: None,
            usage: MockNativeChildUsage::Split,
            emit_timing_tool: false,
            name: child_name.into(),
            prompt: child_prompt.into(),
            grandchild: None,
            lifecycle: MockChildLifecycle::CompleteAndRetain,
        })])
    }

    pub fn with_native_child_usage(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
        usage: MockNativeChildUsage,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            registration_gate: None,
            usage,
            emit_timing_tool: false,
            name: child_name.into(),
            prompt: child_prompt.into(),
            grandchild: None,
            lifecycle: MockChildLifecycle::CompleteAndRetain,
        })])
    }

    pub fn with_native_child_and_grandchild(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
        grandchild_name: impl Into<String>,
        grandchild_prompt: impl Into<String>,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            registration_gate: None,
            usage: MockNativeChildUsage::Split,
            emit_timing_tool: false,
            name: child_name.into(),
            prompt: child_prompt.into(),
            lifecycle: MockChildLifecycle::CompleteAndRetain,
            grandchild: Some(MockNativeGrandchild {
                name: grandchild_name.into(),
                prompt: grandchild_prompt.into(),
            }),
        })])
    }

    pub fn with_dropped_native_child(
        self,
        child_name: impl Into<String>,
        child_prompt: impl Into<String>,
    ) -> Self {
        self.with_appended_steps(vec![MockStep::SpawnNativeChild(MockNativeChild {
            registration_gate: None,
            usage: MockNativeChildUsage::Split,
            emit_timing_tool: false,
            name: child_name.into(),
            prompt: child_prompt.into(),
            grandchild: None,
            lifecycle: MockChildLifecycle::CompleteAndDrop,
        })])
    }

    pub fn with_view_image(self, tool_call_id: &str, path: String) -> Self {
        self.with_appended_steps(vec![
            MockStep::emit(BackendEvent::Chat(protocol::ChatEvent::ToolRequest(
                protocol::ToolRequest {
                    tool_call_id: tool_call_id.to_owned(),
                    tool_name: "view_image".to_owned(),
                    tool_type: protocol::ToolRequestType::ViewImage { path },
                },
            ))),
            MockStep::emit(BackendEvent::Chat(
                protocol::ChatEvent::ToolExecutionCompleted(protocol::ToolExecutionCompletedData {
                    tool_call_id: tool_call_id.to_owned(),
                    outcome: protocol::ToolExecutionOutcome::Succeeded {
                        result: protocol::ToolExecutionResult::ViewImage {
                            image: None,
                            preview_error: None,
                        },
                    },
                }),
            )),
        ])
    }

    pub fn with_open_native_subagent(self, tool_call_id: &str, agent_id: AgentId) -> Self {
        self.with_appended_steps(vec![
            MockStep::emit(emit::tool_request(protocol::ToolRequest {
                tool_call_id: tool_call_id.to_owned(),
                tool_name: "Task".to_owned(),
                tool_type: protocol::ToolRequestType::AgentSpawn {
                    prompt: None,
                    name: Some("Native progress".to_owned()),
                    execution_mode: protocol::AgentExecutionMode::Background,
                },
            })),
            MockStep::emit(emit::tool_progress(protocol::ToolProgressData {
                tool_call_id: tool_call_id.to_owned(),
                execution_mode: protocol::ToolExecutionMode::Background,
                cancellable: false,
                update: protocol::ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
                    agent_id,
                    backend_kind: None,
                    agent_name: "Native progress".to_owned(),
                    last_tool_name: None,
                    tool_calls: 0,
                    completed: false,
                    status: protocol::SubAgentProgressStatus::Running,
                }),
            })),
        ])
    }

    /// Leave a background tool call in flight and end the turn without ever
    /// collecting it: the request and a background-mode progress snapshot go
    /// out, and no completion follows. This is the state a provider leaves
    /// behind when it loses track of a command it started — the card stays
    /// open and its in-flight tray row has nothing left that could close it.
    pub fn with_tool_round_trips(self, count: usize) -> Self {
        let mut steps = Vec::new();
        for index in 0..count {
            let id = format!("retention-tool-{index}");
            steps.push(MockStep::emit(emit::tool_request(protocol::ToolRequest {
                tool_call_id: id.clone(),
                tool_name: "run_command".to_owned(),
                tool_type: protocol::ToolRequestType::RunCommand {
                    command: "printf bounded-trace".to_owned(),
                    working_directory: "/tmp/test".to_owned(),
                },
            })));
            steps.push(MockStep::emit(emit::tool_completed(
                protocol::ToolExecutionCompletedData {
                    tool_call_id: id,
                    outcome: protocol::ToolExecutionOutcome::Succeeded {
                        result: protocol::ToolExecutionResult::Other {
                            result: serde_json::json!({"fixture": true}),
                        },
                    },
                },
            )));
        }
        self.with_appended_steps(steps)
    }

    pub fn with_open_background_task(self, tool_call_id: impl Into<String>) -> Self {
        let steps = emit::background_task_started_frames(&tool_call_id.into(), true)
            .into_iter()
            .map(MockStep::emit)
            .collect();
        self.with_appended_steps(steps)
    }

    /// Leave a background tool call in flight once the turn has gone idle, and
    /// drain it when `drain` is released. An agent parked like this reads as
    /// inactive — no turn, no output, nothing on the stream — while still
    /// owing the background task's result a place in this context.
    pub fn with_background_task_until(
        self,
        tool_call_id: impl Into<String>,
        drain: &MockGateHandle,
    ) -> Self {
        let tool_call_id = tool_call_id.into();
        let mut steps: Vec<MockStep> = emit::background_task_started_frames(&tool_call_id, false)
            .into_iter()
            .map(MockStep::emit)
            .collect();
        steps.push(MockStep::Gate(drain.gate()));
        steps.extend(
            emit::background_task_finished_frames(&tool_call_id)
                .into_iter()
                .map(MockStep::emit),
        );
        self.with_appended_steps(steps)
    }

    fn with_appended_steps(mut self, extra: Vec<MockStep>) -> Self {
        match &mut self.body {
            MockTurnBody::Steps(steps) => steps.extend(extra),
            MockTurnBody::RenderedEcho(_)
            | MockTurnBody::RenderedTextForInput { .. }
            | MockTurnBody::RenderedHistoryJoin
            | MockTurnBody::RenderedWorkspaceRoots => {
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
            MockTurnBody::RenderedEcho(gate) => {
                let text = format!(
                    "{}mock backend response to: {input}",
                    startup_mcp_response_prefix(session_id)
                );
                if matches!(self.finish, MockTurnFinish::Held { .. }) {
                    held_steps(text)
                } else {
                    text_steps(
                        text,
                        TextShape {
                            gate,
                            ..TextShape::default()
                        },
                    )
                }
            }
            MockTurnBody::RenderedTextForInput {
                text,
                required,
                gate,
            } => text_steps(
                if input.contains(&required) {
                    text
                } else {
                    "Mock required input was absent".into()
                },
                TextShape {
                    gate: Some(gate),
                    ..TextShape::default()
                },
            ),
            MockTurnBody::RenderedHistoryJoin => text_steps(
                format!(
                    "mock history: {}",
                    mock_prompt_history(session_id).join(" | ")
                ),
                TextShape::default(),
            ),
            MockTurnBody::RenderedWorkspaceRoots => {
                let roots = super::session_store()
                    .lock()
                    .expect("mock backend session store mutex poisoned")
                    .get(&session_id.0)
                    .expect("live mock session")
                    .workspace_roots
                    .clone();
                text_steps(
                    serde_json::to_string(&roots).expect("workspace roots JSON"),
                    TextShape::default(),
                )
            }
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
    /// Emit the text one character per delta instead of all at once, which is
    /// how Codex really streams (measured: 1.44 characters per delta).
    per_character: bool,
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
    ];
    if shape.per_character {
        steps.extend(
            text.chars()
                .map(|ch| MockStep::emit(emit::stream_delta(ch.to_string()))),
        );
    } else {
        steps.push(MockStep::emit(emit::stream_delta(text.clone())));
    }
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
    /// Parks before the turn starts, refusing every steer as no active turn.
    TurnStartGate {
        start: MockGate,
        steer_waiting: MockGate,
    },
    SpawnNativeChild(MockNativeChild),
    AgentControlAwait(MockAgentControlAwait),
    TychatSend(String),
    TydePermissions,
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

#[derive(Debug, Clone, Copy)]
pub enum MockNativeChildUsage {
    Split,
    TotalOnly(u64),
    SplitWithTotal(u64),
    MixedOptionalWithEqualTotal,
}

#[derive(Debug, Clone)]
pub(super) struct MockNativeChild {
    pub(super) registration_gate: Option<MockGate>,
    pub(super) emit_timing_tool: bool,
    pub(super) usage: MockNativeChildUsage,
    pub(super) name: String,
    pub(super) prompt: String,
    pub(super) lifecycle: MockChildLifecycle,
    pub(super) grandchild: Option<MockNativeGrandchild>,
}

#[derive(Debug, Clone)]
pub(super) struct MockNativeGrandchild {
    pub(super) name: String,
    pub(super) prompt: String,
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
