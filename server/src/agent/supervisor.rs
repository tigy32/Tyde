//! Agent supervisor: a hidden one-shot model call that reviews an idle
//! agent's last turn and decides whether the user's request is actually
//! finished or the agent should be kicked back to work.
//!
//! Like the agent name generator, the supervisor is an implementation detail
//! of the host — it never becomes a protocol entity. Each verdict runs on a
//! throwaway unregistered agent id with an isolated tempdir workspace, no
//! tools, and inference-only backend hardening.

use protocol::{
    ChatEvent, ChatMessage, Envelope, FrameKind, MessageSender, SUPERVISOR_MESSAGE_PREFIX,
    SendMessagePayload, Task, TaskList, TaskStatus,
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
    /// The user's request is complete (or legitimately waiting on the user).
    Done,
    /// The agent stopped early; send this follow-up message to keep it going.
    Continue { message: String },
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
                snapshot.cancelled_since_user_message = true;
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
            if message.content.starts_with(SUPERVISOR_MESSAGE_PREFIX) {
                snapshot.kicks_since_user_message =
                    snapshot.kicks_since_user_message.saturating_add(1);
            } else {
                snapshot.last_user_message = Some(message.content.clone());
                snapshot.user_message_count = snapshot.user_message_count.saturating_add(1);
                snapshot.kicks_since_user_message = 0;
                snapshot.last_error_since_user_message = None;
            }
            // Any new message (real or kick) supersedes an earlier cancel:
            // work is running again on purpose.
            snapshot.cancelled_since_user_message = false;
        }
        MessageSender::Assistant { .. } => {
            *latest_assistant_message_id = message.message_id.clone();
            snapshot.current_context_input_tokens = message
                .context_breakdown
                .as_ref()
                .map(|breakdown| breakdown.input_tokens);
            if !message.content.trim().is_empty() {
                snapshot.last_assistant_message = Some(message.content.clone());
            }
        }
        MessageSender::Error => {
            snapshot.last_error_since_user_message = Some(message.content.clone());
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
    pub kicks_so_far: u32,
    pub max_kicks: u32,
    /// Model tier for the verdict call; `None` runs the backend's default.
    pub cost_hint: Option<SpawnCostHint>,
    pub use_mock_backend: bool,
    pub capacity_tx: HostCapacityTx,
}

pub(crate) async fn generate_supervision_verdict(
    request: GenerateSupervisionVerdictRequest,
) -> Result<SupervisionVerdict, String> {
    if request.use_mock_backend {
        return generate_mock_supervision_verdict(&request);
    }

    let prompt = build_supervision_prompt(&request);
    let spawn_config = supervision_spawn_config(request.cost_hint);
    let isolated_workspace = tempfile::tempdir()
        .map_err(|err| format!("failed to create isolated supervision workspace: {err}"))?;
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
            return Err(format!(
                "agent supervisor failed to start for backend {:?}: {}",
                request.backend_kind, err
            ));
        }
    };

    let result = collect_supervision_events(&mut events).await;
    if let Err(err) = &result {
        tracing::warn!(
            backend_kind = ?request.backend_kind,
            error = %err,
            "agent supervision call failed"
        );
    }
    result
}

fn supervision_spawn_config(cost_hint: Option<SpawnCostHint>) -> BackendSpawnConfig {
    BackendSpawnConfig {
        execution_mode: BackendExecutionMode::InferenceOnly,
        cost_hint,
        custom_agent_id: None,
        startup_mcp_servers: Vec::new(),
        session_settings: None,
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
) -> Result<SupervisionVerdict, String> {
    let mut streamed_text = String::new();
    while let Some(event) = events.recv().await {
        match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                return Err(message.content);
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
                return parse_supervision_verdict(&candidate);
            }
            ChatEvent::TypingStatusChanged(false) => {
                return Err(
                    "agent supervisor turn completed before producing a verdict".to_string()
                );
            }
            _ => {}
        }
    }

    Err("agent supervisor ended before producing a verdict".to_string())
}

