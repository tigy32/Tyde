use std::collections::HashSet;

use protocol::{
    AgentControlAgentRef, AgentControlProgress, AgentControlProgressKind, AgentId, ChatEvent,
    ToolExecutionNormalizationFailure, ToolExecutionResult, ToolProgressData, ToolProgressUpdate,
    ToolRequestType, TydeAgentWaitStatus,
};
use serde::Deserialize;
use serde_json::Value;

const MAX_PARSE_DEPTH: usize = 8;
const ARGUMENT_WRAPPER_KEYS: &[&str] = &[
    "arguments",
    "args",
    "input",
    "input_data",
    "inputData",
    "tool_input",
    "toolInput",
    "parameters",
    "params",
];

pub(crate) fn is_tyde_agent_control_spawn_tool_name(tool_name: &str) -> bool {
    is_tyde_agent_control_tool_name(tool_name, "tydespawnagent")
}

pub(crate) fn is_tyde_agent_control_await_tool_name(tool_name: &str) -> bool {
    is_tyde_agent_control_tool_name(tool_name, "tydeawaitagents")
        || normalize_tool_name(tool_name).ends_with("tydeagentawaittydeawaitagents")
}

fn is_tyde_agent_control_send_message_tool_name(tool_name: &str) -> bool {
    is_tyde_agent_control_tool_name(tool_name, "tydesendagentmessage")
}

#[derive(Debug)]
pub(crate) struct ToolNormalizeError {
    pub(crate) tool: String,
    pub(crate) normalization_failure: ToolExecutionNormalizationFailure,
    pub(crate) detail: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingToolNormalizationFailure {
    pub(crate) kind: ToolExecutionNormalizationFailure,
    pub(crate) detail: String,
}

impl std::fmt::Display for ToolNormalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "canonical tool '{}' violated its typed contract: {}",
            self.tool, self.detail
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendAgentMessageResult {
    ok: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AwaitAgentsResult {
    ready: Vec<TydeAgentWaitStatus>,
    still_thinking: Vec<TydeAgentWaitStatus>,
}

pub(crate) fn tyde_tool_request_type(
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<ToolRequestType>, ToolNormalizeError> {
    let typed = if is_tyde_agent_control_spawn_tool_name(tool_name) {
        let (prompt, name) = find_spawn_request_arguments(arguments, 0).ok_or_else(|| {
            normalize_error(
                tool_name,
                ToolExecutionNormalizationFailure::CanonicalRequest,
                "expected a non-empty prompt in canonical arguments",
            )
        })?;
        ToolRequestType::AgentSpawn {
            prompt: Some(prompt),
            name,
        }
    } else if is_tyde_agent_control_send_message_tool_name(tool_name) {
        let (agent_id, message) = find_send_message_arguments(arguments, 0).ok_or_else(|| {
            normalize_error(
                tool_name,
                ToolExecutionNormalizationFailure::CanonicalRequest,
                "expected non-empty agent_id/agentId and message in canonical arguments",
            )
        })?;
        ToolRequestType::TydeSendAgentMessage { agent_id, message }
    } else if is_tyde_agent_control_await_tool_name(tool_name) {
        let agent_ids = parse_await_agent_refs(arguments)
            .into_iter()
            .map(|agent| agent.agent_id)
            .collect::<Vec<_>>();
        if agent_ids.is_empty() {
            return Err(normalize_error(
                tool_name,
                ToolExecutionNormalizationFailure::CanonicalRequest,
                "agent_ids must contain at least one non-empty id",
            ));
        }
        ToolRequestType::TydeAwaitAgents { agent_ids }
    } else {
        return Ok(None);
    };
    Ok(Some(typed))
}

fn find_spawn_request_arguments(value: &Value, depth: usize) -> Option<(String, Option<String>)> {
    if depth > MAX_PARSE_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(prompt) = map
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|prompt| !prompt.is_empty())
            {
                let name = map
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                return Some((prompt.to_owned(), name));
            }
            ARGUMENT_WRAPPER_KEYS.iter().find_map(|key| {
                map.get(*key)
                    .and_then(|nested| find_spawn_request_arguments(nested, depth + 1))
            })
        }
        Value::String(text) => parse_embedded_json(text)
            .and_then(|parsed| find_spawn_request_arguments(&parsed, depth + 1)),
        _ => None,
    }
}

