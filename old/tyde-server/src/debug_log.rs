use std::collections::VecDeque;

use serde::Serialize;
use serde_json::Value;

const DEFAULT_DEBUG_EVENT_LOG_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct DebugEventEntry {
    pub seq: u64,
    pub stream: String,
    pub timestamp_ms: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DebugEventBatch {
    pub events: Vec<DebugEventEntry>,
    pub latest_seq: u64,
}

#[derive(Debug)]
pub struct DebugEventLog {
    next_seq: u64,
    events: VecDeque<DebugEventEntry>,
    limit: usize,
}

impl DebugEventLog {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            events: VecDeque::new(),
            limit: DEFAULT_DEBUG_EVENT_LOG_LIMIT,
        }
    }

    pub fn push(&mut self, stream: &str, payload: Value, timestamp_ms: u64) {
        let event = DebugEventEntry {
            seq: self.next_seq,
            stream: stream.to_string(),
            timestamp_ms,
            payload,
        };
        self.next_seq += 1;
        self.events.push_back(event);
        while self.events.len() > self.limit {
            let _ = self.events.pop_front();
        }
    }

    pub fn events_since(
        &self,
        since_seq: u64,
        limit: usize,
        stream: Option<&str>,
    ) -> DebugEventBatch {
        let normalized_stream = stream.map(str::trim).filter(|raw| !raw.is_empty());
        let events = self
            .events
            .iter()
            .filter(|event| event.seq > since_seq)
            .filter(|event| {
                normalized_stream
                    .map(|value| event.stream == value)
                    .unwrap_or(true)
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        DebugEventBatch {
            events,
            latest_seq: self.next_seq.saturating_sub(1),
        }
    }
}

impl Default for DebugEventLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::DebugEventLog;
    use serde_json::json;

    #[test]
    fn filters_events_by_stream() {
        let mut log = DebugEventLog::new();
        log.push("chat", json!({"a": 1}), 100);
        log.push("debug", json!({"b": 2}), 200);

        let batch = log.events_since(0, 10, Some("chat"));
        assert_eq!(batch.latest_seq, 2);
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].stream, "chat");
    }
}
