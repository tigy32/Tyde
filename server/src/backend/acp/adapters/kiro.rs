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
use serde_json::Value;

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
                    backend_kind: BackendKind::Acp,
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
            let params = serde_json::json!({ "kind": kind });
            kiro_impl::map_tool_request_type(&params, args, workspace_root).await
        })
    }

    fn map_tool_result(
        &self,
        completion: &crate::backend::acp::AcpToolCallCompletion,
        request_payload: Option<&Value>,
    ) -> Value {
        kiro_impl::map_tool_completion_result(completion, request_payload)
    }

    fn extra_env(&self) -> HashMap<String, String> {
        self.spec
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn adapter() -> KiroAdapter {
        KiroAdapter::new(AcpAgentSpec {
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            adapter: AcpAdapterId::Kiro,
        })
    }

    fn auth_method(id: &str, description: Option<&str>) -> AcpAuthMethod {
        AcpAuthMethod {
            id: id.to_string(),
            name: Some("Kiro Login".to_string()),
            description: description.map(str::to_string),
        }
    }

    #[test]
    fn kiro_login_is_external_setup_with_provider_instruction() {
        let handling = adapter().auth_method_handling(&auth_method(
            KIRO_LOGIN_METHOD_ID,
            Some("Run 'kiro-cli login' in terminal to authenticate. See https://kiro.dev/docs"),
        ));

        assert_eq!(
            handling,
            AcpAuthMethodHandling::ExternalSetup {
                instruction:
                    "Run 'kiro-cli login' in terminal to authenticate. See https://kiro.dev/docs"
                        .to_string(),
            }
        );
    }

    #[test]
    fn kiro_login_without_description_uses_actionable_fallback() {
        let handling = adapter().auth_method_handling(&auth_method(KIRO_LOGIN_METHOD_ID, None));

        assert_eq!(
            handling,
            AcpAuthMethodHandling::ExternalSetup {
                instruction: KIRO_LOGIN_FALLBACK_INSTRUCTION.to_string(),
            }
        );
    }

    #[test]
    fn kiro_login_provider_instruction_is_single_line_and_bounded() {
        let description = format!("Run login.\n{}", "x".repeat(600));
        let handling =
            adapter().auth_method_handling(&auth_method(KIRO_LOGIN_METHOD_ID, Some(&description)));
        let AcpAuthMethodHandling::ExternalSetup { instruction } = handling else {
            panic!("kiro-login must require external setup");
        };

        assert_eq!(instruction.chars().count(), KIRO_AUTH_INSTRUCTION_MAX_CHARS);
        assert!(!instruction.chars().any(char::is_control));
        assert!(instruction.starts_with("Run login. "));
    }

    #[test]
    fn unknown_kiro_auth_method_remains_protocol_authentication() {
        assert_eq!(
            adapter().auth_method_handling(&auth_method("future-method", None)),
            AcpAuthMethodHandling::ProtocolAuthenticate
        );
    }

    #[test]
    fn stock_adapter_keeps_protocol_authentication_default() {
        let stock = crate::backend::acp::adapters::stock::StockAdapter::new(AcpAgentSpec {
            command: "stock-agent".to_string(),
            args: vec!["acp".to_string()],
            cwd: None,
            env: BTreeMap::new(),
            adapter: AcpAdapterId::Stock,
        });

        assert_eq!(
            stock.auth_method_handling(&auth_method(KIRO_LOGIN_METHOD_ID, None)),
            AcpAuthMethodHandling::ProtocolAuthenticate
        );
    }

    #[test]
    fn legacy_notification_family_maps_onto_standard_discriminants() {
        let a = adapter();
        let normalized = a
            .normalize_notification(
                "session/notification",
                &serde_json::json!({ "type": "agent_message_chunk" }),
            )
            .expect("recognized");
        assert_eq!(normalized.session_update, "agent_message_chunk");
    }

    #[test]
    fn standard_session_update_is_left_to_the_generic_dispatch() {
        let a = adapter();
        assert!(
            a.normalize_notification("session/update", &serde_json::json!({ "type": "whatever" }))
                .is_none(),
            "the adapter must not intercept the standard notification method"
        );
    }

    #[test]
    fn unknown_legacy_type_is_ignored_rather_than_guessed() {
        let a = adapter();
        assert!(
            a.normalize_notification(
                "session/notification",
                &serde_json::json!({ "type": "some_future_thing" })
            )
            .is_none()
        );
    }

    #[test]
    fn prompt_decoration_carries_model_and_mode() {
        let a = adapter();
        let caps = crate::backend::acp::adapter::AcpCapabilities::default();
        let mut params = serde_json::json!({ "sessionId": "s1" });
        a.decorate_prompt(
            &mut params,
            &AcpRequestCtx {
                session_id: "s1",
                model: Some("claude-sonnet"),
                mode: Some("default"),
                system_prompt: None,
                capabilities: &caps,
            },
        );
        assert_eq!(params["modelId"], "claude-sonnet");
        assert_eq!(params["modeId"], "default");
        assert!(
            params.get("systemPrompt").is_none(),
            "absent steering must not become an empty systemPrompt"
        );
    }

    #[test]
    fn blank_steering_is_not_sent_as_a_system_prompt() {
        let a = adapter();
        let caps = crate::backend::acp::adapter::AcpCapabilities::default();
        let mut params = serde_json::json!({});
        a.decorate_session_new(
            &mut params,
            &AcpRequestCtx {
                session_id: "s1",
                model: None,
                mode: None,
                system_prompt: Some("   "),
                capabilities: &caps,
            },
        );
        assert!(params.get("systemPrompt").is_none());
    }
}