fn find_send_message_arguments(value: &Value, depth: usize) -> Option<(AgentId, String)> {
    if depth > MAX_PARSE_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            let agent_id = ["agent_id", "agentId"]
                .into_iter()
                .find_map(|key| map.get(key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let message = map.get("message").and_then(Value::as_str);
            if let (Some(agent_id), Some(message)) = (agent_id, message)
                && !message.trim().is_empty()
            {
                return Some((AgentId(agent_id.to_owned()), message.to_owned()));
            }
            ARGUMENT_WRAPPER_KEYS.iter().find_map(|key| {
                map.get(*key)
                    .and_then(|nested| find_send_message_arguments(nested, depth + 1))
            })
        }
        Value::String(text) => parse_embedded_json(text)
            .and_then(|parsed| find_send_message_arguments(&parsed, depth + 1)),
        _ => None,
    }
}

pub(crate) fn tyde_tool_result(
    tool_name: &str,
    result: &Value,
) -> Result<Option<ToolExecutionResult>, ToolNormalizeError> {
    let canonical = canonical_result_value(result);
    let typed = if is_tyde_agent_control_send_message_tool_name(tool_name) {
        let parsed: SendAgentMessageResult = parse_canonical(
            tool_name,
            &canonical,
            ToolExecutionNormalizationFailure::CanonicalResult,
        )?;
        if !parsed.ok {
            return Err(normalize_error(
                tool_name,
                ToolExecutionNormalizationFailure::CanonicalResult,
                "successful result did not acknowledge delivery",
            ));
        }
        ToolExecutionResult::TydeSendAgentMessage
    } else if is_tyde_agent_control_await_tool_name(tool_name) {
        let parsed: AwaitAgentsResult = parse_canonical(
            tool_name,
            &canonical,
            ToolExecutionNormalizationFailure::CanonicalResult,
        )?;
        ToolExecutionResult::TydeAwaitAgents {
            ready: parsed.ready,
            still_thinking: parsed.still_thinking,
        }
    } else {
        return Ok(None);
    };
    Ok(Some(typed))
}

pub(crate) fn normalize_tyde_chat_event(
    event: ChatEvent,
    normalization_failures: &mut std::collections::HashMap<String, PendingToolNormalizationFailure>,
) -> (ChatEvent, Option<String>) {
    match event {
        ChatEvent::ToolRequest(mut request) => {
            let ToolRequestType::Other { args } = &request.tool_type else {
                return (ChatEvent::ToolRequest(request), None);
            };
            match tyde_tool_request_type(&request.tool_name, args) {
                Ok(Some(typed)) => {
                    request.tool_type = typed;
                    (ChatEvent::ToolRequest(request), None)
                }
                Ok(None) => (ChatEvent::ToolRequest(request), None),
                Err(error) => {
                    normalization_failures.insert(
                        request.tool_call_id.clone(),
                        PendingToolNormalizationFailure {
                            kind: error.normalization_failure,
                            detail: error.to_string(),
                        },
                    );
                    (ChatEvent::ToolRequest(request), None)
                }
            }
        }
        ChatEvent::ToolExecutionCompleted(mut completion) => {
            let request_failure = normalization_failures.remove(&completion.tool_call_id);
            let result_failure = if completion.success {
                match &completion.tool_result {
                    ToolExecutionResult::Other { .. } => match tyde_tool_result(
                        &completion.tool_name,
                        &serde_json::to_value(&completion.tool_result)
                            .expect("serialize tool result"),
                    ) {
                        Ok(Some(typed)) => {
                            completion.tool_result = typed;
                            None
                        }
                        Ok(None) => None,
                        Err(error) => Some(PendingToolNormalizationFailure {
                            kind: error.normalization_failure,
                            detail: error.to_string(),
                        }),
                    },
                    _ => None,
                }
            } else {
                None
            };
            let reported_failure = if request_failure.is_none() && result_failure.is_none() {
                completion
                    .normalization_failure
                    .map(|kind| PendingToolNormalizationFailure {
                        kind,
                        detail: normalization_failure_detail(kind),
                    })
            } else {
                None
            };
            let failure = merge_pending_normalization_failure(
                merge_pending_normalization_failure(reported_failure, request_failure),
                result_failure,
            );
            if let Some(failure) = failure {
                completion.success = false;
                completion.error = Some(failure.detail);
                completion.normalization_failure = Some(failure.kind);
            }
            (ChatEvent::ToolExecutionCompleted(completion), None)
        }
        event => (event, None),
    }
}

