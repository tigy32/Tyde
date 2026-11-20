use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use protocol::{AcpAdapterId, AcpAgentSpec, BackendKind, SlashCommand};
use serde_json::{Value, json};
use tyde_process_env::launch::WorkspacePath;

use crate::backend::acp::AcpSpawnSpec;
use crate::backend::acp::adapter::{
    AcpAgentAdapter, AcpRequestCtx, AcpRequestUsageSnapshot, AcpRequestUsageWait, AcpSessionKind,
    AcpSessionRoots, AcpSlashCommandCtx, AcpSlashCommandPlan,
};
use crate::backend::antigravity::{
    AntigravitySkillProjection, antigravity_mcp_tool_result,
    antigravity_provider_message_id_for_tool, antigravity_request_usages,
    antigravity_user_message_images, prepare_antigravity_skills, prepare_antigravity_tool_policy,
    resolve_workspace_roots,
};
use crate::backend::antigravity_stream::{
    MCP_DISPATCH_TOOL, exit_code_from_result, mcp_inner_call, tool_request_type,
};

/// CLI commands that change global configuration `agy` shares across every
/// process; Tyde owns model, thinking level, mode, and permissions per session.
const ANTIGRAVITY_CONFIGURATION_COMMANDS: &[&str] =
    &["config", "effort", "hooks", "model", "permissions"];

/// Read-only session commands answered locally by Tyde because `agy acp`'s
/// `available_commands_update` advertises only prompt-expanding commands.
const ANTIGRAVITY_TYDE_COMMANDS: &[(&str, &str)] = &[
    ("status", "Show current session status"),
    ("models", "List available models"),
];

pub struct AntigravityAdapter {
    spec: AcpAgentSpec,
    skill_projection: Option<Arc<AntigravitySkillProjection>>,
    tool_policy_projection: Option<Arc<tempfile::TempDir>>,
    startup_warning: Option<String>,
    excludes_ask_user: bool,
    storage_root: Option<PathBuf>,
}

impl AntigravityAdapter {
    pub fn new(spec: AcpAgentSpec) -> Self {
        Self {
            spec,
            skill_projection: None,
            tool_policy_projection: None,
            startup_warning: None,
            excludes_ask_user: false,
            storage_root: None,
        }
    }

    pub(in crate::backend::acp) fn for_session(
        spec: AcpAgentSpec,
        config: &crate::backend::ResolvedSpawnConfig,
        storage_root: Option<&Path>,
    ) -> Result<Self, String> {
        let skill_setup = prepare_antigravity_skills(&config.skills)?;
        let tool_policy_projection =
            prepare_antigravity_tool_policy(&config.excluded_tool_categories)?;
        Ok(Self {
            spec,
            skill_projection: skill_setup.projection.map(Arc::new),
            tool_policy_projection: tool_policy_projection.map(Arc::new),
            startup_warning: skill_setup.degraded_notice,
            excludes_ask_user: config
                .excluded_tool_categories
                .contains(&protocol::ToolCategory::AskUser),
            storage_root: storage_root.map(Path::to_path_buf),
        })
    }

    fn projection_dirs(&self) -> Result<Vec<String>, String> {
        let mut dirs = Vec::new();
        if let Some(projection) = self.skill_projection.as_ref() {
            dirs.push(WorkspacePath::host(projection.path())?.execution_string()?);
        }
        if let Some(projection) = self.tool_policy_projection.as_ref() {
            dirs.push(WorkspacePath::host(projection.path())?.execution_string()?);
        }
        Ok(dirs)
    }

    fn decorate_session_params(
        &self,
        params: &mut Value,
        ctx: &AcpRequestCtx<'_>,
    ) -> Result<(), String> {
        let root = params
            .as_object_mut()
            .ok_or_else(|| "ACP session parameters must be a JSON object".to_owned())?;
        if let Some(prompt) = ctx
            .system_prompt
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        {
            let meta = root
                .entry("_meta")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| "ACP session _meta must be a JSON object".to_owned())?;
            let agy = meta
                .entry("antigravity")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| "ACP _meta.antigravity must be a JSON object".to_owned())?;
            agy.insert("systemPrompt".to_owned(), json!({ "append": prompt }));
        }
        let mut extra_dirs = Vec::new();
        for workspace_root in ctx.workspace_roots.iter().skip(1) {
            let path = WorkspacePath::host(workspace_root)?.execution_string()?;
            if !extra_dirs.contains(&path) {
                extra_dirs.push(path);
            }
        }
        for dir in self.projection_dirs()? {
            if !extra_dirs.contains(&dir) {
                extra_dirs.push(dir);
            }
        }
        if !extra_dirs.is_empty() {
            root.insert("additionalDirectories".to_owned(), json!(extra_dirs));
        }
        Ok(())
    }
}

