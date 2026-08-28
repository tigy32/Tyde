//! Kiro, driven through the generic ACP backend.
//!
//! Kiro predates parts of the specification and extends others, so it needs
//! more than [`super::stock::StockAdapter`]:
//!
//! * `kiro-cli-chat` is a companion binary that toolbox-style wrappers often
//!   fail to symlink, so it is resolved as a sibling of `kiro-cli`.
//! * Sessions live as JSON files under `~/.kiro/sessions/cli/`; Kiro does not
//!   implement `session/list`, and it does not check PID liveness before
//!   honoring a `.lock` file, so stale locks must be cleared before
//!   `session/load`.
//! * It emits a proprietary `session/notification` family alongside the
//!   standard `session/update`.
//! * Assistant text can carry terminal control sequences that must be stripped
//!   before it reaches a chat stream.
//! * `session/new` and `session/prompt` accept non-standard `systemPrompt`,
//!   `modelId`, and `modeId` fields.
//!
//! Everything else — the session lifecycle itself — is the generic backend's.

use std::borrow::Cow;
use std::collections::HashMap;

use futures_util::future::BoxFuture;
use protocol::{AcpAdapterId, AcpAgentSpec, BackendKind, SessionId};
use serde_json::{Value, json};

use crate::backend::BackendSession;
use crate::backend::acp::AcpSpawnSpec;
use crate::backend::acp::adapter::{
    AcpAgentAdapter, AcpAuthMethod, AcpAuthMethodHandling, AcpRequestCtx, AcpSessionKind,
    AcpSessionRoots, NormalizedUpdate,
};
use crate::backend::acp::backend as kiro_impl;

const KIRO_LOGIN_METHOD_ID: &str = "kiro-login";
const KIRO_LOGIN_FALLBACK_INSTRUCTION: &str =
    "Run 'kiro-cli login' in a terminal, then retry Kiro in Tyde.";
const KIRO_AUTH_INSTRUCTION_MAX_CHARS: usize = 512;

pub struct KiroAdapter {
    spec: AcpAgentSpec,
}

impl KiroAdapter {
    pub fn new(spec: AcpAgentSpec) -> Self {
        Self { spec }
    }

    /// Kiro's own working directories, which reserve scratch subdirectories
    /// for admin and ephemeral sessions so they stay out of the user's
    /// session list.
    fn session_is_hidden(cwd: &str) -> bool {
        cwd.contains(kiro_impl::KIRO_ADMIN_SESSION_SUBDIR)
            || cwd.contains(kiro_impl::KIRO_EPHEMERAL_SESSION_SUBDIR)
    }

    fn external_login_instruction(description: Option<&str>) -> String {
        let sanitized = description
            .unwrap_or_default()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>();
        let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            KIRO_LOGIN_FALLBACK_INSTRUCTION.to_string()
        } else {
            normalized
                .chars()
                .take(KIRO_AUTH_INSTRUCTION_MAX_CHARS)
                .collect()
        }
    }
}

impl AcpAgentAdapter for KiroAdapter {
    fn id(&self) -> AcpAdapterId {
        AcpAdapterId::Kiro
    }

    fn display_name(&self) -> &str {
        "Kiro"
    }

    fn auth_method_handling(&self, method: &AcpAuthMethod) -> AcpAuthMethodHandling {
        if method.id == KIRO_LOGIN_METHOD_ID {
            AcpAuthMethodHandling::ExternalSetup {
                instruction: Self::external_login_instruction(method.description.as_deref()),
            }
        } else {
            AcpAuthMethodHandling::ProtocolAuthenticate
        }
    }

