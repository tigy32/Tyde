use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

const DEFAULT_LIMIT_PER_CONVERSATION: usize = 5_000;

#[derive(Debug, Clone, Serialize)]
pub struct ChatEventEntry {
    pub seq: u64,
    pub conversation_id: u64,
    pub event: Value,
    pub timestamp_ms: u64,
}

pub struct ChatEventBuffer {
    next_seq: u64,
    conversations: HashMap<u64, VecDeque<ChatEventEntry>>,
    limit_per_conversation: usize,
}

impl ChatEventBuffer {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            conversations: HashMap::new(),
            limit_per_conversation: DEFAULT_LIMIT_PER_CONVERSATION,
        }
    }

    pub fn push(&mut self, conversation_id: u64, event: Value) -> ChatEventEntry {
        let seq = self.next_seq;
        self.next_seq += 1;

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let entry = ChatEventEntry {
            seq,
            conversation_id,
            event,
            timestamp_ms,
        };

        let log = self.conversations.entry(conversation_id).or_default();
        if log.len() >= self.limit_per_conversation {
            log.pop_front();
        }
        log.push_back(entry.clone());

        entry
    }

    /// Returns events for a single conversation where `entry.seq > since_seq`,
    /// up to `limit` entries.
    #[cfg(test)]
    pub fn events_since(
        &self,
        conversation_id: u64,
        since_seq: u64,
        limit: usize,
    ) -> Vec<&ChatEventEntry> {
        let Some(log) = self.conversations.get(&conversation_id) else {
            return Vec::new();
        };
        log.iter()
            .filter(|e| e.seq > since_seq)
            .take(limit)
            .collect()
    }

    /// Returns missed events across all conversations given per-conversation
    /// last-seen sequence numbers. Conversations not in `since_seqs` return
    /// all buffered events (they are new to the client).
    pub fn all_events_since(&self, since_seqs: &HashMap<u64, u64>) -> Vec<&ChatEventEntry> {
        let mut result = Vec::new();
        for (conv_id, log) in &self.conversations {
            let since = since_seqs.get(conv_id).copied().unwrap_or(0);
            for entry in log.iter() {
                if entry.seq > since {
                    result.push(entry);
                }
            }
        }
        result.sort_by_key(|e| e.seq);
        result
    }

    pub fn remove_conversation(&mut self, conversation_id: u64) {
        self.conversations.remove(&conversation_id);
    }

    #[cfg(test)]
    pub fn latest_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub fn latest_seq_for_conversation(&self, conversation_id: u64) -> u64 {
        self.conversations
            .get(&conversation_id)
            .and_then(|log| log.back())
            .map(|e| e.seq)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn push_and_retrieve() {
        let mut buf = ChatEventBuffer::new();
        let e1 = buf.push(1, json!({"kind": "StreamStart"}));
        let e2 = buf.push(1, json!({"kind": "StreamEnd"}));
        let e3 = buf.push(2, json!({"kind": "StreamStart"}));

        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(e3.seq, 3);

        let events = buf.events_since(1, 0, 100);
        assert_eq!(events.len(), 2);

        let events = buf.events_since(1, 1, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 2);

        let events = buf.events_since(2, 0, 100);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn all_events_since_replays_correctly() {
        let mut buf = ChatEventBuffer::new();
        buf.push(1, json!({"kind": "A"}));
        buf.push(2, json!({"kind": "B"}));
        buf.push(1, json!({"kind": "C"}));

        let mut seqs = HashMap::new();
        seqs.insert(1u64, 1u64); // saw seq 1 for conv 1
                                 // conv 2 not in map → replay everything

        let events = buf.all_events_since(&seqs);
        assert_eq!(events.len(), 2); // seq 2 (conv 2) + seq 3 (conv 1)
        assert_eq!(events[0].seq, 2);
        assert_eq!(events[1].seq, 3);
    }

    #[test]
    fn respects_limit() {
        let mut buf = ChatEventBuffer {
            next_seq: 1,
            conversations: HashMap::new(),
            limit_per_conversation: 3,
        };
        buf.push(1, json!(1));
        buf.push(1, json!(2));
        buf.push(1, json!(3));
        buf.push(1, json!(4)); // evicts seq 1

        let events = buf.events_since(1, 0, 100);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 2);
    }

    #[test]
    fn remove_conversation_clears() {
        let mut buf = ChatEventBuffer::new();
        buf.push(1, json!("a"));
        buf.remove_conversation(1);
        assert_eq!(buf.events_since(1, 0, 100).len(), 0);
    }

    #[test]
    fn latest_seq_tracking() {
        let mut buf = ChatEventBuffer::new();
        assert_eq!(buf.latest_seq(), 0);
        buf.push(1, json!("a"));
        assert_eq!(buf.latest_seq(), 1);
        buf.push(2, json!("b"));
        assert_eq!(buf.latest_seq(), 2);
        assert_eq!(buf.latest_seq_for_conversation(1), 1);
        assert_eq!(buf.latest_seq_for_conversation(2), 2);
        assert_eq!(buf.latest_seq_for_conversation(999), 0);
    }
}