fn merge_pending_normalization_failure(
    existing: Option<PendingToolNormalizationFailure>,
    incoming: Option<PendingToolNormalizationFailure>,
) -> Option<PendingToolNormalizationFailure> {
    match (existing, incoming) {
        (None, None) => None,
        (Some(failure), None) | (None, Some(failure)) => Some(failure),
        (Some(existing), Some(incoming)) => Some(PendingToolNormalizationFailure {
            kind: existing.kind.combined_with(incoming.kind),
            detail: if existing.detail == incoming.detail {
                existing.detail
            } else {
                format!("{}; {}", existing.detail, incoming.detail)
            },
        }),
    }
}

fn normalization_failure_detail(failure: ToolExecutionNormalizationFailure) -> String {
    match failure {
        ToolExecutionNormalizationFailure::CanonicalRequest => {
            "Canonical tool request failed typed validation".to_string()
        }
        ToolExecutionNormalizationFailure::CanonicalResult => {
            "Canonical tool result failed typed validation".to_string()
        }
        ToolExecutionNormalizationFailure::CanonicalRequestAndResult => {
            "Canonical tool request and result failed typed validation".to_string()
        }
    }
}

fn parse_canonical<T: for<'de> Deserialize<'de>>(
    tool_name: &str,
    value: &Value,
    normalization_failure: ToolExecutionNormalizationFailure,
) -> Result<T, ToolNormalizeError> {
    serde_json::from_value(value.clone()).map_err(|_| {
        normalize_error(
            tool_name,
            normalization_failure,
            "result does not match the canonical schema",
        )
    })
}

fn normalize_error(
    tool_name: &str,
    normalization_failure: ToolExecutionNormalizationFailure,
    detail: impl Into<String>,
) -> ToolNormalizeError {
    ToolNormalizeError {
        tool: tool_name.to_string(),
        normalization_failure,
        detail: detail.into(),
    }
}

fn canonical_result_value(value: &Value) -> Value {
    if value.get("kind").and_then(Value::as_str) == Some("Other") {
        return value
            .get("result")
            .map(canonical_result_value)
            .unwrap_or_else(|| value.clone());
    }
    if let Some(text) = value.as_str()
        && let Some(parsed) = parse_embedded_json(text)
    {
        return canonical_result_value(&parsed);
    }
    if let Some(text) = value.pointer("/content/0/text").and_then(Value::as_str)
        && let Some(parsed) = parse_embedded_json(text)
    {
        return canonical_result_value(&parsed);
    }
    for key in ["result", "structuredContent", "structured_content"] {
        if let Some(candidate) = value.get(key) {
            let normalized = canonical_result_value(candidate);
            if normalized.is_object() {
                return normalized;
            }
        }
    }
    value.clone()
}

pub(crate) fn await_progress_data_for_tool(
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
) -> Option<ToolProgressData> {
    if !is_tyde_agent_control_await_tool_name(tool_name) {
        return None;
    }
    agent_control_progress_data(
        tool_call_id,
        tool_name,
        AgentControlProgressKind::Await,
        parse_await_agent_refs(arguments),
    )
}

pub(crate) fn spawn_progress_data_for_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    tool_result: &Value,
) -> Option<ToolProgressData> {
    if !is_tyde_agent_control_spawn_tool_name(tool_name) {
        return None;
    }
    agent_control_progress_data(
        tool_call_id,
        tool_name,
        AgentControlProgressKind::Spawn,
        parse_spawn_agent_ref(tool_result).into_iter().collect(),
    )
}

pub(crate) fn parse_await_agent_refs(arguments: &Value) -> Vec<AgentControlAgentRef> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    collect_await_agent_refs(arguments, 0, &mut refs, &mut seen);
    refs
}

