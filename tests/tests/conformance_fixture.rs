//! Harness for `conformance.rs`: boot a host, drive a real provider over the
//! real protocol, hand back the [`Turn`]s it produced.
//!
//! Helpers here return data for the test to judge. They fail only when they
//! cannot produce what was asked for, never on a backend contract — that split
//! is what keeps `conformance.rs` readable as a list of guarantees.

// Suppressed rather than fixed: Cargo compiles this file a second time as a
// test binary with no tests in it, where every item is unreachable.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::FutureExt;
use protocol::{
    AgentActivityStats, AgentActivityStatsPayload, AgentBootstrapEvent, AgentBootstrapPayload,
    AgentCompactPayload, AgentErrorPayload, AgentId, AgentStartPayload, AskUserQuestion,
    BackendCapacityPayload, BackendCapacitySnapshot, BackendCapacityState, BackendKind, ChatEvent,
    ChatMessage, ChatMessageId, ClientErrorPayload, ContextCompactionNotifyPayload,
    ContextCompactionTimelineEvent, Envelope, FetchSessionHistoryPayload, FrameKind,
    HistoryPageRequestId, HostBootstrapPayload, ImageData, ListSessionsPayload, McpServerConfig,
    McpServerId, McpServerUpsertPayload, McpTransportConfig, MessageMetadataUpdateData,
    MessageSender, MessageTokenUsage, NewAgentPayload, QueuedMessagesPayload, SendMessagePayload,
    SendMessageToolResponse, SessionHistoryPayload, SessionId, SessionListPayload,
    SessionSchemaEntry, SessionSchemasPayload, SessionSettingValue, SessionSettingsPayload,
    SessionSettingsSchema, SessionSettingsValues, SessionSummary, SetSessionSettingsPayload, Skill,
    SkillId, SkillNotifyPayload, SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload,
    SpawnCostHint, Steering, SteeringId, SteeringNotifyPayload, SteeringScope,
    SteeringUpsertPayload, StreamPath, TaskList, ToolExecutionCompletedData, ToolExecutionOutcome,
    ToolExecutionResult, ToolRequest, ToolUseData,
};
use serde_json::json;
use tyde_agent_adapter::BackendCapability;
use uuid::Uuid;

/// Control-plane replies do not wait on a model. Kept named because three call
/// sites share it; the per-turn waits are inline at their one use each.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
static CONFORMANCE_RUN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static LEGACY_CODEX_DYNAMIC_AWAIT_ACTIVE: AtomicBool = AtomicBool::new(false);
static CODEX_NESTED_SUBAGENT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Seeded here and asserted in `conformance.rs`; shared so the two cannot drift.
pub const SCRATCH_DIR: &str = "scratch";

/// Every model name the provider may report for the pin this suite configures.
/// The first entry is the value fed to the backend config; the rest are aliases
/// the provider may report instead — Claude is configured as `haiku` and reports
/// its full id. Empty means this backend is not pinned.
///
/// A run that quietly escalates to an expensive model is a bill, not a result,
/// so the value that configures the host and the value the assertion expects
/// have to come from the same place.
pub fn pinned_models(backend: BackendKind) -> Vec<String> {
    match backend {
        BackendKind::Claude => vec!["haiku".to_owned(), "claude-haiku-4-5-20251001".to_owned()],
        // `codex.rs` documents this variable: pinning a different model without
        // a rebuild is how you tell model-specific drift from a real defect.
        BackendKind::Codex => vec![if LEGACY_CODEX_DYNAMIC_AWAIT_ACTIVE.load(Ordering::Relaxed)
            || CODEX_NESTED_SUBAGENT_ACTIVE.load(Ordering::Relaxed)
        {
            "gpt-5.6-sol".to_owned()
        } else {
            env_or("TYDE_CODEX_TEST_MODEL", "gpt-5.6-luna")
        }],
        _ => Vec::new(),
    }
}

/// Hermes takes its model as a per-spawn session setting, not a complexity-tier
/// config, and the value must be the *exact* string the schema publishes as a
/// select option — `validate_session_setting` compares `Select` values by
/// equality. Hermes encodes those options as JSON (`encode_model_option_value`,
/// `hermes.rs:5893`), so the older `"<model> --provider <provider>"` spelling
/// is rejected at startup, before its legacy parser ever sees it.
///
/// The model is not an arbitrary cheap pick. `minimax/minimax-m3` splits the
/// reasoning and content channels one token late, so the opening
/// `Reply with exactly TYDE_READY` handshake came back as `_READY` (or empty)
/// at random — measured against the pre-scrub stream, with `TYDE` sitting at
/// the tail of the *reasoning* channel. Every scenario opens with that
/// handshake, so the slip killed a different unrelated scenario each run and
/// read as per-scenario flake. `deepseek/deepseek-v4-flash` dropped 0 of 28
/// handshakes over two full runs and costs a sixth as much.
fn hermes_session_settings() -> SessionSettingsValues {
    let provider = env_or("TYDE_HERMES_TEST_PROVIDER", "openrouter");
    let model = env_or("TYDE_HERMES_TEST_MODEL", "deepseek/deepseek-v4-flash");
    let mut values = SessionSettingsValues::default();
    values.0.insert(
        "model".to_owned(),
        SessionSettingValue::String(json!({"model": model, "provider": provider}).to_string()),
    );
    // Not "none": that switched reasoning off outright, so every scenario ran
    // with the reasoning path dark and `ReasoningDeltas` went unasserted for
    // Hermes on any model. "low" keeps the cost near the floor while leaving
    // the channel live.
    values.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String(env_or("TYDE_HERMES_TEST_REASONING", "low")),
    );
    values
}

/// Point the MCP bridge at the `tyde-server` this checkout built.
///
/// `resolve_bridge_executable` uses the running executable when it is a Tyde
/// binary and otherwise falls back to the installed
/// `~/.tyde/bin/current/tyde-server`. Under nextest the running executable is a
/// test harness — cloned outside `target/` by the macOS wrapper, so it has no
/// sibling to find — and that fallback runs whatever release happens to be
/// installed.
///
/// Which is a trap, and it has already sprung once: `cargo nextest run -p
/// tests` builds the `server` library but not the `tyde-server` binary, so a
/// fresh checkout has no local build, every Antigravity MCP scenario silently
/// ran an older bridge, and the failure read as "the model never called the
/// tool" — a plausible-looking model problem with no hint that the bridge under
/// test was never involved. So a missing local build is an error here, not a
/// fallback.
fn require_locally_built_mcp_bridge(backends: &[BackendKind]) {
    const ENV: &str = "TYDE_HERMES_BRIDGE_EXECUTABLE";
    // Only the backends whose MCP goes through the bridge care.
    if !backends
        .iter()
        .any(|kind| matches!(kind, BackendKind::Antigravity | BackendKind::Hermes))
    {
        return;
    }
    if std::env::var_os(ENV).is_some() {
        return;
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests crate has a workspace parent");
    let binary = if cfg!(windows) {
        "tyde-server.exe"
    } else {
        "tyde-server"
    };
    let candidates = ["debug", "release"]
        .into_iter()
        .map(|profile| workspace.join("target").join(profile).join(binary))
        .collect::<Vec<_>>();
    let built = candidates.iter().find(|path| path.is_file());
    let Some(built) = built else {
        panic!(
            "the conformance suite needs this checkout's MCP bridge, and none is built.\n\
             Run `cargo build -p tyde-server`, then re-run.\n\
             Looked for: {candidates:?}\n\
             Without it the suite would fall back to the installed release, testing a bridge \
             built from different code than the one under test."
        );
    };
    // SAFETY: set once, before any scenario spawns a backend.
    unsafe { std::env::set_var(ENV, built) };
}

/// Selection only. Whether a backend can actually run is the server's question,
/// and it answers it authoritatively when the spawn fails — a check here would
/// be a second, divergent copy of six different installation rules.
fn enabled_backends() -> Vec<BackendKind> {
    assert_eq!(
        std::env::var("TYDE_RUN_REAL_AI_TESTS").ok().as_deref(),
        Some("1"),
        "set TYDE_RUN_REAL_AI_TESTS=1 to authorize the paid conformance suite"
    );
    match std::env::var("TYDE_REAL_BACKENDS") {
        // Every supported backend. `BackendKind` has no enumeration to derive
        // this from — the server hand-writes the same list in
        // `backend_config_schema_catalog`.
        Err(_) => vec![
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Kiro,
            BackendKind::Hermes,
            BackendKind::Antigravity,
        ],
        Ok(configured) => {
            let mut selected = Vec::new();
            for value in configured
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                let backend = match value.to_ascii_lowercase().as_str() {
                    "claude" => BackendKind::Claude,
                    "codex" => BackendKind::Codex,
                    "kiro" | "acp" => BackendKind::Kiro,
                    "hermes" => BackendKind::Hermes,
                    "antigravity" | "agy" => BackendKind::Antigravity,
                    other => panic!("unknown backend {other:?} in TYDE_REAL_BACKENDS"),
                };
                if !selected.contains(&backend) {
                    selected.push(backend);
                }
            }
            assert!(!selected.is_empty(), "TYDE_REAL_BACKENDS selected nothing");
            selected
        }
    }
}

