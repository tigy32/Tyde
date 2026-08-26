use std::collections::HashSet;

use protocol::{
    AgentControlAgentRef, AgentControlProgress, AgentControlProgressKind,
    AgentControlProgressStatus, AgentExecutionMode, AgentId, ChatEvent, ToolExecutionMode,
    ToolExecutionNormalizationFailure, ToolExecutionOutcome, ToolExecutionResult, ToolProgressData,
    ToolProgressUpdate, ToolRequestType, TydeAgentWaitStatus,
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

pub(crate) fn is_tyde_agent_control_send_message_tool_name(tool_name: &str) -> bool {
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

/// Projects a call to one of Tyde's own agent-control MCP tools onto its typed
/// request.
///
/// Every backend reaches these tools through the same MCP servers but reports
/// the name its own way -- `tyde_await_agents`, `mcp_tyde_tyde_await_agents`,
/// `mcp__tyde_agent_await__tyde_await_agents` -- which the matchers normalize.
///
/// This lives above the individual backends on purpose. A projection written
/// inside one backend is a projection the other four silently lack, which is
/// exactly how four of five ended up rendering the await as an untyped card
/// and never registering it as an active await.
pub(crate) fn tyde_tool_request_type(
    tool_name: &str,
    arguments: &Value,
) -> Option<ToolRequestType> {
    if is_tyde_agent_control_await_tool_name(tool_name) {
        let agent_ids: Vec<AgentId> = parse_await_agent_refs(arguments)
            .into_iter()
            .map(|agent| agent.agent_id)
            .collect();
        // An await naming nobody is not an await card; leaving it untyped keeps
        // the raw arguments visible instead of rendering an empty watch list.
        return (!agent_ids.is_empty()).then_some(ToolRequestType::TydeAwaitAgents { agent_ids });
    }
    if is_tyde_agent_control_spawn_tool_name(tool_name) {
        return Some(ToolRequestType::AgentSpawn {
            prompt: find_string_field(arguments, "prompt", 0),
            name: find_string_field(arguments, "name", 0),
            // Tyde owns the child's lifetime, not the turn that asked for it:
            // the parent goes idle while the child runs and picks the result up
            // through `tyde_await_agents`.
            execution_mode: AgentExecutionMode::Background,
        });
    }
    if is_tyde_agent_control_send_message_tool_name(tool_name) {
        let agent_id = find_string_field(arguments, "agent_id", 0)?;
        let message = find_string_field(arguments, "message", 0)?;
        return Some(ToolRequestType::TydeSendAgentMessage {
            agent_id: AgentId(agent_id),
            message,
        });
    }
    None
}

/// Mirrors [`parse_await_agent_refs`]'s tolerance: providers wrap arguments in
/// their own envelope (Codex nests them under `args`) and some deliver them as
/// a JSON string.
fn find_string_field(value: &Value, key: &str, depth: usize) -> Option<String> {
    if depth > MAX_PARSE_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(Value::as_str)
                && !found.is_empty()
            {
                return Some(found.to_owned());
            }
            ARGUMENT_WRAPPER_KEYS
                .iter()
                .filter_map(|wrapper| map.get(*wrapper))
                .find_map(|nested| find_string_field(nested, key, depth + 1))
        }
        Value::String(text) => {
            let parsed = parse_embedded_json(text)?;
            find_string_field(&parsed, key, depth + 1)
        }
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
    let ChatEvent::ToolExecutionCompleted(mut completion) = event else {
        return (event, None);
    };
    if let Some(failure) = normalization_failures.remove(&completion.tool_call_id) {
        completion.outcome = ToolExecutionOutcome::Failed {
            message: failure.detail.clone(),
            details: Some(failure.detail),
            normalization_failure: Some(failure.kind),
        };
    }
    (ChatEvent::ToolExecutionCompleted(completion), None)
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
    if let Some(text) = value.get("mcp_result").and_then(Value::as_str)
        && let Some(parsed) = parse_embedded_json(text)
    {
        tracing::debug!("normalized canonical tool result from Tycode MCP envelope");
        return canonical_result_value(&parsed);
    }
    if let Some(text) = value
        .pointer("/items/0/Json/content/0/text")
        .or_else(|| value.pointer("/items/0/json/content/0/text"))
        .and_then(Value::as_str)
        && let Some(parsed) = parse_embedded_json(text)
    {
        tracing::debug!("normalized canonical tool result from ACP MCP envelope");
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
        AgentControlProgressKind::Await,
        parse_await_agent_refs(arguments),
    )
}

pub(crate) fn terminal_await_progress_data_for_tool(
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
    status: AgentControlProgressStatus,
) -> Option<ToolProgressData> {
    let mut progress = await_progress_data_for_tool(tool_call_id, tool_name, arguments)?;
    let ToolProgressUpdate::AgentControl(update) = &mut progress.update else {
        unreachable!()
    };
    update.status = status;
    Some(progress)
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
    progress_kind: AgentControlProgressKind,
    agents: Vec<AgentControlAgentRef>,
) -> Option<ToolProgressData> {
    (!agents.is_empty()).then(|| ToolProgressData {
        tool_call_id: tool_call_id.to_string(),
        execution_mode: ToolExecutionMode::Foreground,
        cancellable: false,
        update: ToolProgressUpdate::AgentControl(AgentControlProgress {
            progress_kind,
            agents,
            status: if progress_kind == AgentControlProgressKind::Await {
                AgentControlProgressStatus::Running
            } else {
                AgentControlProgressStatus::Completed
            },
        }),
    })
}

fn is_tyde_agent_control_tool_name(tool_name: &str, bare_normalized_name: &str) -> bool {
    let normalized = normalize_tool_name(tool_name);
    normalized == bare_normalized_name
        || normalized.ends_with(&format!("tydeagentcontrol{bare_normalized_name}"))
        || normalized.ends_with(&format!("mcp{bare_normalized_name}"))
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
