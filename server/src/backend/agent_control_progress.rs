use std::collections::HashSet;

use protocol::{
    AgentControlAgentRef, AgentControlProgress, AgentControlProgressKind, AgentId,
    ToolProgressData, ToolProgressUpdate,
};
use serde_json::Value;

const MAX_PARSE_DEPTH: usize = 8;

pub(crate) fn is_tyde_agent_control_spawn_tool_name(tool_name: &str) -> bool {
    is_tyde_agent_control_tool_name(tool_name, "tydespawnagent")
}

pub(crate) fn is_tyde_agent_control_await_tool_name(tool_name: &str) -> bool {
    is_tyde_agent_control_tool_name(tool_name, "tydeawaitagents")
        || normalize_tool_name(tool_name).ends_with("tydeagentawaittydeawaitagents")
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
            for key in [
                "arguments",
                "args",
                "input",
                "input_data",
                "inputData",
                "tool_input",
                "toolInput",
                "parameters",
                "params",
            ] {
                if let Some(candidate) = map.get(key) {
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
}