/// One prompt and everything the client received in response to it.
///
/// Carries the prompt so a failure names the turn that broke by what it asked
/// for. An index into a conversation would not survive reordering, and would
/// tell the reader nothing.
pub struct Turn {
    backend: BackendKind,
    prompt: String,
    events: Vec<ChatEvent>,
    /// Activity-stats snapshots seen while this turn ran. Some evidence never
    /// rides on a `ChatEvent` -- `current_context_usage` is reported on the
    /// agent's stats frame -- so a turn that only collected chat events could
    /// not assert on it at all.
    activity_stats: Vec<AgentActivityStats>,
}

impl Turn {
    pub fn activity_stats(&self) -> &[AgentActivityStats] {
        &self.activity_stats
    }

    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// Whether the backend that produced this turn claims a capability.
    ///
    /// Mirrors `ConformanceHost::declares` for assertions that run without a
    /// host in hand, so a check can gate on a declaration rather than on
    /// whether the data it wanted happens to be present. Gating on the data is
    /// how an assertion excuses itself from the very defect it exists to catch.
    pub fn declares(&self, capability: BackendCapability) -> bool {
        server::backend::capabilities_for_backend_kind(self.backend).contains(capability)
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    pub fn user_messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User) => {
                Some(message)
            }
            _ => None,
        })
    }

    /// Prefix for this turn's assertion failures. Not an identity — it names the
    /// turn by what it asked for, which an index into the conversation could not.
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{} turn {prompt:?}", backend_label(self.backend))
    }

    pub fn tool_requests(&self) -> impl Iterator<Item = &ToolRequest> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolRequest(request) => Some(request),
            _ => None,
        })
    }

    /// The assistant messages the client materialized, in stream order.
    ///
    /// Each one is supposed to be exactly one provider response, and its
    /// `tool_calls` are the calls that response issued — the client's only
    /// handle on which response a tool card belongs to.
    pub fn assistant_messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::StreamEnd(end) => Some(&end.message),
            _ => None,
        })
    }

    pub fn tool_completions(&self) -> impl Iterator<Item = &ToolExecutionCompletedData> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolExecutionCompleted(completion) => Some(completion),
            _ => None,
        })
    }

    /// Every tool call an assistant response declared, in stream order.
    ///
    /// [`Turn::tool_requests`] carries Tyde's *normalized* executable form,
    /// which deliberately drops the provider's own tool name — a `ToolRequest`
    /// says "run this command", not "the model called `mcp__probe__record`".
    /// The declaration is the only place the provider name and the raw
    /// arguments survive, so anything asserting on which tool the model picked
    /// or what it passed has to read them from here.
    pub fn tool_declarations(&self) -> impl Iterator<Item = &ToolUseData> {
        self.events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::StreamEnd(end) => Some(&end.message.tool_calls),
                ChatEvent::MessageAdded(message) => Some(&message.tool_calls),
                _ => None,
            })
            .flatten()
    }

    /// The provider tool name behind a request, or `None` if no response in the
    /// turn declared it. `assert_every_request_was_declared` is what turns that
    /// `None` into a failure; callers here can assume a declared request.
    pub fn declared_name(&self, tool_call_id: &str) -> Option<&str> {
        self.tool_declarations()
            .find(|call| call.tool_call_id == tool_call_id)
            .map(|call| call.name.as_str())
    }

    /// Failure-message material. Nothing asserts on it.
    pub fn tool_request_names(&self) -> Vec<String> {
        self.tool_requests()
            .map(|request| format!("{}({})", tool_kind(request), request.tool_call_id))
            .collect()
    }

    /// Failure-message material. Nothing asserts on it — the outcome is
    /// summarised rather than `Debug`-printed because the full result payload
    /// buries the one thing that matters, which tool produced what.
    pub fn completion_summaries(&self) -> Vec<String> {
        self.tool_completions()
            .map(|completion| {
                let outcome = match &completion.outcome {
                    ToolExecutionOutcome::Succeeded { result } => {
                        format!("ok:{}", result_kind(result))
                    }
                    ToolExecutionOutcome::Failed { message, .. } => format!("failed:{message}"),
                    ToolExecutionOutcome::Cancelled { message } => format!("cancelled:{message}"),
                };
                format!("{}=>{outcome}", completion.tool_call_id)
            })
            .collect()
    }

    /// Everything the turn streamed, whether or not it became a message.
    ///
    /// [`Turn::final_text`] reads the assembled `StreamEnd`, which a cancelled
    /// turn is required *not* to produce: the partial deltas of an aborted
    /// response never become a message. This is the only view of what the user
    /// actually watched appear.
    pub fn streamed_text(&self) -> String {
        self.events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::StreamDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The text the user ends up looking at. Falls back to accumulated deltas
    /// for backends whose `StreamEnd` omits the assembled content.
    pub fn final_text(&self) -> String {
        let mut streamed = String::new();
        let mut last_final = String::new();
        for event in &self.events {
            match event {
                ChatEvent::StreamStart(_) => streamed.clear(),
                ChatEvent::StreamDelta(delta) => streamed.push_str(&delta.text),
                ChatEvent::StreamEnd(end) => {
                    let content = if end.message.content.trim().is_empty() {
                        streamed.trim().to_owned()
                    } else {
                        end.message.content.clone()
                    };
                    if !content.trim().is_empty() {
                        last_final = content;
                    }
                }
                _ => {}
            }
        }
        last_final
    }

    /// Late metadata applied on top of what `StreamEnd` carried, one entry per
    /// response that ended up with a value.
    ///
    /// Reading `StreamEnd` alone would miss every backend that reports usage
    /// after the response is assembled, which is the ordinary case rather than
    /// the exception: a provider knows its output count once the request
    /// finishes, not while it is still streaming.
    fn merged_metadata<T: Clone>(
        &self,
        on_message: impl Fn(&ChatMessage) -> Option<&T>,
        on_update: impl Fn(&MessageMetadataUpdateData) -> Option<&T>,
    ) -> Vec<T> {
        let mut responses: Vec<(Option<ChatMessageId>, Option<T>)> = Vec::new();
        for event in &self.events {
            match event {
                ChatEvent::StreamEnd(end) => responses.push((
                    end.message.message_id.clone(),
                    on_message(&end.message).cloned(),
                )),
                ChatEvent::MessageMetadataUpdated(update) => {
                    if let Some(value) = on_update(update)
                        && let Some(slot) = responses
                            .iter_mut()
                            .find(|(id, _)| id.as_ref() == Some(&update.message_id))
                    {
                        slot.1 = Some(value.clone());
                    }
                }
                _ => {}
            }
        }
        responses
            .into_iter()
            .filter_map(|(_, value)| value)
            .collect()
    }

    /// Token usage per provider response, as the client finally holds it.
    pub fn reported_usage(&self) -> Vec<MessageTokenUsage> {
        self.merged_metadata(
            |message| message.token_usage.as_ref(),
            |update| update.token_usage.as_ref(),
        )
    }

    /// Every task list this turn pushed, in order.
    pub fn task_updates(&self) -> impl Iterator<Item = &TaskList> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::TaskUpdate(list) => Some(list),
            _ => None,
        })
    }
}

pub fn tool_kind(request: &ToolRequest) -> &'static str {
    use protocol::ToolRequestType as T;
    match request.tool_type {
        T::ModifyFile { .. } => "modify_file",
        T::RunCommand { .. } => "run_command",
        T::ReadFiles { .. } => "read_files",
        T::SearchTypes { .. } => "search_types",
        T::GetTypeDocs { .. } => "get_type_docs",
        T::AskUserQuestion { .. } => "ask_user_question",
        T::ExitPlanMode { .. } => "exit_plan_mode",
        T::AgentSpawn { .. } => "agent_spawn",
        T::GenerateImage { .. } => "generate_image",
        T::WebSearch { .. } => "web_search",
        T::ViewImage { .. } => "view_image",
        T::Sleep { .. } => "sleep",
        T::TydeSendAgentMessage { .. } => "tyde_send_agent_message",
        T::TydeAwaitAgents { .. } => "tyde_await_agents",
        T::Other { .. } => "other",
    }
}

fn result_kind(result: &ToolExecutionResult) -> &'static str {
    use ToolExecutionResult as R;
    match result {
        R::ModifyFile { .. } => "modify_file",
        R::RunCommand { .. } => "run_command",
        R::ReadFiles { .. } => "read_files",
        R::SearchTypes { .. } => "search_types",
        R::GetTypeDocs { .. } => "get_type_docs",
        R::TydeSendAgentMessage => "tyde_send_agent_message",
        R::TydeAwaitAgents { .. } => "tyde_await_agents",
        R::GenerateImage { .. } => "generate_image",
        R::WebSearch => "web_search",
        R::ViewImage => "view_image",
        R::Sleep => "sleep",
        R::Other { .. } => "other",
    }
}

pub struct Host {
    client: client::Connection,
    handle: server::HostHandle,
    backend_kind: BackendKind,
    store: PathBuf,
    workspace: PathBuf,
    latest_capacity: HashMap<BackendKind, BackendCapacitySnapshot>,
}