impl AcpAgentAdapter for AntigravityAdapter {
    fn id(&self) -> AcpAdapterId {
        AcpAdapterId::Antigravity
    }

    fn display_name(&self) -> &str {
        "Antigravity"
    }

    fn backend_kind(&self) -> BackendKind {
        BackendKind::Antigravity
    }

    fn agent_name(&self) -> &str {
        "antigravity"
    }

    fn client_capabilities(&self) -> Value {
        // Antigravity ACP executes its own native read/edit/write/run_command
        // tools with structured diffs and exit codes when the client does not
        // intercept fs/terminal calls.
        json!({})
    }

    fn delivers_system_prompt_natively(&self) -> bool {
        true
    }

    fn storage_root(&self) -> Option<&Path> {
        self.storage_root.as_deref()
    }

    fn decorate_session_new(
        &self,
        params: &mut Value,
        ctx: &AcpRequestCtx<'_>,
    ) -> Result<(), String> {
        self.decorate_session_params(params, ctx)
    }

    fn decorate_session_load(
        &self,
        params: &mut Value,
        ctx: &AcpRequestCtx<'_>,
    ) -> Result<(), String> {
        self.decorate_session_params(params, ctx)
    }

    fn resolve_roots<'a>(
        &'a self,
        workspace_roots: &'a [String],
        _kind: AcpSessionKind,
    ) -> BoxFuture<'a, Result<AcpSessionRoots, String>> {
        Box::pin(async move {
            let (primary, _) = resolve_workspace_roots(workspace_roots)?;
            Ok(AcpSessionRoots {
                session_cwd: primary.clone(),
                scope_root: primary,
            })
        })
    }

    fn spawn_spec(&self, roots: &AcpSessionRoots) -> Result<AcpSpawnSpec, String> {
        let command = self.spec.command.trim();
        let program = if command.is_empty() { "agy" } else { command };
        let mut owned_args: Vec<String> = if self.spec.args.is_empty() {
            vec![
                "acp".to_owned(),
                "--dangerously-skip-permissions".to_owned(),
            ]
        } else {
            self.spec.args.clone()
        };
        if !owned_args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
        {
            owned_args.push("--dangerously-skip-permissions".to_owned());
        }
        for dir in self.projection_dirs()? {
            owned_args.push("--add-dir".to_owned());
            owned_args.push(dir);
        }
        let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        Ok(AcpSpawnSpec::new("Antigravity ACP", program, &arg_refs)
            .with_local_cwd(roots.session_cwd.clone()))
    }

    fn normalize_slash_commands(&self, commands: Vec<SlashCommand>) -> Vec<SlashCommand> {
        let mut normalized: Vec<SlashCommand> = commands
            .into_iter()
            .filter(|command| !ANTIGRAVITY_CONFIGURATION_COMMANDS.contains(&command.name.as_str()))
            .collect();
        for (name, description) in ANTIGRAVITY_TYDE_COMMANDS {
            if normalized.iter().any(|command| command.name == *name) {
                continue;
            }
            normalized.push(SlashCommand {
                name: (*name).to_owned(),
                description: Some((*description).to_owned()),
                input_hint: None,
            });
        }
        normalized
    }

    fn plan_slash_command(
        &self,
        command: &SlashCommand,
        _message: &str,
        ctx: &AcpSlashCommandCtx<'_>,
    ) -> AcpSlashCommandPlan {
        match command.name.as_str() {
            "status" => AcpSlashCommandPlan::Reply(render_antigravity_status(ctx)),
            "models" => AcpSlashCommandPlan::Reply(render_antigravity_models(ctx)),
            _ => AcpSlashCommandPlan::Prompt,
        }
    }

    fn normalize_tool_name<'a>(
        &self,
        tool_name: &'a str,
        args: &Value,
        params: &Value,
    ) -> Cow<'a, str> {
        let raw_name = params
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(tool_name);
        if raw_name == MCP_DISPATCH_TOOL {
            let (inner_name, _) = mcp_inner_call(args);
            if !inner_name.is_empty() {
                return Cow::Owned(inner_name);
            }
        }
        if let Some(rest) = raw_name.strip_prefix("mcp_")
            && let Some(tyde_index) = rest.find("tyde_")
        {
            let candidate = &rest[tyde_index..];
            if crate::backend::agent_control_progress::tyde_tool_request_type(candidate, args)
                .is_some()
            {
                return Cow::Owned(candidate.to_owned());
            }
        }
        if raw_name != tool_name {
            Cow::Owned(raw_name.to_owned())
        } else {
            Cow::Borrowed(tool_name)
        }
    }

    fn is_mcp_tool(
        &self,
        tool_name: &str,
        params: &Value,
        servers: &[crate::backend::StartupMcpServer],
    ) -> bool {
        let raw_name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(tool_name);
        if raw_name == MCP_DISPATCH_TOOL {
            return true;
        }
        servers.iter().any(|server| {
            let exact = format!("mcp_{}_", server.name);
            let sanitized = format!(
                "mcp_{}_",
                server
                    .name
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .collect::<String>()
            );
            raw_name.starts_with(&exact)
                || raw_name.starts_with(&sanitized)
                || tool_name.starts_with(&exact)
                || tool_name.starts_with(&sanitized)
        })
    }

    fn unwrap_mcp_tool_args(
        &self,
        _tool_name: &str,
        args: &Value,
        params: &Value,
    ) -> Option<Value> {
        if params.get("name").and_then(Value::as_str) == Some(MCP_DISPATCH_TOOL) {
            return Some(mcp_inner_call(args).1);
        }
        None
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
            if self.excludes_ask_user && kind == "ask_question" {
                return json!({ "kind": "Other", "args": args });
            }
            if kind == "invoke_subagent"
                && args.get("TypeName").and_then(Value::as_str) == Some("image-generator")
            {
                return json!({
                    "kind": "GenerateImage",
                    "prompt": args.get("Prompt").or_else(|| args.get("prompt")),
                });
            }
            if matches!(kind, "read_file" | "view_file")
                && let Some(path) = args
                    .get("Path")
                    .or_else(|| args.get("AbsolutePath"))
                    .or_else(|| args.get("path"))
                    .and_then(Value::as_str)
                && [".png", ".jpg", ".jpeg", ".gif", ".webp"]
                    .iter()
                    .any(|ext| path.to_ascii_lowercase().ends_with(ext))
            {
                return json!({ "kind": "ViewImage", "path": path });
            }
            if kind == "write_file"
                && let Some(path) = args.get("Path").and_then(Value::as_str)
            {
                return json!({
                    "kind": "ModifyFile",
                    "file_path": path,
                    "before": "",
                    "after": args.get("Content").and_then(Value::as_str).unwrap_or_default(),
                });
            }
            if kind == "run_command" {
                let command = args
                    .get("CommandLine")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let cwd = args
                    .get("Cwd")
                    .and_then(Value::as_str)
                    .filter(|cwd| !cwd.is_empty())
                    .unwrap_or(workspace_root);
                return json!({
                    "kind": "RunCommand",
                    "command": command,
                    "working_directory": cwd,
                });
            }
            let mapped = tool_request_type(kind, args, true);
            serde_json::to_value(mapped).expect("ToolRequestType serialization is infallible")
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
            match crate::backend::agent_control_progress::tyde_tool_result(tool_name, &mapped) {
                Ok(Some(result)) => match serde_json::to_value(result) {
                    Ok(value) => return value,
                    Err(error) => {
                        return json!({
                            "kind": "Error",
                            "short_message": format!("Failed to serialize {tool_name} result"),
                            "detailed_message": error.to_string(),
                        });
                    }
                },
                Ok(None) => return mapped,
                Err(error) => {
                    return json!({
                        "kind": "Error",
                        "short_message": format!("Invalid {tool_name} result"),
                        "detailed_message": error.to_string(),
                    });
                }
            }
        }
        if completion.is_mcp_tool {
            return crate::backend::acp::backend::map_tool_completion_result(
                completion,
                request_payload,
            );
        }
        if !completion.success {
            return super::super::tools::error_result(completion);
        }
        match request_kind {
            Some("RunCommand") => {
                let Some(raw_output) = completion.tool_result.get("output").and_then(Value::as_str)
                else {
                    return json!({
                        "kind": "Error",
                        "short_message": "Antigravity RunCommand completion omitted output text",
                        "detailed_message": serde_json::to_string_pretty(&completion.tool_result)
                            .unwrap_or_else(|_| completion.tool_result.to_string()),
                    });
                };
                if let Some(exit_code) = exit_code_from_result(raw_output) {
                    let stdout = strip_agy_command_headers(raw_output);
                    return json!({
                        "kind": "RunCommand",
                        "exit_code": exit_code,
                        "stdout": stdout,
                        "stderr": "",
                    });
                }
                // `agy`'s `run_command` auto-backgrounds commands that exceed
                // `WaitMsBeforeAsync` (max 10s) and reports the task ID without
                // an exit code yet.
                if raw_output.contains("Tool is running as a background task") {
                    return json!({
                        "kind": "Other",
                        "result": completion.tool_result,
                    });
                }
                json!({
                    "kind": "Error",
                    "short_message": "Antigravity RunCommand completion omitted exit code",
                    "detailed_message": raw_output,
                })
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
            Some("ReadFiles") => {
                let files = request_payload
                    .and_then(|payload| payload.get("file_paths"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|path| json!({ "path": path, "bytes": Value::Null }))
                    .collect::<Vec<_>>();
                json!({ "kind": "ReadFiles", "files": files })
            }
            Some("WebSearch") => json!({ "kind": "WebSearch" }),
            Some("ViewImage") => json!({ "kind": "ViewImage" }),
            Some("Sleep") => json!({ "kind": "Sleep" }),
            Some("GenerateImage") => json!({
                "kind": "GenerateImage",
                "revised_prompt": Value::Null,
                "image_count": 1,
            }),
            _ => json!({
                "kind": "Other",
                "result": completion.tool_result,
            }),
        }
    }

    fn result_for_tool_completion<'a>(
        &'a self,
        session_id: &'a str,
        _workspace_root: &'a str,
        tool_call_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, String>> {
        Box::pin(async move {
            antigravity_mcp_tool_result(self.storage_root.as_deref(), session_id, tool_call_id)
                .await
                .map(Some)
        })
    }

    fn provider_message_id_for_tool<'a>(
        &'a self,
        session_id: &'a str,
        _workspace_root: &'a str,
        tool_call_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            antigravity_provider_message_id_for_tool(
                self.storage_root.as_deref(),
                session_id,
                tool_call_id,
            )
            .await
            .map(Some)
        })
    }

    fn request_usages_for_session<'a>(
        &'a self,
        session_id: &'a str,
        _workspace_root: &'a str,
        wait: Option<AcpRequestUsageWait>,
    ) -> BoxFuture<'a, Result<AcpRequestUsageSnapshot, String>> {
        Box::pin(async move {
            antigravity_request_usages(self.storage_root.as_deref(), session_id, wait).await
        })
    }

    fn user_message_images_for_replay<'a>(
        &'a self,
        session_id: &'a str,
        from_index: usize,
        expected_text: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<protocol::ImageData>, usize), String>> {
        Box::pin(async move {
            antigravity_user_message_images(
                self.storage_root.as_deref(),
                session_id,
                from_index,
                expected_text,
            )
            .await
        })
    }

    fn startup_warning(&self) -> Option<&str> {
        self.startup_warning.as_deref()
    }
}

