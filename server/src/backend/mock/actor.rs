//! The mock command loop and scripted-turn executor.

use std::collections::VecDeque;
use std::sync::Arc;

use protocol::{AgentInput, SendMessagePayload, SessionId};
use tokio::sync::{mpsc, watch};

use crate::sub_agent::{SubAgentEmitter, SubAgentHandle};

use super::control::{
    MockCleanliness, MockCommand, MockControlCommand, MockRequest, MockTerminalReport,
    MockTerminalReportSlot, MockViolation,
};
use super::emit::{self, MockAgentControlAwaitMcp, MockEventSender};
use super::gate::MockGate;
use super::record_prompt;
use super::script::{MockPendingTool, MockScript, MockStep, MockTurn, MockTurnFinish};

pub(super) struct MockLoopConfig {
    pub(super) initial_message: Option<String>,
    pub(super) user_bubbles_from_history: bool,
    pub(super) agent_control_await_mcp: Option<MockAgentControlAwaitMcp>,
    pub(super) launch_script: MockScript,
}

pub(super) fn start_mock_command_loop(
    session_id: SessionId,
    command_rx: mpsc::UnboundedReceiver<MockCommand>,
    events_tx: MockEventSender,
    subagent_emitter_rx: watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    control_rx: mpsc::UnboundedReceiver<MockControlCommand>,
    terminal_report: MockTerminalReportSlot,
    config: MockLoopConfig,
) {
    let MockLoopConfig {
        initial_message,
        user_bubbles_from_history,
        agent_control_await_mcp,
        launch_script,
    } = config;
    let script = VecDeque::from(launch_script.turns);
    let unbounded_echo = launch_script.unbounded_echo;
    let user_bubbles = launch_script.user_bubbles || user_bubbles_from_history;
    let actor = MockActor {
        session_id,
        command_rx,
        events_tx,
        subagent_emitter_rx,
        agent_control_await_mcp,
        active_subagents: Vec::new(),
        initial_message,
        script,
        unbounded_echo,
        user_bubbles,
        phase: TurnPhase::Idle,
        violations: Vec::new(),
        requests: Vec::new(),
        parked_at_gate: false,
        terminal_report,
    };
    let control = ControlPlane {
        rx: control_rx,
        open: true,
    };
    tokio::spawn(actor.run(control));
}

enum TurnPhase {
    Idle,
    Held { interruptible: bool },
    ToolPending(MockPendingTool),
}

/// The test-control mailbox, kept outside `MockActor` so a `select!` can poll
/// it alongside futures that borrow the actor (or a gate owned by the running
/// step).
struct ControlPlane {
    rx: mpsc::UnboundedReceiver<MockControlCommand>,
    open: bool,
}

impl ControlPlane {
    /// Resolves with the next control command; pends forever once every
    /// `MockControl` handle is gone, so a `select!` arm over this never
    /// busy-loops.
    async fn recv(&mut self) -> MockControlCommand {
        loop {
            if !self.open {
                std::future::pending::<()>().await;
            }
            match self.rx.recv().await {
                Some(command) => return command,
                None => self.open = false,
            }
        }
    }
}

struct MockActor {
    session_id: SessionId,
    command_rx: mpsc::UnboundedReceiver<MockCommand>,
    events_tx: MockEventSender,
    subagent_emitter_rx: watch::Receiver<Option<Arc<dyn SubAgentEmitter>>>,
    agent_control_await_mcp: Option<MockAgentControlAwaitMcp>,
    active_subagents: Vec<SubAgentHandle>,
    initial_message: Option<String>,
    script: VecDeque<MockTurn>,
    unbounded_echo: bool,
    user_bubbles: bool,
    phase: TurnPhase,
    violations: Vec<MockViolation>,
    requests: Vec<MockRequest>,
    parked_at_gate: bool,
    terminal_report: MockTerminalReportSlot,
}

impl Drop for MockActor {
    fn drop(&mut self) {
        if !self.violations.is_empty() {
            tracing::warn!(
                violations = ?self.violations,
                "mock backend closed with recorded invariant violations"
            );
        }
    }
}

impl MockActor {
    async fn run(mut self, mut control: ControlPlane) {
        self.run_loop(&mut control).await;
        self.publish_terminal_report();
    }

    fn publish_terminal_report(&self) {
        let report = MockTerminalReport {
            violations: self.violations.clone(),
            requests: self.requests.clone(),
            queued_turns: self.script.len(),
            parked_at_gate: self.parked_at_gate,
        };
        *self
            .terminal_report
            .lock()
            .expect("mock terminal report mutex poisoned") = Some(report);
    }

