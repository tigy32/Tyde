use std::borrow::Cow;

use futures_util::future::BoxFuture;
use protocol::{AcpAdapterId, AcpAgentSpec, BackendKind, MessageTokenUsage, TokenUsage};
use serde_json::{Value, json};

use crate::backend::acp::AcpSpawnSpec;
use crate::backend::acp::adapter::{AcpAgentAdapter, AcpSessionKind, AcpSessionRoots};
use crate::backend::acp::adapters::stock::pick_local_workspace_root;

pub struct OpenCodeAdapter {
    spec: AcpAgentSpec,
}

impl OpenCodeAdapter {
    pub fn new(spec: AcpAgentSpec) -> Self {
        Self { spec }
    }

    fn command(&self) -> &str {
        let command = self.spec.command.trim();
        if command.is_empty() {
            "opencode"
        } else {
            command
        }
    }

    fn config_content(&self) -> Result<String, String> {
        let mut config = match std::env::var("OPENCODE_CONFIG_CONTENT") {
            Ok(raw) if !raw.trim().is_empty() => serde_json::from_str::<Value>(&raw)
                .map_err(|error| format!("invalid OPENCODE_CONFIG_CONTENT: {error}"))?,
            _ => json!({}),
        };
        let config = config
            .as_object_mut()
            .ok_or("OPENCODE_CONFIG_CONTENT must be a JSON object")?;
        let permission = config.entry("permission").or_insert_with(|| json!({}));
        if let Some(permission) = permission.as_object_mut() {
            // OpenCode does not forward permission requests raised by its internal
            // subagent sessions over ACP, so its default "ask" deadlocks the parent.
            permission
                .entry("external_directory")
                .or_insert_with(|| Value::String("allow".to_owned()));
        }
        serde_json::to_string(config)
            .map_err(|error| format!("failed to serialize OpenCode configuration: {error}"))
    }

    async fn exported_tool(
        &self,
        session_id: &str,
        workspace_root: &str,
        tool_call_id: &str,
        require_usage: bool,
        require_complete_input: bool,
        require_result: bool,
    ) -> Option<(Value, Value)> {
        const EXPORT_ATTEMPTS: usize = 30;
        for attempt in 0..EXPORT_ATTEMPTS {
            let mut command = tokio::process::Command::new(self.command());
            command
                .args(["export", session_id])
                .current_dir(workspace_root)
                .kill_on_drop(true);
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(5), command.output()).await;
            if let Ok(Ok(output)) = output
                && output.status.success()
                && let Ok(export) = serde_json::from_slice::<Value>(&output.stdout)
                && let Some(messages) = export.get("messages").and_then(Value::as_array)
                && let Some(found) = messages.iter().find_map(|message| {
                    message
                        .get("parts")
                        .and_then(Value::as_array)?
                        .iter()
                        .find(|part| {
                            part.get("callID").and_then(Value::as_str) == Some(tool_call_id)
                        })
                        .map(|part| {
                            (
                                message.get("info").cloned().unwrap_or(Value::Null),
                                part.clone(),
                            )
                        })
                })
            {
                let input = found.1.get("state").and_then(|state| state.get("input"));
                let tool_name = found.1.get("tool").and_then(Value::as_str);
                let complete_input = input.is_some_and(|input| {
                    let nonempty = input.as_object().is_some_and(|object| !object.is_empty());
                    nonempty
                        && (!matches!(tool_name, Some("bash" | "shell"))
                            || input.get("command").is_some()
                            || input.get("cmd").is_some())
                });
                let state = found.1.get("state");
                let complete_result = state.is_some_and(|state| {
                    matches!(
                        state.get("status").and_then(Value::as_str),
                        Some("completed" | "error")
                    ) && (state.get("output").is_some() || state.get("error").is_some())
                });
                if (!require_usage
                    || opencode_token_usage(&found.0).is_some_and(|usage| usage.total_tokens > 0))
                    && (!require_complete_input || complete_input)
                    && (!require_result || complete_result)
                {
                    return Some(found);
                }
            }
            if attempt + 1 < EXPORT_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        tracing::warn!(
            session_id,
            tool_call_id,
            "OpenCode session export did not contain the tool call"
        );
        None
    }
}

