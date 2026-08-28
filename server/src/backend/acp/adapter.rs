//! Agent-specific behavior behind the generic ACP session lifecycle.
//!
//! The generic backend in [`super::backend`] owns everything the Agent Client
//! Protocol specifies: `initialize`, `authenticate`, `session/new`,
//! `session/prompt`, `session/cancel`, `session/load`, and the standard
//! `session/update` notification family. An [`AcpAgentAdapter`] supplies only
//! what the protocol does *not* cover for a particular agent — how to find its
//! binary, non-standard request fields, proprietary notification families,
//! stream text that needs sanitizing, and session enumeration for agents that
//! don't implement `session/list`.
//!
//! A conforming agent needs [`adapters::stock::StockAdapter`], which is all
//! defaults. Every method here that has a sensible specification-only answer
//! carries a default implementation, so adding an agent means overriding the
//! handful of things it does differently — not reimplementing a session.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use protocol::{AcpAdapterId, AcpAgentSpec};
use serde_json::Value;

use super::AcpSpawnSpec;
use crate::backend::BackendSession;

/// Working directories for one ACP session.
///
/// `session_cwd` is where the agent process runs; `scope_root` is the
/// workspace root Tyde reports and resolves relative tool paths against. They
/// differ for agents that need a scratch directory outside the workspace.
#[derive(Debug, Clone)]
pub struct AcpSessionRoots {
    pub session_cwd: String,
    pub scope_root: String,
}

/// How the session was requested, so an adapter can pick its directories.
#[derive(Debug, Clone, Copy, Default)]
pub struct AcpSessionKind {
    /// Tyde-internal session that must not appear in the user's session list.
    pub admin_session: bool,
    /// Session whose state is discarded on shutdown.
    pub ephemeral: bool,
}

/// One authentication method advertised by an ACP agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAuthMethod {
    /// Wire identifier sent back to agent-handled authentication.
    pub id: String,
    /// Optional user-facing method name.
    pub name: Option<String>,
    /// Optional user-facing setup instruction.
    pub description: Option<String>,
}

/// How an adapter handles one advertised authentication method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpAuthMethodHandling {
    /// Invoke ACP `authenticate` with the advertised method identifier.
    ProtocolAuthenticate,
    /// Authentication must be completed outside the ACP connection.
    ExternalSetup {
        /// Bounded user-facing instruction describing the required setup.
        instruction: String,
    },
}

/// What the agent told us it can do, from the `initialize` response.
///
/// The generic backend fills this in during negotiation and refuses to use a
/// capability the agent did not advertise — no probing, no optimistic calls
/// that fail late.
#[derive(Debug, Clone, Default)]
pub struct AcpCapabilities {
    /// Protocol version the agent echoed back.
    pub protocol_version: u32,
    /// `agentCapabilities.loadSession` — may we call `session/load`?
    pub load_session: bool,
    /// `promptCapabilities.image` — may prompts carry image content blocks?
    pub image: bool,
    /// `agentCapabilities.sessionCapabilities.list` — may we call
    /// `session/list`? This is the spec's own way to enumerate sessions, so an
    /// agent advertising it needs no adapter support to be resumable.
    pub session_list: bool,
    /// `authMethods`, in the order the agent listed them.
    pub auth_methods: Vec<AcpAuthMethod>,
    /// Agent name/version from `agentInfo`, for diagnostics.
    pub agent_info: Option<String>,
}

/// Per-request context handed to the decorate hooks.
pub struct AcpRequestCtx<'a> {
    pub session_id: &'a str,
    pub model: Option<&'a str>,
    pub mode: Option<&'a str>,
    /// Combined system/steering instructions Tyde wants applied, when the
    /// session has any.
    pub system_prompt: Option<&'a str>,
    pub capabilities: &'a AcpCapabilities,
}

/// A notification an adapter recognized and translated into standard terms.
///
/// Agents that predate or extend the specification emit their own notification
/// families. Rather than teaching the generic backend about each one, the
/// adapter rewrites them into the standard `session/update` shape and the
/// generic dispatch handles them normally.
pub struct NormalizedUpdate {
    /// Standard `sessionUpdate` discriminant, e.g. `agent_message_chunk`.
    pub session_update: &'static str,
    /// Payload in standard `session/update` shape.
    pub params: Value,
}

