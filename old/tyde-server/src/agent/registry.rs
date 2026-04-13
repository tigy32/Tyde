use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use tyde_protocol::protocol::{
    ChatEvent, OperationCancelledData, StreamEndData, SubprocessExitData, ToolRequest,
};

use crate::{AgentId, ToolPolicy};

use super::{
    Agent, AgentEvent, AgentEventBatch, AgentHandle, AgentInfo, Backend, CollectedAgentResult,
};

const MAX_SUMMARY_LEN: usize = 180;
const MAX_MESSAGE_LEN: usize = 4_000;
const DEFAULT_EVENT_LOG_LIMIT: usize = 5_000;

pub struct AgentRegistry {
    agents: HashMap<AgentId, Agent>,
    next_event_seq: u64,
    events: VecDeque<AgentEvent>,
    event_log_limit: usize,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            next_event_seq: 1,
            events: VecDeque::new(),
            event_log_limit: DEFAULT_EVENT_LOG_LIMIT,
        }
    }

    // ── Lookups ──────────────────────────────────────────────────────

    pub fn has_agent(&self, agent_id: &str) -> bool {
        self.agents.contains_key(agent_id)
    }

    pub fn get_info(&self, agent_id: &str) -> Option<AgentInfo> {
        self.agents.get(agent_id).map(|a| a.info.clone())
    }

    pub fn agent_handle(&self, agent_id: &str) -> Option<AgentHandle> {
        self.agents.get(agent_id)?.agent_handle()
    }

    pub fn tracks_local_session_store(&self, agent_id: &str) -> bool {
        self.agents
            .get(agent_id)
            .map_or(false, |a| a.tracks_local_session_store())
    }

    pub fn backend(&self, agent_id: &str) -> Option<&dyn Backend> {
        self.agents.get(agent_id)?.backend()
    }

    pub fn list_agents(&self) -> Vec<AgentInfo> {
        let mut out: Vec<AgentInfo> = self.agents.values().map(|a| a.info.clone()).collect();
        out.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
        out
    }

    pub fn children_of(&self, agent_id: &str) -> Vec<AgentInfo> {
        let mut children: Vec<AgentInfo> = self
            .agents
            .values()
            .filter(|a| a.info.parent_agent_id.as_deref() == Some(agent_id))
            .map(|a| a.info.clone())
            .collect();
        children.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
        children
    }

    pub fn active_ids(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }

    pub fn workspace_roots(&self, agent_id: &str) -> Option<Vec<String>> {
        self.agents
            .get(agent_id)
            .map(|a| a.info.workspace_roots.clone())
    }

    pub fn backend_kind(&self, agent_id: &str) -> Option<String> {
        self.agents
            .get(agent_id)
            .map(|a| a.info.backend_kind.clone())
    }

    pub fn agent_summaries(&self) -> Vec<(String, String, Vec<String>)> {
        let mut ids: Vec<_> = self.agents.keys().cloned().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|agent_id| {
                let agent = self.agents.get(&agent_id)?;
                Some((
                    agent_id,
                    agent.info.backend_kind.clone(),
                    agent.info.workspace_roots.clone(),
                ))
            })
            .collect()
    }

    // ── Registration ────────────────────────────────────────────────

    pub fn reserve_agent_id(&self) -> AgentId {
        loop {
            let id = uuid::Uuid::new_v4().to_string();
            if !self.agents.contains_key(&id) {
                return id;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &mut self,
        backend: Box<dyn Backend>,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let agent_id = self.reserve_agent_id();
        self.register_with_id(
            agent_id,
            backend,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            ui_owner_project_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_with_id(
        &mut self,
        agent_id: AgentId,
        backend: Box<dyn Backend>,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let info = self.insert(
            agent_id,
            Some(backend),
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            ui_owner_project_id,
        );
        self.push_event(
            info.agent_id.clone(),
            "agent_spawned",
            true,
            Some("Running...".to_string()),
        );
        info
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_metadata(
        &mut self,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let agent_id = self.reserve_agent_id();
        self.register_metadata_with_id(
            agent_id,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            ui_owner_project_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_metadata_with_id(
        &mut self,
        agent_id: AgentId,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let info = self.insert(
            agent_id,
            None,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            ui_owner_project_id,
        );
        self.push_event(
            info.agent_id.clone(),
            "agent_spawned",
            true,
            Some("Running...".to_string()),
        );
        info
    }

    pub fn remove(&mut self, agent_id: &str) -> Option<Box<dyn Backend>> {
        self.agents.remove(agent_id)?.take_backend()
    }

    pub fn drain_all(&mut self) -> Vec<Box<dyn Backend>> {
        self.agents
            .drain()
            .filter_map(|(_, mut agent)| agent.take_backend())
            .collect()
    }

    // ── Mutation ─────────────────────────────────────────────────────

    pub fn rename_agent(&mut self, agent_id: &str, name: String) -> bool {
        let (is_running, message) = {
            let Some(agent) = self.agents.get_mut(agent_id) else {
                return false;
            };
            if agent.info.name == name {
                return false;
            }
            agent.info.name = name.clone();
            agent.info.updated_at_ms = now_ms();
            (agent.info.is_running, Some(name))
        };
        self.push_event(agent_id.to_string(), "agent_renamed", is_running, message);
        true
    }

    pub fn set_agent_definition(
        &mut self,
        agent_id: &str,
        definition_id: Option<String>,
        tool_policy: ToolPolicy,
    ) {
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.info.agent_definition_id = definition_id;
            agent.info.tool_policy = tool_policy;
        }
    }

    pub fn update_agent_type(&mut self, agent_id: &str, agent_type: Option<String>) {
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.info.agent_type = agent_type;
        }
    }

    pub fn mark_agent_running(&mut self, agent_id: &str, summary: Option<String>) -> bool {
        self.update_agent(agent_id, Some(true), summary, None, "agent_running", None)
    }

    pub fn mark_agent_failed(&mut self, agent_id: &str, message: String) -> bool {
        self.update_agent(
            agent_id,
            Some(false),
            Some(message.clone()),
            Some(message),
            "agent_failed",
            None,
        )
    }

    pub fn mark_agent_closed(&mut self, agent_id: &str, message: Option<String>) -> bool {
        self.update_agent(
            agent_id,
            Some(false),
            Some(message.unwrap_or_else(|| "Agent closed".to_string())),
            None,
            "agent_closed",
            None,
        )
    }

    pub fn configure_agent_definition(
        &mut self,
        agent_id: &str,
        agent_type: Option<String>,
        definition_id: Option<String>,
        tool_policy: ToolPolicy,
    ) -> Option<AgentInfo> {
        self.get_info(agent_id)?;
        self.update_agent_type(agent_id, agent_type);
        self.set_agent_definition(agent_id, definition_id, tool_policy);
        self.get_info(agent_id)
    }

    // ── Chat event processing ───────────────────────────────────────

    pub fn record_chat_event(&mut self, agent_id: &str, event: &ChatEvent) -> bool {
        if !self.agents.contains_key(agent_id) {
            return false;
        }

        match event {
            ChatEvent::TypingStatusChanged(typing) => {
                let summary = if *typing {
                    None
                } else {
                    Some("Completed".to_string())
                };
                self.update_agent(
                    agent_id,
                    Some(*typing),
                    summary,
                    None,
                    if *typing {
                        "typing_started"
                    } else {
                        "typing_stopped"
                    },
                    None,
                )
            }
            ChatEvent::StreamEnd(StreamEndData { message }) => {
                let normalized = normalize_message(&message.content);
                let summary =
                    (!normalized.is_empty()).then(|| summarize_text(&normalized, MAX_SUMMARY_LEN));
                let last_message = (!normalized.is_empty()).then_some(normalized);
                self.update_agent(agent_id, None, summary, None, "stream_end", last_message)
            }
            ChatEvent::Error(error) => {
                let error = error.trim();
                let error = if error.is_empty() {
                    "Agent failed".to_string()
                } else {
                    error.to_string()
                };
                self.update_agent(
                    agent_id,
                    None,
                    Some(error.clone()),
                    Some(error),
                    "error",
                    None,
                )
            }
            ChatEvent::ToolRequest(ToolRequest { tool_name, .. }) => {
                let tool_name = tool_name.trim();
                let tool_name = if tool_name.is_empty() {
                    "tool"
                } else {
                    tool_name
                };
                self.update_agent(
                    agent_id,
                    None,
                    Some(format!("Using {tool_name}...")),
                    None,
                    "tool_request",
                    None,
                )
            }
            ChatEvent::OperationCancelled(OperationCancelledData { message }) => {
                let message = message.trim();
                let message = if message.is_empty() {
                    "Operation cancelled".to_string()
                } else {
                    message.to_string()
                };
                self.update_agent(
                    agent_id,
                    None,
                    Some(message),
                    None,
                    "operation_cancelled",
                    None,
                )
            }
            ChatEvent::SubprocessExit(SubprocessExitData { exit_code }) => {
                if *exit_code == Some(0) {
                    self.update_agent(
                        agent_id,
                        Some(false),
                        Some("Completed".to_string()),
                        None,
                        "subprocess_exit_ok",
                        None,
                    )
                } else {
                    let message = match exit_code {
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
            ChatEvent::StreamStart(_) => {
                self.update_agent(agent_id, None, None, None, "stream_start", None)
            }
            _ => false,
        }
    }

    // ── Event log ───────────────────────────────────────────────────

    pub fn events_since(&self, since_seq: u64, limit: usize) -> AgentEventBatch {
        let cap = limit.clamp(1, 1000);
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.seq > since_seq)
            .take(cap)
            .cloned()
            .collect();
        AgentEventBatch {
            events,
            latest_seq: self.next_event_seq.saturating_sub(1),
        }
    }

    pub fn latest_event_seq_for_agent(&self, agent_id: &str) -> Option<u64> {
        self.events
            .iter()
            .rev()
            .find(|e| e.agent_id == agent_id)
            .map(|e| e.seq)
    }

    pub fn collect_result(&self, agent_id: &str) -> Result<CollectedAgentResult, String> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| format!("Agent {agent_id} not found"))?;
        if agent.info.is_running {
            return Err(format!(
                "Agent {agent_id} is still running; collect_result requires a stopped agent"
            ));
        }
        let final_message = agent.info.last_message.clone().or_else(|| {
            let s = agent.info.summary.trim();
            (!s.is_empty()).then(|| s.to_string())
        });
        Ok(CollectedAgentResult {
            agent: agent.info.clone(),
            final_message,
            changed_files: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    // ── Internal ────────────────────────────────────────────────────

    fn insert(
        &mut self,
        agent_id: AgentId,
        backend: Option<Box<dyn Backend>>,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let now = now_ms();
        let explicit_owner = ui_owner_project_id
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let inherited_owner = parent_agent_id
            .as_ref()
            .and_then(|pid| self.agents.get(pid))
            .and_then(|parent| parent.info.ui_owner_project_id.clone());

        let info = AgentInfo {
            agent_id: agent_id.clone(),
            ui_owner_project_id: explicit_owner.or(inherited_owner),
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            agent_type: None,
            agent_definition_id: None,
            tool_policy: ToolPolicy::Unrestricted,
            is_running: true,
            summary: "Running...".to_string(),
            created_at_ms: now,
            updated_at_ms: now,
            ended_at_ms: None,
            last_error: None,
            last_message: None,
        };

        self.agents.insert(
            agent_id,
            Agent {
                info: info.clone(),
                backend,
            },
        );
        info
    }

    fn update_agent(
        &mut self,
        agent_id: &str,
        is_running: Option<bool>,
        summary: Option<String>,
        last_error: Option<String>,
        event_kind: &str,
        last_message: Option<String>,
    ) -> bool {
        let mut changed = false;
        let now = now_ms();

        let (running, event_message) = {
            let Some(agent) = self.agents.get_mut(agent_id) else {
                return false;
            };
            let info = &mut agent.info;

            if let Some(running) = is_running {
                if info.is_running != running {
                    info.is_running = running;
                    changed = true;
                }
                if !running {
                    if info.ended_at_ms.is_none() {
                        changed = true;
                    }
                    info.ended_at_ms = Some(now);
                } else if info.ended_at_ms.is_some() {
                    changed = true;
                    info.ended_at_ms = None;
                }
            }

            info.updated_at_ms = now;

            if let Some(value) = summary {
                let normalized = summarize_text(&value, MAX_SUMMARY_LEN);
                if !normalized.is_empty() && info.summary != normalized {
                    info.summary = normalized;
                    changed = true;
                }
            }

            if let Some(err) = last_error {
                let normalized = summarize_text(&err, MAX_MESSAGE_LEN);
                if info.last_error.as_deref() != Some(normalized.as_str()) {
                    info.last_error = Some(normalized);
                    changed = true;
                }
            }

            if let Some(msg) = last_message {
                let normalized = normalize_message(&msg);
                let next = (!normalized.is_empty()).then_some(normalized);
                if info.last_message != next {
                    info.last_message = next;
                    changed = true;
                }
            }

            let msg = (!info.summary.is_empty()).then(|| info.summary.clone());
            (info.is_running, msg)
        };

        self.push_event(agent_id.to_string(), event_kind, running, event_message);
        changed
    }

    fn push_event(
        &mut self,
        agent_id: AgentId,
        kind: &str,
        is_running: bool,
        message: Option<String>,
    ) {
        self.events.push_back(AgentEvent {
            seq: self.next_event_seq,
            agent_id,
            kind: kind.to_string(),
            is_running,
            timestamp_ms: now_ms(),
            message,
        });
        self.next_event_seq += 1;
        while self.events.len() > self.event_log_limit {
            self.events.pop_front();
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
    let mut truncated: String = compact.chars().take(keep).collect();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use tyde_protocol::protocol::{
        ChatEvent, ChatMessage, MessageSender, StreamEndData, SubprocessExitData,
    };

    use super::*;

    fn make_registry_with_stopped_agent() -> (AgentRegistry, AgentId) {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
            None,
        );
        let id = info.agent_id.clone();

        reg.record_chat_event(&id, &ChatEvent::TypingStatusChanged(true));
        reg.record_chat_event(
            &id,
            &ChatEvent::StreamEnd(StreamEndData {
                message: ChatMessage {
                    timestamp: 0,
                    sender: MessageSender::Assistant {
                        agent: "test".into(),
                    },
                    content: "Done with the task".into(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                },
            }),
        );
        reg.record_chat_event(&id, &ChatEvent::TypingStatusChanged(false));

        assert!(!reg.get_info(&id).unwrap().is_running);
        (reg, id)
    }

    #[test]
    fn typing_status_controls_is_running() {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
            None,
        );
        assert!(reg.get_info(&info.agent_id).unwrap().is_running);

        reg.record_chat_event(&info.agent_id, &ChatEvent::TypingStatusChanged(true));
        assert!(reg.get_info(&info.agent_id).unwrap().is_running);

        reg.record_chat_event(&info.agent_id, &ChatEvent::TypingStatusChanged(false));
        let agent = reg.get_info(&info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn collect_result_succeeds_for_stopped_agent() {
        let (reg, id) = make_registry_with_stopped_agent();
        let collected = reg.collect_result(&id).unwrap();
        assert!(!collected.agent.is_running);
        assert_eq!(
            collected.final_message.as_deref(),
            Some("Done with the task")
        );
    }

    #[test]
    fn collect_result_errors_for_running_agent() {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
            None,
        );
        assert!(reg
            .collect_result(&info.agent_id)
            .unwrap_err()
            .contains("still running"));
    }

    #[test]
    fn collect_result_errors_for_unknown_agent() {
        let reg = AgentRegistry::new();
        assert!(reg.collect_result("999").unwrap_err().contains("not found"));
    }

    #[test]
    fn subprocess_exit_stops_agent() {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
            None,
        );

        reg.record_chat_event(&info.agent_id, &ChatEvent::TypingStatusChanged(true));
        reg.record_chat_event(
            &info.agent_id,
            &ChatEvent::SubprocessExit(SubprocessExitData { exit_code: Some(0) }),
        );

        let agent = reg.get_info(&info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn mark_agent_closed_stops_agent() {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "test".into(),
            None,
        );
        assert!(reg.mark_agent_closed(&info.agent_id, Some("Terminated".to_string())));

        let agent = reg.get_info(&info.agent_id).unwrap();
        assert!(!agent.is_running);
        assert!(agent.ended_at_ms.is_some());
    }

    #[test]
    fn child_agent_inherits_ui_owner_from_parent() {
        let mut reg = AgentRegistry::new();
        let parent = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            None,
            "parent".into(),
            Some("project-a".into()),
        );
        let child = reg.register_metadata(
            vec!["/tmp".into()],
            "tycode".into(),
            Some(parent.agent_id.clone()),
            "child".into(),
            None,
        );

        assert_eq!(
            reg.get_info(&child.agent_id)
                .and_then(|a| a.ui_owner_project_id),
            Some("project-a".to_string()),
        );
    }

    #[test]
    fn register_and_remove() {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp".into()],
            "mock".into(),
            None,
            "test".into(),
            None,
        );
        assert!(reg.has_agent(&info.agent_id));

        // Metadata-only agents have no backend to remove
        assert!(reg.remove(&info.agent_id).is_none());
        assert!(!reg.has_agent(&info.agent_id));
    }

    #[test]
    fn workspace_roots_and_backend_kind() {
        let mut reg = AgentRegistry::new();
        let info = reg.register_metadata(
            vec!["/tmp/project".into()],
            "codex".into(),
            None,
            "test".into(),
            None,
        );

        assert_eq!(
            reg.workspace_roots(&info.agent_id),
            Some(vec!["/tmp/project".to_string()])
        );
        assert_eq!(reg.backend_kind(&info.agent_id), Some("codex".to_string()));
    }
}