impl AcpAgentAdapter for OpenCodeAdapter {
    fn id(&self) -> AcpAdapterId {
        AcpAdapterId::Opencode
    }

    fn display_name(&self) -> &str {
        "OpenCode"
    }

    fn backend_kind(&self) -> BackendKind {
        BackendKind::Opencode
    }

    fn agent_name(&self) -> &str {
        "opencode"
    }

    fn resolve_roots<'a>(
        &'a self,
        workspace_roots: &'a [String],
        _ssh_host: Option<&'a str>,
        _kind: AcpSessionKind,
    ) -> BoxFuture<'a, Result<AcpSessionRoots, String>> {
        Box::pin(async move {
            let scope_root = pick_local_workspace_root(workspace_roots, "OpenCode")?;
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
            return Err("OpenCode does not yet support Tyde SSH sessions".to_owned());
        }
        let command = self.spec.command.trim();
        let program = if command.is_empty() {
            "opencode"
        } else {
            command
        };
        let args = if self.spec.args.is_empty() {
            vec!["acp"]
        } else {
            self.spec.args.iter().map(String::as_str).collect()
        };
        Ok(AcpSpawnSpec::new("OpenCode ACP", program, &args)
            .with_local_cwd(roots.session_cwd.clone())
            .with_local_env("OPENCODE_CONFIG_CONTENT", self.config_content()?))
    }

    fn delete_session<'a>(
        &'a self,
        session_id: &'a str,
        ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if ssh_host.is_some() {
                return Err("OpenCode does not yet support Tyde SSH sessions".to_owned());
            }
            let output = tokio::process::Command::new("opencode")
                .args(["session", "delete", session_id])
                .output()
                .await
                .map_err(|error| format!("failed to delete OpenCode session: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "failed to delete OpenCode session: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
    }

    fn normalize_tool_name<'a>(
        &self,
        tool_name: &'a str,
        _args: &Value,
        _params: &Value,
    ) -> Cow<'a, str> {
        for prefix in ["tyde-agent-control_", "tyde-agent-await_"] {
            if let Some(name) = tool_name.strip_prefix(prefix) {
                return Cow::Owned(name.to_owned());
            }
        }
        Cow::Borrowed(tool_name)
    }

    fn map_tool_request<'a>(
        &'a self,
        kind: &'a str,
        args: &'a Value,
        workspace_root: &'a str,
    ) -> BoxFuture<'a, Value> {
        Box::pin(async move {
            if let Some(tool_type) =
                crate::backend::agent_control_progress::tyde_tool_request_type(kind, args)
                && let Ok(value) = serde_json::to_value(tool_type)
            {
                return value;
            }
            if kind == "task" || kind == "think" || args.get("subagent_type").is_some() {
                return json!({
                    "kind": "AgentSpawn",
                    "prompt": args.get("prompt").and_then(Value::as_str),
                    "name": args.get("description").and_then(Value::as_str),
                    "execution_mode": "foreground",
                });
            }
            if matches!(kind, "websearch" | "web_search") {
                return json!({
                    "kind": "WebSearch",
                    "query": args.get("query").and_then(Value::as_str),
                });
            }
            if kind == "edit"
                && let Some(file_path) = args
                    .get("filePath")
                    .or_else(|| args.get("file_path"))
                    .and_then(Value::as_str)
                && let Some(after) = args.get("content").and_then(Value::as_str)
            {
                return json!({
                    "kind": "ModifyFile",
                    "file_path": file_path,
                    "before": "",
                    "after": after,
                });
            }
            super::super::tools::default_map_tool_request(kind, args, workspace_root).await
        })
    }

    fn map_tool_result(
        &self,
        completion: &crate::backend::acp::AcpToolCallCompletion,
        request_payload: Option<&Value>,
    ) -> Value {
        if completion.is_mcp_tool || !completion.success {
            return super::super::backend::map_tool_completion_result(completion, request_payload);
        }
        match request_payload
            .and_then(|payload| payload.get("kind"))
            .and_then(Value::as_str)
        {
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
                let metadata = completion.tool_result.get("metadata");
                let exit_code = metadata
                    .and_then(|value| value.get("exit"))
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                let stdout = completion
                    .tool_result
                    .get("output")
                    .or_else(|| metadata.and_then(|value| value.get("output")))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "kind": "RunCommand",
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": "",
                })
            }
            Some("ReadFiles") => {
                let bytes = completion
                    .tool_result
                    .get("metadata")
                    .and_then(|value| value.get("preview"))
                    .or_else(|| completion.tool_result.get("output"))
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or_default();
                let files = request_payload
                    .and_then(|payload| payload.get("file_paths"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|path| json!({ "path": path, "bytes": bytes }))
                    .collect::<Vec<_>>();
                json!({ "kind": "ReadFiles", "files": files })
            }
            Some("WebSearch") => json!({ "kind": "WebSearch" }),
            Some("ViewImage") => json!({ "kind": "ViewImage" }),
            _ => json!({
                "kind": "Other",
                "result": completion.tool_result,
            }),
        }
    }

    fn map_usage(&self, raw: Option<&Value>) -> Option<MessageTokenUsage> {
        Some(MessageTokenUsage::request_known(opencode_token_usage(
            raw?,
        )?))
    }

    fn usage_for_tool_completion<'a>(
        &'a self,
        session_id: &'a str,
        workspace_root: &'a str,
        tool_call_id: &'a str,
    ) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let (info, _) = self
                .exported_tool(session_id, workspace_root, tool_call_id, true, false, false)
                .await?;
            Some(json!({
                "usage": opencode_token_usage(&info)?,
                "_tyde_provider_message_id": info.get("id").and_then(Value::as_str),
            }))
        })
    }

    fn args_for_tool_request<'a>(
        &'a self,
        session_id: &'a str,
        workspace_root: &'a str,
        tool_call_id: &'a str,
    ) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let (_, part) = self
                .exported_tool(session_id, workspace_root, tool_call_id, false, true, false)
                .await?;
            part.get("state")?.get("input").cloned()
        })
    }

    fn result_for_tool_completion<'a>(
        &'a self,
        session_id: &'a str,
        workspace_root: &'a str,
        tool_call_id: &'a str,
    ) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let (_, part) = self
                .exported_tool(session_id, workspace_root, tool_call_id, false, false, true)
                .await?;
            let state = part.get("state")?;
            Some(json!({
                "content": state
                    .get("output")
                    .or_else(|| state.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "isError": state.get("status").and_then(Value::as_str) == Some("error"),
                "_meta": state.get("metadata").cloned().unwrap_or(Value::Null),
            }))
        })
    }

    fn provider_message_id_for_tool<'a>(
        &'a self,
        session_id: &'a str,
        workspace_root: &'a str,
        tool_call_id: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let (info, _) = self
                .exported_tool(
                    session_id,
                    workspace_root,
                    tool_call_id,
                    false,
                    false,
                    false,
                )
                .await?;
            info.get("id").and_then(Value::as_str).map(str::to_owned)
        })
    }
}

