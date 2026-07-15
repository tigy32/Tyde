use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use protocol::types::StreamIdentityViolation;
use protocol::{
    BackendAccessMode, CapacityBucket, CapacityBucketId, CapacityCoverage, CapacityMeasure,
    CapacityPlanLabel, CapacityReport, CapacityReset, CapacityScope, CapacitySource,
    CapacityUnavailableReason, CapacityWindow, ChatMessageId, CodexLimitSlot, ModelRequestId,
    ModelRequestTokenUsage, ModelTurnId, ServerGeneratedChatMessageIdOrigin,
    ServerGeneratedChatMessageIdentity, TokenUsage, TokenUsageUnavailableReason,
    ToolExecutionNormalizationFailure, ValueProvenance,
};

use crate::agent_control_mcp::AGENT_CONTROL_AWAIT_MCP_SERVER_NAME;
use crate::backend::agent_control_progress::{
    await_progress_data_for_tool, spawn_progress_data_for_tool_result, tyde_tool_request_type,
    tyde_tool_result,
};
use crate::backend::turn_emitter::{
    AgentName, MessageMetadataUpdatePayload, StreamEndPayload, ToolCompletedPayload, TurnEmitter,
};
use crate::backend::{
    BackendExecutionMode, BackendStartupError, SessionCommand, StartupMcpServer,
    StartupMcpTransport, render_combined_spawn_instructions,
};
use crate::process_env;
use crate::review_mcp::REVIEW_FEEDBACK_MCP_SERVER_NAME;
use crate::sub_agent::SubAgentEmitter;
use crate::subprocess::ImageAttachment;

const CODEX_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const CODEX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_AGENT_NAME: &str = "codex";
const CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT: u64 = 200_000;
// The entire GPT-5 family (gpt-5, gpt-5.x, their -codex and -mini variants)
// ships a 400k context window per OpenAI's model docs. `codex-mini-latest` is
// the lone exception at 200k. This is only a pre-first-turn fallback — once a
// turn reports `context_window` in token usage we use that instead.
const CODEX_ESTIMATED_CONTEXT_WINDOW_GPT5_FAMILY: u64 = 400_000;
const CODEX_ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const CODEX_MIN_SYSTEM_PROMPT_BYTES: u64 = 1_024;
const CODEX_FORCED_APPROVAL_POLICY: &str = "never";
const CODEX_INFERENCE_APPROVAL_POLICY: &str = "untrusted";
const CODEX_UNRESTRICTED_SANDBOX: &str = "danger-full-access";
// ReadOnly is advisory in Tyde; keep Codex writable enough for cargo target/.
const CODEX_READ_ONLY_SANDBOX: &str = "workspace-write";
const CODEX_INFERENCE_SANDBOX: &str = "read-only";
const CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS: bool = true;
const CODEX_REASONING_SUMMARY_LEVEL: &str = "detailed";

#[cfg(any(test, feature = "test-support"))]
static CODEX_TEST_APP_SERVER_BINARY: std::sync::OnceLock<
    std::sync::Mutex<Option<std::path::PathBuf>>,
> = std::sync::OnceLock::new();
#[cfg(test)]
static CODEX_TEST_NATIVE_HOME: std::sync::OnceLock<std::sync::Mutex<Option<std::path::PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
fn codex_test_app_server_binary_override() -> &'static std::sync::Mutex<Option<std::path::PathBuf>>
{
    CODEX_TEST_APP_SERVER_BINARY.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn codex_test_native_home_override() -> &'static std::sync::Mutex<Option<std::path::PathBuf>> {
    CODEX_TEST_NATIVE_HOME.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(feature = "test-support")]
pub struct CodexTestAppServerGuard {
    previous: Option<std::path::PathBuf>,
}

#[cfg(feature = "test-support")]
impl Drop for CodexTestAppServerGuard {
    fn drop(&mut self) {
        *codex_test_app_server_binary_override()
            .lock()
            .expect("codex test app-server binary mutex poisoned") = self.previous.take();
    }
}

#[cfg(feature = "test-support")]
pub fn install_test_app_server_binary(binary: std::path::PathBuf) -> CodexTestAppServerGuard {
    let previous = codex_test_app_server_binary_override()
        .lock()
        .expect("codex test app-server binary mutex poisoned")
        .replace(binary);
    CodexTestAppServerGuard { previous }
}

#[cfg(test)]
mod capacity_mapping_tests {
    use super::*;

    #[test]
    fn passive_notification_maps_complete_vendor_buckets_without_unit_conversion() {
        let report = map_passive_rate_limits_updated(&json!({
            "rateLimits": {
                "limitId": "subscription", "limitName": "subscription",
                "primary": {"usedPercent": 82, "windowDurationMins": 300, "resetsAt": 1_700_000_000},
                "secondary": {"usedPercent": 17, "windowDurationMins": 10_080, "resetsAt": 1_700_100_000},
                "credits": {"hasCredits": true, "unlimited": false, "balance": "12"},
                "individualLimit": true, "planType": "pro", "rateLimitReachedType": null
            }
        })).expect("complete passive notification");
        assert_eq!(
            report.coverage,
            protocol::CapacityCoverage::AllVendorBuckets
        );
        assert_eq!(
            report.plan.as_ref().map(|plan| plan.label.as_str()),
            Some("pro")
        );
        assert_eq!(report.buckets[0].label, "subscription primary limit");
        let protocol::CapacityMeasure::UsedPercent {
            used_percent,
            remaining_percent,
            provenance,
        } = &report.buckets[0].measure
        else {
            panic!("primary percent");
        };
        assert_eq!((*used_percent, *remaining_percent), (82, 18));
        assert!(provenance.vendor_reported);
        assert_eq!(
            report.buckets[0].measure.used_percent_provenance(),
            Some(protocol::PercentValueProvenance::VendorReported)
        );
        assert_eq!(
            report.buckets[0].measure.remaining_percent_provenance(),
            Some(protocol::PercentValueProvenance::DerivedComplement)
        );
        assert!(matches!(
            &report.buckets[2].measure,
            protocol::CapacityMeasure::Credits { .. }
        ));
    }

    #[test]
    fn incomplete_or_out_of_range_passive_notification_never_claims_full_coverage() {
        assert_eq!(
            map_passive_rate_limits_updated(&json!({"rateLimits": {}})),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
        let malformed = json!({"rateLimits": {
            "limitId":"x", "limitName":"x",
            "primary":{"usedPercent":101,"windowDurationMins":300,"resetsAt":1},
            "secondary":{"usedPercent":1,"windowDurationMins":300,"resetsAt":1},
            "credits":{"hasCredits":false,"unlimited":false,"balance":null},
            "individualLimit":false,"planType":null,"rateLimitReachedType":null
        }});
        assert_eq!(
            map_passive_rate_limits_updated(&malformed),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
    }

    #[test]
    fn malformed_credit_or_timestamp_fields_are_unavailable_without_raw_values() {
        let malformed = json!({"rateLimits": {
            "limitId":"x", "limitName":"x",
            "primary":{"usedPercent":1,"windowDurationMins":300,"resetsAt":1},
            "secondary":{"usedPercent":1,"windowDurationMins":300,"resetsAt":1},
            "credits":{"hasCredits":true,"unlimited":false,"balance":{"raw":"secret"}},
            "individualLimit":false,"planType":null,"rateLimitReachedType":null
        }});
        assert_eq!(
            map_passive_rate_limits_updated(&malformed),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
        let overflow = json!({"rateLimits": {
            "limitId":"x", "limitName":"x",
            "primary":{"usedPercent":1,"windowDurationMins":300,"resetsAt":18446744073709551615_u64},
            "secondary":{"usedPercent":1,"windowDurationMins":300,"resetsAt":1},
            "credits":{"hasCredits":true,"unlimited":false,"balance":null},
            "individualLimit":false,"planType":null,"rateLimitReachedType":null
        }});
        assert_eq!(
            map_passive_rate_limits_updated(&overflow),
            Err(protocol::CapacityUnavailableReason::MalformedReport)
        );
    }
}

fn codex_command() -> Command {
    #[cfg(any(test, feature = "test-support"))]
    {
        let override_path = codex_test_app_server_binary_override()
            .lock()
            .expect("codex test app-server binary mutex poisoned")
            .clone();
        if let Some(path) = override_path {
            return Command::new(path);
        }
    }

    Command::new("codex")
}

#[derive(Clone)]
pub struct CodexCommandHandle {
    inner: Arc<CodexInner>,
}

impl CodexCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        self.inner.execute(command).await
    }

    async fn update_runtime_settings(&self, settings: Value) -> Result<(), String> {
        self.inner.update_runtime_settings(settings).await
    }
}

pub struct CodexSession {
    inner: Arc<CodexInner>,
    // The thread/start or thread/fork response is the authoritative source for
    // this value. Keep it outside the event-state lock so parent-session
    // publication cannot be delayed by an early raw child notification.
    session_id: SessionId,
}

struct CodexThreadResponseConfig<'a> {
    startup_mcp_servers: &'a [StartupMcpServer],
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
}

struct CodexSessionSpawnOptions {
    ephemeral: bool,
    access_mode: BackendAccessMode,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    execution_mode: BackendExecutionMode,
}

impl CodexSession {
    pub async fn spawn(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            CodexSessionSpawnOptions {
                ephemeral: false,
                access_mode,
                subagent_emitter: None,
                execution_mode: BackendExecutionMode::Agent,
            },
        )
        .await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            CodexSessionSpawnOptions {
                ephemeral: true,
                access_mode,
                subagent_emitter: None,
                execution_mode: BackendExecutionMode::Agent,
            },
        )
        .await
    }

    pub async fn spawn_admin(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            CodexSessionSpawnOptions {
                ephemeral: true,
                access_mode,
                subagent_emitter: None,
                execution_mode: BackendExecutionMode::Agent,
            },
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        options: CodexSessionSpawnOptions,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let CodexSessionSpawnOptions {
            ephemeral,
            access_mode,
            subagent_emitter,
            execution_mode,
        } = options;
        let steering_tempfile = match steering_content {
            Some(content) if !content.trim().is_empty() => {
                Some(crate::steering::write_codex_steering_tempfile(content)?)
            }
            _ => None,
        };
        let (rpc, inbound_rx) = match CodexRpc::spawn(
            ssh_host.as_deref(),
            startup_mcp_servers,
            steering_tempfile.as_deref(),
            access_mode,
            execution_mode,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                remove_codex_steering_tempfile(&steering_tempfile);
                return Err(err);
            }
        };

        rpc.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "tyde",
                    "title": Value::Null,
                    "version": "0.1"
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )
        .await?;

        let cwd = if ssh_host.is_some() {
            // For remote sessions, extract the remote path (host already stripped)
            let parsed = crate::remote::parse_remote_workspace_roots(workspace_roots)?
                .ok_or("Expected remote workspace roots for SSH session")?;
            parsed
                .1
                .into_iter()
                .next()
                .ok_or("No remote workspace root found")?
        } else {
            pick_workspace_root(workspace_roots)?
        };

        let thread_started = rpc
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "sandbox": codex_sandbox_mode(access_mode, execution_mode),
                    "approvalPolicy": codex_approval_policy(execution_mode),
                    "ephemeral": ephemeral || execution_mode == BackendExecutionMode::InferenceOnly,
                    "experimentalRawEvents": CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS,
                    "persistExtendedHistory": false
                }),
            )
            .await?;

        Self::from_thread_response(
            rpc,
            inbound_rx,
            steering_tempfile,
            CodexThreadResponseConfig {
                startup_mcp_servers,
                access_mode,
                execution_mode,
            },
            thread_started,
            "thread/start",
            subagent_emitter,
        )
        .await
    }

    pub async fn fork(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
        from_thread_id: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let steering_tempfile = match steering_content {
            Some(content) if !content.trim().is_empty() => {
                Some(crate::steering::write_codex_steering_tempfile(content)?)
            }
            _ => None,
        };
        let (rpc, inbound_rx) = match CodexRpc::spawn(
            ssh_host.as_deref(),
            startup_mcp_servers,
            steering_tempfile.as_deref(),
            access_mode,
            BackendExecutionMode::Agent,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                remove_codex_steering_tempfile(&steering_tempfile);
                return Err(err);
            }
        };

        if let Err(err) = rpc
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "tyde",
                        "title": Value::Null,
                        "version": "0.1"
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }),
            )
            .await
        {
            cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
            return Err(err);
        }

        let cwd = if ssh_host.is_some() {
            let parsed = match crate::remote::parse_remote_workspace_roots(workspace_roots) {
                Ok(parsed) => parsed,
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
                    return Err(err);
                }
            };
            let Some((_, paths)) = parsed else {
                cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
                return Err("Expected remote workspace roots for SSH session".to_string());
            };
            let Some(path) = paths.into_iter().next() else {
                cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
                return Err("No remote workspace root found".to_string());
            };
            path
        } else {
            match pick_workspace_root(workspace_roots) {
                Ok(root) => root,
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
                    return Err(err);
                }
            }
        };

        let mut fork_params = json!({
            "threadId": from_thread_id,
            "cwd": cwd.clone(),
            "sandbox": codex_sandbox_mode(access_mode, BackendExecutionMode::Agent),
            "approvalPolicy": CODEX_FORCED_APPROVAL_POLICY,
            "ephemeral": false,
            "experimentalRawEvents": CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS,
            "persistExtendedHistory": false
        });
        fork_params["runtimeWorkspaceRoots"] =
            json!(codex_runtime_workspace_roots(workspace_roots, &cwd));

        let thread_forked = match rpc.request("thread/fork", fork_params).await {
            Ok(value) => value,
            Err(err) => {
                cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
                return Err(format!("Codex thread/fork failed: {err}"));
            }
        };
        if thread_forked
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .is_none()
        {
            cleanup_codex_startup_failure(rpc, &steering_tempfile).await;
            return Err("Codex thread/fork response missing thread.id".to_string());
        }

        Self::from_thread_response(
            rpc,
            inbound_rx,
            steering_tempfile,
            CodexThreadResponseConfig {
                startup_mcp_servers,
                access_mode,
                execution_mode: BackendExecutionMode::Agent,
            },
            thread_forked,
            "thread/fork",
            None,
        )
        .await
    }

    async fn from_thread_response(
        rpc: CodexRpc,
        inbound_rx: mpsc::UnboundedReceiver<CodexInbound>,
        steering_tempfile: Option<std::path::PathBuf>,
        config: CodexThreadResponseConfig<'_>,
        thread_response: Value,
        method: &str,
        subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let thread_id = thread_response
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Codex {method} response missing thread.id"))?
            .to_string();
        let session_id = SessionId(thread_id.clone());
        let generated_identity_epoch = codex_generated_identity_epoch(&thread_id);

        let model = thread_response
            .get("model")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let emitter = Arc::new(TurnEmitter::new_for_agent(
            event_tx,
            AgentName(CODEX_AGENT_NAME),
        ));

        let inner = Arc::new(CodexInner {
            rpc,
            emitter,
            state: Mutex::new(CodexState {
                thread_id,
                model,
                reasoning_effort: thread_response
                    .get("reasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                approval_policy: None,
                access_mode: config.access_mode,
                execution_mode: config.execution_mode,
                turn_network_access: codex_has_http_mcp_servers(config.startup_mcp_servers),
                active_turn_id: None,
                active_stream: None,
                completed_agent_messages: HashMap::new(),
                quarantined_turn_id: None,
                generated_identity_epoch,
                next_generated_identity_ordinal: 1,
                pending_tool_call_ids: HashSet::new(),
                close_active_stream_when_tools_idle: false,
                pending_message_metadata: None,
                completed_message_metadata_by_turn: HashMap::new(),
                token_usage_by_turn: HashMap::new(),
                model_token_usage_by_turn: HashMap::new(),
                turn_context_by_turn: HashMap::new(),
                file_change_call_ids: HashMap::new(),
                pending_request: None,
                pending_user_input_bytes: 0,
                conversation_bytes_total: 0,
                subagent_emitter,
                pending_subagent_spawns: HashMap::new(),
                conflicting_subagent_threads: HashMap::new(),
                registering_subagent_threads: HashSet::new(),
                unknown_owner_notifications: HashSet::new(),
                subagent_streams: HashMap::new(),
                completed_subagent_streams: HashMap::new(),
            }),
            steering_tempfile,
        });

        let forward_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut rx = inbound_rx;
            while let Some(msg) = rx.recv().await {
                forward_inner.handle_inbound(msg).await;
            }
        });

        Ok((Self { inner, session_id }, event_rx))
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    pub fn command_handle(&self) -> CodexCommandHandle {
        CodexCommandHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub async fn list_mcp_server_statuses(&self) -> Result<Value, String> {
        self.inner
            .rpc
            .request(
                "mcpServerStatus/list",
                json!({
                    "detail": "toolsAndAuthOnly",
                    "limit": 100
                }),
            )
            .await
    }

    pub async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<Value>,
        meta: Option<Value>,
    ) -> Result<Value, String> {
        let thread_id = {
            let state = self.inner.state.lock().await;
            state.thread_id.clone()
        };

        self.inner
            .rpc
            .request(
                "mcpServer/tool/call",
                json!({
                    "threadId": thread_id,
                    "server": server,
                    "tool": tool,
                    "arguments": arguments,
                    "_meta": meta
                }),
            )
            .await
    }

    pub(crate) async fn set_subagent_emitter(
        &self,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(), String> {
        let mut state = self.inner.state.lock().await;
        state.subagent_emitter = Some(emitter);
        Ok(())
    }

    pub async fn shutdown(self) {
        self.inner.rpc.shutdown().await;
        remove_codex_steering_tempfile(&self.inner.steering_tempfile);
    }
}

async fn cleanup_codex_startup_failure(
    rpc: CodexRpc,
    steering_tempfile: &Option<std::path::PathBuf>,
) {
    rpc.shutdown().await;
    remove_codex_steering_tempfile(steering_tempfile);
}

fn remove_codex_steering_tempfile(steering_tempfile: &Option<std::path::PathBuf>) {
    if let Some(path) = steering_tempfile
        && let Err(e) = std::fs::remove_file(path)
    {
        tracing::warn!(
            "Failed to remove steering temp file {}: {e}",
            path.display()
        );
    }
}

/// Maps only the verified passive app-server notification. This deliberately
/// consumes the already-open app-server notification.
pub(crate) fn map_passive_rate_limits_updated(
    params: &Value,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    let snapshot = params.get("rateLimits").unwrap_or(params);
    const REQUIRED: [&str; 8] = [
        "limitId",
        "limitName",
        "primary",
        "secondary",
        "credits",
        "individualLimit",
        "planType",
        "rateLimitReachedType",
    ];
    if !snapshot.is_object() || REQUIRED.iter().any(|field| snapshot.get(field).is_none()) {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    if snapshot.get("limitId").and_then(Value::as_str).is_none()
        || !matches!(snapshot.get("individualLimit"), Some(Value::Bool(_)))
    {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    let reached_scope = match snapshot.get("rateLimitReachedType") {
        Some(Value::Null) => CapacityScope::NotReported,
        Some(Value::String(value))
            if value.len() <= 128 && !value.chars().any(char::is_control) =>
        {
            if value.starts_with("workspace_") {
                CapacityScope::Workspace
            } else if value.starts_with("organization_") {
                CapacityScope::OrganizationSpend
            } else {
                CapacityScope::NotReported
            }
        }
        _ => return Err(CapacityUnavailableReason::MalformedReport),
    };
    let limit_name = snapshot
        .get("limitName")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let mut buckets = Vec::new();
    for (field, slot, label) in [
        ("primary", CodexLimitSlot::Primary, "primary limit"),
        ("secondary", CodexLimitSlot::Secondary, "secondary limit"),
    ] {
        let window = snapshot
            .get(field)
            .and_then(Value::as_object)
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let used = window
            .get("usedPercent")
            .and_then(Value::as_u64)
            .filter(|value| *value <= 100)
            .ok_or(CapacityUnavailableReason::MalformedReport)? as u8;
        let duration_minutes = window
            .get("windowDurationMins")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let reset = match window.get("resetsAt") {
            None | Some(Value::Null) => CapacityReset::NotReported,
            Some(value) => value
                .as_u64()
                .and_then(|seconds| seconds.checked_mul(1000))
                .map(|at_ms| CapacityReset::At { at_ms })
                .ok_or(CapacityUnavailableReason::MalformedReport)?,
        };
        buckets.push(CapacityBucket {
            id: CapacityBucketId::Codex { slot },
            label: format!("{limit_name} {label}"),
            measure: CapacityMeasure::UsedPercent {
                used_percent: used,
                remaining_percent: 100 - used,
                provenance: ValueProvenance {
                    vendor_reported: true,
                },
            },
            scope: if snapshot.get("individualLimit").and_then(Value::as_bool) == Some(true) {
                CapacityScope::Individual
            } else {
                CapacityScope::Account
            },
            window: CapacityWindow::Rolling { duration_minutes },
            reset,
            status: None,
        });
    }
    let credits = snapshot
        .get("credits")
        .and_then(Value::as_object)
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let has_credits = credits
        .get("hasCredits")
        .and_then(Value::as_bool)
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    let balance = match credits.get("balance") {
        None | Some(Value::Null) => None,
        Some(Value::String(value))
            if !value.is_empty() && value.len() <= 64 && !value.chars().any(char::is_control) =>
        {
            Some(value.clone())
        }
        Some(Value::Number(value)) => {
            let value = value.to_string();
            Some(
                (value.len() <= 64)
                    .then_some(value)
                    .ok_or(CapacityUnavailableReason::MalformedReport)?,
            )
        }
        Some(_) => return Err(CapacityUnavailableReason::MalformedReport),
    };
    buckets.push(CapacityBucket {
        id: CapacityBucketId::Codex {
            slot: CodexLimitSlot::Credits,
        },
        label: "credits".to_string(),
        measure: CapacityMeasure::Credits {
            has_credits,
            unlimited,
            balance,
        },
        scope: reached_scope,
        window: CapacityWindow::NotReported,
        reset: CapacityReset::NotReported,
        status: None,
    });
    Ok(CapacityReport {
        source: CapacitySource::CodexAccountRateLimitsUpdated,
        observed_at_ms: None,
        plan: match snapshot.get("planType") {
            Some(Value::Null) => None,
            Some(Value::String(label))
                if !label.is_empty()
                    && label.len() <= 128
                    && !label.chars().any(char::is_control) =>
            {
                Some(CapacityPlanLabel {
                    label: label.clone(),
                })
            }
            _ => return Err(CapacityUnavailableReason::MalformedReport),
        },
        buckets,
        coverage: CapacityCoverage::AllVendorBuckets,
    })
}

/// Route the verified passive notification through the emitter supplied by the
/// owning agent session. The adapter never discovers a host globally.
pub(crate) fn forward_passive_rate_limits_updated(params: &Value, emitter: &dyn SubAgentEmitter) {
    let state = match map_passive_rate_limits_updated(params) {
        Ok(report) => protocol::BackendCapacityState::Known { report },
        Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
    };
    emitter.on_backend_capacity(protocol::BackendKind::Codex, state);
}

pub(crate) async fn probe_session_settings_schema(
    program: Option<&str>,
) -> Result<SessionSettingsSchema, String> {
    let (rpc, _inbound_rx) = CodexRpc::spawn_with_local_program(
        None,
        &[],
        None,
        BackendAccessMode::Unrestricted,
        BackendExecutionMode::Agent,
        program,
    )
    .await
    .map_err(|err| format!("Codex model discovery failed to spawn app-server: {err}"))?;

    if let Err(err) = rpc
        .request(
            "initialize",
            json!({
                "clientInfo": { "name": "tyde", "title": Value::Null, "version": "0.1" },
                "capabilities": { "experimentalApi": true }
            }),
        )
        .await
    {
        return codex_probe_result_with_cleanup(
            Err(format!("Codex model discovery initialize failed: {err}")),
            rpc.terminate().await,
        );
    }

    let response = rpc
        .request("model/list", json!({ "includeHidden": false }))
        .await;
    let response = codex_probe_result_with_cleanup(
        response.map_err(|err| format!("Codex model discovery model/list RPC failed: {err}")),
        rpc.terminate().await,
    )?;

    let raw_models = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("Codex model discovery model/list response missing data array: {response}")
        })?;

    let models = codex_model_metadata_from_raw(raw_models);

    if models.is_empty() {
        return Err("Codex model discovery model/list returned no usable models".to_string());
    }

    Ok(codex_session_settings_schema(models))
}

fn codex_probe_result_with_cleanup<T>(
    operation: Result<T, String>,
    cleanup: Result<(), String>,
) -> Result<T, String> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(format!(
            "Codex model discovery app-server cleanup failed: {cleanup_error}"
        )),
        (Err(error), Err(cleanup_error)) => Err(format!(
            "{error}; Codex app-server cleanup also failed: {cleanup_error}"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexModelMetadata {
    option: protocol::SelectOption,
    reasoning_options: Vec<protocol::SelectOption>,
    is_default: bool,
}

fn codex_model_metadata_from_raw(raw_models: &[Value]) -> Vec<CodexModelMetadata> {
    let mut models = raw_models
        .iter()
        .filter_map(codex_model_metadata_entry_from_raw)
        .collect::<Vec<_>>();

    models.sort_by(|a, b| compare_codex_model_ids_for_display(&a.option.value, &b.option.value));
    models.dedup_by(|a, b| a.option.value.eq_ignore_ascii_case(&b.option.value));
    models
}

fn codex_model_metadata_entry_from_raw(model: &Value) -> Option<CodexModelMetadata> {
    let id = model
        .get("model")
        .or_else(|| model.get("id"))
        .and_then(Value::as_str)?
        .trim();
    if id.is_empty() {
        return None;
    }

    let mut reasoning_options = model
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(codex_reasoning_option_from_raw)
        .collect::<Vec<_>>();
    reasoning_options.dedup_by(|a, b| a.value == b.value);

    Some(CodexModelMetadata {
        option: protocol::SelectOption {
            value: id.to_string(),
            // Codex's displayName casing is not currently normalized across entries
            // (for example, `gpt-...` and `GPT-...` can appear in one response).
            // The model id is the canonical value we send back to Codex, so use it as
            // the label too and normalize only display casing.
            label: codex_model_label_from_id(id),
        },
        reasoning_options,
        is_default: model
            .get("isDefault")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn codex_reasoning_option_from_raw(value: &Value) -> Option<protocol::SelectOption> {
    let effort = value.get("reasoningEffort").and_then(Value::as_str)?.trim();
    if effort.is_empty() {
        return None;
    }
    Some(protocol::SelectOption {
        value: effort.to_string(),
        label: match effort {
            "xhigh" => "XHigh".to_string(),
            _ => effort
                .split(['-', '_'])
                .filter(|part| !part.is_empty())
                .map(|part| {
                    let mut chars = part.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_uppercase().collect::<String>()
                            + &chars.as_str().to_ascii_lowercase()
                    })
                })
                .collect::<Vec<_>>()
                .join(" "),
        },
    })
}

fn codex_model_label_from_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

fn compare_codex_model_ids_for_display(a: &str, b: &str) -> std::cmp::Ordering {
    let a_numbers = numeric_components(a);
    let b_numbers = numeric_components(b);

    for (a_number, b_number) in a_numbers.iter().zip(b_numbers.iter()) {
        match b_number.cmp(a_number) {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }

    match a_numbers.len().cmp(&b_numbers.len()) {
        // More numeric components are treated as a more specific/newer version:
        // e.g. `gpt-5.1` sorts before `gpt-5`.
        std::cmp::Ordering::Greater => return std::cmp::Ordering::Less,
        std::cmp::Ordering::Less => return std::cmp::Ordering::Greater,
        std::cmp::Ordering::Equal => {}
    }

    let a_normalized = a.to_ascii_lowercase();
    let b_normalized = b.to_ascii_lowercase();
    match a_normalized.cmp(&b_normalized) {
        std::cmp::Ordering::Equal => a.cmp(b),
        ordering => ordering,
    }
}

fn numeric_components(value: &str) -> Vec<u64> {
    let mut components = Vec::new();
    let mut current: Option<u64> = None;

    for byte in value.bytes() {
        if byte.is_ascii_digit() {
            let digit = u64::from(byte - b'0');
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit),
            );
        } else if let Some(number) = current.take() {
            components.push(number);
        }
    }

    if let Some(number) = current {
        components.push(number);
    }

    components
}

fn codex_generated_identity_epoch(thread_id: &str) -> u64 {
    thread_id.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Clone)]
struct PendingRequest {
    request_id: Value,
    tool_call_id: String,
    kind: PendingRequestKind,
}

#[derive(Clone)]
enum PendingRequestKind {
    CommandApproval,
    FileChangeApproval,
    ExecCommandApproval,
    ApplyPatchApproval,
    UserInput { questions: Vec<String> },
}

#[derive(Clone)]
struct ActiveStreamState {
    turn_id: String,
    message_id: ChatMessageId,
    generated_identity: Option<ServerGeneratedChatMessageIdentity>,
    text: String,
    reasoning: String,
    reasoning_only: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct CompletedCodexAgentMessage {
    completion_text: String,
    completion_reasoning: Option<String>,
    generated_identity: Option<ServerGeneratedChatMessageIdentity>,
}

enum CodexAgentMessageOpen {
    Open {
        message_id: ChatMessageId,
        generated_identity: Option<ServerGeneratedChatMessageIdentity>,
        model: String,
    },
    Existing,
    Terminal,
    Foreign,
    Quarantined,
}

#[derive(Clone, Default)]
struct TurnContextEstimate {
    conversation_history_bytes: u64,
    tool_io_bytes: u64,
    reasoning_bytes: u64,
}

#[derive(Clone)]
struct PendingCodexMessageMetadata {
    turn_id: String,
    message_id: ChatMessageId,
    model: String,
    turn_context: TurnContextEstimate,
}

#[derive(Clone, Default)]
struct CodexTurnTokenUsage {
    request_count: u32,
    latest_request: Option<TokenUsage>,
    turn: TokenUsage,
    cumulative: Option<TokenUsage>,
    model_context_window: Option<u64>,
}

struct CodexSubAgentStream {
    emitter: Arc<TurnEmitter>,
    spawn_item_id: String,
    activity_item_id: Option<String>,
    agent_path: String,
    sender_thread_id: String,
    active_turn_id: Option<String>,
    current_message_id: Option<ChatMessageId>,
    current_generated_identity: Option<ServerGeneratedChatMessageIdentity>,
    current_reasoning_only: bool,
    current_text: String,
    current_reasoning: String,
    completed_agent_messages: HashMap<ChatMessageId, CompletedCodexAgentMessage>,
    quarantined_turn_id: Option<String>,
    quarantined: bool,
    generated_identity_epoch: u64,
    next_generated_identity_ordinal: u64,
    pending_message_metadata: Option<PendingCodexMessageMetadata>,
    token_usage_by_turn: HashMap<String, Value>,
}

struct CompletedCodexSubAgentStream {
    emitter: Arc<TurnEmitter>,
    spawn_item_id: String,
    activity_item_id: Option<String>,
    agent_path: String,
    sender_thread_id: String,
    pending_message_metadata: Option<PendingCodexMessageMetadata>,
}

fn completed_codex_subagent_stream(stream: CodexSubAgentStream) -> CompletedCodexSubAgentStream {
    CompletedCodexSubAgentStream {
        emitter: stream.emitter,
        spawn_item_id: stream.spawn_item_id,
        activity_item_id: stream.activity_item_id,
        agent_path: stream.agent_path,
        sender_thread_id: stream.sender_thread_id,
        pending_message_metadata: stream.pending_message_metadata,
    }
}

#[derive(Clone)]
struct CodexSubAgentSpawnInfo {
    item_id: String,
    name: String,
    description: String,
    agent_type: String,
    receiver_thread_id: String,
    sender_thread_id: String,
}

struct CodexSubAgentActivity {
    item_id: Option<String>,
    agent_thread_id: String,
    agent_path: String,
    kind: String,
}

enum CodexNotificationOwner {
    Parent { thread_id: String },
    LiveChild { thread_id: String },
    CompletedChild { thread_id: String },
    Unknown { thread_id: Option<String> },
}

struct CodexState {
    thread_id: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    approval_policy: Option<String>,
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
    turn_network_access: bool,
    active_turn_id: Option<String>,
    active_stream: Option<ActiveStreamState>,
    completed_agent_messages: HashMap<ChatMessageId, CompletedCodexAgentMessage>,
    quarantined_turn_id: Option<String>,
    generated_identity_epoch: u64,
    next_generated_identity_ordinal: u64,
    pending_tool_call_ids: HashSet<String>,
    close_active_stream_when_tools_idle: bool,
    pending_message_metadata: Option<PendingCodexMessageMetadata>,
    completed_message_metadata_by_turn: HashMap<String, PendingCodexMessageMetadata>,
    token_usage_by_turn: HashMap<String, Value>,
    model_token_usage_by_turn: HashMap<String, CodexTurnTokenUsage>,
    turn_context_by_turn: HashMap<String, TurnContextEstimate>,
    file_change_call_ids: HashMap<String, Vec<String>>,
    pending_request: Option<PendingRequest>,
    pending_user_input_bytes: u64,
    conversation_bytes_total: u64,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    pending_subagent_spawns: HashMap<String, CodexSubAgentSpawnInfo>,
    conflicting_subagent_threads: HashMap<String, String>,
    registering_subagent_threads: HashSet<String>,
    unknown_owner_notifications: HashSet<String>,
    subagent_streams: HashMap<String, CodexSubAgentStream>,
    completed_subagent_streams: HashMap<String, CompletedCodexSubAgentStream>,
}

struct CodexInner {
    rpc: CodexRpc,
    emitter: Arc<TurnEmitter>,
    state: Mutex<CodexState>,
    steering_tempfile: Option<std::path::PathBuf>,
}

impl CodexInner {
    async fn apply_local_settings(&self, settings: &Value) {
        let Some(obj) = settings.as_object() else {
            return;
        };
        let mut state = self.state.lock().await;

        if let Some(model_value) = obj.get("model") {
            if model_value.is_null() {
                state.model = None;
            } else if let Some(model) = model_value.as_str() {
                let normalized = model.trim();
                state.model = if normalized.is_empty() {
                    None
                } else {
                    Some(normalized.to_string())
                };
            }
        }

        if let Some(effort_value) = obj
            .get("reasoning_effort")
            .or_else(|| obj.get("reasoningEffort"))
        {
            if effort_value.is_null() {
                state.reasoning_effort = None;
            } else if let Some(raw) = effort_value.as_str() {
                state.reasoning_effort = normalize_reasoning_effort(raw);
            }
        }

        if obj.contains_key("approval_policy") || obj.contains_key("approvalPolicy") {
            state.approval_policy = Some(CODEX_FORCED_APPROVAL_POLICY.to_string());
        }
    }

    async fn update_runtime_settings(&self, settings: Value) -> Result<(), String> {
        let thread_id = self.state.lock().await.thread_id.clone();
        self.rpc
            .request(
                "thread/update",
                json!({
                    "threadId": thread_id,
                    "settings": settings,
                }),
            )
            .await?;
        self.apply_local_settings(&settings).await;
        Ok(())
    }

    async fn open_agent_message_item(&self, message_id: ChatMessageId) -> CodexAgentMessageOpen {
        let mut state = self.state.lock().await;
        if state.quarantined_turn_id.is_some() {
            return CodexAgentMessageOpen::Quarantined;
        }
        if state.completed_agent_messages.contains_key(&message_id) {
            return CodexAgentMessageOpen::Terminal;
        }
        if let Some(stream) = state.active_stream.as_ref() {
            return if stream.message_id == message_id {
                CodexAgentMessageOpen::Existing
            } else {
                CodexAgentMessageOpen::Foreign
            };
        }

        let turn_id = state
            .active_turn_id
            .clone()
            .unwrap_or_else(|| message_id.0.clone());
        let model = state.model.clone().unwrap_or_else(|| "codex".to_string());
        state.active_stream = Some(ActiveStreamState {
            turn_id,
            message_id: message_id.clone(),
            generated_identity: None,
            text: String::new(),
            reasoning: String::new(),
            reasoning_only: false,
        });
        CodexAgentMessageOpen::Open {
            message_id,
            generated_identity: None,
            model,
        }
    }

    async fn open_reasoning_message_item(
        &self,
        provider_message_id: Option<ChatMessageId>,
    ) -> CodexAgentMessageOpen {
        let mut state = self.state.lock().await;
        if state.quarantined_turn_id.is_some() {
            return CodexAgentMessageOpen::Quarantined;
        }
        if let Some(stream) = state.active_stream.as_ref() {
            let same_identity = match provider_message_id.as_ref() {
                Some(message_id) => stream.message_id == *message_id,
                None => {
                    stream.reasoning_only
                        && stream.generated_identity.as_ref().is_some_and(|identity| {
                            identity.origin == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                        })
                }
            };
            return if same_identity {
                CodexAgentMessageOpen::Existing
            } else {
                CodexAgentMessageOpen::Foreign
            };
        }
        let generated_identity = provider_message_id.is_none().then(|| {
            let identity = ServerGeneratedChatMessageIdentity {
                origin: ServerGeneratedChatMessageIdOrigin::IdlessReasoning,
                stream_epoch: state.generated_identity_epoch,
                item_ordinal: state.next_generated_identity_ordinal,
            };
            state.next_generated_identity_ordinal =
                state.next_generated_identity_ordinal.saturating_add(1);
            identity
        });
        let message_id = provider_message_id.unwrap_or_else(|| {
            generated_identity
                .as_ref()
                .expect("generated identity")
                .message_id()
        });
        if state.completed_agent_messages.contains_key(&message_id) {
            return CodexAgentMessageOpen::Terminal;
        }
        let turn_id = state
            .active_turn_id
            .clone()
            .unwrap_or_else(|| message_id.0.clone());
        let model = state.model.clone().unwrap_or_else(|| "codex".to_string());
        state.active_stream = Some(ActiveStreamState {
            turn_id,
            message_id: message_id.clone(),
            generated_identity: generated_identity.clone(),
            text: String::new(),
            reasoning: String::new(),
            reasoning_only: true,
        });
        CodexAgentMessageOpen::Open {
            message_id,
            generated_identity,
            model,
        }
    }

    fn emit_stream_start(
        emitter: &TurnEmitter,
        message_id: ChatMessageId,
        generated_identity: Option<&ServerGeneratedChatMessageIdentity>,
        model: &str,
    ) {
        if let Some(identity) = generated_identity {
            emitter.stream_start_with_generated_identity(
                identity,
                AgentName(CODEX_AGENT_NAME),
                Some(model),
            );
        } else {
            emitter.stream_start_with_id(message_id, AgentName(CODEX_AGENT_NAME), Some(model));
        }
    }

    async fn reject_agent_message_identity(
        &self,
        violation: StreamIdentityViolation,
        method: &str,
        provider_item_id: Option<&str>,
    ) {
        let (thread_id, turn_id, active_item_id, active_buffer_len) = {
            let mut state = self.state.lock().await;
            if state.quarantined_turn_id.is_some() {
                return;
            }
            state.quarantined_turn_id = state.active_turn_id.clone();
            let active_stream = state.active_stream.take();
            state.pending_message_metadata = None;
            (
                state.thread_id.clone(),
                state.active_turn_id.clone(),
                active_stream
                    .as_ref()
                    .map(|stream| stream.message_id.clone()),
                active_stream.as_ref().map_or(0, |stream| stream.text.len()),
            )
        };
        tracing::warn!(
            codex_method = method,
            thread_id,
            ?turn_id,
            ?provider_item_id,
            ?active_item_id,
            active_buffer_len,
            ?violation,
            "Codex agentMessage identity violation"
        );
        self.emitter
            .discard_open_stream_with_identity_violation(violation);
    }

    async fn append_reasoning_to_active_stream(&self, reasoning: &str) {
        if !contains_non_whitespace(reasoning) {
            return;
        }
        let emission = {
            let mut state = self.state.lock().await;
            if state.quarantined_turn_id.is_some() {
                return;
            }
            let (message_id, appended) = if let Some(stream) = state.active_stream.as_mut() {
                if stream.reasoning.split('\n').any(|line| line == reasoning) {
                    (None, false)
                } else {
                    if !stream.reasoning.is_empty() && !stream.reasoning.ends_with('\n') {
                        stream.reasoning.push('\n');
                    }
                    stream.reasoning.push_str(reasoning);
                    (Some(stream.message_id.clone()), true)
                }
            } else {
                (None, true)
            };
            if appended && let Some(turn_id) = state.active_turn_id.as_ref().cloned() {
                let estimate = state.turn_context_by_turn.entry(turn_id).or_default();
                estimate.reasoning_bytes = estimate
                    .reasoning_bytes
                    .saturating_add(reasoning.len() as u64);
            }
            message_id
        };
        if let Some(message_id) = emission {
            self.emitter
                .stream_reasoning_delta_with_id(message_id, reasoning);
        }
    }

    async fn track_tool_requests(&self, tool_call_ids: impl IntoIterator<Item = String>) {
        let mut state = self.state.lock().await;
        state.pending_tool_call_ids.extend(tool_call_ids);
    }

    async fn mark_tool_completed(&self, tool_call_id: &str) {
        self.state
            .lock()
            .await
            .pending_tool_call_ids
            .remove(tool_call_id);
    }

    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                self.emit_user_message_added(&message, images.as_deref());
                // UI contract: show typing immediately when a user turn is submitted,
                // without waiting for Codex to acknowledge turn/started.
                self.emitter.typing_status_changed(true);

                if self.respond_pending_request(&message).await? {
                    return Ok(());
                }

                let (
                    thread_id,
                    model_override,
                    effort_override,
                    approval_policy_override,
                    access_mode,
                    execution_mode,
                    turn_network_access,
                ) = {
                    let mut state = self.state.lock().await;
                    state.pending_user_input_bytes = message.len() as u64;
                    let (model_override, effort_override) = match state.execution_mode {
                        BackendExecutionMode::Agent => {
                            (state.model.clone(), state.reasoning_effort.clone())
                        }
                        BackendExecutionMode::InferenceOnly => (None, None),
                    };
                    (
                        state.thread_id.clone(),
                        model_override,
                        effort_override,
                        state.approval_policy.clone(),
                        state.access_mode,
                        state.execution_mode,
                        state.turn_network_access,
                    )
                };

                let mut input_items = vec![json!({
                    "type": "text",
                    "text": message,
                    "text_elements": []
                })];

                if let Some(imgs) = images {
                    for image in imgs {
                        let path = persist_temp_image(&image).await?;
                        input_items.push(json!({
                            "type": "localImage",
                            "path": path
                        }));
                    }
                }

                let mut params = json!({
                    "threadId": thread_id,
                    "input": input_items
                });

                if let Some(model) = model_override {
                    params["model"] = Value::String(model);
                }
                if let Some(effort) = effort_override {
                    params["effort"] = Value::String(effort);
                }
                params["summary"] = Value::String(CODEX_REASONING_SUMMARY_LEVEL.to_string());
                let approval_policy = approval_policy_override
                    .unwrap_or_else(|| codex_approval_policy(execution_mode).to_string());
                params["approvalPolicy"] = Value::String(approval_policy);
                params["sandboxPolicy"] =
                    codex_sandbox_policy(access_mode, turn_network_access, execution_mode);

                if let Err(err) = self.rpc.request("turn/start", params).await {
                    self.emitter.typing_status_changed(false);
                    return Err(err);
                }
                Ok(())
            }
            SessionCommand::CancelConversation => {
                let (thread_id, turn_id_opt) = {
                    let state = self.state.lock().await;
                    (state.thread_id.clone(), state.active_turn_id.clone())
                };
                let Some(turn_id) = turn_id_opt else {
                    return Ok(());
                };
                let _ = self
                    .rpc
                    .request(
                        "turn/interrupt",
                        json!({
                            "threadId": thread_id,
                            "turnId": turn_id
                        }),
                    )
                    .await?;
                Ok(())
            }
            SessionCommand::GetSettings => {
                // Phase 6 handles config/settings parity. Keep non-failing no-op for now.
                Ok(())
            }
            SessionCommand::ListSessions => self.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => self.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => self.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                // Phase 6 handles profiles parity.
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => {
                // Phase 6 handles profile switching parity.
                Ok(())
            }
            SessionCommand::GetModuleSchemas => {
                // Phase 6 handles module schema parity.
                Ok(())
            }
            SessionCommand::ListModels => self.list_models().await,
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                self.apply_local_settings(&settings).await;
                Ok(())
            }
        }
    }

    async fn list_sessions(&self) -> Result<(), String> {
        let mut cursor: Option<String> = None;
        let mut sessions: Vec<Value> = Vec::new();

        for _ in 0..20 {
            let mut params = json!({ "limit": 100 });
            if let Some(cur) = cursor.as_ref() {
                params["cursor"] = Value::String(cur.clone());
            }

            let response = self.rpc.request("thread/list", params).await?;
            let page = response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            if page.is_empty() {
                break;
            }

            for thread in page {
                if let Some(metadata) = codex_thread_to_session_metadata(&thread) {
                    sessions.push(metadata);
                }
            }

            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(|s| s.to_string());

            if cursor.is_none() || sessions.len() >= 1000 {
                break;
            }
        }

        self.emitter.sessions_list(sessions);
        Ok(())
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        let response = self
            .rpc
            .request(
                "thread/resume",
                json!({
                    "threadId": session_id,
                    "experimentalRawEvents": CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS
                }),
            )
            .await?;

        let thread = response
            .get("thread")
            .ok_or("Codex thread/resume response missing thread")?;
        let resumed_thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .ok_or("Codex thread/resume response missing thread.id")?
            .to_string();
        let resumed_model = response
            .get("model")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let turns = thread
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "Codex resume response missing 'turns' array".to_string())?;

        self.complete_all_codex_subagents().await;

        {
            let mut state = self.state.lock().await;
            state.thread_id = resumed_thread_id;
            if let Some(model) = resumed_model.clone() {
                state.model = Some(model);
            }
            state.active_turn_id = None;
            state.active_stream = None;
            state.pending_message_metadata = None;
            state.token_usage_by_turn.clear();
            state.model_token_usage_by_turn.clear();
            state.turn_context_by_turn.clear();
            state.file_change_call_ids.clear();
            state.pending_request = None;
            state.pending_user_input_bytes = 0;
            state.conversation_bytes_total = 0;
        }

        self.emitter.conversation_cleared();
        self.emitter.typing_status_changed(false);

        let model = resumed_model.unwrap_or_else(|| "codex".to_string());
        let restored_bytes = self.emit_resumed_thread_history(&turns, &model).await;
        let mut state = self.state.lock().await;
        state.conversation_bytes_total = restored_bytes;

        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        match self
            .rpc
            .request(
                "thread/archive",
                json!({
                    "threadId": session_id
                }),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                let normalized = err.to_ascii_lowercase();
                if normalized.contains("no rollout found")
                    || normalized.contains("thread not found")
                    || normalized.contains("not found")
                {
                    return Ok(());
                }
                Err(err)
            }
        }
    }

    async fn list_models(&self) -> Result<(), String> {
        let response = self
            .rpc
            .request("model/list", json!({ "includeHidden": false }))
            .await?;

        let raw_models = response
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let models: Vec<Value> = raw_models
            .iter()
            .filter_map(|m| {
                let id = m
                    .get("model")
                    .or_else(|| m.get("id"))
                    .and_then(Value::as_str)?;
                let display_name = m.get("displayName").and_then(Value::as_str).unwrap_or(id);
                let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false);
                Some(json!({
                    "id": id,
                    "displayName": display_name,
                    "isDefault": is_default,
                }))
            })
            .collect();

        self.emitter.models_list(models);
        Ok(())
    }

    async fn emit_resumed_thread_history(&self, turns: &[Value], model: &str) -> u64 {
        let mut total_bytes = 0u64;

        for turn in turns {
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };

            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();

                match item_type {
                    "userMessage" => {
                        let text = extract_codex_item_text(item);
                        if text.trim().is_empty() {
                            continue;
                        }
                        total_bytes = total_bytes.saturating_add(text.len() as u64);
                        self.emitter.user_message(&text, Vec::new());
                    }
                    "agentMessage" => {
                        let text = extract_codex_item_text(item);
                        if text.trim().is_empty() {
                            continue;
                        }
                        let reasoning = extract_codex_item_reasoning(item);
                        total_bytes = total_bytes.saturating_add(text.len() as u64);
                        self.emitter.assistant_message(
                            crate::backend::turn_emitter::AssistantMessagePayload {
                                agent: AgentName(CODEX_AGENT_NAME),
                                message_id: None,
                                content: text,
                                reasoning: reasoning.map(|summary| json!({ "text": summary })),
                                tool_calls: Vec::new(),
                                model_info: Some(json!({ "model": model })),
                                request_usage: None,
                                turn_usage: None,
                                cumulative_usage: None,
                                context_breakdown: None,
                                images: Vec::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
        }

        total_bytes
    }

    async fn respond_pending_request(&self, message: &str) -> Result<bool, String> {
        let pending = {
            let mut state = self.state.lock().await;
            state.pending_request.take()
        };

        let Some(pending) = pending else {
            return Ok(false);
        };

        match pending.kind {
            PendingRequestKind::CommandApproval => {
                let decision = parse_approval_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::FileChangeApproval => {
                let decision = parse_approval_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "file_change_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::ExecCommandApproval => {
                let decision = parse_review_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "exec_command_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::ApplyPatchApproval => {
                let decision = parse_review_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "apply_patch_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::UserInput { questions } => {
                let normalized = if message.trim().is_empty() {
                    String::new()
                } else {
                    message.trim().to_string()
                };
                let mut answers = serde_json::Map::new();
                for q in &questions {
                    answers.insert(q.clone(), json!({ "answers": [normalized] }));
                }
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "answers": answers
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "ask_user_question",
                    true,
                    json!({"kind": "Other", "result": {"answered": true}}),
                    None,
                )
                .await;
            }
        }

        Ok(true)
    }

    async fn handle_inbound(&self, inbound: CodexInbound) {
        match inbound {
            CodexInbound::Stderr(line) => {
                self.emitter.subprocess_stderr(&line);
            }
            CodexInbound::Closed { exit_code } => {
                self.complete_all_codex_subagents().await;
                self.emitter.subprocess_exit(exit_code);
                // The app-server exited on its own; reap it now rather than
                // leaving a zombie until session teardown (CodexRpc::Drop won't
                // fire while the forwarder still holds Arc<CodexInner>).
                self.rpc.reap_after_exit().await;
            }
            CodexInbound::Notification { method, params } => {
                if method.starts_with("codex/event/") {
                    self.handle_legacy_codex_event(&method, &params).await;
                    return;
                }
                self.handle_notification(&method, &params).await;
            }
            CodexInbound::ServerRequest { id, method, params } => {
                self.handle_server_request(id, &method, &params).await;
            }
        }
    }

    async fn handle_notification(&self, method: &str, params: &Value) {
        self.trace_agent_message_identity_event(method, params)
            .await;
        if matches!(method, "subAgentActivity" | "sub_agent_activity") {
            let mut item = params
                .get("item")
                .cloned()
                .unwrap_or_else(|| params.clone());
            if item.get("type").is_none() {
                item["type"] = Value::String("subAgentActivity".to_string());
            }
            self.register_codex_subagent_activity_if_needed(&item).await;
            return;
        }
        let suppress_root_response_before_routing = if is_codex_response_side_notification(method) {
            let notification_thread_id = extract_notification_thread_id(params);
            let state = self.state.lock().await;
            state
                .quarantined_turn_id
                .as_ref()
                .is_some_and(|quarantined_turn_id| {
                    let belongs_to_root = notification_thread_id
                        .as_ref()
                        .is_none_or(|thread_id| thread_id == &state.thread_id);
                    let repeats_quarantined_turn = method != "turn/started"
                        || extract_turn_id(params)
                            .as_ref()
                            .is_none_or(|turn_id| turn_id == quarantined_turn_id);
                    belongs_to_root && repeats_quarantined_turn
                })
        } else {
            false
        };
        if suppress_root_response_before_routing {
            tracing::debug!(
                codex_method = method,
                "Ignoring late Codex response notification for quarantined root turn"
            );
            return;
        }
        if self
            .handle_subagent_notification_if_needed(method, params)
            .await
        {
            return;
        }
        let suppress_quarantined_response = if is_codex_response_side_notification(method) {
            let state = self.state.lock().await;
            state
                .quarantined_turn_id
                .as_ref()
                .is_some_and(|quarantined_turn_id| {
                    method != "turn/started"
                        || extract_turn_id(params)
                            .as_ref()
                            .is_none_or(|turn_id| turn_id == quarantined_turn_id)
                })
        } else {
            false
        };
        if suppress_quarantined_response {
            tracing::debug!(
                codex_method = method,
                "Ignoring late Codex response notification for quarantined root turn"
            );
            return;
        }

        match method {
            "account/rateLimits/updated" => {
                let emitter = self.state.lock().await.subagent_emitter.clone();
                if let Some(emitter) = emitter {
                    forward_passive_rate_limits_updated(params, emitter.as_ref());
                }
            }
            "turn/started" => {
                let turn_id = params
                    .get("turn")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("turn")
                    .to_string();
                {
                    let mut state = self.state.lock().await;
                    state.active_turn_id = Some(turn_id.clone());
                    state.quarantined_turn_id = None;
                    state.active_stream = None;
                    state.pending_tool_call_ids.clear();
                    state.close_active_stream_when_tools_idle = false;
                    state.pending_message_metadata = None;
                    let pending_user_input = state.pending_user_input_bytes;
                    state.pending_user_input_bytes = 0;
                    state.conversation_bytes_total = state
                        .conversation_bytes_total
                        .saturating_add(pending_user_input);
                    let history_bytes = state.conversation_bytes_total;
                    state.turn_context_by_turn.insert(
                        turn_id.clone(),
                        TurnContextEstimate {
                            conversation_history_bytes: history_bytes,
                            ..TurnContextEstimate::default()
                        },
                    );
                }
                self.emitter.typing_status_changed(true);
            }
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let Some(message_id) = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()))
                else {
                    self.reject_agent_message_identity(
                        StreamIdentityViolation::MissingMessageId,
                        method,
                        None,
                    )
                    .await;
                    return;
                };
                if delta.is_empty() {
                    return;
                }
                match self.open_agent_message_item(message_id.clone()).await {
                    CodexAgentMessageOpen::Open {
                        message_id,
                        generated_identity,
                        model,
                    } => {
                        self.emitter.typing_status_changed(true);
                        Self::emit_stream_start(
                            self.emitter.as_ref(),
                            message_id,
                            generated_identity.as_ref(),
                            &model,
                        );
                    }
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::DuplicateTerminalMessageId,
                            method,
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                    CodexAgentMessageOpen::Quarantined => (),
                    CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::ForeignActiveMessageId,
                            method,
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                }
                {
                    let mut state = self.state.lock().await;
                    if let Some(stream) = state
                        .active_stream
                        .as_mut()
                        .filter(|stream| stream.message_id == message_id)
                    {
                        stream.text.push_str(&delta);
                    }
                }
                self.emitter.stream_delta_with_id(message_id, &delta);
            }
            reasoning_method if is_reasoning_notification_method(reasoning_method) => {
                let Some(delta) = extract_codex_reasoning_delta_text(params) else {
                    return;
                };
                let provider_item_id = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                match self
                    .open_reasoning_message_item(provider_item_id.clone())
                    .await
                {
                    CodexAgentMessageOpen::Open {
                        message_id,
                        generated_identity,
                        model,
                    } => {
                        self.emitter.typing_status_changed(true);
                        Self::emit_stream_start(
                            self.emitter.as_ref(),
                            message_id,
                            generated_identity.as_ref(),
                            &model,
                        );
                    }
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::DuplicateTerminalMessageId,
                            method,
                            provider_item_id.as_ref().map(|item_id| item_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                    CodexAgentMessageOpen::Quarantined => (),
                    CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::ForeignActiveMessageId,
                            method,
                            provider_item_id.as_ref().map(|item_id| item_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                }
                self.append_reasoning_to_active_stream(&delta).await;
            }
            "item/started" => {
                self.handle_item_started(params).await;
            }
            "item/completed" => {
                self.handle_item_completed(params).await;
            }
            "turn/plan/updated" => {
                self.handle_plan_update(params);
            }
            "thread/tokenUsage/updated" => {
                self.handle_root_token_usage_updated(params).await;
            }
            "model/rerouted" => {
                if let Some(model) = params.get("toModel").and_then(Value::as_str) {
                    let mut state = self.state.lock().await;
                    state.model = Some(model.to_string());
                }
            }
            "turn/completed" => {
                self.handle_turn_completed(params).await;
            }
            "error" => {
                self.handle_error_notification(params).await;
            }
            _ => {}
        }
    }

    async fn trace_agent_message_identity_event(&self, method: &str, params: &Value) {
        let item = params.get("item");
        let item_type = item
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .or_else(|| (method == "item/agentMessage/delta").then_some("agentMessage"));
        if item_type != Some("agentMessage") {
            return;
        }

        let provider_item_id = item
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .or_else(|| params.get("itemId").and_then(Value::as_str))
            .or_else(|| params.get("item_id").and_then(Value::as_str));
        let thread_id = params
            .get("threadId")
            .or_else(|| params.get("thread_id"))
            .and_then(Value::as_str);
        let turn_id = extract_turn_id(params);
        let (active_turn_id, active_item_id, active_buffer_len) = {
            let state = self.state.lock().await;
            (
                state.active_turn_id.clone(),
                state
                    .active_stream
                    .as_ref()
                    .map(|stream| stream.message_id.clone()),
                state
                    .active_stream
                    .as_ref()
                    .map_or(0, |stream| stream.text.len()),
            )
        };
        tracing::debug!(
            codex_method = method,
            ?thread_id,
            ?turn_id,
            ?provider_item_id,
            provider_item_type = item_type,
            ?active_turn_id,
            ?active_item_id,
            active_buffer_len,
            "Codex agentMessage identity event"
        );
    }

    async fn handle_root_token_usage_updated(&self, params: &Value) {
        let (metadata_update, model_usage) = {
            let mut state = self.state.lock().await;
            let model = state.model.clone();
            let model_usage = extract_model_request_token_usage(params, model.as_deref()).and_then(
                |(turn_id, request, cumulative, context_window)| {
                    record_model_request_token_usage(
                        &mut state.model_token_usage_by_turn,
                        turn_id,
                        request,
                        cumulative,
                        context_window,
                    )
                },
            );
            let Some((turn_id, token_usage)) = extract_turn_token_usage(params, model.as_deref())
            else {
                return;
            };

            let metadata_update =
                if let Some(pending) = state.completed_message_metadata_by_turn.remove(&turn_id) {
                    let context_breakdown = estimate_context_breakdown(
                        Some(&token_usage),
                        &pending.turn_context,
                        Some(&pending.model),
                    );
                    let model_token_usage = state.model_token_usage_by_turn.get(&turn_id).cloned();
                    Some((pending, token_usage, model_token_usage, context_breakdown))
                } else if state.active_turn_id.as_deref() == Some(turn_id.as_str())
                    || state
                        .active_stream
                        .as_ref()
                        .is_some_and(|stream| stream.turn_id == turn_id)
                    || state
                        .pending_message_metadata
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == turn_id)
                {
                    state.token_usage_by_turn.insert(turn_id, token_usage);
                    None
                } else {
                    None
                };
            (metadata_update, model_usage)
        };

        if let Some(usage) = model_usage {
            self.emitter.model_request_token_usage(&usage);
        }
        if let Some((pending, token_usage, model_token_usage, context_breakdown)) = metadata_update
        {
            emit_codex_message_metadata_update(
                &self.emitter,
                pending,
                Some(token_usage),
                model_token_usage.as_ref(),
                context_breakdown,
            );
        }
    }

    async fn handle_subagent_notification_if_needed(&self, method: &str, params: &Value) -> bool {
        let explicitly_thread_scoped_control = matches!(method, "model/rerouted" | "error")
            && extract_notification_thread_id(params).is_some();
        if !is_thread_scoped_codex_notification(method) && !explicitly_thread_scoped_control {
            return false;
        }
        let owner = {
            let state = self.state.lock().await;
            classify_codex_notification_owner(&state, params)
        };

        match owner {
            CodexNotificationOwner::Parent { thread_id } => {
                tracing::debug!(method, thread_id, "Codex notification ownership: parent");
                false
            }
            CodexNotificationOwner::LiveChild { thread_id } => {
                let model = self
                    .state
                    .lock()
                    .await
                    .model
                    .clone()
                    .unwrap_or_else(|| "codex".to_string());
                tracing::debug!(
                    method,
                    thread_id,
                    "Codex notification ownership: live child"
                );
                self.handle_subagent_notification(method, params, &thread_id, &model)
                    .await;
                true
            }
            CodexNotificationOwner::CompletedChild { thread_id } => {
                let model = self
                    .state
                    .lock()
                    .await
                    .model
                    .clone()
                    .unwrap_or_else(|| "codex".to_string());
                tracing::warn!(
                    method,
                    thread_id,
                    "Codex notification ownership: completed child"
                );
                self.handle_completed_subagent_notification(method, params, &thread_id, &model)
                    .await;
                true
            }
            CodexNotificationOwner::Unknown { thread_id } => {
                let thread_id = thread_id.unwrap_or_else(|| "<missing>".to_string());
                let key = format!("{method}:{thread_id}");
                let (first_observation, known_child_count) = {
                    let mut state = self.state.lock().await;
                    let first_observation = state.unknown_owner_notifications.insert(key);
                    let known_child_count =
                        state.subagent_streams.len() + state.completed_subagent_streams.len();
                    (first_observation, known_child_count)
                };
                if first_observation {
                    let message = format!(
                        "Codex ownership invariant failed: thread-scoped notification '{method}' belongs to unregistered thread '{thread_id}'"
                    );
                    tracing::error!(method, thread_id, known_child_count, "{message}");
                    self.emitter.backend_error(&message);
                } else {
                    tracing::debug!(
                        method,
                        thread_id,
                        "Repeated unknown Codex thread notification suppressed"
                    );
                }
                true
            }
        }
    }

    async fn handle_subagent_notification(
        &self,
        method: &str,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        {
            let suppress_quarantined_response = {
                let state = self.state.lock().await;
                state
                    .subagent_streams
                    .get(stream_key)
                    .is_some_and(|stream| {
                        stream.quarantined
                            && (method != "turn/started"
                                || extract_turn_id(params).as_ref().is_none_or(|turn_id| {
                                    Some(turn_id) == stream.quarantined_turn_id.as_ref()
                                }))
                    })
            };
            if suppress_quarantined_response {
                tracing::debug!(
                    child_thread_id = stream_key,
                    codex_method = method,
                    "Ignoring late Codex response notification for quarantined child turn"
                );
                return;
            }
        }
        match method {
            "turn/started" => {
                let turn_id = extract_turn_id(params).unwrap_or_else(|| "turn".to_string());
                let Some(emitter) = self
                    .update_codex_subagent_stream(stream_key, |stream| {
                        stream.active_turn_id = Some(turn_id.clone());
                        stream.current_message_id = None;
                        stream.current_generated_identity = None;
                        stream.current_reasoning_only = false;
                        stream.current_text.clear();
                        stream.current_reasoning.clear();
                        stream.quarantined_turn_id = None;
                        stream.quarantined = false;
                        stream.pending_message_metadata = None;
                    })
                    .await
                else {
                    return;
                };
                emitter.typing_status_changed(true);
            }
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if delta.is_empty() {
                    return;
                }
                let Some(message_id) = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|message_id| !message_id.trim().is_empty())
                    .map(|message_id| ChatMessageId(message_id.to_string()))
                else {
                    self.reject_subagent_message_identity(
                        stream_key,
                        StreamIdentityViolation::MissingMessageId,
                        method,
                    )
                    .await;
                    return;
                };
                let (emitter, open_stream, violation) = {
                    let mut state = self.state.lock().await;
                    let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                        return;
                    };
                    if stream.quarantined {
                        return;
                    }
                    if stream.completed_agent_messages.contains_key(&message_id) {
                        (
                            Arc::clone(&stream.emitter),
                            false,
                            Some(StreamIdentityViolation::DuplicateTerminalMessageId),
                        )
                    } else if let Some(active_id) = stream.current_message_id.as_ref() {
                        if active_id == &message_id && !stream.current_reasoning_only {
                            (Arc::clone(&stream.emitter), false, None)
                        } else {
                            (
                                Arc::clone(&stream.emitter),
                                false,
                                Some(StreamIdentityViolation::ForeignActiveMessageId),
                            )
                        }
                    } else {
                        stream.current_message_id = Some(message_id.clone());
                        stream.current_generated_identity = None;
                        stream.current_reasoning_only = false;
                        stream.current_text.clear();
                        stream.current_reasoning.clear();
                        (Arc::clone(&stream.emitter), true, None)
                    }
                };
                if let Some(violation) = violation {
                    self.reject_subagent_message_identity(stream_key, violation, method)
                        .await;
                    return;
                }
                if open_stream {
                    emitter.stream_start_with_id(
                        message_id.clone(),
                        AgentName(CODEX_AGENT_NAME),
                        Some(model),
                    );
                }
                {
                    let mut state = self.state.lock().await;
                    if let Some(stream) = state.subagent_streams.get_mut(stream_key) {
                        stream.current_text.push_str(&delta);
                    }
                }
                emitter.stream_delta_with_id(message_id, &delta);
            }
            reasoning_method if is_reasoning_notification_method(reasoning_method) => {
                let Some(delta) = extract_codex_reasoning_delta_text(params) else {
                    return;
                };
                let provider_item_id = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                let (emitter, message_id, generated_identity, open_stream, violation) = {
                    let mut state = self.state.lock().await;
                    let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                        return;
                    };
                    if stream.quarantined {
                        return;
                    }
                    if let Some(active_message_id) = stream.current_message_id.clone() {
                        let matches_idless_reasoning = stream.current_reasoning_only
                            && stream
                                .current_generated_identity
                                .as_ref()
                                .is_some_and(|identity| {
                                    identity.origin
                                        == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                                });
                        let matches = match provider_item_id.as_ref() {
                            Some(item_id) => {
                                item_id == &active_message_id && stream.current_reasoning_only
                            }
                            None => matches_idless_reasoning,
                        };
                        if !matches {
                            (
                                Arc::clone(&stream.emitter),
                                None,
                                None,
                                false,
                                Some(StreamIdentityViolation::ForeignActiveMessageId),
                            )
                        } else {
                            stream.current_reasoning.push_str(&delta);
                            (
                                Arc::clone(&stream.emitter),
                                Some(active_message_id),
                                stream.current_generated_identity.clone(),
                                false,
                                None,
                            )
                        }
                    } else {
                        let generated_identity = provider_item_id.is_none().then(|| {
                            let identity = ServerGeneratedChatMessageIdentity {
                                origin: ServerGeneratedChatMessageIdOrigin::IdlessReasoning,
                                stream_epoch: stream.generated_identity_epoch,
                                item_ordinal: stream.next_generated_identity_ordinal,
                            };
                            stream.next_generated_identity_ordinal =
                                stream.next_generated_identity_ordinal.saturating_add(1);
                            identity
                        });
                        let message_id = provider_item_id.clone().unwrap_or_else(|| {
                            generated_identity
                                .as_ref()
                                .expect("generated child reasoning identity")
                                .message_id()
                        });
                        if stream.completed_agent_messages.contains_key(&message_id) {
                            (
                                Arc::clone(&stream.emitter),
                                Some(message_id),
                                generated_identity,
                                false,
                                Some(StreamIdentityViolation::DuplicateTerminalMessageId),
                            )
                        } else {
                            stream.current_message_id = Some(message_id.clone());
                            stream.current_generated_identity = generated_identity.clone();
                            stream.current_reasoning_only = true;
                            stream.current_text.clear();
                            stream.current_reasoning = delta.clone();
                            (
                                Arc::clone(&stream.emitter),
                                Some(message_id),
                                generated_identity,
                                true,
                                None,
                            )
                        }
                    }
                };
                if let Some(violation) = violation {
                    self.reject_subagent_message_identity(stream_key, violation, method)
                        .await;
                    return;
                }
                if let Some(message_id) = message_id {
                    if open_stream {
                        Self::emit_stream_start(
                            emitter.as_ref(),
                            message_id.clone(),
                            generated_identity.as_ref(),
                            model,
                        );
                    }
                    emitter.stream_reasoning_delta_with_id(message_id, &delta);
                }
            }
            "item/started" => {
                if params.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") {
                    let provider_message_id = params
                        .pointer("/item/id")
                        .and_then(Value::as_str)
                        .filter(|message_id| !message_id.trim().is_empty())
                        .map(|message_id| ChatMessageId(message_id.to_string()));
                    let (emitter, message_id, generated_identity, open_stream, violation) = {
                        let mut state = self.state.lock().await;
                        let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                            return;
                        };
                        if stream.quarantined {
                            return;
                        }
                        if let Some(active_message_id) = stream.current_message_id.clone() {
                            let matches = stream.current_reasoning_only
                                && match provider_message_id.as_ref() {
                                    Some(message_id) => message_id == &active_message_id,
                                    None => stream.current_generated_identity.as_ref().is_some_and(
                                        |identity| {
                                            identity.origin
                                                == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                                        },
                                    ),
                                };
                            (
                                Arc::clone(&stream.emitter),
                                Some(active_message_id),
                                stream.current_generated_identity.clone(),
                                false,
                                (!matches)
                                    .then_some(StreamIdentityViolation::ForeignActiveMessageId),
                            )
                        } else {
                            let generated_identity = provider_message_id.is_none().then(|| {
                                let identity = ServerGeneratedChatMessageIdentity {
                                    origin: ServerGeneratedChatMessageIdOrigin::IdlessReasoning,
                                    stream_epoch: stream.generated_identity_epoch,
                                    item_ordinal: stream.next_generated_identity_ordinal,
                                };
                                stream.next_generated_identity_ordinal =
                                    stream.next_generated_identity_ordinal.saturating_add(1);
                                identity
                            });
                            let message_id = provider_message_id.clone().unwrap_or_else(|| {
                                generated_identity
                                    .as_ref()
                                    .expect("generated child reasoning identity")
                                    .message_id()
                            });
                            let violation = stream
                                .completed_agent_messages
                                .contains_key(&message_id)
                                .then_some(StreamIdentityViolation::DuplicateTerminalMessageId);
                            if violation.is_none() {
                                stream.current_message_id = Some(message_id.clone());
                                stream.current_generated_identity = generated_identity.clone();
                                stream.current_reasoning_only = true;
                                stream.current_text.clear();
                                stream.current_reasoning.clear();
                            }
                            (
                                Arc::clone(&stream.emitter),
                                Some(message_id),
                                generated_identity,
                                violation.is_none(),
                                violation,
                            )
                        }
                    };
                    if let Some(violation) = violation {
                        self.reject_subagent_message_identity(stream_key, violation, method)
                            .await;
                        return;
                    }
                    if open_stream {
                        Self::emit_stream_start(
                            emitter.as_ref(),
                            message_id.expect("opened child reasoning message"),
                            generated_identity.as_ref(),
                            model,
                        );
                    }
                    return;
                }
                if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") {
                    let Some(message_id) = params
                        .pointer("/item/id")
                        .and_then(Value::as_str)
                        .filter(|message_id| !message_id.trim().is_empty())
                        .map(|message_id| ChatMessageId(message_id.to_string()))
                    else {
                        self.reject_subagent_message_identity(
                            stream_key,
                            StreamIdentityViolation::MissingMessageId,
                            method,
                        )
                        .await;
                        return;
                    };
                    let (emitter, open_stream, violation) = {
                        let mut state = self.state.lock().await;
                        let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                            return;
                        };
                        if stream.quarantined {
                            return;
                        }
                        if stream.completed_agent_messages.contains_key(&message_id) {
                            (
                                Arc::clone(&stream.emitter),
                                false,
                                Some(StreamIdentityViolation::DuplicateTerminalMessageId),
                            )
                        } else if let Some(active_id) = stream.current_message_id.as_ref() {
                            if active_id == &message_id && !stream.current_reasoning_only {
                                (Arc::clone(&stream.emitter), false, None)
                            } else {
                                (
                                    Arc::clone(&stream.emitter),
                                    false,
                                    Some(StreamIdentityViolation::ForeignActiveMessageId),
                                )
                            }
                        } else {
                            stream.current_message_id = Some(message_id.clone());
                            stream.current_generated_identity = None;
                            stream.current_reasoning_only = false;
                            stream.current_text.clear();
                            stream.current_reasoning.clear();
                            (Arc::clone(&stream.emitter), true, None)
                        }
                    };
                    if let Some(violation) = violation {
                        self.reject_subagent_message_identity(stream_key, violation, method)
                            .await;
                        return;
                    }
                    if open_stream {
                        emitter.stream_start_with_id(
                            message_id,
                            AgentName(CODEX_AGENT_NAME),
                            Some(model),
                        );
                    }
                    return;
                }
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                self.handle_subagent_item_started(params, emitter.as_ref());
            }
            "item/completed" => {
                self.handle_subagent_item_completed(params, stream_key, model)
                    .await;
            }
            "turn/plan/updated" => {
                let tasks = codex_plan_update_task_list_from_params(params).unwrap_or_else(|| {
                    protocol::TaskList {
                        title: "Plan".to_string(),
                        tasks: Vec::new(),
                    }
                });
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                emitter.task_update(&tasks);
            }
            "thread/tokenUsage/updated" => {
                self.handle_subagent_token_usage_updated(params, stream_key, model)
                    .await;
            }
            "turn/completed" => {
                self.handle_subagent_turn_completed(params, stream_key, model)
                    .await;
            }
            "error" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex error")
                    .to_string();
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                emitter.backend_error(&message);
                emitter.typing_status_changed(false);
            }
            _ => {}
        }
    }

    async fn handle_completed_subagent_notification(
        &self,
        method: &str,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        match method {
            "thread/tokenUsage/updated" | "turn/completed" => {
                self.handle_completed_subagent_token_usage(params, stream_key, model)
                    .await;
            }
            _ => {
                let emitter = {
                    let state = self.state.lock().await;
                    state
                        .completed_subagent_streams
                        .get(stream_key)
                        .map(|stream| Arc::clone(&stream.emitter))
                };
                if let Some(emitter) = emitter {
                    let message = format!(
                        "Codex ownership invariant failed: late child content '{method}' arrived after child thread '{stream_key}' completed"
                    );
                    tracing::error!(method, thread_id = stream_key, "{message}");
                    emitter.backend_error(&message);
                }
            }
        }
    }

    async fn codex_subagent_emitter(&self, stream_key: &str) -> Option<Arc<TurnEmitter>> {
        let state = self.state.lock().await;
        state
            .subagent_streams
            .get(stream_key)
            .map(|stream| Arc::clone(&stream.emitter))
    }

    async fn update_codex_subagent_stream(
        &self,
        stream_key: &str,
        update: impl FnOnce(&mut CodexSubAgentStream),
    ) -> Option<Arc<TurnEmitter>> {
        let mut state = self.state.lock().await;
        let stream = state.subagent_streams.get_mut(stream_key)?;
        update(stream);
        Some(Arc::clone(&stream.emitter))
    }

    async fn reject_subagent_message_identity(
        &self,
        stream_key: &str,
        violation: StreamIdentityViolation,
        method: &str,
    ) {
        let emitter = {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return;
            };
            if stream.quarantined {
                return;
            }
            stream.quarantined = true;
            stream.quarantined_turn_id = stream.active_turn_id.clone();
            stream.current_message_id = None;
            stream.current_generated_identity = None;
            stream.current_reasoning_only = false;
            stream.current_text.clear();
            stream.current_reasoning.clear();
            Arc::clone(&stream.emitter)
        };
        tracing::warn!(
            child_thread_id = stream_key,
            codex_method = method,
            ?violation,
            "Codex child agentMessage identity violation"
        );
        emitter.discard_open_stream_with_identity_violation(violation);
    }

    fn handle_subagent_item_started(&self, params: &Value, emitter: &TurnEmitter) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("tool-call")
            .to_string();

        match item_type {
            "commandExecution" => {
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                emitter.tool_request(
                    &item_id,
                    "run_command",
                    json!({
                        "kind": "RunCommand",
                        "command": command,
                        "working_directory": cwd
                    }),
                );
            }
            "fileChange" => {
                let file_changes = parse_codex_file_changes(item);
                if file_changes.is_empty() {
                    return;
                }
                let total = file_changes.len();
                for (idx, change) in file_changes.iter().enumerate() {
                    let call_id = codex_file_change_call_id(&item_id, idx, total);
                    emit_modify_file_request_to(
                        emitter,
                        &call_id,
                        &change.path,
                        &change.before,
                        &change.after,
                    );
                }
            }
            "collabToolCall" | "collabAgentToolCall" | "mcpToolCall" | "dynamicToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type)
                    .to_string();
                emitter.tool_request(
                    &item_id,
                    &tool_name,
                    json!({
                        "kind": "Other",
                        "args": item
                    }),
                );
            }
            _ => {}
        }
    }

    async fn handle_subagent_item_completed(&self, params: &Value, stream_key: &str, model: &str) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("item")
            .to_string();

        match item_type {
            "agentMessage" => {
                let Some(provider_item_id) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                else {
                    self.reject_subagent_message_identity(
                        stream_key,
                        StreamIdentityViolation::MissingMessageId,
                        "item/completed",
                    )
                    .await;
                    return;
                };
                let message_id = ChatMessageId(provider_item_id.to_string());
                let text = extract_codex_item_text(item);
                let reasoning = extract_codex_item_reasoning(item);
                let turn_id_from_params = extract_turn_id(params);
                let Some((
                    emitter,
                    token_usage,
                    unavailable_reason,
                    synthetic_start,
                    content,
                    final_reasoning,
                )) = self
                    .complete_subagent_message(
                        stream_key,
                        turn_id_from_params,
                        message_id.clone(),
                        model.to_string(),
                        text,
                        reasoning,
                    )
                    .await
                else {
                    return;
                };
                if synthetic_start {
                    emitter.stream_start_with_id(
                        message_id.clone(),
                        AgentName(CODEX_AGENT_NAME),
                        Some(model),
                    );
                    if let Some(reasoning) = final_reasoning.as_deref() {
                        emitter.stream_reasoning_delta_with_id(message_id.clone(), reasoning);
                    }
                }
                emitter.stream_end_with_id(
                    message_id,
                    StreamEndPayload {
                        content,
                        agent: Some(AgentName(CODEX_AGENT_NAME)),
                        model: Some(model.to_string()),
                        request_usage: token_usage.clone(),
                        turn_usage: token_usage,
                        cumulative_usage: None,
                        token_usage_unavailable_reason: unavailable_reason,
                        reasoning: final_reasoning,
                        tool_calls: Vec::new(),
                        context_breakdown: None,
                    },
                );
            }
            "reasoning" => {
                self.complete_subagent_reasoning_item(
                    stream_key,
                    extract_turn_id(params),
                    item.get("id")
                        .and_then(Value::as_str)
                        .filter(|item_id| !item_id.trim().is_empty())
                        .map(|item_id| ChatMessageId(item_id.to_string())),
                    model,
                    extract_codex_item_reasoning(item)
                        .filter(|reasoning| contains_non_whitespace(reasoning)),
                )
                .await;
            }
            "commandExecution" => {
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                let output = item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let success = exit_code == 0;
                let error_message = if success {
                    None
                } else {
                    Some(format!("Command failed with exit code {exit_code}"))
                };
                emitter.tool_completed(ToolCompletedPayload {
                    tool_call_id: &item_id,
                    tool_name: "run_command",
                    tool_result: json!({
                        "kind": "RunCommand",
                        "exit_code": exit_code,
                        "stdout": output,
                        "stderr": ""
                    }),
                    success,
                    error: error_message.as_deref(),
                });
            }
            "fileChange" => {
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let success = item.get("status").and_then(Value::as_str) == Some("completed");
                let file_changes = parse_codex_file_changes(item);
                let err_str = if success {
                    None
                } else {
                    Some("File changes were not applied")
                };
                if file_changes.is_empty() {
                    emitter.tool_completed(ToolCompletedPayload {
                        tool_call_id: &item_id,
                        tool_name: "file_change",
                        tool_result: json!({
                            "kind": "Other",
                            "result": item
                        }),
                        success,
                        error: err_str,
                    });
                    return;
                }
                let total = file_changes.len();
                for (idx, change) in file_changes.iter().enumerate() {
                    let call_id = codex_file_change_call_id(&item_id, idx, total);
                    emitter.tool_completed(ToolCompletedPayload {
                        tool_call_id: &call_id,
                        tool_name: "modify_file",
                        tool_result: json!({
                            "kind": "ModifyFile",
                            "lines_added": change.lines_added,
                            "lines_removed": change.lines_removed
                        }),
                        success,
                        error: err_str,
                    });
                }
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type);
                let success = item.get("status").and_then(Value::as_str) == Some("completed")
                    || item.get("success").and_then(Value::as_bool) == Some(true);
                let error_message = if success {
                    None
                } else {
                    Some(format!("{tool_name} failed"))
                };
                emitter.tool_completed(ToolCompletedPayload {
                    tool_call_id: &item_id,
                    tool_name,
                    tool_result: json!({
                        "kind": "Other",
                        "result": item
                    }),
                    success,
                    error: error_message.as_deref(),
                });
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("collab_tool");
                let success = codex_item_success(item);
                let error_message = if success {
                    None
                } else {
                    Some(format!("{tool_name} failed"))
                };
                emitter.tool_completed(ToolCompletedPayload {
                    tool_call_id: &item_id,
                    tool_name,
                    tool_result: json!({
                        "kind": "Other",
                        "result": item
                    }),
                    success,
                    error: error_message.as_deref(),
                });
            }
            _ => {}
        }
    }

    async fn complete_subagent_message(
        &self,
        stream_key: &str,
        turn_id_from_params: Option<String>,
        message_id: ChatMessageId,
        model: String,
        completion_text: String,
        completion_reasoning: Option<String>,
    ) -> Option<(
        Arc<TurnEmitter>,
        Option<Value>,
        Option<TokenUsageUnavailableReason>,
        bool,
        String,
        Option<String>,
    )> {
        let result = {
            let mut state = self.state.lock().await;
            let stream = state.subagent_streams.get_mut(stream_key)?;
            if stream.quarantined {
                return None;
            }
            let completion = CompletedCodexAgentMessage {
                completion_text: completion_text.clone(),
                completion_reasoning: completion_reasoning.clone(),
                generated_identity: None,
            };
            if let Some(previous) = stream.completed_agent_messages.get(&message_id) {
                if previous == &completion {
                    Ok(None)
                } else {
                    Err(StreamIdentityViolation::ConflictingDuplicateCompletion)
                }
            } else if stream
                .current_message_id
                .as_ref()
                .is_some_and(|active_message_id| {
                    active_message_id != &message_id || stream.current_reasoning_only
                })
            {
                Err(StreamIdentityViolation::ForeignActiveMessageId)
            } else {
                let synthetic_start = stream.current_message_id.is_none();
                let content = if completion_text.is_empty() {
                    stream.current_text.clone()
                } else {
                    completion_text
                };
                let reasoning = if stream.current_reasoning.trim().is_empty() {
                    completion_reasoning
                } else {
                    Some(stream.current_reasoning.clone())
                }
                .filter(|reasoning| contains_non_whitespace(reasoning));
                let turn_id = turn_id_from_params
                    .or_else(|| stream.active_turn_id.clone())
                    .unwrap_or_else(|| "turn".to_string());
                stream.current_message_id = None;
                stream.current_generated_identity = None;
                stream.current_reasoning_only = false;
                stream.current_text.clear();
                stream.current_reasoning.clear();
                stream
                    .completed_agent_messages
                    .insert(message_id.clone(), completion);
                let token_usage = stream.token_usage_by_turn.remove(&turn_id);
                let unavailable_reason = if token_usage.is_some() {
                    None
                } else {
                    stream.pending_message_metadata = Some(PendingCodexMessageMetadata {
                        turn_id,
                        message_id: message_id.clone(),
                        model,
                        turn_context: TurnContextEstimate::default(),
                    });
                    Some(TokenUsageUnavailableReason::BackendDidNotReport)
                };
                Ok(Some((
                    Arc::clone(&stream.emitter),
                    token_usage,
                    unavailable_reason,
                    synthetic_start,
                    content,
                    reasoning,
                )))
            }
        };
        match result {
            Ok(result) => result,
            Err(violation) => {
                self.reject_subagent_message_identity(stream_key, violation, "item/completed")
                    .await;
                None
            }
        }
    }

    async fn complete_subagent_reasoning_item(
        &self,
        stream_key: &str,
        turn_id_from_params: Option<String>,
        provider_message_id: Option<ChatMessageId>,
        model: &str,
        completion_reasoning: Option<String>,
    ) {
        let raw_completion_reasoning = completion_reasoning.clone();
        let result = {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return;
            };
            if stream.quarantined {
                return;
            }
            let matches_active = stream.current_reasoning_only
                && match provider_message_id.as_ref() {
                    Some(message_id) => stream.current_message_id.as_ref() == Some(message_id),
                    None => stream
                        .current_generated_identity
                        .as_ref()
                        .is_some_and(|identity| {
                            identity.origin == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                        }),
                };
            if stream.current_message_id.is_some() && !matches_active {
                Err(StreamIdentityViolation::ForeignActiveMessageId)
            } else {
                let generated_identity = if stream.current_message_id.is_some() {
                    stream.current_generated_identity.clone()
                } else {
                    provider_message_id.is_none().then(|| {
                        let identity = ServerGeneratedChatMessageIdentity {
                            origin: ServerGeneratedChatMessageIdOrigin::IdlessReasoning,
                            stream_epoch: stream.generated_identity_epoch,
                            item_ordinal: stream.next_generated_identity_ordinal,
                        };
                        stream.next_generated_identity_ordinal =
                            stream.next_generated_identity_ordinal.saturating_add(1);
                        identity
                    })
                };
                let message_id = stream.current_message_id.clone().unwrap_or_else(|| {
                    provider_message_id.clone().unwrap_or_else(|| {
                        generated_identity
                            .as_ref()
                            .expect("generated child reasoning identity")
                            .message_id()
                    })
                });
                let synthetic_start = stream.current_message_id.is_none();
                let reasoning = completion_reasoning.or_else(|| {
                    contains_non_whitespace(&stream.current_reasoning)
                        .then_some(stream.current_reasoning.clone())
                });
                let completion = CompletedCodexAgentMessage {
                    completion_text: String::new(),
                    completion_reasoning: raw_completion_reasoning,
                    generated_identity: generated_identity.clone(),
                };
                if let Some(previous) = stream.completed_agent_messages.get(&message_id) {
                    if previous == &completion {
                        return;
                    }
                    Err(StreamIdentityViolation::ConflictingDuplicateCompletion)
                } else {
                    let turn_id = turn_id_from_params
                        .or_else(|| stream.active_turn_id.clone())
                        .unwrap_or_else(|| "turn".to_string());
                    stream.current_message_id = None;
                    stream.current_generated_identity = None;
                    stream.current_reasoning_only = false;
                    stream.current_text.clear();
                    stream.current_reasoning.clear();
                    stream
                        .completed_agent_messages
                        .insert(message_id.clone(), completion);
                    let token_usage = stream.token_usage_by_turn.remove(&turn_id);
                    let unavailable_reason = if token_usage.is_some() {
                        None
                    } else {
                        stream.pending_message_metadata = Some(PendingCodexMessageMetadata {
                            turn_id,
                            message_id: message_id.clone(),
                            model: model.to_string(),
                            turn_context: TurnContextEstimate::default(),
                        });
                        Some(TokenUsageUnavailableReason::BackendDidNotReport)
                    };
                    Ok((
                        Arc::clone(&stream.emitter),
                        message_id,
                        generated_identity,
                        synthetic_start,
                        reasoning,
                        token_usage,
                        unavailable_reason,
                    ))
                }
            }
        };
        let (
            emitter,
            message_id,
            generated_identity,
            synthetic_start,
            reasoning,
            token_usage,
            unavailable_reason,
        ) = match result {
            Ok(result) => result,
            Err(violation) => {
                self.reject_subagent_message_identity(stream_key, violation, "item/completed")
                    .await;
                return;
            }
        };
        if synthetic_start {
            Self::emit_stream_start(
                emitter.as_ref(),
                message_id.clone(),
                generated_identity.as_ref(),
                model,
            );
            if let Some(reasoning) = reasoning.as_deref() {
                emitter.stream_reasoning_delta_with_id(message_id.clone(), reasoning);
            }
        }
        emitter.stream_end_with_id(
            message_id,
            StreamEndPayload {
                content: String::new(),
                agent: Some(AgentName(CODEX_AGENT_NAME)),
                model: Some(model.to_string()),
                request_usage: token_usage.clone(),
                turn_usage: token_usage,
                cumulative_usage: None,
                token_usage_unavailable_reason: unavailable_reason,
                reasoning,
                tool_calls: Vec::new(),
                context_breakdown: None,
            },
        );
    }

    async fn handle_subagent_token_usage_updated(
        &self,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        let Some((turn_id, token_usage)) = extract_turn_token_usage(params, Some(model)) else {
            return;
        };
        if let Some((emitter, pending, token_usage, context_breakdown)) = self
            .record_subagent_token_usage(stream_key, turn_id, token_usage)
            .await
        {
            emit_codex_message_metadata_update(
                emitter.as_ref(),
                pending,
                Some(token_usage),
                None,
                context_breakdown,
            );
        }
    }

    async fn record_subagent_token_usage(
        &self,
        stream_key: &str,
        turn_id: String,
        token_usage: Value,
    ) -> Option<(Arc<TurnEmitter>, PendingCodexMessageMetadata, Value, Value)> {
        let mut state = self.state.lock().await;
        let stream = state.subagent_streams.get_mut(stream_key)?;
        if stream.quarantined {
            return None;
        }
        stream
            .token_usage_by_turn
            .insert(turn_id.clone(), token_usage.clone());
        let pending_ready = stream
            .pending_message_metadata
            .as_ref()
            .is_some_and(|pending| pending.turn_id == turn_id);
        if !pending_ready {
            return None;
        }
        let pending = stream.pending_message_metadata.take()?;
        let token_usage = stream.token_usage_by_turn.remove(&turn_id)?;
        let context_breakdown = estimate_context_breakdown(
            Some(&token_usage),
            &pending.turn_context,
            Some(&pending.model),
        );
        Some((
            Arc::clone(&stream.emitter),
            pending,
            token_usage,
            context_breakdown,
        ))
    }

    async fn handle_completed_subagent_token_usage(
        &self,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        let Some((turn_id, token_usage)) = extract_turn_token_usage(params, Some(model)) else {
            return;
        };
        if let Some((emitter, pending, token_usage, context_breakdown)) = self
            .record_completed_subagent_token_usage(stream_key, turn_id, token_usage)
            .await
        {
            emit_codex_message_metadata_update(
                emitter.as_ref(),
                pending,
                Some(token_usage),
                None,
                context_breakdown,
            );
        }
    }

    async fn record_completed_subagent_token_usage(
        &self,
        stream_key: &str,
        turn_id: String,
        token_usage: Value,
    ) -> Option<(Arc<TurnEmitter>, PendingCodexMessageMetadata, Value, Value)> {
        let mut state = self.state.lock().await;
        let stream = state.completed_subagent_streams.get_mut(stream_key)?;
        let pending_ready = stream
            .pending_message_metadata
            .as_ref()
            .is_some_and(|pending| pending.turn_id == turn_id);
        if !pending_ready {
            return None;
        }
        let pending = stream.pending_message_metadata.take()?;
        let context_breakdown = estimate_context_breakdown(
            Some(&token_usage),
            &pending.turn_context,
            Some(&pending.model),
        );
        Some((
            Arc::clone(&stream.emitter),
            pending,
            token_usage,
            context_breakdown,
        ))
    }

    async fn handle_subagent_turn_completed(&self, params: &Value, stream_key: &str, model: &str) {
        let turn_status = params
            .get("turn")
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        if let Some((turn_id, token_usage)) = extract_turn_token_usage(params, Some(model))
            && let Some((emitter, pending, token_usage, context_breakdown)) = self
                .record_subagent_token_usage(stream_key, turn_id, token_usage)
                .await
        {
            emit_codex_message_metadata_update(
                emitter.as_ref(),
                pending,
                Some(token_usage),
                None,
                context_breakdown,
            );
        }
        let Some((emitter, quarantined, open_item, partial_idless_reasoning)) = ({
            let mut state = self.state.lock().await;
            state.subagent_streams.get_mut(stream_key).map(|stream| {
                let open_item = stream.current_message_id.is_some();
                let mut partial_idless_reasoning = None;
                if extract_turn_id(params)
                    .as_ref()
                    .is_none_or(|turn_id| stream.active_turn_id.as_ref() == Some(turn_id))
                {
                    let durable_idless_reasoning = stream.current_reasoning_only
                        && stream
                            .current_generated_identity
                            .as_ref()
                            .is_some_and(|identity| {
                                identity.origin
                                    == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                            })
                        && contains_non_whitespace(&stream.current_reasoning);
                    if durable_idless_reasoning
                        && let Some(message_id) = stream.current_message_id.clone()
                    {
                        let reasoning = stream.current_reasoning.clone();
                        stream.completed_agent_messages.insert(
                            message_id.clone(),
                            CompletedCodexAgentMessage {
                                completion_text: String::new(),
                                completion_reasoning: Some(reasoning.clone()),
                                generated_identity: stream.current_generated_identity.clone(),
                            },
                        );
                        partial_idless_reasoning = Some((message_id, reasoning));
                    }
                    stream.active_turn_id = None;
                    stream.current_message_id = None;
                    stream.current_generated_identity = None;
                    stream.current_reasoning_only = false;
                    stream.current_text.clear();
                    stream.current_reasoning.clear();
                    if open_item {
                        stream.quarantined = true;
                        stream.quarantined_turn_id = extract_turn_id(params);
                    }
                }
                (
                    Arc::clone(&stream.emitter),
                    stream.quarantined,
                    open_item,
                    partial_idless_reasoning,
                )
            })
        }) else {
            return;
        };

        if let Some((message_id, reasoning)) = partial_idless_reasoning {
            emitter.stream_end_with_id(
                message_id,
                StreamEndPayload {
                    content: String::new(),
                    agent: Some(AgentName(CODEX_AGENT_NAME)),
                    model: Some(model.to_string()),
                    request_usage: None,
                    turn_usage: None,
                    cumulative_usage: None,
                    token_usage_unavailable_reason: None,
                    reasoning: Some(reasoning),
                    tool_calls: Vec::new(),
                    context_breakdown: None,
                },
            );
            emitter.operation_cancelled("Codex child turn ended before reasoning item completion");
            return;
        }

        if quarantined {
            if open_item {
                emitter.discard_open_stream_with_identity_violation(
                    StreamIdentityViolation::MismatchedEndMessageId,
                );
            }
            return;
        }

        if turn_status == "interrupted" {
            emitter.operation_cancelled("Operation cancelled");
        } else {
            emitter.typing_status_changed(false);
            if turn_status == "failed" {
                let message = params
                    .get("turn")
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex turn failed")
                    .to_string();
                emitter.backend_error(&message);
            }
        }
        tracing::debug!(
            thread_id = stream_key,
            status = turn_status,
            "Codex child turn completed; retaining ownership for possible follow-up"
        );
    }

    async fn handle_legacy_codex_event(&self, method: &str, params: &Value) {
        let Some(delta) = extract_reasoning_delta_from_legacy_codex_event(method, params) else {
            return;
        };
        self.emit_reasoning_delta(delta).await;
    }

    async fn emit_reasoning_delta(&self, delta: String) {
        match self.open_reasoning_message_item(None).await {
            CodexAgentMessageOpen::Open {
                message_id,
                generated_identity,
                model,
            } => {
                self.emitter.typing_status_changed(true);
                Self::emit_stream_start(
                    self.emitter.as_ref(),
                    message_id,
                    generated_identity.as_ref(),
                    &model,
                );
            }
            CodexAgentMessageOpen::Existing => {}
            CodexAgentMessageOpen::Quarantined => return,
            CodexAgentMessageOpen::Terminal => {
                self.reject_agent_message_identity(
                    StreamIdentityViolation::DuplicateTerminalMessageId,
                    "codex/event/reasoning",
                    None,
                )
                .await;
                return;
            }
            CodexAgentMessageOpen::Foreign => {
                self.reject_agent_message_identity(
                    StreamIdentityViolation::ForeignActiveMessageId,
                    "codex/event/reasoning",
                    None,
                )
                .await;
                return;
            }
        }
        self.append_reasoning_to_active_stream(&delta).await;
    }

    async fn handle_error_notification(&self, params: &Value) {
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex error")
            .to_string();
        let terminal = {
            let state = self.state.lock().await;
            is_terminal_codex_error_notification(&state, params)
        };

        if terminal {
            self.complete_all_codex_subagents().await;
            self.emitter.backend_error(&message);
            self.emitter.typing_status_changed(false);
            return;
        }

        self.emitter
            .subprocess_stderr(&format!("Codex warning: {message}"));
    }

    async fn handle_server_request(&self, id: Value, method: &str, params: &Value) {
        let inference_only =
            self.state.lock().await.execution_mode == BackendExecutionMode::InferenceOnly;
        if inference_only && is_codex_tool_server_request(method) {
            let response = match method {
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                    json!({ "decision": "decline" })
                }
                "execCommandApproval" | "applyPatchApproval" => {
                    json!({ "decision": "denied" })
                }
                "mcpServer/elicitation/request" => json!({ "action": "cancel" }),
                "item/tool/requestUserInput" => json!({ "answers": {} }),
                "item/tool/call" => json!({
                    "success": false,
                    "contentItems": [{
                        "type": "inputText",
                        "text": "Transient inference does not permit tools."
                    }]
                }),
                _ => json!({ "decision": "decline" }),
            };
            if let Err(err) = self.rpc.respond(id, response).await {
                self.emitter.backend_error(&format!(
                    "Codex transient inference failed to reject tool request '{method}': {err}"
                ));
            } else {
                self.emitter.backend_error(&format!(
                    "Codex transient inference rejected tool request '{method}'"
                ));
            }
            self.emitter.typing_status_changed(false);
            return;
        }

        match method {
            "item/commandExecution/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("approval")
                    .to_string();
                let tool_call_id = format!("approval-{item_id}");
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        params
                            .get("command")
                            .and_then(Value::as_str)
                            .map(|cmd| format!("Approve command: {cmd}"))
                    })
                    .unwrap_or_else(|| "Approve pending command?".to_string());

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::CommandApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emitter.tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    json!({
                        "kind": "Other",
                        "args": {
                            "question": question,
                            "type": "command_approval"
                        }
                    }),
                );
            }
            "item/fileChange/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("file-approval")
                    .to_string();
                let tool_call_id = format!("file-approval-{item_id}");
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Approve pending file changes?")
                    .to_string();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::FileChangeApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emitter.tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    json!({
                        "kind": "Other",
                        "args": {
                            "question": question,
                            "type": "file_change_approval"
                        }
                    }),
                );
            }
            "execCommandApproval" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("exec-approval")
                    .to_string();
                let tool_call_id = format!("exec-approval-{call_id}");
                let command_text = params
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        if command_text.is_empty() {
                            None
                        } else {
                            Some(format!("Approve command: {command_text}"))
                        }
                    })
                    .unwrap_or_else(|| "Approve pending command?".to_string());

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::ExecCommandApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emitter.tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    json!({
                        "kind": "Other",
                        "args": {
                            "question": question,
                            "type": "command_approval"
                        }
                    }),
                );
            }
            "applyPatchApproval" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("patch-approval")
                    .to_string();
                let tool_call_id = format!("patch-approval-{call_id}");
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Approve pending file changes?")
                    .to_string();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::ApplyPatchApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emitter.tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    json!({
                        "kind": "Other",
                        "args": {
                            "question": question,
                            "type": "file_change_approval"
                        }
                    }),
                );
            }
            "item/tool/requestUserInput" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("request-user-input")
                    .to_string();
                let tool_call_id = format!("request-user-input-{item_id}");
                let questions = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let question_ids = questions
                    .iter()
                    .filter_map(|q| q.get("id").and_then(Value::as_str).map(|s| s.to_string()))
                    .collect::<Vec<_>>();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::UserInput {
                            questions: question_ids,
                        },
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emitter.tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    json!({
                        "kind": "Other",
                        "args": {
                            "questions": questions,
                            "type": "request_user_input"
                        }
                    }),
                );
            }
            "mcpServer/elicitation/request" => {
                let result = codex_mcp_elicitation_result(params);
                if let Err(err) = self.rpc.respond(id, result).await {
                    self.emitter.subprocess_stderr(&format!(
                        "Failed to resolve Codex MCP elicitation request: {err}"
                    ));
                }
            }
            "item/tool/call" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("dynamic-tool-call");
                let tool_name = params
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("dynamic_tool");

                self.track_tool_requests(std::iter::once(call_id.to_string()))
                    .await;
                self.emitter.tool_request(
                    call_id,
                    tool_name,
                    json!({
                        "kind": "Other",
                        "args": {
                            "type": "dynamic_tool_call",
                            "arguments": params.get("arguments").cloned().unwrap_or(Value::Null)
                        }
                    }),
                );

                let response_payload = json!({
                    "success": false,
                    "contentItems": [
                        {
                            "type": "inputText",
                            "text": "Dynamic client tool calls are not yet supported in Tyde."
                        }
                    ]
                });
                let _ = self.rpc.respond(id, response_payload).await;
                self.emit_tool_execution_completed(
                    call_id,
                    tool_name,
                    false,
                    json!({
                        "kind": "Error",
                        "short_message": "Dynamic client tool calls are not yet supported in Tyde.",
                        "detailed_message": "Codex requested a client-side dynamic tool call that Tyde has not implemented yet."
                    }),
                    Some("Dynamic client tool calls are not yet supported in Tyde.".to_string()),
                )
                .await;
            }
            _ => {
                let _ = self
                    .rpc
                    .respond(
                        id,
                        json!({"ignored": true, "reason": "unsupported_server_request"}),
                    )
                    .await;
            }
        }
    }

    async fn add_active_turn_tool_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock().await;
        let Some(turn_id) = state.active_turn_id.as_ref().cloned() else {
            return;
        };
        let estimate = state.turn_context_by_turn.entry(turn_id).or_default();
        estimate.tool_io_bytes = estimate.tool_io_bytes.saturating_add(bytes);
    }

    async fn handle_item_started(&self, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item.get("id").and_then(Value::as_str);

        if self.state.lock().await.quarantined_turn_id.is_some() {
            return;
        }

        match item_type {
            "agentMessage" => {
                let Some(item_id) = item_id.filter(|item_id| !item_id.trim().is_empty()) else {
                    self.reject_agent_message_identity(
                        StreamIdentityViolation::MissingMessageId,
                        "item/started",
                        None,
                    )
                    .await;
                    return;
                };
                let message_id = ChatMessageId(item_id.to_string());
                match self.open_agent_message_item(message_id.clone()).await {
                    CodexAgentMessageOpen::Open {
                        message_id,
                        generated_identity,
                        model,
                    } => {
                        self.emitter.typing_status_changed(true);
                        Self::emit_stream_start(
                            self.emitter.as_ref(),
                            message_id,
                            generated_identity.as_ref(),
                            &model,
                        );
                    }
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::DuplicateTerminalMessageId,
                            "item/started",
                            Some(&message_id.0),
                        )
                        .await;
                    }
                    CodexAgentMessageOpen::Quarantined => (),
                    CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::ForeignActiveMessageId,
                            "item/started",
                            Some(&message_id.0),
                        )
                        .await;
                    }
                }
            }
            "reasoning" => {
                let provider_message_id = item_id
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                match self
                    .open_reasoning_message_item(provider_message_id.clone())
                    .await
                {
                    CodexAgentMessageOpen::Open {
                        message_id,
                        generated_identity,
                        model,
                    } => {
                        self.emitter.typing_status_changed(true);
                        Self::emit_stream_start(
                            self.emitter.as_ref(),
                            message_id,
                            generated_identity.as_ref(),
                            &model,
                        );
                    }
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::DuplicateTerminalMessageId,
                            "item/started",
                            provider_message_id
                                .as_ref()
                                .map(|message_id| message_id.0.as_str()),
                        )
                        .await;
                    }
                    CodexAgentMessageOpen::Quarantined => {}
                    CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            StreamIdentityViolation::ForeignActiveMessageId,
                            "item/started",
                            provider_message_id
                                .as_ref()
                                .map(|message_id| message_id.0.as_str()),
                        )
                        .await;
                    }
                }
            }
            "commandExecution" => {
                let item_id = item_id.unwrap_or("tool-call").to_string();
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                self.emitter.tool_request(
                    &item_id,
                    "run_command",
                    json!({
                        "kind": "RunCommand",
                        "command": command,
                        "working_directory": cwd
                    }),
                );
            }
            "fileChange" => {
                let item_id = item_id.unwrap_or("tool-call").to_string();
                let file_changes = parse_codex_file_changes(item);
                if file_changes.is_empty() {
                    return;
                }

                let total = file_changes.len();
                let call_ids = file_changes
                    .iter()
                    .enumerate()
                    .map(|(idx, _)| codex_file_change_call_id(&item_id, idx, total))
                    .collect::<Vec<_>>();

                {
                    let mut state = self.state.lock().await;
                    state
                        .file_change_call_ids
                        .insert(item_id.clone(), call_ids.clone());
                }

                self.track_tool_requests(call_ids.clone()).await;
                for (change, call_id) in file_changes.into_iter().zip(call_ids) {
                    self.emit_modify_file_request(
                        &call_id,
                        &change.path,
                        &change.before,
                        &change.after,
                    );
                }
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let item_id = item_id.unwrap_or("tool-call").to_string();
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("collab_tool")
                    .to_string();
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                emit_codex_tool_request(&self.emitter, &item_id, &tool_name, item);
                self.emit_agent_control_await_progress_if_needed(&item_id, &tool_name, item);
                self.record_codex_subagent_spawn_metadata_if_needed(item)
                    .await;
            }
            "subAgentActivity" | "sub_agent_activity" => {
                self.register_codex_subagent_activity_if_needed(item).await;
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let item_id = item_id.unwrap_or("tool-call").to_string();
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type)
                    .to_string();
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                emit_codex_tool_request(&self.emitter, &item_id, &tool_name, item);
                self.emit_agent_control_await_progress_if_needed(&item_id, &tool_name, item);
            }
            _ => {}
        }
    }

    async fn handle_item_completed(&self, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };

        if self.state.lock().await.quarantined_turn_id.is_some() {
            return;
        }

        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item.get("id").and_then(Value::as_str);

        match item_type {
            "agentMessage" => {
                let Some(item_id) = item_id.filter(|item_id| !item_id.trim().is_empty()) else {
                    self.reject_agent_message_identity(
                        StreamIdentityViolation::MissingMessageId,
                        "item/completed",
                        None,
                    )
                    .await;
                    return;
                };
                let message_id = ChatMessageId(item_id.to_string());
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let completion_reasoning = extract_codex_item_reasoning(item);
                let completion = CompletedCodexAgentMessage {
                    completion_text: text.clone(),
                    completion_reasoning: completion_reasoning.clone(),
                    generated_identity: None,
                };
                let result = {
                    let mut state = self.state.lock().await;
                    if let Some(previous) = state.completed_agent_messages.get(&message_id) {
                        if previous == &completion {
                            None
                        } else {
                            Some(Err(StreamIdentityViolation::ConflictingDuplicateCompletion))
                        }
                    } else if let Some(active) = state.active_stream.as_ref() {
                        if active.message_id != message_id || active.reasoning_only {
                            Some(Err(StreamIdentityViolation::ForeignActiveMessageId))
                        } else {
                            let stream = state
                                .active_stream
                                .take()
                                .expect("active Codex stream disappeared while completing item");
                            Some(Ok((stream, false)))
                        }
                    } else {
                        Some(Ok((
                            ActiveStreamState {
                                turn_id: state
                                    .active_turn_id
                                    .clone()
                                    .unwrap_or_else(|| "turn".to_string()),
                                message_id: message_id.clone(),
                                generated_identity: None,
                                text: String::new(),
                                reasoning: String::new(),
                                reasoning_only: false,
                            },
                            true,
                        )))
                    }
                };
                let Some(result) = result else {
                    tracing::debug!(
                        provider_item_id = message_id.0.as_str(),
                        "Ignoring idempotent duplicate Codex agentMessage completion"
                    );
                    return;
                };
                let (stream, synthetic_start) = match result {
                    Ok(stream) => stream,
                    Err(violation) => {
                        self.reject_agent_message_identity(
                            violation,
                            "item/completed",
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                };
                let content = if text.is_empty() { stream.text } else { text };
                let reasoning = if stream.reasoning.trim().is_empty() {
                    completion_reasoning
                } else {
                    Some(stream.reasoning)
                }
                .filter(|reasoning| contains_non_whitespace(reasoning));
                let model = {
                    let mut state = self.state.lock().await;
                    state.close_active_stream_when_tools_idle = false;
                    state.conversation_bytes_total = state
                        .conversation_bytes_total
                        .saturating_add(content.len() as u64);
                    let model = state.model.clone().unwrap_or_else(|| "codex".to_string());
                    let turn_context = state
                        .turn_context_by_turn
                        .get(&stream.turn_id)
                        .cloned()
                        .unwrap_or_default();
                    let metadata = metadata_target_for_visible_message(
                        stream.turn_id,
                        stream.message_id.clone(),
                        &content,
                        reasoning.as_deref(),
                        model.clone(),
                        turn_context,
                    );
                    if let Some(metadata) = metadata {
                        state.pending_message_metadata = Some(metadata);
                    }
                    state
                        .completed_agent_messages
                        .insert(message_id.clone(), completion);
                    model
                };
                if synthetic_start {
                    Self::emit_stream_start(
                        self.emitter.as_ref(),
                        message_id.clone(),
                        None,
                        &model,
                    );
                    if let Some(reasoning) = reasoning.as_deref() {
                        self.emitter
                            .stream_reasoning_delta_with_id(message_id.clone(), reasoning);
                    }
                }
                self.emitter.stream_end_with_id(
                    message_id,
                    StreamEndPayload {
                        content,
                        agent: Some(AgentName(CODEX_AGENT_NAME)),
                        model: Some(model),
                        request_usage: None,
                        turn_usage: None,
                        cumulative_usage: None,
                        token_usage_unavailable_reason: None,
                        reasoning,
                        tool_calls: Vec::new(),
                        context_breakdown: None,
                    },
                );
            }
            "subAgentActivity" | "sub_agent_activity" => {
                self.register_codex_subagent_activity_if_needed(item).await;
            }
            "userMessage" => {
                // User messages are emitted synchronously when sent to keep ordering stable.
            }
            "reasoning" => {
                let completion_reasoning = extract_codex_item_reasoning(item)
                    .filter(|reasoning| contains_non_whitespace(reasoning));
                let raw_completion_reasoning = completion_reasoning.clone();
                let provider_message_id = item_id
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                let result = {
                    let mut state = self.state.lock().await;
                    let matches_active = |stream: &ActiveStreamState| {
                        if !stream.reasoning_only {
                            return false;
                        }
                        match provider_message_id.as_ref() {
                            Some(message_id) => stream.message_id == *message_id,
                            None => stream.generated_identity.as_ref().is_some_and(|identity| {
                                identity.origin
                                    == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                            }),
                        }
                    };
                    if let Some(message_id) = provider_message_id.as_ref()
                        && let Some(previous) = state.completed_agent_messages.get(message_id)
                    {
                        let completion = CompletedCodexAgentMessage {
                            completion_text: String::new(),
                            completion_reasoning: completion_reasoning.clone(),
                            generated_identity: None,
                        };
                        if previous == &completion {
                            None
                        } else {
                            Some(Err(StreamIdentityViolation::ConflictingDuplicateCompletion))
                        }
                    } else if let Some(active) = state.active_stream.as_ref() {
                        if matches_active(active) {
                            let stream = state
                                .active_stream
                                .take()
                                .expect("active Codex reasoning stream disappeared");
                            Some(Ok((stream, false)))
                        } else {
                            Some(Err(StreamIdentityViolation::ForeignActiveMessageId))
                        }
                    } else {
                        let generated_identity = provider_message_id.is_none().then(|| {
                            let identity = ServerGeneratedChatMessageIdentity {
                                origin: ServerGeneratedChatMessageIdOrigin::IdlessReasoning,
                                stream_epoch: state.generated_identity_epoch,
                                item_ordinal: state.next_generated_identity_ordinal,
                            };
                            state.next_generated_identity_ordinal =
                                state.next_generated_identity_ordinal.saturating_add(1);
                            identity
                        });
                        let message_id = provider_message_id.clone().unwrap_or_else(|| {
                            generated_identity
                                .as_ref()
                                .expect("generated reasoning identity")
                                .message_id()
                        });
                        Some(Ok((
                            ActiveStreamState {
                                turn_id: state
                                    .active_turn_id
                                    .clone()
                                    .unwrap_or_else(|| "turn".to_string()),
                                message_id,
                                generated_identity,
                                text: String::new(),
                                reasoning: String::new(),
                                reasoning_only: true,
                            },
                            true,
                        )))
                    }
                };
                let Some(result) = result else {
                    return;
                };
                let (stream, synthetic_start) = match result {
                    Ok(result) => result,
                    Err(violation) => {
                        self.reject_agent_message_identity(
                            violation,
                            "item/completed",
                            provider_message_id
                                .as_ref()
                                .map(|message_id| message_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                };
                let reasoning = completion_reasoning.or_else(|| {
                    contains_non_whitespace(&stream.reasoning).then_some(stream.reasoning.clone())
                });
                let completion = CompletedCodexAgentMessage {
                    completion_text: String::new(),
                    completion_reasoning: raw_completion_reasoning,
                    generated_identity: stream.generated_identity.clone(),
                };
                let model = {
                    let mut state = self.state.lock().await;
                    let model = state.model.clone().unwrap_or_else(|| "codex".to_string());
                    let turn_context = state
                        .turn_context_by_turn
                        .get(&stream.turn_id)
                        .cloned()
                        .unwrap_or_default();
                    state.pending_message_metadata = metadata_target_for_visible_message(
                        stream.turn_id,
                        stream.message_id.clone(),
                        "",
                        reasoning.as_deref(),
                        model.clone(),
                        turn_context,
                    );
                    state
                        .completed_agent_messages
                        .insert(stream.message_id.clone(), completion);
                    model
                };
                if synthetic_start {
                    Self::emit_stream_start(
                        self.emitter.as_ref(),
                        stream.message_id.clone(),
                        stream.generated_identity.as_ref(),
                        &model,
                    );
                    if let Some(reasoning) = reasoning.as_deref() {
                        self.emitter
                            .stream_reasoning_delta_with_id(stream.message_id.clone(), reasoning);
                    }
                }
                self.emitter.stream_end_with_id(
                    stream.message_id,
                    StreamEndPayload {
                        content: String::new(),
                        agent: Some(AgentName(CODEX_AGENT_NAME)),
                        model: Some(model),
                        request_usage: None,
                        turn_usage: None,
                        cumulative_usage: None,
                        token_usage_unavailable_reason: None,
                        reasoning,
                        tool_calls: Vec::new(),
                        context_breakdown: None,
                    },
                );
            }
            "commandExecution" => {
                let item_id = item_id.unwrap_or("item").to_string();
                self.add_active_turn_tool_bytes(estimate_command_execution_tool_bytes(item))
                    .await;
                let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                let output = item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let success = exit_code == 0;
                self.emit_tool_execution_completed(
                    &item_id,
                    "run_command",
                    success,
                    json!({
                        "kind": "RunCommand",
                        "exit_code": exit_code,
                        "stdout": output,
                        "stderr": ""
                    }),
                    if success {
                        None
                    } else {
                        Some(format!("Command failed with exit code {exit_code}"))
                    },
                )
                .await;
            }
            "fileChange" => {
                let item_id = item_id.unwrap_or("item").to_string();
                self.add_active_turn_tool_bytes(estimate_file_change_tool_bytes(item))
                    .await;
                let success = item.get("status").and_then(Value::as_str) == Some("completed");
                let known_call_ids = {
                    let mut state = self.state.lock().await;
                    state
                        .file_change_call_ids
                        .remove(&item_id)
                        .unwrap_or_default()
                };
                let file_changes = parse_codex_file_changes(item);
                let completions =
                    codex_file_change_completion_plan(&item_id, &known_call_ids, &file_changes);

                if !completions.is_empty() {
                    for completion in completions {
                        if let Some(change) = completion.request.as_ref() {
                            self.emit_modify_file_request(
                                &completion.call_id,
                                &change.path,
                                &change.before,
                                &change.after,
                            );
                        }

                        self.emit_tool_execution_completed(
                            &completion.call_id,
                            "modify_file",
                            success,
                            json!({
                                "kind": "ModifyFile",
                                "lines_added": completion.lines_added,
                                "lines_removed": completion.lines_removed
                            }),
                            if success {
                                None
                            } else {
                                Some("File changes were not applied".to_string())
                            },
                        )
                        .await;
                    }
                    return;
                }

                self.emit_tool_execution_completed(
                    &item_id,
                    "file_change",
                    success,
                    json!({
                        "kind": "Other",
                        "result": item
                    }),
                    if success {
                        None
                    } else {
                        Some("File changes were not applied".to_string())
                    },
                )
                .await;
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let item_id = item_id.unwrap_or("item").to_string();
                self.add_active_turn_tool_bytes(estimate_generic_tool_bytes(item))
                    .await;
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type);
                let success = item.get("status").and_then(Value::as_str) == Some("completed")
                    || item.get("success").and_then(Value::as_bool) == Some(true);
                if success {
                    self.emit_agent_control_spawn_progress_if_needed(&item_id, tool_name, item);
                }
                self.emit_tool_execution_completed(
                    &item_id,
                    tool_name,
                    success,
                    json!({
                        "kind": "Other",
                        "result": item
                    }),
                    if success {
                        None
                    } else {
                        Some(format!("{tool_name} failed"))
                    },
                )
                .await;
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let item_id = item_id.unwrap_or("item").to_string();
                self.add_active_turn_tool_bytes(estimate_generic_tool_bytes(item))
                    .await;
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("collab_tool");
                let success = codex_item_success(item);
                if success {
                    self.emit_agent_control_spawn_progress_if_needed(&item_id, tool_name, item);
                }
                self.emit_tool_execution_completed(
                    &item_id,
                    tool_name,
                    success,
                    json!({
                        "kind": "Other",
                        "result": item
                    }),
                    if success {
                        None
                    } else {
                        Some(format!("{tool_name} failed"))
                    },
                )
                .await;
                self.record_codex_subagent_spawn_metadata_if_needed(item)
                    .await;
            }
            _ => {}
        }
    }

    async fn record_codex_subagent_spawn_metadata_if_needed(&self, item: &Value) {
        let Some(spawn) = parse_codex_subagent_collab(item) else {
            return;
        };
        let receiver_thread_id = spawn.receiver_thread_id.clone();
        let conflict = {
            let mut state = self.state.lock().await;
            if spawn.sender_thread_id != state.thread_id {
                let message = format!(
                    "Codex ownership invariant failed: spawn metadata for child thread '{}' names sender '{}' instead of parent thread '{}'",
                    receiver_thread_id, spawn.sender_thread_id, state.thread_id
                );
                state
                    .conflicting_subagent_threads
                    .insert(receiver_thread_id.clone(), message.clone());
                Some(message)
            } else if let Some(existing) = state.pending_subagent_spawns.get(&receiver_thread_id) {
                if existing.item_id == spawn.item_id
                    && existing.sender_thread_id == spawn.sender_thread_id
                {
                    tracing::debug!(
                        receiver_thread_id = receiver_thread_id.as_str(),
                        "Repeated authoritative Codex child spawn metadata"
                    );
                    None
                } else {
                    Some(format!(
                        "Codex ownership invariant failed: child thread '{}' has contradictory pending spawn metadata ('{}' and '{}')",
                        receiver_thread_id, existing.item_id, spawn.item_id
                    ))
                }
            } else {
                tracing::debug!(
                    item_id = spawn.item_id.as_str(),
                    sender_thread_id = spawn.sender_thread_id.as_str(),
                    receiver_thread_id = receiver_thread_id.as_str(),
                    "Recorded authoritative Codex child spawn metadata"
                );
                state
                    .pending_subagent_spawns
                    .insert(receiver_thread_id.clone(), spawn);
                None
            }
        };
        if let Some(message) = conflict {
            tracing::error!(
                receiver_thread_id = receiver_thread_id.as_str(),
                "{message}"
            );
            self.emitter.backend_error(&message);
        }
    }

    async fn register_codex_subagent_activity_if_needed(&self, item: &Value) {
        let Some(activity) = parse_codex_subagent_activity(item) else {
            return;
        };
        if activity.kind != "started" {
            tracing::debug!(
                kind = activity.kind.as_str(),
                agent_thread_id = activity.agent_thread_id.as_str(),
                agent_path = activity.agent_path.as_str(),
                "Observed non-start Codex sub-agent activity"
            );
            return;
        }

        let thread_id = activity.agent_thread_id.clone();
        let (spawn, subagent_sink, rejection, idempotent) = {
            let mut state = self.state.lock().await;
            if let Some(message) = state.conflicting_subagent_threads.get(&thread_id) {
                (None, None, Some(message.clone()), false)
            } else if let Some(stream) = state.subagent_streams.get(&thread_id) {
                if stream.agent_path == activity.agent_path
                    && stream.sender_thread_id == state.thread_id
                    && !stream.spawn_item_id.is_empty()
                    && (stream.activity_item_id.is_none()
                        || activity.item_id.is_none()
                        || stream.activity_item_id == activity.item_id)
                {
                    (None, None, None, true)
                } else {
                    (
                        None,
                        None,
                        Some(format!(
                            "Codex ownership invariant failed: child thread '{}' was re-registered with contradictory activity metadata",
                            thread_id
                        )),
                        false,
                    )
                }
            } else if let Some(stream) = state.completed_subagent_streams.get(&thread_id) {
                if stream.agent_path == activity.agent_path
                    && stream.sender_thread_id == state.thread_id
                    && !stream.spawn_item_id.is_empty()
                    && (stream.activity_item_id.is_none()
                        || activity.item_id.is_none()
                        || stream.activity_item_id == activity.item_id)
                {
                    (None, None, None, true)
                } else {
                    (
                        None,
                        None,
                        Some(format!(
                            "Codex ownership invariant failed: completed child thread '{}' was re-registered with contradictory activity metadata",
                            thread_id
                        )),
                        false,
                    )
                }
            } else if !state.registering_subagent_threads.insert(thread_id.clone()) {
                (
                    None,
                    None,
                    Some(format!(
                        "Codex ownership invariant failed: child thread '{}' has concurrent duplicate registration activity",
                        thread_id
                    )),
                    false,
                )
            } else {
                let spawn = state
                    .pending_subagent_spawns
                    .remove(&thread_id)
                    .unwrap_or_else(|| CodexSubAgentSpawnInfo {
                        item_id: thread_id.clone(),
                        name: activity.agent_path.clone(),
                        description: activity.agent_path.clone(),
                        agent_type: "sub-agent".to_string(),
                        receiver_thread_id: thread_id.clone(),
                        sender_thread_id: state.thread_id.clone(),
                    });
                (Some(spawn), state.subagent_emitter.clone(), None, false)
            }
        };
        if idempotent {
            tracing::debug!(
                agent_thread_id = thread_id.as_str(),
                agent_path = activity.agent_path.as_str(),
                "Repeated Codex child activity is idempotent"
            );
            return;
        }
        if let Some(message) = rejection {
            tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
            self.emitter.backend_error(&message);
            return;
        }
        let (Some(spawn), Some(subagent_sink)) = (spawn, subagent_sink) else {
            let message = format!(
                "Codex ownership invariant failed: child thread '{}' started before its sub-agent emitter was installed",
                thread_id
            );
            let mut state = self.state.lock().await;
            state.registering_subagent_threads.remove(&thread_id);
            tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
            self.emitter.backend_error(&message);
            return;
        };

        let spawn_item_id = spawn.item_id.clone();
        let sender_thread_id = spawn.sender_thread_id.clone();
        let handle = match subagent_sink
            .on_subagent_spawned(
                spawn.item_id,
                spawn.name,
                spawn.description,
                spawn.agent_type,
                Some(SessionId(thread_id.clone())),
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                let message = format!(
                    "Codex child relay registration failed for thread '{}': {error}",
                    thread_id
                );
                {
                    let mut state = self.state.lock().await;
                    state.registering_subagent_threads.remove(&thread_id);
                }
                tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
                self.complete_all_codex_subagents().await;
                self.emitter.backend_error(&message);
                self.emitter.typing_status_changed(false);
                return;
            }
        };
        let (raw_event_tx, raw_event_rx) = mpsc::unbounded_channel();
        spawn_codex_subagent_event_bridge(raw_event_rx, handle.event_tx);
        let emitter = Arc::new(TurnEmitter::new_for_agent(
            raw_event_tx,
            AgentName(CODEX_AGENT_NAME),
        ));

        let duplicate_after_spawn = {
            let mut state = self.state.lock().await;
            state.registering_subagent_threads.remove(&thread_id);
            if state.subagent_streams.contains_key(&thread_id)
                || state.completed_subagent_streams.contains_key(&thread_id)
            {
                true
            } else {
                tracing::info!(
                    agent_thread_id = thread_id.as_str(),
                    agent_path = activity.agent_path.as_str(),
                    spawn_item_id = spawn_item_id.as_str(),
                    sender_thread_id = sender_thread_id.as_str(),
                    "Registered authoritative Codex child thread"
                );
                state.subagent_streams.insert(
                    thread_id.clone(),
                    CodexSubAgentStream {
                        emitter,
                        spawn_item_id,
                        activity_item_id: activity.item_id.clone(),
                        agent_path: activity.agent_path.clone(),
                        sender_thread_id,
                        active_turn_id: None,
                        current_message_id: None,
                        current_generated_identity: None,
                        current_reasoning_only: false,
                        current_text: String::new(),
                        current_reasoning: String::new(),
                        completed_agent_messages: HashMap::new(),
                        quarantined_turn_id: None,
                        quarantined: false,
                        generated_identity_epoch: codex_generated_identity_epoch(&thread_id),
                        next_generated_identity_ordinal: 1,
                        pending_message_metadata: None,
                        token_usage_by_turn: HashMap::new(),
                    },
                );
                false
            }
        };
        if duplicate_after_spawn {
            let message = format!(
                "Codex ownership invariant failed: child relay was created twice for thread '{}'",
                thread_id
            );
            tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
            self.emitter.backend_error(&message);
        }
    }

    #[cfg(test)]
    async fn complete_codex_subagent_if_needed(&self, thread_id: &str) {
        let mut state = self.state.lock().await;
        if let Some(stream) = state.subagent_streams.remove(thread_id) {
            state.completed_subagent_streams.insert(
                thread_id.to_string(),
                completed_codex_subagent_stream(stream),
            );
        }
    }

    async fn complete_all_codex_subagents(&self) {
        let mut state = self.state.lock().await;
        let streams = state.subagent_streams.drain().collect::<Vec<_>>();
        for (item_id, stream) in streams {
            if stream.current_message_id.is_some() {
                stream.emitter.discard_open_stream_with_identity_violation(
                    StreamIdentityViolation::MismatchedEndMessageId,
                );
            } else if stream.active_turn_id.is_some() {
                stream
                    .emitter
                    .operation_cancelled("Parent agent turn ended before the sub-agent completed");
            } else {
                tracing::debug!(
                    thread_id = item_id.as_str(),
                    "Retaining completed Codex child without cancellation during parent teardown"
                );
            }
            state
                .completed_subagent_streams
                .insert(item_id, completed_codex_subagent_stream(stream));
        }
    }

    fn handle_plan_update(&self, params: &Value) {
        let title = params
            .get("explanation")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Plan")
            .to_string();

        let tasks = params
            .get("plan")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(idx, step)| protocol::Task {
                id: idx as u64 + 1,
                description: step
                    .get("step")
                    .and_then(Value::as_str)
                    .unwrap_or("step")
                    .to_string(),
                status: map_plan_status(step.get("status").and_then(Value::as_str).unwrap_or("")),
            })
            .collect::<Vec<_>>();

        self.emitter
            .task_update(&protocol::TaskList { title, tasks });
    }

    async fn handle_turn_completed(&self, params: &Value) {
        let completed_turn_id = extract_turn_id(params);
        if self.state.lock().await.quarantined_turn_id.as_ref() == completed_turn_id.as_ref() {
            return;
        }
        let turn_status = params
            .get("turn")
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        let model_hint = {
            let state = self.state.lock().await;
            state.model.clone()
        };
        let turn_usage = extract_turn_token_usage(params, model_hint.as_deref());
        let model_usage = extract_model_request_token_usage(params, model_hint.as_deref());

        let (open_item_without_completion, partial_idless_reasoning, metadata_update, model_usage) = {
            let mut state = self.state.lock().await;
            if let Some((turn_id, token_usage)) = turn_usage {
                state.token_usage_by_turn.insert(turn_id, token_usage);
            }
            let model_usage =
                model_usage.and_then(|(turn_id, request, cumulative, context_window)| {
                    record_model_request_token_usage(
                        &mut state.model_token_usage_by_turn,
                        turn_id,
                        request,
                        cumulative,
                        context_window,
                    )
                });

            let completed_turn_id =
                extract_turn_id(params).or_else(|| state.active_turn_id.clone());
            state.active_turn_id = None;
            let mut open_item_without_completion = false;
            let mut partial_idless_reasoning = None;
            let mut metadata_update = None;
            if let Some(turn_id) = completed_turn_id {
                let has_open_stream = state
                    .active_stream
                    .as_ref()
                    .is_some_and(|stream| stream.turn_id == turn_id);
                let open_stream = has_open_stream
                    .then(|| state.active_stream.take())
                    .flatten();
                if let Some(stream) = open_stream {
                    state.pending_message_metadata = None;
                    state.quarantined_turn_id = Some(turn_id.clone());
                    let durable_idless_reasoning = stream.reasoning_only
                        && stream.generated_identity.as_ref().is_some_and(|identity| {
                            identity.origin == ServerGeneratedChatMessageIdOrigin::IdlessReasoning
                        })
                        && contains_non_whitespace(&stream.reasoning);
                    if durable_idless_reasoning {
                        let reasoning = stream.reasoning.clone();
                        state.completed_agent_messages.insert(
                            stream.message_id.clone(),
                            CompletedCodexAgentMessage {
                                completion_text: String::new(),
                                completion_reasoning: Some(reasoning.clone()),
                                generated_identity: stream.generated_identity.clone(),
                            },
                        );
                        partial_idless_reasoning = Some((stream.message_id, reasoning));
                    } else {
                        open_item_without_completion = true;
                    }
                }

                if turn_status != "interrupted"
                    && partial_idless_reasoning.is_none()
                    && !open_item_without_completion
                {
                    if state
                        .pending_message_metadata
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == turn_id)
                        && let Some(pending) = state.pending_message_metadata.take()
                    {
                        let token_usage = state.token_usage_by_turn.remove(&turn_id);
                        let context_breakdown = estimate_context_breakdown(
                            token_usage.as_ref(),
                            &pending.turn_context,
                            Some(&pending.model),
                        );
                        if token_usage.is_none() {
                            state
                                .completed_message_metadata_by_turn
                                .insert(turn_id.clone(), pending.clone());
                        }
                        let model_token_usage =
                            state.model_token_usage_by_turn.get(&turn_id).cloned();
                        metadata_update =
                            Some((pending, token_usage, model_token_usage, context_breakdown));
                    }
                } else if turn_status == "interrupted"
                    && state
                        .pending_message_metadata
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == turn_id)
                {
                    state.pending_message_metadata = None;
                    state.completed_message_metadata_by_turn.remove(&turn_id);
                }
                state.turn_context_by_turn.remove(&turn_id);
                state.token_usage_by_turn.remove(&turn_id);
                state.model_token_usage_by_turn.remove(&turn_id);
            }
            state.pending_request = None;
            state.file_change_call_ids.clear();
            state.pending_tool_call_ids.clear();
            state.close_active_stream_when_tools_idle = false;
            state.pending_user_input_bytes = 0;
            (
                open_item_without_completion,
                partial_idless_reasoning,
                metadata_update,
                model_usage,
            )
        };

        if let Some(usage) = model_usage {
            self.emitter.model_request_token_usage(&usage);
        }
        if let Some((message_id, reasoning)) = partial_idless_reasoning {
            if matches!(turn_status.as_str(), "interrupted" | "failed") {
                self.complete_all_codex_subagents().await;
            }
            self.emitter.stream_end_with_id(
                message_id,
                StreamEndPayload {
                    content: String::new(),
                    agent: Some(AgentName(CODEX_AGENT_NAME)),
                    model: Some(model_hint.unwrap_or_else(|| "codex".to_string())),
                    request_usage: None,
                    turn_usage: None,
                    cumulative_usage: None,
                    token_usage_unavailable_reason: None,
                    reasoning: Some(reasoning),
                    tool_calls: Vec::new(),
                    context_breakdown: None,
                },
            );
            self.emitter
                .operation_cancelled("Codex turn ended before reasoning item completion");
            return;
        }
        if open_item_without_completion {
            if matches!(turn_status.as_str(), "interrupted" | "failed") {
                self.complete_all_codex_subagents().await;
            }
            self.emitter.discard_open_stream_with_identity_violation(
                StreamIdentityViolation::MismatchedEndMessageId,
            );
            return;
        }
        if let Some((pending, token_usage, model_token_usage, context_breakdown)) = metadata_update
        {
            emit_codex_message_metadata_update(
                &self.emitter,
                pending,
                token_usage,
                model_token_usage.as_ref(),
                context_breakdown,
            );
        }

        if turn_status == "interrupted" {
            self.complete_all_codex_subagents().await;
            // emitter.operation_cancelled runs the full cancel tail:
            // flush pending tools → OperationCancelled → TypingStatusChanged(false).
            self.emitter.operation_cancelled("Operation cancelled");
            return;
        }

        self.emitter.typing_status_changed(false);

        if turn_status == "failed" {
            let message = params
                .get("turn")
                .and_then(|v| v.get("error"))
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .to_string();
            self.complete_all_codex_subagents().await;
            self.emitter.backend_error(&message);
        }
    }

    async fn emit_tool_execution_completed(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        success: bool,
        tool_result: Value,
        error: Option<String>,
    ) {
        let (tool_result, normalization_failure) = normalize_codex_tool_result(
            &self.emitter,
            tool_call_id,
            tool_name,
            tool_result,
            success,
        );
        let completed = ToolCompletedPayload {
            tool_call_id,
            tool_name,
            tool_result,
            success,
            error: error.as_deref(),
        };
        if let Some(normalization_failure) = normalization_failure {
            self.emitter
                .tool_completed_with_normalization_failure(completed, normalization_failure);
        } else {
            self.emitter.tool_completed(completed);
        }
        self.mark_tool_completed(tool_call_id).await;
    }

    fn emit_agent_control_await_progress_if_needed(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &Value,
    ) {
        emit_agent_control_await_progress_to(&self.emitter, tool_call_id, tool_name, arguments);
    }

    fn emit_agent_control_spawn_progress_if_needed(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        tool_result: &Value,
    ) {
        emit_agent_control_spawn_progress_to(&self.emitter, tool_call_id, tool_name, tool_result);
    }

    fn emit_modify_file_request(
        &self,
        tool_call_id: &str,
        file_path: &str,
        before: &str,
        after: &str,
    ) {
        self.emitter.tool_request(
            tool_call_id,
            "modify_file",
            json!({
                "kind": "ModifyFile",
                "file_path": file_path,
                "before": before,
                "after": after
            }),
        );
    }

    fn emit_user_message_added(&self, content: &str, images: Option<&[ImageAttachment]>) {
        let image_payload = images
            .unwrap_or(&[])
            .iter()
            .map(|image| {
                json!({
                    "media_type": image.media_type,
                    "data": image.data
                })
            })
            .collect::<Vec<_>>();
        self.emitter.user_message(content, image_payload);
    }
}

fn emit_modify_file_request_to(
    emitter: &TurnEmitter,
    tool_call_id: &str,
    file_path: &str,
    before: &str,
    after: &str,
) {
    emitter.tool_request(
        tool_call_id,
        "modify_file",
        json!({
            "kind": "ModifyFile",
            "file_path": file_path,
            "before": before,
            "after": after
        }),
    );
}

fn extract_notification_thread_id(params: &Value) -> Option<String> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| params.get("thread_id").and_then(Value::as_str))
        .or_else(|| {
            params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("threadId"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("thread_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| params.get("senderThreadId").and_then(Value::as_str))
        .map(|id| id.to_string())
}

fn is_thread_scoped_codex_notification(method: &str) -> bool {
    matches!(
        method,
        "turn/started" | "turn/completed" | "turn/plan/updated" | "thread/tokenUsage/updated"
    ) || method.starts_with("item/")
        || is_reasoning_notification_method(method)
}

fn classify_codex_notification_owner(state: &CodexState, params: &Value) -> CodexNotificationOwner {
    let Some(thread_id) = extract_notification_thread_id(params) else {
        return CodexNotificationOwner::Unknown { thread_id: None };
    };
    if thread_id == state.thread_id {
        return CodexNotificationOwner::Parent { thread_id };
    }
    if state.subagent_streams.contains_key(&thread_id) {
        return CodexNotificationOwner::LiveChild { thread_id };
    }
    if state.completed_subagent_streams.contains_key(&thread_id) {
        return CodexNotificationOwner::CompletedChild { thread_id };
    }
    CodexNotificationOwner::Unknown {
        thread_id: Some(thread_id),
    }
}

fn codex_plan_update_task_list_from_params(params: &Value) -> Option<protocol::TaskList> {
    let title = params
        .get("explanation")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Plan")
        .to_string();
    let plan = params.get("plan").and_then(Value::as_array)?;
    let tasks = plan
        .iter()
        .enumerate()
        .map(|(idx, step)| protocol::Task {
            id: idx as u64 + 1,
            description: step
                .get("step")
                .and_then(Value::as_str)
                .unwrap_or("step")
                .to_string(),
            status: map_plan_status(step.get("status").and_then(Value::as_str).unwrap_or("")),
        })
        .collect::<Vec<_>>();

    Some(protocol::TaskList { title, tasks })
}

fn codex_thread_to_session_metadata(thread: &Value) -> Option<Value> {
    let session_id = thread.get("id").and_then(Value::as_str)?;
    let preview = thread
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if preview.trim().is_empty() {
                "Codex Session".to_string()
            } else {
                preview.clone()
            }
        });

    let created_at = thread
        .get("createdAt")
        .and_then(Value::as_u64)
        .unwrap_or_else(unix_now_ms);
    let last_modified = thread
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or(created_at);
    let workspace_root = thread
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let message_count: Option<u64> = thread.get("turns").and_then(Value::as_array).map(|turns| {
        turns
            .iter()
            .filter_map(|turn| turn.get("items").and_then(Value::as_array))
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("userMessage" | "agentMessage")
                        )
                    })
                    .count() as u64
            })
            .sum::<u64>()
    });

    Some(json!({
        "id": session_id,
        "session_id": session_id,
        "title": title,
        "created_at": created_at,
        "last_modified": last_modified,
        "last_message_preview": preview,
        "workspace_root": workspace_root,
        "message_count": message_count,
        "backend_kind": "codex"
    }))
}

fn codex_item_success(item: &Value) -> bool {
    if let Some(success) = item.get("success").and_then(Value::as_bool) {
        return success;
    }

    let normalized_status = item
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| item.get("agentStatus").and_then(Value::as_str))
        .map(|status| status.trim().to_ascii_lowercase());

    match normalized_status.as_deref() {
        Some("completed" | "complete" | "succeeded" | "success" | "ok" | "done") => true,
        Some("failed" | "error" | "cancelled" | "canceled" | "interrupted" | "denied") => false,
        _ => true,
    }
}

fn parse_codex_subagent_collab(item: &Value) -> Option<CodexSubAgentSpawnInfo> {
    if item.get("type").and_then(Value::as_str) != Some("collabAgentToolCall")
        || item.get("agentsStates").is_some()
    {
        return None;
    }
    let has_spawn_shape = item.get("prompt").and_then(Value::as_str).is_some()
        && (item
            .get("receiverAgentType")
            .and_then(Value::as_str)
            .is_some()
            || item
                .get("receiverAgentName")
                .and_then(Value::as_str)
                .is_some());
    if !has_spawn_shape {
        return None;
    }
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| item.get("callId").and_then(Value::as_str))?
        .to_string();
    let receiver_thread_id = item
        .get("receiverThreadId")
        .and_then(Value::as_str)?
        .to_string();
    let sender_thread_id = item
        .get("senderThreadId")
        .and_then(Value::as_str)?
        .to_string();
    if receiver_thread_id.trim().is_empty() || sender_thread_id.trim().is_empty() {
        return None;
    }
    let agent_type = item
        .get("receiverAgentType")
        .and_then(Value::as_str)
        .unwrap_or("sub-agent")
        .to_string();
    let description = item
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = item
        .get("receiverAgentName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Sub-agent")
        .to_string();
    Some(CodexSubAgentSpawnInfo {
        item_id,
        name,
        description,
        agent_type,
        receiver_thread_id,
        sender_thread_id,
    })
}

fn parse_codex_subagent_activity(item: &Value) -> Option<CodexSubAgentActivity> {
    if !matches!(
        item.get("type").and_then(Value::as_str),
        Some("subAgentActivity" | "sub_agent_activity")
    ) {
        return None;
    }
    let agent_thread_id = item
        .get("agentThreadId")
        .or_else(|| item.get("agent_thread_id"))
        .and_then(Value::as_str)?
        .to_string();
    if agent_thread_id.trim().is_empty() {
        return None;
    }
    Some(CodexSubAgentActivity {
        item_id: item.get("id").and_then(Value::as_str).map(str::to_string),
        agent_thread_id,
        agent_path: item
            .get("agentPath")
            .or_else(|| item.get("agent_path"))
            .and_then(Value::as_str)
            .unwrap_or("Sub-agent")
            .to_string(),
        kind: item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase(),
    })
}

fn extract_codex_item_text(item: &Value) -> String {
    if let Some(text) = item.get("text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return text.to_string();
    }

    let mut chunks: Vec<String> = Vec::new();
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        for part in content {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
                continue;
            }
            if let Some(text) = part.get("inputText").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
                continue;
            }
            if let Some(text) = part.get("value").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                chunks.push(text.to_string());
            }
        }
    }

    if chunks.is_empty() {
        String::new()
    } else {
        chunks.join("\n")
    }
}

fn extract_codex_reasoning_delta_text(params: &Value) -> Option<String> {
    for key in [
        "delta",
        "text",
        "summaryText",
        "summary_text",
        "reasoningSummary",
        "reasoning_summary",
        "reasoningSummaryText",
        "reasoning_summary_text",
        "summary",
        "reasoning",
        "thinking",
        "content",
    ] {
        if let Some(text) = extract_codex_reasoning_delta_fragment(params.get(key)) {
            return Some(text);
        }
    }

    for nested in ["msg", "event", "payload"] {
        if let Some(value) = params.get(nested)
            && let Some(text) = extract_codex_reasoning_delta_text(value)
        {
            return Some(text);
        }
    }

    params.get("item").and_then(extract_codex_item_reasoning)
}

fn extract_codex_reasoning_delta_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Array(values) => {
            let mut out = String::new();
            for part in values {
                if let Some(text) = extract_codex_reasoning_delta_fragment(Some(part)) {
                    out.push_str(&text);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        Value::Object(map) => {
            for key in [
                "delta",
                "summary_delta",
                "summaryDelta",
                "reasoning_delta",
                "reasoningDelta",
                "text",
                "value",
                "token",
                "output_text",
                "outputText",
                "summaryText",
                "summary_text",
                "summary",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "reasoning",
                "thinking",
                "content",
                "parts",
            ] {
                if let Some(text) = extract_codex_reasoning_delta_fragment(map.get(key)) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_reasoning_delta_from_legacy_codex_event(method: &str, params: &Value) -> Option<String> {
    let event_type = extract_codex_event_type(method, params)?;
    if event_type == "agent_reasoning_section_break" {
        return Some("\n\n".to_string());
    }
    if !is_codex_event_reasoning_type(&event_type) {
        return None;
    }
    extract_codex_reasoning_delta_text(params)
}

fn metadata_target_for_visible_message(
    turn_id: String,
    message_id: ChatMessageId,
    content: &str,
    reasoning: Option<&str>,
    model: String,
    turn_context: TurnContextEstimate,
) -> Option<PendingCodexMessageMetadata> {
    if message_id.0.trim().is_empty() {
        return None;
    }
    if !contains_non_whitespace(content) && !reasoning.is_some_and(contains_non_whitespace) {
        return None;
    }
    Some(PendingCodexMessageMetadata {
        turn_id,
        message_id,
        model,
        turn_context,
    })
}

fn emit_codex_message_metadata_update(
    emitter: &TurnEmitter,
    pending: PendingCodexMessageMetadata,
    token_usage: Option<Value>,
    model_token_usage: Option<&CodexTurnTokenUsage>,
    context_breakdown: Value,
) {
    let (request_usage, turn_usage, cumulative_usage) =
        codex_message_usage_values(token_usage, model_token_usage);
    emitter.message_metadata_updated(MessageMetadataUpdatePayload {
        message_id: pending.message_id.0,
        model_info: Some(json!({ "model": pending.model })),
        request_usage,
        turn_usage,
        cumulative_usage,
        context_breakdown: Some(context_breakdown),
    });
}

fn codex_message_usage_values(
    token_usage: Option<Value>,
    model_token_usage: Option<&CodexTurnTokenUsage>,
) -> (Option<Value>, Option<Value>, Option<Value>) {
    match model_token_usage {
        Some(usage) => (
            usage.latest_request.as_ref().map(codex_token_usage_value),
            Some(codex_token_usage_value(&usage.turn)),
            usage.cumulative.as_ref().map(codex_token_usage_value),
        ),
        None => (token_usage.clone(), token_usage, None),
    }
}

fn codex_token_usage_value(usage: &TokenUsage) -> Value {
    serde_json::to_value(usage).expect("Codex token usage must serialize")
}

fn extract_codex_event_type(method: &str, params: &Value) -> Option<String> {
    params
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("msg")
                .and_then(|msg| msg.get("type"))
                .and_then(Value::as_str)
        })
        .or_else(|| method.strip_prefix("codex/event/"))
        .map(|raw| raw.trim().to_ascii_lowercase())
}

fn is_codex_event_reasoning_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "agent_reasoning"
            | "agent_reasoning_delta"
            | "agent_reasoning_raw_content"
            | "agent_reasoning_raw_content_delta"
    )
}

fn extract_codex_item_reasoning(item: &Value) -> Option<String> {
    extract_codex_reasoning_fragment(item.get("reasoning"))
        .or_else(|| extract_codex_reasoning_fragment(item.get("summaryText")))
        .or_else(|| extract_codex_reasoning_fragment(item.get("summary")))
        .or_else(|| extract_codex_reasoning_fragment(item.get("reasoningSummary")))
        .or_else(|| {
            let mut chunks = Vec::new();
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    let part_type = part
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if !part_type.contains("reason")
                        && !part_type.contains("think")
                        && !part_type.contains("summary")
                    {
                        continue;
                    }
                    if let Some(text) = extract_codex_reasoning_fragment(Some(part)) {
                        chunks.push(text);
                    }
                }
            }
            join_nonempty_chunks(chunks)
        })
}

fn extract_codex_reasoning_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            if !contains_non_whitespace(text) {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Array(values) => {
            let mut chunks = Vec::new();
            for part in values {
                if let Some(text) = extract_codex_reasoning_fragment(Some(part)) {
                    chunks.push(text);
                }
            }
            join_nonempty_chunks(chunks)
        }
        Value::Object(map) => {
            for key in [
                "text",
                "summaryText",
                "summary_text",
                "summary",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "reasoning",
                "thinking",
                "output_text",
                "outputText",
                "delta",
                "summary_delta",
                "summaryDelta",
                "reasoning_delta",
                "reasoningDelta",
                "token",
                "value",
                "content",
                "parts",
            ] {
                if let Some(text) = extract_codex_reasoning_fragment(map.get(key)) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn is_reasoning_notification_method(method: &str) -> bool {
    let normalized = method.to_ascii_lowercase();
    normalized.starts_with("item/reasoning/")
        || normalized.starts_with("item/reasoning")
        || normalized.starts_with("item/thinking/")
        || normalized.starts_with("item/thinking")
}

fn is_codex_response_side_notification(method: &str) -> bool {
    method.starts_with("item/")
        || method.starts_with("turn/")
        || method == "thread/tokenUsage/updated"
        || method == "model/rerouted"
        || method == "error"
}

fn is_terminal_codex_error_notification(state: &CodexState, params: &Value) -> bool {
    if params.get("fatal").and_then(Value::as_bool) == Some(true)
        || params.get("terminal").and_then(Value::as_bool) == Some(true)
        || params.get("recoverable").and_then(Value::as_bool) == Some(false)
    {
        return true;
    }

    state.active_turn_id.is_none()
        && state.active_stream.is_none()
        && state.pending_request.is_none()
}

fn join_nonempty_chunks(chunks: Vec<String>) -> Option<String> {
    let normalized = chunks
        .into_iter()
        .filter(|chunk| contains_non_whitespace(chunk))
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.join("\n"))
    }
}

fn contains_non_whitespace(text: &str) -> bool {
    text.chars().any(|ch| !ch.is_whitespace())
}

fn map_plan_status(status: &str) -> protocol::TaskStatus {
    match status {
        "completed" => protocol::TaskStatus::Completed,
        "inProgress" => protocol::TaskStatus::InProgress,
        _ => protocol::TaskStatus::Pending,
    }
}

#[derive(Debug, Clone)]
struct CodexFileChange {
    path: String,
    before: String,
    after: String,
    lines_added: u64,
    lines_removed: u64,
}

#[derive(Debug, Clone)]
struct CodexFileChangeCompletion {
    call_id: String,
    request: Option<CodexFileChange>,
    lines_added: u64,
    lines_removed: u64,
}

fn codex_file_change_call_id(item_id: &str, index: usize, total: usize) -> String {
    if total <= 1 {
        item_id.to_string()
    } else {
        format!("{item_id}#{}", index + 1)
    }
}

fn codex_file_change_completion_plan(
    item_id: &str,
    known_call_ids: &[String],
    file_changes: &[CodexFileChange],
) -> Vec<CodexFileChangeCompletion> {
    if file_changes.is_empty() {
        return known_call_ids
            .iter()
            .map(|call_id| CodexFileChangeCompletion {
                call_id: call_id.clone(),
                request: None,
                lines_added: 0,
                lines_removed: 0,
            })
            .collect();
    }

    let total = file_changes.len();
    let mut completions = Vec::with_capacity(known_call_ids.len().max(total));
    for (idx, change) in file_changes.iter().enumerate() {
        let known_call_id = known_call_ids.get(idx).cloned();
        completions.push(CodexFileChangeCompletion {
            call_id: known_call_id
                .unwrap_or_else(|| codex_file_change_call_id(item_id, idx, total)),
            request: (known_call_ids.get(idx).is_none()).then(|| change.clone()),
            lines_added: change.lines_added,
            lines_removed: change.lines_removed,
        });
    }

    completions.extend(known_call_ids.iter().skip(total).map(|call_id| {
        CodexFileChangeCompletion {
            call_id: call_id.clone(),
            request: None,
            lines_added: 0,
            lines_removed: 0,
        }
    }));

    completions
}

fn parse_codex_file_changes(item: &Value) -> Vec<CodexFileChange> {
    let Some(changes) = item.get("changes").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut parsed = Vec::new();
    for change in changes {
        let path = change
            .get("path")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                change
                    .get("kind")
                    .and_then(|k| k.get("move_path"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string();
        if path.trim().is_empty() {
            continue;
        }

        let diff = change
            .get("diff")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (before, after, lines_added, lines_removed) = parse_unified_diff_preview(diff);

        parsed.push(CodexFileChange {
            path,
            before,
            after,
            lines_added,
            lines_removed,
        });
    }

    parsed
}

fn parse_unified_diff_preview(diff: &str) -> (String, String, u64, u64) {
    let mut before_lines: Vec<String> = Vec::new();
    let mut after_lines: Vec<String> = Vec::new();
    let mut lines_added = 0u64;
    let mut lines_removed = 0u64;

    for line in diff.lines() {
        if line.starts_with("@@") || line.starts_with('\\') || line.is_empty() {
            continue;
        }

        if let Some(text) = line.strip_prefix('+') {
            // Skip patch file headers (`+++`) while counting actual additions.
            if !line.starts_with("+++ ") {
                after_lines.push(text.to_string());
                lines_added += 1;
            }
            continue;
        }

        if let Some(text) = line.strip_prefix('-') {
            // Skip patch file headers (`---`) while counting actual removals.
            if !line.starts_with("--- ") {
                before_lines.push(text.to_string());
                lines_removed += 1;
            }
            continue;
        }

        if let Some(text) = line.strip_prefix(' ') {
            before_lines.push(text.to_string());
            after_lines.push(text.to_string());
            continue;
        }

        before_lines.push(line.to_string());
        after_lines.push(line.to_string());
    }

    (
        before_lines.join("\n"),
        after_lines.join("\n"),
        lines_added,
        lines_removed,
    )
}

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

const CODEX_TOKEN_USAGE_COUNTER_KEYS: &[&str] = &[
    "inputTokens",
    "input_tokens",
    "prompt_tokens",
    "outputTokens",
    "output_tokens",
    "completion_tokens",
    "totalTokens",
    "total_tokens",
    "cachedInputTokens",
    "cached_prompt_tokens",
    "cacheCreationInputTokens",
    "cache_creation_input_tokens",
    "reasoningOutputTokens",
    "reasoning_tokens",
];

fn has_numeric_token_usage_counter(value: &Value) -> bool {
    CODEX_TOKEN_USAGE_COUNTER_KEYS
        .iter()
        .any(|key| value.get(*key).and_then(Value::as_u64).is_some())
}

fn extract_turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| params.get("turn_id").and_then(Value::as_str))
        .or_else(|| params.get("id").and_then(Value::as_str))
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("turnId"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("turn_id"))
                .and_then(Value::as_str)
        })
        .map(|id| id.to_string())
}

fn extract_turn_token_usage_value(params: &Value) -> Option<&Value> {
    params
        .get("tokenUsage")
        .or_else(|| params.get("token_usage"))
        .or_else(|| params.get("usage"))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("tokenUsage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("token_usage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("usage")))
}

fn extract_turn_token_usage(params: &Value, model_hint: Option<&str>) -> Option<(String, Value)> {
    let turn_id = extract_turn_id(params)?;
    let usage = extract_turn_token_usage_value(params)?;
    let normalized = normalize_token_usage_with_envelope(usage, Some(params), model_hint)?;
    Some((turn_id, normalized))
}

fn extract_model_request_token_usage(
    params: &Value,
    model_hint: Option<&str>,
) -> Option<(String, TokenUsage, TokenUsage, Option<u64>)> {
    let turn_id = extract_turn_id(params)?;
    let raw = extract_turn_token_usage_value(params)?;
    let request_value = normalize_token_usage_with_envelope(raw, Some(params), model_hint)?;
    let cumulative_raw = raw
        .get("total")
        .filter(|value| value.is_object())
        .unwrap_or_else(|| {
            raw.get("last")
                .filter(|value| value.is_object())
                .unwrap_or(raw)
        });
    let cumulative_value =
        normalize_token_usage_with_envelope(cumulative_raw, Some(params), model_hint)?;
    let model_context_window = request_value.get("context_window").and_then(Value::as_u64);
    let request = serde_json::from_value(request_value).ok()?;
    let cumulative = serde_json::from_value(cumulative_value).ok()?;
    Some((turn_id, request, cumulative, model_context_window))
}

fn record_model_request_token_usage(
    usage_by_turn: &mut HashMap<String, CodexTurnTokenUsage>,
    turn_id: String,
    request: TokenUsage,
    cumulative: TokenUsage,
    model_context_window: Option<u64>,
) -> Option<ModelRequestTokenUsage> {
    let state = usage_by_turn.entry(turn_id.clone()).or_default();
    if state.cumulative.as_ref() == Some(&cumulative) {
        return None;
    }

    let sequence = state.request_count;
    state.request_count = state.request_count.saturating_add(1);
    add_token_usage(&mut state.turn, &request);
    state.latest_request = Some(request.clone());
    state.cumulative = Some(cumulative.clone());
    state.model_context_window = model_context_window.or(state.model_context_window);

    Some(ModelRequestTokenUsage {
        request_id: ModelRequestId {
            turn_id: ModelTurnId(turn_id),
            sequence,
        },
        request,
        turn: state.turn.clone(),
        cumulative,
        model_context_window: state.model_context_window,
    })
}

fn add_token_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cached_prompt_tokens =
        add_optional_token_usage(total.cached_prompt_tokens, usage.cached_prompt_tokens);
    total.cache_creation_input_tokens = add_optional_token_usage(
        total.cache_creation_input_tokens,
        usage.cache_creation_input_tokens,
    );
    total.reasoning_tokens =
        add_optional_token_usage(total.reasoning_tokens, usage.reasoning_tokens);
}

fn add_optional_token_usage(total: Option<u64>, usage: Option<u64>) -> Option<u64> {
    match (total, usage) {
        (None, None) => None,
        (total, usage) => Some(total.unwrap_or(0).saturating_add(usage.unwrap_or(0))),
    }
}

fn normalize_token_usage_with_envelope(
    raw: &Value,
    envelope: Option<&Value>,
    model_hint: Option<&str>,
) -> Option<Value> {
    let source = raw
        .get("last")
        .filter(|value| value.is_object())
        .unwrap_or(raw);
    if !has_numeric_token_usage_counter(source) {
        return None;
    }

    // OpenAI convention: `inputTokens` is the TOTAL including cached tokens,
    // and `cachedInputTokens` is a subset.  Our internal contract (matching
    // Anthropic) expects `input_tokens` to be the non-cached portion only,
    // with cache fields as separate additive values.
    let cached_prompt_tokens =
        usage_u64(source, &["cachedInputTokens", "cached_prompt_tokens"]).unwrap_or(0);
    let cache_creation_input_tokens = usage_u64(
        source,
        &["cacheCreationInputTokens", "cache_creation_input_tokens"],
    )
    .unwrap_or(0);
    let raw_input_tokens = usage_u64(source, &["inputTokens"]).unwrap_or(0);
    let input_tokens = if source.get("inputTokens").is_some() {
        raw_input_tokens
            .saturating_sub(cached_prompt_tokens)
            .saturating_sub(cache_creation_input_tokens)
    } else {
        usage_u64(source, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0)
    };
    let prompt_tokens_total = if raw_input_tokens > 0 {
        raw_input_tokens
    } else {
        input_tokens
            .saturating_add(cached_prompt_tokens)
            .saturating_add(cache_creation_input_tokens)
    };

    // OpenAI convention: `outputTokens` includes reasoning.  Our contract
    // treats `reasoning_tokens` as an informational subset of `output_tokens`,
    // so `output_tokens` is stored as-is (already includes reasoning).
    let output_tokens = usage_u64(
        source,
        &["outputTokens", "output_tokens", "completion_tokens"],
    )
    .unwrap_or(0);
    let reasoning_tokens =
        usage_u64(source, &["reasoningOutputTokens", "reasoning_tokens"]).unwrap_or(0);

    // total_tokens = input_tokens + output_tokens (no double-counting).
    let total_tokens =
        usage_u64(source, &["totalTokens", "total_tokens"]).unwrap_or(input_tokens + output_tokens);
    let context_window = context_window_from_token_usage(raw, source, envelope)
        .filter(|window| *window > 0)
        .unwrap_or_else(|| {
            let model_estimate = codex_estimated_context_window_for_model(model_hint);
            std::cmp::max(model_estimate, prompt_tokens_total.max(1))
        });

    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": cached_prompt_tokens,
        "cache_creation_input_tokens": cache_creation_input_tokens,
        "reasoning_tokens": reasoning_tokens,
        "context_window": context_window
    }))
}

fn context_window_from_token_usage(
    raw: &Value,
    last: &Value,
    envelope: Option<&Value>,
) -> Option<u64> {
    const WINDOW_KEYS: &[&str] = &[
        "modelContextWindow",
        "model_context_window",
        "contextWindow",
        "context_window",
        "maxInputTokens",
        "max_input_tokens",
        "maxTokens",
        "max_tokens",
        "maxPromptTokens",
        "max_prompt_tokens",
    ];

    find_context_window_in_value(raw, WINDOW_KEYS, 2)
        .or_else(|| find_context_window_in_value(last, WINDOW_KEYS, 2))
        .or_else(|| envelope.and_then(|value| find_context_window_in_value(value, WINDOW_KEYS, 4)))
}

fn find_context_window_in_value(value: &Value, keys: &[&str], depth: usize) -> Option<u64> {
    if depth == 0 {
        return None;
    }

    if let Some(obj) = value.as_object() {
        for key in keys {
            if let Some(window) = obj.get(*key).and_then(Value::as_u64).filter(|w| *w > 0) {
                return Some(window);
            }
        }
        for nested in obj.values() {
            if let Some(window) = find_context_window_in_value(nested, keys, depth - 1) {
                return Some(window);
            }
        }
        return None;
    }

    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(window) = find_context_window_in_value(item, keys, depth - 1) {
                return Some(window);
            }
        }
    }

    None
}

fn codex_estimated_context_window_for_model(model_hint: Option<&str>) -> u64 {
    let Some(model) = model_hint else {
        return CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT;
    };
    let normalized = model.trim().to_ascii_lowercase();
    // `codex-mini-latest` is the one GPT-5-era model with a 200k window, so it
    // must be checked before the broader gpt-5 family match below.
    if normalized.contains("codex-mini") {
        return CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT;
    }
    // Match the whole gpt-5 family by substring so this stays correct across
    // version bumps, `-codex`/`-mini` suffixes, and provider prefixes (the CLI
    // now reports ids like `openai.gpt-5.5`).
    if normalized.contains("gpt-5") {
        return CODEX_ESTIMATED_CONTEXT_WINDOW_GPT5_FAMILY;
    }
    CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT
}

fn estimate_context_breakdown(
    token_usage: Option<&Value>,
    turn_context: &TurnContextEstimate,
    model_hint: Option<&str>,
) -> Value {
    let base_input_tokens = token_usage
        .and_then(|usage| usage.get("input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cached_prompt_tokens = token_usage
        .and_then(|usage| usage.get("cached_prompt_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_creation_input_tokens = token_usage
        .and_then(|usage| {
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    // Context utilization should reflect the full prompt footprint, including cache hits/writes.
    let mut input_tokens = base_input_tokens
        .saturating_add(cached_prompt_tokens)
        .saturating_add(cache_creation_input_tokens);
    let context_window = token_usage
        .and_then(|usage| usage.get("context_window").and_then(Value::as_u64))
        .filter(|window| *window > 0)
        .unwrap_or_else(|| {
            let model_estimate = codex_estimated_context_window_for_model(model_hint);
            std::cmp::max(model_estimate, input_tokens.max(1))
        });

    let reasoning_from_tokens = token_usage
        .and_then(|usage| usage.get("reasoning_tokens").and_then(Value::as_u64))
        .unwrap_or(0)
        .saturating_mul(CODEX_ESTIMATED_BYTES_PER_TOKEN);

    let reasoning_est = std::cmp::max(turn_context.reasoning_bytes, reasoning_from_tokens);
    let tools_est = turn_context.tool_io_bytes;
    let history_est = turn_context.conversation_history_bytes;
    let observed_bytes = reasoning_est
        .saturating_add(tools_est)
        .saturating_add(history_est);

    let mut total_prompt_bytes = input_tokens.saturating_mul(CODEX_ESTIMATED_BYTES_PER_TOKEN);
    if total_prompt_bytes == 0 {
        let system_floor = if observed_bytes > 0 {
            CODEX_MIN_SYSTEM_PROMPT_BYTES
        } else {
            0
        };
        total_prompt_bytes = observed_bytes.saturating_add(system_floor);
        if total_prompt_bytes > 0 {
            input_tokens = total_prompt_bytes.div_ceil(CODEX_ESTIMATED_BYTES_PER_TOKEN);
        }
    }

    let mut system_prompt_bytes = if total_prompt_bytes == 0 {
        0
    } else {
        let target = total_prompt_bytes / 10;
        std::cmp::max(CODEX_MIN_SYSTEM_PROMPT_BYTES, target)
    };
    system_prompt_bytes = std::cmp::min(system_prompt_bytes, total_prompt_bytes);

    let mut remaining = total_prompt_bytes.saturating_sub(system_prompt_bytes);
    let reasoning_bytes = std::cmp::min(reasoning_est, remaining);
    remaining = remaining.saturating_sub(reasoning_bytes);

    let tool_io_bytes = std::cmp::min(tools_est, remaining);
    remaining = remaining.saturating_sub(tool_io_bytes);

    let conversation_history_bytes = std::cmp::min(history_est, remaining);
    remaining = remaining.saturating_sub(conversation_history_bytes);

    let context_injection_bytes = remaining;

    json!({
        "system_prompt_bytes": system_prompt_bytes,
        "tool_io_bytes": tool_io_bytes,
        "conversation_history_bytes": conversation_history_bytes,
        "reasoning_bytes": reasoning_bytes,
        "context_injection_bytes": context_injection_bytes,
        "input_tokens": input_tokens,
        "context_window": context_window
    })
}

fn estimate_command_execution_tool_bytes(item: &Value) -> u64 {
    value_str_len(item, "command")
        .saturating_add(value_str_len(item, "cwd"))
        .saturating_add(value_str_len(item, "aggregatedOutput"))
}

fn estimate_file_change_tool_bytes(item: &Value) -> u64 {
    let mut total = 0u64;
    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        for change in changes {
            total = total
                .saturating_add(value_str_len(change, "path"))
                .saturating_add(value_str_len(change, "diff"));
        }
    }
    if total > 0 {
        return total;
    }
    estimate_generic_tool_bytes(item)
}

fn estimate_generic_tool_bytes(item: &Value) -> u64 {
    let bytes = serde_json::to_vec(item)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    std::cmp::min(bytes, 128_000)
}

fn value_str_len(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn codex_mcp_elicitation_result(params: &Value) -> Value {
    let server_name = params
        .get("serverName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let approval_kind = params
        .get("_meta")
        .and_then(|meta| meta.get("codex_approval_kind"))
        .and_then(Value::as_str);
    if approval_kind == Some("mcp_tool_call")
        && matches!(
            server_name,
            "tyde-debug"
                | "tyde-agent-control"
                | AGENT_CONTROL_AWAIT_MCP_SERVER_NAME
                | REVIEW_FEEDBACK_MCP_SERVER_NAME
        )
    {
        return json!({
            "action": "accept",
            "content": {}
        });
    }

    json!({
        "action": "cancel"
    })
}

#[cfg(test)]
fn codex_server_request_result(method: &str, params: &Value) -> Value {
    match method {
        "mcpServer/elicitation/request" => codex_mcp_elicitation_result(params),
        _ => json!({"ignored": true, "reason": "unsupported_server_request"}),
    }
}

fn parse_approval_decision(message: &str) -> &'static str {
    let normalized = message.trim().to_ascii_lowercase();
    if normalized.starts_with("cancel") {
        return "cancel";
    }
    if normalized.contains("decline")
        || normalized.contains("deny")
        || normalized == "no"
        || normalized == "n"
    {
        return "decline";
    }
    if normalized.contains("always") || normalized.contains("for session") {
        return "acceptForSession";
    }
    "accept"
}

fn is_codex_tool_server_request(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "execCommandApproval"
            | "applyPatchApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
            | "item/tool/call"
    )
}

fn parse_review_decision(message: &str) -> &'static str {
    match parse_approval_decision(message) {
        "accept" => "approved",
        "acceptForSession" => "approved_for_session",
        "decline" => "denied",
        "cancel" => "abort",
        _ => "approved",
    }
}

fn codex_has_http_mcp_servers(startup_mcp_servers: &[StartupMcpServer]) -> bool {
    startup_mcp_servers.iter().any(|server| {
        matches!(
            server.transport,
            StartupMcpTransport::Http {
                url: _,
                headers: _,
                bearer_token_env_var: _,
            }
        )
    })
}

fn codex_sandbox_mode(
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
) -> &'static str {
    if execution_mode == BackendExecutionMode::InferenceOnly {
        return CODEX_INFERENCE_SANDBOX;
    }
    match access_mode {
        BackendAccessMode::Unrestricted => CODEX_UNRESTRICTED_SANDBOX,
        BackendAccessMode::ReadOnly => CODEX_READ_ONLY_SANDBOX,
    }
}

fn codex_approval_policy(execution_mode: BackendExecutionMode) -> &'static str {
    match execution_mode {
        BackendExecutionMode::Agent => CODEX_FORCED_APPROVAL_POLICY,
        BackendExecutionMode::InferenceOnly => CODEX_INFERENCE_APPROVAL_POLICY,
    }
}

fn codex_danger_full_access_sandbox_policy(_network_access: bool) -> Value {
    json!({ "type": "dangerFullAccess" })
}

fn codex_workspace_write_sandbox_policy(network_access: bool) -> Value {
    json!({
        "type": "workspaceWrite",
        "networkAccess": network_access,
    })
}

fn codex_inference_sandbox_policy() -> Value {
    json!({
        "type": "readOnly",
        "networkAccess": false,
    })
}

fn codex_sandbox_policy(
    access_mode: BackendAccessMode,
    network_access: bool,
    execution_mode: BackendExecutionMode,
) -> Value {
    if execution_mode == BackendExecutionMode::InferenceOnly {
        return codex_inference_sandbox_policy();
    }
    match access_mode {
        BackendAccessMode::Unrestricted => codex_danger_full_access_sandbox_policy(network_access),
        BackendAccessMode::ReadOnly => codex_workspace_write_sandbox_policy(network_access),
    }
}

fn codex_inference_config_overrides() -> Vec<String> {
    [
        "features.shell_tool=false",
        "features.unified_exec=false",
        "features.js_repl=false",
        "features.code_mode=false",
        "features.code_mode_host=false",
        "features.code_mode_only=false",
        "features.multi_agent=false",
        "features.multi_agent_v2=false",
        "features.multi_agent_mode=false",
        "features.web_search_request=false",
        "features.web_search_cached=false",
        "features.standalone_web_search=false",
        "features.search_tool=false",
        "features.image_generation=false",
        "features.apps=false",
        "features.enable_mcp_apps=false",
        "features.tool_search=false",
        "features.plugins=false",
        "features.tool_suggest=false",
        "features.request_permissions_tool=false",
        "features.default_mode_request_user_input=false",
        "features.in_app_browser=false",
        "features.browser_use=false",
        "features.browser_use_full_cdp_access=false",
        "features.browser_use_external=false",
        "features.computer_use=false",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn codex_native_home() -> Result<std::path::PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = codex_test_native_home_override()
        .lock()
        .expect("codex test native home mutex poisoned")
        .clone()
    {
        return Ok(path);
    }

    if let Some(path) = std::env::var_os("CODEX_HOME")
        && !path.is_empty()
    {
        return Ok(path.into());
    }
    Ok(crate::paths::home_dir()?.join(".codex"))
}

fn create_codex_inference_home() -> Result<tempfile::TempDir, String> {
    let isolated_home = tempfile::Builder::new()
        .prefix("tyde-codex-inference-")
        .tempdir()
        .map_err(|err| format!("Failed to create isolated Codex inference home: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(isolated_home.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|err| format!("Failed to secure isolated Codex inference home: {err}"))?;
    }

    let source_auth = codex_native_home()?.join("auth.json");
    match std::fs::metadata(&source_auth) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err("Codex authentication source is not a regular file".to_owned());
            }
            let mut source = std::fs::File::open(&source_auth)
                .map_err(|err| format!("Failed to open Codex authentication source: {err}"))?;
            let destination = isolated_home.path().join("auth.json");
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut isolated = options
                .open(destination)
                .map_err(|err| format!("Failed to create isolated Codex authentication: {err}"))?;
            std::io::copy(&mut source, &mut isolated)
                .map_err(|err| format!("Failed to copy Codex authentication securely: {err}"))?;
            isolated
                .sync_all()
                .map_err(|err| format!("Failed to persist isolated Codex authentication: {err}"))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect Codex authentication source: {err}"
            ));
        }
    }

    let config_path = isolated_home.path().join("config.toml");
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut config = options
        .open(config_path)
        .map_err(|err| format!("Failed to create isolated Codex configuration: {err}"))?;
    use std::io::Write as _;
    config
        .write_all(b"cli_auth_credentials_store = \"file\"\n")
        .map_err(|err| format!("Failed to write isolated Codex configuration: {err}"))?;
    config
        .sync_all()
        .map_err(|err| format!("Failed to persist isolated Codex configuration: {err}"))?;

    Ok(isolated_home)
}

fn codex_app_server_args(
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
    config_overrides: &[String],
) -> Vec<String> {
    let mut args = vec![
        "--sandbox".to_string(),
        codex_sandbox_mode(access_mode, execution_mode).to_string(),
        "app-server".to_string(),
        "--listen".to_string(),
        "stdio://".to_string(),
    ];
    for override_key_value in config_overrides {
        args.push("-c".to_string());
        args.push(override_key_value.clone());
    }
    args
}

fn normalize_reasoning_effort(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let value = match normalized.as_str() {
        "off" => "none",
        "min" => "minimal",
        "med" => "medium",
        _ => normalized.as_str(),
    };
    Some(value.to_string())
}

fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
    if let Some(root) = workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.trim_start().starts_with("ssh://"))
        .cloned()
    {
        return Ok(root);
    }
    if workspace_roots
        .iter()
        .any(|root| !root.trim().is_empty() && root.trim_start().starts_with("ssh://"))
    {
        return Err("Codex backend requires at least one local workspace root".to_string());
    }
    crate::backend::tyde_owned_no_root_cwd("codex")
}

fn codex_runtime_workspace_roots(workspace_roots: &[String], cwd: &str) -> Vec<String> {
    let mut roots = workspace_roots
        .iter()
        .filter_map(|root| {
            let trimmed = root.trim();
            (!trimmed.is_empty() && !trimmed.starts_with("ssh://")).then(|| root.clone())
        })
        .collect::<Vec<_>>();

    if roots.is_empty() {
        if workspace_roots.iter().any(|root| !root.trim().is_empty()) {
            roots.push(cwd.to_string());
        }
    } else if !roots.iter().any(|root| root == cwd) {
        roots.insert(0, cwd.to_string());
    }

    roots
}

async fn persist_temp_image(image: &ImageAttachment) -> Result<String, String> {
    static IMAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

    let bytes = BASE64_STANDARD
        .decode(image.data.trim())
        .map_err(|e| format!("Failed to decode image attachment '{}': {e}", image.name))?;

    let ext = media_type_to_extension(&image.media_type);
    let id = IMAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts_ms = unix_now_ms();

    let dir = std::env::temp_dir().join("tyde-codex-images");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create temp image directory: {e}"))?;

    let file_name = format!("{}_{}_{}.{}", sanitize_name(&image.name), ts_ms, id, ext);
    let path = dir.join(file_name);
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| format!("Failed to write temp image file: {e}"))?;

    Ok(path.to_string_lossy().to_string())
}

fn sanitize_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    }
}

fn media_type_to_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "png",
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

#[derive(Clone)]
enum CodexInbound {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Stderr(String),
    Closed {
        exit_code: Option<i32>,
    },
}

fn toml_quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

const CODEX_AGENT_AWAIT_TOOL_TIMEOUT_SECS: u64 = 315_576_000;

fn codex_mcp_config_overrides(startup_mcp_servers: &[StartupMcpServer]) -> Vec<String> {
    let mut overrides = Vec::new();

    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let base = format!("mcp_servers.{name}");
        if name == AGENT_CONTROL_AWAIT_MCP_SERVER_NAME {
            // Codex otherwise applies its 300-second default to a tool whose
            // contract is to wait until an agent changes state.
            overrides.push(format!(
                "{base}.tool_timeout_sec={CODEX_AGENT_AWAIT_TOOL_TIMEOUT_SECS}"
            ));
        }
        match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
                ..
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                overrides.push(format!("{base}.url={}", toml_quoted(trimmed_url)));
                for (key, value) in headers {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    overrides.push(format!("{base}.http_headers.{key}={}", toml_quoted(value)));
                }
                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    overrides.push(format!(
                        "{base}.bearer_token_env_var={}",
                        toml_quoted(env_var)
                    ));
                }
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                overrides.push(format!("{base}.command={}", toml_quoted(trimmed_command)));
                if !args.is_empty() {
                    let args_literal =
                        serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
                    overrides.push(format!("{base}.args={args_literal}"));
                }
                for (key, value) in env {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    overrides.push(format!("{base}.env.{key}={}", toml_quoted(value)));
                }
            }
        }
    }

    overrides
}

type PendingRpcMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

fn codex_rpc_error_message(err_obj: &Value) -> String {
    let message = err_obj
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (err_obj.get("code").and_then(Value::as_i64), message) {
        (Some(code), Some(message)) => format!("Codex JSON-RPC error {code}: {message}"),
        (Some(code), None) => format!("Codex JSON-RPC error {code}: {err_obj}"),
        (None, Some(message)) => message,
        (None, None) => format!("Codex JSON-RPC error: {err_obj}"),
    }
}

struct CodexRpc {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingRpcMap,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<AsyncGroupChild>>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    _isolated_codex_home: Option<tempfile::TempDir>,
}

impl CodexRpc {
    async fn spawn(
        ssh_host: Option<&str>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_tempfile: Option<&std::path::Path>,
        access_mode: BackendAccessMode,
        execution_mode: BackendExecutionMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CodexInbound>), String> {
        Self::spawn_with_local_program(
            ssh_host,
            startup_mcp_servers,
            steering_tempfile,
            access_mode,
            execution_mode,
            None,
        )
        .await
    }

    async fn spawn_with_local_program(
        ssh_host: Option<&str>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_tempfile: Option<&std::path::Path>,
        access_mode: BackendAccessMode,
        execution_mode: BackendExecutionMode,
        local_program: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CodexInbound>), String> {
        if execution_mode == BackendExecutionMode::InferenceOnly && ssh_host.is_some() {
            return Err(
                "Codex transient inference requires a local isolated configuration".to_owned(),
            );
        }
        let isolated_codex_home = if execution_mode == BackendExecutionMode::InferenceOnly {
            Some(create_codex_inference_home()?)
        } else {
            None
        };
        let mut config_overrides = codex_mcp_config_overrides(startup_mcp_servers);
        if execution_mode == BackendExecutionMode::InferenceOnly {
            config_overrides.extend(codex_inference_config_overrides());
        }
        if let Some(path) = steering_tempfile {
            config_overrides.push(format!(
                "model_instructions_file={}",
                toml_quoted(&path.display().to_string())
            ));
        }
        let mut child = if let Some(host) = ssh_host {
            let remote_args = codex_app_server_args(access_mode, execution_mode, &config_overrides);
            crate::remote::spawn_remote_process(host, "codex", &remote_args, None).await?
        } else {
            let mut cmd = match local_program {
                Some(program) => Command::new(program),
                None => codex_command(),
            };
            for arg in codex_app_server_args(access_mode, execution_mode, &config_overrides) {
                cmd.arg(arg);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                cmd.env("PATH", path);
            }
            if let Some(home) = isolated_codex_home.as_ref() {
                cmd.env("CODEX_HOME", home.path());
            }
            #[cfg(test)]
            if execution_mode == BackendExecutionMode::Agent
                && let Some(home) = codex_test_native_home_override()
                    .lock()
                    .expect("codex test native home mutex poisoned")
                    .clone()
            {
                cmd.env("CODEX_HOME", home);
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .group_spawn()
                .map_err(|e| format!("Failed to spawn Codex app-server: {e}"))?
        };

        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or("Failed to capture Codex stdin")?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or("Failed to capture Codex stdout")?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or("Failed to capture Codex stderr")?;

        let child_ref = Arc::new(Mutex::new(Some(child)));
        let pending: PendingRpcMap = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();

        let stdout_pending = Arc::clone(&pending);
        let stdout_inbound = inbound_tx.clone();
        let stdout_child = Arc::clone(&child_ref);
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let parsed = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("Failed to parse Codex stdout JSON: {err}; line: {line}");
                        continue;
                    }
                };

                if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
                    let has_method = parsed.get("method").is_some();
                    let has_result_or_error =
                        parsed.get("result").is_some() || parsed.get("error").is_some();
                    if has_result_or_error && !has_method {
                        let response = if let Some(result) = parsed.get("result") {
                            Ok(result.clone())
                        } else {
                            let err_obj = parsed.get("error").cloned().unwrap_or(Value::Null);
                            Err(codex_rpc_error_message(&err_obj))
                        };
                        if let Some(tx) = stdout_pending.lock().await.remove(&id) {
                            let _ = tx.send(response);
                        }
                        continue;
                    }
                }

                if let Some(method) = parsed.get("method").and_then(Value::as_str) {
                    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(id) = parsed.get("id").cloned() {
                        let _ = stdout_inbound.send(CodexInbound::ServerRequest {
                            id,
                            method: method.to_string(),
                            params,
                        });
                    } else {
                        let _ = stdout_inbound.send(CodexInbound::Notification {
                            method: method.to_string(),
                            params,
                        });
                    }
                }
            }

            let exit_code = match stdout_child.lock().await.as_mut() {
                Some(child) => child
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|status| status.code()),
                None => None,
            };

            let mut pending = stdout_pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("Codex app-server exited before response".to_string()));
            }
            drop(pending);

            let _ = stdout_inbound.send(CodexInbound::Closed { exit_code });
        });

        let stderr_inbound = inbound_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_inbound.send(CodexInbound::Stderr(line));
            }
        });

        Ok((
            Self {
                stdin: Arc::new(Mutex::new(stdin)),
                pending,
                next_id: AtomicU64::new(1),
                child: child_ref,
                stdout_task,
                stderr_task,
                _isolated_codex_home: isolated_codex_home,
            },
            inbound_rx,
        ))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self.send_json(&payload).await {
            let _ = self.pending.lock().await.remove(&id);
            return Err(err);
        }
        observe_codex_request_sent(method);

        match tokio::time::timeout(CODEX_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Codex response channel closed".to_string()),
            Err(_) => {
                let _ = self.pending.lock().await.remove(&id);
                Err(format!("Codex request timed out for method '{method}'"))
            }
        }
    }

    async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .await
    }

    async fn send_json(&self, value: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let line = format!("{value}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to Codex stdin: {e}"))
    }

    async fn shutdown(&self) {
        let mut child_guard = self.child.lock().await;
        let Some(mut child) = child_guard.take() else {
            return;
        };

        match tokio::time::timeout(CODEX_SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill().await;
            }
        }
        // child is taken (None) — Drop will be a no-op. Drop the readers so the
        // parent-side stdio pipe fds are released even if EOF hasn't propagated.
        self.stdout_task.abort();
        self.stderr_task.abort();
    }

    async fn terminate(&self) -> Result<(), String> {
        let child = self.child.lock().await.take();
        let result = match child {
            Some(mut child) => terminate_codex_child(&mut child).await,
            None => Ok(()),
        };
        self.stdout_task.abort();
        self.stderr_task.abort();
        result
    }

    /// Reap the app-server after it exited on its own (stdout EOF → `Closed`).
    ///
    /// Unlike claude (whose stdout reader calls `mark_process_exited`, which
    /// removes the runtime from its slot so `Drop` fires), nothing takes the
    /// `CodexRpc` out of `CodexInner` when the process exits mid-session — the
    /// forwarder task still holds `Arc<CodexInner>`, so `Drop` won't run until
    /// session teardown. Without this, an exited app-server lingers as a zombie
    /// for the rest of the session (the dominant observed leak). The reader
    /// tasks are already ending on EOF, so this only takes the child and
    /// `wait()`s it. Idempotent with `shutdown()`/`Drop`.
    async fn reap_after_exit(&self) {
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

async fn terminate_codex_child(child: &mut AsyncGroupChild) -> Result<(), String> {
    let kill_error = child
        .start_kill()
        .err()
        .map(|err| format!("failed to kill Codex app-server process group: {err}"));
    let wait_error = wait_for_codex_child_exit(child.wait(), CODEX_SHUTDOWN_TIMEOUT)
        .await
        .err();

    match (kill_error, wait_error) {
        (None, None) => Ok(()),
        (Some(error), None) | (None, Some(error)) => Err(error),
        (Some(kill_error), Some(wait_error)) => Err(format!("{kill_error}; {wait_error}")),
    }
}

async fn wait_for_codex_child_exit(
    wait: impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
    timeout: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(_)) => None,
        Ok(Err(err)) => Some(format!(
            "failed to reap Codex app-server process group: {err}"
        )),
        Err(_) => Some(format!(
            "timed out after {}s reaping Codex app-server process group",
            timeout.as_secs_f64()
        )),
    }
    .map_or(Ok(()), Err)
}

impl Drop for CodexRpc {
    /// Last-ditch net for panic/teardown. NOTE: because the forwarder task
    /// holds `Arc<CodexInner>` (which owns this `CodexRpc`), this Drop does NOT
    /// fire on mid-session process exit — the two real leak paths are covered
    /// explicitly instead:
    ///
    /// - Process self-exit: `handle_inbound(Closed)` calls `reap_after_exit()`.
    /// - Client disconnect / teardown: `shutdown()` reaps the running child.
    ///
    /// Drop then only runs at final `CodexInner` teardown and is normally a
    /// no-op (child already taken); it remains as a backstop for any path that
    /// drops a `CodexRpc` without calling either of the above.
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        crate::backend::subprocess::reap_group_child_slot(&self.child);
    }
}

// ---------------------------------------------------------------------------
// Backend trait implementation
// ---------------------------------------------------------------------------

use protocol::{
    AgentInput, ChatEvent, ChatMessage, MessageSender, SessionId, SessionSettingField,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SpawnCostHint,
};

use super::{
    Backend, BackendEvent, BackendSession, BackendSpawnConfig, EventStream,
    protocol_images_to_attachments, resolve_settings as resolve_backend_settings,
    session_settings_to_json,
};

pub struct CodexBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    settings_tx: mpsc::UnboundedSender<CodexSettingsUpdate>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
}

struct CodexSettingsUpdate {
    payload: protocol::SetSessionSettingsPayload,
    reply: oneshot::Sender<Result<(), String>>,
}

impl CodexBackend {
    pub(crate) async fn set_subagent_emitter(
        &self,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(), String> {
        self.subagent_emitter_tx.send(Some(emitter)).map_err(|_| {
            "Codex sub-agent emitter update failed: backend event loop is not running".to_string()
        })
    }

    pub(crate) async fn spawn_with_subagent_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_with_initial_emitter(workspace_roots, config, initial_input, Some(emitter))
            .await
    }

    async fn spawn_with_initial_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        initial_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, EventStream), String> {
        let inference_only = config.execution_mode == BackendExecutionMode::InferenceOnly;
        let initial_emitter = (!inference_only).then_some(initial_emitter).flatten();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (settings_tx, mut settings_rx) = mpsc::unbounded_channel::<CodexSettingsUpdate>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(initial_emitter.clone());
        let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, String>>();
        let (startup_cancel_tx, startup_cancel_rx) = oneshot::channel();
        let mut startup_cancel_guard = CodexStartupCancelGuard(Some(startup_cancel_tx));

        tokio::spawn(async move {
            let combined_instructions = (!inference_only)
                .then(|| render_combined_spawn_instructions(&config.resolved_spawn_config))
                .flatten();
            let startup_mcp_servers = if inference_only {
                &[][..]
            } else {
                config.startup_mcp_servers.as_slice()
            };
            let session_result = CodexSession::spawn_with_mode(
                &workspace_roots,
                None,
                startup_mcp_servers,
                combined_instructions.as_deref(),
                CodexSessionSpawnOptions {
                    ephemeral: false,
                    access_mode: config.resolved_spawn_config.access_mode,
                    subagent_emitter: initial_emitter,
                    execution_mode: config.execution_mode,
                },
            )
            .await;
            let (session, mut raw_events) = match session_result {
                Ok(value) => value,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!("Failed to start Codex session: {err}")));
                    return;
                }
            };

            // `thread/start` has already supplied the authoritative ID. Publish
            // it before doing any further startup work: Codex may announce a
            // native child before the initial turn RPC responds.
            let session_id = session.session_id();
            if ready_tx.send(Ok(session_id)).is_err() {
                session.shutdown().await;
                return;
            }
            observe_codex_spawn_ready();
            if startup_cancel_rx.await.is_ok() {
                session.shutdown().await;
                observe_codex_spawn_startup_cancelled();
                return;
            }

            let handle = session.command_handle();
            let resolved_settings = if inference_only {
                protocol::SessionSettingsValues::default()
            } else {
                resolve_session_settings(&config)
            };
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("reasoning_effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            tracing::debug!(
                inference_only,
                has_model_override = model_override.is_some(),
                has_effort_override = effort_override.is_some(),
                "Codex startup settings resolved"
            );
            let mut normalization_failures = HashMap::new();
            let mut pending_initial_input_cancelled = false;
            if model_override.is_some() || effort_override.is_some() {
                tracing::debug!("Codex startup dispatching thread/update");
                let settings = json!({
                    "model": model_override,
                    "reasoning_effort": effort_override,
                    "approval_policy": CODEX_FORCED_APPROVAL_POLICY,
                });
                let settings_handle = handle.clone();
                let settings_request = settings_handle.update_runtime_settings(settings);
                tokio::pin!(settings_request);

                enum StartupSettingsPhase {
                    Configured,
                    Terminated,
                    Failed(String),
                }

                let settings_phase = loop {
                    tokio::select! {
                        biased;
                        interrupt = interrupt_rx.recv() => {
                            let Some(()) = interrupt else {
                                break StartupSettingsPhase::Terminated;
                            };
                            if !pending_initial_input_cancelled {
                                // No initial turn exists yet, so Codex cannot cancel it.
                                pending_initial_input_cancelled = true;
                                let _ = events_tx.send(BackendEvent::Chat(
                                    ChatEvent::OperationCancelled(
                                        protocol::OperationCancelledData {
                                            message: "Operation cancelled".to_string(),
                                        },
                                    ),
                                ));
                                let _ = events_tx
                                    .send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                            }
                        }
                        result = &mut settings_request => {
                            while interrupt_rx.try_recv().is_ok() {
                                if !pending_initial_input_cancelled {
                                    pending_initial_input_cancelled = true;
                                    let _ = events_tx.send(BackendEvent::Chat(
                                        ChatEvent::OperationCancelled(
                                            protocol::OperationCancelledData {
                                                message: "Operation cancelled".to_string(),
                                            },
                                        ),
                                    ));
                                    let _ = events_tx
                                        .send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                                }
                            }
                            break match result {
                                Ok(()) => StartupSettingsPhase::Configured,
                                Err(err) => StartupSettingsPhase::Failed(format!(
                                    "Failed to configure Codex session: {err}"
                                )),
                            };
                        }
                        incoming = raw_events.recv() => {
                            let Some(raw) = incoming else {
                                break StartupSettingsPhase::Failed(
                                    "Codex event stream ended while applying startup settings"
                                        .to_string(),
                                );
                            };
                            if !forward_codex_backend_stream_event(
                                raw,
                                &events_tx,
                                &mut normalization_failures,
                            ) {
                                break StartupSettingsPhase::Terminated;
                            }
                        }
                        changed = subagent_emitter_rx.changed() => {
                            if changed.is_err() {
                                break StartupSettingsPhase::Terminated;
                            }
                            let maybe_emitter = subagent_emitter_rx.borrow().clone();
                            if let Some(emitter) = maybe_emitter
                                && let Err(err) = session.set_subagent_emitter(emitter).await
                            {
                                break StartupSettingsPhase::Failed(format!(
                                    "Codex sub-agent emitter update failed while applying startup settings: {err}"
                                ));
                            }
                        }
                    }
                };

                match settings_phase {
                    StartupSettingsPhase::Configured => {}
                    StartupSettingsPhase::Terminated => {
                        drop(events_tx);
                        session.shutdown().await;
                        return;
                    }
                    StartupSettingsPhase::Failed(message) => {
                        tracing::error!(
                            %message,
                            "Codex startup settings failed after session publication"
                        );
                        let _ = events_tx.send(BackendEvent::Chat(backend_error_message(message)));
                        let _ = events_tx
                            .send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                        drop(events_tx);
                        session.shutdown().await;
                        return;
                    }
                }
            } else {
                let local_settings = json!({
                    "model": Value::Null,
                    "reasoning_effort": Value::Null,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings: local_settings,
                        persist: false,
                    })
                    .await
                {
                    let message = format!("Failed to apply Codex startup settings: {err}");
                    tracing::error!(
                        %message,
                        "Codex startup settings failed after session publication"
                    );
                    let _ = events_tx.send(BackendEvent::Chat(backend_error_message(message)));
                    let _ =
                        events_tx.send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                    drop(events_tx);
                    session.shutdown().await;
                    return;
                }
            }

            let images = protocol_images_to_attachments(initial_input.images);
            let (initial_turn_tx, mut initial_turn_rx) = oneshot::channel();
            let mut initial_turn_pending = !pending_initial_input_cancelled;
            if initial_turn_pending {
                let initial_turn_handle = handle.clone();
                tokio::spawn(async move {
                    let result = initial_turn_handle
                        .execute(SessionCommand::SendMessage {
                            message: initial_input.message,
                            images,
                        })
                        .await;
                    let _ = initial_turn_tx.send(result);
                });
            } else {
                drop(initial_turn_tx);
            }

            loop {
                tokio::select! {
                    result = &mut initial_turn_rx, if initial_turn_pending => {
                        initial_turn_pending = false;
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                let message = format!("Failed to send initial Codex prompt: {err}");
                                tracing::error!(%err, "Codex initial prompt failed after session registration");
                                let _ = events_tx.send(BackendEvent::Chat(backend_error_message(message)));
                                break;
                            }
                            Err(_) => {
                                let _ = events_tx.send(BackendEvent::Chat(backend_error_message(
                                    "Codex initial prompt task ended before reporting its result".to_string(),
                                )));
                                break;
                            }
                        }
                    }
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else { break; };
                        if !forward_codex_backend_stream_event(
                            raw,
                            &events_tx,
                            &mut normalization_failures,
                        ) {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else { break; };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let images = protocol_images_to_attachments(payload.images);
                                if let Err(err) = handle.execute(SessionCommand::SendMessage {
                                    message: payload.message,
                                    images,
                                }).await {
                                    tracing::error!(%err, "Failed to send Codex follow-up");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(_) => {}
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!("queued-message inputs must be handled by the agent actor before reaching the backend");
                            }
                        }
                    }
                    update = settings_rx.recv() => {
                        let Some(update) = update else { break; };
                        let result = handle
                            .update_runtime_settings(session_settings_to_json(&update.payload.values))
                            .await
                            .map_err(|err| format!("Codex session settings update failed: {err}"));
                        let _ = update.reply.send(result);
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(()) = interrupt else { break; };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!(%err, "Failed to interrupt Codex turn");
                            break;
                        }
                    }
                    changed = subagent_emitter_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let maybe_emitter = subagent_emitter_rx.borrow().clone();
                        if let Some(emitter) = maybe_emitter
                            && let Err(err) = session.set_subagent_emitter(emitter).await
                        {
                            tracing::error!(%err, "Codex sub-agent emitter update failed");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        let session_id = match ready_rx.await {
            Ok(Ok(session_id)) => session_id,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Codex spawn initialization task ended early".to_string()),
        };
        startup_cancel_guard.disarm();
        let backend_session_id = Arc::new(std::sync::Mutex::new(Some(session_id)));

        Ok((
            Self {
                input_tx,
                settings_tx,
                interrupt_tx,
                session_id: backend_session_id,
                subagent_emitter_tx,
            },
            EventStream::new_backend(events_rx),
        ))
    }
}

fn codex_session_settings_schema(models: Vec<CodexModelMetadata>) -> SessionSettingsSchema {
    let default_reasoning_options = models
        .iter()
        .find(|model| model.is_default)
        .map(|model| model.reasoning_options.clone())
        .unwrap_or_default();
    let model_options = models.iter().map(|model| model.option.clone()).collect();
    let reasoning_options_by_model = protocol::SelectOptionsBySetting {
        setting_key: "model".to_string(),
        values: models
            .into_iter()
            .map(|model| protocol::SelectOptionsForValue {
                setting_value: model.option.value,
                options: model.reasoning_options,
            })
            .collect(),
    };
    SessionSettingsSchema {
        backend_kind: protocol::BackendKind::Codex,
        fields: vec![
            SessionSettingField {
                key: "model".to_string(),
                label: "Model".to_string(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: model_options,
                    default: None,
                    nullable: true,
                },
            },
            SessionSettingField {
                key: "reasoning_effort".to_string(),
                label: "Reasoning Effort".to_string(),
                description: None,
                use_slider: true,
                select_options_by_setting: Some(reasoning_options_by_model),
                field_type: SessionSettingFieldType::Select {
                    options: default_reasoning_options,
                    default: None,
                    nullable: true,
                },
            },
        ],
    }
}

pub(crate) fn codex_cost_hint_defaults(
    cost_hint: SpawnCostHint,
) -> protocol::SessionSettingsValues {
    match cost_hint {
        SpawnCostHint::Low | SpawnCostHint::Medium | SpawnCostHint::High => {
            protocol::SessionSettingsValues::default()
        }
    }
}

pub(crate) fn codex_tier_config_from_schema(
    schema: &SessionSettingsSchema,
    selected_values: &protocol::SessionSettingsValues,
) -> Result<protocol::BackendTierConfig, String> {
    if schema.backend_kind != protocol::BackendKind::Codex {
        return Err("Codex tier resolution received a non-Codex schema".to_owned());
    }
    let reasoning_field = schema
        .fields
        .iter()
        .find(|field| field.key == "reasoning_effort")
        .ok_or_else(|| "Codex model metadata omitted reasoning_effort".to_owned())?;
    let options = reasoning_field
        .select_options(selected_values)
        .filter(|options| !options.is_empty())
        .ok_or_else(|| {
            "selected Codex model metadata advertised no reasoning efforts".to_owned()
        })?;
    let low = options
        .first()
        .ok_or_else(|| "selected Codex model metadata has no lowest reasoning effort".to_owned())?
        .value
        .clone();
    let high = options
        .last()
        .ok_or_else(|| "selected Codex model metadata has no highest reasoning effort".to_owned())?
        .value
        .clone();
    let mut low_values = protocol::SessionSettingsValues::default();
    low_values.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String(low),
    );
    let mut high_values = protocol::SessionSettingsValues::default();
    high_values.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String(high),
    );
    Ok(protocol::BackendTierConfig {
        low: low_values,
        high: high_values,
    })
}

pub(crate) fn resolve_session_settings(
    config: &BackendSpawnConfig,
) -> protocol::SessionSettingsValues {
    resolve_backend_settings(
        config,
        &CodexBackend::session_settings_schema(),
        codex_cost_hint_defaults,
    )
}

fn backend_error_message(content: String) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Error,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn backend_warning_message(content: String) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Warning,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn is_codex_thread_fork_unsupported_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("-32601")
        || (normalized.contains("thread/fork")
            && (normalized.contains("method not found")
                || normalized.contains("unknown method")
                || normalized.contains("unknown request")
                || normalized.contains("unsupported method")))
}

fn codex_thread_fork_unsupported_message() -> String {
    "Installed Codex CLI does not support session fork (app-server method `thread/fork`). Update Codex CLI to 0.136.0 or newer and try again."
        .to_string()
}

fn codex_ssh_fork_unsupported_error(workspace_roots: &[String]) -> Option<BackendStartupError> {
    if !workspace_roots
        .iter()
        .any(|root| root.trim_start().starts_with("ssh://"))
    {
        return None;
    }

    let detail = match crate::remote::parse_remote_workspace_roots(workspace_roots) {
        Ok(Some((host, _))) => format!(" for SSH host '{host}'"),
        Ok(None) => " for SSH workspace roots".to_string(),
        Err(err) => format!(" for SSH workspace roots ({err})"),
    };
    Some(BackendStartupError::unsupported(format!(
        "Codex backend does not support session fork{detail} yet"
    )))
}

fn emit_agent_control_await_progress_to(
    emitter: &TurnEmitter,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
) {
    if let Some(progress) = await_progress_data_for_tool(tool_call_id, tool_name, arguments) {
        emitter.tool_progress(&progress);
    }
}

fn emit_codex_tool_request(
    emitter: &TurnEmitter,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
) {
    let (tool_type, normalization_failure) = match tyde_tool_request_type(tool_name, arguments) {
        Ok(Some(typed)) => (
            serde_json::to_value(typed).expect("serialize tool request"),
            None,
        ),
        Ok(None) => (json!({ "kind": "Other", "args": arguments }), None),
        Err(error) => {
            tracing::error!(
                tool = %tool_name,
                tool_call_id = %tool_call_id,
                detail = %error.detail,
                "Canonical Tyde tool request normalization failed"
            );
            emitter.backend_error(&format!(
                "Failed to normalize canonical tool request '{}' ({}): {}",
                tool_name, tool_call_id, error
            ));
            (
                json!({ "kind": "Other", "args": arguments }),
                Some(error.normalization_failure),
            )
        }
    };
    if let Some(normalization_failure) = normalization_failure {
        emitter.tool_request_with_normalization_failure(
            tool_call_id,
            tool_name,
            tool_type,
            normalization_failure,
        );
    } else {
        emitter.tool_request(tool_call_id, tool_name, tool_type);
    }
}

fn normalize_codex_tool_result(
    emitter: &TurnEmitter,
    tool_call_id: &str,
    tool_name: &str,
    tool_result: Value,
    success: bool,
) -> (Value, Option<ToolExecutionNormalizationFailure>) {
    if !success {
        return (tool_result, None);
    }
    match tyde_tool_result(tool_name, &tool_result) {
        Ok(Some(typed)) => (
            serde_json::to_value(typed).expect("serialize tool result"),
            None,
        ),
        Ok(None) => (tool_result, None),
        Err(error) => {
            tracing::error!(
                tool = %tool_name,
                tool_call_id = %tool_call_id,
                detail = %error.detail,
                "Canonical Tyde tool result normalization failed"
            );
            emitter.backend_error(&format!(
                "Failed to normalize canonical tool result '{}' ({}): {}",
                tool_name, tool_call_id, error
            ));
            (tool_result, Some(error.normalization_failure))
        }
    }
}

fn emit_agent_control_spawn_progress_to(
    emitter: &TurnEmitter,
    tool_call_id: &str,
    tool_name: &str,
    tool_result: &Value,
) {
    if let Some(progress) =
        spawn_progress_data_for_tool_result(tool_call_id, tool_name, tool_result)
    {
        emitter.tool_progress(&progress);
    }
}

fn spawn_codex_subagent_event_bridge(
    mut raw_rx: mpsc::UnboundedReceiver<Value>,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
) {
    tokio::spawn(async move {
        let mut normalization_failures = HashMap::new();
        while let Some(raw) = raw_rx.recv().await {
            if !forward_codex_backend_event(raw, &event_tx, &mut normalization_failures) {
                break;
            }
        }
    });
}

fn forward_codex_backend_stream_event(
    raw: Value,
    events_tx: &mpsc::UnboundedSender<BackendEvent>,
    normalization_failures: &mut HashMap<String, ToolExecutionNormalizationFailure>,
) -> bool {
    if let Some(usage) = model_request_token_usage_from_raw(&raw) {
        return events_tx
            .send(BackendEvent::ModelRequestTokenUsage(usage))
            .is_ok();
    }
    let Some(event) = codex_backend_event_from_raw(&raw, normalization_failures) else {
        return true;
    };
    if let Some(error) = event.normalization_error
        && events_tx.send(BackendEvent::Chat(error)).is_err()
    {
        return false;
    }
    if events_tx
        .send(BackendEvent::Chat(event.chat_event))
        .is_err()
    {
        return false;
    }
    !event.terminal
}

fn forward_codex_backend_event(
    raw: Value,
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    normalization_failures: &mut HashMap<String, ToolExecutionNormalizationFailure>,
) -> bool {
    if model_request_token_usage_from_raw(&raw).is_some() {
        return true;
    }
    let Some(event) = codex_backend_event_from_raw(&raw, normalization_failures) else {
        return true;
    };
    if let Some(error) = event.normalization_error
        && events_tx.send(error).is_err()
    {
        return false;
    }
    if events_tx.send(event.chat_event).is_err() {
        return false;
    }
    !event.terminal
}

fn model_request_token_usage_from_raw(value: &Value) -> Option<ModelRequestTokenUsage> {
    if value.get("kind").and_then(Value::as_str) != Some("ModelRequestTokenUsage") {
        return None;
    }
    serde_json::from_value(value.get("data")?.clone()).ok()
}

struct CodexForwardedBackendEvent {
    chat_event: ChatEvent,
    terminal: bool,
    normalization_error: Option<ChatEvent>,
}

fn codex_backend_event_from_raw(
    value: &Value,
    normalization_failures: &mut HashMap<String, ToolExecutionNormalizationFailure>,
) -> Option<CodexForwardedBackendEvent> {
    match serde_json::from_value::<ChatEvent>(value.clone()) {
        Ok(event) => {
            let (chat_event, normalization_error) =
                normalize_codex_chat_event(event, normalization_failures);
            Some(CodexForwardedBackendEvent {
                chat_event,
                terminal: false,
                normalization_error,
            })
        }
        Err(err) => {
            let Some(kind) = value.get("kind").and_then(Value::as_str) else {
                tracing::warn!(raw = %value, error = %err, "Ignoring Codex raw event without kind");
                return None;
            };

            match kind {
                "ModelRequestTokenUsage" => None,
                "Error" => Some(CodexForwardedBackendEvent {
                    chat_event: backend_error_message(codex_raw_event_message(
                        value,
                        "Codex backend error",
                    )),
                    terminal: false,
                    normalization_error: None,
                }),
                "SubprocessStderr" => {
                    let message = codex_raw_event_message(value, "Codex subprocess stderr");
                    tracing::warn!(message = %message, "Codex subprocess stderr");
                    if codex_stderr_is_visible_warning(&message) {
                        Some(CodexForwardedBackendEvent {
                            chat_event: backend_warning_message(message),
                            terminal: false,
                            normalization_error: None,
                        })
                    } else {
                        None
                    }
                }
                "SubprocessExit" => {
                    let message = codex_subprocess_exit_message(value);
                    tracing::error!(message = %message, "Codex subprocess exited");
                    Some(CodexForwardedBackendEvent {
                        chat_event: backend_error_message(message),
                        terminal: true,
                        normalization_error: None,
                    })
                }
                other => {
                    tracing::warn!(
                        kind = %other,
                        raw = %value,
                        error = %err,
                        "Ignoring unsupported Codex raw event"
                    );
                    None
                }
            }
        }
    }
}

fn normalize_codex_chat_event(
    event: ChatEvent,
    normalization_failures: &mut HashMap<String, ToolExecutionNormalizationFailure>,
) -> (ChatEvent, Option<ChatEvent>) {
    match event {
        ChatEvent::ToolRequest(mut request) => {
            let protocol::ToolRequestType::Other { args } = &request.tool_type else {
                return (ChatEvent::ToolRequest(request), None);
            };
            match tyde_tool_request_type(&request.tool_name, args) {
                Ok(Some(typed)) => {
                    request.tool_type = typed;
                    (ChatEvent::ToolRequest(request), None)
                }
                Ok(None) => (ChatEvent::ToolRequest(request), None),
                Err(error) => {
                    tracing::error!(
                        tool = %request.tool_name,
                        tool_call_id = %request.tool_call_id,
                        detail = %error.detail,
                        "Canonical Tyde tool request normalization failed"
                    );
                    let visible = backend_error_message(format!(
                        "Failed to normalize canonical tool request '{}' ({}): {}",
                        request.tool_name, request.tool_call_id, error
                    ));
                    normalization_failures
                        .insert(request.tool_call_id.clone(), error.normalization_failure);
                    (ChatEvent::ToolRequest(request), Some(visible))
                }
            }
        }
        ChatEvent::ToolExecutionCompleted(mut completion) => {
            let request_failure = normalization_failures.remove(&completion.tool_call_id);
            let mut normalization_error = None;
            let result_failure = if completion.success {
                match &completion.tool_result {
                    protocol::ToolExecutionResult::Other { .. } => match tyde_tool_result(
                        &completion.tool_name,
                        &serde_json::to_value(&completion.tool_result)
                            .expect("serialize tool result"),
                    ) {
                        Ok(Some(typed)) => {
                            completion.tool_result = typed;
                            None
                        }
                        Ok(None) => None,
                        Err(error) => {
                            tracing::error!(
                                tool = %completion.tool_name,
                                tool_call_id = %completion.tool_call_id,
                                detail = %error.detail,
                                "Canonical Tyde tool result normalization failed"
                            );
                            normalization_error = Some(backend_error_message(format!(
                                "Failed to normalize canonical tool result '{}' ({}): {}",
                                completion.tool_name, completion.tool_call_id, error
                            )));
                            Some(error.normalization_failure)
                        }
                    },
                    _ => None,
                }
            } else {
                None
            };
            completion.normalization_failure = merge_completion_normalization_failure(
                completion.normalization_failure,
                request_failure,
            );
            completion.normalization_failure = merge_completion_normalization_failure(
                completion.normalization_failure,
                result_failure,
            );
            (
                ChatEvent::ToolExecutionCompleted(completion),
                normalization_error,
            )
        }
        event => (event, None),
    }
}

fn merge_completion_normalization_failure(
    existing: Option<ToolExecutionNormalizationFailure>,
    incoming: Option<ToolExecutionNormalizationFailure>,
) -> Option<ToolExecutionNormalizationFailure> {
    match (existing, incoming) {
        (None, None) => None,
        (Some(failure), None) | (None, Some(failure)) => Some(failure),
        (Some(existing), Some(incoming)) => Some(existing.combined_with(incoming)),
    }
}

fn codex_stderr_is_visible_warning(message: &str) -> bool {
    message.trim_start().starts_with("Codex warning:")
}

fn codex_subprocess_exit_message(value: &Value) -> String {
    match value
        .get("data")
        .and_then(|data| data.get("exit_code"))
        .and_then(Value::as_i64)
    {
        Some(exit_code) => format!("Codex subprocess exited with code {exit_code}"),
        None => "Codex subprocess exited".to_string(),
    }
}

struct CodexStartupCancelGuard(Option<oneshot::Sender<()>>);

#[cfg(test)]
struct CodexRequestObserver {
    method: String,
    sender: oneshot::Sender<()>,
}

#[cfg(test)]
static CODEX_REQUEST_SENT_OBSERVER: std::sync::Mutex<Option<CodexRequestObserver>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static CODEX_SPAWN_READY_OBSERVER: std::sync::Mutex<Option<oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static CODEX_SPAWN_STARTUP_CANCEL_OBSERVER: std::sync::Mutex<Option<oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static CODEX_FORK_STARTUP_CANCEL_OBSERVER: std::sync::Mutex<Option<oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn install_codex_request_observer(method: &str) -> oneshot::Receiver<()> {
    let (sender, receiver) = oneshot::channel();
    let mut observer = CODEX_REQUEST_SENT_OBSERVER
        .lock()
        .expect("Codex request observer mutex poisoned");
    assert!(
        observer.is_none(),
        "a Codex request observer is already installed"
    );
    *observer = Some(CodexRequestObserver {
        method: method.to_string(),
        sender,
    });
    receiver
}

#[cfg(test)]
fn observe_codex_request_sent(method: &str) {
    let mut observer = CODEX_REQUEST_SENT_OBSERVER
        .lock()
        .expect("Codex request observer mutex poisoned");
    if observer
        .as_ref()
        .is_some_and(|expected| expected.method == method)
        && let Some(expected) = observer.take()
    {
        let _ = expected.sender.send(());
    }
}

#[cfg(not(test))]
fn observe_codex_request_sent(_method: &str) {}

#[cfg(test)]
fn observe_codex_fork_startup_cancelled() {
    if let Some(observer) = CODEX_FORK_STARTUP_CANCEL_OBSERVER
        .lock()
        .expect("Codex fork startup cancel observer mutex poisoned")
        .take()
    {
        let _ = observer.send(());
    }
}

#[cfg(not(test))]
fn observe_codex_fork_startup_cancelled() {}

#[cfg(test)]
fn observe_codex_spawn_ready() {
    if let Some(observer) = CODEX_SPAWN_READY_OBSERVER
        .lock()
        .expect("Codex spawn ready observer mutex poisoned")
        .take()
    {
        let _ = observer.send(());
    }
}

#[cfg(not(test))]
fn observe_codex_spawn_ready() {}

#[cfg(test)]
fn observe_codex_spawn_startup_cancelled() {
    if let Some(observer) = CODEX_SPAWN_STARTUP_CANCEL_OBSERVER
        .lock()
        .expect("Codex spawn startup cancel observer mutex poisoned")
        .take()
    {
        let _ = observer.send(());
    }
}

#[cfg(not(test))]
fn observe_codex_spawn_startup_cancelled() {}

impl CodexStartupCancelGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CodexStartupCancelGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

fn codex_raw_event_message(value: &Value, default_message: &str) -> String {
    value
        .get("data")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| data.get("message"))
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        })
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        })
        .map(str::to_string)
        .or_else(|| {
            value
                .get("data")
                .filter(|data| !data.is_null())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| default_message.to_string())
}

impl Backend for CodexBackend {
    fn session_settings_schema() -> SessionSettingsSchema {
        codex_session_settings_schema(Vec::new())
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_with_initial_emitter(workspace_roots, config, initial_input, None).await
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: protocol::SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (settings_tx, mut settings_rx) = mpsc::unbounded_channel::<CodexSettingsUpdate>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);

        let session_id = session_id.0;
        let backend_session_id =
            Arc::new(std::sync::Mutex::new(Some(SessionId(session_id.clone()))));

        tokio::spawn(async move {
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match CodexSession::spawn(
                &workspace_roots,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
                config.resolved_spawn_config.access_mode,
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    tracing::error!("Failed to spawn Codex resume session: {err}");
                    return;
                }
            };

            let handle = session.command_handle();
            let maybe_emitter = subagent_emitter_rx.borrow().clone();
            if let Some(emitter) = maybe_emitter
                && let Err(err) = session.set_subagent_emitter(emitter).await
            {
                tracing::error!(%err, "Failed to install Codex sub-agent emitter for resumed session");
                session.shutdown().await;
                return;
            }
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("reasoning_effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() || effort_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "reasoning_effort": effort_override,
                    "approval_policy": CODEX_FORCED_APPROVAL_POLICY,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    tracing::error!("Failed to configure resumed Codex session: {err}");
                    session.shutdown().await;
                    return;
                }
            }

            if let Err(err) = handle
                .execute(SessionCommand::ResumeSession { session_id })
                .await
            {
                tracing::error!("Failed to resume Codex session: {err}");
                session.shutdown().await;
                return;
            }

            let mut normalization_failures = HashMap::new();
            while let Ok(raw) = raw_events.try_recv() {
                if !forward_codex_backend_stream_event(raw, &events_tx, &mut normalization_failures)
                {
                    session.shutdown().await;
                    return;
                }
            }
            let _ = resume_replay_complete_tx.send(());

            loop {
                tokio::select! {
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else {
                            break;
                        };
                        if !forward_codex_backend_stream_event(
                            raw,
                            &events_tx,
                            &mut normalization_failures,
                        ) {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let images = protocol_images_to_attachments(payload.images);
                                if let Err(err) = handle
                                    .execute(SessionCommand::SendMessage {
                                        message: payload.message,
                                        images,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to send Codex resume follow-up: {err}");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(_) => {}
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!(
                                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                                );
                            }
                        }
                    }
                    update = settings_rx.recv() => {
                        let Some(update) = update else { break };
                        let result = handle
                            .update_runtime_settings(session_settings_to_json(&update.payload.values))
                            .await;
                        let _ = update.reply.send(result);
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(()) = interrupt else { break };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!("Failed to interrupt resumed Codex turn: {err}");
                            break;
                        }
                    }
                    changed = subagent_emitter_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let maybe_emitter = subagent_emitter_rx.borrow().clone();
                        if let Some(emitter) = maybe_emitter
                            && let Err(err) = session.set_subagent_emitter(emitter).await
                        {
                            tracing::error!(%err, "Failed to update Codex sub-agent emitter for resumed session");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        Ok((
            Self {
                input_tx,
                settings_tx,
                interrupt_tx,
                session_id: backend_session_id,
                subagent_emitter_tx,
            },
            EventStream::new_backend_with_resume_replay_barrier(
                events_rx,
                resume_replay_complete_rx,
            ),
        ))
    }

    async fn fork(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: protocol::SessionId,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        if let Some(error) = codex_ssh_fork_unsupported_error(&workspace_roots) {
            return Err(error);
        }

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (settings_tx, mut settings_rx) = mpsc::unbounded_channel::<CodexSettingsUpdate>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);

        let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, BackendStartupError>>();
        let (startup_cancel_tx, mut startup_cancel_rx) = oneshot::channel();
        let mut startup_cancel_guard = CodexStartupCancelGuard(Some(startup_cancel_tx));

        tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match CodexSession::fork(
                &workspace_roots,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
                config.resolved_spawn_config.access_mode,
                &from_session_id.0,
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    let startup_error = if is_codex_thread_fork_unsupported_error(&err) {
                        BackendStartupError::unsupported(codex_thread_fork_unsupported_message())
                    } else {
                        BackendStartupError::backend_failed(format!(
                            "Failed to fork Codex session: {err}"
                        ))
                    };
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(startup_error));
                    }
                    return;
                }
            };

            let child_session_id = session.session_id();
            let handle = session.command_handle();
            let maybe_emitter = subagent_emitter_rx.borrow().clone();
            if let Some(emitter) = maybe_emitter
                && let Err(err) = session.set_subagent_emitter(emitter).await
            {
                session.shutdown().await;
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(BackendStartupError::backend_failed(format!(
                        "Failed to install Codex sub-agent emitter for forked session: {err}"
                    ))));
                }
                return;
            }

            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("reasoning_effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() || effort_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "reasoning_effort": effort_override,
                    "approval_policy": CODEX_FORCED_APPROVAL_POLICY,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    session.shutdown().await;
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(BackendStartupError::backend_failed(format!(
                            "Failed to configure forked Codex session: {err}"
                        ))));
                    }
                    return;
                }
            }

            if ready_tx.as_ref().is_some_and(oneshot::Sender::is_closed) {
                session.shutdown().await;
                observe_codex_fork_startup_cancelled();
                return;
            }

            let images = protocol_images_to_attachments(initial_input.images);
            let initial_prompt = handle.execute(SessionCommand::SendMessage {
                message: initial_input.message,
                images,
            });
            tokio::pin!(initial_prompt);
            tokio::select! {
                biased;
                _ = &mut startup_cancel_rx => {
                    session.shutdown().await;
                    observe_codex_fork_startup_cancelled();
                    return;
                }
                result = &mut initial_prompt => {
                    if let Err(err) = result {
                        session.shutdown().await;
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Err(BackendStartupError::backend_failed(format!(
                                "Failed to send initial Codex fork prompt: {err}"
                            ))));
                        }
                        return;
                    }
                }
            }

            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(child_session_id));
            }
            let mut normalization_failures = HashMap::new();

            loop {
                tokio::select! {
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else {
                            break;
                        };
                        if !forward_codex_backend_stream_event(
                            raw,
                            &events_tx,
                            &mut normalization_failures,
                        ) {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                let images = protocol_images_to_attachments(payload.images);
                                if let Err(err) = handle
                                    .execute(SessionCommand::SendMessage {
                                        message: payload.message,
                                        images,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to send Codex fork follow-up: {err}");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(_) => {}
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!(
                                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                                );
                            }
                        }
                    }
                    update = settings_rx.recv() => {
                        let Some(update) = update else { break };
                        let result = handle
                            .update_runtime_settings(session_settings_to_json(&update.payload.values))
                            .await;
                        let _ = update.reply.send(result);
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(()) = interrupt else { break };
                        if let Err(err) = handle.execute(SessionCommand::CancelConversation).await {
                            tracing::error!("Failed to interrupt forked Codex turn: {err}");
                            break;
                        }
                    }
                    changed = subagent_emitter_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let maybe_emitter = subagent_emitter_rx.borrow().clone();
                        if let Some(emitter) = maybe_emitter
                            && let Err(err) = session.set_subagent_emitter(emitter).await
                        {
                            tracing::error!(%err, "Failed to update Codex sub-agent emitter for forked session");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        let child_session_id = match ready_rx.await {
            Ok(Ok(session_id)) => session_id,
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                return Err(BackendStartupError::backend_failed(
                    "Codex fork initialization task ended early",
                ));
            }
        };
        startup_cancel_guard.disarm();
        let backend_session_id = Arc::new(std::sync::Mutex::new(Some(child_session_id)));

        Ok((
            Self {
                input_tx,
                settings_tx,
                interrupt_tx,
                session_id: backend_session_id,
                subagent_emitter_tx,
            },
            EventStream::new_backend(events_rx),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        Err("CodexBackend::list_sessions requires a live Codex RPC session".to_string())
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("codex session_id mutex poisoned")
            .clone()
            .expect("codex session_id not initialized")
    }

    async fn send(&self, input: AgentInput) -> bool {
        match input {
            AgentInput::UpdateSessionSettings(_) => false,
            other => self.input_tx.send(other).is_ok(),
        }
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.settings_tx
            .send(CodexSettingsUpdate { payload, reply })
            .map_err(|_| "Codex backend terminated before applying session settings".to_owned())?;
        result
            .await
            .map_err(|_| "Codex settings update response channel closed".to_owned())?
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).is_ok()
    }

    async fn shutdown(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sub_agent::SubAgentHandle;
    use protocol::{
        AgentBootstrapEvent, AgentBootstrapPayload, AgentControlProgressKind, AgentErrorCode,
        AgentId, AgentOrigin, AgentStartPayload, BackendKind, BackendSetupPayload, ChatEvent,
        Envelope, FrameKind, HostBootstrapPayload, HostSettings, MobileAccessStatePayload,
        MobileBrokerStatus, MobilePairingState, NewAgentPayload, PROTOCOL_VERSION,
        ProtocolValidator, StreamPath, TeamPresetCatalog, TokenUsageScope, ToolProgressUpdate,
        Version, WelcomePayload,
    };
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, MutexGuard, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static CODEX_FAKE_APP_SERVER_SERIAL: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

    struct CodexFakeAppServer {
        _dir: tempfile::TempDir,
        binary: std::path::PathBuf,
        capture: std::path::PathBuf,
        argv_capture: std::path::PathBuf,
        initial_turn_gate: std::path::PathBuf,
        fork_response_marker: std::path::PathBuf,
        startup_settings_gate: std::path::PathBuf,
        command_execution_marker: std::path::PathBuf,
        native_mcp_contacts: std::path::PathBuf,
    }

    #[derive(Clone)]
    struct CapturedCodexRequest {
        pid: u64,
        request: Value,
    }

    struct CapturedCodexArgv {
        pid: u64,
        argv: Vec<String>,
        codex_home: Option<std::path::PathBuf>,
        auth_present: bool,
        native_mcp_configured: bool,
    }

    struct CodexTestAppServerBinaryGuard {
        _serial: MutexGuard<'static, ()>,
        previous: Option<std::path::PathBuf>,
        previous_native_home: Option<std::path::PathBuf>,
    }

    impl CodexTestAppServerBinaryGuard {
        fn set(binary: std::path::PathBuf) -> Self {
            Self::set_with_native_home(binary, None)
        }

        fn set_with_native_home(
            binary: std::path::PathBuf,
            native_home: Option<std::path::PathBuf>,
        ) -> Self {
            let serial = CODEX_FAKE_APP_SERVER_SERIAL
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = codex_test_app_server_binary_override()
                .lock()
                .expect("codex test app-server binary mutex poisoned")
                .replace(binary);
            let previous_native_home = std::mem::replace(
                &mut *codex_test_native_home_override()
                    .lock()
                    .expect("codex test native home mutex poisoned"),
                native_home,
            );
            Self {
                _serial: serial,
                previous,
                previous_native_home,
            }
        }
    }

    impl Drop for CodexTestAppServerBinaryGuard {
        fn drop(&mut self) {
            *codex_test_app_server_binary_override()
                .lock()
                .expect("codex test app-server binary mutex poisoned") = self.previous.take();
            *codex_test_native_home_override()
                .lock()
                .expect("codex test native home mutex poisoned") = self.previous_native_home.take();
        }
    }

    #[test]
    fn codex_pick_workspace_root_uses_tyde_no_root_cwd_for_empty_roots() {
        let root = pick_workspace_root(&[]).expect("empty roots should resolve to no-root cwd");

        assert!(std::path::Path::new(&root).is_dir());
        assert!(
            std::path::Path::new(&root)
                .ends_with(std::path::Path::new(".tyde").join("codex").join("no-root"))
        );
        assert!(codex_runtime_workspace_roots(&[], &root).is_empty());
    }

    #[test]
    fn codex_pick_workspace_root_keeps_ssh_only_roots_invalid() {
        let err = pick_workspace_root(&["ssh://devbox.example.com/workspace".to_string()])
            .expect_err("ssh-only local roots should remain invalid");

        assert!(err.contains("requires at least one local workspace root"));
    }

    #[test]
    fn codex_cost_hints_do_not_guess_model_metadata_values() {
        let values = codex_cost_hint_defaults(protocol::SpawnCostHint::Low);

        assert!(!values.0.contains_key("model"));
        assert!(!values.0.contains_key("reasoning_effort"));
    }

    impl CodexFakeAppServer {
        fn new(mode: &str, child_thread_id: &str) -> Self {
            let dir = tempfile::tempdir().expect("fake codex app-server tempdir");
            let binary = dir.path().join("codex-fake-app-server.py");
            let capture = dir.path().join("requests.jsonl");
            let argv_capture = dir.path().join("argv.json");
            let initial_turn_gate = dir.path().join("initial-turn-gate");
            let fork_response_marker = dir.path().join("fork-response-sent");
            let startup_settings_gate = dir.path().join("startup-settings-gate");
            let command_execution_marker = dir.path().join("command-executed");
            let native_mcp_contacts = dir.path().join("native-mcp-contacts.jsonl");
            let mut script = String::new();
            script.push_str("#!/usr/bin/env python3\n");
            script.push_str("import json, os, sys, threading, time\n");
            script.push_str(&format!(
                "CAPTURE = {}\n",
                serde_json::to_string(&capture.to_string_lossy()).expect("capture path JSON")
            ));
            script.push_str(&format!(
                "ARGV_CAPTURE = {}\n",
                serde_json::to_string(&argv_capture.to_string_lossy())
                    .expect("argv capture path JSON")
            ));
            script.push_str(&format!(
                "MODE = {}\n",
                serde_json::to_string(mode).expect("mode JSON")
            ));
            script.push_str(&format!(
                "CHILD_THREAD_ID = {}\n",
                serde_json::to_string(child_thread_id).expect("child thread id JSON")
            ));
            script.push_str(&format!(
                "INITIAL_TURN_GATE = {}\n",
                serde_json::to_string(&initial_turn_gate.to_string_lossy())
                    .expect("initial turn gate path JSON")
            ));
            script.push_str(&format!(
                "FORK_RESPONSE_MARKER = {}\n",
                serde_json::to_string(&fork_response_marker.to_string_lossy())
                    .expect("fork response marker path JSON")
            ));
            script.push_str(&format!(
                "STARTUP_SETTINGS_GATE = {}\n",
                serde_json::to_string(&startup_settings_gate.to_string_lossy())
                    .expect("startup settings gate path JSON")
            ));
            script.push_str(&format!(
                "COMMAND_EXECUTION_MARKER = {}\n",
                serde_json::to_string(&command_execution_marker.to_string_lossy())
                    .expect("command execution marker path JSON")
            ));
            script.push_str(&format!(
                "NATIVE_MCP_CONTACTS = {}\n",
                serde_json::to_string(&native_mcp_contacts.to_string_lossy())
                    .expect("native MCP contacts path JSON")
            ));
            script.push_str(
                r#"
def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

codex_home = os.environ.get("CODEX_HOME")
auth_present = bool(codex_home) and os.path.isfile(os.path.join(codex_home, "auth.json"))
native_mcp_configured = False
if codex_home:
    try:
        with open(os.path.join(codex_home, "config.toml"), "r", encoding="utf-8") as native_config:
            native_mcp_configured = "[mcp_servers.native-fixture]" in native_config.read()
    except FileNotFoundError:
        pass
if native_mcp_configured:
    with open(NATIVE_MCP_CONTACTS, "a", encoding="utf-8") as contacts:
        contacts.write(json.dumps({"pid": os.getpid()}, separators=(",", ":")) + "\n")
with open(ARGV_CAPTURE, "a", encoding="utf-8") as argv_capture:
    argv_capture.write(json.dumps({"pid": os.getpid(), "argv": sys.argv[1:], "codex_home": codex_home, "auth_present": auth_present, "native_mcp_configured": native_mcp_configured}, separators=(",", ":")) + "\n")

turn_count = 0
inference_only = "features.shell_tool=false" in sys.argv
for line in sys.stdin:
    try:
        request = json.loads(line)
    except Exception:
        continue
    with open(CAPTURE, "a", encoding="utf-8") as capture:
        capture.write(json.dumps({"pid": os.getpid(), "request": request}, separators=(",", ":")) + "\n")
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params") or {}
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "userAgent": "fake-codex/0",
                "codexHome": "/tmp/fake-codex-home",
                "platformFamily": "unix",
                "platformOs": "test"
            }
        })
    elif method == "thread/start":
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "thread": {
                    "id": "fresh-thread-id",
                    "sessionId": "fresh-thread-id",
                    "turns": []
                },
                "model": "fake-codex-model"
            }
        })
    elif method == "thread/fork":
        if MODE == "unsupported":
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32601,
                    "message": "Method not found: thread/fork"
                }
            })
        else:
            if MODE == "fork_startup_delayed":
                while not os.path.exists(INITIAL_TURN_GATE):
                    time.sleep(0.005)
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "thread": {
                        "id": CHILD_THREAD_ID,
                        "sessionId": CHILD_THREAD_ID,
                        "forkedFromId": params.get("threadId"),
                        "turns": []
                    },
                    "model": "fake-codex-model"
                }
            })
            if MODE == "fork_startup_delayed":
                with open(FORK_RESPONSE_MARKER, "w", encoding="utf-8") as marker:
                    marker.write("sent")
    elif method == "turn/start":
        turn_count += 1
        if MODE == "inference_reject_command" and inference_only:
            send({
                "jsonrpc": "2.0",
                "id": 900,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "threadId": "fresh-thread-id",
                    "turnId": "turn-inference",
                    "itemId": "naming-command",
                    "command": "touch " + COMMAND_EXECUTION_MARKER
                }
            })
        elif MODE == "inference_reject_command":
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-agent"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"fresh-thread-id","item":{"id":"message-agent","type":"agentMessage","text":"real agent complete"}}})
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-agent","status":"completed"}}})
        elif MODE == "fresh_agent_control_progress":
            send({
                "jsonrpc": "2.0",
                "method": "turn/started",
                "params": {
                    "threadId": "fresh-thread-id",
                    "turn": {
                        "id": "turn-fresh"
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/started",
                "params": {
                    "threadId": "fresh-thread-id",
                    "item": {
                        "id": "await-call-1",
                        "type": "mcpToolCall",
                        "tool": "mcp__tyde-agent-await__tyde_await_agents",
                        "arguments": {
                            "agent_ids": ["agent-a", "agent-b"]
                        }
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/completed",
                "params": {
                    "threadId": "fresh-thread-id",
                    "item": {
                        "id": "await-call-1",
                        "type": "mcpToolCall",
                        "tool": "mcp__tyde-agent-await__tyde_await_agents",
                        "status": "completed"
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/started",
                "params": {
                    "threadId": "fresh-thread-id",
                    "item": {
                        "id": "spawn-call-1",
                        "type": "mcpToolCall",
                        "tool": "mcp__tyde-agent-control__tyde_spawn_agent",
                        "arguments": {
                            "name": "Builder"
                        }
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/completed",
                "params": {
                    "threadId": "fresh-thread-id",
                    "item": {
                        "id": "spawn-call-1",
                        "type": "mcpToolCall",
                        "tool": "mcp__tyde-agent-control__tyde_spawn_agent",
                        "status": "completed",
                        "output": "{\"agent_id\":\"agent-spawned\",\"name\":\"Builder\"}"
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {
                    "threadId": "fresh-thread-id",
                    "turn": {
                        "id": "turn-fresh",
                        "status": "completed"
                    }
                }
            })
        elif MODE == "fresh_late_token_usage":
            send({
                "jsonrpc": "2.0",
                "method": "turn/started",
                "params": {
                    "threadId": "fresh-thread-id",
                    "turn": {
                        "id": "turn-fresh-late-usage"
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": "fresh-thread-id",
                    "itemId": "msg-fresh-late-usage",
                    "delta": "fresh late done"
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/completed",
                "params": {
                    "threadId": "fresh-thread-id",
                    "item": {
                        "id": "msg-fresh-late-usage",
                        "type": "agentMessage",
                        "text": "fresh late done"
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {
                    "threadId": "fresh-thread-id",
                    "turn": {
                        "id": "turn-fresh-late-usage",
                        "status": "completed"
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "thread/tokenUsage/updated",
                "params": {
                    "threadId": "fresh-thread-id",
                    "turnId": "turn-fresh-late-usage",
                    "tokenUsage": {
                        "input_tokens": 31,
                        "output_tokens": 10,
                        "total_tokens": 41
                    }
                }
            })
        elif MODE == "fresh_native_child_before_initial_response":
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-parent-live-order"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"019f60f0-7a69-73f0-9ab3-7ddc24062e30","type":"collabAgentToolCall","tool":"spawn","senderThreadId":"fresh-thread-id","receiverThreadId":CHILD_THREAD_ID,"prompt":"reply exactly QUICK_DONE","receiverAgentType":"sub-agent","receiverAgentName":"/root/quick_child"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"activity-quick-child","type":"sub_agent_activity","kind":"started","agent_thread_id":CHILD_THREAD_ID,"agent_path":"/root/quick_child"}}})
            def delayed_initial_turn_response(response_id):
                while not os.path.exists(INITIAL_TURN_GATE):
                    time.sleep(0.005)
                send({"jsonrpc":"2.0","id":response_id,"result":{"turn":{"id":"turn-fake"}}})
            threading.Thread(target=delayed_initial_turn_response, args=(request_id,), daemon=True).start()
            continue
        elif MODE == "fresh_native_child_routing":
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-parent"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"spawn-call","type":"collabAgentToolCall","tool":"spawn","senderThreadId":"fresh-thread-id","receiverThreadId":CHILD_THREAD_ID,"prompt":"inspect ownership","receiverAgentType":"worker","receiverAgentName":"Worker"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"activity-child","type":"subAgentActivity","kind":"started","agentThreadId":CHILD_THREAD_ID,"agentPath":"/root/worker"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"fresh-thread-id","item":{"id":"activity-child","type":"subAgentActivity","kind":"started","agentThreadId":CHILD_THREAD_ID,"agentPath":"/root/worker"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"wait-call","type":"collabAgentToolCall","tool":"wait","senderThreadId":"fresh-thread-id","receiverThreadId":CHILD_THREAD_ID}}})
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":CHILD_THREAD_ID,"turn":{"id":"turn-child"}}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":CHILD_THREAD_ID,"itemId":"message-child","delta":"child-only"}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":CHILD_THREAD_ID,"item":{"id":"message-child","type":"agentMessage","text":"child-only"}}})
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":CHILD_THREAD_ID,"turn":{"id":"turn-child","status":"interrupted"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"fresh-thread-id","item":{"id":"wait-call","type":"collabAgentToolCall","tool":"wait","senderThreadId":"fresh-thread-id","receiverThreadId":CHILD_THREAD_ID,"agentsStates":{CHILD_THREAD_ID:{"status":"cancelled"}}}}})
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-parent","status":"completed"}}})
        elif MODE == "fresh_native_multi_child_routing":
            child_a, child_b = "child-a", "child-b"
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-parent-multi"}}})
            for child, label in ((child_a, "alpha"), (child_b, "beta")):
                send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"spawn-"+label,"type":"collabAgentToolCall","senderThreadId":"fresh-thread-id","receiverThreadId":child,"prompt":"inspect "+label,"receiverAgentType":"worker","receiverAgentName":label}}})
                send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"activity-"+label,"type":"sub_agent_activity","kind":"started","agent_thread_id":child,"agent_path":"/root/"+label}}})
                send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"fresh-thread-id","item":{"id":"wait-"+label,"type":"collabAgentToolCall","tool":"wait","senderThreadId":"fresh-thread-id","receiverThreadId":child}}})
            for child, label, status in ((child_b, "beta", "interrupted"), (child_a, "alpha", "completed")):
                send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":child,"turn":{"id":"turn-"+label}}})
                send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":child,"itemId":"message-"+label,"delta":label+"-only"}})
                send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":child,"item":{"id":"message-"+label,"type":"agentMessage","text":label+"-only"}}})
                send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":child,"turn":{"id":"turn-"+label,"status":status}}})
                send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"fresh-thread-id","item":{"id":"wait-"+label,"type":"collabAgentToolCall","tool":"wait","senderThreadId":"fresh-thread-id","receiverThreadId":child,"agentsStates":{child:{"status":status}}}}})
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-parent-multi","status":"completed"}}})
        elif MODE in ("runtime_settings", "reject_runtime_settings"):
            turn_id = "turn-settings-" + str(turn_count)
            message_id = "message-settings-" + str(turn_count)
            send({
                "jsonrpc": "2.0",
                "method": "turn/started",
                "params": {"threadId": "fresh-thread-id", "turn": {"id": turn_id}}
            })
            send({
                "jsonrpc": "2.0",
                "method": "item/completed",
                "params": {
                    "threadId": "fresh-thread-id",
                    "item": {
                        "id": message_id,
                        "type": "agentMessage",
                        "text": "settings turn " + str(turn_count)
                    }
                }
            })
            send({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {"threadId": "fresh-thread-id", "turn": {"id": turn_id, "status": "completed"}}
            })
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"turn": {"id": "turn-fake"}}
        })
    elif method == "turn/interrupt":
        if MODE == "fresh_native_child_before_initial_response":
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"fresh-thread-id","turn":{"id":"turn-parent-live-order","status":"interrupted"}}})
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif method == "thread/update":
        if MODE == "startup_settings_delayed":
            def delayed_startup_settings_response(response_id):
                while not os.path.exists(STARTUP_SETTINGS_GATE):
                    time.sleep(0.005)
                send({"jsonrpc":"2.0","id":response_id,"result":{}})
            threading.Thread(target=delayed_startup_settings_response, args=(request_id,), daemon=True).start()
            continue
        elif MODE == "startup_settings_rejected" or (
            MODE == "reject_runtime_settings"
            and "approval_policy" not in params.get("settings", {})
        ):
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "settings rejected"}
            })
        else:
            send({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif method is None and request_id == 900 and MODE == "inference_reject_command":
        if (request.get("result") or {}).get("decision") != "decline":
            with open(COMMAND_EXECUTION_MARKER, "w", encoding="utf-8") as marker:
                marker.write("executed")
    else:
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {
                "code": -32601,
                "message": "Method not found: " + str(method)
            }
        })
"#,
            );
            std::fs::write(&binary, script).expect("write fake codex app-server");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&binary)
                    .expect("fake codex app-server metadata")
                    .permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&binary, permissions)
                    .expect("chmod fake codex app-server");
            }
            Self {
                _dir: dir,
                binary,
                capture,
                argv_capture,
                initial_turn_gate,
                fork_response_marker,
                startup_settings_gate,
                command_execution_marker,
                native_mcp_contacts,
            }
        }

        fn release_initial_turn(&self) {
            std::fs::write(&self.initial_turn_gate, "release").expect("release fake initial turn");
        }

        fn release_startup_settings(&self) {
            std::fs::write(&self.startup_settings_gate, "release")
                .expect("release fake startup settings");
        }

        fn requests(&self) -> Vec<Value> {
            self.captured_requests()
                .into_iter()
                .map(|captured| captured.request)
                .collect()
        }

        fn captured_requests(&self) -> Vec<CapturedCodexRequest> {
            let contents = std::fs::read_to_string(&self.capture).unwrap_or_default();
            contents
                .lines()
                .map(|line| {
                    let value: Value = serde_json::from_str(line).expect("captured request JSON");
                    match value.get("request") {
                        Some(request) => CapturedCodexRequest {
                            pid: value.get("pid").and_then(Value::as_u64).unwrap_or_default(),
                            request: request.clone(),
                        },
                        None => CapturedCodexRequest {
                            pid: 0,
                            request: value,
                        },
                    }
                })
                .collect()
        }

        fn captured_fork_process(&self, parent_thread_id: &str) -> (u64, Vec<Value>) {
            let captured = self.captured_requests();
            let fork_pid = captured
                .iter()
                .find(|captured| {
                    captured.request.get("method").and_then(Value::as_str) == Some("thread/fork")
                        && captured
                            .request
                            .pointer("/params/threadId")
                            .and_then(Value::as_str)
                            == Some(parent_thread_id)
                })
                .map(|captured| captured.pid)
                .expect("captured thread/fork request");
            let requests = captured
                .into_iter()
                .filter(|captured| captured.pid == fork_pid)
                .map(|captured| captured.request)
                .collect::<Vec<_>>();
            (fork_pid, requests)
        }

        fn captured_argv(&self) -> Vec<CapturedCodexArgv> {
            let contents = std::fs::read_to_string(&self.argv_capture).unwrap_or_default();
            contents
                .lines()
                .map(|line| {
                    let value: Value =
                        serde_json::from_str(line).expect("fake app-server argv JSON");
                    match value.get("argv") {
                        Some(argv) => CapturedCodexArgv {
                            pid: value.get("pid").and_then(Value::as_u64).unwrap_or_default(),
                            argv: serde_json::from_value(argv.clone())
                                .expect("fake app-server argv array"),
                            codex_home: value
                                .get("codex_home")
                                .and_then(Value::as_str)
                                .map(std::path::PathBuf::from),
                            auth_present: value
                                .get("auth_present")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            native_mcp_configured: value
                                .get("native_mcp_configured")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        },
                        None => CapturedCodexArgv {
                            pid: 0,
                            argv: serde_json::from_value(value)
                                .expect("fake app-server argv array"),
                            codex_home: None,
                            auth_present: false,
                            native_mcp_configured: false,
                        },
                    }
                })
                .collect()
        }

        fn argv_for_pid(&self, pid: u64) -> Vec<String> {
            self.captured_argv()
                .into_iter()
                .find(|captured| captured.pid == pid)
                .map(|captured| captured.argv)
                .expect("fake app-server argv for pid")
        }

        fn environment_for_pid(&self, pid: u64) -> CapturedCodexArgv {
            self.captured_argv()
                .into_iter()
                .find(|captured| captured.pid == pid)
                .expect("fake app-server environment for pid")
        }

        fn native_mcp_contact_pids(&self) -> Vec<u64> {
            std::fs::read_to_string(&self.native_mcp_contacts)
                .unwrap_or_default()
                .lines()
                .map(|line| {
                    serde_json::from_str::<Value>(line)
                        .expect("native MCP contact JSON")
                        .get("pid")
                        .and_then(Value::as_u64)
                        .expect("native MCP contact pid")
                })
                .collect()
        }
    }

    fn codex_steering_tempfiles() -> HashSet<std::path::PathBuf> {
        std::fs::read_dir(std::env::temp_dir())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("tyde-codex-steering-") && name.ends_with(".md")
                    })
            })
            .collect()
    }

    const RUN_REAL_AI_TESTS_ENV: &str = "TYDE_RUN_REAL_AI_TESTS";
    const LIVE_CODEX_TEST_ENV: &str = "TYDE_LIVE_CODEX_TEST";

    fn live_codex_tests_enabled() -> bool {
        std::env::var(RUN_REAL_AI_TESTS_ENV).ok().as_deref() == Some("1")
            || std::env::var(LIVE_CODEX_TEST_ENV).ok().as_deref() == Some("1")
    }

    fn live_test_verbose() -> bool {
        std::env::var("TYDE_LIVE_CODEX_TEST_VERBOSE")
            .ok()
            .as_deref()
            == Some("1")
    }

    fn live_test_log(msg: &str) {
        eprintln!("[live-codex-test] {msg}");
    }

    fn skip_live_codex_test() {
        eprintln!(
            "Skipping live Codex test (set {RUN_REAL_AI_TESTS_ENV}=1 or {LIVE_CODEX_TEST_ENV}=1 to run)."
        );
    }

    fn test_file_change(path: &str, lines_added: u64, lines_removed: u64) -> CodexFileChange {
        CodexFileChange {
            path: path.to_string(),
            before: "before".to_string(),
            after: "after".to_string(),
            lines_added,
            lines_removed,
        }
    }

    #[test]
    fn file_change_completion_plan_completes_missing_started_paths() {
        let known_call_ids = vec![
            "change-1#1".to_string(),
            "change-1#2".to_string(),
            "change-1#3".to_string(),
            "change-1#4".to_string(),
        ];
        let file_changes = vec![
            test_file_change("src/a.rs", 4, 1),
            test_file_change("src/b.rs", 2, 0),
        ];

        let completions =
            codex_file_change_completion_plan("change-1", &known_call_ids, &file_changes);

        assert_eq!(completions.len(), 4);
        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["change-1#1", "change-1#2", "change-1#3", "change-1#4"]
        );
        assert_eq!(
            completions
                .iter()
                .map(|completion| (completion.lines_added, completion.lines_removed))
                .collect::<Vec<_>>(),
            vec![(4, 1), (2, 0), (0, 0), (0, 0)]
        );
        assert!(
            completions
                .iter()
                .all(|completion| completion.request.is_none()),
            "known request ids should not emit duplicate requests"
        );
    }

    #[test]
    fn file_change_completion_plan_completes_known_ids_without_changes() {
        let known_call_ids = vec!["change-2#1".to_string(), "change-2#2".to_string()];

        let completions = codex_file_change_completion_plan("change-2", &known_call_ids, &[]);

        assert_eq!(
            completions
                .iter()
                .map(|completion| (completion.call_id.as_str(), completion.lines_added))
                .collect::<Vec<_>>(),
            vec![("change-2#1", 0), ("change-2#2", 0)]
        );
    }

    #[test]
    fn file_change_completion_plan_requests_new_completed_changes() {
        let file_changes = vec![
            test_file_change("src/a.rs", 1, 0),
            test_file_change("src/b.rs", 0, 3),
        ];

        let completions = codex_file_change_completion_plan("change-3", &[], &file_changes);

        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["change-3#1", "change-3#2"]
        );
        assert_eq!(
            completions
                .iter()
                .filter_map(|completion| completion.request.as_ref())
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/b.rs"]
        );
    }

    #[test]
    fn codex_model_options_use_canonical_labels_and_version_sort() {
        let raw_models = vec![
            json!({
                "id": "gpt-5.4",
                "model": "gpt-5.4",
                "displayName": "gpt-5.4",
            }),
            json!({
                "id": "gpt-5.5",
                "model": "gpt-5.5",
                "displayName": "GPT-5.5",
            }),
            json!({
                "id": "gpt-5.4-mini",
                "model": "gpt-5.4-mini",
                "displayName": "GPT-5.4-Mini",
            }),
            json!({
                "id": "gpt-5.3-codex",
                "model": "gpt-5.3-codex",
                "displayName": "gpt-5.3-codex",
            }),
            json!({
                "id": "gpt-5.3-codex-spark",
                "model": "gpt-5.3-codex-spark",
                "displayName": "GPT-5.3-Codex-Spark",
            }),
            json!({
                "id": "gpt-5.2",
                "model": "gpt-5.2",
                "displayName": "gpt-5.2",
            }),
        ];

        let options = codex_model_metadata_from_raw(&raw_models)
            .into_iter()
            .map(|model| model.option)
            .collect::<Vec<_>>();

        assert_eq!(
            options,
            vec![
                protocol::SelectOption {
                    value: "gpt-5.5".to_string(),
                    label: "gpt-5.5".to_string(),
                },
                protocol::SelectOption {
                    value: "gpt-5.4".to_string(),
                    label: "gpt-5.4".to_string(),
                },
                protocol::SelectOption {
                    value: "gpt-5.4-mini".to_string(),
                    label: "gpt-5.4-mini".to_string(),
                },
                protocol::SelectOption {
                    value: "gpt-5.3-codex".to_string(),
                    label: "gpt-5.3-codex".to_string(),
                },
                protocol::SelectOption {
                    value: "gpt-5.3-codex-spark".to_string(),
                    label: "gpt-5.3-codex-spark".to_string(),
                },
                protocol::SelectOption {
                    value: "gpt-5.2".to_string(),
                    label: "gpt-5.2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn codex_model_version_sort_handles_multi_digit_components() {
        let raw_models = vec![
            json!({ "model": "gpt-5.9" }),
            json!({ "model": "gpt-5.10" }),
            json!({ "model": "gpt-5.10-mini" }),
            json!({ "model": "gpt-5" }),
        ];

        let values = codex_model_metadata_from_raw(&raw_models)
            .into_iter()
            .map(|model| model.option.value)
            .collect::<Vec<_>>();

        assert_eq!(
            values,
            vec!["gpt-5.10", "gpt-5.10-mini", "gpt-5.9", "gpt-5"]
        );
    }

    #[test]
    fn codex_auto_model_remains_unset_in_dynamic_schema_and_spawn_settings() {
        let schema = codex_session_settings_schema(codex_model_metadata_from_raw(&[json!({
            "model": "gpt-5.6",
            "isDefault": true,
            "supportedReasoningEfforts": [
                { "reasoningEffort": "low" },
                { "reasoningEffort": "max" }
            ]
        })]));
        let model_field = schema
            .fields
            .iter()
            .find(|field| field.key == "model")
            .expect("Codex model field");
        let SessionSettingFieldType::Select {
            default, nullable, ..
        } = &model_field.field_type
        else {
            panic!("Codex model field should be a select");
        };
        assert_eq!(default, &None);
        assert!(*nullable);

        let resolved = resolve_session_settings(&BackendSpawnConfig::default());
        assert!(
            !resolved.0.contains_key("model"),
            "Auto must omit the model override so Codex selects its effective model"
        );
        assert!(
            !resolved.0.contains_key("reasoning_effort"),
            "Auto must not force a reasoning level over Codex's model default"
        );
    }

    #[test]
    fn codex_reasoning_options_follow_each_models_metadata() {
        let schema = codex_session_settings_schema(codex_model_metadata_from_raw(&[
            json!({
                "model": "gpt-5.6",
                "isDefault": true,
                "supportedReasoningEfforts": [
                    { "reasoningEffort": "low" },
                    { "reasoningEffort": "xhigh" },
                    { "reasoningEffort": "max" },
                    { "reasoningEffort": "ultra" }
                ]
            }),
            json!({
                "model": "gpt-5.5",
                "supportedReasoningEfforts": [
                    { "reasoningEffort": "low" },
                    { "reasoningEffort": "high" }
                ]
            }),
        ]));
        let reasoning_field = schema
            .fields
            .iter()
            .find(|field| field.key == "reasoning_effort")
            .expect("Codex reasoning field");

        let mut values = protocol::SessionSettingsValues::default();
        assert_eq!(
            reasoning_field
                .select_options(&values)
                .expect("default model reasoning options")
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "xhigh", "max", "ultra"]
        );

        values.0.insert(
            "model".to_string(),
            SessionSettingValue::String("gpt-5.5".to_string()),
        );
        assert_eq!(
            reasoning_field
                .select_options(&values)
                .expect("selected model reasoning options")
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "high"]
        );

        let tiers =
            codex_tier_config_from_schema(&schema, &protocol::SessionSettingsValues::default())
                .expect("Codex tiers should resolve from default model metadata");
        assert_eq!(
            tiers.low.0.get("reasoning_effort"),
            Some(&SessionSettingValue::String("low".to_owned()))
        );
        assert_eq!(
            tiers.high.0.get("reasoning_effort"),
            Some(&SessionSettingValue::String("ultra".to_owned()))
        );
        assert!(!tiers.low.0.contains_key("model"));
        assert!(!tiers.high.0.contains_key("model"));

        let selected_model_tiers = codex_tier_config_from_schema(&schema, &values)
            .expect("Codex tiers should follow the selected model metadata");
        assert_eq!(
            selected_model_tiers.high.0.get("reasoning_effort"),
            Some(&SessionSettingValue::String("high".to_owned()))
        );

        values.0.insert(
            "reasoning_effort".to_string(),
            SessionSettingValue::String("max".to_string()),
        );
        let error = crate::backend::validate_session_settings_values(&schema, &values)
            .expect_err("unsupported model/effort pair must be rejected");
        assert!(error.contains("reasoning_effort"));
        assert!(error.contains("max"));
    }

    #[test]
    fn codex_reasoning_normalization_preserves_max() {
        assert_eq!(normalize_reasoning_effort("max").as_deref(), Some("max"));
        assert_eq!(
            normalize_reasoning_effort("xhigh").as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn codex_model_probe_surfaces_cleanup_failure_after_success() {
        let error = codex_probe_result_with_cleanup(
            Ok::<_, String>(()),
            Err("timed out reaping test app-server".to_string()),
        )
        .expect_err("cleanup failure must fail model discovery");

        assert!(error.contains("Codex model discovery app-server cleanup failed"));
        assert!(error.contains("timed out reaping test app-server"));
    }

    #[test]
    fn codex_model_probe_preserves_operation_and_cleanup_failures() {
        let error = codex_probe_result_with_cleanup::<()>(
            Err("model/list failed".to_string()),
            Err("kill failed".to_string()),
        )
        .expect_err("both failures must remain visible");

        assert!(error.contains("model/list failed"));
        assert!(error.contains("Codex app-server cleanup also failed: kill failed"));
    }

    #[tokio::test]
    async fn codex_child_wait_timeout_is_bounded_and_explicit() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_codex_child_exit(
                std::future::pending::<std::io::Result<std::process::ExitStatus>>(),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("bounded child wait must return")
        .expect_err("pending child wait must time out");

        assert!(result.contains("timed out after 0.01s"));
        assert!(result.contains("reaping Codex app-server process group"));
    }

    async fn wait_for_codex_test_stream_end(events: &mut EventStream) -> protocol::StreamEndData {
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if let ChatEvent::StreamEnd(data) = event {
                    return data;
                }
            }
            panic!("fake Codex event stream ended before StreamEnd");
        })
        .await
        .expect("fake Codex StreamEnd timeout")
    }

    async fn wait_for_codex_test_error(events: &mut EventStream) -> String {
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.recv().await {
                if let ChatEvent::MessageAdded(message) = event
                    && matches!(message.sender, MessageSender::Error)
                {
                    return message.content;
                }
            }
            panic!("fake Codex event stream ended before typed error");
        })
        .await
        .expect("fake Codex typed error timeout")
    }

    #[tokio::test]
    async fn codex_naming_mode_isolates_config_and_rejects_commands() {
        let fake = CodexFakeAppServer::new("inference_reject_command", "unused");
        let native_home = tempfile::tempdir().expect("native Codex home fixture");
        std::fs::write(
            native_home.path().join("config.toml"),
            "[mcp_servers.native-fixture]\nurl = \"https://native.invalid/mcp\"\n",
        )
        .expect("write native Codex MCP fixture");
        std::fs::write(
            native_home.path().join("auth.json"),
            r#"{"OPENAI_API_KEY":"fixture-only","tokens":null}"#,
        )
        .expect("write native Codex auth fixture");
        let _guard = CodexTestAppServerBinaryGuard::set_with_native_home(
            fake.binary.clone(),
            Some(native_home.path().to_path_buf()),
        );
        let isolated_workspace = tempfile::tempdir().expect("isolated naming workspace");
        let isolated_root = isolated_workspace.path().to_string_lossy().to_string();
        let configured_mcp = StartupMcpServer {
            name: "method-not-allowed".to_owned(),
            transport: StartupMcpTransport::Http {
                url: "https://example.com/mcp".to_owned(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        };
        let mut configured_settings = protocol::SessionSettingsValues::default();
        configured_settings.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("configured-model".to_owned()),
        );
        configured_settings.0.insert(
            "reasoning_effort".to_owned(),
            SessionSettingValue::String("high".to_owned()),
        );
        let task = "Investigate the configured MCP endpoint";
        let naming_prompt = crate::agent::build_name_generation_prompt(task);
        let mut naming_config = crate::agent::agent_name_generation_spawn_config();
        naming_config.startup_mcp_servers = vec![configured_mcp.clone()];
        naming_config.session_settings = Some(configured_settings.clone());
        naming_config.resolved_spawn_config.instructions =
            Some("This configured agent instruction must not enter naming.".to_owned());

        let (naming_backend, mut naming_events) = <CodexBackend as Backend>::spawn(
            vec![isolated_root.clone()],
            naming_config,
            protocol::SendMessagePayload {
                message: naming_prompt.clone(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn production Codex naming backend against fake app-server");
        let naming_error = wait_for_codex_test_error(&mut naming_events).await;
        assert_eq!(
            naming_error,
            "Codex transient inference rejected tool request 'item/commandExecution/requestApproval'"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        naming_backend.shutdown().await;

        let naming_capture = fake.captured_requests();
        let naming_pid = naming_capture
            .iter()
            .find(|captured| {
                captured
                    .request
                    .pointer("/params/input/0/text")
                    .and_then(Value::as_str)
                    == Some(naming_prompt.as_str())
            })
            .map(|captured| captured.pid)
            .expect("captured naming turn");
        let naming_requests = naming_capture
            .iter()
            .filter(|captured| captured.pid == naming_pid)
            .map(|captured| &captured.request)
            .collect::<Vec<_>>();
        let naming_thread = naming_requests
            .iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some("thread/start"))
            .expect("naming thread/start");
        assert_eq!(
            naming_thread.pointer("/params/cwd").and_then(Value::as_str),
            Some(isolated_root.as_str())
        );
        assert_eq!(
            naming_thread
                .pointer("/params/sandbox")
                .and_then(Value::as_str),
            Some(CODEX_INFERENCE_SANDBOX)
        );
        assert_eq!(
            naming_thread
                .pointer("/params/approvalPolicy")
                .and_then(Value::as_str),
            Some(CODEX_INFERENCE_APPROVAL_POLICY)
        );
        assert_eq!(
            naming_thread
                .pointer("/params/ephemeral")
                .and_then(Value::as_bool),
            Some(true)
        );
        let naming_turn = naming_requests
            .iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            .expect("naming turn/start");
        assert_eq!(
            naming_turn
                .pointer("/params/input/0/text")
                .and_then(Value::as_str),
            Some(naming_prompt.as_str())
        );
        assert_eq!(naming_turn.pointer("/params/model"), None);
        assert_eq!(naming_turn.pointer("/params/effort"), None);
        assert_eq!(
            naming_turn
                .pointer("/params/sandboxPolicy/type")
                .and_then(Value::as_str),
            Some("readOnly")
        );
        assert_eq!(
            naming_turn
                .pointer("/params/sandboxPolicy/networkAccess")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            naming_turn
                .pointer("/params/approvalPolicy")
                .and_then(Value::as_str),
            Some(CODEX_INFERENCE_APPROVAL_POLICY)
        );
        let rejection = naming_requests
            .iter()
            .find(|request| request.get("id").and_then(Value::as_u64) == Some(900))
            .expect("captured naming command rejection");
        assert_eq!(
            rejection
                .pointer("/result/decision")
                .and_then(Value::as_str),
            Some("decline")
        );
        let naming_argv = fake.argv_for_pid(naming_pid);
        for disabled in codex_inference_config_overrides() {
            assert!(naming_argv.contains(&disabled), "missing {disabled}");
        }
        assert!(
            !naming_argv
                .iter()
                .any(|arg| arg.starts_with("mcp_servers."))
        );
        assert!(
            !naming_argv
                .iter()
                .any(|arg| arg.starts_with("model_instructions_file="))
        );
        let naming_environment = fake.environment_for_pid(naming_pid);
        assert_ne!(
            naming_environment.codex_home.as_deref(),
            Some(native_home.path())
        );
        assert!(naming_environment.auth_present);
        assert!(!naming_environment.native_mcp_configured);
        assert!(!fake.native_mcp_contact_pids().contains(&naming_pid));
        assert!(!fake.command_execution_marker.exists());

        let real_workspace = tempfile::tempdir().expect("real agent workspace");
        let real_root = real_workspace.path().to_string_lossy().to_string();
        let real_task = "Perform the full configured agent task";
        let real_config = BackendSpawnConfig {
            execution_mode: BackendExecutionMode::Agent,
            startup_mcp_servers: vec![configured_mcp],
            session_settings: Some(configured_settings),
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                instructions: Some("Retain the real agent instruction.".to_owned()),
                access_mode: BackendAccessMode::Unrestricted,
                ..Default::default()
            },
            ..Default::default()
        };
        let (real_backend, mut real_events) = <CodexBackend as Backend>::spawn(
            vec![real_root.clone()],
            real_config,
            protocol::SendMessagePayload {
                message: real_task.to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn production real-agent Codex backend against fake app-server");
        let _ = wait_for_codex_test_stream_end(&mut real_events).await;
        real_backend.shutdown().await;

        let real_capture = fake.captured_requests();
        let real_pid = real_capture
            .iter()
            .find(|captured| {
                captured
                    .request
                    .pointer("/params/input/0/text")
                    .and_then(Value::as_str)
                    == Some(real_task)
            })
            .map(|captured| captured.pid)
            .expect("captured real agent turn");
        let real_requests = real_capture
            .iter()
            .filter(|captured| captured.pid == real_pid)
            .map(|captured| &captured.request)
            .collect::<Vec<_>>();
        let real_thread = real_requests
            .iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some("thread/start"))
            .expect("real agent thread/start");
        assert_eq!(
            real_thread.pointer("/params/cwd").and_then(Value::as_str),
            Some(real_root.as_str())
        );
        assert_eq!(
            real_thread
                .pointer("/params/ephemeral")
                .and_then(Value::as_bool),
            Some(false)
        );
        let real_turn = real_requests
            .iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            .expect("real agent turn/start");
        assert_eq!(
            real_turn
                .pointer("/params/input/0/text")
                .and_then(Value::as_str),
            Some(real_task)
        );
        assert_eq!(
            real_turn.pointer("/params/model").and_then(Value::as_str),
            Some("configured-model")
        );
        assert_eq!(
            real_turn.pointer("/params/effort").and_then(Value::as_str),
            Some("high")
        );
        let real_argv = fake.argv_for_pid(real_pid);
        assert!(real_argv.iter().any(|arg| {
            arg == "mcp_servers.method-not-allowed.url=\"https://example.com/mcp\""
        }));
        assert!(
            real_argv
                .iter()
                .any(|arg| arg.starts_with("model_instructions_file="))
        );
        assert!(!real_argv.contains(&"features.shell_tool=false".to_owned()));
        let real_environment = fake.environment_for_pid(real_pid);
        assert_eq!(
            real_environment.codex_home.as_deref(),
            Some(native_home.path())
        );
        assert!(real_environment.auth_present);
        assert!(real_environment.native_mcp_configured);
        assert!(fake.native_mcp_contact_pids().contains(&real_pid));
    }

    #[tokio::test]
    async fn codex_session_runtime_settings_acknowledge_provider_before_followup() {
        let fake = CodexFakeAppServer::new("runtime_settings", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let workspace_roots = vec![workspace.path().to_string_lossy().to_string()];
        let (session, _events) = CodexSession::spawn(
            &workspace_roots,
            None,
            &[],
            None,
            BackendAccessMode::ReadOnly,
        )
        .await
        .expect("spawn fake Codex session");
        let handle = session.command_handle();

        handle
            .update_runtime_settings(json!({
                "model": "gpt-updated",
                "reasoning_effort": "max",
            }))
            .await
            .expect("provider should accept settings update");
        handle
            .execute(SessionCommand::SendMessage {
                message: "followup".to_owned(),
                images: None,
            })
            .await
            .expect("send followup after settings acknowledgement");
        session.shutdown().await;

        let requests = fake.requests();
        let update_index = requests
            .iter()
            .position(|request| {
                request.get("method").and_then(Value::as_str) == Some("thread/update")
            })
            .expect("thread/update request");
        let turn_index = requests
            .iter()
            .position(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            .expect("turn/start request");
        assert!(update_index < turn_index);
        assert_eq!(
            requests[update_index]
                .pointer("/params/settings/reasoning_effort")
                .and_then(Value::as_str),
            Some("max")
        );
        assert_eq!(
            requests[turn_index]
                .pointer("/params/model")
                .and_then(Value::as_str),
            Some("gpt-updated")
        );
        assert_eq!(
            requests[turn_index]
                .pointer("/params/effort")
                .and_then(Value::as_str),
            Some("max")
        );
    }

    #[tokio::test]
    async fn codex_session_rejected_runtime_settings_preserve_live_state() {
        let fake = CodexFakeAppServer::new("reject_runtime_settings", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let workspace_roots = vec![workspace.path().to_string_lossy().to_string()];
        let (session, _events) = CodexSession::spawn(
            &workspace_roots,
            None,
            &[],
            None,
            BackendAccessMode::ReadOnly,
        )
        .await
        .expect("spawn fake Codex session");
        let handle = session.command_handle();
        handle
            .execute(SessionCommand::UpdateSettings {
                settings: json!({
                    "model": "gpt-initial",
                    "reasoning_effort": "low",
                }),
                persist: false,
            })
            .await
            .expect("configure initial live settings");

        let error = handle
            .update_runtime_settings(json!({
                "model": "gpt-rejected",
                "reasoning_effort": "max",
            }))
            .await
            .expect_err("provider rejection must reach the caller");
        assert!(error.contains("settings rejected"));
        handle
            .execute(SessionCommand::SendMessage {
                message: "followup".to_owned(),
                images: None,
            })
            .await
            .expect("send followup after rejected settings");
        session.shutdown().await;

        let turn = fake
            .requests()
            .into_iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            .expect("turn/start request");
        assert_eq!(
            turn.pointer("/params/model").and_then(Value::as_str),
            Some("gpt-initial")
        );
        assert_eq!(
            turn.pointer("/params/effort").and_then(Value::as_str),
            Some("low")
        );
    }

    #[tokio::test]
    async fn codex_runtime_settings_update_changes_followup_turn_overrides() {
        let fake = CodexFakeAppServer::new("runtime_settings", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let (mut backend, mut events) = <CodexBackend as Backend>::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "initial".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Codex backend");
        let _ = wait_for_codex_test_stream_end(&mut events).await;

        let mut values = protocol::SessionSettingsValues::default();
        values.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("gpt-updated".to_owned()),
        );
        values.0.insert(
            "reasoning_effort".to_owned(),
            SessionSettingValue::String("max".to_owned()),
        );
        Backend::update_session_settings(
            &mut backend,
            protocol::SetSessionSettingsPayload { values },
        )
        .await
        .expect("provider should accept settings update");
        assert!(
            Backend::send(
                &backend,
                AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: "followup".to_owned(),
                    images: None,
                    origin: None,
                    tool_response: None,
                }),
            )
            .await
        );
        let followup_end = wait_for_codex_test_stream_end(&mut events).await;
        assert_eq!(
            followup_end
                .message
                .model_info
                .as_ref()
                .map(|model| model.model.as_str()),
            Some("gpt-updated")
        );

        let turn_requests = fake
            .requests()
            .into_iter()
            .filter(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            .collect::<Vec<_>>();
        let followup = turn_requests.last().expect("followup turn/start request");
        assert_eq!(
            followup.pointer("/params/model").and_then(Value::as_str),
            Some("gpt-updated")
        );
        assert_eq!(
            followup.pointer("/params/effort").and_then(Value::as_str),
            Some("max")
        );
    }

    #[tokio::test]
    async fn codex_rejected_runtime_settings_do_not_change_followup_turn() {
        let fake = CodexFakeAppServer::new("reject_runtime_settings", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let mut initial_values = protocol::SessionSettingsValues::default();
        initial_values.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("gpt-initial".to_owned()),
        );
        initial_values.0.insert(
            "reasoning_effort".to_owned(),
            SessionSettingValue::String("low".to_owned()),
        );
        let (mut backend, mut events) = <CodexBackend as Backend>::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                session_settings: Some(initial_values),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "initial".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("spawn fake Codex backend");
        let _ = wait_for_codex_test_stream_end(&mut events).await;

        let mut values = protocol::SessionSettingsValues::default();
        values.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("gpt-rejected".to_owned()),
        );
        values.0.insert(
            "reasoning_effort".to_owned(),
            SessionSettingValue::String("max".to_owned()),
        );
        let error = Backend::update_session_settings(
            &mut backend,
            protocol::SetSessionSettingsPayload { values },
        )
        .await
        .expect_err("provider rejection must reach the caller");
        assert!(error.contains("settings rejected"));

        assert!(
            Backend::send(
                &backend,
                AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: "followup".to_owned(),
                    images: None,
                    origin: None,
                    tool_response: None,
                }),
            )
            .await
        );
        let followup_end = wait_for_codex_test_stream_end(&mut events).await;
        assert_eq!(
            followup_end
                .message
                .model_info
                .as_ref()
                .map(|model| model.model.as_str()),
            Some("gpt-initial")
        );
        let turn_requests = fake
            .requests()
            .into_iter()
            .filter(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            .collect::<Vec<_>>();
        let followup = turn_requests.last().expect("followup turn/start request");
        assert_eq!(
            followup.pointer("/params/model").and_then(Value::as_str),
            Some("gpt-initial")
        );
        assert_eq!(
            followup.pointer("/params/effort").and_then(Value::as_str),
            Some("low")
        );
    }

    #[tokio::test]
    async fn codex_backend_fork_uses_thread_fork_child_id_and_child_turn() {
        let fake = CodexFakeAppServer::new("ok", "child-thread-id");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let workspace_root = workspace.path().to_string_lossy().to_string();
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            SessionSettingValue::String("gpt-test".to_string()),
        );
        settings.0.insert(
            "reasoning_effort".to_string(),
            SessionSettingValue::String("medium".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(settings),
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                instructions: Some("Use the fork-specific instructions.".to_string()),
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        };

        let (backend, _events) = <CodexBackend as Backend>::fork(
            vec![workspace_root.clone(), "/tmp".to_string()],
            config,
            SessionId("parent-thread-id".to_string()),
            protocol::SendMessagePayload {
                message: "child prompt".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("Codex fork should start against fake app-server");

        assert_eq!(
            Backend::session_id(&backend),
            SessionId("child-thread-id".to_string())
        );
        backend.shutdown().await;

        let (_fork_pid, requests) = fake.captured_fork_process("parent-thread-id");
        let methods = requests
            .iter()
            .filter_map(|request| request.get("method").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(methods, vec!["initialize", "thread/fork", "turn/start"]);

        let fork_params = requests[1].get("params").expect("thread/fork params");
        assert_eq!(
            fork_params.get("threadId").and_then(Value::as_str),
            Some("parent-thread-id")
        );
        assert_eq!(
            fork_params.get("cwd").and_then(Value::as_str),
            Some(workspace_root.as_str())
        );
        assert_eq!(
            fork_params.get("sandbox").and_then(Value::as_str),
            Some(CODEX_READ_ONLY_SANDBOX)
        );
        assert_eq!(
            fork_params.get("approvalPolicy").and_then(Value::as_str),
            Some(CODEX_FORCED_APPROVAL_POLICY)
        );
        assert_eq!(
            fork_params
                .get("experimentalRawEvents")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            fork_params
                .get("persistExtendedHistory")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            fork_params
                .get("runtimeWorkspaceRoots")
                .and_then(Value::as_array)
                .expect("runtimeWorkspaceRoots")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            vec![workspace_root.as_str(), "/tmp"]
        );

        // Other tests may trigger short-lived model discovery while this fake
        // app-server override is installed; this exact sequence is for the
        // process that handled thread/fork. CodexSession::execute(UpdateSettings)
        // only updates local state used by subsequent turn/start calls, so
        // configuring model/effort above should not add a thread/update RPC.
        let turn_params = requests[2].get("params").expect("turn/start params");
        assert_eq!(
            turn_params.get("threadId").and_then(Value::as_str),
            Some("child-thread-id"),
            "the initial turn must be sent to the returned child thread, not the parent"
        );
        assert_eq!(
            turn_params.pointer("/input/0/text").and_then(Value::as_str),
            Some("child prompt")
        );
        assert_eq!(
            turn_params.get("model").and_then(Value::as_str),
            Some("gpt-test")
        );
        assert_eq!(
            turn_params.get("effort").and_then(Value::as_str),
            Some("medium")
        );
        assert_eq!(
            turn_params
                .pointer("/sandboxPolicy/type")
                .and_then(Value::as_str),
            Some("workspaceWrite")
        );
    }

    #[tokio::test]
    async fn dropping_codex_spawn_after_readiness_cancels_before_initial_prompt() {
        let fake = CodexFakeAppServer::new("ok", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let (ready_tx, mut ready_rx) = oneshot::channel();
        *CODEX_SPAWN_READY_OBSERVER
            .lock()
            .expect("Codex spawn ready observer mutex poisoned") = Some(ready_tx);
        let mut thread_start_request_rx = install_codex_request_observer("thread/start");
        let (cancelled_tx, cancelled_rx) = oneshot::channel();
        *CODEX_SPAWN_STARTUP_CANCEL_OBSERVER
            .lock()
            .expect("Codex spawn startup cancel observer mutex poisoned") = Some(cancelled_tx);
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let mut startup = Box::pin(CodexBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "must not submit after readiness cancellation".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        ));
        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, async {
            tokio::select! {
                biased;
                observed = &mut thread_start_request_rx => {
                    observed.expect("Codex worker must retain the thread/start request observer");
                }
                result = startup.as_mut() => {
                    match result {
                        Ok(_) => panic!("Codex spawn completed before issuing thread/start"),
                        Err(error) => {
                            panic!("Codex spawn failed before issuing thread/start: {error}")
                        }
                    }
                }
            }
        })
        .await
        .expect("fixture must observe the real thread/start write before readiness");
        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, async {
            tokio::select! {
                biased;
                observed = &mut ready_rx => {
                    observed.expect("Codex worker must retain readiness observer");
                }
                _ = startup.as_mut() => {
                    panic!("Codex spawn must not complete before the readiness handoff is observed");
                }
            }
        })
        .await
        .expect("production Codex spawn must reach its readiness handoff");

        drop(startup);

        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, cancelled_rx)
            .await
            .expect("detached Codex spawn worker must acknowledge startup cancellation")
            .expect("detached Codex spawn worker must retain its cancellation observer");
        let methods = fake
            .requests()
            .into_iter()
            .filter_map(|request| {
                request
                    .get("method")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();
        assert!(methods.iter().any(|method| method == "thread/start"));
        assert!(
            !methods.iter().any(|method| method == "turn/start"),
            "dropping ordinary Codex startup at the readiness handoff must prevent the paid initial prompt: {methods:?}"
        );
    }

    #[tokio::test]
    async fn dropping_codex_fork_startup_cancels_before_initial_prompt() {
        let fake = CodexFakeAppServer::new("fork_startup_delayed", "cancelled-child-thread");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let (cancelled_tx, cancelled_rx) = oneshot::channel();
        *CODEX_FORK_STARTUP_CANCEL_OBSERVER
            .lock()
            .expect("Codex fork startup cancel observer mutex poisoned") = Some(cancelled_tx);
        let mut fork_request_rx = install_codex_request_observer("thread/fork");
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let mut startup = Box::pin(<CodexBackend as Backend>::fork(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            SessionId("cancelled-parent-thread".to_owned()),
            protocol::SendMessagePayload {
                message: "must not submit after actor cancellation".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        ));
        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, async {
            tokio::select! {
                observed = &mut fork_request_rx => {
                    observed.expect("Codex worker must retain the fork request observer");
                }
                _ = startup.as_mut() => {
                    panic!("fork startup completed before the fixture held its real fork request");
                }
            }
        })
        .await
        .expect("fixture must observe the detached Codex fork request before readiness");
        drop(startup);
        fake.release_initial_turn();

        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, cancelled_rx)
            .await
            .expect("detached Codex fork worker must acknowledge startup cancellation")
            .expect("detached Codex fork worker must retain its cancellation observer");
        assert!(
            fake.fork_response_marker.exists(),
            "fixture must release the detached worker's real fork response before cancellation"
        );
        assert!(
            !fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("turn/start")
            }),
            "dropping startup must explicitly cancel the detached worker before the paid initial prompt"
        );
    }

    #[tokio::test]
    async fn codex_backend_fork_cleans_steering_tempfile_when_app_server_spawn_fails() {
        let missing_binary_dir = tempfile::tempdir().expect("missing binary tempdir");
        let _guard = CodexTestAppServerBinaryGuard::set(
            missing_binary_dir.path().join("missing-codex-app-server"),
        );
        let before = codex_steering_tempfiles();

        let result = CodexSession::fork(
            &["/tmp".to_string()],
            None,
            &[],
            Some("Temporary fork instructions."),
            BackendAccessMode::ReadOnly,
            "parent-thread-id",
        )
        .await;

        let err = match result {
            Ok((session, _)) => {
                session.shutdown().await;
                panic!("missing app-server binary should fail fork")
            }
            Err(err) => err,
        };
        assert!(
            err.contains("Failed to spawn Codex app-server"),
            "unexpected fork spawn error: {err}"
        );
        let after = codex_steering_tempfiles();
        let leaked = after.difference(&before).collect::<Vec<_>>();
        assert!(
            leaked.is_empty(),
            "CodexSession::fork should remove steering tempfiles when app-server spawn fails; leaked={leaked:?}"
        );
    }

    #[tokio::test]
    async fn codex_backend_fork_method_not_found_is_unsupported() {
        let fake = CodexFakeAppServer::new("unsupported", "unused-child-thread-id");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let config = BackendSpawnConfig {
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                instructions: Some("Temporary fork instructions.".to_string()),
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = <CodexBackend as Backend>::fork(
            vec!["/tmp".to_string()],
            config,
            SessionId("parent-thread-id".to_string()),
            protocol::SendMessagePayload {
                message: "child prompt".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await;
        let err = match result {
            Ok((backend, _)) => {
                backend.shutdown().await;
                panic!("Codex fork should fail when app-server lacks thread/fork")
            }
            Err(err) => err,
        };

        assert_eq!(err.code, AgentErrorCode::Unsupported);
        assert!(err.message.contains("thread/fork"));
        assert!(err.message.contains("Update Codex CLI"));

        let (fork_pid, requests) = fake.captured_fork_process("parent-thread-id");
        let methods = requests
            .iter()
            .filter_map(|request| request.get("method").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec!["initialize", "thread/fork"],
            "unsupported fork must not fall back to thread/start, resume, or session-file copying"
        );

        let steering_path = fake
            .argv_for_pid(fork_pid)
            .windows(2)
            .find_map(|args| {
                if args[0] == "-c" {
                    args[1].strip_prefix("model_instructions_file=")
                } else {
                    None
                }
            })
            .map(|quoted| {
                serde_json::from_str::<String>(quoted)
                    .expect("model_instructions_file should be TOML/JSON quoted")
            })
            .expect("fake Codex app-server should receive a steering tempfile override");
        assert!(
            !std::path::Path::new(&steering_path).exists(),
            "Codex fork startup failure should remove steering tempfile {steering_path}"
        );
    }

    #[tokio::test]
    async fn codex_backend_fork_rejects_ssh_roots_without_local_app_server() {
        let fake = CodexFakeAppServer::new("ok", "unused-child-thread-id");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());

        let result = <CodexBackend as Backend>::fork(
            vec!["ssh://devbox.example.com/workspace".to_string()],
            BackendSpawnConfig::default(),
            SessionId("parent-thread-id".to_string()),
            protocol::SendMessagePayload {
                message: "child prompt".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await;

        let err = match result {
            Ok((backend, _)) => {
                backend.shutdown().await;
                panic!("SSH-backed Codex fork should fail before app-server startup")
            }
            Err(err) => err,
        };
        assert_eq!(err.code, AgentErrorCode::Unsupported);
        assert!(err.message.contains("SSH host 'devbox.example.com'"));
        assert!(
            fake.requests().iter().all(|request| {
                request.get("method").and_then(Value::as_str) != Some("thread/fork")
                    || request.pointer("/params/threadId").and_then(Value::as_str)
                        != Some("parent-thread-id")
            }),
            "SSH fork must not silently try a local Codex thread/fork"
        );
    }

    fn summarize_live_event(event: &Value) -> String {
        let kind = event
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "ToolRequest" => {
                let tool_name = event
                    .get("data")
                    .and_then(|d| d.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let call_id = event
                    .get("data")
                    .and_then(|d| d.get("tool_call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!("kind=ToolRequest tool={tool_name} call_id={call_id}")
            }
            "ToolExecutionCompleted" => {
                let tool_name = event
                    .get("data")
                    .and_then(|d| d.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let success = event
                    .get("data")
                    .and_then(|d| d.get("success"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let call_id = event
                    .get("data")
                    .and_then(|d| d.get("tool_call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!(
                    "kind=ToolExecutionCompleted tool={tool_name} success={success} call_id={call_id}"
                )
            }
            "Error" => {
                let data = event.get("data").cloned().unwrap_or(Value::Null);
                format!("kind=Error data={data}")
            }
            "StreamStart" => {
                let model = event
                    .get("data")
                    .and_then(|d| d.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!("kind=StreamStart model={model}")
            }
            "StreamEnd" => {
                let content = event
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let preview = if content.len() > 80 {
                    format!("{}...", &content[..80])
                } else {
                    content.to_string()
                };
                format!("kind=StreamEnd preview={preview:?}")
            }
            "MessageAdded" => {
                let sender = event
                    .get("data")
                    .and_then(|d| d.get("sender"))
                    .cloned()
                    .unwrap_or(Value::Null);
                format!("kind=MessageAdded sender={sender}")
            }
            "TypingStatusChanged" => {
                let typing = event.get("data").and_then(Value::as_bool).unwrap_or(false);
                format!("kind=TypingStatusChanged typing={typing}")
            }
            other => format!("kind={other}"),
        }
    }

    fn test_codex_state() -> CodexState {
        CodexState {
            thread_id: "thread-test".to_string(),
            model: Some("codex".to_string()),
            reasoning_effort: Some("xhigh".to_string()),
            approval_policy: None,
            access_mode: BackendAccessMode::Unrestricted,
            execution_mode: BackendExecutionMode::Agent,
            turn_network_access: false,
            active_turn_id: Some("turn-test".to_string()),
            active_stream: Some(ActiveStreamState {
                turn_id: "turn-test".to_string(),
                message_id: ChatMessageId("msg-seed".to_string()),
                generated_identity: None,
                text: String::new(),
                reasoning: String::new(),
                reasoning_only: false,
            }),
            completed_agent_messages: HashMap::new(),
            quarantined_turn_id: None,
            generated_identity_epoch: codex_generated_identity_epoch("thread-test"),
            next_generated_identity_ordinal: 1,
            pending_tool_call_ids: HashSet::new(),
            close_active_stream_when_tools_idle: false,
            pending_message_metadata: None,
            completed_message_metadata_by_turn: HashMap::new(),
            token_usage_by_turn: HashMap::new(),
            model_token_usage_by_turn: HashMap::new(),
            turn_context_by_turn: HashMap::new(),
            file_change_call_ids: HashMap::new(),
            pending_request: None,
            pending_user_input_bytes: 0,
            conversation_bytes_total: 0,
            subagent_emitter: None,
            pending_subagent_spawns: HashMap::new(),
            conflicting_subagent_threads: HashMap::new(),
            registering_subagent_threads: HashSet::new(),
            unknown_owner_notifications: HashSet::new(),
            subagent_streams: HashMap::new(),
            completed_subagent_streams: HashMap::new(),
        }
    }

    fn test_codex_inner() -> (Arc<CodexInner>, mpsc::UnboundedReceiver<Value>) {
        let mut child = Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .group_spawn()
            .expect("spawn test child");
        let stdin = child
            .inner()
            .stdin
            .take()
            .expect("capture test child stdin");
        let stdout = child.inner().stdout.take();
        let stderr = child.inner().stderr.take();
        let stdout_task = tokio::spawn(async move {
            drop(stdout);
        });
        let stderr_task = tokio::spawn(async move {
            drop(stderr);
        });
        let rpc = CodexRpc {
            stdin: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            child: Arc::new(Mutex::new(Some(child))),
            stdout_task,
            stderr_task,
            _isolated_codex_home: None,
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(CodexInner {
            rpc,
            emitter: Arc::new(TurnEmitter::new_for_agent(
                event_tx,
                AgentName(CODEX_AGENT_NAME),
            )),
            state: Mutex::new(test_codex_state()),
            steering_tempfile: None,
        });
        (inner, event_rx)
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        let mut normalization_failures = HashMap::new();
        while let Ok(event) = rx.try_recv() {
            match event.get("kind").and_then(Value::as_str) {
                Some("ModelRequestTokenUsage") => {}
                Some("Error") => {
                    let forwarded =
                        codex_backend_event_from_raw(&event, &mut normalization_failures)
                            .expect("raw Codex error must forward to a visible chat event");
                    out.push(
                        serde_json::to_value(forwarded.chat_event)
                            .expect("serialize forwarded Codex error"),
                    );
                }
                _ => out.push(event),
            }
        }
        out
    }

    async fn attach_test_codex_subagent(
        inner: &Arc<CodexInner>,
        subagent_tx: mpsc::UnboundedSender<Value>,
        receiver_thread_id: &str,
    ) {
        let mut state = inner.state.lock().await;
        state.thread_id = "thread-parent".to_string();
        state.subagent_streams.insert(
            receiver_thread_id.to_string(),
            CodexSubAgentStream {
                emitter: Arc::new(TurnEmitter::new_for_agent(
                    subagent_tx,
                    AgentName(CODEX_AGENT_NAME),
                )),
                spawn_item_id: receiver_thread_id.to_string(),
                activity_item_id: None,
                agent_path: receiver_thread_id.to_string(),
                sender_thread_id: "thread-parent".to_string(),
                active_turn_id: None,
                current_message_id: None,
                current_generated_identity: None,
                current_reasoning_only: false,
                current_text: String::new(),
                current_reasoning: String::new(),
                completed_agent_messages: HashMap::new(),
                quarantined_turn_id: None,
                quarantined: false,
                generated_identity_epoch: codex_generated_identity_epoch(receiver_thread_id),
                next_generated_identity_ordinal: 1,
                pending_message_metadata: None,
                token_usage_by_turn: HashMap::new(),
            },
        );
    }

    fn event_kinds(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| event.get("kind").and_then(Value::as_str))
            .collect()
    }

    fn forwarded_codex_event(raw: Value) -> (bool, Option<ChatEvent>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut normalization_failures = HashMap::new();
        let keep_open = forward_codex_backend_event(raw, &tx, &mut normalization_failures);
        (keep_open, rx.try_recv().ok())
    }

    fn forwarded_visible_codex_event(raw: Value) -> (bool, ChatEvent) {
        let (keep_open, event) = forwarded_codex_event(raw);
        (
            keep_open,
            event.expect("raw Codex event should become a ChatEvent"),
        )
    }

    #[test]
    fn forward_codex_backend_event_passes_valid_chat_event_unchanged() {
        let raw = json!({
            "kind": "TypingStatusChanged",
            "data": true,
        });

        let (keep_open, event) = forwarded_visible_codex_event(raw.clone());

        assert!(keep_open);
        assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));
        assert_eq!(
            serde_json::to_value(event).expect("serialize forwarded event"),
            raw
        );
    }

    #[test]
    fn malformed_canonical_codex_tool_request_is_visible_and_inspectable() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut normalization_failures = HashMap::new();
        assert!(forward_codex_backend_event(
            json!({
                "kind": "ToolRequest",
                "data": {
                    "tool_call_id": "call-malformed",
                    "tool_name": "mcp__tyde-agent-control__tyde_send_agent_message",
                    "tool_type": {
                        "kind": "Other",
                        "args": { "agent_id": "agent-a" }
                    }
                }
            }),
            &tx,
            &mut normalization_failures,
        ));

        let ChatEvent::MessageAdded(error) = rx.try_recv().expect("visible invariant error") else {
            panic!("malformed canonical request must emit a visible error");
        };
        assert!(matches!(error.sender, MessageSender::Error));
        assert!(error.content.contains("tyde_send_agent_message"));
        assert!(error.content.contains("call-malformed"));

        let ChatEvent::ToolRequest(request) = rx.try_recv().expect("inspectable raw request")
        else {
            panic!("malformed canonical request must remain inspectable");
        };
        assert!(matches!(
            request.tool_type,
            protocol::ToolRequestType::Other { .. }
        ));
    }

    #[test]
    fn malformed_codex_request_marks_its_completion_without_exposing_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut normalization_failures = HashMap::new();
        assert!(forward_codex_backend_event(
            json!({
                "kind": "ToolRequest",
                "data": {
                    "tool_call_id": "call-normalization",
                    "tool_name": "mcp__tyde-agent-control__tyde_send_agent_message",
                    "tool_type": {
                        "kind": "Other",
                        "args": { "agent_id": "agent-a", "api_key": "request-secret" }
                    }
                }
            }),
            &tx,
            &mut normalization_failures,
        ));
        let _ = rx.try_recv().expect("visible normalization diagnostic");
        let _ = rx.try_recv().expect("inspectable fallback request");

        assert!(forward_codex_backend_event(
            json!({
                "kind": "ToolExecutionCompleted",
                "data": {
                    "tool_call_id": "call-normalization",
                    "tool_name": "mcp__tyde-agent-control__tyde_send_agent_message",
                    "tool_result": { "kind": "Other", "result": { "ok": true } },
                    "success": true,
                    "error": null,
                }
            }),
            &tx,
            &mut normalization_failures,
        ));
        let ChatEvent::ToolExecutionCompleted(completion) =
            rx.try_recv().expect("marked completion")
        else {
            panic!("expected marked tool completion");
        };
        assert_eq!(
            completion.normalization_failure,
            Some(ToolExecutionNormalizationFailure::CanonicalRequest)
        );
        let encoded = serde_json::to_string(&completion).expect("serialize marked completion");
        assert!(!encoded.contains("request-secret"));
    }

    #[test]
    fn malformed_codex_result_marks_only_its_completion() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut normalization_failures = HashMap::new();
        assert!(forward_codex_backend_event(
            json!({
                "kind": "ToolRequest",
                "data": {
                    "tool_call_id": "call-result-normalization",
                    "tool_name": "mcp__tyde-agent-await__tyde_await_agents",
                    "tool_type": {
                        "kind": "Other",
                        "args": { "arguments": { "agent_ids": ["agent-a"] } }
                    }
                }
            }),
            &tx,
            &mut normalization_failures,
        ));
        let ChatEvent::ToolRequest(request) = rx.try_recv().expect("typed request") else {
            panic!("expected typed tool request");
        };
        assert!(matches!(
            request.tool_type,
            protocol::ToolRequestType::TydeAwaitAgents { .. }
        ));

        assert!(forward_codex_backend_event(
            json!({
                "kind": "ToolExecutionCompleted",
                "data": {
                    "tool_call_id": "call-result-normalization",
                    "tool_name": "mcp__tyde-agent-await__tyde_await_agents",
                    "tool_result": {
                        "kind": "Other",
                        "result": { "ready": [], "api_key": "result-secret" }
                    },
                    "success": true,
                    "error": null,
                }
            }),
            &tx,
            &mut normalization_failures,
        ));
        let ChatEvent::MessageAdded(error) = rx.try_recv().expect("result diagnostic") else {
            panic!("expected visible result diagnostic");
        };
        assert!(matches!(error.sender, MessageSender::Error));
        assert!(
            !serde_json::to_string(&error)
                .expect("serialize result diagnostic")
                .contains("result-secret")
        );
        let ChatEvent::ToolExecutionCompleted(completion) =
            rx.try_recv().expect("marked completion")
        else {
            panic!("expected marked tool completion");
        };
        assert_eq!(
            completion.normalization_failure,
            Some(ToolExecutionNormalizationFailure::CanonicalResult)
        );
    }

    #[test]
    fn model_request_usage_uses_backend_only_event_path() {
        let usage = ModelRequestTokenUsage {
            request_id: ModelRequestId {
                turn_id: ModelTurnId("turn-1".to_owned()),
                sequence: 3,
            },
            request: TokenUsage {
                total_tokens: 12,
                ..TokenUsage::default()
            },
            turn: TokenUsage {
                total_tokens: 42,
                ..TokenUsage::default()
            },
            cumulative: TokenUsage {
                total_tokens: 142,
                ..TokenUsage::default()
            },
            model_context_window: Some(400_000),
        };
        let raw = json!({
            "kind": "ModelRequestTokenUsage",
            "data": serde_json::to_value(&usage).expect("serialize usage"),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut normalization_failures = HashMap::new();

        assert!(forward_codex_backend_stream_event(
            raw.clone(),
            &tx,
            &mut normalization_failures,
        ));
        let BackendEvent::ModelRequestTokenUsage(forwarded) =
            rx.try_recv().expect("backend usage event")
        else {
            panic!("usage should not be forwarded as chat");
        };
        assert_eq!(forwarded, usage);

        let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
        assert!(forward_codex_backend_event(
            raw,
            &chat_tx,
            &mut normalization_failures,
        ));
        assert!(chat_rx.try_recv().is_err());
    }

    #[test]
    fn forward_codex_backend_event_converts_raw_error_to_visible_message() {
        let (keep_open, event) = forwarded_visible_codex_event(json!({
            "kind": "Error",
            "data": "backend exploded",
        }));

        assert!(keep_open);
        let ChatEvent::MessageAdded(message) = event else {
            panic!("raw Error should become MessageAdded, got {event:?}");
        };
        assert!(matches!(message.sender, MessageSender::Error));
        assert_eq!(message.content, "backend exploded");
    }

    #[test]
    fn forward_codex_backend_event_converts_warning_stderr_to_visible_message() {
        let (keep_open, event) = forwarded_visible_codex_event(json!({
            "kind": "SubprocessStderr",
            "data": "Codex warning: Tool warning",
        }));

        assert!(keep_open);
        let ChatEvent::MessageAdded(message) = event else {
            panic!("stderr should become MessageAdded, got {event:?}");
        };
        assert!(matches!(message.sender, MessageSender::Warning));
        assert_eq!(message.content, "Codex warning: Tool warning");
    }

    #[test]
    fn forward_codex_backend_event_logs_generic_stderr_without_chat_event() {
        let (keep_open, event) = forwarded_codex_event(json!({
            "kind": "SubprocessStderr",
            "data": "debug noise",
        }));

        assert!(keep_open);
        assert!(
            event.is_none(),
            "generic stderr should be logged but not forwarded to chat"
        );
    }

    #[test]
    fn forward_codex_backend_event_converts_subprocess_exit_to_terminal_error() {
        let (keep_open, event) = forwarded_visible_codex_event(json!({
            "kind": "SubprocessExit",
            "data": { "exit_code": 7 },
        }));

        assert!(!keep_open);
        let ChatEvent::MessageAdded(message) = event else {
            panic!("subprocess exit should become MessageAdded, got {event:?}");
        };
        assert!(matches!(message.sender, MessageSender::Error));
        assert_eq!(message.content, "Codex subprocess exited with code 7");
    }

    #[tokio::test]
    async fn codex_backend_fresh_spawn_emits_agent_control_progress() {
        let fake = CodexFakeAppServer::new("fresh_agent_control_progress", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let workspace_root = workspace.path().to_string_lossy().to_string();

        let (backend, mut events) = <CodexBackend as Backend>::spawn(
            vec![workspace_root],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "start".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("Codex fresh spawn should start against fake app-server");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_await = false;
        let mut saw_spawn = false;
        while tokio::time::Instant::now() < deadline && !(saw_await && saw_spawn) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(ChatEvent::ToolProgress(progress))) => {
                    let ToolProgressUpdate::AgentControl(progress_update) = progress.update else {
                        continue;
                    };
                    match progress_update.progress_kind {
                        AgentControlProgressKind::Await => {
                            saw_await |= progress.tool_call_id == "await-call-1"
                                && progress_update
                                    .agents
                                    .iter()
                                    .map(|agent| agent.agent_id.0.as_str())
                                    .collect::<Vec<_>>()
                                    == vec!["agent-a", "agent-b"];
                        }
                        AgentControlProgressKind::Spawn => {
                            saw_spawn |= progress.tool_call_id == "spawn-call-1"
                                && progress_update.agents.len() == 1
                                && progress_update.agents[0].agent_id
                                    == AgentId("agent-spawned".to_string())
                                && progress_update.agents[0].name.as_deref() == Some("Builder");
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }

        backend.shutdown().await;
        assert!(
            saw_await,
            "fresh Codex session loop did not emit Await progress"
        );
        assert!(
            saw_spawn,
            "fresh Codex session loop did not emit Spawn progress"
        );
    }

    #[tokio::test]
    async fn codex_backend_fresh_spawn_routes_native_child_by_thread() {
        let fake = CodexFakeAppServer::new("fresh_native_child_routing", "child-thread-id");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let emitter = Arc::new(RecordingSubAgentEmitter::new());
        let (backend, mut events) = CodexBackend::spawn_with_subagent_emitter(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "start".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
            emitter.clone() as Arc<dyn SubAgentEmitter>,
        )
        .await
        .expect("production Codex spawn should start against fake app-server");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut parent_events = Vec::new();
        let mut saw_wait_request = false;
        let mut saw_wait_completion = false;
        let mut saw_parent_terminal = false;
        while tokio::time::Instant::now() < deadline && !saw_parent_terminal {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(event)) => {
                    saw_wait_request |= matches!(
                        &event,
                        ChatEvent::ToolRequest(request) if request.tool_call_id == "wait-call"
                    );
                    saw_wait_completion |= matches!(
                        &event,
                        ChatEvent::ToolExecutionCompleted(completion)
                            if completion.tool_call_id == "wait-call"
                                && completion.success
                                && completion.error.is_none()
                    );
                    saw_parent_terminal |= matches!(&event, ChatEvent::TypingStatusChanged(false));
                    parent_events.push(event);
                }
                Ok(None) | Err(_) => break,
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            emitter.spawn_count().await,
            1,
            "repeated child activity must not create another relay"
        );
        assert!(
            !parent_events.iter().any(|event| matches!(
                event,
                ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error)
            )),
            "identical activity started/completed must not surface a parent error"
        );
        assert!(
            saw_wait_request,
            "fixture must exercise the parent's pending wait card"
        );
        assert!(
            saw_wait_completion,
            "wait must complete with Codex's result, not cancellation"
        );
        assert!(
            saw_parent_terminal,
            "fixture must drain through the parent terminal marker"
        );
        let child_events = emitter.events_by_agent().await;
        assert!(child_events.values().flatten().any(|event| matches!(
            event,
            ChatEvent::StreamEnd(payload) if payload.message.content == "child-only"
        )));
        assert!(
            child_events
                .values()
                .flatten()
                .any(|event| matches!(event, ChatEvent::OperationCancelled(_)))
        );
        assert!(!parent_events.iter().any(|event| matches!(
            event,
            ChatEvent::StreamEnd(payload) if payload.message.content == "child-only"
        )));
        assert!(
            !parent_events
                .iter()
                .any(|event| matches!(event, ChatEvent::OperationCancelled(_))),
            "the child interruption must not cancel the parent wait/turn"
        );

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn codex_backend_publishes_parent_session_before_live_order_child_activity() {
        let fake = CodexFakeAppServer::new(
            "fresh_native_child_before_initial_response",
            "native-quick-child-thread",
        );
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let (ready_tx, mut ready_rx) = oneshot::channel();
        *CODEX_SPAWN_READY_OBSERVER
            .lock()
            .expect("Codex spawn ready observer mutex poisoned") = Some(ready_tx);
        let mut initial_turn_request_rx = install_codex_request_observer("turn/start");
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let emitter = Arc::new(RecordingSubAgentEmitter::new());
        let startup = CodexBackend::spawn_with_subagent_emitter(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "start release child routing".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
            emitter.clone() as Arc<dyn SubAgentEmitter>,
        );
        tokio::pin!(startup);
        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, async {
            tokio::select! {
                biased;
                observed = &mut ready_rx => {
                    observed
                        .expect("Codex worker must retain its parent-session readiness observer");
                }
                result = &mut startup => {
                    match result {
                        Ok(_) => panic!(
                            "Codex startup completed before its parent-session readiness handoff was observed"
                        ),
                        Err(error) => {
                            panic!(
                                "production Codex startup failed before parent-session readiness: {error}"
                            )
                        }
                    }
                }
            }
        })
        .await
        .expect(
            "fixture must observe authoritative parent-session publication before the delayed initial turn response",
        );
        let (backend, mut events) = match tokio::time::timeout(
            Duration::from_millis(500),
            &mut startup,
        )
        .await
        {
            Ok(Ok(started)) => started,
            Ok(Err(error)) => {
                panic!("production Codex startup failed before child ordering check: {error}")
            }
            Err(_) => {
                fake.release_initial_turn();
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut startup).await;
                panic!(
                    "Codex startup waited for the initial turn response instead of publishing the parent session before native child activity"
                );
            }
        };
        assert_eq!(
            backend.session_id().0,
            "fresh-thread-id",
            "startup must publish the authoritative thread/start session before the delayed turn response"
        );

        // Ordinary spawn now deliberately waits for the caller to accept the
        // authoritative session before it submits the paid initial turn. Claim
        // that handoff first, then require the fixture to observe that turn.
        tokio::time::timeout(CODEX_REQUEST_TIMEOUT, &mut initial_turn_request_rx)
            .await
            .expect(
                "fixture must launch its delayed initial turn after the ordinary-spawn readiness handoff",
            )
            .expect("Codex worker must retain the delayed initial-turn request observer");

        let registration_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while emitter.spawn_count().await == 0
            && tokio::time::Instant::now() < registration_deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            emitter.spawn_count().await,
            1,
            "the live-order child activity must register before the parent interrupt"
        );

        assert!(
            backend.interrupt().await,
            "the parent interrupt must reach Codex while the initial turn response is delayed"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut parent_events = Vec::new();
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(event)) => {
                    let terminal = matches!(&event, ChatEvent::TypingStatusChanged(false));
                    parent_events.push(event);
                    if terminal {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        fake.release_initial_turn();

        let spawns = emitter.spawns().await;
        assert_eq!(
            spawns.len(),
            1,
            "the live-order child must allocate exactly once"
        );
        assert_eq!(
            spawns[0].tool_use_id,
            "019f60f0-7a69-73f0-9ab3-7ddc24062e30"
        );
        assert_eq!(spawns[0].name, "/root/quick_child");
        assert_eq!(spawns[0].native_thread_id, "native-quick-child-thread");
        assert!(
            !parent_events.iter().any(|event| matches!(
                event,
                ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error)
            )),
            "authoritative child activity before the initial turn response must not surface a parent error: {parent_events:?}"
        );
        assert!(
            parent_events
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false))),
            "the parent must reach its terminal idle marker after its interrupt"
        );
        assert!(
            parent_events
                .iter()
                .any(|event| matches!(event, ChatEvent::OperationCancelled(_))),
            "the delayed initial turn must receive the full interruption tail"
        );

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn codex_backend_cancels_pending_initial_input_during_startup_settings() {
        let fake = CodexFakeAppServer::new("startup_settings_delayed", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            SessionSettingValue::String("fake-startup-model".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(settings),
            ..BackendSpawnConfig::default()
        };
        assert_eq!(
            resolve_session_settings(&config).0.get("model"),
            Some(&SessionSettingValue::String(
                "fake-startup-model".to_string()
            )),
            "fixture must pass an authoritative model override into Codex startup"
        );
        let startup = CodexBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            config,
            protocol::SendMessagePayload {
                message: "wait for startup settings".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        );
        tokio::pin!(startup);
        let mut startup_result = None;

        let thread_start_deadline = tokio::time::Instant::now() + CODEX_REQUEST_TIMEOUT;
        while !fake
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("thread/start"))
            && tokio::time::Instant::now() < thread_start_deadline
        {
            if let Some(Err(error)) = startup_result.as_ref() {
                panic!("production Codex startup failed before thread/start: {error}");
            }
            tokio::select! {
                result = &mut startup, if startup_result.is_none() => {
                    startup_result = Some(result);
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        assert!(
            fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("thread/start")
            }),
            "fixture must observe the authoritative thread/start request before startup-settings dispatch"
        );
        let settings_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while !fake
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("thread/update"))
            && tokio::time::Instant::now() < settings_deadline
        {
            if let Some(Err(error)) = startup_result.as_ref() {
                panic!("production Codex startup failed before delayed thread/update: {error}");
            }
            tokio::select! {
                result = &mut startup, if startup_result.is_none() => {
                    startup_result = Some(result);
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        assert!(
            fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("thread/update")
            }),
            "the fake must hold the startup thread/update request; captured methods: {:?}",
            fake.requests()
                .iter()
                .filter_map(|request| request.get("method").and_then(Value::as_str))
                .collect::<Vec<_>>()
        );
        let (backend, mut events) = match startup_result {
            Some(Ok(started)) => started,
            Some(Err(error)) => {
                panic!("production Codex startup failed before delayed thread/update: {error}")
            }
            None => tokio::time::timeout(Duration::from_millis(500), &mut startup)
                .await
                .expect("parent session must publish before delayed thread/update")
                .expect("production Codex spawn should publish its parent session"),
        };

        assert!(
            backend.interrupt().await,
            "the published backend must accept an interrupt while thread/update is delayed"
        );
        fake.release_startup_settings();
        let mut cancellation_events = tokio::time::timeout(Duration::from_secs(1), async {
            let mut cancellation_events = Vec::new();
            loop {
                let Some(event) = events.recv().await else {
                    panic!("pending initial-input cancellation ended before its idle tail");
                };
                let terminal = matches!(&event, ChatEvent::TypingStatusChanged(false));
                cancellation_events.push(event);
                if terminal {
                    return cancellation_events;
                }
            }
        })
        .await
        .expect("the pending initial-input cancellation must emit its terminal idle tail");
        assert_eq!(
            cancellation_events
                .iter()
                .filter(|event| matches!(event, ChatEvent::OperationCancelled(_)))
                .count(),
            1,
            "pending initial-input cancellation must emit one cancellation event"
        );
        assert_eq!(
            cancellation_events
                .iter()
                .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
                .count(),
            1,
            "pending initial-input cancellation must emit one idle event"
        );
        assert!(
            !fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("turn/interrupt")
            }),
            "pre-turn cancellation must not issue a no-op turn/interrupt request"
        );

        assert!(
            backend
                .send(AgentInput::SendMessage(protocol::SendMessagePayload {
                    message: "follow up after cancelled initial".to_string(),
                    images: None,
                    origin: None,
                    tool_response: None,
                }))
                .await,
            "the backend must remain idle and accept a follow-up after cancelling its initial input"
        );
        let follow_up_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while !fake.requests().iter().any(|request| {
            request
                .pointer("/params/input/0/text")
                .and_then(Value::as_str)
                == Some("follow up after cancelled initial")
        }) && tokio::time::Instant::now() < follow_up_deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            fake.requests().iter().any(|request| {
                request
                    .pointer("/params/input/0/text")
                    .and_then(Value::as_str)
                    == Some("follow up after cancelled initial")
            }),
            "a follow-up after the cancelled initial input must start a real Codex turn"
        );
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| {
                    request.get("method").and_then(Value::as_str) == Some("turn/start")
                })
                .count(),
            1,
            "settings completion must not resurrect the cancelled initial prompt"
        );
        while let Ok(event) = events.try_recv() {
            cancellation_events.push(event);
        }
        assert_eq!(
            cancellation_events
                .iter()
                .filter(|event| matches!(event, ChatEvent::OperationCancelled(_)))
                .count(),
            1,
            "the follow-up must not duplicate the initial cancellation tail"
        );
        assert_eq!(
            cancellation_events
                .iter()
                .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
                .count(),
            1,
            "the follow-up must not duplicate the initial idle tail"
        );

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn codex_backend_dispatches_initial_prompt_after_startup_settings_success() {
        let fake = CodexFakeAppServer::new("startup_settings_delayed", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            SessionSettingValue::String("fake-startup-model".to_string()),
        );
        let (backend, _) = CodexBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                session_settings: Some(settings),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "dispatch after settings".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("production Codex spawn should publish before delayed startup settings");

        let settings_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while !fake
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("thread/update"))
            && tokio::time::Instant::now() < settings_deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("turn/start")
            }),
            "the initial prompt must not start before thread/update succeeds"
        );

        fake.release_startup_settings();
        let prompt_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while !fake
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
            && tokio::time::Instant::now() < prompt_deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("turn/start")
            }),
            "the initial prompt must dispatch after successful startup settings"
        );

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn codex_backend_startup_settings_rejection_emits_error_then_idle() {
        let fake = CodexFakeAppServer::new("startup_settings_rejected", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            SessionSettingValue::String("fake-startup-model".to_string()),
        );
        let (backend, mut events) = CodexBackend::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig {
                session_settings: Some(settings),
                ..BackendSpawnConfig::default()
            },
            protocol::SendMessagePayload {
                message: "settings must reject".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("parent session must publish before startup-settings rejection");

        let (error_index, idle_index) = tokio::time::timeout(Duration::from_secs(1), async {
            let mut events_seen = Vec::new();
            loop {
                let Some(event) = events.recv().await else {
                    panic!("startup settings rejection ended before typed terminal events");
                };
                events_seen.push(event);
                let error_index = events_seen.iter().position(|event| {
                    matches!(
                        event,
                        ChatEvent::MessageAdded(message)
                            if matches!(message.sender, MessageSender::Error)
                                && message.content.contains("Failed to configure Codex session")
                    )
                });
                let idle_index = events_seen
                    .iter()
                    .position(|event| matches!(event, ChatEvent::TypingStatusChanged(false)));
                if let (Some(error_index), Some(idle_index)) = (error_index, idle_index) {
                    return (error_index, idle_index);
                }
            }
        })
        .await
        .expect("startup settings rejection must surface typed error and idle terminal event");
        assert!(
            error_index < idle_index,
            "startup settings failure must emit the typed error before terminal idle"
        );
        assert!(
            !fake.requests().iter().any(|request| {
                request.get("method").and_then(Value::as_str) == Some("turn/start")
            }),
            "a rejected startup settings update must never dispatch the initial prompt"
        );
        assert!(
            matches!(
                tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
                Ok(None)
            ),
            "startup settings failure must close the parent event stream after its idle terminal"
        );

        backend.shutdown().await;
    }

    #[tokio::test]
    async fn codex_backend_fresh_spawn_routes_multiple_native_children_independently() {
        let fake = CodexFakeAppServer::new("fresh_native_multi_child_routing", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let emitter = Arc::new(RecordingSubAgentEmitter::new());
        let (backend, mut events) = CodexBackend::spawn_with_subagent_emitter(
            vec![workspace.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "start".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
            emitter.clone() as Arc<dyn SubAgentEmitter>,
        )
        .await
        .expect("production Codex spawn should start against multi-child fake app-server");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut parent_events = Vec::new();
        let mut completed_waits = HashSet::new();
        let mut saw_parent_terminal = false;
        while tokio::time::Instant::now() < deadline && !saw_parent_terminal {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(event)) => {
                    if let ChatEvent::ToolExecutionCompleted(completion) = &event
                        && completion.success
                        && (completion.tool_call_id == "wait-alpha"
                            || completion.tool_call_id == "wait-beta")
                    {
                        completed_waits.insert(completion.tool_call_id.clone());
                    }
                    saw_parent_terminal |= matches!(&event, ChatEvent::TypingStatusChanged(false));
                    parent_events.push(event);
                }
                Ok(None) | Err(_) => break,
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(saw_parent_terminal);
        assert_eq!(emitter.spawn_count().await, 2);
        assert_eq!(
            completed_waits,
            HashSet::from(["wait-alpha".to_string(), "wait-beta".to_string()])
        );
        let events_by_thread = emitter.events_by_native_thread().await;
        let alpha_events = events_by_thread
            .get("child-a")
            .expect("recorded child A association");
        let beta_events = events_by_thread
            .get("child-b")
            .expect("recorded child B association");
        let has_content = |events: &[ChatEvent], content: &str| {
            events.iter().any(|event| {
                matches!(
                    event,
                    ChatEvent::StreamEnd(payload) if payload.message.content == content
                )
            })
        };
        assert!(has_content(alpha_events, "alpha-only"));
        assert!(!has_content(alpha_events, "beta-only"));
        assert!(has_content(beta_events, "beta-only"));
        assert!(!has_content(beta_events, "alpha-only"));
        assert!(
            !parent_events
                .iter()
                .any(|event| matches!(event, ChatEvent::OperationCancelled(_)))
        );
        assert!(!parent_events.iter().any(|event| matches!(
            event,
            ChatEvent::StreamEnd(payload) if payload.message.content == "alpha-only" || payload.message.content == "beta-only"
        )));
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn codex_backend_fresh_spawn_patches_late_token_usage_after_turn_completed() {
        let fake = CodexFakeAppServer::new("fresh_late_token_usage", "unused");
        let _guard = CodexTestAppServerBinaryGuard::set(fake.binary.clone());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let workspace_root = workspace.path().to_string_lossy().to_string();

        let (backend, mut events) = <CodexBackend as Backend>::spawn(
            vec![workspace_root],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "start".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("Codex fresh spawn should start against fake app-server");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_stream_end = false;
        let mut saw_pre_usage_metadata = false;
        let mut saw_late_usage_metadata = false;
        while tokio::time::Instant::now() < deadline && !saw_late_usage_metadata {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(ChatEvent::StreamEnd(data))) => {
                    if data.message.content == "fresh late done" {
                        saw_stream_end = true;
                        assert!(
                            data.message.token_usage.is_none(),
                            "fresh Codex StreamEnd should not fabricate usage before late update"
                        );
                    }
                }
                Ok(Some(ChatEvent::MessageMetadataUpdated(update))) => {
                    if update.message_id.0 != "msg-fresh-late-usage" {
                        continue;
                    }
                    let Some(token_usage) = update.token_usage.as_ref() else {
                        saw_pre_usage_metadata = true;
                        continue;
                    };
                    match &token_usage.request {
                        TokenUsageScope::Known { usage } => {
                            assert_eq!(usage.total_tokens, 41);
                        }
                        other => panic!("expected known request usage patch, got {other:?}"),
                    }
                    match &token_usage.turn {
                        TokenUsageScope::Known { usage } => {
                            assert_eq!(usage.total_tokens, 41);
                        }
                        other => panic!("expected known turn usage patch, got {other:?}"),
                    }
                    assert!(
                        !matches!(
                            &token_usage.turn,
                            TokenUsageScope::Known { usage } if usage.total_tokens == 0
                        ),
                        "late Codex usage must not be fabricated as Known(0)"
                    );
                    saw_late_usage_metadata = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }

        backend.shutdown().await;
        assert!(
            saw_stream_end,
            "fresh Codex session loop did not emit StreamEnd"
        );
        assert!(
            saw_pre_usage_metadata,
            "fresh Codex session loop did not emit completed-turn metadata before late usage"
        );
        assert!(
            saw_late_usage_metadata,
            "fresh Codex inline loop did not patch late token usage metadata"
        );
    }

    fn assert_codex_protocol_valid(events: &[Value]) {
        let mut validator = ProtocolValidator::new();
        let host_stream = StreamPath("/host/local".to_string());
        let agent_stream = StreamPath("/agent/agent-1/instance-1".to_string());
        let agent_id = AgentId("agent-1".to_string());
        let new_agent = NewAgentPayload {
            agent_id: agent_id.clone(),
            name: "Test Agent".to_string(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp".to_string()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: agent_stream.clone(),
            activity_summary: Default::default(),
        };
        let welcome = Envelope::from_payload(
            host_stream.clone(),
            FrameKind::Welcome,
            0,
            &WelcomePayload {
                protocol_version: PROTOCOL_VERSION,
                tyde_version: Version {
                    major: 0,
                    minor: 0,
                    patch: 0,
                },
                release_version: None,
            },
        )
        .expect("serialize Welcome");
        validator
            .validate_envelope(&welcome)
            .expect("Welcome validates");
        let bootstrap = Envelope::from_payload(
            host_stream,
            FrameKind::HostBootstrap,
            1,
            &HostBootstrapPayload {
                settings: HostSettings {
                    enabled_backends: vec![BackendKind::Codex],
                    default_backend: Some(BackendKind::Codex),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
                mobile_access: MobileAccessStatePayload {
                    broker_status: MobileBrokerStatus::Disabled,
                    pairing: MobilePairingState::Idle,
                    paired_devices: vec![],
                },
                backend_setup: BackendSetupPayload { backends: vec![] },
                session_schemas: vec![],
                backend_config_schemas: vec![],
                backend_config_snapshots: vec![],
                launch_profile_catalog: Default::default(),
                sessions: vec![],
                session_list: Default::default(),
                projects: vec![],
                mcp_servers: vec![],
                skills: vec![],
                steering: vec![],
                custom_agents: vec![],
                team_preset_catalog: TeamPresetCatalog {
                    role_presets: vec![],
                    personality_traits: vec![],
                    personality_presets: vec![],
                    team_templates: vec![],
                },
                team_drafts: vec![],
                teams: vec![],
                team_members: vec![],
                team_member_bindings: vec![],
                agents: vec![new_agent.clone()],
                task_token_usages: Vec::new(),
                workflow_summaries: vec![],
                workflow_diagnostics: vec![],
                workflow_runs: vec![],
                workflow_locations: vec![],
                agents_view_preferences: None,
            },
        )
        .expect("serialize HostBootstrap");
        validator
            .validate_envelope(&bootstrap)
            .expect("HostBootstrap validates");
        let agent_bootstrap = Envelope::from_payload(
            agent_stream.clone(),
            FrameKind::AgentBootstrap,
            0,
            &AgentBootstrapPayload {
                events: vec![AgentBootstrapEvent::AgentStart(AgentStartPayload {
                    agent_id,
                    name: new_agent.name,
                    origin: new_agent.origin,
                    backend_kind: new_agent.backend_kind,
                    launch_profile_id: None,
                    workspace_roots: new_agent.workspace_roots,
                    custom_agent_id: new_agent.custom_agent_id,
                    team_id: new_agent.team_id,
                    team_member_id: new_agent.team_member_id,
                    project_id: new_agent.project_id,
                    parent_agent_id: new_agent.parent_agent_id,
                    session_id: None,
                    workflow: None,
                    created_at_ms: new_agent.created_at_ms,
                })],
                latest_output: Default::default(),
            },
        )
        .expect("serialize AgentBootstrap");
        validator
            .validate_envelope(&agent_bootstrap)
            .expect("AgentBootstrap validates");

        for (index, event) in events.iter().enumerate() {
            let chat_event: ChatEvent =
                serde_json::from_value(event.clone()).expect("emitter produced ChatEvent JSON");
            let envelope = Envelope::from_payload(
                agent_stream.clone(),
                FrameKind::ChatEvent,
                index as u64 + 1,
                &chat_event,
            )
            .expect("serialize ChatEvent");
            validator
                .validate_envelope(&envelope)
                .unwrap_or_else(|err| panic!("event {index} violates protocol: {err}"));
        }
    }

    #[derive(Clone, Debug)]
    struct RecordedSpawn {
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        agent_id: AgentId,
        native_thread_id: String,
    }

    struct RecordingSubAgentEmitter {
        next_agent_id: AtomicU64,
        spawns: tokio::sync::Mutex<Vec<RecordedSpawn>>,
        events_by_agent_id: Arc<tokio::sync::Mutex<HashMap<AgentId, Vec<ChatEvent>>>>,
    }

    impl RecordingSubAgentEmitter {
        fn new() -> Self {
            Self {
                next_agent_id: AtomicU64::new(1),
                spawns: tokio::sync::Mutex::new(Vec::new()),
                events_by_agent_id: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            }
        }

        async fn spawn_count(&self) -> usize {
            self.spawns.lock().await.len()
        }

        async fn spawns(&self) -> Vec<RecordedSpawn> {
            self.spawns.lock().await.clone()
        }

        async fn events_by_agent(&self) -> HashMap<AgentId, Vec<ChatEvent>> {
            self.events_by_agent_id.lock().await.clone()
        }

        async fn events_by_native_thread(&self) -> HashMap<String, Vec<ChatEvent>> {
            let spawns = self.spawns.lock().await;
            let events = self.events_by_agent_id.lock().await;
            spawns
                .iter()
                .map(|spawn| {
                    (
                        spawn.native_thread_id.clone(),
                        events.get(&spawn.agent_id).cloned().unwrap_or_default(),
                    )
                })
                .collect()
        }
    }

    impl SubAgentEmitter for RecordingSubAgentEmitter {
        fn on_subagent_spawned(
            &self,
            tool_use_id: String,
            name: String,
            description: String,
            agent_type: String,
            session_id_hint: Option<SessionId>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<SubAgentHandle, String>> + Send + '_>,
        > {
            Box::pin(async move {
                let agent_id = AgentId(format!(
                    "subagent-{}",
                    self.next_agent_id.fetch_add(1, Ordering::Relaxed)
                ));
                let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
                let events_by_agent_id = Arc::clone(&self.events_by_agent_id);
                let agent_id_for_events = agent_id.clone();
                tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        let mut guard = events_by_agent_id.lock().await;
                        guard
                            .entry(agent_id_for_events.clone())
                            .or_default()
                            .push(event);
                    }
                });
                live_test_log(&format!(
                    "spawn callback: tool_use_id={tool_use_id} agent_id={} name={name:?} agent_type={agent_type:?} description={description:?}",
                    agent_id.0
                ));
                let native_thread_id = session_id_hint
                    .expect("Codex child registration must carry the native thread ID")
                    .0;
                self.spawns.lock().await.push(RecordedSpawn {
                    tool_use_id,
                    name,
                    description,
                    agent_type,
                    agent_id: agent_id.clone(),
                    native_thread_id,
                });
                Ok(SubAgentHandle { event_tx, agent_id })
            })
        }
    }

    struct FailingSubAgentEmitter;

    impl SubAgentEmitter for FailingSubAgentEmitter {
        fn on_subagent_spawned(
            &self,
            _tool_use_id: String,
            _name: String,
            _description: String,
            _agent_type: String,
            _session_id_hint: Option<SessionId>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<SubAgentHandle, String>> + Send + '_>,
        > {
            Box::pin(async { Err("parent session is unavailable".to_string()) })
        }
    }

    async fn live_test_select_model(session: &CodexSession) -> Option<String> {
        let response = session
            .inner
            .rpc
            .request("model/list", json!({ "includeHidden": false }))
            .await
            .ok()?;
        let models = response
            .get("data")
            .or_else(|| response.get("models"))
            .and_then(Value::as_array)?;
        // `gpt-5.3-codex-spark` is cheaper, but the current Tyde Codex backend
        // always sends `reasoning.summary`, and Spark rejects that parameter.
        // Prefer the cheapest compatible model for live tests until that
        // backend-level incompatibility is addressed.
        let preferred = ["gpt-5.3-codex", "gpt-5.4-mini", "gpt-5.4"];
        preferred.iter().find_map(|candidate| {
            models
                .iter()
                .find(|model| {
                    model
                        .get("model")
                        .or_else(|| model.get("id"))
                        .and_then(Value::as_str)
                        == Some(*candidate)
                })
                .map(|_| (*candidate).to_string())
        })
    }

    async fn live_test_wait_for_mcp_tool(
        session: &CodexSession,
        server_name: &str,
        tool_name: &str,
        timeout: Duration,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let response = session
                .list_mcp_server_statuses()
                .await
                .expect("list mcp server statuses");

            let found = response
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|server| {
                    server.get("name").and_then(Value::as_str) == Some(server_name)
                        && server
                            .get("tools")
                            .and_then(Value::as_object)
                            .is_some_and(|tools| tools.contains_key(tool_name))
                })
                .cloned();

            if let Some(server) = found {
                return server;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for MCP tool {server_name}/{tool_name}; last response={}",
                response
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    #[test]
    #[ignore = "Live Codex test. Use --ignored and TYDE_RUN_REAL_AI_TESTS=1."]
    fn live_codex_spawn_agent_round_trip_emits_subagent_callbacks() {
        live_test_log("starting live codex sub-agent test");
        if !live_codex_tests_enabled() {
            skip_live_codex_test();
            return;
        }
        live_test_log("preflight: live Codex test env set");

        let codex_available = std::process::Command::new("codex")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        live_test_log(&format!(
            "preflight: codex --version available={codex_available}"
        ));
        if !codex_available {
            eprintln!("Skipping live Codex test (`codex` CLI is not available).");
            return;
        }

        if let Ok(out) = std::process::Command::new("codex")
            .args(["login", "status"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            let logged_in = out.status.success() && combined.contains("logged in");
            let explicitly_not_logged_in = combined.contains("not logged in");
            if live_test_verbose() {
                live_test_log(&format!(
                    "preflight: codex login status exit={} stdout={:?} stderr={:?}",
                    out.status, stdout, stderr
                ));
            } else {
                live_test_log(&format!(
                    "preflight: codex login status exit={} logged_in={} explicitly_not_logged_in={}",
                    out.status, logged_in, explicitly_not_logged_in
                ));
            }
            if explicitly_not_logged_in || (!logged_in && out.status.success()) {
                eprintln!(
                    "Skipping live Codex test (`codex login status` indicates no active login)."
                );
                return;
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let workspace = std::env::temp_dir().join(format!("tyde-codex-live-subagent-{suffix}"));
            std::fs::create_dir_all(&workspace).expect("create temp workspace");
            std::fs::write(workspace.join("hello.txt"), "hello from live test\n")
                .expect("seed workspace file");
            live_test_log(&format!("workspace prepared: {}", workspace.display()));

            let workspace_roots = vec![workspace.to_string_lossy().to_string()];
            live_test_log("spawning CodexSession");
            let (session, mut event_rx) = CodexSession::spawn(
                &workspace_roots,
                None,
                &[],
                None,
                BackendAccessMode::Unrestricted,
            )
            .await
                .expect("spawn codex session");
            live_test_log("CodexSession spawned");
            let emitter = Arc::new(RecordingSubAgentEmitter::new());
            session
                .set_subagent_emitter(emitter.clone() as Arc<dyn SubAgentEmitter>)
                .await
                .expect("attach Codex sub-agent emitter");
            live_test_log("sub-agent emitter attached");

            let prompt = r#"Test harness: you MUST call spawn_agent exactly once and then wait_agent.
1) spawn_agent: use agent_type "worker", message "Read hello.txt and reply exactly: LIVE_SUBAGENT_OK".
2) wait_agent: wait for that spawned agent id.
3) Return a one-line summary.
If you skip spawn_agent or wait_agent, this test fails."#;
            live_test_log(&format!("sending prompt: {prompt}"));

            session
                .command_handle()
                .execute(SessionCommand::SendMessage {
                    message: prompt.to_string(),
                    images: None,
                })
                .await
                .expect("send message");
            live_test_log("prompt sent; waiting for completion callback");

            let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
            let idle_grace = Duration::from_secs(8);
            let mut poll_ticks: u64 = 0;
            let mut tool_request_count: u64 = 0;
            let mut tool_execution_completed_count: u64 = 0;
            let mut stream_end_count: u64 = 0;
            let mut last_stream_end_preview: Option<String> = None;
            let mut seen_typing_true = false;
            let mut last_typing_status: Option<bool> = None;
            let mut idle_edge_at: Option<tokio::time::Instant> = None;
            let mut event_stream_closed = false;
            while tokio::time::Instant::now() < deadline {
                poll_ticks = poll_ticks.saturating_add(1);
                if let Some(idle_at) = idle_edge_at
                    && tokio::time::Instant::now().duration_since(idle_at) >= idle_grace {
                        live_test_log(&format!(
                            "idle edge grace elapsed ({:?}); exiting wait loop",
                            idle_grace
                        ));
                        break;
                    }

                match tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await {
                    Ok(Some(event)) => {
                        if live_test_verbose() {
                            live_test_log(&format!("event(raw): {event}"));
                        } else {
                            live_test_log(&format!("event: {}", summarize_live_event(&event)));
                        }
                        if event.get("kind").and_then(Value::as_str) == Some("Error") {
                            let spawn_count_now = emitter.spawn_count().await;
                            live_test_log(&format!(
                                "error event encountered; spawn_count={spawn_count_now}"
                            ));
                            panic!("Codex emitted error during live subagent test: {event}");
                        }
                        match event.get("kind").and_then(Value::as_str) {
                            Some("ToolRequest") => {
                                tool_request_count = tool_request_count.saturating_add(1);
                            }
                            Some("ToolExecutionCompleted") => {
                                tool_execution_completed_count =
                                    tool_execution_completed_count.saturating_add(1);
                            }
                            Some("StreamEnd") => {
                                stream_end_count = stream_end_count.saturating_add(1);
                                let content = event
                                    .get("data")
                                    .and_then(|d| d.get("message"))
                                    .and_then(|m| m.get("content"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if !content.is_empty() {
                                    let preview = if content.len() > 120 {
                                        format!("{}...", &content[..120])
                                    } else {
                                        content.to_string()
                                    };
                                    last_stream_end_preview = Some(preview);
                                }
                            }
                            Some("TypingStatusChanged") => {
                                let typing =
                                    event.get("data").and_then(Value::as_bool).unwrap_or(false);
                                if typing {
                                    seen_typing_true = true;
                                    idle_edge_at = None;
                                }
                                if matches!(last_typing_status, Some(true)) && !typing {
                                    idle_edge_at = Some(tokio::time::Instant::now());
                                    live_test_log(
                                        "detected TypingStatusChanged true->false (model idle edge)",
                                    );
                                }
                                last_typing_status = Some(typing);
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => {
                        event_stream_closed = true;
                        live_test_log("event stream closed before completion");
                        break;
                    }
                    Err(_) => {
                        if poll_ticks.is_multiple_of(10) {
                            live_test_log(&format!(
                                "still waiting... elapsed={}s",
                                poll_ticks.saturating_mul(2)
                            ));
                        }
                    }
                }
            }

            let spawn_count = emitter.spawn_count().await;
            let wait_diagnostics = format!(
                "seen_typing_true={} last_typing_status={:?} idle_edge_observed={} tool_requests={} tool_execution_completed_events={} stream_ends={} last_stream_end_preview={:?} event_stream_closed={} poll_ticks={}",
                seen_typing_true,
                last_typing_status,
                idle_edge_at.is_some(),
                tool_request_count,
                tool_execution_completed_count,
                stream_end_count,
                last_stream_end_preview,
                event_stream_closed,
                poll_ticks
            );
            live_test_log(&format!("post-run counts: spawn_count={spawn_count}"));
            live_test_log(&format!("wait diagnostics: {wait_diagnostics}"));
            assert!(
                spawn_count > 0,
                "Expected at least one sub-agent spawn callback from live Codex run. diagnostics={wait_diagnostics}"
            );
            let spawns = emitter.spawns.lock().await;
            for spawn in spawns.iter() {
                live_test_log(&format!(
                    "recorded spawn: tool_use_id={} agent_id={} name={:?} agent_type={:?} description={:?}",
                    spawn.tool_use_id,
                    spawn.agent_id,
                    spawn.name,
                    spawn.agent_type,
                    spawn.description
                ));
            }
            assert!(
                spawns.iter().any(|s| !s.tool_use_id.is_empty()),
                "spawn callback should include a tool_use_id. diagnostics={wait_diagnostics}"
            );
            assert!(
                spawns.iter().any(|s| !s.agent_id.0.is_empty()),
                "spawn callback should include a non-empty agent_id. diagnostics={wait_diagnostics}"
            );
            assert!(
                spawns.iter().any(|s| !s.name.trim().is_empty()),
                "spawn callback should include a display name. diagnostics={wait_diagnostics}"
            );
            assert!(
                spawns
                    .iter()
                    .any(|s| !s.description.is_empty() || !s.agent_type.is_empty()),
                "spawn callback should include description or agent type metadata. diagnostics={wait_diagnostics}"
            );
            let events_by_agent = emitter.events_by_agent().await;
            for (agent_id, events) in &events_by_agent {
                live_test_log(&format!(
                    "sub-agent event stream: agent_id={} events={}",
                    agent_id,
                    events.len()
                ));
            }
            assert!(
                events_by_agent.values().any(|events| {
                    events
                        .iter()
                        .any(|event| matches!(event, ChatEvent::StreamEnd(_)))
                }),
                "sub-agent event stream should include a StreamEnd. diagnostics={wait_diagnostics}"
            );
            assert!(
                events_by_agent.values().any(|events| {
                    events
                        .iter()
                        .any(|event| matches!(event, ChatEvent::ToolRequest(_)))
                }),
                "sub-agent event stream should include at least one ToolRequest. diagnostics={wait_diagnostics}"
            );
            drop(spawns);
            live_test_log("shutting down session");
            session.shutdown().await;

            let _ = std::fs::remove_dir_all(&workspace);
            live_test_log("workspace removed; final assertions");

            live_test_log("live codex sub-agent test completed successfully");
        });
    }

    #[test]
    #[ignore = "Live Codex test. Use --ignored and TYDE_RUN_REAL_AI_TESTS=1."]
    fn live_codex_session_can_call_tyde_debug_mcp_tool_via_rpc() {
        live_test_log("starting live codex MCP RPC test");
        if !live_codex_tests_enabled() {
            skip_live_codex_test();
            return;
        }

        let codex_available = std::process::Command::new("codex")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !codex_available {
            eprintln!("Skipping live Codex test (`codex` CLI is not available).");
            return;
        }

        if let Ok(out) = std::process::Command::new("codex")
            .args(["login", "status"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            let logged_in = out.status.success() && combined.contains("logged in");
            let explicitly_not_logged_in = combined.contains("not logged in");
            if explicitly_not_logged_in || (!logged_in && out.status.success()) {
                eprintln!(
                    "Skipping live Codex test (`codex login status` indicates no active login)."
                );
                return;
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let workspace = std::env::temp_dir().join(format!("tyde-codex-live-mcp-rpc-{suffix}"));
            std::fs::create_dir_all(&workspace).expect("create temp workspace");

            let debug_mcp =
                crate::debug_mcp::start_server(None).expect("start Tyde debug MCP server");
            live_test_log(&format!("debug MCP URL: {}", debug_mcp.url));

            let workspace_roots = vec![workspace.to_string_lossy().to_string()];
            let startup_mcp_servers = vec![StartupMcpServer {
                name: "tyde-debug".to_string(),
                transport: StartupMcpTransport::Http {
                    url: debug_mcp.url.clone(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            }];

            let (session, _event_rx) = CodexSession::spawn(
                &workspace_roots,
                None,
                &startup_mcp_servers,
                None,
                BackendAccessMode::Unrestricted,
            )
            .await
            .expect("spawn codex session");

            let server = live_test_wait_for_mcp_tool(
                &session,
                "tyde-debug",
                "tyde_dev_instance_list",
                Duration::from_secs(30),
            )
            .await;
            live_test_log(&format!("mcp server inventory: {server}"));

            let tools = server
                .get("tools")
                .and_then(Value::as_object)
                .expect("server tools map");
            assert!(
                tools.contains_key("tyde_dev_instance_list"),
                "expected tyde_dev_instance_list in MCP inventory: {server}"
            );

            let response = session
                .call_mcp_tool("tyde-debug", "tyde_dev_instance_list", None, None)
                .await
                .expect("call tyde_dev_instance_list");
            live_test_log(&format!("mcp tool response: {response}"));

            assert_ne!(
                response.get("isError").and_then(Value::as_bool),
                Some(true),
                "expected successful MCP tool call: {response}"
            );

            let content = response
                .get("content")
                .and_then(Value::as_array)
                .expect("mcp tool content array");
            assert!(
                content.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("text")
                        && item
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.trim_start().starts_with('['))
                }),
                "expected JSON text content from tyde_dev_instance_list: {response}"
            );

            session.shutdown().await;
            let _ = std::fs::remove_dir_all(&workspace);
        });
    }

    #[test]
    #[ignore = "Live Codex test. Use --ignored and TYDE_RUN_REAL_AI_TESTS=1."]
    fn live_codex_session_can_call_tyde_agent_control_mcp_tool_via_rpc() {
        live_test_log("starting live codex agent-control MCP RPC test");
        if !live_codex_tests_enabled() {
            skip_live_codex_test();
            return;
        }

        let codex_available = std::process::Command::new("codex")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !codex_available {
            eprintln!("Skipping live Codex test (`codex` CLI is not available).");
            return;
        }

        if let Ok(out) = std::process::Command::new("codex")
            .args(["login", "status"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            let logged_in = out.status.success() && combined.contains("logged in");
            let explicitly_not_logged_in = combined.contains("not logged in");
            if explicitly_not_logged_in || (!logged_in && out.status.success()) {
                eprintln!(
                    "Skipping live Codex test (`codex login status` indicates no active login)."
                );
                return;
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let workspace =
                std::env::temp_dir().join(format!("tyde-codex-live-agent-control-{suffix}"));
            std::fs::create_dir_all(&workspace).expect("create temp workspace");

            let host = crate::host::spawn_host_with_mock_backend(
                workspace.join("sessions.json"),
                workspace.join("projects.json"),
                workspace.join("settings.json"),
            )
            .expect("spawn mock host");
            let agent_control = crate::agent_control_mcp::start_server(None, host)
                .expect("start Tyde agent-control MCP server");
            live_test_log(&format!("agent-control MCP URL: {}", agent_control.url));

            let workspace_roots = vec![workspace.to_string_lossy().to_string()];
            let startup_mcp_servers = vec![StartupMcpServer {
                name: "tyde-agent-control".to_string(),
                transport: StartupMcpTransport::Http {
                    url: agent_control.url.clone(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            }];

            let (session, _event_rx) = CodexSession::spawn(
                &workspace_roots,
                None,
                &startup_mcp_servers,
                None,
                BackendAccessMode::Unrestricted,
            )
            .await
            .expect("spawn codex session");

            let server = live_test_wait_for_mcp_tool(
                &session,
                "tyde-agent-control",
                "tyde_list_agents",
                Duration::from_secs(30),
            )
            .await;
            live_test_log(&format!("agent-control inventory: {server}"));

            let tools = server
                .get("tools")
                .and_then(Value::as_object)
                .expect("server tools map");
            assert!(
                tools.contains_key("tyde_list_agents"),
                "expected tyde_list_agents in MCP inventory: {server}"
            );
            assert!(
                tools.contains_key("tyde_spawn_agent"),
                "expected tyde_spawn_agent in MCP inventory: {server}"
            );

            let response = session
                .call_mcp_tool("tyde-agent-control", "tyde_list_agents", None, None)
                .await
                .expect("call tyde_list_agents");
            live_test_log(&format!("agent-control tool response: {response}"));

            assert_ne!(
                response.get("isError").and_then(Value::as_bool),
                Some(true),
                "expected successful MCP tool call: {response}"
            );

            let content = response
                .get("content")
                .and_then(Value::as_array)
                .expect("mcp tool content array");
            assert!(
                content.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("text")
                        && item
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.trim_start().starts_with('['))
                }),
                "expected JSON text content from tyde_list_agents: {response}"
            );

            session.shutdown().await;
            let _ = std::fs::remove_dir_all(&workspace);
        });
    }

    #[test]
    #[ignore = "Live Codex test. Use --ignored and TYDE_RUN_REAL_AI_TESTS=1."]
    fn live_codex_model_emits_mcp_tool_call_for_tyde_debug_tool() {
        live_test_log("starting live codex model-driven MCP test");
        if !live_codex_tests_enabled() {
            skip_live_codex_test();
            return;
        }

        let codex_available = std::process::Command::new("codex")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !codex_available {
            eprintln!("Skipping live Codex test (`codex` CLI is not available).");
            return;
        }

        if let Ok(out) = std::process::Command::new("codex")
            .args(["login", "status"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
            let logged_in = out.status.success() && combined.contains("logged in");
            let explicitly_not_logged_in = combined.contains("not logged in");
            if explicitly_not_logged_in || (!logged_in && out.status.success()) {
                eprintln!(
                    "Skipping live Codex test (`codex login status` indicates no active login)."
                );
                return;
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let workspace =
                std::env::temp_dir().join(format!("tyde-codex-live-mcp-model-{suffix}"));
            std::fs::create_dir_all(&workspace).expect("create temp workspace");

            let debug_mcp =
                crate::debug_mcp::start_server(None).expect("start Tyde debug MCP server");
            let workspace_roots = vec![workspace.to_string_lossy().to_string()];
            let startup_mcp_servers = vec![StartupMcpServer {
                name: "tyde-debug".to_string(),
                transport: StartupMcpTransport::Http {
                    url: debug_mcp.url.clone(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            }];

            let (session, mut event_rx) =
                CodexSession::spawn(
                    &workspace_roots,
                    None,
                    &startup_mcp_servers,
                    None,
                    BackendAccessMode::Unrestricted,
                )
                .await
                    .expect("spawn codex session");

            let _ = live_test_wait_for_mcp_tool(
                &session,
                "tyde-debug",
                "tyde_dev_instance_list",
                Duration::from_secs(30),
            )
            .await;

            if let Some(model) = live_test_select_model(&session).await {
                live_test_log(&format!("using live Codex test model: {model}"));
                session
                    .command_handle()
                    .execute(SessionCommand::UpdateSettings {
                        settings: json!({ "model": model }),
                        persist: false,
                    })
                    .await
                    .expect("set live codex test model");
            } else {
                live_test_log("no preferred live Codex test model found; using session default");
            }

            let prompt = r#"Test harness: call the MCP tool `tyde_dev_instance_list` exactly once, then reply with the number of instances it returned as a single line like `instances=0`.
Do not describe the tool, and do not skip the tool call."#;

            session
                .command_handle()
                .execute(SessionCommand::SendMessage {
                    message: prompt.to_string(),
                    images: None,
                })
                .await
                .expect("send message");

            let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
            let mut saw_mcp_tool_request = false;
            let mut saw_mcp_tool_completion = false;
            let mut final_message: Option<String> = None;

            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await {
                    Ok(Some(event)) => {
                        if live_test_verbose() {
                            live_test_log(&format!("event(raw): {event}"));
                        } else {
                            live_test_log(&format!("event: {}", summarize_live_event(&event)));
                        }

                        if event.get("kind").and_then(Value::as_str) == Some("Error") {
                            panic!("Codex emitted error during live MCP test: {event}");
                        }

                        match event.get("kind").and_then(Value::as_str) {
                            Some("ToolRequest") => {
                                if event
                                    .get("data")
                                    .and_then(|d| d.get("tool_name"))
                                    .and_then(Value::as_str)
                                    == Some("tyde_dev_instance_list")
                                {
                                    saw_mcp_tool_request = true;
                                }
                            }
                            Some("ToolExecutionCompleted") => {
                                if event
                                    .get("data")
                                    .and_then(|d| d.get("tool_name"))
                                    .and_then(Value::as_str)
                                    == Some("tyde_dev_instance_list")
                                {
                                    saw_mcp_tool_completion = true;
                                }
                            }
                            Some("StreamEnd") => {
                                final_message = event
                                    .get("data")
                                    .and_then(|d| d.get("message"))
                                    .and_then(|m| m.get("content"))
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string);
                                if saw_mcp_tool_request && saw_mcp_tool_completion {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }

            assert!(
                saw_mcp_tool_request,
                "expected Codex to emit ToolRequest for tyde_dev_instance_list; final_message={final_message:?}"
            );
            assert!(
                saw_mcp_tool_completion,
                "expected Codex to emit ToolExecutionCompleted for tyde_dev_instance_list; final_message={final_message:?}"
            );
            assert!(
                final_message
                    .as_deref()
                    .is_some_and(|message| message.trim().starts_with("instances=")),
                "expected final message summarizing instance count, got {final_message:?}"
            );

            session.shutdown().await;
            let _ = std::fs::remove_dir_all(&workspace);
        });
    }

    #[test]
    fn conflicting_collab_sender_blocks_child_activity_registration() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            {
                let mut state = inner.state.lock().await;
                state.thread_id = "parent-thread".to_string();
            }
            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "parent-thread",
                        "item": {
                            "type": "collabAgentToolCall",
                            "id": "conflicting-spawn",
                            "senderThreadId": "other-parent",
                            "receiverThreadId": "child-thread",
                            "prompt": "inspect",
                            "receiverAgentType": "worker"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "parent-thread",
                        "item": {
                            "type": "subAgentActivity",
                            "kind": "started",
                            "agentThreadId": "child-thread",
                            "agentPath": "/root/worker"
                        }
                    }),
                )
                .await;
            assert!(
                !inner
                    .state
                    .lock()
                    .await
                    .subagent_streams
                    .contains_key("child-thread")
            );
            let events = drain_events(&mut parent_rx);
            assert!(events.iter().any(|event| {
                event
                    .pointer("/data/content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| {
                        content.contains("names sender 'other-parent'")
                            && content.contains("child thread 'child-thread'")
                    })
            }));
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn child_relay_registration_failure_is_a_parent_error_without_a_phantom_child() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            {
                let mut state = inner.state.lock().await;
                state.thread_id = "parent-thread".to_string();
                state.subagent_emitter = Some(Arc::new(FailingSubAgentEmitter));
            }
            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "parent-thread",
                        "item": {
                            "type": "collabAgentToolCall",
                            "id": "019f60f0-7a69-73f0-9ab3-7ddc24062e30",
                            "senderThreadId": "parent-thread",
                            "receiverThreadId": "native-quick-child-thread",
                            "prompt": "reply exactly QUICK_DONE",
                            "receiverAgentType": "sub-agent",
                            "receiverAgentName": "/root/quick_child"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "parent-thread",
                        "item": {
                            "type": "subAgentActivity",
                            "id": "activity-quick-child",
                            "kind": "started",
                            "agentThreadId": "native-quick-child-thread",
                            "agentPath": "/root/quick_child"
                        }
                    }),
                )
                .await;

            let state = inner.state.lock().await;
            assert!(
                !state
                    .subagent_streams
                    .contains_key("native-quick-child-thread")
            );
            assert!(
                !state
                    .registering_subagent_threads
                    .contains("native-quick-child-thread")
            );
            drop(state);
            let events = drain_events(&mut parent_rx);
            assert!(events.iter().any(|event| {
                event
                    .pointer("/data/content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| {
                        content.contains("Codex child relay registration failed")
                            && content.contains("parent session is unavailable")
                    })
            }));
            assert!(events.iter().any(|event| {
                event.get("kind").and_then(Value::as_str) == Some("TypingStatusChanged")
                    && event.get("data").and_then(Value::as_bool) == Some(false)
            }));
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn connection_scoped_notifications_preserve_global_semantics_without_thread_id() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            inner
                .handle_notification(
                    "mcpServer/startupStatus/updated",
                    &json!({"serverName": "tyde-agent-control", "status": "ready"}),
                )
                .await;
            inner
                .handle_notification("account/rateLimits/updated", &json!({"rateLimits": []}))
                .await;
            assert!(drain_events(&mut parent_rx).is_empty());

            inner
                .handle_notification(
                    "error",
                    &json!({"message": "connection failed", "fatal": true}),
                )
                .await;
            let events = drain_events(&mut parent_rx);
            assert!(events.iter().any(|event| {
                event.pointer("/data/content").and_then(Value::as_str) == Some("connection failed")
            }));
            assert!(events.iter().any(|event| {
                event.get("kind").and_then(Value::as_str) == Some("TypingStatusChanged")
                    && event.get("data").and_then(Value::as_bool) == Some(false)
            }));
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn thread_scoped_notification_without_thread_id_is_an_ownership_invariant() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            inner
                .handle_notification("turn/started", &json!({"turn": {"id": "missing-owner"}}))
                .await;
            let events = drain_events(&mut parent_rx);
            assert!(events.iter().any(|event| {
                event
                    .pointer("/data/content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| {
                        content.contains("thread-scoped notification 'turn/started'")
                            && content.contains("<missing>")
                    })
            }));
            assert!(
                !events.iter().any(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("StreamStart")
                })
            );
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn unknown_child_thread_surfaces_ownership_error_without_parent_content() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({
                        "threadId": "unknown-child-thread",
                        "itemId": "unknown-message",
                        "delta": "must not reach parent"
                    }),
                )
                .await;
            let events = drain_events(&mut parent_rx);
            assert!(events.iter().any(|event| {
                event.get("kind").and_then(Value::as_str) == Some("MessageAdded")
                    && event
                        .pointer("/data/content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| content.contains("ownership invariant failed"))
            }));
            assert!(!events.iter().any(|event| {
                event.pointer("/data/content").and_then(Value::as_str)
                    == Some("must not reach parent")
            }));
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn subagent_thread_notifications_route_to_subagent_channel_not_parent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();

            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-1").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-sub-1",
                        "turn": { "id": "turn-sub-1" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "thread-sub-1",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-1"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "thread-sub-1",
                        "item": {
                            "type": "commandExecution",
                            "id": "cmd-sub-1",
                            "command": "cat hello.txt",
                            "cwd": "/tmp"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-1",
                        "item": {
                            "type": "commandExecution",
                            "id": "cmd-sub-1",
                            "exitCode": 0,
                            "aggregatedOutput": "LIVE_SUBAGENT_OK"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-1",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-1",
                            "text": "LIVE_SUBAGENT_OK"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({
                        "threadId": "thread-sub-1",
                        "turn": {
                            "id": "turn-sub-1",
                            "status": "completed"
                        }
                    }),
                )
                .await;

            let parent_events = drain_events(&mut parent_rx);
            assert!(
                parent_events.is_empty(),
                "sub-agent thread notifications should not emit into parent conversation: {parent_events:?}"
            );

            let subagent_events = drain_events(&mut subagent_rx);
            assert_eq!(
                event_kinds(&subagent_events),
                vec![
                    "TypingStatusChanged",
                    "StreamStart",
                    "ToolRequest",
                    "ToolExecutionCompleted",
                    "StreamEnd",
                    "TypingStatusChanged"
                ],
                "child routing must retain the provider message boundary through its tool lifecycle: {subagent_events:?}"
            );
            assert_eq!(
                subagent_events[1]
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some("msg-sub-1")
            );
            assert_eq!(
                subagent_events[2]
                    .pointer("/data/tool_call_id")
                    .and_then(Value::as_str),
                Some("cmd-sub-1")
            );
            assert_eq!(
                subagent_events[2]
                    .pointer("/data/tool_name")
                    .and_then(Value::as_str),
                Some("run_command")
            );
            assert_eq!(
                subagent_events[3]
                    .pointer("/data/tool_call_id")
                    .and_then(Value::as_str),
                Some("cmd-sub-1")
            );
            assert_eq!(
                subagent_events[3]
                    .pointer("/data/tool_name")
                    .and_then(Value::as_str),
                Some("run_command")
            );
            assert_eq!(
                subagent_events[4]
                    .pointer("/data/message/message_id")
                    .and_then(Value::as_str),
                Some("msg-sub-1")
            );
            assert_eq!(
                subagent_events[4]
                    .pointer("/data/message/content")
                    .and_then(Value::as_str),
                Some("LIVE_SUBAGENT_OK")
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn parent_interruption_does_not_cancel_idle_completed_child_turn() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, _parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-idle").await;
            inner
                .handle_notification(
                    "turn/started",
                    &json!({"threadId":"thread-sub-idle","turn":{"id":"turn-sub-idle"}}),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({"threadId":"thread-sub-idle","item":{"id":"message-idle","type":"agentMessage","text":"done"}}),
                )
                .await;
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({"threadId":"thread-sub-idle","turn":{"id":"turn-sub-idle","status":"completed"}}),
                )
                .await;
            let completed_events = drain_events(&mut subagent_rx);
            assert!(
                completed_events.iter().any(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("StreamEnd")
                        && event
                            .get("data")
                            .and_then(|data| data.get("message"))
                            .and_then(|message| message.get("content"))
                            .and_then(Value::as_str)
                            == Some("done")
                }),
                "the child must retain its clean terminal content before the parent ends: {completed_events:?}"
            );
            assert!(
                !completed_events.iter().any(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("OperationCancelled")
                }),
                "a clean child turn must not be labelled cancelled before parent teardown: {completed_events:?}"
            );

            inner
                .handle_notification(
                    "turn/completed",
                    &json!({"threadId":"thread-parent","turn":{"id":"parent-turn","status":"interrupted"}}),
                )
                .await;
            let events = drain_events(&mut subagent_rx);
            assert!(
                !events.iter().any(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("OperationCancelled")
                }),
                "a completed child must never be retroactively cancelled: {events:?}"
            );
            let state = inner.state.lock().await;
            assert!(!state.subagent_streams.contains_key("thread-sub-idle"));
            assert!(state.completed_subagent_streams.contains_key("thread-sub-idle"));
            drop(state);
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn abandoning_parent_terminally_cancels_live_subagents() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, _parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-cancel").await;
            inner
                .update_codex_subagent_stream("thread-sub-cancel", |stream| {
                    stream.active_turn_id = Some("turn-sub-cancel".to_string());
                    stream.current_message_id = Some(ChatMessageId("child-message".to_string()));
                })
                .await
                .expect("active child stream");
            let emitter = inner
                .codex_subagent_emitter("thread-sub-cancel")
                .await
                .expect("sub-agent emitter");
            emitter.typing_status_changed(true);
            emitter.stream_start_with_id(
                ChatMessageId("child-message".to_string()),
                AgentName(CODEX_AGENT_NAME),
                Some("test-model"),
            );
            emitter.stream_delta_with_id(ChatMessageId("child-message".to_string()), "working");

            inner.complete_all_codex_subagents().await;

            let events = drain_events(&mut subagent_rx);
            assert_eq!(
                event_kinds(&events),
                vec![
                    "TypingStatusChanged",
                    "StreamStart",
                    "StreamDelta",
                    "MessageAdded",
                    "OperationCancelled",
                    "TypingStatusChanged"
                ]
            );
            assert_eq!(
                events.last().and_then(|event| event.get("data")),
                Some(&Value::Bool(false))
            );
            assert!(inner.state.lock().await.subagent_streams.is_empty());
        });
    }

    #[test]
    fn root_late_token_usage_after_turn_completed_updates_message_metadata() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-test",
                        "turn": { "id": "turn-root-late-usage" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-root-late-usage",
                            "text": "root done"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({
                        "threadId": "thread-test",
                        "turn": {
                            "id": "turn-root-late-usage",
                            "status": "completed"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "thread/tokenUsage/updated",
                    &json!({
                        "threadId": "thread-test",
                        "turnId": "turn-root-late-usage",
                        "tokenUsage": {
                            "input_tokens": 19,
                            "output_tokens": 8,
                            "total_tokens": 27
                        }
                    }),
                )
                .await;

            let events = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&events),
                vec![
                    "TypingStatusChanged",
                    "StreamStart",
                    "StreamEnd",
                    "MessageMetadataUpdated",
                    "TypingStatusChanged",
                    "MessageMetadataUpdated"
                ],
                "late usage must patch the completed provider item without creating another message: {events:?}"
            );
            let response_message_ids = events
                .iter()
                .filter_map(|event| match event.get("kind").and_then(Value::as_str) {
                    Some("StreamStart") | Some("MessageMetadataUpdated") => event
                        .pointer("/data/message_id")
                        .and_then(Value::as_str),
                    Some("StreamEnd") => event
                        .pointer("/data/message/message_id")
                        .and_then(Value::as_str),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                response_message_ids,
                vec![
                    "msg-root-late-usage",
                    "msg-root-late-usage",
                    "msg-root-late-usage",
                    "msg-root-late-usage"
                ],
                "the provider item start, end, initial metadata, and late metadata patch must retain one immutable message identity"
            );
            let metadata_updates = events
                .iter()
                .filter(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("MessageMetadataUpdated")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                metadata_updates.len(),
                2,
                "expected initial metadata plus late token usage patch: {events:?}"
            );
            let known_updates = metadata_updates
                .iter()
                .filter(|event| {
                    event
                        .pointer("/data/token_usage/turn/usage/total_tokens")
                        .and_then(Value::as_u64)
                        == Some(27)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                known_updates.len(),
                1,
                "late root usage should emit exactly one known metadata patch: {events:?}"
            );
            assert_eq!(
                known_updates[0]
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some("msg-root-late-usage")
            );
            {
                let state = inner.state.lock().await;
                assert!(
                    state.token_usage_by_turn.is_empty(),
                    "late usage should not remain orphaned in root state"
                );
                assert!(
                    state.completed_message_metadata_by_turn.is_empty(),
                    "late usage target should be consumed after patching"
                );
            }

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn subagent_stream_end_uses_token_usage_reported_before_completion() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-usage").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-sub-usage",
                        "turn": { "id": "turn-sub-usage" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "thread/tokenUsage/updated",
                    &json!({
                        "threadId": "thread-sub-usage",
                        "turnId": "turn-sub-usage",
                        "tokenUsage": {
                            "input_tokens": 8,
                            "output_tokens": 4,
                            "total_tokens": 12
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-usage",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-usage",
                            "text": "sub done"
                        }
                    }),
                )
                .await;

            let parent_events = drain_events(&mut parent_rx);
            assert!(
                parent_events.is_empty(),
                "child token usage must not emit into parent stream: {parent_events:?}"
            );

            let subagent_events = drain_events(&mut subagent_rx);
            let stream_end = subagent_events
                .iter()
                .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                .expect("sub-agent StreamEnd");
            assert_eq!(
                stream_end
                    .pointer("/data/message/token_usage/request/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(12)
            );
            assert_eq!(
                stream_end
                    .pointer("/data/message/token_usage/turn/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(12)
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn subagent_empty_token_usage_becomes_explicitly_unavailable() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-empty-usage").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-sub-empty-usage",
                        "turn": { "id": "turn-sub-empty-usage" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "thread/tokenUsage/updated",
                    &json!({
                        "threadId": "thread-sub-empty-usage",
                        "turnId": "turn-sub-empty-usage",
                        "tokenUsage": {}
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-empty-usage",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-empty-usage",
                            "text": "sub done"
                        }
                    }),
                )
                .await;

            let parent_events = drain_events(&mut parent_rx);
            assert!(
                parent_events.is_empty(),
                "empty child token usage must not emit into parent stream: {parent_events:?}"
            );

            let subagent_events = drain_events(&mut subagent_rx);
            let stream_end = subagent_events
                .iter()
                .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                .expect("sub-agent StreamEnd");
            assert_eq!(
                stream_end
                    .pointer("/data/message/token_usage/turn/kind")
                    .and_then(Value::as_str),
                Some("unavailable")
            );
            assert_eq!(
                stream_end
                    .pointer("/data/message/token_usage/turn/reason")
                    .and_then(Value::as_str),
                Some("backend_did_not_report")
            );
            assert!(
                stream_end
                    .pointer("/data/message/token_usage/turn/usage/total_tokens")
                    .is_none(),
                "empty usage must not be emitted as Known(0): {stream_end:?}"
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn subagent_late_token_usage_after_removal_patches_child_not_parent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-removed-usage").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-sub-removed-usage",
                        "turn": { "id": "turn-sub-removed-usage" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-removed-usage",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-removed-usage",
                            "text": "sub done"
                        }
                    }),
                )
                .await;
            inner
                .complete_codex_subagent_if_needed("thread-sub-removed-usage")
                .await;
            inner
                .handle_notification(
                    "thread/tokenUsage/updated",
                    &json!({
                        "threadId": "thread-sub-removed-usage",
                        "turnId": "turn-sub-removed-usage",
                        "tokenUsage": {
                            "input_tokens": 11,
                            "output_tokens": 6,
                            "total_tokens": 17
                        }
                    }),
                )
                .await;

            let parent_events = drain_events(&mut parent_rx);
            assert!(
                parent_events.is_empty(),
                "removed child token usage must not emit into parent stream: {parent_events:?}"
            );
            {
                let state = inner.state.lock().await;
                assert!(
                    state.token_usage_by_turn.is_empty(),
                    "removed child token usage must not pollute root pending usage"
                );
            }

            let subagent_events = drain_events(&mut subagent_rx);
            let stream_end = subagent_events
                .iter()
                .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                .expect("sub-agent StreamEnd");
            assert_eq!(
                stream_end
                    .pointer("/data/message/token_usage/turn/reason")
                    .and_then(Value::as_str),
                Some("backend_did_not_report")
            );

            let metadata_update = subagent_events
                .iter()
                .find(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("MessageMetadataUpdated")
                })
                .expect("sub-agent MessageMetadataUpdated");
            assert_eq!(
                metadata_update
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some("msg-sub-removed-usage")
            );
            assert_eq!(
                metadata_update
                    .pointer("/data/token_usage/turn/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(17)
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn subagent_turn_completed_usage_after_removal_patches_child_not_parent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-removed-turn").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-sub-removed-turn",
                        "turn": { "id": "turn-sub-removed-turn" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-removed-turn",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-removed-turn",
                            "text": "sub done"
                        }
                    }),
                )
                .await;
            inner
                .complete_codex_subagent_if_needed("thread-sub-removed-turn")
                .await;
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({
                        "threadId": "thread-sub-removed-turn",
                        "turn": {
                            "id": "turn-sub-removed-turn",
                            "status": "completed",
                            "usage": {
                                "input_tokens": 23,
                                "output_tokens": 5,
                                "total_tokens": 28
                            }
                        }
                    }),
                )
                .await;

            let parent_events = drain_events(&mut parent_rx);
            assert!(
                parent_events.is_empty(),
                "removed child turn usage must not emit into parent stream: {parent_events:?}"
            );
            {
                let state = inner.state.lock().await;
                assert!(
                    state.token_usage_by_turn.is_empty(),
                    "removed child turn usage must not pollute root pending usage"
                );
            }

            let subagent_events = drain_events(&mut subagent_rx);
            let metadata_update = subagent_events
                .iter()
                .find(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("MessageMetadataUpdated")
                })
                .expect("sub-agent MessageMetadataUpdated");
            assert_eq!(
                metadata_update
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some("msg-sub-removed-turn")
            );
            assert_eq!(
                metadata_update
                    .pointer("/data/token_usage/turn/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(28)
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn subagent_turn_completed_usage_updates_subagent_message_metadata() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel::<Value>();
            attach_test_codex_subagent(&inner, subagent_tx, "thread-sub-late-usage").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({
                        "threadId": "thread-sub-late-usage",
                        "turn": { "id": "turn-sub-late-usage" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-sub-late-usage",
                        "item": {
                            "type": "agentMessage",
                            "id": "msg-sub-late-usage",
                            "text": "sub done"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({
                        "threadId": "thread-sub-late-usage",
                        "turn": {
                            "id": "turn-sub-late-usage",
                            "status": "completed",
                            "usage": {
                                "input_tokens": 13,
                                "output_tokens": 8,
                                "total_tokens": 21
                            }
                        }
                    }),
                )
                .await;

            let parent_events = drain_events(&mut parent_rx);
            assert!(
                parent_events.is_empty(),
                "late child token usage must not emit into parent stream: {parent_events:?}"
            );

            let subagent_events = drain_events(&mut subagent_rx);
            let stream_end = subagent_events
                .iter()
                .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                .expect("sub-agent StreamEnd");
            assert_eq!(
                stream_end
                    .pointer("/data/message/token_usage/turn/reason")
                    .and_then(Value::as_str),
                Some("backend_did_not_report")
            );

            let metadata_update = subagent_events
                .iter()
                .find(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("MessageMetadataUpdated")
                })
                .expect("sub-agent MessageMetadataUpdated");
            assert_eq!(
                metadata_update
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some("msg-sub-late-usage")
            );
            assert_eq!(
                metadata_update
                    .pointer("/data/token_usage/request/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(21)
            );
            assert_eq!(
                metadata_update
                    .pointer("/data/token_usage/turn/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(21)
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn idless_reasoning_streams_immediately_and_keeps_generated_identity() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-idless" } }),
                )
                .await;
            drain_events(&mut rx);

            inner
                .handle_notification(
                    "item/reasoning/delta",
                    &json!({ "threadId": "thread-test", "delta": "Inspecting constraints." }),
                )
                .await;
            let reasoning_events = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&reasoning_events),
                vec!["TypingStatusChanged", "StreamStart", "StreamReasoningDelta"]
            );
            let generated_id = reasoning_events[1]
                .pointer("/data/message_id")
                .and_then(Value::as_str)
                .expect("generated reasoning id")
                .to_string();
            assert!(generated_id.starts_with("server-generated:idless_reasoning:"));
            assert_eq!(
                reasoning_events[2]
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some(generated_id.as_str())
            );

            inner
                .handle_notification(
                    "item/started",
                    &json!({
                        "threadId": "thread-test",
                        "item": {
                            "type": "commandExecution",
                            "id": "reasoning-tool",
                            "command": "pwd",
                            "cwd": "/tmp"
                        }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": {
                            "type": "commandExecution",
                            "id": "reasoning-tool",
                            "exitCode": 0,
                            "aggregatedOutput": "/tmp"
                        }
                    }),
                )
                .await;
            assert_eq!(
                event_kinds(&drain_events(&mut rx)),
                vec!["ToolRequest", "ToolExecutionCompleted"]
            );

            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "reasoning", "summary": "Inspecting constraints." }
                    }),
                )
                .await;
            let end_events = drain_events(&mut rx);
            assert_eq!(event_kinds(&end_events), vec!["StreamEnd"]);
            assert_eq!(
                end_events[0]
                    .pointer("/data/message/message_id")
                    .and_then(Value::as_str),
                Some(generated_id.as_str())
            );
            assert_eq!(
                end_events[0]
                    .pointer("/data/message/reasoning/text")
                    .and_then(Value::as_str),
                Some("Inspecting constraints.")
            );
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-idless", "status": "completed" } }),
                )
                .await;
            drain_events(&mut rx);
            assert!(drain_events(&mut rx).is_empty());
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn idless_reasoning_interrupt_persists_partial_message_before_cancel() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-cancel" } }),
                )
                .await;
            drain_events(&mut rx);
            inner
                .handle_notification(
                    "item/reasoning/delta",
                    &json!({ "threadId": "thread-test", "delta": "Still thinking" }),
                )
                .await;
            let open_events = drain_events(&mut rx);
            let generated_id = open_events
                .iter()
                .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"))
                .and_then(|event| event.pointer("/data/message_id"))
                .and_then(Value::as_str)
                .expect("generated reasoning id")
                .to_string();

            inner
                .handle_notification(
                    "turn/completed",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-cancel", "status": "interrupted" } }),
                )
                .await;
            let terminal = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&terminal),
                vec!["StreamEnd", "OperationCancelled", "TypingStatusChanged"]
            );
            assert_eq!(
                terminal[0]
                    .pointer("/data/message/message_id")
                    .and_then(Value::as_str),
                Some(generated_id.as_str())
            );
            assert_eq!(
                terminal[0]
                    .pointer("/data/message/reasoning/text")
                    .and_then(Value::as_str),
                Some("Still thinking")
            );
            assert_eq!(
                terminal[0]
                    .pointer("/data/message/content")
                    .and_then(Value::as_str),
                Some("")
            );
            assert_codex_protocol_valid(&[open_events, terminal.clone()].concat());

            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "reasoning", "summary": "late" }
                    }),
                )
                .await;
            assert!(drain_events(&mut rx).is_empty());
            assert!(generated_id.starts_with("server-generated:idless_reasoning:"));
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn idless_reasoning_turn_completion_persists_partial_message_before_cancel() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-partial" } }),
                )
                .await;
            let mut replay_shaped_events = drain_events(&mut rx);
            inner
                .handle_notification(
                    "item/reasoning/delta",
                    &json!({ "threadId": "thread-test", "delta": "Partial reasoning" }),
                )
                .await;
            let live = drain_events(&mut rx);
            let generated_id = live
                .iter()
                .find(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"))
                .and_then(|event| event.pointer("/data/message_id"))
                .and_then(Value::as_str)
                .expect("generated reasoning id")
                .to_string();
            replay_shaped_events.extend(live);

            inner
                .handle_notification(
                    "turn/completed",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-partial", "status": "completed" } }),
                )
                .await;
            let terminal = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&terminal),
                vec!["StreamEnd", "OperationCancelled", "TypingStatusChanged"]
            );
            assert_eq!(
                terminal[0]
                    .pointer("/data/message/message_id")
                    .and_then(Value::as_str),
                Some(generated_id.as_str())
            );
            assert_eq!(
                terminal[0]
                    .pointer("/data/message/reasoning/text")
                    .and_then(Value::as_str),
                Some("Partial reasoning")
            );
            assert_eq!(
                terminal[0]
                    .pointer("/data/message/content")
                    .and_then(Value::as_str),
                Some("")
            );
            replay_shaped_events.extend(terminal);
            assert_codex_protocol_valid(&replay_shaped_events);

            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "reasoning", "summary": "late" }
                    }),
                )
                .await;
            assert!(drain_events(&mut rx).is_empty());
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn codex_metadata_update_targets_last_visible_segment() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-segmented" } }),
                )
                .await;
            drain_events(&mut rx);

            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": {
                            "type": "reasoning",
                            "id": "reasoning-1",
                            "summary": "Inspecting the failure first."
                        }
                    }),
                )
                .await;
            let events = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&events),
                vec!["StreamStart", "StreamReasoningDelta", "StreamEnd"]
            );

            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "thread-test", "itemId": "answer-1", "delta": "Done" }),
                )
                .await;
            drain_events(&mut rx);
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": {
                            "type": "agentMessage",
                            "id": "answer-1",
                            "text": "Done"
                        }
                    }),
                )
                .await;
            let events = drain_events(&mut rx);
            assert_eq!(
                events
                    .iter()
                    .filter_map(|event| event.get("kind").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec!["StreamEnd"]
            );

            inner
                .handle_notification(
                    "turn/completed",
                    &json!({
                        "threadId": "thread-test",
                        "turn": {
                            "id": "turn-segmented",
                            "status": "completed",
                            "usage": {
                                "inputTokens": 20,
                                "outputTokens": 7,
                                "totalTokens": 27
                            }
                        }
                    }),
                )
                .await;
            let events = drain_events(&mut rx);
            assert_eq!(
                events
                    .iter()
                    .filter_map(|event| event.get("kind").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec!["MessageMetadataUpdated", "TypingStatusChanged"]
            );
            assert_eq!(
                events[0]
                    .pointer("/data/message_id")
                    .and_then(Value::as_str),
                Some("answer-1")
            );
            assert_eq!(
                events[0]
                    .pointer("/data/token_usage/turn/usage/total_tokens")
                    .and_then(Value::as_u64),
                Some(27)
            );

            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn wrong_completed_item_id_discards_once_without_typed_end() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-wrong-end" } }),
                )
                .await;
            drain_events(&mut rx);
            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "thread-test", "itemId": "open-item", "delta": "partial" }),
                )
                .await;
            drain_events(&mut rx);

            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "agentMessage", "id": "wrong-item", "text": "wrong" }
                    }),
                )
                .await;
            let terminal = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&terminal),
                vec!["MessageAdded", "OperationCancelled", "TypingStatusChanged"]
            );
            assert!(terminal.iter().all(|event| {
                event.get("kind").and_then(Value::as_str) != Some("StreamEnd")
            }));

            for item_id in ["open-item", "wrong-item", "another-late-item"] {
                inner
                    .handle_notification(
                        "item/completed",
                        &json!({
                            "threadId": "thread-test",
                            "item": { "type": "agentMessage", "id": item_id, "text": "late" }
                        }),
                    )
                    .await;
            }
            for (method, params) in [
                (
                    "turn/plan/updated",
                    json!({
                        "threadId": "thread-test",
                        "explanation": "late plan",
                        "plan": [{ "step": "must not render", "status": "pending" }]
                    }),
                ),
                (
                    "model/rerouted",
                    json!({ "threadId": "thread-test", "toModel": "late-model" }),
                ),
                (
                    "error",
                    json!({ "threadId": "thread-test", "message": "late error", "fatal": true }),
                ),
                (
                    "thread/tokenUsage/updated",
                    json!({
                        "threadId": "thread-test",
                        "turnId": "turn-wrong-end",
                        "tokenUsage": { "inputTokens": 1, "outputTokens": 1, "totalTokens": 2 }
                    }),
                ),
                (
                    "item/reasoning/delta",
                    json!({ "threadId": "thread-test", "delta": "late reasoning" }),
                ),
                (
                    "item/started",
                    json!({
                        "threadId": "thread-test",
                        "item": { "type": "agentMessage", "id": "late-start" }
                    }),
                ),
                (
                    "item/agentMessage/delta",
                    json!({
                        "threadId": "thread-test",
                        "itemId": "late-start",
                        "delta": "late text"
                    }),
                ),
                (
                    "turn/completed",
                    json!({
                        "threadId": "thread-test",
                        "turn": { "id": "turn-wrong-end", "status": "completed" }
                    }),
                ),
                (
                    "turn/started",
                    json!({
                        "threadId": "thread-test",
                        "turn": { "id": "turn-wrong-end" }
                    }),
                ),
            ] {
                inner.handle_notification(method, &params).await;
            }
            assert!(
                drain_events(&mut rx).is_empty(),
                "quarantine must suppress every later root response notification"
            );
            assert_eq!(inner.state.lock().await.model.as_deref(), Some("codex"));
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn codex_agent_message_items_keep_distinct_stream_boundaries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            let mut all_events = Vec::new();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-items" } }),
                )
                .await;
            all_events.extend(drain_events(&mut rx));

            for (item_id, delta, text) in [
                ("item-alpha", "alpha delta", "alpha complete"),
                ("item-beta", "beta delta", "beta complete"),
                ("item-gamma", "gamma delta", "gamma complete"),
            ] {
                inner
                    .handle_notification(
                        "item/started",
                        &json!({
                            "threadId": "thread-test",
                            "item": { "type": "agentMessage", "id": item_id }
                        }),
                    )
                    .await;
                inner
                    .handle_notification(
                        "item/agentMessage/delta",
                        &json!({ "threadId": "thread-test", "item_id": item_id, "delta": delta }),
                    )
                    .await;
                inner
                    .handle_notification(
                        "item/completed",
                        &json!({
                            "threadId": "thread-test",
                            "item": { "type": "agentMessage", "id": item_id, "text": text }
                        }),
                )
                .await;
                all_events.extend(drain_events(&mut rx));
            }
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({
                        "threadId": "thread-test",
                        "turn": { "id": "turn-items", "status": "completed" }
                    }),
                )
                .await;
            all_events.extend(drain_events(&mut rx));

            assert_eq!(
                all_events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"))
                    .map(|event| event.pointer("/data/message_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("item-alpha"), Some("item-beta"), Some("item-gamma")]
            );
            assert_eq!(
                all_events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamDelta"))
                    .map(|event| event.pointer("/data/message_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("item-alpha"), Some("item-beta"), Some("item-gamma")]
            );
            assert_eq!(
                all_events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                    .map(|event| event.pointer("/data/message/message_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("item-alpha"), Some("item-beta"), Some("item-gamma")]
            );
            assert_codex_protocol_valid(&all_events);
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn codex_completion_only_items_and_duplicate_completion_are_item_scoped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            {
                let mut state = inner.state.lock().await;
                state.active_stream = None;
            }
            let mut all_events = Vec::new();
            for (item_id, text) in [
                ("complete-alpha", "alpha"),
                ("complete-beta", "beta"),
                ("complete-gamma", "gamma"),
            ] {
                let event = json!({
                    "threadId": "thread-test",
                    "item": { "type": "agentMessage", "id": item_id, "text": text }
                });
                inner.handle_notification("item/completed", &event).await;
                all_events.extend(drain_events(&mut rx));
                if item_id == "complete-alpha" {
                    inner.handle_notification("item/completed", &event).await;
                    assert!(
                        drain_events(&mut rx).is_empty(),
                        "byte-equivalent completion must not create another relay"
                    );
                }
            }

            assert_eq!(
                all_events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"))
                    .count(),
                3
            );
            assert_eq!(
                all_events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                    .map(|event| event.pointer("/data/message/content").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("alpha"), Some("beta"), Some("gamma")]
            );
            assert_codex_protocol_valid(&all_events);
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn codex_child_agent_messages_use_provider_item_ids_without_turn_streams() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (child_tx, mut child_rx) = mpsc::unbounded_channel();
            attach_test_codex_subagent(&inner, child_tx, "child-thread").await;

            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "child-thread", "turn": { "id": "child-turn" } }),
                )
                .await;
            assert!(drain_events(&mut parent_rx).is_empty());
            drain_events(&mut child_rx);

            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "child-thread", "itemId": "child-alpha", "delta": "alpha" }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "child-thread",
                        "item": { "type": "agentMessage", "id": "child-alpha", "text": "alpha" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "child-thread",
                        "item": { "type": "agentMessage", "id": "child-beta", "text": "" }
                    }),
                )
                .await;

            let events = drain_events(&mut child_rx);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"))
                    .map(|event| event.pointer("/data/message_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("child-alpha"), Some("child-beta")]
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamEnd"))
                    .map(|event| event.pointer("/data/message/message_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("child-alpha"), Some("child-beta")]
            );
            assert!(
                events.iter().all(|event| event.get("kind").and_then(Value::as_str) != Some("Error")),
                "child item streams must not emit identity errors: {events:?}"
            );
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn child_wrong_completion_id_is_quarantined_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut parent_rx) = test_codex_inner();
            let (child_tx, mut child_rx) = mpsc::unbounded_channel();
            attach_test_codex_subagent(&inner, child_tx, "child-quarantine").await;
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "child-quarantine", "turn": { "id": "child-turn" } }),
                )
                .await;
            assert!(drain_events(&mut parent_rx).is_empty());
            drain_events(&mut child_rx);
            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "child-quarantine", "itemId": "child-open", "delta": "partial" }),
                )
                .await;
            drain_events(&mut child_rx);

            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "child-quarantine",
                        "item": { "type": "agentMessage", "id": "child-wrong", "text": "wrong" }
                    }),
                )
                .await;
            let terminal = drain_events(&mut child_rx);
            assert_eq!(
                event_kinds(&terminal),
                vec!["MessageAdded", "OperationCancelled", "TypingStatusChanged"]
            );
            assert!(terminal.iter().all(|event| {
                event.get("kind").and_then(Value::as_str) != Some("StreamEnd")
            }));

            for (method, params) in [
                (
                    "item/completed",
                    json!({
                        "threadId": "child-quarantine",
                        "item": { "type": "agentMessage", "id": "child-open", "text": "late" }
                    }),
                ),
                (
                    "item/started",
                    json!({
                        "threadId": "child-quarantine",
                        "item": { "type": "agentMessage", "id": "child-late" }
                    }),
                ),
                (
                    "item/reasoning/delta",
                    json!({ "threadId": "child-quarantine", "delta": "late reasoning" }),
                ),
                (
                    "turn/plan/updated",
                    json!({
                        "threadId": "child-quarantine",
                        "explanation": "late plan",
                        "plan": [{ "step": "must not render", "status": "pending" }]
                    }),
                ),
                (
                    "model/rerouted",
                    json!({ "threadId": "child-quarantine", "toModel": "late-model" }),
                ),
                (
                    "error",
                    json!({
                        "threadId": "child-quarantine",
                        "message": "late child error",
                        "fatal": true
                    }),
                ),
                (
                    "thread/tokenUsage/updated",
                    json!({
                        "threadId": "child-quarantine",
                        "turnId": "child-turn",
                        "tokenUsage": { "inputTokens": 1, "outputTokens": 1, "totalTokens": 2 }
                    }),
                ),
                (
                    "turn/completed",
                    json!({
                        "threadId": "child-quarantine",
                        "turn": { "id": "child-turn", "status": "completed" }
                    }),
                ),
                (
                    "turn/started",
                    json!({
                        "threadId": "child-quarantine",
                        "turn": { "id": "child-turn" }
                    }),
                ),
            ] {
                inner.handle_notification(method, &params).await;
            }
            assert!(
                drain_events(&mut child_rx).is_empty(),
                "quarantine must suppress every later child response notification"
            );
            assert!(drain_events(&mut parent_rx).is_empty());
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn conflicting_duplicate_completion_is_quarantined_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            {
                let mut state = inner.state.lock().await;
                state.active_stream = None;
            }
            let first = json!({
                "threadId": "thread-test",
                "item": { "type": "agentMessage", "id": "conflict-id", "text": "first" }
            });
            inner.handle_notification("item/completed", &first).await;
            let completed = drain_events(&mut rx);
            assert_eq!(event_kinds(&completed), vec!["StreamStart", "StreamEnd"]);

            let conflicting = json!({
                "threadId": "thread-test",
                "item": { "type": "agentMessage", "id": "conflict-id", "text": "second" }
            });
            inner
                .handle_notification("item/completed", &conflicting)
                .await;
            let terminal = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&terminal),
                vec!["MessageAdded", "OperationCancelled", "TypingStatusChanged"]
            );
            assert!(
                terminal.iter().all(|event| {
                    event.get("kind").and_then(Value::as_str) != Some("StreamEnd")
                })
            );

            inner
                .handle_notification("item/completed", &conflicting)
                .await;
            inner.handle_notification("item/completed", &first).await;
            assert!(
                drain_events(&mut rx).is_empty(),
                "a quarantined duplicate must have zero extra tails"
            );
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn codex_turn_completed_quarantines_an_uncompleted_root_item() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-open" } }),
                )
                .await;
            drain_events(&mut rx);
            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "thread-test", "itemId": "open-item", "delta": "partial" }),
                )
                .await;
            drain_events(&mut rx);
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-open", "status": "completed" } }),
                )
                .await;
            let terminal = drain_events(&mut rx);
            assert_eq!(
                event_kinds(&terminal),
                vec!["MessageAdded", "OperationCancelled", "TypingStatusChanged"],
                "uncompleted item needs exactly one discard tail: {terminal:?}"
            );
            assert!(
                terminal.iter().all(|event| event.get("kind").and_then(Value::as_str) != Some("StreamEnd")),
                "turn completion must not fabricate an item completion: {terminal:?}"
            );
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "agentMessage", "id": "open-item", "text": "late" }
                    }),
                )
                .await;
            assert!(
                drain_events(&mut rx).is_empty(),
                "late completion must not resurrect a quarantined turn"
            );
            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "thread-test", "itemId": "another-late", "delta": "late" }),
                )
                .await;
            inner
                .handle_notification(
                    "turn/completed",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-open", "status": "completed" } }),
                )
                .await;
            assert!(
                drain_events(&mut rx).is_empty(),
                "late notifications must have zero extra discard tails"
            );
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn codex_reasoning_item_and_assistant_item_keep_separate_ids() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (inner, mut rx) = test_codex_inner();
            inner
                .handle_notification(
                    "turn/started",
                    &json!({ "threadId": "thread-test", "turn": { "id": "turn-reasoning" } }),
                )
                .await;
            drain_events(&mut rx);
            inner
                .handle_notification(
                    "item/reasoning/delta",
                    &json!({ "threadId": "thread-test", "itemId": "reasoning-item", "delta": "thinking" }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "reasoning", "id": "reasoning-item", "summary": "thinking" }
                    }),
                )
                .await;
            inner
                .handle_notification(
                    "item/agentMessage/delta",
                    &json!({ "threadId": "thread-test", "itemId": "answer-item", "delta": "answer" }),
                )
                .await;
            inner
                .handle_notification(
                    "item/completed",
                    &json!({
                        "threadId": "thread-test",
                        "item": { "type": "agentMessage", "id": "answer-item", "text": "answer" }
                    }),
                )
                .await;
            let events = drain_events(&mut rx);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.get("kind").and_then(Value::as_str) == Some("StreamStart"))
                    .map(|event| event.pointer("/data/message_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                vec![Some("reasoning-item"), Some("answer-item")]
            );
            assert!(
                events.iter().any(|event| {
                    event.get("kind").and_then(Value::as_str) == Some("StreamEnd")
                        && event.pointer("/data/message/message_id").and_then(Value::as_str)
                            == Some("reasoning-item")
                }),
                "provider reasoning completion must close its own response: {events:?}"
            );
            assert!(events.iter().any(|event| {
                event.get("kind").and_then(Value::as_str) == Some("StreamEnd")
                    && event
                        .pointer("/data/message/message_id")
                        .and_then(Value::as_str)
                        == Some("answer-item")
            }));
            assert!(
                events.iter().all(|event| event.get("kind").and_then(Value::as_str) != Some("MessageAdded")),
                "distinct provider boundaries must not raise a foreign-ID error: {events:?}"
            );
            inner.rpc.shutdown().await;
        });
    }

    #[test]
    fn extract_codex_reasoning_delta_text_preserves_leading_whitespace() {
        let payload = json!({ "delta": " targeted web search" });
        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some(" targeted web search".to_string())
        );
    }

    #[test]
    fn extract_codex_reasoning_delta_text_parses_nested_shapes() {
        let payload = json!({
            "itemId": "abc",
            "delta": {
                "summary": {
                    "text": "Need to inspect parser edge-cases."
                }
            }
        });

        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some("Need to inspect parser edge-cases.".to_string())
        );
    }

    #[test]
    fn extract_codex_item_reasoning_reads_reasoning_content_blocks() {
        let item = json!({
            "type": "agentMessage",
            "content": [
                { "type": "text", "text": "Visible answer" },
                { "type": "reasoning_summary", "summary": "Checking assumptions first." }
            ]
        });

        assert_eq!(
            extract_codex_item_reasoning(&item),
            Some("Checking assumptions first.".to_string())
        );
    }

    #[test]
    fn extract_codex_item_reasoning_preserves_boundary_whitespace() {
        let item = json!({
            "type": "reasoning",
            "reasoning": " user"
        });

        assert_eq!(
            extract_codex_item_reasoning(&item),
            Some(" user".to_string())
        );
    }

    #[test]
    fn extract_codex_reasoning_delta_text_accepts_reasoning_summary_aliases() {
        let payload = json!({
            "itemId": "abc",
            "reasoningSummary": {
                "output_text": "Need to confirm assumptions before edits."
            }
        });

        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some("Need to confirm assumptions before edits.".to_string())
        );
    }

    #[test]
    fn extract_codex_reasoning_delta_text_accepts_legacy_event_msg_shape() {
        let payload = json!({
            "msg": {
                "type": "agent_reasoning_raw_content_delta",
                "delta": "Inspecting event payload shape."
            }
        });

        assert_eq!(
            extract_codex_reasoning_delta_text(&payload),
            Some("Inspecting event payload shape.".to_string())
        );
    }

    #[test]
    fn extract_codex_event_type_reads_legacy_method_suffix() {
        let payload = json!({});
        assert_eq!(
            extract_codex_event_type("codex/event/agent_reasoning_raw_content_delta", &payload),
            Some("agent_reasoning_raw_content_delta".to_string())
        );
    }

    #[test]
    fn extract_reasoning_delta_from_legacy_codex_event_parses_reasoning_delta() {
        let payload = json!({
            "msg": {
                "type": "agent_reasoning_raw_content_delta",
                "delta": "Evaluating alternatives."
            }
        });
        assert_eq!(
            extract_reasoning_delta_from_legacy_codex_event(
                "codex/event/agent_reasoning_raw_content_delta",
                &payload
            ),
            Some("Evaluating alternatives.".to_string())
        );
    }

    #[test]
    fn extract_reasoning_delta_from_legacy_codex_event_maps_section_break() {
        assert_eq!(
            extract_reasoning_delta_from_legacy_codex_event(
                "codex/event/agent_reasoning_section_break",
                &json!({})
            ),
            Some("\n\n".to_string())
        );
    }

    #[test]
    fn extract_reasoning_delta_from_legacy_codex_event_ignores_non_reasoning() {
        let payload = json!({
            "msg": {
                "type": "agent_message_delta",
                "delta": "Visible answer text."
            }
        });
        assert_eq!(
            extract_reasoning_delta_from_legacy_codex_event(
                "codex/event/agent_message_delta",
                &payload
            ),
            None
        );
    }

    #[test]
    fn is_codex_event_reasoning_type_handles_supported_values() {
        assert!(is_codex_event_reasoning_type("agent_reasoning"));
        assert!(is_codex_event_reasoning_type(
            "agent_reasoning_raw_content_delta"
        ));
        assert!(!is_codex_event_reasoning_type("agent_message_delta"));
    }

    #[test]
    fn is_reasoning_notification_method_accepts_alias_shapes() {
        assert!(is_reasoning_notification_method(
            "item/reasoning/summaryTextDelta"
        ));
        assert!(is_reasoning_notification_method(
            "item/reasoningSummaryText/delta"
        ));
        assert!(is_reasoning_notification_method(
            "item/reasoning_summary_text/delta"
        ));
        assert!(is_reasoning_notification_method("item/thinking/textDelta"));
        assert!(!is_reasoning_notification_method("item/agentMessage/delta"));
    }

    #[test]
    fn codex_error_notifications_are_non_terminal_while_turn_is_active() {
        let state = test_codex_state();
        assert!(!is_terminal_codex_error_notification(
            &state,
            &json!({ "message": "Tool warning" })
        ));
    }

    #[test]
    fn codex_error_notifications_are_terminal_when_idle_or_explicitly_fatal() {
        let mut idle_state = test_codex_state();
        idle_state.active_turn_id = None;
        idle_state.active_stream = None;
        idle_state.pending_request = None;

        assert!(is_terminal_codex_error_notification(
            &idle_state,
            &json!({ "message": "Session failed" })
        ));

        let active_state = test_codex_state();
        assert!(is_terminal_codex_error_notification(
            &active_state,
            &json!({ "message": "Fatal turn error", "fatal": true })
        ));
        assert!(is_terminal_codex_error_notification(
            &active_state,
            &json!({ "message": "Fatal turn error", "recoverable": false })
        ));
    }

    #[test]
    fn extract_codex_item_reasoning_reads_reasoning_thread_item_summary() {
        let item = json!({
            "type": "reasoning",
            "id": "reasoning-item-1",
            "summary": ["Check constraints", "Then produce final answer"],
            "content": []
        });

        assert_eq!(
            extract_codex_item_reasoning(&item),
            Some("Check constraints\nThen produce final answer".to_string())
        );
    }

    #[test]
    fn normalize_token_usage_accepts_flat_snake_case_payloads() {
        let normalized = normalize_token_usage_with_envelope(
            &json!({
            "input_tokens": 120,
            "output_tokens": 80,
            "total_tokens": 200,
            "cached_prompt_tokens": 20,
            "cache_creation_input_tokens": 5,
            "reasoning_tokens": 7,
            "context_window": 200000
            }),
            None,
            None,
        )
        .expect("valid token usage");

        assert_eq!(normalized["input_tokens"], json!(120));
        assert_eq!(normalized["output_tokens"], json!(80));
        assert_eq!(normalized["total_tokens"], json!(200));
        assert_eq!(normalized["cached_prompt_tokens"], json!(20));
        assert_eq!(normalized["cache_creation_input_tokens"], json!(5));
        assert_eq!(normalized["reasoning_tokens"], json!(7));
        assert_eq!(normalized["context_window"], json!(200000));
    }

    #[test]
    fn extract_turn_token_usage_ignores_null_empty_and_context_only_usage() {
        let null_usage = json!({
            "turn": {
                "id": "turn_null",
                "usage": Value::Null
            }
        });
        assert!(extract_turn_token_usage(&null_usage, None).is_none());

        let empty_usage = json!({
            "turnId": "turn_empty",
            "tokenUsage": {}
        });
        assert!(extract_turn_token_usage(&empty_usage, None).is_none());

        let context_only_usage = json!({
            "turn": {
                "id": "turn_context",
                "usage": {
                    "contextWindow": 400_000
                }
            }
        });
        assert!(extract_turn_token_usage(&context_only_usage, None).is_none());
    }

    #[test]
    fn extract_turn_token_usage_reads_nested_turn_shape() {
        let payload = json!({
            "turn": {
                "id": "turn_123",
                "usage": {
                    "input_tokens": 90,
                    "output_tokens": 30,
                    "total_tokens": 120
                }
            }
        });

        let (turn_id, usage) = extract_turn_token_usage(&payload, None).expect("turn usage");
        assert_eq!(turn_id, "turn_123");
        assert_eq!(usage["input_tokens"], json!(90));
        assert_eq!(usage["output_tokens"], json!(30));
        assert_eq!(usage["total_tokens"], json!(120));
    }

    #[test]
    fn extract_turn_token_usage_reads_context_window_from_event_wrapper() {
        let payload = json!({
            "turn": {
                "id": "turn_123",
                "usage": {
                    "input_tokens": 90,
                    "output_tokens": 30,
                    "total_tokens": 120
                }
            },
            "modelUsage": {
                "gpt-5.3-codex": {
                    "contextWindow": 400_000
                }
            }
        });

        let (_, usage) =
            extract_turn_token_usage(&payload, Some("gpt-5.3-codex")).expect("turn usage");
        assert_eq!(usage["context_window"], json!(400_000));
    }

    #[test]
    fn model_request_usage_keeps_last_turn_and_thread_scopes() {
        let first_payload = json!({
            "turnId": "turn_123",
            "tokenUsage": {
                "last": {
                    "inputTokens": 100,
                    "cachedInputTokens": 80,
                    "outputTokens": 10,
                    "reasoningOutputTokens": 4,
                    "totalTokens": 110
                },
                "total": {
                    "inputTokens": 1_000,
                    "cachedInputTokens": 900,
                    "outputTokens": 100,
                    "reasoningOutputTokens": 40,
                    "totalTokens": 1_100
                },
                "modelContextWindow": 400_000
            }
        });
        let second = json!({
            "turnId": "turn_123",
            "tokenUsage": {
                "last": {
                    "inputTokens": 120,
                    "cachedInputTokens": 90,
                    "outputTokens": 20,
                    "reasoningOutputTokens": 5,
                    "totalTokens": 140
                },
                "total": {
                    "inputTokens": 1_120,
                    "cachedInputTokens": 990,
                    "outputTokens": 120,
                    "reasoningOutputTokens": 45,
                    "totalTokens": 1_240
                },
                "modelContextWindow": 400_000
            }
        });
        let mut by_turn = HashMap::new();

        let (turn_id, request, cumulative, context_window) =
            extract_model_request_token_usage(&first_payload, Some("gpt-5.5"))
                .expect("first usage");
        let first = record_model_request_token_usage(
            &mut by_turn,
            turn_id,
            request,
            cumulative,
            context_window,
        )
        .expect("first request");
        assert_eq!(first.request_id.sequence, 0);
        assert_eq!(first.request.total_tokens, 110);
        assert_eq!(first.turn.total_tokens, 110);
        assert_eq!(first.cumulative.total_tokens, 1_100);
        let (turn_id, request, cumulative, context_window) =
            extract_model_request_token_usage(&first_payload, Some("gpt-5.5"))
                .expect("duplicate usage");
        assert!(
            record_model_request_token_usage(
                &mut by_turn,
                turn_id,
                request,
                cumulative,
                context_window,
            )
            .is_none(),
            "duplicate cumulative snapshots must not invent requests"
        );

        let (turn_id, request, cumulative, context_window) =
            extract_model_request_token_usage(&second, Some("gpt-5.5")).expect("second usage");
        let second = record_model_request_token_usage(
            &mut by_turn,
            turn_id,
            request,
            cumulative,
            context_window,
        )
        .expect("second request");
        assert_eq!(second.request_id.sequence, 1);
        assert_eq!(second.request.total_tokens, 140);
        assert_eq!(second.turn.total_tokens, 250);
        assert_eq!(second.cumulative.total_tokens, 1_240);
        assert_eq!(second.model_context_window, Some(400_000));

        let (request, turn, cumulative) = codex_message_usage_values(None, by_turn.get("turn_123"));
        assert_eq!(request.expect("request usage")["total_tokens"], json!(140));
        assert_eq!(turn.expect("turn usage")["total_tokens"], json!(250));
        assert_eq!(
            cumulative.expect("cumulative usage")["total_tokens"],
            json!(1_240)
        );
    }

    #[test]
    fn estimate_context_breakdown_uses_model_aware_context_fallback() {
        let usage = json!({
            "input_tokens": 90,
            "output_tokens": 30,
            "total_tokens": 120,
            "cached_prompt_tokens": 0,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 0,
            "context_window": Value::Null
        });
        let turn_context = TurnContextEstimate::default();
        let breakdown =
            estimate_context_breakdown(Some(&usage), &turn_context, Some("gpt-5.3-codex"));
        assert_eq!(
            breakdown.get("context_window").and_then(Value::as_u64),
            Some(CODEX_ESTIMATED_CONTEXT_WINDOW_GPT5_FAMILY)
        );
    }

    #[test]
    fn codex_estimated_context_window_covers_gpt5_family_and_prefixes() {
        // Whole gpt-5 family is 400k, regardless of suffix or provider prefix.
        for model in [
            "gpt-5",
            "gpt-5.1",
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.4-mini",
            "gpt-5-codex",
            "gpt-5.3-codex",
            "gpt-5.3-codex-spark",
            // The CLI now reports provider-prefixed ids.
            "openai.gpt-5.5",
            "openai.gpt-5.3-codex",
        ] {
            assert_eq!(
                codex_estimated_context_window_for_model(Some(model)),
                CODEX_ESTIMATED_CONTEXT_WINDOW_GPT5_FAMILY,
                "{model} should map to the gpt-5 family window"
            );
        }

        // codex-mini-latest is the 200k exception and must not be swept into
        // the gpt-5 family branch.
        assert_eq!(
            codex_estimated_context_window_for_model(Some("codex-mini-latest")),
            CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT
        );

        // Unknown / unset models fall back to the conservative default.
        assert_eq!(
            codex_estimated_context_window_for_model(None),
            CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT
        );
        assert_eq!(
            codex_estimated_context_window_for_model(Some("some-future-model")),
            CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT
        );
    }

    #[test]
    fn parse_codex_subagent_collab_reads_authoritative_thread_ids() {
        let item = json!({
            "type": "collabAgentToolCall",
            "id": "collab-1",
            "senderThreadId": "thread-parent",
            "receiverThreadId": "thread-child",
            "prompt": "Review src/auth.ts",
            "receiverAgentType": "reviewer",
            "receiverAgentName": "Auth Reviewer"
        });

        let parsed = parse_codex_subagent_collab(&item).expect("collab item");
        assert_eq!(parsed.item_id, "collab-1");
        assert_eq!(parsed.sender_thread_id, "thread-parent");
        assert_eq!(parsed.receiver_thread_id, "thread-child");
        assert_eq!(parsed.name, "Auth Reviewer");
    }

    #[test]
    fn parse_codex_subagent_activity_reads_authoritative_child_thread() {
        let item = json!({
            "type": "subAgentActivity",
            "kind": "started",
            "agentThreadId": "thread-child",
            "agentPath": "/root/reviewer"
        });
        let parsed = parse_codex_subagent_activity(&item).expect("activity item");
        assert_eq!(parsed.kind, "started");
        assert_eq!(parsed.agent_thread_id, "thread-child");
        assert_eq!(parsed.agent_path, "/root/reviewer");
    }

    #[test]
    fn parse_captured_rollout_subagent_activity_shape() {
        let item = json!({
            "type": "sub_agent_activity",
            "kind": "started",
            "agent_thread_id": "019f5938-c06a-actual-child",
            "agent_path": "/root/map_planner"
        });
        let parsed = parse_codex_subagent_activity(&item).expect("captured rollout activity");
        assert_eq!(parsed.kind, "started");
        assert_eq!(parsed.agent_thread_id, "019f5938-c06a-actual-child");
        assert_eq!(parsed.agent_path, "/root/map_planner");
    }

    #[test]
    fn collab_wait_followup_and_interrupt_do_not_become_spawn_metadata() {
        for item in [
            json!({"type":"collabAgentToolCall","id":"wait","senderThreadId":"parent","receiverThreadId":"child","agentsStates":{},"prompt":"wait","receiverAgentType":"worker"}),
            json!({"type":"collabAgentToolCall","id":"followup","senderThreadId":"parent","receiverThreadId":"child","prompt":"continue"}),
            json!({"type":"collabToolCall","id":"interrupt","senderThreadId":"parent","receiverThreadId":"child","prompt":"stop","receiverAgentType":"worker"}),
        ] {
            assert!(parse_codex_subagent_collab(&item).is_none(), "{item}");
        }
    }

    #[test]
    fn codex_item_success_uses_status_and_success_flag() {
        assert!(codex_item_success(&json!({ "status": "completed" })));
        assert!(!codex_item_success(&json!({ "status": "failed" })));
        assert!(!codex_item_success(
            &json!({ "success": false, "status": "completed" })
        ));
    }

    #[test]
    fn codex_mcp_elicitation_approves_tyde_review_tools() {
        let result = codex_mcp_elicitation_result(&json!({
            "serverName": REVIEW_FEEDBACK_MCP_SERVER_NAME,
            "_meta": {
                "codex_approval_kind": "mcp_tool_call"
            }
        }));

        assert_eq!(result.get("action").and_then(Value::as_str), Some("accept"));
        assert!(result.get("content").and_then(Value::as_object).is_some());
    }

    #[test]
    fn codex_mcp_elicitation_cancels_unknown_tools() {
        let result = codex_mcp_elicitation_result(&json!({
            "serverName": "external-mcp",
            "_meta": {
                "codex_approval_kind": "mcp_tool_call"
            }
        }));

        assert_eq!(result.get("action").and_then(Value::as_str), Some("cancel"));
    }

    #[test]
    fn codex_server_request_result_uses_elicitation_policy() {
        let result = codex_server_request_result(
            "mcpServer/elicitation/request",
            &json!({
                "serverName": REVIEW_FEEDBACK_MCP_SERVER_NAME,
                "_meta": {
                    "codex_approval_kind": "mcp_tool_call"
                }
            }),
        );

        assert_eq!(result.get("action").and_then(Value::as_str), Some("accept"));
    }

    #[test]
    fn codex_server_request_result_resolves_unknown_requests() {
        let result = codex_server_request_result("unknown/request", &json!({}));

        assert_eq!(result.get("ignored").and_then(Value::as_bool), Some(true));
        assert_eq!(
            result.get("reason").and_then(Value::as_str),
            Some("unsupported_server_request")
        );
    }

    #[test]
    fn codex_danger_full_access_sandbox_policy_is_danger_full_access() {
        let policy = codex_danger_full_access_sandbox_policy(false);
        assert_eq!(
            policy.get("type").and_then(Value::as_str),
            Some("dangerFullAccess")
        );
        assert_eq!(policy.get("networkAccess"), None);
    }

    #[test]
    fn codex_danger_full_access_sandbox_policy_ignores_network_flag() {
        let policy = codex_danger_full_access_sandbox_policy(true);
        assert_eq!(
            policy.get("type").and_then(Value::as_str),
            Some("dangerFullAccess")
        );
        assert_eq!(policy.get("networkAccess"), None);
    }

    #[test]
    fn codex_read_only_access_mode_sets_writable_cli_and_turn_sandbox() {
        let args = codex_app_server_args(
            BackendAccessMode::ReadOnly,
            BackendExecutionMode::Agent,
            &[],
        );
        assert_eq!(
            args.iter().map(String::as_str).collect::<Vec<_>>()[..3],
            ["--sandbox", "workspace-write", "app-server"]
        );

        // Intentional behavior change: read-only is best-effort guidance, so
        // Codex must allow workspace writes for build/test outputs like target/.
        let policy = codex_sandbox_policy(
            BackendAccessMode::ReadOnly,
            true,
            BackendExecutionMode::Agent,
        );
        assert_eq!(
            policy.get("type").and_then(Value::as_str),
            Some("workspaceWrite")
        );
        assert_eq!(
            policy.get("networkAccess").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn codex_unrestricted_access_mode_keeps_danger_full_sandbox() {
        let args = codex_app_server_args(
            BackendAccessMode::Unrestricted,
            BackendExecutionMode::Agent,
            &[],
        );
        assert_eq!(
            args.iter().map(String::as_str).collect::<Vec<_>>()[..3],
            ["--sandbox", "danger-full-access", "app-server"]
        );

        let policy = codex_sandbox_policy(
            BackendAccessMode::Unrestricted,
            true,
            BackendExecutionMode::Agent,
        );
        assert_eq!(
            policy.get("type").and_then(Value::as_str),
            Some("dangerFullAccess")
        );
        assert_eq!(policy.get("networkAccess"), None);
    }

    #[test]
    fn codex_has_http_mcp_servers_detects_http_transports() {
        assert!(codex_has_http_mcp_servers(&[StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url: "http://127.0.0.1:4012/mcp".to_string(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        }]));
        assert!(!codex_has_http_mcp_servers(&[StartupMcpServer {
            name: "stdio-server".to_string(),
            transport: StartupMcpTransport::Stdio {
                command: "mcp-server".to_string(),
                args: vec!["serve".to_string()],
                env: HashMap::new(),
            },
        }]));
    }

    #[test]
    fn codex_http_mcp_config_includes_url_and_headers() {
        let overrides = codex_mcp_config_overrides(&[StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url: "http://127.0.0.1:4012/mcp?repo_root=%2Ftmp%2Fproj".to_string(),
                headers: HashMap::from([("x-ignored".to_string(), "value".to_string())]),
                bearer_token_env_var: None,
            },
        }]);

        assert!(overrides.iter().any(|entry| {
            entry
                == "mcp_servers.tyde-debug.url=\"http://127.0.0.1:4012/mcp?repo_root=%2Ftmp%2Fproj\""
        }));
        assert!(
            overrides
                .iter()
                .any(|entry| entry == "mcp_servers.tyde-debug.http_headers.x-ignored=\"value\""),
            "expected Codex MCP config to emit HTTP header overrides: {overrides:?}"
        );
    }

    #[test]
    fn codex_only_await_mcp_has_session_scale_deadline() {
        let servers = [
            StartupMcpServer {
                name: "tyde-agent-control".to_string(),
                transport: StartupMcpTransport::Http {
                    url: "http://127.0.0.1:4012/mcp".to_string(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            },
            StartupMcpServer {
                name: AGENT_CONTROL_AWAIT_MCP_SERVER_NAME.to_string(),
                transport: StartupMcpTransport::Http {
                    url: "http://127.0.0.1:4012/await".to_string(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            },
        ];
        let overrides = codex_mcp_config_overrides(&servers);

        assert!(
            !overrides
                .iter()
                .any(|entry| entry.starts_with("mcp_servers.tyde-agent-control.tool_timeout_sec="))
        );
        assert!(overrides.iter().any(|entry| {
            entry
                == &format!(
                    "mcp_servers.{AGENT_CONTROL_AWAIT_MCP_SERVER_NAME}.tool_timeout_sec={CODEX_AGENT_AWAIT_TOOL_TIMEOUT_SECS}"
                )
        }));
    }
}