pub(crate) fn parse_spawn_agent_ref(result: &Value) -> Option<AgentControlAgentRef> {
    find_spawn_agent_ref(result, 0)
}

fn agent_control_progress_data(
    tool_call_id: &str,
    tool_name: &str,
    progress_kind: AgentControlProgressKind,
    agents: Vec<AgentControlAgentRef>,
) -> Option<ToolProgressData> {
    (!agents.is_empty()).then(|| ToolProgressData {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        update: ToolProgressUpdate::AgentControl(AgentControlProgress {
            progress_kind,
            agents,
        }),
    })
}

fn is_tyde_agent_control_tool_name(tool_name: &str, bare_normalized_name: &str) -> bool {
    let normalized = normalize_tool_name(tool_name);
    normalized == bare_normalized_name
        || normalized.ends_with(&format!("tydeagentcontrol{bare_normalized_name}"))
        || normalized.ends_with(&format!("mcptyde{bare_normalized_name}"))
}

fn normalize_tool_name(tool_name: &str) -> String {
    tool_name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn collect_await_agent_refs(
    value: &Value,
    depth: usize,
    refs: &mut Vec<AgentControlAgentRef>,
    seen: &mut HashSet<String>,
) {
    if depth > MAX_PARSE_DEPTH {
        return;
    }

    match value {
        Value::Object(map) => {
            for key in ["agent_ids", "agentIds", "agent_id", "agentId"] {
                if let Some(candidate) = map.get(key) {
                    collect_agent_ref_values(candidate, depth + 1, refs, seen);
                }
            }
            for key in ARGUMENT_WRAPPER_KEYS {
                if let Some(candidate) = map.get(*key) {
                    collect_await_agent_refs(candidate, depth + 1, refs, seen);
                }
            }
        }
        Value::Array(_) => collect_agent_ref_values(value, depth + 1, refs, seen),
        Value::String(text) => {
            if let Some(parsed) = parse_embedded_json(text) {
                collect_await_agent_refs(&parsed, depth + 1, refs, seen);
            }
        }
        _ => {}
    }
}

fn collect_agent_ref_values(
    value: &Value,
    depth: usize,
    refs: &mut Vec<AgentControlAgentRef>,
    seen: &mut HashSet<String>,
) {
    if depth > MAX_PARSE_DEPTH {
        return;
    }

    match value {
        Value::String(text) => {
            if let Some(parsed) = parse_embedded_json(text) {
                collect_agent_ref_values(&parsed, depth + 1, refs, seen);
            } else {
                push_agent_ref(refs, seen, text, None);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_agent_ref_values(value, depth + 1, refs, seen);
            }
        }
        Value::Object(map) => {
            if let Some(agent_id) = string_field(value, &["agent_id", "agentId", "id"]) {
                let name = string_field(value, &["name", "agent_name", "agentName"]);
                push_agent_ref(refs, seen, agent_id, name);
            }
            for key in ["agent_ids", "agentIds", "agent_id", "agentId"] {
                if let Some(candidate) = map.get(key) {
                    collect_agent_ref_values(candidate, depth + 1, refs, seen);
                }
            }
        }
        _ => {}
    }
}

fn find_spawn_agent_ref(value: &Value, depth: usize) -> Option<AgentControlAgentRef> {
    if depth > MAX_PARSE_DEPTH {
        return None;
    }

    match value {
        Value::Object(map) => {
            if let Some(agent_id) = string_field(
                value,
                &["agent_id", "agentId", "spawned_agent_id", "spawnedAgentId"],
            ) {
                let name = string_field(
                    value,
                    &[
                        "name",
                        "agent_name",
                        "agentName",
                        "display_name",
                        "displayName",
                    ],
                )
                .and_then(normalize_optional_string);
                return normalized_agent_ref(agent_id, name);
            }

            for key in [
                "result",
                "data",
                "payload",
                "json",
                "structuredContent",
                "content",
                "contentItems",
                "items",
                "resource",
                "resource_link",
                "resourceLink",
                "text",
                "output",
                "aggregatedOutput",
            ] {
                if let Some(candidate) = map.get(key)
                    && let Some(found) = find_spawn_agent_ref(candidate, depth + 1)
                {
                    return Some(found);
                }
            }

            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_spawn_agent_ref(value, depth + 1)),
        Value::String(text) => {
            parse_embedded_json(text).and_then(|parsed| find_spawn_agent_ref(&parsed, depth + 1))
        }
        _ => None,
    }
}

fn parse_embedded_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

fn string_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let map = value.as_object()?;
    keys.iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn push_agent_ref(
    refs: &mut Vec<AgentControlAgentRef>,
    seen: &mut HashSet<String>,
    agent_id: &str,
    name: Option<&str>,
) {
    let Some(agent_ref) = normalized_agent_ref(agent_id, name.and_then(normalize_optional_string))
    else {
        return;
    };
    if seen.insert(agent_ref.agent_id.0.clone()) {
        refs.push(agent_ref);
    }
}

fn normalized_agent_ref(agent_id: &str, name: Option<String>) -> Option<AgentControlAgentRef> {
    let agent_id = agent_id.trim();
    if agent_id.is_empty() {
        return None;
    }
    Some(AgentControlAgentRef {
        agent_id: AgentId(agent_id.to_string()),
        name,
    })
}

fn normalize_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_tyde_agent_control_tool_names() {
        assert!(is_tyde_agent_control_spawn_tool_name("tyde_spawn_agent"));
        assert!(is_tyde_agent_control_spawn_tool_name(
            "mcp__tyde-agent-control__tyde_spawn_agent"
        ));
        assert!(is_tyde_agent_control_await_tool_name(
            "mcp__tyde_agent_control__tyde_await_agents"
        ));
        assert!(is_tyde_agent_control_await_tool_name(
            "mcp__tyde-agent-await__tyde_await_agents"
        ));
        assert!(is_tyde_agent_control_spawn_tool_name(
            "mcp_tyde_tyde_spawn_agent"
        ));
        assert!(is_tyde_agent_control_await_tool_name(
            "mcp_tyde_tyde_await_agents"
        ));

        assert!(!is_tyde_agent_control_spawn_tool_name("spawn_agent"));
        assert!(!is_tyde_agent_control_await_tool_name("wait_agent"));
    }

    #[test]
    fn parses_await_agent_ids_from_common_argument_shapes() {
        let refs = parse_await_agent_refs(&json!({
            "agent_ids": [
                "agent-a",
                " ",
                { "agent_id": "agent-b", "name": " Builder " },
                "agent-a"
            ],
            "other": true
        }));
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].agent_id, AgentId("agent-a".to_owned()));
        assert_eq!(refs[0].name, None);
        assert_eq!(refs[1].agent_id, AgentId("agent-b".to_owned()));
        assert_eq!(refs[1].name.as_deref(), Some("Builder"));

        let refs = parse_await_agent_refs(&json!({
            "arguments": "{\"agentIds\":[\"agent-c\",\"agent-d\"]}"
        }));
        let ids = refs
            .into_iter()
            .map(|agent| agent.agent_id.0)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["agent-c", "agent-d"]);
    }

    #[test]
    fn parses_await_agent_ids_from_codex_mcp_item_wrapper() {
        let progress = await_progress_data_for_tool(
            "call-await",
            "mcp__tyde-agent-await__tyde_await_agents",
            &json!({
                "id": "call-await",
                "type": "mcpToolCall",
                "tool": "mcp__tyde-agent-await__tyde_await_agents",
                "arguments": {
                    "agent_ids": [
                        "agent-a",
                        { "agent_id": "agent-b", "name": " Builder " }
                    ]
                }
            }),
        )
        .expect("await progress");

        let ToolProgressUpdate::AgentControl(progress) = progress.update else {
            panic!("expected agent-control progress");
        };
        assert_eq!(progress.progress_kind, AgentControlProgressKind::Await);
        assert_eq!(
            progress
                .agents
                .iter()
                .map(|agent| (agent.agent_id.0.as_str(), agent.name.as_deref()))
                .collect::<Vec<_>>(),
            vec![("agent-a", None), ("agent-b", Some("Builder"))]
        );
    }

    #[test]
    fn parses_spawn_agent_result_from_wrappers() {
        let direct = parse_spawn_agent_ref(&json!({
            "agent_id": "agent-a",
            "name": "Scout"
        }))
        .expect("direct result");
        assert_eq!(direct.agent_id, AgentId("agent-a".to_owned()));
        assert_eq!(direct.name.as_deref(), Some("Scout"));

        let other_wrapper = parse_spawn_agent_ref(&json!({
            "kind": "Other",
            "result": "{\"agent_id\":\"agent-b\",\"name\":\"Builder\"}"
        }))
        .expect("tool result wrapper");
        assert_eq!(other_wrapper.agent_id, AgentId("agent-b".to_owned()));
        assert_eq!(other_wrapper.name.as_deref(), Some("Builder"));

        let mcp_content = parse_spawn_agent_ref(&json!({
            "result": {
                "content": [{
                    "type": "text",
                    "text": "{\"agent_id\":\"agent-c\",\"name\":\"Reviewer\"}"
                }]
            }
        }))
        .expect("mcp content wrapper");
        assert_eq!(mcp_content.agent_id, AgentId("agent-c".to_owned()));
        assert_eq!(mcp_content.name.as_deref(), Some("Reviewer"));
    }

    #[test]
    fn builds_progress_snapshots_only_for_matching_tools() {
        let await_progress = await_progress_data_for_tool(
            "call-await",
            "tyde_await_agents",
            &json!({ "agent_ids": ["agent-a"] }),
        )
        .expect("await progress");
        let ToolProgressUpdate::AgentControl(progress) = await_progress.update else {
            panic!("expected agent-control progress");
        };
        assert_eq!(progress.progress_kind, AgentControlProgressKind::Await);
        assert_eq!(progress.agents[0].agent_id, AgentId("agent-a".to_owned()));

        assert!(
            spawn_progress_data_for_tool_result(
                "call-native",
                "spawn_agent",
                &json!({ "agent_id": "agent-a" })
            )
            .is_none()
        );
    }

    #[test]
    fn normalizes_canonical_tyde_requests_and_results() {
        for tool_name in [
            "tyde_send_agent_message",
            "mcp__tyde-agent-control__tyde_send_agent_message",
        ] {
            assert_eq!(
                tyde_tool_request_type(
                    tool_name,
                    &json!({ "agent_id": "agent-a", "message": "# Follow up" })
                )
                .expect("send request")
                .expect("canonical request"),
                ToolRequestType::TydeSendAgentMessage {
                    agent_id: AgentId("agent-a".to_owned()),
                    message: "# Follow up".to_owned(),
                }
            );
            assert_eq!(
                tyde_tool_result(
                    tool_name,
                    &json!({ "kind": "Other", "result": { "ok": true } })
                )
                .expect("send result")
                .expect("canonical result"),
                ToolExecutionResult::TydeSendAgentMessage
            );
        }

        for tool_name in [
            "tyde_await_agents",
            "mcp__tyde-agent-control__tyde_await_agents",
            "mcp__tyde-agent-await__tyde_await_agents",
        ] {
            assert_eq!(
                tyde_tool_request_type(
                    tool_name,
                    &json!({ "arguments": { "agent_ids": ["agent-a", "agent-b"] } })
                )
                .expect("await request")
                .expect("canonical request"),
                ToolRequestType::TydeAwaitAgents {
                    agent_ids: vec![AgentId("agent-a".to_owned()), AgentId("agent-b".to_owned())],
                }
            );
            assert!(matches!(
                tyde_tool_result(
                    tool_name,
                    &json!({
                        "kind": "Other",
                        "result": {
                            "ready": [{ "agent_id": "agent-a", "status": "idle" }],
                            "still_thinking": [{ "agent_id": "agent-b", "status": "thinking" }]
                        }
                    })
                )
                .expect("await result")
                .expect("canonical result"),
                ToolExecutionResult::TydeAwaitAgents { ready, still_thinking }
                    if ready[0].agent_id == AgentId("agent-a".to_owned())
                        && still_thinking[0].agent_id == AgentId("agent-b".to_owned())
            ));
        }
    }

    #[test]
    fn request_normalizer_and_progress_accept_observed_argument_wrappers() {
        let exact_message = "  # Keep exact bytes\n\n- item  ";
        for wrapper in ARGUMENT_WRAPPER_KEYS {
            let await_arguments = json!({
                (*wrapper): { "agentIds": ["agent-a", "agent-b"] }
            });
            let typed = tyde_tool_request_type("tyde_await_agents", &await_arguments)
                .expect("observed await wrapper must not error")
                .expect("canonical await request");
            assert_eq!(
                typed,
                ToolRequestType::TydeAwaitAgents {
                    agent_ids: vec![AgentId("agent-a".to_owned()), AgentId("agent-b".to_owned())],
                },
                "await wrapper {wrapper}"
            );
            let progress =
                await_progress_data_for_tool("call-await", "tyde_await_agents", &await_arguments)
                    .expect("observed await wrapper must produce progress");
            let ToolProgressUpdate::AgentControl(progress) = progress.update else {
                panic!("expected agent-control progress for {wrapper}");
            };
            assert_eq!(
                progress
                    .agents
                    .into_iter()
                    .map(|agent| agent.agent_id)
                    .collect::<Vec<_>>(),
                vec![AgentId("agent-a".to_owned()), AgentId("agent-b".to_owned())],
                "await progress wrapper {wrapper}"
            );

            let send_arguments = json!({
                (*wrapper): { "agentId": "agent-a", "message": exact_message }
            });
            assert_eq!(
                tyde_tool_request_type("tyde_send_agent_message", &send_arguments)
                    .expect("observed send wrapper must not error")
                    .expect("canonical send request"),
                ToolRequestType::TydeSendAgentMessage {
                    agent_id: AgentId("agent-a".to_owned()),
                    message: exact_message.to_owned(),
                },
                "send wrapper {wrapper}"
            );
        }

        let embedded_await = json!({
            "arguments": "{\"agentIds\":[\"agent-c\",\"agent-d\"]}"
        });
        assert_eq!(
            tyde_tool_request_type("tyde_await_agents", &embedded_await)
                .expect("embedded await JSON must not error")
                .expect("canonical await request"),
            ToolRequestType::TydeAwaitAgents {
                agent_ids: vec![AgentId("agent-c".to_owned()), AgentId("agent-d".to_owned())],
            }
        );
        for singular in [
            json!({ "agent_id": "agent-e" }),
            json!({ "agentId": { "id": "agent-e", "name": "E" } }),
        ] {
            assert_eq!(
                tyde_tool_request_type("tyde_await_agents", &singular)
                    .expect("observed singular await shape must not error")
                    .expect("canonical await request"),
                ToolRequestType::TydeAwaitAgents {
                    agent_ids: vec![AgentId("agent-e".to_owned())],
                }
            );
        }

        let embedded_send = json!({
            "parameters": {
                "input": {
                    "args": format!(
                        "{{\"agentId\":\"agent-c\",\"message\":{}}}",
                        serde_json::to_string(exact_message).expect("serialize message")
                    )
                }
            }
        });
        assert_eq!(
            tyde_tool_request_type("tyde_send_agent_message", &embedded_send)
                .expect("embedded send JSON must not error")
                .expect("canonical send request"),
            ToolRequestType::TydeSendAgentMessage {
                agent_id: AgentId("agent-c".to_owned()),
                message: exact_message.to_owned(),
            }
        );
    }

    #[test]
    fn malformed_canonical_tyde_payloads_are_errors() {
        let request =
            tyde_tool_request_type("tyde_send_agent_message", &json!({ "agent_id": "agent-a" }));
        assert_eq!(
            request
                .expect_err("malformed canonical request")
                .normalization_failure,
            ToolExecutionNormalizationFailure::CanonicalRequest
        );
        let result = tyde_tool_result(
            "mcp__tyde-agent-await__tyde_await_agents",
            &json!({ "ready": [] }),
        );
        assert_eq!(
            result
                .expect_err("malformed canonical result")
                .normalization_failure,
            ToolExecutionNormalizationFailure::CanonicalResult
        );
        assert_eq!(
            tyde_tool_request_type("unrelated", &json!({})).expect("unrelated request"),
            None
        );
    }
}