fn opencode_token_usage(raw: &Value) -> Option<TokenUsage> {
    let source = raw
        .get("usage")
        .or_else(|| raw.get("tokens"))
        .unwrap_or(raw);
    let cache = source.get("cache");
    let input_tokens = source
        .get("inputTokens")
        .or_else(|| source.get("input_tokens"))
        .or_else(|| source.get("input"))
        .and_then(Value::as_u64)?;
    let output_tokens = source
        .get("outputTokens")
        .or_else(|| source.get("output_tokens"))
        .or_else(|| source.get("output"))
        .and_then(Value::as_u64)?;
    let cached_prompt_tokens = source
        .get("cachedReadTokens")
        .or_else(|| source.get("cached_prompt_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| cache?.get("read")?.as_u64());
    let cache_creation_input_tokens = source
        .get("cachedWriteTokens")
        .or_else(|| source.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| cache?.get("write")?.as_u64());
    let reasoning_tokens = source
        .get("thoughtTokens")
        .or_else(|| source.get("reasoning_tokens"))
        .or_else(|| source.get("reasoning"))
        .and_then(Value::as_u64);
    let total_tokens = source
        .get("totalTokens")
        .or_else(|| source.get("total_tokens"))
        .or_else(|| source.get("total"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            input_tokens
                .saturating_add(output_tokens)
                .saturating_add(cached_prompt_tokens.unwrap_or(0))
                .saturating_add(cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(reasoning_tokens.unwrap_or(0))
        });
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_prompt_tokens,
        cache_creation_input_tokens,
        reasoning_tokens,
    })
}