    fn resolve_roots<'a>(
        &'a self,
        workspace_roots: &'a [String],
        ssh_host: Option<&'a str>,
        kind: AcpSessionKind,
    ) -> BoxFuture<'a, Result<AcpSessionRoots, String>> {
        Box::pin(async move {
            let roots = kiro_impl::resolve_kiro_session_roots(
                workspace_roots,
                ssh_host,
                kind.admin_session,
                kind.ephemeral,
            )
            .await?;
            Ok(AcpSessionRoots {
                session_cwd: roots.session_cwd,
                scope_root: roots.scope_root,
            })
        })
    }

    fn spawn_spec(
        &self,
        roots: &AcpSessionRoots,
        ssh_host: Option<&str>,
    ) -> Result<AcpSpawnSpec, String> {
        // An explicitly configured command wins, so a user can point the
        // built-in profile at a non-standard install; otherwise fall back to
        // sibling resolution from `kiro-cli`.
        let configured = self.spec.command.trim();
        let program = if configured.is_empty() {
            kiro_impl::resolve_kiro_chat_binary()
        } else {
            configured.to_string()
        };

        let args: Vec<&str> = if self.spec.args.is_empty() {
            vec!["acp"]
        } else {
            self.spec.args.iter().map(String::as_str).collect()
        };

        let mut spawn =
            AcpSpawnSpec::new("Kiro ACP", program, &args).with_local_cwd(roots.session_cwd.clone());
        if ssh_host.is_some() {
            spawn = spawn.with_remote_cwd(roots.session_cwd.clone());
        }
        Ok(spawn)
    }

    fn decorate_session_new(&self, params: &mut Value, ctx: &AcpRequestCtx<'_>) {
        // Kiro takes Tyde's combined system/steering instructions as a
        // non-standard `systemPrompt` on session creation.
        if let Some(system_prompt) = ctx.system_prompt.filter(|value| !value.trim().is_empty()) {
            params["systemPrompt"] = Value::String(system_prompt.to_string());
        }
    }

    fn decorate_prompt(&self, params: &mut Value, ctx: &AcpRequestCtx<'_>) {
        // Kiro accepts per-prompt model and mode selection rather than only
        // the session-scoped `session/set_model` and `session/set_mode`.
        if let Some(model) = ctx.model {
            params["modelId"] = Value::String(model.to_string());
        }
        if let Some(mode) = ctx.mode {
            params["modeId"] = Value::String(mode.to_string());
        }
        if let Some(system_prompt) = ctx.system_prompt.filter(|value| !value.trim().is_empty()) {
            params["systemPrompt"] = Value::String(system_prompt.to_string());
        }
    }

    fn normalize_notification(&self, method: &str, params: &Value) -> Option<NormalizedUpdate> {
        // Kiro's proprietary family carries the discriminant in `type` rather
        // than `sessionUpdate`, and omits the `update` envelope.
        if method != "session/notification" {
            return None;
        }
        let raw = params.get("type").and_then(Value::as_str)?;
        let session_update = match crate::backend::acp::normalize_update_type(raw).as_str() {
            "agentmessagechunk" => "agent_message_chunk",
            "toolcall" => "tool_call",
            "toolcallupdate" => "tool_call_update",
            "turnend" => "turn_end",
            "error" => "error",
            "currentmodeupdate" => "current_mode_update",
            "configoptionupdate" => "config_option_update",
            _ => return None,
        };
        Some(NormalizedUpdate {
            session_update,
            params: params.clone(),
        })
    }

    fn sanitize_stream_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let stripped = kiro_impl::strip_ansi_and_controls(text);
        if stripped == text {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(stripped)
        }
    }

    fn list_sessions<'a>(
        &'a self,
        ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<BackendSession>, String>> {
        Box::pin(async move {
            let raw = match ssh_host {
                Some(host) => kiro_impl::load_remote_kiro_sessions(host).await?,
                None => kiro_impl::load_local_kiro_sessions().await?,
            };

            let mut sessions = Vec::new();
            for (session_id, metadata) in raw {
                let cwd = metadata
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if Self::session_is_hidden(&cwd) {
                    continue;
                }
                let timestamp = kiro_impl::extract_session_timestamp(&metadata);
                sessions.push(BackendSession {
                    id: SessionId(session_id),
                    backend_kind: BackendKind::Kiro,
                    workspace_roots: if cwd.is_empty() {
                        Vec::new()
                    } else {
                        vec![cwd]
                    },
                    title: Some(kiro_impl::extract_session_title(&metadata)),
                    token_count: None,
                    created_at_ms: Some(timestamp),
                    updated_at_ms: Some(timestamp),
                    resumable: true,
                });
            }
            sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
            Ok(sessions)
        })
    }

    fn before_session_load<'a>(
        &'a self,
        session_id: &'a str,
        ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Kiro reads `.lock` files without checking whether the recorded
            // PID is alive, so a crashed session blocks `session/load`
            // forever. Clearing a stale lock is best-effort: a failure here
            // should not mask the load error that follows.
            let _ = match ssh_host {
                Some(host) => kiro_impl::clear_remote_kiro_session_lock(host, session_id).await,
                None => kiro_impl::clear_local_kiro_session_lock(session_id).await,
            };
            Ok(())
        })
    }

    fn delete_session<'a>(
        &'a self,
        session_id: &'a str,
        ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match ssh_host {
                Some(host) => kiro_impl::delete_remote_kiro_session(host, session_id).await,
                None => kiro_impl::delete_local_kiro_session(session_id).await,
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
            if kind == "search"
                && let Some(query) = args
                    .get("query")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
            {
                return json!({
                    "kind": "WebSearch",
                    "query": query,
                });
            }
            if kind == "read"
                && let Some(path) = kiro_single_image_path(args)
            {
                return json!({
                    "kind": "ViewImage",
                    "path": path,
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
        kiro_impl::map_tool_completion_result(completion, request_payload)
    }

    fn map_task_update(
        &self,
        completion: &crate::backend::acp::AcpToolCallCompletion,
        request_payload: Option<&Value>,
    ) -> Option<protocol::TaskList> {
        kiro_task_update(completion, request_payload)
    }

    fn extra_env(&self) -> HashMap<String, String> {
        self.spec
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }
}

fn kiro_single_image_path(args: &Value) -> Option<&str> {
    let paths = args
        .get("operations")?
        .as_array()?
        .iter()
        .filter(|operation| operation.get("mode").and_then(Value::as_str) == Some("Image"))
        .flat_map(|operation| {
            operation
                .get("image_paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let paths = paths.collect::<Vec<_>>();
    match paths.as_slice() {
        [path] => Some(*path),
        _ => None,
    }
}

fn kiro_task_update(
    completion: &crate::backend::acp::AcpToolCallCompletion,
    request_payload: Option<&Value>,
) -> Option<protocol::TaskList> {
    if !completion.success {
        return None;
    }

    let request_payload = request_payload?;
    if request_payload.get("kind").and_then(Value::as_str) != Some("Other") {
        return None;
    }
    let args = request_payload.get("args")?;
    let command = args.get("command").and_then(Value::as_str)?;
    let is_task_command = match command {
        "create" => args.get("task_list_description").is_some() && args.get("tasks").is_some(),
        "complete" => args.get("completed_task_ids").is_some(),
        "remove" => args.get("remove_task_ids").is_some(),
        _ => false,
    };
    if !is_task_command {
        return None;
    }

    let snapshot = completion
        .tool_result
        .get("items")?
        .as_array()?
        .first()?
        .get("Json")?;
    let raw_tasks = snapshot.get("tasks")?.as_array()?;
    let tasks = raw_tasks
        .iter()
        .map(|task| {
            let id = task
                .get("id")
                .and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok()))?;
            let description = task.get("task_description").and_then(Value::as_str)?.trim();
            if description.is_empty() {
                return None;
            }
            Some(protocol::Task {
                id,
                description: description.to_string(),
                status: if task
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    protocol::TaskStatus::Completed
                } else {
                    protocol::TaskStatus::Pending
                },
            })
        })
        .collect::<Option<Vec<_>>>()?;

    Some(protocol::TaskList {
        title: snapshot
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tasks,
    })
}