/// `git worktree add` is the only way to reach the CLI's session relocation, and
/// it needs a repository with a commit behind it. Every workspace gets one so
/// the worktree scenario does not need a differently-shaped fixture.
fn init_workspace_repo(workspace: &Path) {
    for args in [
        ["init", "-q", "-b", "main"].as_slice(),
        ["config", "user.email", "conformance@tyde.test"].as_slice(),
        ["config", "user.name", "Tyde Conformance"].as_slice(),
        ["add", "-A"].as_slice(),
        ["commit", "-q", "-m", "conformance workspace"].as_slice(),
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .unwrap_or_else(|err| panic!("run git {args:?}: {err}"));
        assert!(
            output.status.success(),
            "git {args:?} failed seeding the conformance workspace: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Add a worktree the agent can be asked to enter, and hand back its path.
pub fn add_worktree(host: &Host, name: &str) -> PathBuf {
    let path = host.workspace().join(".claude/worktrees").join(name);
    let output = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            &path.to_string_lossy(),
            "-b",
            name,
            "HEAD",
        ])
        .current_dir(host.workspace())
        .output()
        .expect("run git worktree add");
    assert!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

/// Where Claude keeps a session for a given working directory, mirroring
/// `claude.rs`'s `claude_session_file_path`: canonicalize, then collapse
/// separators, dots and underscores to `-`.
///
/// The scenario needs this because the relocation is invisible from the event
/// stream — the file simply stops being under one directory and starts being
/// under another.
pub fn claude_session_file(cwd: &Path, session_id: &SessionId) -> PathBuf {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let encoded: String = canonical
        .to_string_lossy()
        .trim()
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | ':' | '.' | '_') {
                '-'
            } else {
                ch
            }
        })
        .collect();
    PathBuf::from(std::env::var("HOME").expect("HOME must be set to locate Claude sessions"))
        .join(".claude")
        .join("projects")
        .join(encoded)
        .join(format!("{}.jsonl", session_id.0))
}

impl Host {
    async fn new(backend_kind: BackendKind, store: &Path, workspace: &Path) -> Self {
        std::fs::write(workspace.join("README.txt"), "tyde conformance workspace")
            .expect("seed workspace");
        // Something for the destructive-command turn to remove. A directory
        // rather than a file because the providers that gate risky commands
        // gate on *recursion* — a plain `rm <file>` passes every such check, so
        // a scenario built on one asserts nothing about the gate.
        std::fs::create_dir_all(workspace.join(SCRATCH_DIR)).expect("seed scratch directory");
        std::fs::write(workspace.join(SCRATCH_DIR).join("notes.txt"), "scratch")
            .expect("seed scratch file");
        init_workspace_repo(workspace);

        let settings_path = store.join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&host_settings(backend_kind)).expect("serialize settings"),
        )
        .expect("seed settings store");

        let handle = server::spawn_host_with_store_paths(
            store.join("sessions.json"),
            store.join("projects.json"),
            settings_path,
        )
        .expect("initialize host with real backends");

        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();
        let client_config = client::ClientConfig::current();
        let connection_host = handle.clone();
        tokio::spawn(async move {
            let conn = server::accept(&server_config, server_stream)
                .await
                .expect("server handshake failed");
            if let Err(err) = server::run_connection(conn, connection_host).await {
                eprintln!("server connection loop failed: {err:?}");
            }
        });
        let client = client::connect(&client_config, client_stream)
            .await
            .expect("client handshake failed");

        Self {
            client,
            handle,
            backend_kind,
            store: store.to_path_buf(),
            workspace: workspace.to_path_buf(),
            latest_capacity: HashMap::new(),
        }
    }

    pub fn backend(&self) -> BackendKind {
        self.backend_kind
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Whether this backend claims a capability, for scenarios that gate a
    /// single turn rather than the whole run.
    ///
    /// `run_scenario` gates entire scenarios the same way. A scenario that
    /// walks several tools needs the finer grain: Codex has no file-reading
    /// tool and does not declare `GenericReadFiles` — it reads through the
    /// shell — so a read turn is a question it cannot answer, while every other
    /// turn around it is still worth asserting.
    pub fn declares(&self, capability: BackendCapability) -> bool {
        server::backend::capabilities_for_backend_kind(self.backend_kind).contains(capability)
    }

    pub fn workspace_roots(&self) -> Vec<String> {
        vec![self.workspace.to_string_lossy().into_owned()]
    }

    async fn next_envelope(&mut self, timeout: Duration, context: &str) -> Envelope {
        let backend_kind = self.backend_kind;
        match tokio::time::timeout(timeout, self.client.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                if envelope.kind == FrameKind::BackendCapacity {
                    let payload: BackendCapacityPayload = envelope
                        .parse_payload()
                        .expect("parse BackendCapacityPayload");
                    for snapshot in payload.snapshots {
                        self.latest_capacity.insert(snapshot.backend_kind, snapshot);
                    }
                }
                envelope
            }
            Ok(Ok(None)) => panic!("connection closed while waiting for {context}"),
            Ok(Err(error)) => panic!("next_event failed waiting for {context}: {error:?}"),
            Err(_) => panic!(
                "{backend_kind:?} timed out after {}s waiting for {context}",
                timeout.as_secs()
            ),
        }
    }

    pub async fn await_known_capacity(&mut self) -> BackendCapacitySnapshot {
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        loop {
            if let Some(snapshot) = self.latest_capacity.get(&self.backend_kind)
                && matches!(snapshot.state, BackendCapacityState::Known { .. })
            {
                return snapshot.clone();
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "{:?}: timed out waiting for known subscription capacity; latest state: {:?}",
                self.backend_kind,
                self.latest_capacity.get(&self.backend_kind)
            );
            let _ = self.next_envelope(remaining, "BackendCapacity").await;
        }
    }
}

pub async fn await_session_schema(host: &mut Host) -> SessionSettingsSchema {
    loop {
        let envelope = host
            .next_envelope(CONTROL_TIMEOUT, "session settings schema")
            .await;
        fail_on_client_error(&envelope, "await_session_schema");
        let schemas = match envelope.kind {
            FrameKind::HostBootstrap => {
                envelope
                    .parse_payload::<HostBootstrapPayload>()
                    .expect("parse HostBootstrap")
                    .session_schemas
            }
            FrameKind::SessionSchemas => {
                envelope
                    .parse_payload::<SessionSchemasPayload>()
                    .expect("parse SessionSchemas")
                    .schemas
            }
            _ => continue,
        };
        let Some(entry) = schemas
            .into_iter()
            .find(|entry| entry.backend_kind() == host.backend_kind)
        else {
            continue;
        };
        match entry {
            SessionSchemaEntry::Ready { schema } => return schema,
            SessionSchemaEntry::Pending { .. } => continue,
            SessionSchemaEntry::Unavailable { message, .. } => {
                panic!(
                    "{:?}: session settings schema unavailable: {message}",
                    host.backend()
                )
            }
        }
    }
}

pub async fn set_session_setting(
    host: &mut Host,
    agent: &Agent,
    key: &str,
    value: &str,
) -> SessionSettingsValues {
    let mut update = SessionSettingsValues::default();
    update.0.insert(
        key.to_string(),
        SessionSettingValue::String(value.to_string()),
    );
    host.client
        .set_session_settings(&agent.stream, SetSessionSettingsPayload { values: update })
        .await
        .expect("set_session_settings failed");

    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SessionSettings").await;
        fail_on_agent_error(&envelope, "set_session_setting");
        if envelope.stream != agent.stream || envelope.kind != FrameKind::SessionSettings {
            continue;
        }
        let payload: SessionSettingsPayload = envelope
            .parse_payload()
            .expect("parse SessionSettingsPayload");
        if payload.values.0.get(key) == Some(&SessionSettingValue::String(value.to_string())) {
            return payload.values;
        }
    }
}

/// Install one host skill and wait until the running host has rescanned it.
pub async fn install_skill(host: &mut Host, name: &str, description: &str, body: &str) {
    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "HostBootstrap").await;
        fail_on_client_error(&envelope, "install_skill bootstrap");
        if envelope.kind == FrameKind::HostBootstrap {
            break;
        }
    }
    let skill = Skill {
        id: SkillId(name.to_owned()),
        name: name.to_owned(),
        title: None,
        description: Some(description.to_owned()),
    };
    let skill_dir = host.store.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).expect("create conformance skill directory");
    std::fs::write(
        skill_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&skill).expect("serialize conformance skill metadata"),
    )
    .expect("write conformance skill metadata");
    std::fs::write(skill_dir.join("SKILL.md"), body).expect("write conformance skill body");
    host.client
        .skill_refresh(SkillRefreshPayload::default())
        .await
        .expect("skill_refresh failed");
    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SkillNotify").await;
        fail_on_client_error(&envelope, "install_skill");
        if envelope.kind != FrameKind::SkillNotify {
            continue;
        }
        let notify: SkillNotifyPayload =
            envelope.parse_payload().expect("parse SkillNotifyPayload");
        if notify
            == (SkillNotifyPayload::Upsert {
                skill: skill.clone(),
            })
        {
            break;
        }
    }
}

