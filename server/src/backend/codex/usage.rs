use std::collections::HashMap;

use serde::Deserialize;

use super::CodexTurnTokenUsage;
use protocol::{
    CurrentContextUsage, ModelRequestId, ModelRequestTokenUsage, ModelTurnId, TokenUsage,
};

#[derive(Default)]
pub(super) struct OwnRequestUsage {
    offset: usize,
    event_owner: Option<String>,
    responses: HashMap<String, NativeRequest>,
    pub turns: HashMap<String, CodexTurnTokenUsage>,
    cumulative: TokenUsage,
    latest: Option<ModelRequestTokenUsage>,
}

#[derive(Deserialize, PartialEq, Eq)]
struct NativeRequest {
    thread_id: String,
    turn_id: String,
    response_id: String,
    usage: NativeUsage,
}

#[derive(Deserialize, PartialEq, Eq)]
struct NativeUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    reasoning_output_tokens: Option<u64>,
}

impl NativeUsage {
    fn normalized(&self) -> Result<TokenUsage, String> {
        let cached = self.cached_input_tokens;
        let created = self.cache_write_input_tokens;
        let input_tokens = self
            .input_tokens
            .checked_sub(cached)
            .and_then(|input| input.checked_sub(created))
            .ok_or("Codex request cache counters exceed its prompt usage")?;
        if self
            .reasoning_output_tokens
            .is_some_and(|reasoning| reasoning > self.output_tokens)
        {
            return Err("Codex request reasoning exceeds its output usage".to_owned());
        }
        if self.input_tokens.checked_add(self.output_tokens) != Some(self.total_tokens) {
            return Err(
                "Codex request total disagrees with its prompt and output usage".to_owned(),
            );
        }
        Ok(TokenUsage {
            input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cached_prompt_tokens: Some(self.cached_input_tokens),
            cache_creation_input_tokens: Some(self.cache_write_input_tokens),
            reasoning_tokens: self.reasoning_output_tokens,
        })
    }
}

