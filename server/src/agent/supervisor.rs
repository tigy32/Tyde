//! Agent supervisor: a hidden one-shot model call that reviews an idle
//! agent's last turn and decides whether the user's request is actually
//! finished, awaiting user input, or should be kicked back to work.
//!
//! Like the agent name generator, the supervisor is an implementation detail
//! of the host — it never becomes a protocol entity. Each verdict runs on a
//! throwaway unregistered agent id with an isolated tempdir workspace, no
//! tools, and inference-only backend hardening.

use std::time::{Duration, Instant};

use protocol::{
    ChatEvent, ChatMessage, Envelope, FrameKind, MessageSender, SUPERVISOR_MESSAGE_PREFIX,
    SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX, SendMessagePayload, SessionSettingsValues, Task,
    TaskList, TaskStatus,
};
use tokio::sync::mpsc;

use super::registry::AgentStatus;
use super::{
    AgentId, BackendAccessMode, BackendExecutionMode, BackendKind, BackendSpawnConfig, EventStream,
    HostCapacityTx, HostSubAgentEmitterContext, SpawnCostHint, ToolPolicy, spawn_backend,
};

/// Byte caps for ancillary supervision prompt sections, so one huge message
/// cannot blow up the (paid) supervision call. The final assistant message is
/// deliberately uncapped because its actual ending determines the verdict.
const SUPERVISION_SECTION_MAX_BYTES: usize = 4 * 1024;
const SUPERVISION_ERROR_MAX_BYTES: usize = 2 * 1024;