/// Register a stdio MCP server with the host, and wait for it to be stored.
///
/// Call this *before* spawning the agent that should see it. `resolve_spawn_config`
/// reads the MCP store once, at spawn, to build the backend's launch
/// configuration (`host.rs:4260`), so a server registered afterwards reaches the
/// next agent rather than this one.
///
/// Waits for the resulting `McpServerNotify` rather than returning once the
/// frame is written: `mcp_server_upsert` is a one-way send, and a rejected
/// upsert — a reserved name, a store failure — would otherwise show up much
/// later as a model that never called the tool.
pub async fn install_mcp_server(host: &mut Host, name: &str, command: &str, args: Vec<String>) {
    host.client
        .mcp_server_upsert(McpServerUpsertPayload {
            mcp_server: McpServerConfig {
                id: McpServerId(format!("conformance-{name}")),
                name: name.to_owned(),
                // False so that whether the calls arrive together is decided by
                // the model and the backend, not by a hint each backend
                // translates differently.
                supports_parallel_tool_calls: false,
                transport: McpTransportConfig::Stdio {
                    command: command.to_owned(),
                    args,
                    env: HashMap::new(),
                },
            },
        })
        .await
        .expect("mcp_server_upsert failed");
    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "McpServerNotify").await;
        fail_on_client_error(&envelope, "install_mcp_server");
        if envelope.kind == FrameKind::McpServerNotify {
            break;
        }
    }
}

/// Install host-wide steering before a backend resolves its spawn configuration.
pub async fn install_host_steering(host: &mut Host, content: &str) {
    let steering = Steering {
        id: SteeringId(format!("conformance-{}", Uuid::new_v4())),
        scope: SteeringScope::Host,
        title: "AGENTS.md conformance".to_owned(),
        content: content.to_owned(),
    };
    host.client
        .steering_upsert(SteeringUpsertPayload {
            steering: steering.clone(),
        })
        .await
        .expect("steering_upsert failed");
    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SteeringNotify").await;
        fail_on_client_error(&envelope, "install_host_steering");
        if envelope.kind != FrameKind::SteeringNotify {
            continue;
        }
        let notify: SteeringNotifyPayload = envelope
            .parse_payload()
            .expect("parse SteeringNotifyPayload");
        assert_eq!(
            notify,
            SteeringNotifyPayload::Upsert {
                steering: steering.clone(),
            },
            "host acknowledged a different steering mutation"
        );
        break;
    }
}

pub struct Agent {
    agent_id: AgentId,
    stream: StreamPath,
    /// What the server replayed in `AgentBootstrap` before the first live event:
    /// empty for a fresh spawn, the restored conversation for a resume.
    pub replayed_history: Vec<ChatEvent>,
}

pub async fn spawn_agent(host: &mut Host, prompt: &str) -> Agent {
    let backend_kind = host.backend_kind;
    let workspace_roots = host.workspace_roots();
    host.client
        .spawn_agent(SpawnAgentPayload {
            name: Some("conformance".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots,
                prompt: prompt.to_owned(),
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: Some(SpawnCostHint::Low),
                access_mode: Default::default(),
                session_settings: (backend_kind == BackendKind::Hermes)
                    .then(hermes_session_settings),
            },
        })
        .await
        .expect("spawn_agent failed");
    await_agent_start(host, "spawn").await
}

pub async fn resume_agent(host: &mut Host, session_id: &SessionId) -> Agent {
    host.client
        .spawn_agent(SpawnAgentPayload {
            name: Some("conformance-resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume spawn_agent failed");
    await_agent_start(host, "resume").await
}

async fn await_agent_start(host: &mut Host, context: &str) -> Agent {
    let backend_kind = host.backend_kind;
    let mut new_agent: Option<NewAgentPayload> = None;
    let mut replayed_history = Vec::new();

    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, context).await;
        fail_on_agent_error(&envelope, context);

        if envelope.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = envelope.parse_payload().expect("parse NewAgent");
            assert_eq!(
                payload.backend_kind, backend_kind,
                "{context}: server started the wrong backend"
            );
            new_agent = Some(payload);
            continue;
        }

        let Some(agent) = new_agent.as_ref() else {
            continue;
        };
        if envelope.stream != agent.instance_stream {
            continue;
        }

        let started = match envelope.kind {
            FrameKind::AgentStart => {
                let _: AgentStartPayload = envelope.parse_payload().expect("parse AgentStart");
                true
            }
            FrameKind::AgentBootstrap => {
                let payload: AgentBootstrapPayload =
                    envelope.parse_payload().expect("parse AgentBootstrap");
                let mut started = false;
                for event in payload.events {
                    match event {
                        AgentBootstrapEvent::ChatEvent(event) => replayed_history.push(event),
                        AgentBootstrapEvent::AgentStart(_) => started = true,
                        AgentBootstrapEvent::AgentError(error) => {
                            panic!("{context}: agent failed to start: {}", error.message)
                        }
                        _ => {}
                    }
                }
                started
            }
            _ => false,
        };

        if started {
            return Agent {
                agent_id: agent.agent_id.clone(),
                stream: agent.instance_stream.clone(),
                replayed_history,
            };
        }
    }
}

/// Send without collecting, so the caller can do something to an agent that is
/// still mid-turn.
pub async fn send_prompt(host: &mut Host, agent: &Agent, prompt: &str) {
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");
}

pub async fn ask(host: &mut Host, agent: &Agent, prompt: impl AsRef<str>) -> Turn {
    let prompt = prompt.as_ref();
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");
    collect_turn(host, agent, prompt).await
}

pub async fn ask_with_images(
    host: &mut Host,
    agent: &Agent,
    prompt: &str,
    images: Vec<ImageData>,
) -> Turn {
    host.client
        .send_message_payload(
            &agent.stream,
            SendMessagePayload {
                message: prompt.to_owned(),
                images: Some(images),
                origin: None,
                tool_response: None,
            },
        )
        .await
        .expect("image send_message failed");
    collect_turn(host, agent, prompt).await
}

/// Every chat event from the user echo through the turn going idle. Going idle
/// with no assistant response at all is a failure, not a turn.
pub async fn collect_turn(host: &mut Host, agent: &Agent, prompt: &str) -> Turn {
    let backend = host.backend_kind;
    let label = backend_label(backend);
    let context = format!("{label} turn for prompt {prompt:?}");
    let mut events = Vec::new();
    let mut activity_stats = Vec::new();
    let mut saw_stream_end = false;

    // A backend fast enough to echo the prompt before the client subscribes
    // delivers that echo on the `AgentBootstrap` frame instead of live, and
    // `await_agent_start` has already drained it into `replayed_history`.
    // Measured 2026-08-25: Antigravity publishes the echo at zero subscribers
    // because its `spawn` completes the whole provider handshake before
    // returning, while Claude publishes at one. Both clients see the message —
    // the agent replays chat history to a late subscriber
    // (`attach_subscriber_with_latest_output`) — so waiting only on the live
    // stream makes this a race on backend startup latency rather than a check
    // of anything. The turn still has to reach `StreamEnd` and go idle below.
    let echoed_in_bootstrap = agent
        .replayed_history
        .iter()
        .position(|event| is_user_echo(event, prompt));
    let mut saw_echo = echoed_in_bootstrap.is_some();
    if let Some(start) = echoed_in_bootstrap {
        events.extend(agent.replayed_history[start..].iter().cloned());
    }

    loop {
        // Sized for several model round trips plus real tool execution.
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }

        if envelope.kind == FrameKind::AgentActivityStats {
            let payload: AgentActivityStatsPayload =
                envelope.parse_payload().expect("parse AgentActivityStats");
            activity_stats.push(payload.stats);
            continue;
        }

        for event in chat_events_in(&envelope) {
            eprintln!("{label} {event:?}");

            if is_user_echo(&event, prompt) {
                saw_echo = true;
            }
            if !saw_echo {
                continue;
            }

            let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
            if matches!(event, ChatEvent::StreamEnd(_)) {
                saw_stream_end = true;
            }
            events.push(event);

            if idle {
                assert!(
                    saw_stream_end,
                    "{context}: backend went idle without producing any assistant response"
                );
                return Turn {
                    backend,
                    prompt: prompt.to_owned(),
                    events,
                    activity_stats,
                };
            }
        }
    }
}

/// Collect a native-subagent turn through every spawned child's completion.
/// Detached provider-native children can outlive the first idle boundary, so
/// the ordinary turn collector cannot establish their lifecycle contract.
fn native_subagent_lifecycle_complete(
    turn: &Turn,
    spawned: &[String],
    final_markers: &[&str],
) -> bool {
    spawned.iter().all(|tool_call_id| {
        turn.tool_completions()
            .any(|completion| &completion.tool_call_id == tool_call_id)
    }) && turn.assistant_messages().last().is_some_and(|message| {
        final_markers
            .iter()
            .all(|marker| message.content.contains(marker))
    })
}

