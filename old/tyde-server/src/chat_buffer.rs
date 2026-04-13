use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tyde_protocol::protocol::ChatEvent;

const DEFAULT_LIMIT_PER_AGENT: usize = 5_000;

#[derive(Debug, Clone, Serialize)]
pub struct ChatEventEntry {
    pub seq: u64,
    pub agent_id: String,
    pub event: ChatEvent,
    pub timestamp_ms: u64,
}

pub struct ChatEventBuffer {
    next_seq: u64,
    agents: HashMap<String, VecDeque<ChatEventEntry>>,
    limit_per_agent: usize,
}

impl Default for ChatEventBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatEventBuffer {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            agents: HashMap::new(),
            limit_per_agent: DEFAULT_LIMIT_PER_AGENT,
        }
    }

    pub fn push(&mut self, agent_id: String, event: ChatEvent) -> ChatEventEntry {
        let seq = self.next_seq;
        self.next_seq += 1;

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let entry = ChatEventEntry {
            seq,
            agent_id: agent_id.clone(),
            event,
            timestamp_ms,
        };

        let log = self.agents.entry(agent_id.clone()).or_default();
        if log.len() >= self.limit_per_agent {
            log.pop_front();
        }
        log.push_back(entry.clone());

        entry
    }

    /// Returns events for a single agent where `entry.seq > since_seq`,
    /// up to `limit` entries.
    #[cfg(test)]
    pub fn events_since(
        &self,
        agent_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> Vec<&ChatEventEntry> {
        let Some(log) = self.agents.get(agent_id) else {
            return Vec::new();
        };
        log.iter()
            .filter(|e| e.seq > since_seq)
            .take(limit)
            .collect()
    }

    /// Returns missed events across all agents given per-agent
    /// last-seen sequence numbers. Agents not in `since_seqs` return
    /// all buffered events (they are new to the client).
    pub fn all_events_since(&self, since_seqs: &HashMap<String, u64>) -> Vec<&ChatEventEntry> {
        let mut result = Vec::new();
        for (agent_id, log) in &self.agents {
            let since = since_seqs.get(agent_id).copied().unwrap_or(0);
            for entry in log.iter() {
                if entry.seq > since {
                    result.push(entry);
                }
            }
        }
        result.sort_by_key(|e| e.seq);
        result
    }

    pub fn remove_agent(&mut self, agent_id: &str) {
        self.agents.remove(agent_id);
    }

    #[cfg(test)]
    pub fn latest_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub fn latest_seq_for_agent(&self, agent_id: &str) -> u64 {
        self.agents
            .get(agent_id)
            .and_then(|log| log.back())
            .map(|e| e.seq)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tyde_protocol::protocol::{
        ChatEvent, ChatMessage, MessageSender, StreamEndData, StreamStartData,
    };

    fn stream_start(agent: &str) -> ChatEvent {
        ChatEvent::StreamStart(StreamStartData {
            message_id: None,
            agent: agent.to_string(),
            model: None,
        })
    }

    fn stream_end(content: &str) -> ChatEvent {
        ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                timestamp: 0,
                sender: MessageSender::Assistant {
                    agent: "test".to_string(),
                },
                content: content.to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        })
    }

    #[test]
    fn push_and_retrieve() {
        let mut buf = ChatEventBuffer::new();
        let e1 = buf.push("1".to_string(), stream_start("agent-1"));
        let e2 = buf.push("1".to_string(), stream_end("done"));
        let e3 = buf.push("2".to_string(), stream_start("agent-2"));

        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(e3.seq, 3);

        let events = buf.events_since("1", 0, 100);
        assert_eq!(events.len(), 2);

        let events = buf.events_since("1", 1, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 2);

        let events = buf.events_since("2", 0, 100);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn all_events_since_replays_correctly() {
        let mut buf = ChatEventBuffer::new();
        buf.push("1".to_string(), ChatEvent::TypingStatusChanged(true));
        buf.push("2".to_string(), ChatEvent::TypingStatusChanged(false));
        buf.push("1".to_string(), ChatEvent::Error("boom".to_string()));

        let mut seqs = HashMap::new();
        seqs.insert("1".to_string(), 1u64); // saw seq 1 for agent 1
                                            // agent 2 not in map → replay everything

        let events = buf.all_events_since(&seqs);
        assert_eq!(events.len(), 2); // seq 2 (agent 2) + seq 3 (agent 1)
        assert_eq!(events[0].seq, 2);
        assert_eq!(events[1].seq, 3);
    }

    #[test]
    fn respects_limit() {
        let mut buf = ChatEventBuffer {
            next_seq: 1,
            agents: HashMap::new(),
            limit_per_agent: 3,
        };
        buf.push("1".to_string(), ChatEvent::TypingStatusChanged(true));
        buf.push("1".to_string(), ChatEvent::TypingStatusChanged(false));
        buf.push("1".to_string(), ChatEvent::Error("a".to_string()));
        buf.push("1".to_string(), ChatEvent::Error("b".to_string())); // evicts seq 1

        let events = buf.events_since("1", 0, 100);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 2);
    }

    #[test]
    fn remove_agent_clears() {
        let mut buf = ChatEventBuffer::new();
        buf.push("1".to_string(), ChatEvent::TypingStatusChanged(true));
        buf.remove_agent("1");
        assert_eq!(buf.events_since("1", 0, 100).len(), 0);
    }

    #[test]
    fn latest_seq_tracking() {
        let mut buf = ChatEventBuffer::new();
        assert_eq!(buf.latest_seq(), 0);
        buf.push("1".to_string(), ChatEvent::TypingStatusChanged(true));
        assert_eq!(buf.latest_seq(), 1);
        buf.push("2".to_string(), ChatEvent::TypingStatusChanged(false));
        assert_eq!(buf.latest_seq(), 2);
        assert_eq!(buf.latest_seq_for_agent("1"), 1);
        assert_eq!(buf.latest_seq_for_agent("2"), 2);
        assert_eq!(buf.latest_seq_for_agent("999"), 0);
    }
}