/// What the supervisor decided about an idle agent's turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SupervisionVerdict {
    /// The user's request is complete and no user response is needed.
    Done,
    /// The agent needs feedback, clarification, approval, a choice, or plan
    /// review before it can finish the request.
    AwaitingUser,
    /// The agent stopped early; send this follow-up message to keep it going.
    Continue { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisionFailureKind {
    BackendStart,
    BackendStream,
    BackendTerminal,
    Timeout,
    InvalidVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisionFailure {
    pub kind: SupervisionFailureKind,
    pub message: String,
}

impl SupervisionFailure {
    fn new(kind: SupervisionFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn is_retryable(&self) -> bool {
        !matches!(
            self.kind,
            SupervisionFailureKind::BackendStart | SupervisionFailureKind::BackendTerminal
        )
    }
}

/// Stateless projection of an agent's event log with everything the
/// supervisor scheduler needs. Computed inside the agent actor so it is
/// consistent with the live log; carries no scheduler state, so restarts of
/// the supervision worker can never desync it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SupervisionContextSnapshot {
    /// Content of the most recent real user message (supervisor kicks are
    /// excluded — they carry [`SUPERVISOR_MESSAGE_PREFIX`]).
    pub last_user_message: Option<String>,
    /// Count of real user messages in the whole log. A freshly compacted
    /// replacement agent has exactly one (its bootstrap summary prompt).
    pub user_message_count: u32,
    /// Consecutive supervisor kicks since the last real user message.
    pub kicks_since_user_message: u32,
    /// Body of the most recent supervisor kick (prefix stripped), and the
    /// agent's reply to it. Without these the judge cannot see that it has
    /// already tried this follow-up and been refused, so every repeat attempt
    /// looks like the first one and it re-answers `continue` forever.
    pub last_kick_message: Option<String>,
    pub last_reply_to_kick: Option<String>,
    pub last_assistant_message: Option<String>,
    /// Input-token footprint reported for the latest completed assistant
    /// turn. Absence remains explicit so eligibility never falls back to a
    /// cumulative or task-level usage value.
    pub current_context_input_tokens: Option<u64>,
    /// Most recent error surfaced since the last real user message.
    pub last_error_since_user_message: Option<String>,
    /// The user cancelled/interrupted work since their last message (and no
    /// message arrived after the cancel). Supervising past an intentional
    /// stop would fight the user, so the scheduler skips these turns.
    pub cancelled_since_user_message: bool,
    /// The turn now awaiting a verdict was cut short by the supervisor's stall
    /// timeout, so its final message is a truncation rather than a considered
    /// stopping point. Any later input — a real message or a supervisor kick —
    /// starts a turn of its own and clears this.
    pub last_turn_was_stall_interrupted: bool,
    /// A confirmed or possibly-mutating compaction suppresses automatic
    /// compaction until a new real user message arrives, independently of the
    /// post-compaction token count.
    pub auto_compaction_blocked_until_real_user: bool,
    /// A standalone requested compaction also suppresses a fresh supervisor
    /// verdict for the same user generation.
    pub supervision_verdict_dormant_until_real_user: bool,
    /// Canonical real-user count at the most recent compaction boundary.
    pub compaction_user_message_count: Option<u32>,
    /// A recorded stall-interrupt notice whose cancel event has not been seen
    /// yet. It disarms the very next cancel so the supervisor's own interrupt
    /// is not mistaken for the user pressing stop. Any new message closes the
    /// window, so this can never swallow a later user cancel.
    stall_interrupt_awaiting_cancel: bool,
}

pub(crate) fn supervision_context_snapshot(event_log: &[Envelope]) -> SupervisionContextSnapshot {
    let mut snapshot = SupervisionContextSnapshot::default();
    let mut latest_assistant_message_id = None;
    for envelope in event_log {
        if envelope.kind != FrameKind::ChatEvent {
            continue;
        }
        let Ok(event) = serde_json::from_value::<ChatEvent>(envelope.payload.clone()) else {
            continue;
        };
        match event {
            ChatEvent::MessageAdded(message) => {
                observe_message(&mut snapshot, &mut latest_assistant_message_id, &message)
            }
            ChatEvent::StreamEnd(data) => observe_message(
                &mut snapshot,
                &mut latest_assistant_message_id,
                &data.message,
            ),
            ChatEvent::MessageMetadataUpdated(update) => {
                if latest_assistant_message_id.as_ref() == Some(&update.message_id)
                    && let Some(context_breakdown) = update.context_breakdown
                {
                    snapshot.current_context_input_tokens = Some(context_breakdown.input_tokens);
                }
            }
            ChatEvent::OperationCancelled(_) => {
                if snapshot.stall_interrupt_awaiting_cancel {
                    snapshot.stall_interrupt_awaiting_cancel = false;
                } else {
                    snapshot.cancelled_since_user_message = true;
                }
            }
            ChatEvent::ContextCompaction(marker) => {
                let context_may_have_changed = matches!(
                    marker.mutation,
                    protocol::CompactionMutation::Completed
                        | protocol::CompactionMutation::MayHaveMutated
                );
                if context_may_have_changed {
                    snapshot.auto_compaction_blocked_until_real_user = true;
                    snapshot.current_context_input_tokens = marker.metrics.after_tokens;
                }
                if marker.operation_id.is_some() {
                    snapshot.supervision_verdict_dormant_until_real_user = true;
                }
                snapshot.compaction_user_message_count = Some(snapshot.user_message_count);
            }
            _ => {}
        }
    }
    snapshot
}

fn observe_message(
    snapshot: &mut SupervisionContextSnapshot,
    latest_assistant_message_id: &mut Option<protocol::ChatMessageId>,
    message: &ChatMessage,
) {
    match &message.sender {
        MessageSender::User => {
            if let Some(kick) = message.content.strip_prefix(SUPERVISOR_MESSAGE_PREFIX) {
                snapshot.kicks_since_user_message =
                    snapshot.kicks_since_user_message.saturating_add(1);
                snapshot.last_kick_message = Some(kick.to_owned());
                // The reply belonging to the previous kick is not this kick's
                // reply; the next assistant message is.
                snapshot.last_reply_to_kick = None;
            } else {
                snapshot.last_user_message = Some(message.content.clone());
                snapshot.user_message_count = snapshot.user_message_count.saturating_add(1);
                snapshot.kicks_since_user_message = 0;
                snapshot.last_error_since_user_message = None;
                snapshot.last_kick_message = None;
                snapshot.last_reply_to_kick = None;
                snapshot.auto_compaction_blocked_until_real_user = false;
                snapshot.supervision_verdict_dormant_until_real_user = false;
            }
            // Any new message (real or kick) supersedes an earlier cancel:
            // work is running again on purpose.
            snapshot.cancelled_since_user_message = false;
            snapshot.stall_interrupt_awaiting_cancel = false;
            snapshot.last_turn_was_stall_interrupted = false;
        }
        MessageSender::Assistant { .. } => {
            *latest_assistant_message_id = message.message_id.clone();
            snapshot.current_context_input_tokens = message
                .context_breakdown
                .as_ref()
                .map(|breakdown| breakdown.input_tokens);
            if !message.content.trim().is_empty() {
                snapshot.last_assistant_message = Some(message.content.clone());
                if snapshot.last_kick_message.is_some() {
                    snapshot.last_reply_to_kick = Some(message.content.clone());
                }
            }
        }
        MessageSender::Error => {
            snapshot.last_error_since_user_message = Some(message.content.clone());
        }
        MessageSender::Warning
            if message
                .content
                .starts_with(SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX) =>
        {
            snapshot.last_turn_was_stall_interrupted = true;
            snapshot.stall_interrupt_awaiting_cancel = true;
        }
        MessageSender::System | MessageSender::Warning => {}
    }
}

/// One supervision verdict reads more context than naming, so it gets a
/// longer budget per attempt.
pub(crate) const SUPERVISION_GENERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Grace period between going idle and judging, so queued-message drains and
/// immediate user follow-ups win the race instead of being second-guessed.
const SUPERVISION_DEBOUNCE: Duration = Duration::from_secs(3);
const SUPERVISION_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(480),
];
const _: () = assert!(
    SUPERVISION_RETRY_DELAYS.len() == settings_model::SUPERVISOR_RETRY_ATTEMPTS_MAX as usize
);

/// Settings a verdict was launched under. Editing any of them mid-flight
/// invalidates the answer, because the user changed the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VerdictSettingsFingerprint {
    max_kicks_per_task: u8,
    retry_attempts: u8,
    cost_tier: settings_model::SupervisorCostTier,
}

