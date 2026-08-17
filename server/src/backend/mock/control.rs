//! Backend commands and the test-facing [`MockControl`] handle.

use std::sync::{Arc, Mutex as StdMutex};

use protocol::{AgentInput, SendMessagePayload, SendMessageToolResponse};
use tokio::sync::{mpsc, oneshot};

use super::script::MockTurn;

pub(super) enum MockCommand {
    Input(AgentInput),
    Interrupt,
    /// The backend handed a send back as `Busy` and asked the actor to run a
    /// turn "it started on its own".
    EmitBusySelfTurn,
}

/// A recorded invariant violation at the backend boundary.
#[derive(Debug, Clone)]
pub enum MockViolation {
    /// Input arrived while a held turn was open.
    InputWhileHeld,
    /// Ordinary input arrived while a tool request was still pending.
    InputWhileToolPending,
    /// A tool response the mock cannot model at all.
    UnsupportedToolResponse,
    /// A tool response with no pending tool request to match.
    UnmatchedToolResponse,
    /// A tool response aimed at a tool call that is no longer pending.
    StaleToolCallId { expected: String, actual: String },
    /// Input arrived with no scripted turn left.
    ScriptExhausted { message: String },
}

impl MockViolation {
    /// The transcript card text. The first five strings are asserted by
    /// existing tests and must stay byte-identical.
    pub(super) fn error_card_text(&self) -> String {
        match self {
            Self::InputWhileHeld => {
                "mock backend received input while holding until interrupt".to_owned()
            }
            Self::InputWhileToolPending => {
                "mock backend received normal input while ExitPlanMode is pending".to_owned()
            }
            Self::UnsupportedToolResponse => {
                "Mock backend received an unsupported tool response.".to_owned()
            }
            Self::UnmatchedToolResponse => {
                "No matching pending tool request is waiting for that response.".to_owned()
            }
            Self::StaleToolCallId { expected, actual } => format!(
                "ExitPlanMode response targeted stale tool_call_id {actual}; pending tool_call_id is {expected}."
            ),
            Self::ScriptExhausted { message } => format!(
                "mock backend script exhausted: no scripted turn installed for input: {message}"
            ),
        }
    }
}

/// One thing the server handed this backend instance, captured in arrival order.
#[derive(Debug, Clone)]
pub enum MockRequest {
    Launch { message: String },
    Input(SendMessagePayload),
    ToolResponse(SendMessageToolResponse),
    Interrupt,
}

pub(super) enum MockControlCommand {
    /// Append turns to the script queue and acknowledge once installed.
    Enqueue {
        turns: Vec<MockTurn>,
        ack: oneshot::Sender<()>,
    },
    ReadRequests {
        reply: oneshot::Sender<Vec<MockRequest>>,
    },
    ReadViolations {
        reply: oneshot::Sender<Vec<MockViolation>>,
    },
    ReadCleanliness {
        reply: oneshot::Sender<MockCleanliness>,
    },
}

/// What `assert_clean` inspects.
#[derive(Debug, Clone)]
pub(super) struct MockCleanliness {
    pub(super) violations: Vec<MockViolation>,
    pub(super) queued_turns: usize,
    pub(super) parked_at_gate: bool,
}

/// The actor's parting state, published before its control mailbox closes.
#[derive(Debug, Clone)]
pub(super) struct MockTerminalReport {
    pub(super) violations: Vec<MockViolation>,
    pub(super) requests: Vec<MockRequest>,
    pub(super) queued_turns: usize,
    pub(super) parked_at_gate: bool,
}

pub(super) type MockTerminalReportSlot = Arc<StdMutex<Option<MockTerminalReport>>>;

const CONTROL_CLOSED: &str =
    "mock backend control channel closed (the mock backend actor has exited)";
const TERMINAL_REPORT_MISSING: &str =
    "mock backend exited without publishing a terminal report (its actor never ran or was killed)";

/// Cloneable test handle to one mock backend actor. Reads stay answerable
/// after the actor exits (they fall back to the terminal report); `enqueue`
/// on an exited backend panics — a dead backend cannot accept a script.
#[derive(Debug, Clone)]
pub struct MockControl {
    tx: mpsc::UnboundedSender<MockControlCommand>,
    terminal: MockTerminalReportSlot,
}

impl MockControl {
    pub(super) fn channel() -> (
        Self,
        mpsc::UnboundedReceiver<MockControlCommand>,
        MockTerminalReportSlot,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let terminal: MockTerminalReportSlot = Arc::new(StdMutex::new(None));
        (
            Self {
                tx,
                terminal: Arc::clone(&terminal),
            },
            rx,
            terminal,
        )
    }

    fn terminal_report(&self) -> MockTerminalReport {
        self.terminal
            .lock()
            .expect("mock terminal report mutex poisoned")
            .clone()
            .expect(TERMINAL_REPORT_MISSING)
    }

    /// Install one more scripted turn and return once it is queued.
    pub async fn enqueue(&self, turn: MockTurn) {
        self.enqueue_all([turn]).await;
    }

    pub async fn enqueue_all(&self, turns: impl IntoIterator<Item = MockTurn>) {
        let (ack, installed) = oneshot::channel();
        self.tx
            .send(MockControlCommand::Enqueue {
                turns: turns.into_iter().collect(),
                ack,
            })
            .expect(CONTROL_CLOSED);
        installed.await.expect(CONTROL_CLOSED);
    }

    /// Everything the server has handed this backend instance, in order.
    /// Readable while the actor is live (including parked at a gate) and
    /// after it has exited.
    pub async fn requests(&self) -> Vec<MockRequest> {
        let (reply, read) = oneshot::channel();
        if self
            .tx
            .send(MockControlCommand::ReadRequests { reply })
            .is_err()
        {
            return self.terminal_report().requests;
        }
        match read.await {
            Ok(requests) => requests,
            // The actor exited between accepting and answering; the terminal
            // report was published before the mailbox closed.
            Err(_) => self.terminal_report().requests,
        }
    }

    /// Violations recorded by the actor, including its terminal report.
    pub async fn violations(&self) -> Vec<MockViolation> {
        let (reply, read) = oneshot::channel();
        if self
            .tx
            .send(MockControlCommand::ReadViolations { reply })
            .is_err()
        {
            return self.terminal_report().violations;
        }
        match read.await {
            Ok(violations) => violations,
            Err(_) => self.terminal_report().violations,
        }
    }

    async fn cleanliness(&self) -> MockCleanliness {
        let (reply, read) = oneshot::channel();
        if self
            .tx
            .send(MockControlCommand::ReadCleanliness { reply })
            .is_ok()
            && let Ok(report) = read.await
        {
            return report;
        }
        let terminal = self.terminal_report();
        MockCleanliness {
            violations: terminal.violations,
            queued_turns: terminal.queued_turns,
            parked_at_gate: terminal.parked_at_gate,
        }
    }

    /// Opt-in strictness: panics if the actor has recorded violations, still
    /// holds unconsumed scripted turns, or is parked on an unreleased gate.
    /// Works on a live actor and on the terminal report of an exited one.
    pub async fn assert_clean(&self) {
        let report = self.cleanliness().await;
        assert!(
            report.violations.is_empty() && report.queued_turns == 0 && !report.parked_at_gate,
            "mock backend is not clean: {report:?}"
        );
    }
}