impl OwnRequestUsage {
    // Native cumulative snapshots include fork ancestry and can omit remote
    // compaction. Only identity-bearing request records establish ownership.
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn reconcile(
        &mut self,
        thread_id: &str,
        offset: usize,
        bytes: &[u8],
        context_window: Option<u64>,
        required_turn: Option<&str>,
    ) -> Result<Vec<ModelRequestTokenUsage>, String> {
        if offset != self.offset {
            return Err("Codex request accounting read lost its position".to_owned());
        }
        let suffix = bytes;
        let complete = suffix
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1);
        if required_turn.is_some() && complete != suffix.len() {
            return Err(
                "Codex terminal accounting source ends with an incomplete record".to_owned(),
            );
        }
        let mut requests = Vec::new();
        let mut event_owner = self.event_owner.clone();
        let mut completed_turns = Vec::new();
        let mut pending_ids = HashMap::<String, usize>::new();
        for line in suffix[..complete]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: serde_json::Value = serde_json::from_slice(line)
                .map_err(|_| "Codex request accounting source contains invalid JSON")?;
            match record.get("type").and_then(serde_json::Value::as_str) {
                Some("session_meta") => {
                    event_owner = Some(
                        record
                            .pointer("/payload/id")
                            .and_then(serde_json::Value::as_str)
                            .filter(|id| !id.is_empty())
                            .ok_or("Codex accounting session metadata has no owner")?
                            .to_owned(),
                    );
                    continue;
                }
                Some("event_msg") => {
                    let event = record
                        .get("payload")
                        .ok_or("Codex accounting event has no payload")?;
                    match event.get("type").and_then(serde_json::Value::as_str) {
                        // Copied fork settings retain their explicit owner. Only
                        // a successful own completion requires own request records.
                        Some("thread_settings_applied") => {
                            event_owner = Some(
                                event
                                    .get("thread_id")
                                    .and_then(serde_json::Value::as_str)
                                    .filter(|id| !id.trim().is_empty())
                                    .ok_or("Codex accounting settings checkpoint has no owner")?
                                    .to_owned(),
                            );
                        }
                        Some("task_complete")
                            if event_owner.as_deref() == Some(thread_id)
                                && event.get("error").is_none_or(serde_json::Value::is_null) =>
                        {
                            let turn_id = event
                                .get("turn_id")
                                .and_then(serde_json::Value::as_str)
                                .filter(|id| !id.is_empty())
                                .ok_or("Codex accounting completion has no turn identity")?;
                            completed_turns.push(turn_id.to_owned());
                        }
                        _ => {}
                    }
                    continue;
                }
                Some("token_usage_record") => {}
                _ => continue,
            }
            let payload = record
                .get("payload")
                .ok_or("Codex request accounting record has no payload")?;
            let owner = payload
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                .ok_or("Codex request accounting record has no owner")?;
            if owner != thread_id {
                continue;
            }
            let request: NativeRequest = serde_json::from_value(payload.clone())
                .map_err(|_| "Codex request accounting record has invalid required fields")?;
            if request.response_id.is_empty() || request.turn_id.is_empty() {
                return Err("Codex request accounting record has an empty identity".to_owned());
            }
            let usage = request.usage.normalized()?;
            if self
                .responses
                .get(&request.response_id)
                .is_some_and(|previous| previous != &request)
                || pending_ids.get(&request.response_id).is_some_and(|index| {
                    let (previous, _): &(NativeRequest, TokenUsage) = &requests[*index];
                    previous != &request
                })
            {
                return Err("Codex request accounting identity has conflicting usage".to_owned());
            }
            pending_ids.insert(request.response_id.clone(), requests.len());
            requests.push((request, usage));
        }

        for turn_id in completed_turns
            .iter()
            .map(String::as_str)
            .chain(required_turn)
        {
            if !self.turns.contains_key(turn_id)
                && !requests
                    .iter()
                    .any(|(request, _)| request.turn_id == turn_id)
            {
                return Err(
                    "Codex completed a turn without exact own-request accounting records"
                        .to_owned(),
                );
            }
        }
        let mut events = Vec::new();
        for (request, usage) in requests {
            if self.responses.contains_key(&request.response_id) {
                continue;
            }
            let turn = self.turns.entry(request.turn_id.clone()).or_default();
            turn.request_count = turn
                .request_count
                .checked_add(1)
                .ok_or("Codex request sequence exceeds the accounting protocol")?;
            if turn.request_count == 1 {
                turn.turn = usage.clone();
            } else {
                turn.turn = add_token_usage(&turn.turn, &usage)?;
            }
            if self.responses.is_empty() {
                self.cumulative = usage.clone();
            } else {
                self.cumulative = add_token_usage(&self.cumulative, &usage)?;
            }
            turn.cumulative = Some(self.cumulative.clone());
            turn.model_context_window = context_window.or(turn.model_context_window);
            let event = ModelRequestTokenUsage {
                request_id: ModelRequestId {
                    turn_id: ModelTurnId(request.turn_id.clone()),
                    sequence: turn.request_count,
                },
                request: usage,
                turn: turn.turn.clone(),
                cumulative: self.cumulative.clone(),
                model_context_window: turn.model_context_window,
                current_context_usage: Some(CurrentContextUsage::Unknown),
                estimated_context_breakdown: None,
            };
            self.responses.insert(request.response_id.clone(), request);
            self.latest = Some(event.clone());
            events.push(event);
        }
        self.event_owner = event_owner;
        self.offset += complete;
        Ok(events)
    }

    pub fn context_update(
        &mut self,
        request: &TokenUsage,
        window: u64,
    ) -> Option<ModelRequestTokenUsage> {
        let latest = self.latest.as_mut()?;
        latest.model_context_window = Some(window);
        latest.current_context_usage = Some(super::current_context_usage(request, window));
        Some(latest.clone())
    }
}

fn add_token_usage(total: &TokenUsage, usage: &TokenUsage) -> Result<TokenUsage, String> {
    let add = |a: u64, b: u64| {
        a.checked_add(b)
            .ok_or_else(|| "Codex token usage exceeds the accounting protocol".to_owned())
    };
    let optional = |a: Option<u64>, b: Option<u64>| a.zip(b).map(|(a, b)| add(a, b)).transpose();
    Ok(TokenUsage {
        input_tokens: add(total.input_tokens, usage.input_tokens)?,
        output_tokens: add(total.output_tokens, usage.output_tokens)?,
        total_tokens: add(total.total_tokens, usage.total_tokens)?,
        cached_prompt_tokens: optional(total.cached_prompt_tokens, usage.cached_prompt_tokens)?,
        cache_creation_input_tokens: optional(
            total.cache_creation_input_tokens,
            usage.cache_creation_input_tokens,
        )?,
        reasoning_tokens: optional(total.reasoning_tokens, usage.reasoning_tokens)?,
    })
}