impl From<settings_model::SupervisorSettings> for VerdictSettingsFingerprint {
    fn from(settings: settings_model::SupervisorSettings) -> Self {
        Self {
            max_kicks_per_task: settings.max_kicks_per_task,
            retry_attempts: settings.retry_attempts,
            cost_tier: settings.cost_tier,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SupervisionAction {
    Verdict,
    AutoCompaction,
}

/// Whether the session's compaction history still permits supervising this
/// turn. A replacement agent's bootstrap summary is not work the user asked
/// for, so judging or re-compacting it would loop.
pub(crate) fn supervision_record_allows_action(
    record: Option<&crate::store::session::SessionRecord>,
    context: &SupervisionContextSnapshot,
    action: SupervisionAction,
) -> bool {
    let Some(record) = record else {
        return false;
    };
    if record.compacted_to_session_id.is_some() {
        return false;
    }
    if !record.compaction_operations.is_empty() || context.compaction_user_message_count.is_some() {
        return match action {
            SupervisionAction::Verdict => !context.supervision_verdict_dormant_until_real_user,
            SupervisionAction::AutoCompaction => !context.auto_compaction_blocked_until_real_user,
        };
    }
    !(record.compacted_from_session_id.is_some() && context.user_message_count <= 1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SupervisionRetryReason {
    Failure(SupervisionFailureKind),
    SettingsChanged,
}

#[derive(Debug)]
enum Phase {
    /// A turn is running. Stall timing lives in the actor, which already tracks
    /// the turn start and the last backend event exactly; duplicating it here
    /// would only make it staler.
    Active,
    Debouncing {
        idle_since: Instant,
    },
    /// A restored transcript is replayed work rather than work this agent just
    /// did, so judging it is opt-in.
    RestoreDeferred {
        idle_since: Instant,
    },
    VerdictInFlight {
        idle_since: Instant,
        attempts_started: u8,
        verdict_settings: VerdictSettingsFingerprint,
    },
    RetryPending {
        idle_since: Instant,
        attempts_started: u8,
        due_at: Instant,
    },
    FailureExhausted {
        idle_since: Instant,
        attempts_started: u8,
        retry_due_at: Option<Instant>,
        compaction_epoch: Option<u64>,
    },
    /// Judged and settled, or deliberately skipped. Auto-compaction keys on
    /// this phase, so a turn that ends by asking the user a question relieves
    /// context pressure exactly like one that ends by finishing the work.
    Settled {
        idle_since: Instant,
        compaction_epoch: Option<u64>,
    },
    CompactionPending {
        idle_since: Instant,
    },
    Compacting,
    /// A compaction just landed. Re-judging the replacement's bootstrap turn
    /// would compact it again forever, so this clears only on a real user
    /// message — tracked by count, since the bootstrap prompt is itself one.
    PostCompactionDormant {
        idle_since: Instant,
        user_message_count: u32,
    },
}

/// What the actor should do now that a supervisor deadline has come due.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SupervisorAction {
    None,
    LaunchVerdict { attempts_started: u8 },
    RequestCompaction,
}

/// Supervisor scheduling state, owned by the agent actor.
///
/// This used to be one host-side scheduler over every agent, which meant every
/// decision had to be re-verified against a status watch that could not observe
/// stream deltas. Reading the same state from inside the actor makes the stall
/// clock exact and reduces verdict staleness checks to one activity comparison,
/// in place of re-fetching the conversation, the session record and the
/// settings between deciding and acting.
#[derive(Debug)]
pub(crate) struct SupervisorState {
    phase: Phase,
    last_activity: u64,
}

impl SupervisorState {
    pub(crate) fn new(
        status: &AgentStatus,
        settings: settings_model::SupervisorSettings,
        now: Instant,
    ) -> Self {
        Self {
            phase: Self::fresh_phase(status, settings, now),
            last_activity: status.activity_counter,
        }
    }

    fn fresh_phase(
        status: &AgentStatus,
        settings: settings_model::SupervisorSettings,
        now: Instant,
    ) -> Phase {
        if status.is_active() {
            Phase::Active
        } else if status.is_user_response_pending() {
            Phase::Settled {
                idle_since: now,
                compaction_epoch: None,
            }
        } else if status.restored_without_live_turn && !settings.supervise_restored_agents {
            Phase::RestoreDeferred { idle_since: now }
        } else {
            Phase::Debouncing { idle_since: now }
        }
    }

    fn idle_since(&self) -> Option<Instant> {
        match &self.phase {
            Phase::Debouncing { idle_since }
            | Phase::RestoreDeferred { idle_since }
            | Phase::VerdictInFlight { idle_since, .. }
            | Phase::RetryPending { idle_since, .. }
            | Phase::FailureExhausted { idle_since, .. }
            | Phase::Settled { idle_since, .. }
            | Phase::CompactionPending { idle_since }
            | Phase::PostCompactionDormant { idle_since, .. } => Some(*idle_since),
            Phase::Active | Phase::Compacting => None,
        }
    }

    /// Auto-compaction reacts to context pressure, not to a verdict, so it
    /// fires from every phase where the agent has settled and no judgement is
    /// pending. It stays out of `RestoreDeferred` (supervision there is opt-in)
    /// and `PostCompactionDormant` (that is the anti-loop guard).
    fn compaction_epoch(&self) -> Option<Option<u64>> {
        match &self.phase {
            Phase::Settled {
                compaction_epoch, ..
            }
            | Phase::FailureExhausted {
                compaction_epoch, ..
            } => Some(*compaction_epoch),
            _ => None,
        }
    }

    /// Folds a status the actor just published into the phase machine. A
    /// compaction in progress owns the phase until its own handshake completes.
    pub(crate) fn observe(
        &mut self,
        status: &AgentStatus,
        settings: settings_model::SupervisorSettings,
        event_log: &[Envelope],
        compaction_in_progress: bool,
        now: Instant,
    ) {
        let activity_changed = self.last_activity != status.activity_counter;
        self.last_activity = status.activity_counter;
        match &self.phase {
            Phase::CompactionPending { .. } if compaction_in_progress => {
                self.phase = Phase::Compacting;
                return;
            }
            // The request was refused, or activity beat it there.
            Phase::CompactionPending { .. } if !activity_changed => return,
            Phase::Compacting if compaction_in_progress => return,
            Phase::Compacting => {
                self.phase = Phase::PostCompactionDormant {
                    idle_since: now,
                    user_message_count: supervision_context_snapshot(event_log).user_message_count,
                };
                return;
            }
            _ => {}
        }
        // Projecting the log is linear in its length, so it stays in the one
        // branch that needs it: deciding whether the message that woke a
        // just-compacted agent was a real user message or its own bootstrap.
        if let Phase::PostCompactionDormant {
            user_message_count: at_compaction,
            ..
        } = &self.phase
            && (!activity_changed
                || supervision_context_snapshot(event_log).user_message_count <= *at_compaction)
        {
            return;
        }
        if activity_changed {
            self.phase = Self::fresh_phase(status, settings, now);
            return;
        }
        if status.is_active() {
            self.phase = Phase::Active;
        } else if status.is_user_response_pending() {
            self.phase = Phase::Settled {
                idle_since: self.idle_since().unwrap_or(now),
                compaction_epoch: None,
            };
        } else if matches!(&self.phase, Phase::Active) {
            self.phase = if status.restored_without_live_turn && !settings.supervise_restored_agents
            {
                Phase::RestoreDeferred { idle_since: now }
            } else {
                Phase::Debouncing { idle_since: now }
            };
        }
    }

    /// Earliest instant this agent needs the supervisor to look at it again.
    pub(crate) fn next_deadline(
        &self,
        settings: settings_model::SupervisorSettings,
        epoch: u64,
    ) -> Option<Instant> {
        if !settings.enabled {
            return None;
        }
        if let Some(compaction_epoch) = self.compaction_epoch() {
            if settings.auto_compact_on_success && compaction_epoch != Some(epoch) {
                return self
                    .idle_since()?
                    .checked_add(Duration::from_secs(u64::from(
                        settings.auto_compact_inactivity_delay_seconds,
                    )));
            }
            return None;
        }
        match &self.phase {
            Phase::Debouncing { idle_since } => idle_since.checked_add(SUPERVISION_DEBOUNCE),
            Phase::RetryPending { due_at, .. } => Some(*due_at),
            _ => None,
        }
    }

    pub(crate) fn due_action(
        &self,
        settings: settings_model::SupervisorSettings,
        epoch: u64,
        now: Instant,
    ) -> SupervisorAction {
        if self
            .next_deadline(settings, epoch)
            .is_none_or(|due| due > now)
        {
            return SupervisorAction::None;
        }
        match &self.phase {
            Phase::Debouncing { .. } => SupervisorAction::LaunchVerdict {
                attempts_started: 0,
            },
            Phase::RetryPending {
                attempts_started, ..
            } => SupervisorAction::LaunchVerdict {
                attempts_started: *attempts_started,
            },
            Phase::Settled { .. } | Phase::FailureExhausted { .. } => {
                SupervisorAction::RequestCompaction
            }
            _ => SupervisorAction::None,
        }
    }

    pub(crate) fn begin_verdict(
        &mut self,
        settings: settings_model::SupervisorSettings,
        attempts_started: u8,
    ) {
        let Some(idle_since) = self.idle_since() else {
            return;
        };
        self.phase = Phase::VerdictInFlight {
            idle_since,
            attempts_started: attempts_started.saturating_add(1),
            verdict_settings: VerdictSettingsFingerprint::from(settings),
        };
    }

    /// Settings the in-flight verdict was launched under, or `None` if the
    /// conversation moved on while it was out — which drops the result. The
    /// host needed the conversation, the session record and the settings
    /// re-read here; inside the actor an unchanged activity counter means
    /// nothing happened.
    pub(crate) fn in_flight_verdict(
        &self,
        activity_counter: u64,
    ) -> Option<VerdictSettingsFingerprint> {
        let Phase::VerdictInFlight {
            verdict_settings, ..
        } = &self.phase
        else {
            return None;
        };
        (self.last_activity == activity_counter).then_some(*verdict_settings)
    }

    pub(crate) fn settle(&mut self, now: Instant) {
        self.phase = Phase::Settled {
            idle_since: self.idle_since().unwrap_or(now),
            compaction_epoch: None,
        };
    }

    /// Records a failed or invalidated verdict and schedules the next attempt.
    /// Returns the attempt count once they are exhausted, so the caller can
    /// tell the user supervision gave up; `None` while retries remain.
    pub(crate) fn note_verdict_failure(
        &mut self,
        reason: SupervisionRetryReason,
        settings: settings_model::SupervisorSettings,
        now: Instant,
    ) -> Option<u8> {
        let Phase::VerdictInFlight {
            idle_since,
            attempts_started,
            ..
        } = &self.phase
        else {
            return None;
        };
        let (idle_since, attempts_started) = (*idle_since, *attempts_started);
        let maximum_attempts = settings.retry_attempts.saturating_add(1);
        let delay_index = usize::from(attempts_started.saturating_sub(1));
        if attempts_started >= maximum_attempts {
            self.phase = match reason {
                // The backoff the next attempt *would* have used is kept, so
                // raising the retry limit later resumes on the original
                // schedule instead of firing a burst immediately.
                SupervisionRetryReason::Failure(_) => Phase::FailureExhausted {
                    idle_since,
                    attempts_started,
                    retry_due_at: SUPERVISION_RETRY_DELAYS
                        .get(delay_index)
                        .and_then(|delay| now.checked_add(*delay)),
                    compaction_epoch: None,
                },
                SupervisionRetryReason::SettingsChanged => Phase::Settled {
                    idle_since,
                    compaction_epoch: None,
                },
            };
            return Some(attempts_started);
        }
        let delay = SUPERVISION_RETRY_DELAYS[delay_index];
        self.phase = Phase::RetryPending {
            idle_since,
            attempts_started,
            due_at: now.checked_add(delay).unwrap_or(now),
        };
        None
    }

    pub(crate) fn begin_compaction(&mut self, now: Instant) {
        self.phase = Phase::CompactionPending {
            idle_since: self.idle_since().unwrap_or(now),
        };
    }

    /// Marks the context-pressure gate as answered for this settings epoch, so
    /// a threshold that is not met is evaluated once rather than every tick.
    pub(crate) fn mark_compaction_evaluated(&mut self, epoch: u64) {
        match &mut self.phase {
            Phase::Settled {
                compaction_epoch, ..
            }
            | Phase::FailureExhausted {
                compaction_epoch, ..
            } => *compaction_epoch = Some(epoch),
            _ => {}
        }
    }

    /// Turning `supervise_restored_agents` on judges the restored agents that
    /// were waiting on it from this edit rather than from their original
    /// restore instant: the verdict is being authorized now, and the debounce
    /// exists to let an immediate user follow-up win the race.
    pub(crate) fn apply_settings_change(
        &mut self,
        previous: settings_model::SupervisorSettings,
        current: settings_model::SupervisorSettings,
        now: Instant,
    ) {
        if !previous.supervise_restored_agents
            && current.supervise_restored_agents
            && matches!(&self.phase, Phase::RestoreDeferred { .. })
        {
            self.phase = Phase::Debouncing { idle_since: now };
        }
        // Raising the retry limit resumes an exhausted agent on the backoff it
        // had already earned, rather than firing the next attempt immediately.
        if let Phase::FailureExhausted {
            idle_since,
            attempts_started,
            retry_due_at: Some(due_at),
            ..
        } = &self.phase
            && *attempts_started < current.retry_attempts.saturating_add(1)
        {
            self.phase = Phase::RetryPending {
                idle_since: *idle_since,
                attempts_started: *attempts_started,
                due_at: *due_at,
            };
        }
    }
}

/// Everything one supervision call needs. `verdict_agent_id` must be a fresh
/// unregistered id — the run never appears in the agent registry.
pub(crate) struct GenerateSupervisionVerdictRequest {
    pub verdict_agent_id: AgentId,
    pub backend_kind: BackendKind,
    pub last_user_message: String,
    pub task_list: Option<TaskList>,
    pub last_assistant_message: Option<String>,
    pub last_error: Option<String>,
    /// The turn under review was cut short by the supervisor's stall timeout,
    /// so its final message is a truncation rather than a decision to stop.
    pub stall_interrupted: bool,
    pub kicks_so_far: u32,
    /// The previous kick and the agent's answer to it, so a judge that already
    /// tried this follow-up can see it was refused instead of reissuing it.
    pub last_kick_message: Option<String>,
    pub last_reply_to_kick: Option<String>,
    /// Model tier for the verdict call; `None` runs the backend's default.
    pub cost_hint: Option<SpawnCostHint>,
    pub session_settings: Option<SessionSettingsValues>,
    pub use_mock_backend: bool,
    pub capacity_tx: HostCapacityTx,
}

pub(crate) async fn generate_supervision_verdict(
    request: GenerateSupervisionVerdictRequest,
) -> Result<SupervisionVerdict, SupervisionFailure> {
    if request.use_mock_backend {
        return generate_mock_supervision_verdict(&request);
    }

    let prompt = build_supervision_prompt(&request);
    let spawn_config =
        supervision_spawn_config(request.cost_hint, request.session_settings.clone());
    let isolated_workspace = tempfile::tempdir().map_err(|err| {
        SupervisionFailure::new(
            SupervisionFailureKind::BackendStart,
            format!("failed to create isolated supervision workspace: {err}"),
        )
    })?;
    let workspace_roots = vec![isolated_workspace.path().to_string_lossy().into_owned()];
    let initial_input = SendMessagePayload {
        message: prompt,
        images: None,
        origin: None,
        tool_response: None,
    };
    let (host_sub_agent_spawn_tx, _host_sub_agent_spawn_rx) = mpsc::unbounded_channel();
    let (_backend, mut events, _session_id) = match spawn_backend(
        &request.verdict_agent_id,
        request.backend_kind,
        workspace_roots,
        spawn_config,
        initial_input,
        HostSubAgentEmitterContext {
            host_sub_agent_spawn_tx,
            capacity_tx: request.capacity_tx.clone(),
        },
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            return Err(SupervisionFailure::new(
                SupervisionFailureKind::BackendStart,
                format!(
                    "agent supervisor failed to start for backend {:?}: {}",
                    request.backend_kind, err
                ),
            ));
        }
    };

    let result = collect_supervision_events(&mut events, request.backend_kind).await;
    if let Err(err) = &result {
        tracing::warn!(
            backend_kind = ?request.backend_kind,
            failure_kind = ?err.kind,
            error = %err.message,
            "agent supervision call failed"
        );
    }
    result
}

fn supervision_spawn_config(
    cost_hint: Option<SpawnCostHint>,
    session_settings: Option<SessionSettingsValues>,
) -> BackendSpawnConfig {
    BackendSpawnConfig {
        acp_agent: None,
        execution_mode: BackendExecutionMode::InferenceOnly,
        cost_hint,
        custom_agent_id: None,
        startup_mcp_servers: Vec::new(),
        session_settings,
        provider_version: None,
        antigravity_conversations_dir: None,
        backend_config: Default::default(),
        resolved_spawn_config: super::customization::ResolvedSpawnConfig {
            tool_policy: ToolPolicy::AllowList { tools: Vec::new() },
            access_mode: BackendAccessMode::ReadOnly,
            ..Default::default()
        },
    }
}

async fn collect_supervision_events(
    events: &mut EventStream,
    backend_kind: BackendKind,
) -> Result<SupervisionVerdict, SupervisionFailure> {
    let mut streamed_text = String::new();
    while let Some(event) = events.recv().await {
        match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                return Err(SupervisionFailure::new(
                    supervision_backend_error_kind(backend_kind),
                    message.content,
                ));
            }
            ChatEvent::StreamDelta(delta) => {
                streamed_text.push_str(&delta.text);
            }
            ChatEvent::StreamEnd(data) => {
                let final_content = data.message.content;
                let candidate = if final_content.trim().is_empty() {
                    std::mem::take(&mut streamed_text)
                } else {
                    final_content
                };
                if candidate.trim().is_empty() {
                    continue;
                }
                return parse_supervision_verdict(&candidate).map_err(|message| {
                    SupervisionFailure::new(SupervisionFailureKind::InvalidVerdict, message)
                });
            }
            ChatEvent::TypingStatusChanged(false) => {
                return Err(SupervisionFailure::new(
                    SupervisionFailureKind::BackendStream,
                    "agent supervisor turn completed before producing a verdict",
                ));
            }
            _ => {}
        }
    }

    Err(SupervisionFailure::new(
        SupervisionFailureKind::BackendStream,
        "agent supervisor ended before producing a verdict",
    ))
}

fn supervision_backend_error_kind(backend_kind: BackendKind) -> SupervisionFailureKind {
    if backend_kind == BackendKind::Hermes {
        // Hermes exposes terminal gateway errors without a machine-readable
        // retry disposition. Fail closed so permanent auth/entitlement faults
        // cannot multiply paid supervisor calls; transient faults also stop
        // until user activity until Hermes adds structured error taxonomy.
        SupervisionFailureKind::BackendTerminal
    } else {
        SupervisionFailureKind::BackendStream
    }
}

pub(crate) const MOCK_SUPERVISOR_ERROR: &str = "__mock_supervisor_error__";
pub(crate) const MOCK_SUPERVISOR_INVALID: &str = "__mock_supervisor_invalid__";
pub(crate) const MOCK_SUPERVISOR_AWAITING_USER: &str = "__mock_supervisor_awaiting_user__";
pub(crate) const MOCK_SUPERVISOR_DONE: &str = "__mock_supervisor_done__";
pub(crate) const MOCK_SUPERVISOR_CONTINUE: &str = "__mock_supervisor_continue__";

fn generate_mock_supervision_verdict(
    request: &GenerateSupervisionVerdictRequest,
) -> Result<SupervisionVerdict, SupervisionFailure> {
    if request.last_user_message.contains(MOCK_SUPERVISOR_ERROR) {
        return Err(SupervisionFailure::new(
            SupervisionFailureKind::BackendStream,
            "mock supervision failure",
        ));
    }
    if request.last_user_message.contains(MOCK_SUPERVISOR_INVALID) {
        return parse_supervision_verdict("this is not a verdict").map_err(|message| {
            SupervisionFailure::new(SupervisionFailureKind::InvalidVerdict, message)
        });
    }
    if request
        .last_user_message
        .contains(MOCK_SUPERVISOR_AWAITING_USER)
    {
        return Ok(SupervisionVerdict::AwaitingUser);
    }
    if request.last_user_message.contains(MOCK_SUPERVISOR_DONE) {
        return Ok(SupervisionVerdict::Done);
    }
    if request.last_user_message.contains(MOCK_SUPERVISOR_CONTINUE)
        || request.last_error.is_some()
        || request.stall_interrupted
    {
        return Ok(SupervisionVerdict::Continue {
            message: "Please continue working on the task until it is complete.".to_owned(),
        });
    }
    Ok(SupervisionVerdict::Done)
}

fn build_supervision_prompt(request: &GenerateSupervisionVerdictRequest) -> String {
    let task_list = request
        .task_list
        .as_ref()
        .map(render_task_list)
        .filter(|rendered| !rendered.is_empty())
        .unwrap_or_else(|| "None recorded".to_owned());
    let last_agent_message = request
        .last_assistant_message
        .as_deref()
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|| "None".to_owned());
    let last_error = request
        .last_error
        .as_deref()
        .map(|text| cap_text(text, SUPERVISION_ERROR_MAX_BYTES))
        .unwrap_or_else(|| "None".to_owned());
    let user_message = cap_text(&request.last_user_message, SUPERVISION_SECTION_MAX_BYTES);
    let stall_interrupt_section = if request.stall_interrupted {
        "\nThis turn did not end on its own: it stopped making observable progress, so it was \
cancelled automatically. That is one of the grounds for continue listed above, so treat the \
agent's final message as truncated rather than as a decision to stop. Answer continue unless the \
user must decide something first, and name a smaller concrete next step or a different approach \
rather than repeating the action that stalled.\n"
    } else {
        ""
    };
    let repeat_section = build_repeat_follow_up_section(request);
    format!(
        "You supervise a coding agent that just went idle. Your only job is to decide whether the \
agent's turn ended where the agent intended it to end.\n\
Reply with EXACTLY one of these three forms and nothing else:\n\
VERDICT: done\n\
or\n\
VERDICT: awaiting_user\n\
or\n\
VERDICT: continue\n\
<one short follow-up naming the failure and where to resume>\n\
Rules:\n\
- Default to not interfering: unless you have positive evidence the turn ended unintentionally, \
never answer continue.\n\
- Answer continue ONLY when something outside the agent's control cut the turn off: a provider or \
tool error, a network failure, an HTTP 5xx, or a rate limit; an empty or near-empty final message \
that does not read as a reply; a final message that breaks off mid-sentence, mid-code-block, or \
mid-list; or a turn cancelled automatically for lack of progress. Those are the only grounds for \
continue.\n\
- That work remains, that the task list still has pending or in-progress items, that the agent \
stopped mid-task, or that the agent could have done more are NOT grounds for continue. An agent \
is allowed to stop with work remaining.\n\
- Answer awaiting_user when the final message ends the turn on purpose and expects something from \
the user: a question, a choice, a request for approval or permission, a plan or proposal for \
review, a refusal, or a report handing control back. Treat the agent's stated reason for stopping \
as authoritative. If it says it is waiting on the user, it is, even if the task list is unfinished \
and even if you disagree with its reasoning.\n\
- Answer done when the final message ends the turn on purpose and reads as complete, expecting no \
user response.\n\
- The follow-up message is sent verbatim to the agent and arrives as if the user had sent it. \
Never claim or imply that the user said, approved, permitted, or decided anything: you do not \
speak for the user and you cannot grant approval on their behalf.\n\
- Never argue an agent out of a refusal or past a permission check. An agent that declined to act \
without user approval is awaiting_user, always.\n\
- Never invent new work or expand scope beyond the user's request.\n\
- Name the concrete failure and the resume point, in one or two sentences.\n\
{stall_interrupt_section}\n\
User request:\n{user_message}\n\n\
Agent task list:\n{task_list}\n\n\
Agent's final message:\n{last_agent_message}\n\n\
Most recent error since the user's request:\n{last_error}\n\
{repeat_section}"
    )
}