fn generate_mock_supervision_verdict(
    request: &GenerateSupervisionVerdictRequest,
) -> Result<SupervisionVerdict, String> {
    if request
        .last_user_message
        .contains("__mock_supervisor_error__")
    {
        return Err("mock supervision failure".to_owned());
    }
    if request
        .last_user_message
        .contains("__mock_supervisor_invalid__")
    {
        return parse_supervision_verdict("this is not a verdict");
    }
    if request
        .last_user_message
        .contains("__mock_supervisor_done__")
    {
        return Ok(SupervisionVerdict::Done);
    }
    if request
        .last_user_message
        .contains("__mock_supervisor_continue__")
        || request.last_error.is_some()
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
    format!(
        "You supervise a coding agent that just went idle. Decide whether it actually \
finished the user's request or stopped early.\n\
Reply with EXACTLY one of these two forms and nothing else:\n\
VERDICT: done\n\
or\n\
VERDICT: continue\n\
<one short follow-up message telling the agent to keep working and what remains>\n\
Rules:\n\
- Answer done when the agent's final message shows the request was completed, or the agent \
is legitimately waiting for an answer or decision from the user.\n\
- Answer continue when an error interrupted the work, the agent stopped mid-task, or the \
task list still has pending or in-progress items covered by the user's request.\n\
- The follow-up message is sent verbatim to the agent. Keep it short and specific.\n\
- Never invent new work or expand scope beyond the user's request.\n\n\
User request:\n{user_message}\n\n\
Agent task list:\n{task_list}\n\n\
Agent's final message:\n{last_agent_message}\n\n\
Most recent error since the user's request:\n{last_error}\n\n\
Supervisor follow-ups already sent for this request: {kicks} of {max_kicks} allowed",
        kicks = request.kicks_so_far,
        max_kicks = request.max_kicks,
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

/// Runs one supervision call up to `1 + retry_attempts` times, retrying when
/// the call errors or its output does not parse to a verdict. Each attempt is
/// a fresh ephemeral backend spawn.
pub(crate) async fn run_supervision_with_retries<F, Fut>(
    retry_attempts: u32,
    mut run_attempt: F,
) -> Result<SupervisionVerdict, String>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<SupervisionVerdict, String>>,
{
    let attempts = retry_attempts.saturating_add(1);
    let mut last_error = String::new();
    for attempt in 0..attempts {
        match run_attempt(attempt).await {
            Ok(verdict) => return Ok(verdict),
            Err(err) => {
                tracing::warn!(
                    attempt = attempt + 1,
                    attempts,
                    error = %err,
                    "agent supervision attempt failed"
                );
                last_error = err;
            }
        }
    }
    Err(last_error)
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

    #[test]
    fn parse_accepts_done_and_continue() {
        assert_eq!(
            parse_supervision_verdict("VERDICT: done"),
            Ok(SupervisionVerdict::Done)
        );
        assert_eq!(
            parse_supervision_verdict("verdict: Done\n"),
            Ok(SupervisionVerdict::Done)
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
        assert!(
            parse_supervision_verdict("VERDICT: continue\n\n").is_err(),
            "continue without a follow-up message must be rejected"
        );
        assert!(
            parse_supervision_verdict("I think the VERDICT: done applies").is_err(),
            "prose mentioning the marker mid-sentence is not a verdict"
        );
    }

    #[tokio::test]
    async fn retries_stop_after_first_success() {
        let mut calls = 0u32;
        let result = run_supervision_with_retries(2, |attempt| {
            calls += 1;
            async move {
                if attempt == 0 {
                    Err("first attempt fails".to_owned())
                } else {
                    Ok(SupervisionVerdict::Done)
                }
            }
        })
        .await;
        assert_eq!(result, Ok(SupervisionVerdict::Done));
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn retries_exhaust_to_last_error() {
        let mut calls = 0u32;
        let result = run_supervision_with_retries(1, |attempt| {
            calls += 1;
            async move { Err::<SupervisionVerdict, _>(format!("attempt {attempt} failed")) }
        })
        .await;
        assert_eq!(result, Err("attempt 1 failed".to_owned()));
        assert_eq!(calls, 2, "retry_attempts=1 means exactly two attempts");
    }

    #[test]
    fn prompt_includes_task_list_and_kick_budget() {
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
            kicks_so_far: 1,
            max_kicks: 3,
            cost_hint: Some(SpawnCostHint::Low),
            use_mock_backend: true,
            capacity_tx: mpsc::unbounded_channel().0,
        };
        let prompt = build_supervision_prompt(&request);
        assert!(prompt.contains("implement the parser"));
        assert!(prompt.contains("- [pending] write tests"));
        assert!(prompt.contains("1 of 3 allowed"));
    }
}
