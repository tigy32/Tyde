use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

const MAX_SUMMARY_LEN: usize = 180;
const MAX_MESSAGE_LEN: usize = 4_000;
const DEFAULT_EVENT_LOG_LIMIT: usize = 5_000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Queued,
    Running,
    WaitingInput,
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub agent_id: u64,
    pub conversation_id: u64,
    pub workspace_roots: Vec<String>,
    pub backend_kind: String,
    pub parent_agent_id: Option<u64>,
    pub name: String,
    pub agent_type: Option<String>,
    pub status: AgentStatus,
    pub summary: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub agent_id: u64,
    pub conversation_id: u64,
    pub kind: String,
    pub status: AgentStatus,
    pub timestamp_ms: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentEventBatch {
    pub events: Vec<AgentEvent>,
    pub latest_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectedAgentResult {
    pub agent: AgentInfo,
    pub final_message: Option<String>,
    pub changed_files: Vec<String>,
    pub tool_results: Vec<Value>,
}

pub struct AgentRuntime {
    next_agent_id: u64,
    next_event_seq: u64,
    agents: HashMap<u64, AgentInfo>,
    conversation_to_agent: HashMap<u64, u64>,
    events: VecDeque<AgentEvent>,
    event_log_limit: usize,
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self {
            next_agent_id: 1,
            next_event_seq: 1,
            agents: HashMap::new(),
            conversation_to_agent: HashMap::new(),
            events: VecDeque::new(),
            event_log_limit: DEFAULT_EVENT_LOG_LIMIT,
        }
    }

    pub fn has_agent(&self, agent_id: u64) -> bool {
        self.agents.contains_key(&agent_id)
    }

    pub fn conversation_id_for_agent(&self, agent_id: u64) -> Option<u64> {
        self.agents.get(&agent_id).map(|a| a.conversation_id)
    }

    pub fn get_agent(&self, agent_id: u64) -> Option<AgentInfo> {
        self.agents.get(&agent_id).cloned()
    }

    pub fn list_agents(&self) -> Vec<AgentInfo> {
        let mut out = self.agents.values().cloned().collect::<Vec<_>>();
        out.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
        out
    }

    pub fn children_of(&self, agent_id: u64) -> Vec<AgentInfo> {
        let mut children: Vec<AgentInfo> = self
            .agents
            .values()
            .filter(|a| a.parent_agent_id == Some(agent_id))
            .cloned()
            .collect();
        children.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
        children
    }

    /// Reserve an agent ID without registering the agent yet.
    /// Use `register_agent_with_id` to complete registration later.
    pub fn reserve_agent_id(&mut self) -> u64 {
        let id = self.next_agent_id;
        self.next_agent_id += 1;
        id
    }

    pub fn register_agent(
        &mut self,
        conversation_id: u64,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<u64>,
        name: String,
    ) -> AgentInfo {
        let agent_id = self.reserve_agent_id();
        self.register_agent_with_id(
            agent_id,
            conversation_id,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
        )
    }

    pub fn register_agent_with_id(
        &mut self,
        agent_id: u64,
        conversation_id: u64,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<u64>,
        name: String,
    ) -> AgentInfo {
        let now = now_ms();

        let info = AgentInfo {
            agent_id,
            conversation_id,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            agent_type: None,
            status: AgentStatus::Queued,
            summary: "Queued".to_string(),
            created_at_ms: now,
            updated_at_ms: now,
            ended_at_ms: None,
            last_error: None,
            last_message: None,
        };
        self.agents.insert(agent_id, info.clone());
        self.conversation_to_agent.insert(conversation_id, agent_id);
        self.push_event(
            agent_id,
            conversation_id,
            "agent_spawned",
            AgentStatus::Queued,
            Some("Queued".to_string()),
        );
        info
    }

    pub fn update_agent_type(&mut self, agent_id: u64, agent_type: Option<String>) {
        if let Some(info) = self.agents.get_mut(&agent_id) {
            info.agent_type = agent_type;
        }
    }

    pub fn mark_agent_running(&mut self, agent_id: u64, summary: Option<String>) -> bool {
        self.update_status_for_agent(
            agent_id,
            AgentStatus::Running,
            summary,
            None,
            "agent_running",
            None,
        )
    }

    pub fn mark_conversation_failed(&mut self, conversation_id: u64, message: String) -> bool {
        self.update_status_for_conversation(
            conversation_id,
            AgentStatus::Failed,
            Some(message.clone()),
            Some(message),
            "agent_failed",
            None,
        )
    }

    pub fn mark_conversation_closed(
        &mut self,
        conversation_id: u64,
        message: Option<String>,
    ) -> bool {
        let agent_id = match self.conversation_to_agent.get(&conversation_id).copied() {
            Some(id) => id,
            None => return false,
        };

        let is_terminal = self
            .agents
            .get(&agent_id)
            .map(|agent| agent.status.is_terminal())
            .unwrap_or(false);

        if is_terminal {
            return false;
        }

        self.update_status_for_agent(
            agent_id,
            AgentStatus::Cancelled,
            Some(message.unwrap_or_else(|| "Conversation closed".to_string())),
            None,
            "agent_closed",
            None,
        )
    }

    pub fn record_chat_event(&mut self, conversation_id: u64, event: &Value) -> bool {
        let Some(kind) = event.get("kind").and_then(Value::as_str) else {
            return false;
        };
        if !self.conversation_to_agent.contains_key(&conversation_id) {
            return false;
        }

        match kind {
            "StreamStart" => self.update_status_for_conversation(
                conversation_id,
                AgentStatus::Running,
                Some("Running...".to_string()),
                None,
                "stream_start",
                None,
            ),
            "ToolRequest" => {
                let tool_name = event
                    .get("data")
                    .and_then(|d| d.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                if tool_name == "ask_user_question" {
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::WaitingInput,
                        Some("Waiting for your input".to_string()),
                        None,
                        "tool_request_input",
                        None,
                    )
                } else {
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::Running,
                        Some(format!("Using {tool_name}...")),
                        None,
                        "tool_request",
                        None,
                    )
                }
            }
            "StreamEnd" => {
                let message = event
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let normalized_message = normalize_message(message);
                let summary = if normalized_message.is_empty() {
                    "Completed".to_string()
                } else {
                    summarize_text(&normalized_message, MAX_SUMMARY_LEN)
                };
                let has_tool_calls = event
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(Value::as_array)
                    .map(|calls| !calls.is_empty())
                    .unwrap_or(false);
                if has_tool_calls {
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::Running,
                        Some(summary),
                        None,
                        "stream_end_tool_loop",
                        if normalized_message.is_empty() {
                            None
                        } else {
                            Some(normalized_message)
                        },
                    )
                } else {
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::Completed,
                        Some(summary),
                        None,
                        "stream_end",
                        if normalized_message.is_empty() {
                            None
                        } else {
                            Some(normalized_message)
                        },
                    )
                }
            }
            "Error" => {
                let error = event
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("Agent failed")
                    .to_string();
                self.update_status_for_conversation(
                    conversation_id,
                    AgentStatus::Failed,
                    Some(error.clone()),
                    Some(error),
                    "error",
                    None,
                )
            }
            "OperationCancelled" => {
                let message = event
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Operation cancelled")
                    .to_string();
                self.update_status_for_conversation(
                    conversation_id,
                    AgentStatus::Cancelled,
                    Some(message),
                    None,
                    "operation_cancelled",
                    None,
                )
            }
            "SubprocessExit" => {
                let code = event
                    .get("data")
                    .and_then(|d| d.get("exit_code"))
                    .and_then(Value::as_i64);

                if code == Some(0) {
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::Completed,
                        Some("Completed".to_string()),
                        None,
                        "subprocess_exit_ok",
                        None,
                    )
                } else {
                    let message = match code {
                        Some(c) => format!("Backend exited ({c})"),
                        None => "Backend exited unexpectedly".to_string(),
                    };
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::Failed,
                        Some(message.clone()),
                        Some(message),
                        "subprocess_exit_error",
                        None,
                    )
                }
            }
            "TypingStatusChanged" => {
                let typing = event.get("data").and_then(Value::as_bool).unwrap_or(false);
                if typing {
                    self.update_status_for_conversation(
                        conversation_id,
                        AgentStatus::Running,
                        Some("Running...".to_string()),
                        None,
                        "typing_started",
                        None,
                    )
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn events_since(&self, since_seq: u64, limit: usize) -> AgentEventBatch {
        let cap = limit.clamp(1, 1000);
        let events = self
            .events
            .iter()
            .filter(|event| event.seq > since_seq)
            .take(cap)
            .cloned()
            .collect::<Vec<_>>();
        AgentEventBatch {
            events,
            latest_seq: self.next_event_seq.saturating_sub(1),
        }
    }

    pub fn collect_result(&self, agent_id: u64) -> Result<CollectedAgentResult, String> {
        let agent = self
            .agents
            .get(&agent_id)
            .ok_or_else(|| format!("Agent {agent_id} not found"))?
            .clone();

        if !agent.status.is_terminal() {
            return Err(format!(
                "Agent {agent_id} is still {:?}; collect_result requires a terminal state",
                agent.status
            ));
        }

        let final_message = agent.last_message.clone().or_else(|| {
            if agent.status == AgentStatus::Completed && !agent.summary.trim().is_empty() {
                Some(agent.summary.clone())
            } else {
                None
            }
        });

        Ok(CollectedAgentResult {
            agent,
            final_message,
            changed_files: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    fn update_status_for_conversation(
        &mut self,
        conversation_id: u64,
        status: AgentStatus,
        summary: Option<String>,
        last_error: Option<String>,
        event_kind: &str,
        last_message: Option<String>,
    ) -> bool {
        let agent_id = match self.conversation_to_agent.get(&conversation_id).copied() {
            Some(id) => id,
            None => return false,
        };
        self.update_status_for_agent(
            agent_id,
            status,
            summary,
            last_error,
            event_kind,
            last_message,
        )
    }

    fn update_status_for_agent(
        &mut self,
        agent_id: u64,
        status: AgentStatus,
        summary: Option<String>,
        last_error: Option<String>,
        event_kind: &str,
        last_message: Option<String>,
    ) -> bool {
        let mut changed = false;
        let now = now_ms();

        let event_payload = {
            let Some(agent) = self.agents.get_mut(&agent_id) else {
                return false;
            };

            // Once an agent reaches a terminal state, reject non-terminal transitions.
            // Late events (e.g. TypingStatusChanged) must not revert a finished agent.
            if agent.status.is_terminal() && !status.is_terminal() {
                return false;
            }

            if agent.status != status {
                changed = true;
            }
            agent.status = status.clone();
            agent.updated_at_ms = now;

            if status.is_terminal() {
                if agent.ended_at_ms.is_none() {
                    changed = true;
                }
                agent.ended_at_ms = Some(now);
            } else if agent.ended_at_ms.is_some() {
                changed = true;
                agent.ended_at_ms = None;
            }

            if let Some(summary_value) = summary {
                let normalized = summarize_text(&summary_value, MAX_SUMMARY_LEN);
                if !normalized.is_empty() && agent.summary != normalized {
                    agent.summary = normalized;
                    changed = true;
                }
            }

            if status == AgentStatus::Failed {
                if let Some(err) = last_error {
                    let normalized = summarize_text(&err, MAX_MESSAGE_LEN);
                    if agent.last_error.as_deref() != Some(normalized.as_str()) {
                        agent.last_error = Some(normalized);
                        changed = true;
                    }
                }
            } else if agent.last_error.is_some() {
                changed = true;
                agent.last_error = None;
            }

            if let Some(msg) = last_message {
                let normalized = normalize_message(&msg);
                let next = if normalized.is_empty() {
                    None
                } else {
                    Some(normalized)
                };
                if agent.last_message != next {
                    agent.last_message = next;
                    changed = true;
                }
            }

            (
                agent.conversation_id,
                agent.status.clone(),
                if agent.summary.is_empty() {
                    None
                } else {
                    Some(agent.summary.clone())
                },
            )
        };

        let (conversation_id, current_status, message) = event_payload;
        self.push_event(
            agent_id,
            conversation_id,
            event_kind,
            current_status,
            message,
        );

        changed
    }

    fn push_event(
        &mut self,
        agent_id: u64,
        conversation_id: u64,
        kind: &str,
        status: AgentStatus,
        message: Option<String>,
    ) {
        let event = AgentEvent {
            seq: self.next_event_seq,
            agent_id,
            conversation_id,
            kind: kind.to_string(),
            status,
            timestamp_ms: now_ms(),
            message,
        };
        self.next_event_seq += 1;
        self.events.push_back(event);
        while self.events.len() > self.event_log_limit {
            let _ = self.events.pop_front();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

fn normalize_message(raw: &str) -> String {
    summarize_text(raw, MAX_MESSAGE_LEN)
}

fn summarize_text(raw: &str, max_len: usize) -> String {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= max_len {
        return compact;
    }
    if max_len <= 3 {
        return compact.chars().take(max_len).collect();
    }
    let keep = max_len.saturating_sub(3);
    let mut truncated = String::new();
    for (idx, ch) in compact.chars().enumerate() {
        if idx >= keep {
            break;
        }
        truncated.push(ch);
    }
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_runtime_with_completed_agent() -> (AgentRuntime, u64) {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            100,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );
        let agent_id = info.agent_id;

        // Drive agent to Completed via StreamEnd
        rt.record_chat_event(
            100,
            &json!({
                "kind": "StreamStart",
                "data": {}
            }),
        );
        rt.record_chat_event(
            100,
            &json!({
                "kind": "StreamEnd",
                "data": { "message": { "content": "Done with the task" } }
            }),
        );

        assert_eq!(
            rt.get_agent(agent_id).unwrap().status,
            AgentStatus::Completed
        );
        (rt, agent_id)
    }

    #[test]
    fn terminal_state_rejects_non_terminal_transition() {
        let (mut rt, agent_id) = make_runtime_with_completed_agent();

        // A late TypingStatusChanged(true) must NOT revert Completed -> Running
        let changed = rt.record_chat_event(
            100,
            &json!({
                "kind": "TypingStatusChanged",
                "data": true
            }),
        );
        assert!(!changed);

        let agent = rt.get_agent(agent_id).unwrap();
        assert_eq!(agent.status, AgentStatus::Completed);
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn terminal_state_preserves_last_message() {
        let (mut rt, agent_id) = make_runtime_with_completed_agent();

        // StreamStart after completion must be rejected
        let changed = rt.record_chat_event(
            100,
            &json!({
                "kind": "StreamStart",
                "data": {}
            }),
        );
        assert!(!changed);

        let agent = rt.get_agent(agent_id).unwrap();
        assert_eq!(agent.last_message.as_deref(), Some("Done with the task"));
    }

    #[test]
    fn collect_result_succeeds_for_terminal_agent() {
        let (rt, agent_id) = make_runtime_with_completed_agent();

        let result = rt.collect_result(agent_id);
        assert!(result.is_ok());
        let collected = result.unwrap();
        assert_eq!(collected.agent.status, AgentStatus::Completed);
        assert_eq!(
            collected.final_message.as_deref(),
            Some("Done with the task")
        );
    }

    #[test]
    fn collect_result_errors_for_running_agent() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            200,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );
        rt.mark_agent_running(info.agent_id, Some("Running...".into()));

        let result = rt.collect_result(info.agent_id);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Running"));
    }

    #[test]
    fn collect_result_errors_for_unknown_agent() {
        let rt = AgentRuntime::new();
        let result = rt.collect_result(999);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn failed_agent_preserves_error_after_late_events() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            300,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        rt.record_chat_event(
            300,
            &json!({
                "kind": "Error",
                "data": "Something went wrong"
            }),
        );

        // Late typing event must not clear the error
        rt.record_chat_event(
            300,
            &json!({
                "kind": "TypingStatusChanged",
                "data": true
            }),
        );

        let agent = rt.get_agent(info.agent_id).unwrap();
        assert_eq!(agent.status, AgentStatus::Failed);
        assert!(agent.last_error.is_some());
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn normal_transitions_still_work() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            400,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );
        assert_eq!(
            rt.get_agent(info.agent_id).unwrap().status,
            AgentStatus::Queued
        );

        rt.mark_agent_running(info.agent_id, Some("Running...".into()));
        assert_eq!(
            rt.get_agent(info.agent_id).unwrap().status,
            AgentStatus::Running
        );

        // ToolRequest with ask_user_question -> WaitingInput
        rt.record_chat_event(
            400,
            &json!({
                "kind": "ToolRequest",
                "data": { "tool_name": "ask_user_question" }
            }),
        );
        assert_eq!(
            rt.get_agent(info.agent_id).unwrap().status,
            AgentStatus::WaitingInput
        );

        // Back to Running via StreamStart
        rt.record_chat_event(
            400,
            &json!({
                "kind": "StreamStart",
                "data": {}
            }),
        );
        assert_eq!(
            rt.get_agent(info.agent_id).unwrap().status,
            AgentStatus::Running
        );
    }

    #[test]
    fn stream_end_with_tool_calls_keeps_agent_running() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            450,
            vec!["/tmp".into()],
            "claude".into(),
            None,
            "tool-loop".into(),
        );

        rt.record_chat_event(
            450,
            &json!({
                "kind": "StreamStart",
                "data": {}
            }),
        );
        rt.record_chat_event(
            450,
            &json!({
                "kind": "StreamEnd",
                "data": {
                    "message": {
                        "content": "Using tools...",
                        "tool_calls": [{ "id": "toolu_1", "name": "Task", "arguments": {} }]
                    }
                }
            }),
        );

        let after_loop = rt.get_agent(info.agent_id).unwrap();
        assert_eq!(after_loop.status, AgentStatus::Running);
        assert!(after_loop.ended_at_ms.is_none());

        rt.record_chat_event(
            450,
            &json!({
                "kind": "StreamEnd",
                "data": {
                    "message": {
                        "content": "Final answer",
                        "tool_calls": []
                    }
                }
            }),
        );
        let completed = rt.get_agent(info.agent_id).unwrap();
        assert_eq!(completed.status, AgentStatus::Completed);
        assert!(completed.ended_at_ms.is_some());
    }

    #[test]
    fn collect_result_falls_back_to_summary_when_no_last_message() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            500,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        // Complete via SubprocessExit(0) which sets no last_message
        rt.record_chat_event(
            500,
            &json!({
                "kind": "SubprocessExit",
                "data": { "exit_code": 0 }
            }),
        );

        let result = rt.collect_result(info.agent_id).unwrap();
        assert_eq!(result.agent.status, AgentStatus::Completed);
        // Falls back to summary since last_message is None
        assert_eq!(result.final_message.as_deref(), Some("Completed"));
    }
}