/// Shows a repeating judge its own last attempt and how the agent answered it.
/// Deliberately not phrased as a remaining allowance: a "N of M used" budget
/// reads as something to spend, which is the opposite of the intended nudge.
fn build_repeat_follow_up_section(request: &GenerateSupervisionVerdictRequest) -> String {
    if request.kicks_so_far == 0 {
        return String::new();
    }
    let last_kick = request
        .last_kick_message
        .as_deref()
        .map(|text| cap_text(text, SUPERVISION_SECTION_MAX_BYTES))
        .unwrap_or_else(|| "Not recorded".to_owned());
    let reply = request
        .last_reply_to_kick
        .as_deref()
        .map(|text| cap_text(text, SUPERVISION_SECTION_MAX_BYTES))
        .unwrap_or_else(|| "None".to_owned());
    format!(
        "\nYou have already sent {kicks} automated follow-up(s) for this request, without any new \
instruction from the user in between. Your most recent one and the agent's answer to it follow. \
If your earlier follow-ups did not change the agent's behavior, another one will not either: \
answer awaiting_user.\n\n\
Your most recent follow-up:\n{last_kick}\n\n\
The agent's answer to it:\n{reply}\n",
        kicks = request.kicks_so_far,
    )
}

fn render_task_list(task_list: &TaskList) -> String {
    let mut rendered = String::new();
    if !task_list.title.trim().is_empty() {
        rendered.push_str(task_list.title.trim());
    }
    for task in &task_list.tasks {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&format!(
            "- [{}] {}",
            render_task_status(task),
            task.description
        ));
        if rendered.len() > SUPERVISION_SECTION_MAX_BYTES {
            break;
        }
    }
    cap_text(&rendered, SUPERVISION_SECTION_MAX_BYTES)
}