pub async fn collect_native_subagent_turn(
    host: &mut Host,
    agent: &Agent,
    prompt: &str,
    final_markers: &[&str],
) -> Turn {
    let mut turn = collect_turn(host, agent, prompt).await;
    let spawned = turn
        .tool_requests()
        .filter(|request| {
            matches!(
                request.tool_type,
                protocol::ToolRequestType::AgentSpawn { .. }
            )
        })
        .map(|request| request.tool_call_id.clone())
        .collect::<Vec<_>>();
    if spawned.is_empty() || native_subagent_lifecycle_complete(&turn, &spawned, final_markers) {
        return turn;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return turn;
        }
        let envelope = match tokio::time::timeout(remaining, host.client.next_event()).await {
            Ok(Ok(Some(envelope))) => envelope,
            Ok(Ok(None)) => return turn,
            Err(_) => return turn,
            Ok(Err(error)) => panic!("native subagent settle next_event failed: {error:?}"),
        };
        fail_on_agent_error(&envelope, "native subagent settle");
        fail_on_client_error(&envelope, "native subagent settle");
        if envelope.stream != agent.stream {
            continue;
        }
        if envelope.kind == FrameKind::AgentActivityStats {
            let payload: AgentActivityStatsPayload = envelope
                .parse_payload()
                .expect("parse native subagent AgentActivityStats");
            turn.activity_stats.push(payload.stats);
            continue;
        }
        for event in chat_events_in(&envelope) {
            eprintln!("{} {event:?}", backend_label(turn.backend));
            let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
            turn.events.push(event);
            if idle && native_subagent_lifecycle_complete(&turn, &spawned, final_markers) {
                return turn;
            }
        }
    }
}

/// One agent-control spawn: the parent's turn, and the child the host made
/// while it ran.
pub struct Delegation {
    parent: Turn,
    child: Turn,
    child_agent: NewAgentPayload,
}

impl Delegation {
    pub fn parent(&self) -> &Turn {
        &self.parent
    }

    pub fn child(&self) -> &Turn {
        &self.child
    }

    /// What the *host* recorded about the child, which is what makes it usable
    /// as an oracle: it is written by the registry when the agent is created,
    /// independent of anything the parent's cards claim happened.
    pub fn child_agent(&self) -> &NewAgentPayload {
        &self.child_agent
    }

    /// Every message the child was handed. The only place the prompt that
    /// actually reached the child can be read.
    pub fn child_inputs(&self) -> Vec<&str> {
        self.child
            .events()
            .iter()
            .filter_map(|event| match event {
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::User) =>
                {
                    Some(message.content.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// Both turns, for the checks that run over a whole conversation. A child's
    /// stream is an agent's stream like any other and owes the same guarantees.
    pub fn into_turns(self) -> [Turn; 2] {
        [self.parent, self.child]
    }
}

/// A child boots a second provider process and runs a turn while the parent is
/// still finishing its own, so this covers two turns and a process launch.
const DELEGATION_TIMEOUT: Duration = Duration::from_secs(360);

/// Send `prompt` and follow both sides of the delegation it should produce.
///
/// Two streams advance at once: the child's first turn overlaps whatever the
/// parent does after the spawn tool returns, and either can finish first, so
/// this cannot be [`collect_turn`] run twice.
///
/// The child is identified by the host's own `NewAgent` frame rather than by
/// anything the parent's cards say about it. That is the whole point — a card
/// naming an agent that was never created, or a child created with arguments
/// the card never showed, is exactly what this is for.
///
/// `child_prompt` only labels the child's turn; what the child was actually
/// given is read back off its stream.
pub async fn delegate(
    host: &mut Host,
    parent: &Agent,
    prompt: &str,
    child_prompt: &str,
) -> Delegation {
    let backend = host.backend_kind;
    let label = backend_label(backend);
    send_prompt(host, parent, prompt).await;

    let mut parent_events = Vec::new();
    let mut saw_echo = false;
    let mut saw_stream_end = false;
    let mut parent_idle = false;

    let mut child_agent: Option<NewAgentPayload> = None;
    let mut child_events = Vec::new();
    let mut child_stream_end = false;
    let mut child_idle = false;

    let deadline = tokio::time::Instant::now() + DELEGATION_TIMEOUT;
    while !(parent_idle && child_idle) {
        // Rebuilt each iteration so a timeout names the half that is missing.
        // "Timed out waiting for a delegation" would leave the reader unable to
        // tell a child that was never created from one that never answered.
        let context = format!(
            "{label} delegation for prompt {prompt:?} (parent idle: {parent_idle}, child created: \
             {}, child idle: {child_idle})",
            child_agent.is_some()
        );
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "{context}: gave up");
        let envelope = host.next_envelope(remaining, &context).await;
        fail_on_agent_error(&envelope, &context);

        if envelope.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = envelope.parse_payload().expect("parse NewAgent");
            if payload.parent_agent_id.as_ref() == Some(&parent.agent_id) {
                assert!(
                    child_agent.replace(payload).is_none(),
                    "{context}: the host created a second child for this parent"
                );
            }
            continue;
        }

        if envelope.stream == parent.stream {
            // Anything after the turn went idle belongs to a later turn, not
            // this one. Shutdown-time violations are `assert_clean_close`'s.
            if parent_idle {
                continue;
            }
            for event in chat_events_in(&envelope) {
                eprintln!("{label} parent {event:?}");
                if let ChatEvent::MessageAdded(message) = &event
                    && matches!(message.sender, MessageSender::User)
                    && message.content.contains(prompt)
                {
                    saw_echo = true;
                }
                if !saw_echo {
                    continue;
                }
                if matches!(event, ChatEvent::StreamEnd(_)) {
                    saw_stream_end = true;
                }
                let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
                parent_events.push(event);
                if idle {
                    assert!(
                        saw_stream_end,
                        "{context}: the parent went idle without producing any assistant response"
                    );
                    parent_idle = true;
                }
            }
            continue;
        }

        let Some(agent) = child_agent.as_ref() else {
            continue;
        };
        if envelope.stream != agent.instance_stream {
            continue;
        }
        for event in chat_events_in(&envelope) {
            eprintln!("{label} child {event:?}");
            if matches!(event, ChatEvent::StreamEnd(_)) {
                child_stream_end = true;
            }
            // Idle counts only once the child has answered: a stream that has
            // not started yet also reports not-typing, and taking that as the
            // end of the turn would collect an empty child.
            let idle = child_stream_end && matches!(event, ChatEvent::TypingStatusChanged(false));
            child_events.push(event);
            if idle {
                child_idle = true;
            }
        }
    }

    Delegation {
        parent: Turn {
            backend,
            prompt: prompt.to_owned(),
            events: parent_events,
            activity_stats: Vec::new(),
        },
        child: Turn {
            backend,
            prompt: child_prompt.to_owned(),
            events: child_events,
            activity_stats: Vec::new(),
        },
        // `child_idle` cannot be set before the child exists.
        child_agent: child_agent.expect("a child that ran a turn was created"),
    }
}

/// The `backend_kind` spelling `tyde_spawn_agent` accepts.
///
/// Not the protocol's own: the tool publishes its own schema enum, where the ACP
/// backend is `kiro` (`agent_control_mcp.rs:301`). Spelled out separately from
/// [`backend_label`] so that a schema rename is a compile-time decision here
/// rather than something a logging label quietly decides.
pub fn spawn_tool_backend_name(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Tycode => "tycode-removed",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Kiro => "kiro",
        BackendKind::Hermes => "hermes",
        BackendKind::Antigravity => "antigravity",
    }
}

/// The chat event that echoes `prompt` back as the user's own message.
fn is_user_echo(event: &ChatEvent, prompt: &str) -> bool {
    matches!(
        event,
        ChatEvent::MessageAdded(message)
            if matches!(message.sender, MessageSender::User) && message.content.contains(prompt)
    )
}

fn chat_events_in(envelope: &Envelope) -> Vec<ChatEvent> {
    match envelope.kind {
        FrameKind::ChatEvent => vec![envelope.parse_payload().expect("parse ChatEvent")],
        FrameKind::AgentBootstrap => {
            let payload: AgentBootstrapPayload =
                envelope.parse_payload().expect("parse AgentBootstrap");
            payload
                .events
                .into_iter()
                .filter_map(|event| match event {
                    AgentBootstrapEvent::ChatEvent(event) => Some(event),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn fail_on_agent_error(envelope: &Envelope, context: &str) {
    if envelope.kind == FrameKind::AgentError {
        let error: AgentErrorPayload = envelope.parse_payload().expect("parse AgentError");
        panic!(
            "{context}: backend reported an agent error: {}",
            error.message
        );
    }
}

/// Collect over `window` without requiring a turn boundary. A background task
/// reports its terminal state on its own schedule, long after the turn that
/// started it closed, so no turn-shaped collector can observe it.
pub async fn drain_events_for(host: &mut Host, window: Duration) -> Vec<ChatEvent> {
    let deadline = tokio::time::Instant::now() + window;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, host.client.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                fail_on_agent_error(&envelope, "settle");
                events.extend(chat_events_in(&envelope));
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => panic!("settle next_event failed: {error:?}"),
            Err(_) => break,
        }
    }
    events
}

/// Collect over `window`, restricted to one agent instance stream.
///
/// A parent and its child intentionally remain active at the same time. Tests
/// looking for work after a parent boundary must not mistake legitimate child
/// events for the parent resuming.
pub async fn drain_agent_events_for(
    host: &mut Host,
    agent: &Agent,
    window: Duration,
) -> Vec<ChatEvent> {
    let deadline = tokio::time::Instant::now() + window;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, host.client.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                fail_on_agent_error(&envelope, "agent settle");
                if envelope.stream == agent.stream {
                    events.extend(chat_events_in(&envelope));
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => panic!("agent settle next_event failed: {error:?}"),
            Err(_) => break,
        }
    }
    events
}

/// Returns what arrives during shutdown, which is the emitter's last chance to
/// report violations it recorded after the final turn ended.
pub async fn close_agent(host: &mut Host, agent: &Agent) -> Vec<ChatEvent> {
    host.client
        .close_agent(&agent.stream)
        .await
        .expect("close_agent failed");
    drain_events_for(host, Duration::from_secs(2)).await
}

/// A question the backend asked, plus everything the client saw for a while
/// afterwards while deliberately not answering it.
pub struct Question {
    backend: BackendKind,
    prompt: String,
    request: ToolRequest,
    question: AskUserQuestion,
    events: Vec<ChatEvent>,
}

impl Question {
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{} question {prompt:?}", backend_label(self.backend))
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    pub fn tool_call_id(&self) -> &str {
        &self.request.tool_call_id
    }

    pub fn question(&self) -> &AskUserQuestion {
        &self.question
    }

    /// Chosen from what the provider actually offered rather than from the
    /// prompt: answering with a label the backend did not send would test the
    /// test, not the tool.
    pub fn first_option(&self) -> Option<&str> {
        self.question
            .options
            .first()
            .map(|option| option.label.as_str())
    }

    pub fn completions(&self) -> impl Iterator<Item = &ToolExecutionCompletedData> {
        let tool_call_id = self.request.tool_call_id.clone();
        self.events.iter().filter_map(move |event| match event {
            ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == tool_call_id =>
            {
                Some(completion)
            }
            _ => None,
        })
    }
}

/// How long the client sits quiet with an unanswered question on screen.
///
/// An interactive tool is the one kind that is *supposed* to outlive the turn
/// that asked it, so this window is the only place where a backend that
/// terminalizes the card behind the user's back becomes observable.
const QUESTION_SETTLE: Duration = Duration::from_secs(10);

/// Ask something that should make the backend put a question to the user, and
/// return once it has — then keep listening without answering.
pub async fn ask_question(host: &mut Host, agent: &Agent, prompt: &str) -> Question {
    let backend = host.backend_kind;
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");

    let context = format!("{} question for prompt {prompt:?}", backend_label(backend));
    let mut events = Vec::new();
    let mut asked = None;
    while asked.is_none() {
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }
        for event in chat_events_in(&envelope) {
            if let ChatEvent::ToolRequest(request) = &event
                && let protocol::ToolRequestType::AskUserQuestion { questions } = &request.tool_type
                && let Some(question) = questions.first()
            {
                asked = Some((request.clone(), question.clone()));
            }
            events.push(event);
        }
    }
    let (request, question) = asked.expect("loop exits only once a question was seen");
    events.extend(drain_events_for(host, QUESTION_SETTLE).await);

    Question {
        backend,
        prompt: prompt.to_owned(),
        request,
        question,
        events,
    }
}

/// Answer through the typed tool-response path the UI uses, not as chat text.
pub async fn answer_question(
    host: &mut Host,
    agent: &Agent,
    question: &Question,
    answer: &str,
) -> Turn {
    host.client
        .send_message_payload(
            &agent.stream,
            SendMessagePayload {
                message: answer.to_owned(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::AskUserQuestion {
                    tool_call_id: question.tool_call_id().to_owned(),
                    answer: answer.to_owned(),
                }),
            },
        )
        .await
        .expect("answer send_message failed");
    collect_until_idle(host, agent, &format!("answer {answer:?}")).await
}

/// Collect to the next idle without waiting for a user echo. A tool response is
/// not a chat message, so it never produces one.
pub async fn collect_until_idle(host: &mut Host, agent: &Agent, label: &str) -> Turn {
    let backend = host.backend_kind;
    let trace = backend_label(backend);
    let context = format!("{trace} {label}");
    let mut events = Vec::new();
    loop {
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }
        for event in chat_events_in(&envelope) {
            // Traced like `collect_turn`. Without this a turn collected here
            // leaves nothing in a paid run's log, and a failure such as "final
            // response was empty" cannot be told from "the collector stopped on
            // a stale idle signal left over from the previous turn" — the two
            // have identical assertion output and different causes.
            eprintln!("{trace} {event:?}");
            let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
            events.push(event);
            if idle {
                return Turn {
                    backend,
                    prompt: label.to_owned(),
                    events,
                    activity_stats: Vec::new(),
                };
            }
        }
    }
}

pub async fn cancel_turn(host: &mut Host, agent: &Agent) -> Vec<ChatEvent> {
    host.client
        .interrupt(&agent.stream)
        .await
        .expect("interrupt failed");
    drain_events_for(host, Duration::from_secs(10)).await
}

/// When to send the interrupt.
///
/// The moment is the whole experiment. Interrupting an idle agent, or one that
/// has already finished, exercises nothing; the two states below are the two
/// the protocol's cancellation ordering actually describes.
pub enum InterruptTrigger {
    /// Once the model has streamed this many characters of text, which is both
    /// proof a response is open and a measure of how far into it the stop
    /// lands.
    ///
    /// How deep matters: a stop sent at the first delta arrives before the
    /// provider has committed to a long answer and is the easy case, while the
    /// failure users report is a stop that lands well inside a long message and
    /// is held until the model finishes writing it.
    ///
    /// Counted in characters rather than deltas because a delta count measures
    /// the transport's chunking, not progress through the answer. Measured on
    /// the same prompt, Claude put 5 numbers in a delta on one run and 24 on
    /// another, and Hermes emits 2 characters at a time — so any delta
    /// threshold deep enough to be interesting for one backend is unreachable
    /// for another, and "unreachable" here means the turn ends before the
    /// interrupt is ever sent.
    AfterStreamedChars(usize),
    /// Once a tool card has opened, plus [`TOOL_STARTUP_GRACE`].
    AfterToolRequest,
}

/// How long the client waits for an interrupted turn to report idle.
///
/// Generous next to the sub-second cancellations backends manage today, and far
/// shorter than the ordinary 240s turn budget: a turn that simply runs to
/// completion has to fail here rather than pass four minutes later.
const INTERRUPT_DEADLINE: Duration = Duration::from_secs(45);

/// A tool card opens when the request is emitted, which is before the process
/// behind it has done anything. Interrupting in that window can be satisfied by
/// a backend that had nothing to stop yet, so the command is given a moment to
/// really be running.
const TOOL_STARTUP_GRACE: Duration = Duration::from_secs(3);

/// A turn that was interrupted, and where the interrupt falls in it.
pub struct Interrupted {
    turn: Turn,
    /// How long between sending the interrupt and the turn reporting idle.
    /// `None` when it never did within [`INTERRUPT_DEADLINE`] — deciding
    /// whether that is a defect belongs to `conformance.rs`.
    settled_in: Option<Duration>,
}

impl Interrupted {
    /// The turn itself, for the assertions that do not care that it was cut
    /// short.
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn label(&self) -> String {
        self.turn.label()
    }

