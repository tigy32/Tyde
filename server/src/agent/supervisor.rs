//! Agent supervisor: a hidden one-shot model call that reviews an idle
//! agent's last turn and decides whether the user's request is actually
//! finished, awaiting user input, or should be kicked back to work.
//!
//! Like the agent name generator, the supervisor is an implementation detail
//! of the host — it never becomes a protocol entity. Each verdict runs on a
//! throwaway unregistered agent id with an isolated tempdir workspace, no
//! tools, and inference-only backend hardening.

use protocol::{
    ChatEvent, ChatMessage, Envelope, FrameKind, MessageSender, SUPERVISOR_MESSAGE_PREFIX,
    SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX, SendMessagePayload, SessionSettingsValues, Task,
    TaskList, TaskStatus,
};
use tokio::sync::mpsc;

use super::{
    AgentId, BackendAccessMode, BackendExecutionMode, BackendKind, BackendSpawnConfig, EventStream,
    HostCapacityTx, HostSubAgentEmitterContext, SpawnCostHint, ToolPolicy, spawn_backend,
};

/// Byte caps for each supervision prompt section, so one huge message cannot
/// blow up the (paid) supervision call.
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
        execution_mode: BackendExecutionMode::InferenceOnly,
        cost_hint,
        custom_agent_id: None,
        startup_mcp_servers: Vec::new(),
        session_settings,
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
        .map(|text| cap_text(text, SUPERVISION_SECTION_MAX_BYTES))
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        ChatMessageId, ContextBreakdown, MessageMetadataUpdateData, StreamEndData, StreamPath,
    };

    fn user_message(content: &str) -> ChatMessage {
        chat_message(MessageSender::User, content)
    }

    fn assistant_message(content: &str) -> ChatMessage {
        chat_message(
            MessageSender::Assistant {
                agent: "agent".to_owned(),
            },
            content,
        )
    }

    fn assistant_message_with_context(
        message_id: &str,
        content: &str,
        input_tokens: Option<u64>,
    ) -> ChatMessage {
        let mut message = assistant_message(content);
        message.message_id = Some(ChatMessageId(message_id.to_owned()));
        message.context_breakdown = input_tokens.map(context_breakdown);
        message
    }

    fn context_breakdown(input_tokens: u64) -> ContextBreakdown {
        ContextBreakdown {
            system_prompt_bytes: 1,
            tool_io_bytes: 2,
            conversation_history_bytes: 3,
            reasoning_bytes: 4,
            context_injection_bytes: 5,
            input_tokens,
            context_window: 300_000,
        }
    }

    fn chat_message(sender: MessageSender, content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 0,
            sender,
            content: content.to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    fn envelope(seq: u64, event: ChatEvent) -> Envelope {
        Envelope::from_payload(
            StreamPath("/test".to_owned()),
            FrameKind::ChatEvent,
            seq,
            &event,
        )
        .expect("chat event serializes")
    }

    #[test]
    fn snapshot_tracks_user_and_assistant_messages() {
        let log = vec![
            envelope(
                1,
                ChatEvent::MessageAdded(user_message("build the feature")),
            ),
            envelope(
                2,
                ChatEvent::StreamEnd(StreamEndData {
                    message: assistant_message("done building"),
                }),
            ),
        ];
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(
            snapshot.last_user_message.as_deref(),
            Some("build the feature")
        );
        assert_eq!(snapshot.user_message_count, 1);
        assert_eq!(snapshot.kicks_since_user_message, 0);
        assert_eq!(
            snapshot.last_assistant_message.as_deref(),
            Some("done building")
        );
        assert!(snapshot.last_error_since_user_message.is_none());
        assert!(!snapshot.cancelled_since_user_message);
    }

    #[test]
    fn snapshot_tracks_latest_assistant_context_and_matching_metadata() {
        let mut log = vec![envelope(
            1,
            ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_context("latest", "done", Some(210_000)),
            }),
        )];
        assert_eq!(
            supervision_context_snapshot(&log).current_context_input_tokens,
            Some(210_000)
        );

        log.push(envelope(
            2,
            ChatEvent::MessageMetadataUpdated(MessageMetadataUpdateData {
                message_id: ChatMessageId("older".to_owned()),
                model_info: None,
                token_usage: None,
                context_breakdown: Some(context_breakdown(220_000)),
            }),
        ));
        assert_eq!(
            supervision_context_snapshot(&log).current_context_input_tokens,
            Some(210_000),
            "metadata for another message must not replace the latest context"
        );

        log.push(envelope(
            3,
            ChatEvent::MessageMetadataUpdated(MessageMetadataUpdateData {
                message_id: ChatMessageId("latest".to_owned()),
                model_info: None,
                token_usage: None,
                context_breakdown: Some(context_breakdown(230_000)),
            }),
        ));
        assert_eq!(
            supervision_context_snapshot(&log).current_context_input_tokens,
            Some(230_000),
            "matching metadata must replace the completed message breakdown"
        );

        log.push(envelope(
            4,
            ChatEvent::MessageAdded(assistant_message_with_context("newest", "new answer", None)),
        ));
        assert_eq!(
            supervision_context_snapshot(&log).current_context_input_tokens,
            None,
            "a newer assistant completion without a breakdown must clear stale usage"
        );
    }

    #[test]
    fn snapshot_accepts_late_matching_context_metadata() {
        let mut log = vec![envelope(
            1,
            ChatEvent::StreamEnd(StreamEndData {
                message: assistant_message_with_context("late", "done", None),
            }),
        )];
        assert_eq!(
            supervision_context_snapshot(&log).current_context_input_tokens,
            None,
            "the completed assistant turn starts without context usage"
        );

        log.push(envelope(
            2,
            ChatEvent::MessageMetadataUpdated(MessageMetadataUpdateData {
                message_id: ChatMessageId("late".to_owned()),
                model_info: None,
                token_usage: None,
                context_breakdown: Some(context_breakdown(240_000)),
            }),
        ));
        assert_eq!(
            supervision_context_snapshot(&log).current_context_input_tokens,
            Some(240_000),
            "matching late metadata must promote unavailable usage to known usage"
        );
    }

    #[test]
    fn snapshot_counts_kicks_and_resets_on_real_user_message() {
        let kick = format!("{SUPERVISOR_MESSAGE_PREFIX}keep going");
        let log = vec![
            envelope(1, ChatEvent::MessageAdded(user_message("do the task"))),
            envelope(2, ChatEvent::MessageAdded(user_message(&kick))),
            envelope(3, ChatEvent::MessageAdded(user_message(&kick))),
        ];
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(snapshot.kicks_since_user_message, 2);
        assert_eq!(snapshot.user_message_count, 1);
        assert_eq!(snapshot.last_user_message.as_deref(), Some("do the task"));

        let mut log = log;
        log.push(envelope(
            4,
            ChatEvent::MessageAdded(user_message("new ask")),
        ));
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(snapshot.kicks_since_user_message, 0);
        assert_eq!(snapshot.user_message_count, 2);
        assert_eq!(snapshot.last_user_message.as_deref(), Some("new ask"));
    }

    /// The judge answers `continue` forever if every repeat attempt looks like
    /// its first, so the snapshot must carry the last kick and the reply it
    /// actually drew.
    #[test]
    fn snapshot_pairs_each_kick_with_the_reply_it_drew() {
        let first_kick = format!("{SUPERVISOR_MESSAGE_PREFIX}Finish the tests.");
        let second_kick = format!("{SUPERVISOR_MESSAGE_PREFIX}Finish the tests, then report.");
        let mut log = vec![
            envelope(1, ChatEvent::MessageAdded(user_message("do the task"))),
            envelope(2, ChatEvent::MessageAdded(assistant_message("Approve?"))),
            envelope(3, ChatEvent::MessageAdded(user_message(&first_kick))),
        ];
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(
            snapshot.last_kick_message.as_deref(),
            Some("Finish the tests.")
        );
        assert_eq!(
            snapshot.last_reply_to_kick, None,
            "the pre-kick message is not an answer to the kick"
        );

        log.push(envelope(
            4,
            ChatEvent::MessageAdded(assistant_message("Awaiting explicit user approval.")),
        ));
        assert_eq!(
            supervision_context_snapshot(&log)
                .last_reply_to_kick
                .as_deref(),
            Some("Awaiting explicit user approval.")
        );

        log.push(envelope(
            5,
            ChatEvent::MessageAdded(user_message(&second_kick)),
        ));
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(
            snapshot.last_kick_message.as_deref(),
            Some("Finish the tests, then report.")
        );
        assert_eq!(
            snapshot.last_reply_to_kick, None,
            "the previous kick's reply must not be attributed to the new kick"
        );

        log.push(envelope(
            6,
            ChatEvent::MessageAdded(user_message("new ask")),
        ));
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(
            (snapshot.last_kick_message, snapshot.last_reply_to_kick),
            (None, None),
            "a real user message starts a fresh request with no prior attempts"
        );
    }

    #[test]
    fn snapshot_tracks_errors_and_cancellation_since_user_message() {
        let log = vec![
            envelope(1, ChatEvent::MessageAdded(user_message("first"))),
            envelope(
                2,
                ChatEvent::MessageAdded(chat_message(MessageSender::Error, "stale error")),
            ),
            envelope(3, ChatEvent::MessageAdded(user_message("second"))),
        ];
        let snapshot = supervision_context_snapshot(&log);
        assert!(
            snapshot.last_error_since_user_message.is_none(),
            "errors before the last real user message must not leak into the context"
        );

        let mut log = log;
        log.push(envelope(
            4,
            ChatEvent::MessageAdded(chat_message(MessageSender::Error, "boom")),
        ));
        let snapshot = supervision_context_snapshot(&log);
        assert_eq!(
            snapshot.last_error_since_user_message.as_deref(),
            Some("boom")
        );
    }

    /// A user cancel must keep suppressing supervision, but the supervisor's
    /// own stall interrupt must not: the cancel it provokes is the supervisor's
    /// doing, and the whole point is to judge the truncated turn afterwards.
    #[test]
    fn snapshot_separates_a_stall_interrupt_from_a_user_cancel() {
        let notice = format!(
            "{SUPERVISOR_STALL_INTERRUPT_NOTICE_PREFIX} 30 minutes. Checking how to make progress."
        );
        let log = vec![
            envelope(1, ChatEvent::MessageAdded(user_message("do the task"))),
            envelope(
                2,
                ChatEvent::MessageAdded(chat_message(MessageSender::Warning, &notice)),
            ),
            envelope(
                3,
                ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                    message: "cancelled".to_owned(),
                }),
            ),
        ];
        let snapshot = supervision_context_snapshot(&log);
        assert!(
            !snapshot.cancelled_since_user_message,
            "the supervisor's own interrupt must not read as a user cancel"
        );
        assert!(snapshot.last_turn_was_stall_interrupted);

        let mut user_cancel = log.clone();
        user_cancel.push(envelope(
            4,
            ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                message: "cancelled".to_owned(),
            }),
        ));
        assert!(
            supervision_context_snapshot(&user_cancel).cancelled_since_user_message,
            "only the one cancel the notice armed is excused; a second cancel still counts"
        );

        let mut next_request = log.clone();
        next_request.push(envelope(
            4,
            ChatEvent::MessageAdded(user_message("new ask")),
        ));
        next_request.push(envelope(
            5,
            ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                message: "cancelled".to_owned(),
            }),
        ));
        let snapshot = supervision_context_snapshot(&next_request);
        assert!(
            snapshot.cancelled_since_user_message,
            "a new user message closes the excuse window, so their next stop counts"
        );
        assert!(!snapshot.last_turn_was_stall_interrupted);
    }

    #[test]
    fn snapshot_ignores_unrelated_warning_cards() {
        let log = vec![
            envelope(1, ChatEvent::MessageAdded(user_message("do the task"))),
            envelope(
                2,
                ChatEvent::MessageAdded(chat_message(
                    MessageSender::Warning,
                    "Supervisor could not verify whether this task was complete after 2 attempts",
                )),
            ),
            envelope(
                3,
                ChatEvent::OperationCancelled(protocol::OperationCancelledData {
                    message: "cancelled".to_owned(),
                }),
            ),
        ];
        let snapshot = supervision_context_snapshot(&log);
        assert!(!snapshot.last_turn_was_stall_interrupted);
        assert!(
            snapshot.cancelled_since_user_message,
            "an unrelated warning must not excuse a user cancel"
        );
    }

    #[test]
    fn stall_interrupt_prompt_reports_the_truncated_turn() {
        let mut request = GenerateSupervisionVerdictRequest {
            verdict_agent_id: AgentId("test".to_owned()),
            backend_kind: BackendKind::Claude,
            last_user_message: "implement the parser".to_owned(),
            task_list: None,
            last_assistant_message: Some("Reading the lexer".to_owned()),
            last_error: None,
            stall_interrupted: true,
            kicks_so_far: 0,
            last_kick_message: None,
            last_reply_to_kick: None,
            cost_hint: Some(SpawnCostHint::Low),
            session_settings: None,
            use_mock_backend: true,
            capacity_tx: mpsc::unbounded_channel().0,
        };
        let prompt = build_supervision_prompt(&request);
        assert!(prompt.contains("stopped making observable progress"));
        assert!(prompt.contains("truncated"));
        assert!(prompt.contains("smaller concrete next step"));
        assert!(
            prompt.contains("VERDICT: continue"),
            "the interrupt notice must not displace the verdict contract"
        );
        assert_eq!(
            generate_mock_supervision_verdict(&request),
            Ok(SupervisionVerdict::Continue {
                message: "Please continue working on the task until it is complete.".to_owned()
            })
        );

        request.stall_interrupted = false;
        let prompt = build_supervision_prompt(&request);
        assert!(!prompt.contains("stopped making observable progress"));
        assert!(prompt.contains("implement the parser"));
    }

    #[test]
    fn parse_accepts_all_exact_verdicts() {
        assert_eq!(
            parse_supervision_verdict("VERDICT: done"),
            Ok(SupervisionVerdict::Done)
        );
        assert_eq!(
            parse_supervision_verdict("verdict: Done\n"),
            Ok(SupervisionVerdict::Done)
        );
        assert_eq!(
            parse_supervision_verdict("VERDICT: awaiting_user"),
            Ok(SupervisionVerdict::AwaitingUser)
        );
        assert_eq!(
            parse_supervision_verdict("VERDICT: continue\nKeep going, task 3 is pending."),
            Ok(SupervisionVerdict::Continue {
                message: "Keep going, task 3 is pending.".to_owned()
            })
        );
    }

    #[test]
    fn parse_tolerates_fences_and_markdown_decoration() {
        assert_eq!(
            parse_supervision_verdict("```\nVERDICT: done\n```"),
            Ok(SupervisionVerdict::Done)
        );
        assert_eq!(
            parse_supervision_verdict("```\n**VERDICT: awaiting_user**\nignored detail\n```"),
            Ok(SupervisionVerdict::AwaitingUser)
        );
        assert_eq!(
            parse_supervision_verdict("VERDICT: done\nignored detail"),
            Ok(SupervisionVerdict::Done)
        );
        assert_eq!(
            parse_supervision_verdict("**VERDICT: continue**\nFinish the remaining tests."),
            Ok(SupervisionVerdict::Continue {
                message: "Finish the remaining tests.".to_owned()
            })
        );
    }

    #[test]
    fn parse_rejects_invalid_output() {
        assert!(parse_supervision_verdict("the task looks finished to me").is_err());
        assert!(parse_supervision_verdict("VERDICT: maybe").is_err());
        assert!(parse_supervision_verdict("VERDICT: awaiting").is_err());
        assert!(parse_supervision_verdict("VERDICT: done because complete").is_err());
        assert!(parse_supervision_verdict("VERDICT: awaiting_user now").is_err());
        assert!(
            parse_supervision_verdict("VERDICT: continue\n\n").is_err(),
            "continue without a follow-up message must be rejected"
        );
        assert!(
            parse_supervision_verdict("I think the VERDICT: done applies").is_err(),
            "prose mentioning the marker mid-sentence is not a verdict"
        );
    }

    /// Replaces `prompt_includes_task_list_and_kick_budget`, which pinned the
    /// exact clauses that caused the ping-pong loop this change fixes: it
    /// asserted the prompt contains "stopped early while executable work
    /// remains" and "1 of 3 allowed". The first stated an unfinished task as
    /// sufficient grounds for `continue`, which is true of an agent that
    /// deliberately stopped to ask a question; the second framed the kick
    /// budget as an allowance to spend. Both are now contradicted by the
    /// guidance, so an assertion demanding them would pin the defect. The
    /// contract they reached for (the prompt must carry the full verdict
    /// guidance and the caller's context) is preserved and sharpened below:
    /// each verdict's grounds are now asserted individually, including the
    /// exclusions the old wording left implicit.
    #[test]
    fn prompt_states_the_unintended_stop_contract() {
        let request = GenerateSupervisionVerdictRequest {
            verdict_agent_id: AgentId("test".to_owned()),
            backend_kind: BackendKind::Claude,
            last_user_message: "implement the parser".to_owned(),
            task_list: Some(TaskList {
                title: "Parser work".to_owned(),
                tasks: vec![Task {
                    id: 1,
                    description: "write tests".to_owned(),
                    status: TaskStatus::Pending,
                }],
            }),
            last_assistant_message: Some("I stopped".to_owned()),
            last_error: None,
            stall_interrupted: false,
            kicks_so_far: 1,
            last_kick_message: Some("Finish the tests, then report results.".to_owned()),
            last_reply_to_kick: Some("Awaiting explicit user approval.".to_owned()),
            cost_hint: Some(SpawnCostHint::Low),
            session_settings: None,
            use_mock_backend: true,
            capacity_tx: mpsc::unbounded_channel().0,
        };
        let prompt = build_supervision_prompt(&request);
        assert!(prompt.contains("implement the parser"));
        assert!(prompt.contains("- [pending] write tests"));

        // continue is gated on evidence the stop was not the agent's choice.
        assert!(prompt.contains("ended where the agent intended it to end"));
        assert!(prompt.contains("never answer continue"));
        assert!(prompt.contains("Answer continue ONLY when something outside the agent's control"));
        assert!(prompt.contains("Those are the only grounds for continue."));
        assert!(prompt.contains("allowed to stop with work remaining"));
        assert!(
            prompt.contains(
                "the task list still has pending or in-progress items, that the agent \
stopped mid-task, or that the agent could have done more are NOT grounds for continue"
            ),
            "unfinished work must be named as an explicit non-ground, not merely left unlisted"
        );

        // A deliberate stop belongs to the user, and the judge may not pose as
        // them to get past it.
        assert!(prompt.contains("Treat the agent's stated reason for stopping as authoritative."));
        assert!(prompt.contains("declined to act without user approval is awaiting_user, always"));
        assert!(prompt.contains("arrives as if the user had sent it"));
        assert!(prompt.contains(
            "Never claim or imply that the user said, approved, permitted, or decided anything"
        ));

        // A repeating judge sees its own refused attempt instead of a budget.
        assert!(prompt.contains("Finish the tests, then report results."));
        assert!(prompt.contains("Awaiting explicit user approval."));
        assert!(prompt.contains("another one will not either: answer awaiting_user"));
        assert!(
            !prompt.contains("Supervisor follow-ups already sent for this request"),
            "the kick count must not read as a remaining allowance to spend"
        );
    }

    #[test]
    fn prompt_omits_the_repeat_section_on_a_first_verdict() {
        let request = GenerateSupervisionVerdictRequest {
            verdict_agent_id: AgentId("test".to_owned()),
            backend_kind: BackendKind::Claude,
            last_user_message: "implement the parser".to_owned(),
            task_list: None,
            last_assistant_message: Some("I stopped".to_owned()),
            last_error: None,
            stall_interrupted: false,
            kicks_so_far: 0,
            last_kick_message: None,
            last_reply_to_kick: None,
            cost_hint: Some(SpawnCostHint::Low),
            session_settings: None,
            use_mock_backend: true,
            capacity_tx: mpsc::unbounded_channel().0,
        };
        let prompt = build_supervision_prompt(&request);
        assert!(
            !prompt.contains("automated follow-up(s)"),
            "a first verdict has no prior attempt to report"
        );
        assert!(prompt.contains("Those are the only grounds for continue."));
    }

    #[test]
    fn mock_sentinels_map_to_explicit_verdicts() {
        fn request(last_user_message: &str) -> GenerateSupervisionVerdictRequest {
            GenerateSupervisionVerdictRequest {
                verdict_agent_id: AgentId("test".to_owned()),
                backend_kind: BackendKind::Claude,
                last_user_message: last_user_message.to_owned(),
                task_list: None,
                last_assistant_message: None,
                last_error: None,
                stall_interrupted: false,
                kicks_so_far: 0,
                last_kick_message: None,
                last_reply_to_kick: None,
                cost_hint: Some(SpawnCostHint::Low),
                session_settings: None,
                use_mock_backend: true,
                capacity_tx: mpsc::unbounded_channel().0,
            }
        }

        assert_eq!(
            generate_mock_supervision_verdict(&request(MOCK_SUPERVISOR_DONE)),
            Ok(SupervisionVerdict::Done)
        );
        assert_eq!(
            generate_mock_supervision_verdict(&request(MOCK_SUPERVISOR_AWAITING_USER)),
            Ok(SupervisionVerdict::AwaitingUser)
        );
        assert!(matches!(
            generate_mock_supervision_verdict(&request(MOCK_SUPERVISOR_CONTINUE)),
            Ok(SupervisionVerdict::Continue { .. })
        ));
        assert!(matches!(
            generate_mock_supervision_verdict(&request(MOCK_SUPERVISOR_ERROR)),
            Err(SupervisionFailure {
                kind: SupervisionFailureKind::BackendStream,
                ..
            })
        ));
        assert!(matches!(
            generate_mock_supervision_verdict(&request(MOCK_SUPERVISOR_INVALID)),
            Err(SupervisionFailure {
                kind: SupervisionFailureKind::InvalidVerdict,
                ..
            })
        ));
    }

    #[test]
    fn supervisor_classifies_non_retryable_backend_failures() {
        let start = SupervisionFailure::new(
            SupervisionFailureKind::BackendStart,
            "profile authentication is unavailable",
        );
        let stream = SupervisionFailure::new(
            SupervisionFailureKind::BackendStream,
            "transient stream ended",
        );
        let terminal = SupervisionFailure::new(
            SupervisionFailureKind::BackendTerminal,
            "Hermes exhausted provider routing",
        );

        assert!(!start.is_retryable());
        assert!(!terminal.is_retryable());
        assert!(stream.is_retryable());
        assert_eq!(
            supervision_backend_error_kind(BackendKind::Hermes),
            SupervisionFailureKind::BackendTerminal
        );
        assert_eq!(
            supervision_backend_error_kind(BackendKind::Claude),
            SupervisionFailureKind::BackendStream
        );
    }

    #[test]
    fn supervisor_hermes_terminal_taxonomy_fails_closed_without_parsing_prose() {
        for message in [
            "No allowed providers are available for the selected model.",
            "provider connection reset before response",
        ] {
            let failure = SupervisionFailure::new(
                supervision_backend_error_kind(BackendKind::Hermes),
                message,
            );
            assert_eq!(failure.kind, SupervisionFailureKind::BackendTerminal);
            assert!(!failure.is_retryable());
        }
    }

    #[test]
    fn supervisor_spawn_preserves_scoped_hermes_session_settings() {
        let mut settings = SessionSettingsValues::default();
        settings.0.insert(
            crate::backend::hermes::HERMES_PROFILE_SETTING.to_string(),
            protocol::SessionSettingValue::String("work".to_string()),
        );
        settings.0.insert(
            "model".to_owned(),
            protocol::SessionSettingValue::String(
                "minimax/minimax-m3 --provider openrouter".to_owned(),
            ),
        );
        settings.0.insert(
            "reasoning_effort".to_owned(),
            protocol::SessionSettingValue::String("none".to_owned()),
        );

        let config = supervision_spawn_config(Some(SpawnCostHint::Low), Some(settings.clone()));

        assert_eq!(config.session_settings, Some(settings));
        assert_eq!(
            config.resolved_spawn_config.tool_policy,
            ToolPolicy::AllowList { tools: Vec::new() }
        );
        assert_eq!(
            config.resolved_spawn_config.access_mode,
            BackendAccessMode::ReadOnly
        );
    }
}