fn render_task_status(task: &Task) -> &'static str {
    match task.status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in progress",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
    }
}

fn cap_text(text: &str, max_bytes: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_owned();
    }
    let mut end = max_bytes;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &trimmed[..end])
}

pub(crate) fn parse_supervision_verdict(raw: &str) -> Result<SupervisionVerdict, String> {
    let mut lines = raw.lines();
    let verdict_word = loop {
        let Some(line) = lines.next() else {
            return Err(format!(
                "supervisor output contained no VERDICT line, got {:?}",
                cap_text(raw, 256)
            ));
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.chars().all(|ch| ch == '`') {
            continue;
        }
        let Some(rest) = strip_verdict_marker(trimmed) else {
            return Err(format!(
                "supervisor output did not start with a VERDICT line, got {:?}",
                cap_text(raw, 256)
            ));
        };
        break rest
            .trim()
            .trim_matches(|ch: char| !ch.is_ascii_alphabetic())
            .to_ascii_lowercase();
    };

    match verdict_word.as_str() {
        "done" => Ok(SupervisionVerdict::Done),
        "awaiting_user" => Ok(SupervisionVerdict::AwaitingUser),
        "continue" => {
            let message = lines
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .trim_matches('`')
                .trim()
                .to_owned();
            if message.is_empty() {
                return Err("supervisor answered continue without a follow-up message".to_owned());
            }
            Ok(SupervisionVerdict::Continue { message })
        }
        other => Err(format!("supervisor produced unknown verdict {other:?}")),
    }
}

fn strip_verdict_marker(line: &str) -> Option<&str> {
    let upper = line.to_ascii_uppercase();
    let marker = upper.find("VERDICT:")?;
    // Reject prose that merely mentions the word mid-sentence; allow leading
    // markdown decoration like "**VERDICT: done**".
    if line[..marker].chars().any(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(&line[marker + "VERDICT:".len()..])
}
