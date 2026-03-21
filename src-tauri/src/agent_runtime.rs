use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

const MAX_SUMMARY_LEN: usize = 180;
const MAX_MESSAGE_LEN: usize = 4_000;
const DEFAULT_EVENT_LOG_LIMIT: usize = 5_000;

#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub agent_id: u64,
    pub conversation_id: u64,
    pub workspace_roots: Vec<String>,
    pub backend_kind: String,
    pub parent_agent_id: Option<u64>,
    pub name: String,
    pub agent_type: Option<String>,
    pub is_running: bool,
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
    pub is_running: bool,
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

    pub fn get_agent_by_conversation(&self, conversation_id: u64) -> Option<AgentInfo> {
        let agent_id = self.conversation_to_agent.get(&conversation_id).copied()?;
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
            is_running: true,
            summary: "Running...".to_string(),
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
            true,
            Some("Running...".to_string()),
        );
        info
    }

    pub fn rename_agent(&mut self, agent_id: u64, name: String) -> bool {
        let Some(info) = self.agents.get_mut(&agent_id) else {
            return false;
        };
        if info.name == name {
            return false;
        }
        info.name = name;
        info.updated_at_ms = now_ms();
        true
    }

    pub fn update_agent_type(&mut self, agent_id: u64, agent_type: Option<String>) {
        if let Some(info) = self.agents.get_mut(&agent_id) {
            info.agent_type = agent_type;
        }
    }

    pub fn mark_agent_running(&mut self, agent_id: u64, summary: Option<String>) -> bool {
        self.update_agent(agent_id, Some(true), summary, None, "agent_running", None)
    }

    pub fn mark_conversation_failed(&mut self, conversation_id: u64, message: String) -> bool {
        let agent_id = match self.conversation_to_agent.get(&conversation_id).copied() {
            Some(id) => id,
            None => return false,
        };
        self.update_agent(
            agent_id,
            Some(false),
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
        self.update_agent(
            agent_id,
            Some(false),
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
        let agent_id = match self.conversation_to_agent.get(&conversation_id).copied() {
            Some(id) => id,
            None => return false,
        };

        match kind {
            "TypingStatusChanged" => {
                let typing = event.get("data").and_then(Value::as_bool).unwrap_or(false);
                // When typing starts, keep the existing summary (e.g. "Using Read...")
                // so the home view preserves richer context between tool calls.
                // Only set "Completed" on typing=false.
                let summary = if typing {
                    None
                } else {
                    Some("Completed".to_string())
                };
                self.update_agent(
                    agent_id,
                    Some(typing),
                    summary,
                    None,
                    if typing {
                        "typing_started"
                    } else {
                        "typing_stopped"
                    },
                    None,
                )
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
                    None
                } else {
                    Some(summarize_text(&normalized_message, MAX_SUMMARY_LEN))
                };
                let last_message = if normalized_message.is_empty() {
                    None
                } else {
                    Some(normalized_message)
                };
                self.update_agent(
                    agent_id,
                    None, // don't touch is_running — TypingStatusChanged is authoritative
                    summary,
                    None,
                    "stream_end",
                    last_message,
                )
            }
            "Error" => {
                let error = event
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("Agent failed")
                    .to_string();
                self.update_agent(
                    agent_id,
                    None,
                    Some(error.clone()),
                    Some(error),
                    "error",
                    None,
                )
            }
            "ToolRequest" => {
                let tool_name = event
                    .get("data")
                    .and_then(|d| d.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                self.update_agent(
                    agent_id,
                    None,
                    Some(format!("Using {tool_name}...")),
                    None,
                    "tool_request",
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
                self.update_agent(
                    agent_id,
                    None,
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
                    self.update_agent(
                        agent_id,
                        Some(false),
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
                    self.update_agent(
                        agent_id,
                        Some(false),
                        Some(message.clone()),
                        Some(message),
                        "subprocess_exit_error",
                        None,
                    )
                }
            }
            "StreamStart" => self.update_agent(
                agent_id,
                None,
                None, // keep existing summary — ToolRequest/StreamEnd provide richer context
                None,
                "stream_start",
                None,
            ),
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

        if agent.is_running {
            return Err(format!(
                "Agent {agent_id} is still running; collect_result requires a stopped agent",
            ));
        }

        let final_message = agent.last_message.clone().or_else(|| {
            if !agent.summary.trim().is_empty() {
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

    fn update_agent(
        &mut self,
        agent_id: u64,
        is_running: Option<bool>,
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

            if let Some(running) = is_running {
                if agent.is_running != running {
                    agent.is_running = running;
                    changed = true;
                }
                if !running {
                    if agent.ended_at_ms.is_none() {
                        changed = true;
                    }
                    agent.ended_at_ms = Some(now);
                } else if agent.ended_at_ms.is_some() {
                    changed = true;
                    agent.ended_at_ms = None;
                }
            }

            agent.updated_at_ms = now;

            if let Some(summary_value) = summary {
                let normalized = summarize_text(&summary_value, MAX_SUMMARY_LEN);
                if !normalized.is_empty() && agent.summary != normalized {
                    agent.summary = normalized;
                    changed = true;
                }
            }

            if let Some(err) = last_error {
                let normalized = summarize_text(&err, MAX_MESSAGE_LEN);
                if agent.last_error.as_deref() != Some(normalized.as_str()) {
                    agent.last_error = Some(normalized);
                    changed = true;
                }
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
                agent.is_running,
                if agent.summary.is_empty() {
                    None
                } else {
                    Some(agent.summary.clone())
                },
            )
        };

        let (conversation_id, running, message) = event_payload;
        self.push_event(agent_id, conversation_id, event_kind, running, message);

        changed
    }

    fn push_event(
        &mut self,
        agent_id: u64,
        conversation_id: u64,
        kind: &str,
        is_running: bool,
        message: Option<String>,
    ) {
        let event = AgentEvent {
            seq: self.next_event_seq,
            agent_id,
            conversation_id,
            kind: kind.to_string(),
            is_running,
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

    fn make_runtime_with_stopped_agent() -> (AgentRuntime, u64) {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            100,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );
        let agent_id = info.agent_id;

        // Start typing, then stop
        rt.record_chat_event(100, &json!({ "kind": "TypingStatusChanged", "data": true }));
        rt.record_chat_event(
            100,
            &json!({ "kind": "StreamEnd", "data": { "message": { "content": "Done with the task" } } }),
        );
        rt.record_chat_event(
            100,
            &json!({ "kind": "TypingStatusChanged", "data": false }),
        );

        assert!(!rt.get_agent(agent_id).unwrap().is_running);
        (rt, agent_id)
    }

    #[test]
    fn typing_status_controls_is_running() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            100,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );
        assert!(rt.get_agent(info.agent_id).unwrap().is_running);

        rt.record_chat_event(100, &json!({ "kind": "TypingStatusChanged", "data": true }));
        assert!(rt.get_agent(info.agent_id).unwrap().is_running);

        rt.record_chat_event(
            100,
            &json!({ "kind": "TypingStatusChanged", "data": false }),
        );
        let agent = rt.get_agent(info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn stream_end_updates_summary_and_message_not_is_running() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            100,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        rt.record_chat_event(100, &json!({ "kind": "TypingStatusChanged", "data": true }));
        rt.record_chat_event(
            100,
            &json!({ "kind": "StreamEnd", "data": { "message": { "content": "Done with the task" } } }),
        );

        let agent = rt.get_agent(info.agent_id).unwrap();
        // StreamEnd should NOT stop the agent — only TypingStatusChanged does that
        assert!(agent.is_running);
        assert_eq!(agent.last_message.as_deref(), Some("Done with the task"));
    }

    #[test]
    fn collect_result_succeeds_for_stopped_agent() {
        let (rt, agent_id) = make_runtime_with_stopped_agent();

        let result = rt.collect_result(agent_id);
        assert!(result.is_ok());
        let collected = result.unwrap();
        assert!(!collected.agent.is_running);
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
        let result = rt.collect_result(info.agent_id);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("still running"));
    }

    #[test]
    fn collect_result_errors_for_unknown_agent() {
        let rt = AgentRuntime::new();
        let result = rt.collect_result(999);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn error_event_sets_last_error() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            300,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        rt.record_chat_event(300, &json!({ "kind": "TypingStatusChanged", "data": true }));
        rt.record_chat_event(
            300,
            &json!({ "kind": "Error", "data": "Something went wrong" }),
        );

        let agent = rt.get_agent(info.agent_id).unwrap();
        assert!(agent.last_error.is_some());
        // Error doesn't change is_running — TypingStatusChanged does
        assert!(agent.is_running);
    }

    #[test]
    fn subprocess_exit_stops_agent() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            400,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        rt.record_chat_event(400, &json!({ "kind": "TypingStatusChanged", "data": true }));
        rt.record_chat_event(
            400,
            &json!({ "kind": "SubprocessExit", "data": { "exit_code": 0 } }),
        );

        let agent = rt.get_agent(info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn subprocess_exit_error_sets_last_error() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            500,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        rt.record_chat_event(500, &json!({ "kind": "TypingStatusChanged", "data": true }));
        rt.record_chat_event(
            500,
            &json!({ "kind": "SubprocessExit", "data": { "exit_code": 1 } }),
        );

        let agent = rt.get_agent(info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.last_error.is_some());
    }

    #[test]
    fn collect_result_falls_back_to_summary_when_no_last_message() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            600,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );

        rt.record_chat_event(
            600,
            &json!({ "kind": "SubprocessExit", "data": { "exit_code": 0 } }),
        );

        let result = rt.collect_result(info.agent_id).unwrap();
        assert_eq!(result.final_message.as_deref(), Some("Completed"));
    }

    #[test]
    fn mark_conversation_closed_stops_agent() {
        let mut rt = AgentRuntime::new();
        let info = rt.register_agent(
            700,
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
        );
        let changed = rt.mark_conversation_closed(700, Some("Terminated".to_string()));
        assert!(changed);

        let agent = rt.get_agent(info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.ended_at_ms.is_some());
    }
}