    async fn run_loop(&mut self, control: &mut ControlPlane) {
        if let Some(initial_message) = self.initial_message.take() {
            self.requests.push(MockRequest::Launch {
                message: initial_message.clone(),
            });
            let compact = initial_message.trim() == "/compact";
            if self.user_bubbles && !compact && !self.emit_user_bubble(&initial_message) {
                return;
            }
            let turn = if compact {
                let prompt_index = record_prompt(&self.session_id, &initial_message);
                Some(MockTurn::compact(
                    &self.session_id,
                    prompt_index,
                    &initial_message,
                ))
            } else {
                let turn = match self.script.pop_front() {
                    Some(turn) => Some(turn),
                    None if self.unbounded_echo => Some(MockTurn::echo()),
                    None => None,
                };
                // A backend that dies during launch must not persist a prompt
                // it never accepted. Later turns have already accepted input.
                let dies_at_launch = matches!(
                    turn.as_ref().map(|turn| &turn.finish),
                    Some(MockTurnFinish::CloseStream)
                );
                if !dies_at_launch {
                    record_prompt(&self.session_id, &initial_message);
                }
                turn
            };
            let alive = match turn {
                Some(turn) => self.run_turn(turn, &initial_message, control).await,
                None => self.record_violation(MockViolation::ScriptExhausted {
                    message: initial_message,
                }),
            };
            if !alive {
                return;
            }
        }

        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    let alive = match command {
                        MockCommand::Input(AgentInput::SendMessage(payload)) => {
                            self.handle_send_message(payload, control).await
                        }
                        MockCommand::Input(AgentInput::UpdateSessionSettings(_)) => true,
                        MockCommand::Input(AgentInput::EditQueuedMessage(_))
                        | MockCommand::Input(AgentInput::CancelQueuedMessage(_))
                        | MockCommand::Input(AgentInput::SendQueuedMessageNow(_)) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                        MockCommand::EmitBusySelfTurn => self.handle_busy_self_turn(control).await,
                        MockCommand::Interrupt => self.handle_interrupt(),
                    };
                    if !alive {
                        return;
                    }
                }
                command = control.recv() => self.handle_control(command),
            }
        }
    }

    fn handle_control(&mut self, command: MockControlCommand) {
        match command {
            MockControlCommand::Enqueue { turns, ack } => {
                self.script.extend(turns);
                let _ = ack.send(());
            }
            MockControlCommand::ReadRequests { reply } => {
                let _ = reply.send(self.requests.clone());
            }
            MockControlCommand::ReadViolations { reply } => {
                let _ = reply.send(self.violations.clone());
            }
            MockControlCommand::ReadCleanliness { reply } => {
                let _ = reply.send(MockCleanliness {
                    violations: self.violations.clone(),
                    queued_turns: self.script.len(),
                    parked_at_gate: self.parked_at_gate,
                });
            }
        }
    }

    async fn handle_send_message(
        &mut self,
        payload: SendMessagePayload,
        control: &mut ControlPlane,
    ) -> bool {
        match payload.tool_response.as_ref() {
            Some(tool_response) => self
                .requests
                .push(MockRequest::ToolResponse(tool_response.clone())),
            None => self.requests.push(MockRequest::Input(payload.clone())),
        }
        if matches!(self.phase, TurnPhase::Held { .. }) {
            return self.record_violation(MockViolation::InputWhileHeld);
        }
        if let Some(tool_response) = payload.tool_response {
            return self.handle_tool_response(tool_response, control).await;
        }
        if matches!(self.phase, TurnPhase::ToolPending(_)) {
            return self.record_violation(MockViolation::InputWhileToolPending);
        }
        let compact = payload.message.trim() == "/compact";
        if self.user_bubbles && !compact && !self.emit_user_bubble(&payload.message) {
            return false;
        }
        let prompt_index = record_prompt(&self.session_id, &payload.message);
        let turn = if compact {
            MockTurn::compact(&self.session_id, prompt_index, &payload.message)
        } else {
            match self.script.pop_front() {
                Some(turn) => turn,
                None if self.unbounded_echo => MockTurn::echo(),
                None => {
                    return self.record_violation(MockViolation::ScriptExhausted {
                        message: payload.message,
                    });
                }
            }
        };
        self.run_turn(turn, &payload.message, control).await
    }

    async fn handle_busy_self_turn(&mut self, control: &mut ControlPlane) -> bool {
        const WAKEUP_INPUT: &str = "self-initiated wakeup";
        let turn = match self.script.pop_front() {
            Some(turn) => turn,
            None if self.unbounded_echo => MockTurn::echo(),
            None => {
                return self.record_violation(MockViolation::ScriptExhausted {
                    message: WAKEUP_INPUT.to_owned(),
                });
            }
        };
        self.run_turn(turn, WAKEUP_INPUT, control).await
    }

    fn emit_user_bubble(&mut self, message: &str) -> bool {
        self.events_tx.send_event(emit::user_bubble(message))
    }

    async fn handle_tool_response(
        &mut self,
        tool_response: protocol::SendMessageToolResponse,
        control: &mut ControlPlane,
    ) -> bool {
        let protocol::SendMessageToolResponse::ExitPlanMode {
            tool_call_id,
            decision,
            feedback,
        } = tool_response
        else {
            return self.record_violation(MockViolation::UnsupportedToolResponse);
        };
        let pending = match std::mem::replace(&mut self.phase, TurnPhase::Idle) {
            TurnPhase::ToolPending(pending) => pending,
            phase => {
                self.phase = phase;
                return self.record_violation(MockViolation::UnmatchedToolResponse);
            }
        };
        if pending.tool_call_id != tool_call_id {
            let expected = pending.tool_call_id.clone();
            // The request is still outstanding: a stale response must not
            // consume it.
            self.phase = TurnPhase::ToolPending(pending);
            return self.record_violation(MockViolation::StaleToolCallId {
                expected,
                actual: tool_call_id,
            });
        }

        let (completion, message) =
            emit::exit_plan_mode_completion(&tool_call_id, decision, feedback);
        if let Some(gate) = &pending.gate_before_completion {
            self.park_at_gate(gate, control).await;
        }
        if !self.events_tx.send_event(completion) {
            return false;
        }
        if let Some(gate) = &pending.gate_after_completion {
            self.park_at_gate(gate, control).await;
        }
        let turn = match self.script.pop_front() {
            Some(turn) => turn,
            None if self.unbounded_echo => MockTurn::echo(),
            None => {
                return self.record_violation(MockViolation::ScriptExhausted { message });
            }
        };
        self.run_turn(turn, &message, control).await
    }

    fn handle_interrupt(&mut self) -> bool {
        self.requests.push(MockRequest::Interrupt);
        if !matches!(
            self.phase,
            TurnPhase::Held {
                interruptible: true
            }
        ) {
            return true;
        }
        emit::cancel_live_native_children(&self.active_subagents);
        let _ = self.events_tx.send_event(emit::cancelled(
            "mock backend interrupted held turn".to_owned(),
        ));
        let _ = self.events_tx.send_event(emit::typing(false));
        self.phase = TurnPhase::Idle;
        true
    }

    async fn run_turn(&mut self, turn: MockTurn, input: &str, control: &mut ControlPlane) -> bool {
        let (steps, finish) = turn.realize(&self.session_id, input);
        for step in steps {
            if !self.run_step(step, control).await {
                return false;
            }
        }
        match finish {
            MockTurnFinish::Done => true,
            MockTurnFinish::Held { interruptible } => {
                self.phase = TurnPhase::Held { interruptible };
                true
            }
            MockTurnFinish::CloseStream => false,
            MockTurnFinish::AwaitToolResponse(pending) => {
                self.phase = TurnPhase::ToolPending(pending);
                true
            }
        }
    }

    async fn run_step(&mut self, step: MockStep, control: &mut ControlPlane) -> bool {
        match step {
            MockStep::Emit(event) => self.events_tx.send_event(*event),
            MockStep::Gate(gate) => {
                self.park_at_gate(&gate, control).await;
                true
            }
            MockStep::SpawnNativeChild(child) => {
                emit::spawn_native_child(
                    &self.events_tx,
                    &mut self.subagent_emitter_rx,
                    &mut self.active_subagents,
                    child,
                )
                .await;
                true
            }
            MockStep::AgentControlAwait(step) => {
                emit::agent_control_await(
                    &self.events_tx,
                    self.agent_control_await_mcp.as_ref(),
                    step,
                )
                .await
            }
        }
    }

    /// Control requests remain serviceable while the turn is parked.
    async fn park_at_gate(&mut self, gate: &MockGate, control: &mut ControlPlane) {
        self.parked_at_gate = true;
        {
            let wait = gate.wait();
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    () = &mut wait => break,
                    command = control.recv() => self.handle_control(command),
                }
            }
        }
        self.parked_at_gate = false;
    }

    fn record_violation(&mut self, violation: MockViolation) -> bool {
        emit::emit_error_card(&self.events_tx, &violation.error_card_text());
        self.violations.push(violation);
        tracing::error!(
            violations = ?self.violations,
            "mock backend closing after script violation"
        );
        false
    }
}