    pub fn events(&self) -> &[ChatEvent] {
        self.turn.events()
    }

    pub fn settled_in(&self) -> Option<Duration> {
        self.settled_in
    }

    pub fn deadline(&self) -> Duration {
        INTERRUPT_DEADLINE
    }
}

/// Run a prompt, interrupt it at `trigger`, and collect until the turn reports
/// idle or [`INTERRUPT_DEADLINE`] expires.
///
/// Panics when the turn ends before the trigger fires: there is then no
/// interrupted turn to hand back, and reporting that as a passing cancellation
/// would be the most misleading thing this harness could do.
/// Stop one background command from its card, exactly as the tray's cancel
/// button does — a client frame naming the card, not a session interrupt.
pub async fn cancel_background_task(host: &mut Host, agent: &Agent, tool_call_id: &str) {
    host.client
        .cancel_background_task(
            &agent.stream,
            protocol::CancelBackgroundTaskPayload {
                tool_call_id: tool_call_id.to_owned(),
            },
        )
        .await
        .expect("cancel_background_task failed");
}

pub async fn interrupt_turn(
    host: &mut Host,
    agent: &Agent,
    prompt: &str,
    trigger: InterruptTrigger,
) -> Interrupted {
    let backend = host.backend_kind;
    let label = backend_label(backend);
    let context = format!("{label} interrupted turn for prompt {prompt:?}");
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");

    let mut events = Vec::new();
    let mut saw_echo = false;
    let mut streamed = 0usize;
    let mut fire = false;
    while !fire {
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }
        for event in chat_events_in(&envelope) {
            eprintln!("{label} {event:?}");
            if let ChatEvent::MessageAdded(message) = &event
                && matches!(message.sender, MessageSender::User)
                && message.content.contains(prompt)
            {
                saw_echo = true;
            }
            if !saw_echo {
                continue;
            }
            match (&event, &trigger) {
                (ChatEvent::StreamDelta(delta), InterruptTrigger::AfterStreamedChars(wanted)) => {
                    streamed += delta.text.chars().count();
                    fire |= streamed >= *wanted;
                }
                (ChatEvent::ToolRequest(_), InterruptTrigger::AfterToolRequest) => fire = true,
                (ChatEvent::TypingStatusChanged(false), _) => panic!(
                    "{context}: the turn went idle before there was anything to interrupt \
                     ({streamed} character(s) streamed, {} tool request(s) seen). The prompt has \
                     to keep the backend busy long enough for an interrupt to land, or this \
                     asserts nothing.",
                    events
                        .iter()
                        .filter(|event| matches!(event, ChatEvent::ToolRequest(_)))
                        .count()
                ),
                _ => {}
            }
            events.push(event);
        }
    }