/// Agent-specific behavior. See the module docs for the division of labor.
pub trait AcpAgentAdapter: Send + Sync + 'static {
    /// Stable identifier, for diagnostics and error messages.
    fn id(&self) -> AcpAdapterId;

    /// Human-readable agent name used in spawn failures and setup errors.
    fn display_name(&self) -> &str;

    /// Resolve the working directories for a session. The default puts the
    /// agent in the first workspace root and reports that same root as scope;
    /// override when the agent needs a scratch directory.
    fn resolve_roots<'a>(
        &'a self,
        workspace_roots: &'a [String],
        ssh_host: Option<&'a str>,
        kind: AcpSessionKind,
    ) -> BoxFuture<'a, Result<AcpSessionRoots, String>>;

    /// Build the process invocation. This is where binary discovery lives —
    /// PATH lookup, sibling-binary resolution, wrapper unwrapping.
    fn spawn_spec(
        &self,
        roots: &AcpSessionRoots,
        ssh_host: Option<&str>,
    ) -> Result<AcpSpawnSpec, String>;

    /// Build a read-only subscription-capacity probe when this specific agent
    /// exposes one outside ACP. The generic ACP protocol has no usage method.
    fn capacity_probe_spec(&self, _roots: &AcpSessionRoots) -> Option<AcpSpawnSpec> {
        None
    }

    /// Client capabilities advertised in `initialize`.
    ///
    /// The default declares filesystem read/write and terminal support, which
    /// [`super::AcpBridge`] implements for every agent. The generic backend
    /// supplies `protocolVersion` and `clientInfo`; an adapter must not set
    /// them.
    fn client_capabilities(&self) -> Value {
        serde_json::json!({
            "fs": { "readTextFile": true, "writeTextFile": true },
            "terminal": true,
        })
    }

    /// Classify an advertised auth method before the generic lifecycle uses it.
    fn auth_method_handling(&self, _method: &AcpAuthMethod) -> AcpAuthMethodHandling {
        AcpAuthMethodHandling::ProtocolAuthenticate
    }

    /// Add non-standard fields to `session/new`. Default: nothing.
    fn decorate_session_new(&self, _params: &mut Value, _ctx: &AcpRequestCtx<'_>) {}

    /// Add non-standard fields to `session/prompt`. Default: nothing.
    fn decorate_prompt(&self, _params: &mut Value, _ctx: &AcpRequestCtx<'_>) {}

    /// Translate a non-standard notification into standard terms, or return
    /// `None` to let the generic dispatch ignore it. Default: recognize
    /// nothing, because a conforming agent only emits `session/update`.
    fn normalize_notification(&self, _method: &str, _params: &Value) -> Option<NormalizedUpdate> {
        None
    }

    /// Clean agent text before it reaches a chat stream. Default: pass through
    /// untouched — an agent that leaks terminal control sequences into its
    /// message content is the exception, not the rule.
    fn sanitize_stream_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }

    /// Enumerate resumable sessions. The default returns none, which is
    /// correct for an agent that neither implements `session/list` nor stores
    /// sessions where Tyde can find them; the generic backend prefers the
    /// protocol method when the agent advertises it.
    fn list_sessions<'a>(
        &'a self,
        _ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<BackendSession>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Hook run immediately before `session/load`, for agents that need
    /// out-of-band cleanup first (stale lock files, for example).
    fn before_session_load<'a>(
        &'a self,
        _session_id: &'a str,
        _ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// Delete a backend-native session.
    fn delete_session<'a>(
        &'a self,
        _session_id: &'a str,
        _ssh_host: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("This ACP agent does not support deleting sessions".to_string()) })
    }

    /// Map an ACP tool call onto a Tyde tool request payload.
    ///
    /// Only `kind` is standardized by ACP; `rawInput` is whatever the agent
    /// chose to send. The default therefore classifies on `kind` but emits a
    /// structured payload *only* when the fields it needs are actually
    /// present, falling back to `Other` rather than inventing an empty path or
    /// command. Override when an agent's `rawInput` shapes are known.
    ///
    /// Async because rendering an edit as a diff means reading the file's
    /// current contents.
    fn map_tool_request<'a>(
        &'a self,
        kind: &'a str,
        args: &'a Value,
        workspace_root: &'a str,
    ) -> BoxFuture<'a, Value> {
        Box::pin(super::tools::default_map_tool_request(
            kind,
            args,
            workspace_root,
        ))
    }

    /// Map an ACP tool result onto a Tyde tool result payload.
    ///
    /// `request_payload` is what [`Self::map_tool_request`] returned for this
    /// call, so a result can be described in terms of the request (an edit's
    /// before/after, a read's file list).
    ///
    /// `rawOutput` is agent-defined, so the default reports failures as
    /// `Error` and everything else as `Other` carrying the raw payload. That
    /// is the most an adapter can honestly say about an unknown agent.
    fn map_tool_result(
        &self,
        completion: &super::AcpToolCallCompletion,
        request_payload: Option<&Value>,
    ) -> Value {
        super::tools::default_map_tool_result(completion, request_payload)
    }

    /// Map a completed native task tool onto an authoritative task snapshot.
    ///
    /// ACP does not standardize task tools, so the default recognizes none.
    /// An adapter may return a snapshot only when the provider result carries
    /// the complete list; the generic backend emits it after the tool result.
    fn map_task_update(
        &self,
        _completion: &super::AcpToolCallCompletion,
        _request_payload: Option<&Value>,
    ) -> Option<protocol::TaskList> {
        None
    }

    /// Interpret whatever usage numbers the agent reported.
    ///
    /// Tyde accounts per provider request. An agent that reports only
    /// prompt- or session-scoped totals cannot answer that, and the honest
    /// result is `TokenUsageScope::Unavailable` rather than a substituted
    /// number from another scope. Default: report nothing.
    fn map_usage(&self, _raw: Option<&Value>) -> Option<protocol::MessageTokenUsage> {
        None
    }

    /// Extra environment for the agent process, merged over the spec's `env`.
    fn extra_env(&self) -> HashMap<String, String> {
        HashMap::new()
    }
}

/// Construct the adapter a launch profile asks for.
pub fn adapter_for_spec(spec: &AcpAgentSpec) -> Arc<dyn AcpAgentAdapter> {
    match spec.adapter {
        AcpAdapterId::Stock => Arc::new(super::adapters::stock::StockAdapter::new(spec.clone())),
        AcpAdapterId::Kiro => Arc::new(super::adapters::kiro::KiroAdapter::new(spec.clone())),
    }
}
