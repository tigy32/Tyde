use std::borrow::Cow;

use futures_util::future::BoxFuture;
use protocol::{
    AcpAdapterId, AcpAgentSpec, BackendKind, MessageTokenUsage, TokenUsage, TokenUsageScope,
    TokenUsageUnavailableReason,
};
use serde_json::{Value, json};

use crate::backend::acp::AcpSpawnSpec;
use crate::backend::acp::adapter::{
    AcpAgentAdapter, AcpSessionKind, AcpSessionRoots, NormalizedUpdate,
};
use crate::backend::acp::adapters::stock::pick_local_workspace_root;

pub struct GrokAdapter {
    spec: AcpAgentSpec,
}

impl GrokAdapter {
    pub fn new(spec: AcpAgentSpec) -> Self {
        Self { spec }
    }
}

impl AcpAgentAdapter for GrokAdapter {
    fn id(&self) -> AcpAdapterId {
        AcpAdapterId::Grok
    }

    fn display_name(&self) -> &str {
        "Grok"
    }

    fn backend_kind(&self) -> BackendKind {
        BackendKind::Grok
    }

    fn agent_name(&self) -> &str {
        "grok"
    }

    fn resolve_roots<'a>(
        &'a self,
        workspace_roots: &'a [String],
        _ssh_host: Option<&'a str>,
        _kind: AcpSessionKind,
    ) -> BoxFuture<'a, Result<AcpSessionRoots, String>> {
        Box::pin(async move {
            let scope_root = pick_local_workspace_root(workspace_roots, "Grok")?;
            Ok(AcpSessionRoots {
                session_cwd: scope_root.clone(),
                scope_root,
            })
        })
    }

    fn spawn_spec(
        &self,
        roots: &AcpSessionRoots,
        ssh_host: Option<&str>,
    ) -> Result<AcpSpawnSpec, String> {
        if ssh_host.is_some() {
            return Err("Grok does not yet support Tyde SSH sessions".to_owned());
        }
        let command = self.spec.command.trim();
        let program = if command.is_empty() { "grok" } else { command };
        let args = if self.spec.args.is_empty() {
            vec!["agent", "stdio"]
        } else {
            self.spec.args.iter().map(String::as_str).collect()
        };
        Ok(AcpSpawnSpec::new("Grok ACP", program, &args).with_local_cwd(roots.session_cwd.clone()))
    }

    fn normalize_notification(&self, method: &str, params: &Value) -> Option<NormalizedUpdate> {
        if method != "_x.ai/session_notification" {
            return None;
        }
        let update = params.get("update")?;
        let session_update = match update.get("sessionUpdate").and_then(Value::as_str)? {
            "response_completed" => "response_end",
            "turn_completed" => "turn_end",
            _ => return None,
        };
        Some(NormalizedUpdate {
            session_update,
            params: update.clone(),
        })
    }

    fn sanitize_stream_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if text.contains("<|eos|>") {
            Cow::Owned(text.replace("<|eos|>", ""))
        } else {
            Cow::Borrowed(text)
        }
    }

    fn normalize_tool_name<'a>(
        &self,
        tool_name: &'a str,
        args: &Value,
        params: &Value,
    ) -> Cow<'a, str> {
        if tool_name == "use_tool" {
            if args.get("agent_ids").is_some() {
                return Cow::Borrowed("tyde_await_agents");
            }
            if args.get("agent_id").is_some() && args.get("message").is_some() {
                return Cow::Borrowed("tyde_send_agent_message");
            }
            if args.get("backend_kind").is_some()
                && args.get("workspace_roots").is_some()
                && args.get("prompt").is_some()
            {
                return Cow::Borrowed("tyde_spawn_agent");
            }
            if let Some(tool_name) = params
                .get("rawInput")
                .and_then(|raw| raw.get("tool_name"))
                .and_then(Value::as_str)
            {
                return Cow::Owned(tool_name.to_owned());
            }
        }
        Cow::Borrowed(tool_name)
    }

    fn defer_tool_request(&self, kind: &str, args: &Value) -> bool {
        kind == "search" || args.get("variant").and_then(Value::as_str) == Some("WebSearch")
    }

    fn refine_tool_request(
        &self,
        completion: &crate::backend::acp::AcpToolCallCompletion,
        request_payload: &Value,
    ) -> Option<Value> {
        if request_payload.get("kind").and_then(Value::as_str) != Some("WebSearch") {
            return None;
        }
        let query = find_string(&completion.tool_result, &["query"])?;
        Some(json!({ "kind": "WebSearch", "query": query }))
    }

    fn delete_session<'a>(
        &'a self,
        session_id: &'a str,
        ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if ssh_host.is_some() {
                return Err("Grok does not yet support Tyde SSH sessions".to_owned());
            }
            let output = tokio::process::Command::new("grok")
                .args(["sessions", "delete", session_id])
                .output()
                .await
                .map_err(|error| format!("failed to delete Grok session: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "failed to delete Grok session: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
    }

    fn map_tool_request<'a>(
        &'a self,
        kind: &'a str,
        args: &'a Value,
        workspace_root: &'a str,
    ) -> BoxFuture<'a, Value> {
        Box::pin(async move {
            let agent_control_name = if args.get("agent_ids").is_some() {
                Some("tyde_await_agents")
            } else if args.get("agent_id").is_some() && args.get("message").is_some() {
                Some("tyde_send_agent_message")
            } else if args.get("backend_kind").is_some()
                && args.get("workspace_roots").is_some()
                && args.get("prompt").is_some()
            {
                Some("tyde_spawn_agent")
            } else {
                None
            };
            if let Some(tool_name) = agent_control_name
                && let Some(tool_type) =
                    crate::backend::agent_control_progress::tyde_tool_request_type(tool_name, args)
                && let Ok(value) = serde_json::to_value(tool_type)
            {
                return value;
            }
            if let Some(command) = args.get("command").and_then(Value::as_str)
                && !command.trim().is_empty()
            {
                return json!({
                    "kind": "RunCommand",
                    "command": command,
                    "working_directory": args
                        .get("cwd")
                        .and_then(Value::as_str)
                        .unwrap_or(workspace_root),
                });
            }
            if kind == "task" || args.get("subagent_type").is_some() {
                let execution_mode = if args
                    .get("background")
                    .or_else(|| args.get("run_in_background"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    protocol::AgentExecutionMode::Background
                } else {
                    protocol::AgentExecutionMode::Foreground
                };
                return serde_json::to_value(protocol::ToolRequestType::AgentSpawn {
                    prompt: args
                        .get("prompt")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: args
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    execution_mode,
                })
                .expect("serialize Grok subagent request");
            }
            if let Some(query) = args.get("query").and_then(Value::as_str) {
                return json!({ "kind": "WebSearch", "query": query });
            }
            if kind == "search" || args.get("variant").and_then(Value::as_str) == Some("WebSearch")
            {
                return json!({ "kind": "WebSearch", "query": "" });
            }
            if let Some(path) = args
                .get("target_file")
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
                && ["png", "jpg", "jpeg", "gif", "webp"]
                    .iter()
                    .any(|extension| path.to_ascii_lowercase().ends_with(extension))
            {
                return json!({ "kind": "ViewImage", "path": path });
            }
            if args.get("target_file").is_some() {
                return super::super::tools::default_map_tool_request("read", args, workspace_root)
                    .await;
            }
            if args.get("file_path").is_some()
                && (args.get("content").is_some()
                    || args.get("new_string").is_some()
                    || args.get("old_string").is_some())
            {
                return super::super::tools::default_map_tool_request("edit", args, workspace_root)
                    .await;
            }
            if kind == "read"
                && let Some(path) = args.get("path").and_then(Value::as_str)
                && ["png", "jpg", "jpeg", "gif", "webp"]
                    .iter()
                    .any(|extension| path.to_ascii_lowercase().ends_with(extension))
            {
                return json!({ "kind": "ViewImage", "path": path });
            }
            super::super::tools::default_map_tool_request(kind, args, workspace_root).await
        })
    }

    fn map_tool_result(
        &self,
        completion: &crate::backend::acp::AcpToolCallCompletion,
        request_payload: Option<&Value>,
    ) -> Value {
        let request_kind = request_payload
            .and_then(|payload| payload.get("kind"))
            .and_then(Value::as_str);
        if matches!(
            request_kind,
            Some("TydeSendAgentMessage" | "TydeAwaitAgents")
        ) {
            let mapped = crate::backend::acp::backend::map_tool_completion_result(
                completion,
                request_payload,
            );
            let tool_name = if request_kind == Some("TydeAwaitAgents") {
                "tyde_await_agents"
            } else {
                "tyde_send_agent_message"
            };
            if let Ok(Some(result)) =
                crate::backend::agent_control_progress::tyde_tool_result(tool_name, &mapped)
                && let Ok(value) = serde_json::to_value(result)
            {
                return value;
            }
            return mapped;
        }
        if completion.is_mcp_tool {
            if let Some(text) = find_string(&completion.tool_result, &["OkayOutput"]) {
                return crate::backend::normalize_mcp_call_tool_result(&json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                }))
                .tool_result;
            }
            return crate::backend::acp::backend::map_tool_completion_result(
                completion,
                request_payload,
            );
        }
        if !completion.success {
            return super::super::tools::error_result(completion);
        }
        match request_payload
            .and_then(|payload| payload.get("kind"))
            .and_then(Value::as_str)
        {
            Some("ReadFiles") => {
                let files = request_payload
                    .and_then(|payload| payload.get("file_paths"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|path| json!({ "path": path, "bytes": 1 }))
                    .collect::<Vec<_>>();
                json!({ "kind": "ReadFiles", "files": files })
            }
            Some("ModifyFile") => {
                let before = request_payload
                    .and_then(|payload| payload.get("before"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let after = request_payload
                    .and_then(|payload| payload.get("after"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (lines_added, lines_removed) =
                    crate::backend::estimate_line_delta(before, after);
                json!({
                    "kind": "ModifyFile",
                    "lines_added": lines_added,
                    "lines_removed": lines_removed,
                })
            }
            Some("RunCommand") => {
                let exit_code =
                    find_i64(&completion.tool_result, &["exitCode", "exit_code"]).unwrap_or(0);
                let stdout = find_string(&completion.tool_result, &["stdout", "output", "text"])
                    .or_else(|| find_byte_string(&completion.tool_result, "output"))
                    .unwrap_or_default();
                let stderr = find_string(&completion.tool_result, &["stderr"]).unwrap_or_default();
                json!({
                    "kind": "RunCommand",
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                })
            }
            Some("WebSearch") => json!({ "kind": "WebSearch" }),
            Some("ViewImage") => json!({ "kind": "ViewImage" }),
            _ => super::super::tools::default_map_tool_result(completion, request_payload),
        }
    }

    fn map_usage(&self, raw: Option<&Value>) -> Option<MessageTokenUsage> {
        let raw = raw?;
        let usage = crate::backend::acp::backend::normalize_token_usage(Some(raw))?;
        let mut usage = serde_json::from_value::<TokenUsage>(usage).ok()?;
        match raw.get("sessionUpdate").and_then(Value::as_str) {
            Some("response_completed") => {
                usage = crate::backend::acp::backend::grok_request_usage_with_cache(usage);
                Some(MessageTokenUsage::request_known(usage))
            }
            Some("turn_completed") => Some(MessageTokenUsage {
                request: TokenUsageScope::Unavailable {
                    reason: TokenUsageUnavailableReason::BackendDidNotReport,
                },
                turn: TokenUsageScope::Known {
                    usage: Box::new(usage),
                },
                cumulative: TokenUsageScope::Unavailable {
                    reason: TokenUsageUnavailableReason::BackendDidNotReport,
                },
            }),
            _ => None,
        }
    }
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    if let Value::Object(object) = value {
        for key in keys {
            if let Some(found) = object.get(*key).and_then(Value::as_str) {
                return Some(found.to_owned());
            }
        }
        return object.values().find_map(|value| find_string(value, keys));
    }
    value
        .as_array()
        .and_then(|values| values.iter().find_map(|value| find_string(value, keys)))
}

fn find_byte_string(value: &Value, key: &str) -> Option<String> {
    if let Value::Object(object) = value {
        if let Some(bytes) = object.get(key).and_then(Value::as_array) {
            let bytes = bytes
                .iter()
                .map(Value::as_u64)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(u8::try_from)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
        return object
            .values()
            .find_map(|value| find_byte_string(value, key));
    }
    value
        .as_array()
        .and_then(|values| values.iter().find_map(|value| find_byte_string(value, key)))
}

fn find_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    if let Value::Object(object) = value {
        for key in keys {
            if let Some(found) = object.get(*key).and_then(Value::as_i64) {
                return Some(found);
            }
        }
        return object.values().find_map(|value| find_i64(value, keys));
    }
    value
        .as_array()
        .and_then(|values| values.iter().find_map(|value| find_i64(value, keys)))
}