    if matches!(trigger, InterruptTrigger::AfterToolRequest) {
        tokio::time::sleep(TOOL_STARTUP_GRACE).await;
    }

    let sent_at = tokio::time::Instant::now();
    host.client
        .interrupt(&agent.stream)
        .await
        .expect("interrupt failed");

    let deadline = sent_at + INTERRUPT_DEADLINE;
    let mut settled_in = None;
    'settle: loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, host.client.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                fail_on_agent_error(&envelope, &context);
                fail_on_client_error(&envelope, &context);
                if envelope.stream != agent.stream {
                    continue;
                }
                for event in chat_events_in(&envelope) {
                    eprintln!("{label} {event:?}");
                    let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
                    events.push(event);
                    if idle {
                        settled_in = Some(sent_at.elapsed());
                        break 'settle;
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => panic!("{context}: next_event failed: {error:?}"),
            Err(_) => break,
        }
    }

    Interrupted {
        turn: Turn {
            backend,
            prompt: prompt.to_owned(),
            events,
            activity_stats: Vec::new(),
        },
        settled_in,
    }
}

/// Send an ordinary message and fail fast if the server parks it in the queue.
///
/// A wedged agent queues instead of refusing, and the queue is silent: without
/// watching for the snapshot frame this reads as a turn that never arrives, and
/// fails minutes later pointing at the wrong thing.
pub async fn ask_expecting_delivery(host: &mut Host, agent: &Agent, prompt: &str) -> Turn {
    let backend = host.backend_kind;
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");

    let context = format!("{} delivery of {prompt:?}", backend_label(backend));
    loop {
        let envelope = host.next_envelope(Duration::from_secs(60), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.kind == FrameKind::QueuedMessages {
            let queued: QueuedMessagesPayload =
                envelope.parse_payload().expect("parse QueuedMessages");
            assert!(
                !queued
                    .messages
                    .iter()
                    .any(|entry| entry.message.contains(prompt)),
                "{context}: the agent queued this message instead of running it, so it believes a \
                 turn is still open. The chat accepts input and never answers."
            );
            continue;
        }
        if envelope.stream != agent.stream {
            continue;
        }
        let events = chat_events_in(&envelope);
        for event in &events {
            eprintln!("{} {event:?}", backend_label(backend));
        }
        if events.iter().any(|event| {
            matches!(event, ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::User)
                    && message.content.contains(prompt))
        }) {
            break;
        }
    }
    collect_until_idle(host, agent, &format!("turn for {prompt:?}")).await
}

/// One native workflow run: the turn that launched it, then everything the
/// client saw until the run reported a terminal snapshot.
pub struct Workflow {
    backend: BackendKind,
    prompt: String,
    turn: Turn,
    /// The launching turn's events followed by everything drained after it, in
    /// arrival order. Ordering across that boundary is the point: the tool call
    /// completes in the turn and the run reports for as long as it takes.
    events: Vec<ChatEvent>,
}

impl Workflow {
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{} workflow {prompt:?}", backend_label(self.backend))
    }

    /// The turn that launched the run, for the universal contract.
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    pub fn snapshots(&self) -> impl Iterator<Item = &protocol::WorkflowRunState> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolProgress(progress) => match &progress.update {
                protocol::ToolProgressUpdate::Workflow(state) => Some(state),
                _ => None,
            },
            _ => None,
        })
    }

    /// Index into [`Workflow::events`] of the first terminal snapshot, and of the
    /// completion of the tool call that launched the run. Both are positions
    /// rather than values because the assertion that matters is their order.
    pub fn terminal_snapshot_position(&self) -> Option<usize> {
        self.events.iter().position(|event| {
            matches!(event, ChatEvent::ToolProgress(progress)
                if matches!(&progress.update, protocol::ToolProgressUpdate::Workflow(state)
                    if state.status != protocol::WorkflowRunStatus::Running))
        })
    }

    pub fn launching_completion_position(&self) -> Option<usize> {
        let tool_call_id = self.tool_call_id()?;
        self.events.iter().position(|event| {
            matches!(event, ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == tool_call_id)
        })
    }

    /// The id every workflow snapshot is addressed to, which is also the tool
    /// call that launched the run.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.events.iter().find_map(|event| match event {
            ChatEvent::ToolProgress(progress)
                if matches!(progress.update, protocol::ToolProgressUpdate::Workflow(_)) =>
            {
                Some(progress.tool_call_id.as_str())
            }
            _ => None,
        })
    }
}

/// How long the client keeps listening for a workflow to finish after the turn
/// that launched it has gone idle.
///
/// Generous on purpose: when the terminal snapshot never arrives this window is
/// paid in full before the assertion fires, and a run that reports normally
/// leaves long before it expires.
const WORKFLOW_SETTLE: Duration = Duration::from_secs(90);

/// Ask for a workflow, then keep listening past the end of the launching turn.
///
/// A native workflow outlives its own tool call: the tool returns a task id
/// immediately and the run reports progress for as long as it takes, so no
/// turn-shaped collector can see one finish. Returns as soon as a terminal
/// snapshot arrives, or after [`WORKFLOW_SETTLE`] without one — deciding whether
/// that silence is a defect is `conformance.rs`'s job, not the harness's.
pub async fn run_workflow(host: &mut Host, agent: &Agent, prompt: &str) -> Workflow {
    let backend = host.backend_kind;
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");
    let turn = collect_turn(host, agent, prompt).await;

    let mut events = turn.events().to_vec();
    let deadline = tokio::time::Instant::now() + WORKFLOW_SETTLE;
    'settle: loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, host.client.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                fail_on_agent_error(&envelope, "workflow settle");
                fail_on_client_error(&envelope, "workflow settle");
                for event in chat_events_in(&envelope) {
                    let terminal = matches!(&event, ChatEvent::ToolProgress(progress)
                        if matches!(&progress.update, protocol::ToolProgressUpdate::Workflow(state)
                            if state.status != protocol::WorkflowRunStatus::Running));
                    events.push(event);
                    if terminal {
                        break 'settle;
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => panic!("workflow settle next_event failed: {error:?}"),
            Err(_) => break,
        }
    }

    Workflow {
        backend,
        prompt: prompt.to_owned(),
        turn,
        events,
    }
}

/// One compaction and every chat event the client saw while it ran.
pub struct Compaction {
    backend: BackendKind,
    events: Vec<ChatEvent>,
    terminal: ContextCompactionNotifyPayload,
}

impl Compaction {
    pub fn label(&self) -> String {
        format!("{} compaction", backend_label(self.backend))
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    /// The durable timeline markers this compaction produced. One compaction is
    /// one marker; more than one is a duplicate row in the user's transcript.
    pub fn markers(&self) -> impl Iterator<Item = &ContextCompactionTimelineEvent> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ContextCompaction(marker) => Some(marker),
            _ => None,
        })
    }

    pub fn terminal(&self) -> &ContextCompactionNotifyPayload {
        &self.terminal
    }
}

/// How long the collector keeps listening after the terminal notify.
///
/// The durable marker and the terminal notify are ordered with respect to each
/// other, but a backend's own *observation* of the same compaction reaches the
/// agent loop on a different channel and lands tens of milliseconds later.
/// Returning at the notify would make a second marker unobservable.
const COMPACTION_SETTLE: Duration = Duration::from_secs(5);

/// Compact through the same client message the UI's compact button sends, and
/// return once the operation reports a terminal status.
pub async fn compact(host: &mut Host, agent: &Agent) -> Compaction {
    let backend = host.backend_kind;
    host.client
        .compact_agent(
            &agent.stream,
            AgentCompactPayload {
                summary_prompt: None,
                max_summary_bytes: None,
            },
        )
        .await
        .expect("compact_agent failed");

    let context = format!("{} compaction", backend_label(backend));
    let mut events = Vec::new();
    let terminal = loop {
        // Summarizing the whole conversation is a model round trip.
        let envelope = host.next_envelope(Duration::from_secs(300), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.kind == FrameKind::ContextCompactionNotify {
            let notify: ContextCompactionNotifyPayload = envelope
                .parse_payload()
                .expect("parse ContextCompactionNotify");
            if notify.status.is_terminal() {
                break notify;
            }
            continue;
        }
        // Unfiltered by stream, unlike a turn: the compaction marker is
        // broadcast by the agent actor rather than echoed back on the stream the
        // request went out on, and there is only ever one agent here.
        events.extend(chat_events_in(&envelope));
    };
    events.extend(drain_events_for(host, COMPACTION_SETTLE).await);

    Compaction {
        backend,
        events,
        terminal,
    }
}

/// A refused control request answers on this frame rather than the agent
/// stream, so without it a rejected compaction reads as a 300s timeout.
fn fail_on_client_error(envelope: &Envelope, context: &str) {
    if envelope.kind == FrameKind::ClientError {
        let error: ClientErrorPayload = envelope.parse_payload().expect("parse ClientError");
        panic!(
            "{context}: server rejected the request with {:?}: {}",
            error.code, error.message
        );
    }
}

/// Fails when the backend stored anything other than exactly one session, since
/// there is then no single session to hand back.
pub async fn stored_session(host: &mut Host) -> SessionSummary {
    let backend_kind = host.backend_kind;
    host.client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let payload: SessionListPayload = loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SessionList").await;
        if envelope.kind == FrameKind::SessionList {
            break envelope.parse_payload().expect("parse SessionList");
        }
    };
    let mut sessions: Vec<_> = payload
        .sessions
        .into_iter()
        .filter(|session| session.backend_kind == backend_kind)
        .collect();
    assert_eq!(
        sessions.len(),
        1,
        "{backend_kind:?}: expected exactly one stored session, found {}",
        sessions.len()
    );
    sessions.remove(0)
}