fn strip_agy_command_headers(raw: &str) -> String {
    if let Some((_, output)) = raw.split_once("\nCommand output:\n") {
        return output.to_owned();
    }
    raw.lines()
        .skip_while(|line| {
            line.starts_with("Created At:")
                || line.starts_with("Completed At:")
                || line.starts_with("Command exited with code ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_antigravity_status(ctx: &AcpSlashCommandCtx<'_>) -> String {
    let mut lines = vec![
        "**Session**".to_owned(),
        format!("- Session: `{}`", ctx.session_id),
        format!("- Working directory: {}", ctx.workspace_root),
    ];
    if let Some(model) = ctx.model {
        lines.push(format!("- Model: {model}"));
    }
    if let Some(mode) = ctx.mode {
        lines.push(format!("- Mode: {mode}"));
    }
    lines.push(String::new());
    lines.push("**Token usage**".to_owned());
    if ctx.usage.total_tokens == 0 {
        lines.push("- No turns completed yet".to_owned());
    } else {
        lines.push(format!(
            "- Total: {} tokens ({} input, {} output)",
            ctx.usage.total_tokens, ctx.usage.input_tokens, ctx.usage.output_tokens
        ));
    }
    if let Some((input_tokens, context_window)) = ctx.context_usage.and_then(|usage| usage.known())
        && context_window > 0
    {
        let percent = input_tokens.saturating_mul(100) / context_window;
        lines.push(format!(
            "- Context: {input_tokens} / {context_window} tokens ({percent}%)"
        ));
    }
    lines.join("\n")
}

fn render_antigravity_models(ctx: &AcpSlashCommandCtx<'_>) -> String {
    if ctx.known_models.is_empty() {
        return "Antigravity has not reported a model list for this session.".to_owned();
    }
    let mut lines = vec!["**Available models**".to_owned()];
    for model in ctx.known_models {
        let Some(id) = model
            .get("id")
            .or_else(|| model.get("modelId"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let name = model
            .get("name")
            .or_else(|| model.get("displayName"))
            .and_then(Value::as_str)
            .filter(|name| *name != id)
            .map(|name| format!(" — {name}"))
            .unwrap_or_default();
        let current = if ctx.model == Some(id) {
            " (current)"
        } else {
            ""
        };
        lines.push(format!("- `{id}`{name}{current}"));
    }
    lines.join("\n")
}