/// The paged-history path is separate server code from the bootstrap replay and
/// has broken on its own; the UI uses both.
pub async fn history_page(host: &mut Host, agent: &Agent) -> SessionHistoryPayload {
    let request_id = HistoryPageRequestId(Uuid::new_v4().to_string());
    host.client
        .fetch_session_history(
            &agent.stream,
            FetchSessionHistoryPayload {
                agent_id: agent.agent_id.clone(),
                request_id: request_id.clone(),
                before_seq: None,
                limit: 200,
            },
        )
        .await
        .expect("fetch_session_history failed");

    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SessionHistory").await;
        if envelope.kind != FrameKind::SessionHistory {
            continue;
        }
        let candidate: SessionHistoryPayload =
            envelope.parse_payload().expect("parse SessionHistory");
        if candidate.request_id == request_id {
            return candidate;
        }
    }
}

fn backend_label(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Tycode => "tycode-removed",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Kiro => "kiro",
        BackendKind::Hermes => "hermes",
    }
}

fn host_settings(backend_kind: BackendKind) -> serde_json::Value {
    let mut settings = json!({
        "settings": {
            "enabled_backends": [backend_kind],
            "default_backend": backend_kind,
            "complexity_tiers_enabled": true,
            "tyde_agent_control_mcp_enabled": true
        }
    });
    // Both tiers get the same pin: a scenario must not be able to escalate cost
    // by asking for something the host considers hard.
    match backend_kind {
        BackendKind::Claude => {
            let model = &pinned_models(backend_kind)[0];
            let tier = json!({"model": {"string": model}, "effort": {"string": "low"}});
            settings["settings"]["backend_tier_configs"] =
                json!({"claude": {"low": tier, "high": tier}});
        }
        BackendKind::Codex => {
            let model = &pinned_models(backend_kind)[0];
            let tier = json!({"model": {"string": model}, "reasoning_effort": {"string": "low"}});
            settings["settings"]["backend_tier_configs"] =
                json!({"codex": {"low": tier, "high": tier}});
        }
        BackendKind::Hermes => {
            let tier = serde_json::to_value(hermes_session_settings())
                .expect("serialize Hermes conformance tier");
            settings["settings"]["backend_tier_configs"] =
                json!({"hermes": {"low": tier, "high": tier}});
        }
        _ => {}
    }
    settings
}

/// Where conformance scratch directories are created.
///
/// Deliberately not `$TMPDIR`, which is what `tempfile::tempdir()` honours. On
/// macOS that is `/var/folders/…`, and `/var` resolves to `/private/var` —
/// which Hermes's file tools refuse to write to, matching the realpath against
/// `_SENSITIVE_PATH_PREFIXES` in `hermes-agent/tools/file_tools.py`. Every
/// scenario that asked Hermes to create a file therefore died on Hermes's own
/// guard before reaching a Tyde assertion. `/tmp` resolves to `/private/tmp`,
/// which is not on that list, and is closer to where a real workspace lives.
const SCRATCH_ROOT: &str = "/tmp";
const CODEX_LEGACY_DYNAMIC_AWAIT_MARKER: &str = ".tyde-conformance-legacy-dynamic-await";

/// A scratch directory under [`SCRATCH_ROOT`] that a backend's file tools will
/// actually write to.
fn scratch_dir(purpose: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("tyde-conformance-{purpose}-"))
        .tempdir_in(SCRATCH_ROOT)
        .unwrap_or_else(|error| panic!("create {purpose} dir under {SCRATCH_ROOT}: {error}"))
}

/// Run `scenario` against each backend in turn, on a thread with a stack deep
/// enough for the recursion real backends hit while decoding JSON.
///
/// The scratch directories outlive the [`Host`] deliberately: teardown kills a
/// provider subprocess that is still running in the workspace, so removing the
/// workspace first would pull the ground out from under it.
/// `requires` narrows the run to backends declaring every listed capability.
/// A gated scenario that matches nothing still reports PASS and nextest cannot
/// report a skip from inside a test, so the empty case is announced instead.
pub fn run_scenario<F, Fut>(requires: &[BackendCapability], scenario: F)
where
    F: Fn(Host) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    run_scenario_where(requires, |_| true, false, false, scenario);
}

pub fn run_nested_subagent_scenario<F, Fut>(requires: &[BackendCapability], scenario: F)
where
    F: Fn(Host) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    run_scenario_where(requires, |_| true, false, true, scenario);
}

pub fn run_legacy_codex_dynamic_await_scenario<F, Fut>(requires: &[BackendCapability], scenario: F)
where
    F: Fn(Host) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    run_scenario_where(
        requires,
        |backend| backend == BackendKind::Codex,
        true,
        false,
        scenario,
    );
}

pub fn run_native_skill_scenario<F, Fut>(scenario: F)
where
    F: Fn(Host) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    run_scenario_where(
        &[],
        server::backend::discovers_skills_natively,
        false,
        false,
        scenario,
    );
}

fn run_scenario_where<F, Fut>(
    requires: &[BackendCapability],
    eligible: impl Fn(BackendKind) -> bool,
    legacy_codex_dynamic_await: bool,
    codex_nested_subagent: bool,
    scenario: F,
) where
    F: Fn(Host) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let _run = CONFORMANCE_RUN_LOCK
        .lock()
        .expect("conformance run lock poisoned");
    struct LegacyCodexModelGuard;
    impl Drop for LegacyCodexModelGuard {
        fn drop(&mut self) {
            LEGACY_CODEX_DYNAMIC_AWAIT_ACTIVE.store(false, Ordering::Relaxed);
            CODEX_NESTED_SUBAGENT_ACTIVE.store(false, Ordering::Relaxed);
        }
    }
    LEGACY_CODEX_DYNAMIC_AWAIT_ACTIVE.store(legacy_codex_dynamic_await, Ordering::Relaxed);
    CODEX_NESTED_SUBAGENT_ACTIVE.store(codex_nested_subagent, Ordering::Relaxed);
    let _model_guard = LegacyCodexModelGuard;
    init_tracing();
    let (backends, skipped): (Vec<_>, Vec<_>) = enabled_backends().into_iter().partition(|kind| {
        let capabilities = server::backend::capabilities_for_backend_kind(*kind);
        eligible(*kind)
            && requires
                .iter()
                .all(|requirement| capabilities.contains(*requirement))
    });
    require_locally_built_mcp_bridge(&backends);
    if backends.is_empty() {
        eprintln!("COVERAGE: no enabled backend declares {requires:?}; this test asserts nothing");
    } else {
        eprintln!("COVERAGE {requires:?}: {backends:?}, skipped {skipped:?}");
    }

    let result = std::thread::Builder::new()
        .name("conformance".to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime")
                .block_on(async move {
                    for backend_kind in backends {
                        let store = scratch_dir("store");
                        let workspace = scratch_dir("workspace");
                        if legacy_codex_dynamic_await && backend_kind == BackendKind::Codex {
                            std::fs::write(
                                workspace.path().join(CODEX_LEGACY_DYNAMIC_AWAIT_MARKER),
                                "pre-beta.76 Codex thread",
                            )
                            .expect("mark legacy Codex dynamic-await conformance workspace");
                        }
                        let host = Host::new(backend_kind, store.path(), workspace.path()).await;
                        let handle = host.handle.clone();

                        let outcome = AssertUnwindSafe(scenario(host)).catch_unwind().await;

                        // Runs even when the scenario panicked: a leaked
                        // provider subprocess keeps costing money.
                        if tokio::time::timeout(
                            Duration::from_secs(30),
                            handle.shutdown_agents_for_conformance(),
                        )
                        .await
                        .is_err()
                        {
                            eprintln!("{backend_kind:?}: backend shutdown timed out");
                        }
                        if let Err(panic) = outcome {
                            std::panic::resume_unwind(panic);
                        }
                    }
                });
        })
        .expect("spawn conformance thread")
        .join();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Honours `RUST_LOG`, so a failing run can be re-run with backend tracing
/// turned up without touching code. `RUST_LOG=server::backend::codex=trace`
/// prints the verbatim JSON of every app-server notification next to Tyde's
/// derived view of it; see `CodexSession::trace_notification_structure`.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
