mod fixture;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use fixture::Fixture;
use protocol::{
    AgentActivityStatsPayload, AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorCode,
    AgentOrigin, AgentStartPayload, BackendKind, ChatEvent, ChatMessage, CommandErrorPayload,
    CustomAgentId, DeleteSessionPayload, Envelope, FetchSessionHistoryPayload, FrameKind,
    HostBootstrapPayload, HostSettingValue, ImageData, ListSessionsPayload,
    MessageMetadataUpdateData, MessageSender, NewAgentPayload, ProtocolValidator,
    SessionHistoryPayload, SessionId, SessionListPayload, SessionSchemaEntry,
    SessionSchemasPayload, SessionSettingFieldType, SessionSettingValue, SessionSettingsValues,
    SessionSummary, SetSessionSettingsPayload, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload, SpawnCostHint, StreamPath, TokenUsage, TokenUsageScope,
    TokenUsageUnavailableReason, ToolExecutionCompletedData, ToolExecutionResult, ToolRequest,
    ToolRequestType,
};
use serde_json::{Value, json};
use server::backend::{Backend, BackendSession};
use tyde_agent_adapter::{BackendObservation, CertificationCase};
use uuid::Uuid;

const REAL_BACKEND_TIMEOUT: Duration = Duration::from_secs(60);
const REAL_BACKEND_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const RUN_REAL_AI_TESTS_ENV: &str = "TYDE_RUN_REAL_AI_TESTS";
const DEFAULT_HERMES_TEST_PYTHON: &str = "/Users/mike/.hermes/tyde-hermes-python";
const DEFAULT_HERMES_TEST_PROVIDER: &str = "openrouter";
const DEFAULT_HERMES_TEST_MODEL: &str = "anthropic/claude-haiku-4.5";
const UNIVERSAL_CLAUDE_MODEL: &str = "haiku";
const UNIVERSAL_CLAUDE_EFFORT: &str = "low";
const UNIVERSAL_CODEX_MODEL: &str = "gpt-5.6-luna";
const UNIVERSAL_CODEX_REASONING_EFFORT: &str = "low";
const SOLID_RED_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJ0lEQVR42u3NsQkAAAjAsP7/tF7hIASyp6lTCQQCgUAgEAgEgi/BAjLD/C5w/SM9AAAAAElFTkSuQmCC";
static REAL_ANTIGRAVITY_NATIVE_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn binary_available(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn backend_binary_available(backend_kind: BackendKind) -> bool {
    match backend_kind {
        BackendKind::Claude => binary_available("claude"),
        BackendKind::Codex => binary_available("codex"),
        BackendKind::Antigravity => binary_available("agy"),
        BackendKind::Tycode => binary_available("tycode-subprocess"),
        BackendKind::Acp => binary_available("kiro-cli-chat") || binary_available("kiro-cli"),
        BackendKind::Hermes => {
            std::env::var("HERMES_PYTHON").is_ok()
                || binary_available("python3")
                || binary_available("python")
        }
    }
}

fn home_is_writable() -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let probe = PathBuf::from(home).join(format!(
        ".tyde-backend-probe-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("thread")
    ));

    let created = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
        .is_ok();
    if created {
        let _ = std::fs::remove_file(&probe);
    }
    created
}

fn remote_network_is_available() -> bool {
    "example.com:443".to_socket_addrs().is_ok()
}

fn real_ai_tests_enabled() -> bool {
    std::env::var(RUN_REAL_AI_TESTS_ENV).ok().as_deref() == Some("1")
}

struct EnvVarGuard {
    key: &'static str,
    old_value: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: String) -> Self {
        let old_value = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, old_value }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.old_value.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn backend_runtime_available(backend_kind: BackendKind) -> bool {
    if !backend_binary_available(backend_kind) {
        return false;
    }
    if !real_ai_tests_enabled() {
        eprintln!("SKIPPED: real AI backend tests require {RUN_REAL_AI_TESTS_ENV}=1");
        return false;
    }

    match backend_kind {
        BackendKind::Tycode => home_is_writable(),
        BackendKind::Claude | BackendKind::Antigravity | BackendKind::Acp => {
            home_is_writable() && remote_network_is_available()
        }
        BackendKind::Codex => remote_network_is_available(),
        BackendKind::Hermes => home_is_writable() && remote_network_is_available(),
    }
}

async fn run_shell_probe(script: &str, timeout: Duration) -> Result<String, String> {
    let child = tokio::process::Command::new("zsh")
        .arg("-lc")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("failed to spawn probe: {err}"))?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("probe timed out after {:?}", timeout))?
        .map_err(|err| format!("failed to wait for probe: {err}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(format!("{stdout}{stderr}"))
    } else {
        Err(format!(
            "probe exited with {}: {}{}",
            output.status, stdout, stderr
        ))
    }
}

async fn probe_backend_runtime(backend_kind: BackendKind) -> Result<(), String> {
    if !backend_binary_available(backend_kind) {
        return Err("backend binary not installed".to_string());
    }
    if !backend_runtime_available(backend_kind) {
        return Err("basic runtime prerequisites unavailable".to_string());
    }

    match backend_kind {
        BackendKind::Claude => {
            let script = r#"
tmpdir=$(mktemp -d)
cd "$tmpdir" || exit 1
printf '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Reply exactly with ok"}]}}\n' \
  | claude --print --verbose --output-format stream-json --input-format stream-json --include-partial-messages --dangerously-skip-permissions --model haiku --effort low
"#;
            let output = run_shell_probe(script, REAL_BACKEND_PROBE_TIMEOUT).await?;
            if output.contains("\"session_id\"") && output.contains("\"result\"") {
                Ok(())
            } else {
                Err(format!(
                    "Claude probe did not emit expected session output: {output}"
                ))
            }
        }
        BackendKind::Codex => Ok(()),
        BackendKind::Antigravity => {
            let script = r#"
tmpdir=$(mktemp -d)
cd "$tmpdir" || exit 1
agy --model 'Gemini 3.5 Flash (Low)' --print-timeout 30s --dangerously-skip-permissions -p 'Reply exactly with ok'
"#;
            let output = run_shell_probe(script, REAL_BACKEND_PROBE_TIMEOUT).await?;
            if output.contains("Authentication required") {
                Err(format!(
                    "Antigravity probe requires authentication: {output}"
                ))
            } else if output.contains("Error: timed out waiting for response") {
                Err(format!("Antigravity probe timed out: {output}"))
            } else if output
                .lines()
                .any(|line| line.trim_start().starts_with("Error:"))
            {
                Err(format!("Antigravity probe failed: {output}"))
            } else if output.trim().is_empty() {
                Err("Antigravity probe emitted no output".to_string())
            } else {
                Ok(())
            }
        }
        BackendKind::Tycode => {
            let workspace = tempfile::tempdir().map_err(|err| format!("{err}"))?;
            std::fs::write(workspace.path().join("README.txt"), "probe workspace")
                .map_err(|err| format!("failed to seed Tycode probe workspace: {err}"))?;
            let result = tokio::time::timeout(
                REAL_BACKEND_PROBE_TIMEOUT,
                <server::backend::tycode::TycodeBackend as Backend>::spawn(
                    vec![workspace.path().to_string_lossy().to_string()],
                    server::backend::BackendSpawnConfig {
                        acp_agent: None,
                        execution_mode: Default::default(),
                        cost_hint: cost_hint_for(BackendKind::Tycode),
                        custom_agent_id: None,
                        startup_mcp_servers: Vec::new(),
                        session_settings: Default::default(),
                        provider_version: None,
                        antigravity_conversations_dir: None,
                        backend_config: Default::default(),
                        resolved_spawn_config: Default::default(),
                    },
                    protocol::SendMessagePayload {
                        message: "Reply exactly with ok".to_owned(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ),
            )
            .await
            .map_err(|_| "Tycode spawn timed out".to_string())?
            .map_err(|err| format!("Tycode spawn failed: {err}"))?;
            let (_backend, mut events) = result;
            tokio::time::timeout(REAL_BACKEND_PROBE_TIMEOUT, async {
                while let Some(event) = events.recv().await {
                    if matches!(event, ChatEvent::StreamEnd(_)) {
                        return Ok(());
                    }
                }
                Err("Tycode probe stream ended before StreamEnd".to_string())
            })
            .await
            .map_err(|_| "Tycode initial turn timed out".to_string())??;
            Ok(())
        }
        BackendKind::Acp => {
            let workspace = tempfile::tempdir().map_err(|err| format!("{err}"))?;
            std::fs::write(workspace.path().join("README.txt"), "probe workspace")
                .map_err(|err| format!("failed to seed Kiro probe workspace: {err}"))?;
            let result = tokio::time::timeout(
                REAL_BACKEND_PROBE_TIMEOUT,
                <server::backend::kiro::KiroBackend as Backend>::spawn(
                    vec![workspace.path().to_string_lossy().to_string()],
                    server::backend::BackendSpawnConfig {
                        acp_agent: None,
                        execution_mode: Default::default(),
                        cost_hint: cost_hint_for(BackendKind::Acp),
                        custom_agent_id: None,
                        startup_mcp_servers: Vec::new(),
                        session_settings: Default::default(),
                        provider_version: None,
                        antigravity_conversations_dir: None,
                        backend_config: Default::default(),
                        resolved_spawn_config: Default::default(),
                    },
                    protocol::SendMessagePayload {
                        message: "Reply exactly with ok".to_owned(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ),
            )
            .await
            .map_err(|_| "Kiro ACP spawn timed out".to_string())?
            .map_err(|err| format!("Kiro ACP spawn failed: {err}"))?;
            let (_backend, mut events) = result;
            tokio::time::timeout(REAL_BACKEND_PROBE_TIMEOUT, async {
                while let Some(event) = events.recv().await {
                    if matches!(event, ChatEvent::StreamEnd(_)) {
                        return Ok(());
                    }
                }
                Err("Kiro probe stream ended before StreamEnd".to_string())
            })
            .await
            .map_err(|_| "Kiro initial turn timed out".to_string())??;
            Ok(())
        }
        BackendKind::Hermes => {
            let workspace = tempfile::tempdir().map_err(|err| format!("{err}"))?;
            std::fs::write(workspace.path().join("README.txt"), "probe workspace")
                .map_err(|err| format!("failed to seed Hermes probe workspace: {err}"))?;
            let result = tokio::time::timeout(
                REAL_BACKEND_PROBE_TIMEOUT,
                <server::backend::hermes::HermesBackend as Backend>::spawn(
                    vec![workspace.path().to_string_lossy().to_string()],
                    server::backend::BackendSpawnConfig {
                        acp_agent: None,
                        execution_mode: Default::default(),
                        cost_hint: cost_hint_for(BackendKind::Hermes),
                        custom_agent_id: None,
                        startup_mcp_servers: Vec::new(),
                        session_settings: Default::default(),
                        provider_version: None,
                        antigravity_conversations_dir: None,
                        backend_config: Default::default(),
                        resolved_spawn_config: Default::default(),
                    },
                    protocol::SendMessagePayload {
                        message: "Reply exactly with ok".to_owned(),
                        images: None,
                        origin: None,
                        tool_response: None,
                    },
                ),
            )
            .await
            .map_err(|_| "Hermes gateway spawn timed out".to_string())?
            .map_err(|err| format!("Hermes gateway spawn failed: {err}"))?;
            let (_backend, mut events) = result;
            tokio::time::timeout(REAL_BACKEND_PROBE_TIMEOUT, async {
                while let Some(event) = events.recv().await {
                    if matches!(event, ChatEvent::StreamEnd(_)) {
                        return Ok(());
                    }
                }
                Err("Hermes probe stream ended before StreamEnd".to_string())
            })
            .await
            .map_err(|_| "Hermes initial turn timed out".to_string())??;
            Ok(())
        }
    }
}

fn cost_hint_for(backend_kind: BackendKind) -> Option<SpawnCostHint> {
    // Low keeps real-backend runs fast and cheap. Medium is a no-op (backend
    // default), which may still be too slow for live probes.
    let _ = backend_kind;
    Some(SpawnCostHint::Low)
}

fn backend_label(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Tycode => "tycode",
        BackendKind::Acp => "kiro",
        BackendKind::Hermes => "hermes",
    }
}

struct AntigravityConversationDbGuard {
    path: PathBuf,
    remove_file: bool,
}

impl AntigravityConversationDbGuard {
    fn create(conversations_dir: &Path, session_id: &SessionId) -> Self {
        std::fs::create_dir_all(conversations_dir).unwrap_or_else(|err| {
            panic!(
                "failed to create fake Antigravity conversations dir {conversations_dir:?}: {err}"
            )
        });
        let path = conversations_dir.join(format!("{}.db", session_id.0));
        let remove_file = !path.exists();
        if remove_file {
            std::fs::write(&path, b"test conversation db").unwrap_or_else(|err| {
                panic!("failed to create fake Antigravity conversation db {path:?}: {err}")
            });
        }
        Self { path, remove_file }
    }
}

impl Drop for AntigravityConversationDbGuard {
    fn drop(&mut self) {
        if self.remove_file {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn set_stored_session_resumable(store_dir: &Path, session_id: &SessionId, resumable: bool) {
    let path = store_dir.join("sessions.json");
    let contents = std::fs::read_to_string(&path).expect("read session store");
    let mut value: serde_json::Value =
        serde_json::from_str(&contents).expect("parse session store");
    let record = value
        .get_mut("records")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|records| records.get_mut(&session_id.0))
        .and_then(serde_json::Value::as_object_mut)
        .unwrap_or_else(|| panic!("missing stored session record {session_id}"));
    record.insert("resumable".to_owned(), serde_json::Value::Bool(resumable));
    let rewritten = serde_json::to_string_pretty(&value).expect("serialize session store");
    std::fs::write(&path, rewritten).expect("write session store");
}

fn write_antigravity_session_record_without_alias(store_dir: &Path, session_id: &SessionId) {
    let path = store_dir.join("sessions.json");
    let mut records = serde_json::Map::new();
    records.insert(
        session_id.0.clone(),
        serde_json::json!({
            "id": session_id.0.clone(),
            "backend_kind": "antigravity",
            "workspace_roots": [],
            "created_at_ms": 1,
            "updated_at_ms": 2,
            "resumable": true
        }),
    );
    let value = serde_json::json!({
        "records": records
    });
    let json = serde_json::to_string_pretty(&value).expect("serialize antigravity session store");
    std::fs::write(&path, json).expect("write antigravity session store");
}

async fn expect_fixture_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) if env.kind == FrameKind::BackendCapacity => {}
            Ok(Ok(Some(env))) => return env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        }
    }
}

fn agent_start_and_chat_events_from_bootstrap(
    env: Envelope,
    context: &str,
) -> (AgentStartPayload, Vec<ChatEvent>) {
    assert_eq!(env.kind, FrameKind::AgentBootstrap, "expected {context}");
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    let mut start = None;
    let mut chat_events = Vec::new();
    for event in payload.events {
        match event {
            AgentBootstrapEvent::AgentStart(value) => start = Some(value),
            AgentBootstrapEvent::ChatEvent(event) => chat_events.push(event),
            _ => {}
        }
    }
    (
        start.unwrap_or_else(|| panic!("AgentBootstrap missing AgentStart for {context}")),
        chat_events,
    )
}

fn agent_start_from_bootstrap(env: Envelope, context: &str) -> AgentStartPayload {
    agent_start_and_chat_events_from_bootstrap(env, context).0
}

async fn expect_fixture_agent_start(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        let env = expect_fixture_event(client, context).await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == *agent_stream {
            return agent_start_from_bootstrap(env, context);
        }
    }
}

async fn expect_fixture_agent_start_with_chat_events(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    context: &str,
) -> (AgentStartPayload, Vec<ChatEvent>) {
    loop {
        let env = expect_fixture_event(client, context).await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == *agent_stream {
            return agent_start_and_chat_events_from_bootstrap(env, context);
        }
    }
}

async fn expect_fixture_initial_turn_completion(
    client: &mut client::Connection,
    agent_stream: &StreamPath,
    bootstrap_chat_events: Vec<ChatEvent>,
    context: &str,
) {
    // Settled no-tool streams are canonicalized to one assistant MessageAdded in replay.
    if bootstrap_chat_events.iter().any(|event| {
        matches!(event, ChatEvent::StreamEnd(_))
            || matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Assistant { .. })
            )
    }) {
        return;
    }
    loop {
        let env = expect_fixture_event(client, context).await;
        if env.kind != FrameKind::ChatEvent || env.stream != *agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse initial ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(_)) {
            return;
        }
    }
}

async fn spawn_mock_agent_and_collect_turn(
    client: &mut client::Connection,
    backend_kind: BackendKind,
    prompt: &str,
) -> String {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Chat".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: prompt.to_string(),
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_fixture_event(client, "NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let agent_stream = new_agent.instance_stream;

    let agent_start = expect_fixture_agent_start(client, &agent_stream, "AgentStart").await;
    assert_eq!(agent_start.agent_id, new_agent.agent_id);

    loop {
        let env = expect_fixture_event(client, "ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if let ChatEvent::StreamEnd(data) = event {
            return data.message.content;
        }
    }
}

fn write_fake_kiro_probe_program(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("fake-kiro-cli-chat");
    std::fs::write(
        &path,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$PWD" > "$(dirname "$0")/probe-cwd"
IFS= read -r _ || exit 1
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
IFS= read -r _ || exit 1
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"kiro-probe-session","availableModels":[{"id":"kiro-sonnet","name":"Kiro Sonnet","isDefault":true},{"id":"kiro-haiku","name":"Kiro Haiku","isDefault":false}]}}'
while IFS= read -r _; do :; done
"#,
    )
    .expect("write fake Kiro probe program");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .expect("stat fake Kiro probe program")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod fake Kiro probe program");
    }
    path
}

struct CodexIdentityFake {
    _dir: tempfile::TempDir,
    binary: PathBuf,
    thread_id: String,
    late_events_written: PathBuf,
    settings_update: PathBuf,
    followup_release: PathBuf,
}

impl CodexIdentityFake {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create Codex identity fake tempdir");
        let binary = dir.path().join("codex-identity-app-server.py");
        // Each thread/start is a fresh provider binding. Reusing a fixed
        // provider session would make the durable identity journal correctly
        // treat records from an earlier test process as this session's history.
        let identity_suffix = Uuid::new_v4();
        let thread_id = format!("identity-thread-{identity_suffix}");
        let child_thread_id = format!("identity-child-{identity_suffix}");
        let program = r#"#!/usr/bin/env python3
import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

turn_count = 0
for line in sys.stdin:
    try:
        request = json.loads(line)
    except Exception:
        continue
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params", {})
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"userAgent":"fake-codex/identity","codexHome":"/tmp/fake-codex-home","platformFamily":"unix","platformOs":"test"}})
    elif method == "model/list":
        send({"jsonrpc":"2.0","id":request_id,"result":{"data":[{"model":"fake-codex-model","isDefault":True,"supportedReasoningEfforts":[{"reasoningEffort":"high"}]}]}})
    elif method == "thread/start":
        send({"jsonrpc":"2.0","id":request_id,"result":{"thread":{"id":"identity-thread","sessionId":"identity-thread","turns":[]},"model":"fake-codex-model"}})
    elif method == "turn/start":
        turn_count += 1
        if turn_count == 1:
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"identity-thread","turn":{"id":"identity-turn-one"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"identity-thread","item":{"id":"parent-tool","type":"commandExecution","command":"pwd","cwd":"/tmp"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-tool","type":"commandExecution","exitCode":0,"aggregatedOutput":"/tmp"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"identity-thread","item":{"id":"parent-one","type":"agentMessage"}}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"identity-thread","itemId":"parent-one","delta":"First "}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"identity-thread","itemId":"parent-one","delta":"response"}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-one","type":"agentMessage","text":" \n"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-one","type":"agentMessage","text":" \n"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-one","type":"agentMessage","text":"First response"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-two","type":"agentMessage","text":"Second response"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-empty","type":"agentMessage","text":""}}})
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"identity-thread","turn":{"id":"identity-turn-one","status":"completed"}}})
        else:
            while not os.path.exists(__file__ + ".followup-release"):
                time.sleep(0.01)
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"identity-thread","turn":{"id":"identity-turn-two"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"parent-followup","type":"agentMessage","text":"Starting child"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"identity-thread","item":{"id":"spawn-child","type":"collabAgentToolCall","tool":"spawn","senderThreadId":"identity-thread","receiverThreadId":"identity-child","prompt":"identity child","receiverAgentType":"worker","receiverAgentName":"Identity child"}}})
            send({"jsonrpc":"2.0","method":"item/started","params":{"threadId":"identity-thread","item":{"id":"activity-child","type":"sub_agent_activity","kind":"started","agent_thread_id":"identity-child","agent_path":"/root/identity_child"}}})
            send({"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"identity-child","turn":{"id":"identity-child-turn"}}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"identity-child","itemId":"child-good","delta":"Child response"}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-child","item":{"id":"child-good","type":"agentMessage","text":"Child response"}}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"identity-child","itemId":"child-active","delta":"Valid before interleave"}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"identity-child","itemId":"child-foreign","delta":"must never appear"}})
            send({"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"identity-child","itemId":"child-late-delta","delta":"must stay quarantined"}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-child","item":{"id":"child-late-completion","type":"agentMessage","text":"must not resurrect"}}})
            with open(__file__ + ".late-events-written", "w") as marker:
                marker.write("done")
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"identity-child","turn":{"id":"identity-child-turn","status":"completed"}}})
            send({"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"identity-thread","item":{"id":"spawn-child","type":"collabAgentToolCall","tool":"spawn","senderThreadId":"identity-thread","receiverThreadId":"identity-child","status":"completed"}}})
            send({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"identity-thread","turn":{"id":"identity-turn-two","status":"completed"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"turn":{"id":"identity-turn"}}})
    elif method == "turn/interrupt":
        send({"jsonrpc":"2.0","id":request_id,"result":{}})
    elif method == "thread/settings/update":
        with open(__file__ + ".settings-update", "w", encoding="utf-8") as settings_file:
            json.dump(params, settings_file, separators=(",", ":"))
        send({"jsonrpc":"2.0","id":request_id,"result":{}})
"#
        .replace("identity-thread", &thread_id)
        .replace("identity-child", &child_thread_id);
        std::fs::write(&binary, program).expect("write Codex identity fake");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&binary)
                .expect("Codex identity fake metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&binary, permissions).expect("chmod Codex identity fake");
        }
        let late_events_written =
            PathBuf::from(format!("{}.late-events-written", binary.to_string_lossy()));
        let settings_update =
            PathBuf::from(format!("{}.settings-update", binary.to_string_lossy()));
        let followup_release =
            PathBuf::from(format!("{}.followup-release", binary.to_string_lossy()));
        Self {
            _dir: dir,
            binary,
            thread_id,
            late_events_written,
            settings_update,
            followup_release,
        }
    }
}

#[derive(Debug, Default)]
struct CodexIdentityObservation {
    stream_starts: Vec<String>,
    stream_deltas: Vec<(String, String)>,
    stream_ends: Vec<(String, String)>,
    errors: usize,
    identity_errors: usize,
    cancellations: usize,
    unexpected_post_cancel_events: Vec<&'static str>,
    idle_transitions: usize,
    cancel_idle_transitions: usize,
    active_transitions: usize,
    tool_requests: Vec<String>,
    stream_end_tool_calls: HashMap<String, Vec<String>>,
}

fn prohibited_post_cancel_event(event: &ChatEvent) -> Option<&'static str> {
    match event {
        ChatEvent::TaskUpdate(_) => Some("TaskUpdate"),
        ChatEvent::StreamStart(_) => Some("StreamStart"),
        ChatEvent::StreamDelta(_) => Some("StreamDelta"),
        ChatEvent::StreamReasoningDelta(_) => Some("StreamReasoningDelta"),
        ChatEvent::StreamEnd(_) => Some("StreamEnd"),
        ChatEvent::MessageMetadataUpdated(_) => Some("MessageMetadataUpdated"),
        ChatEvent::MessageAdded(message)
            if matches!(
                message.sender,
                MessageSender::Error | MessageSender::Assistant { .. }
            ) =>
        {
            Some("MessageAdded")
        }
        ChatEvent::OperationCancelled(_) => Some("OperationCancelled"),
        ChatEvent::TypingStatusChanged(false) => Some("TypingStatusChanged(false)"),
        ChatEvent::TypingStatusChanged(true) => Some("TypingStatusChanged(true)"),
        _ => None,
    }
}

impl CodexIdentityObservation {
    fn observe(&mut self, event: ChatEvent) {
        let expected_first_cancel_idle = self.cancellations > 0
            && self.cancel_idle_transitions == 0
            && matches!(event, ChatEvent::TypingStatusChanged(false));
        if self.cancellations > 0
            && !expected_first_cancel_idle
            && let Some(kind) = prohibited_post_cancel_event(&event)
        {
            self.unexpected_post_cancel_events.push(kind);
        }
        match event {
            ChatEvent::StreamStart(start) => self.stream_starts.push(
                start
                    .message_id
                    .expect("Codex StreamStart must carry message identity"),
            ),
            ChatEvent::StreamDelta(delta) => self.stream_deltas.push((
                delta
                    .message_id
                    .expect("Codex StreamDelta must carry message identity"),
                delta.text,
            )),
            ChatEvent::StreamEnd(end) => {
                let message_id = end
                    .message
                    .message_id
                    .expect("Codex StreamEnd must carry message identity")
                    .0;
                self.stream_end_tool_calls.insert(
                    message_id.clone(),
                    end.message
                        .tool_calls
                        .into_iter()
                        .map(|call| call.id)
                        .collect(),
                );
                self.stream_ends.push((message_id, end.message.content));
            }
            ChatEvent::MessageAdded(ChatMessage {
                sender: MessageSender::Error,
                content,
                ..
            }) => {
                self.errors += 1;
                if content.contains("Stream identity violation: foreign active message id") {
                    self.identity_errors += 1;
                }
            }
            ChatEvent::OperationCancelled(_) => self.cancellations += 1,
            ChatEvent::ToolRequest(request) => self.tool_requests.push(request.tool_call_id),
            ChatEvent::TypingStatusChanged(false) => {
                self.idle_transitions += 1;
                if self.cancellations > 0 {
                    self.cancel_idle_transitions += 1;
                }
            }
            ChatEvent::TypingStatusChanged(true) => self.active_transitions += 1,
            _ => {}
        }
    }

    fn observe_bootstrap(&mut self, bootstrap: AgentBootstrapPayload) {
        for event in bootstrap.events {
            if let AgentBootstrapEvent::ChatEvent(event) = event {
                self.observe(event);
            }
        }
    }
}

async fn connect_and_replay_agent(
    fixture: &Fixture,
    agent_id: &protocol::AgentId,
    context: &str,
) -> (client::Connection, NewAgentPayload, AgentBootstrapPayload) {
    let (mut client, bootstrap) = fixture.connect_with_bootstrap().await;
    let agent = bootstrap
        .agents
        .into_iter()
        .find(|agent| &agent.agent_id == agent_id)
        .unwrap_or_else(|| panic!("{context} HostBootstrap missing agent {agent_id}"));
    loop {
        let env = expect_fixture_event(&mut client, context).await;
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
            panic!("command error while waiting for {context}: {error:?}");
        }
        if env.kind == FrameKind::AgentBootstrap && env.stream == agent.instance_stream {
            let replay = env.parse_payload().expect("parse AgentBootstrap");
            return (client, agent, replay);
        }
    }
}

fn replayed_assistant_messages(bootstrap: &AgentBootstrapPayload) -> Vec<(String, String)> {
    bootstrap
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))
                if matches!(message.sender, MessageSender::Assistant { .. }) =>
            {
                Some((
                    message
                        .message_id
                        .as_ref()
                        .expect("replayed assistant message must retain identity")
                        .0
                        .clone(),
                    message.content.clone(),
                ))
            }
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end)) => Some((
                end.message
                    .message_id
                    .as_ref()
                    .expect("replayed StreamEnd must retain identity")
                    .0
                    .clone(),
                end.message.content.clone(),
            )),
            _ => None,
        })
        .collect()
}

fn replayed_success_messages(
    bootstrap: &AgentBootstrapPayload,
    context: &str,
) -> Vec<(String, String)> {
    let mut observation = CodexIdentityObservation::default();
    observation.observe_bootstrap(bootstrap.clone());
    assert_eq!(
        observation.errors, 0,
        "{context} must not contain an error tail"
    );
    assert_eq!(
        observation.identity_errors, 0,
        "{context} must not contain an identity error"
    );
    assert_eq!(
        observation.cancellations, 0,
        "{context} must not contain a cancellation tail"
    );
    replayed_assistant_messages(bootstrap)
}

fn assert_replayed_tool_container_declares_card(
    bootstrap: &AgentBootstrapPayload,
    tool_call_id: &str,
    context: &str,
) {
    let mut observation = CodexIdentityObservation::default();
    observation.observe_bootstrap(bootstrap.clone());
    assert!(
        observation
            .tool_requests
            .iter()
            .any(|request_id| request_id == tool_call_id),
        "{context} must replay the tool request"
    );
    assert_eq!(
        observation.stream_end_tool_calls.get(tool_call_id),
        Some(&vec![tool_call_id.to_owned()]),
        "{context} tool container must declare its card for history attachment"
    );
}

#[tokio::test]
async fn fake_codex_provider_items_keep_identity_live_late_and_same_host_reconnect() {
    init_tracing();

    let fake = CodexIdentityFake::new();
    let _fake_guard = server::backend::codex::install_test_app_server_binary(fake.binary.clone());
    let workspace = tempfile::tempdir().expect("create Codex identity workspace");
    std::fs::write(
        workspace.path().join("README.txt"),
        "Codex identity test workspace",
    )
    .expect("seed Codex identity workspace");
    let mut fixture = Fixture::new_with_real_codex_backend_and_probe_program(
        fake.binary.to_string_lossy().into_owned(),
    )
    .await;
    let mut session_settings = SessionSettingsValues::default();
    session_settings.0.insert(
        "model".to_owned(),
        SessionSettingValue::String("fake-codex-model".to_owned()),
    );
    session_settings.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String("high".to_owned()),
    );
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Codex identity".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace.path().to_string_lossy().into_owned()],
                prompt: "emit three provider items".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: Some(session_settings),
            },
        })
        .await
        .expect("spawn fake Codex agent");

    let parent = loop {
        let env = expect_fixture_event(&mut fixture.client, "fake Codex NewAgent").await;
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
            panic!("fake Codex spawn failed: {error:?}");
        }
        if env.kind == FrameKind::NewAgent {
            let agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
            if agent.backend_kind == BackendKind::Codex {
                break agent;
            }
        }
    };
    fixture
        .client
        .send_message(
            &parent.instance_stream,
            "exercise child identity quarantine".to_owned(),
        )
        .await
        .expect("queue fake Codex follow-up before authoritative idle");

    let expected_parent = vec![
        ("parent-tool".to_owned(), String::new()),
        ("parent-one".to_owned(), "First response".to_owned()),
        ("parent-two".to_owned(), "Second response".to_owned()),
    ];
    let mut parent_live = CodexIdentityObservation::default();
    loop {
        let env =
            expect_fixture_event(&mut fixture.client, "completed live Codex provider turn").await;
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
            panic!("command error during fake Codex turn: {error:?}");
        }
        if env.stream != parent.instance_stream {
            continue;
        }
        let reached_idle = match env.kind {
            FrameKind::AgentBootstrap => {
                let bootstrap: AgentBootstrapPayload =
                    env.parse_payload().expect("parse parent AgentBootstrap");
                let mut reached_idle = false;
                for event in bootstrap.events {
                    if let AgentBootstrapEvent::ChatEvent(event) = event {
                        reached_idle = matches!(&event, ChatEvent::TypingStatusChanged(false));
                        parent_live.observe(event);
                        if reached_idle {
                            break;
                        }
                    }
                }
                reached_idle
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse parent ChatEvent");
                let reached_idle = matches!(&event, ChatEvent::TypingStatusChanged(false));
                parent_live.observe(event);
                reached_idle
            }
            _ => false,
        };
        if reached_idle {
            assert_eq!(
                parent_live.stream_ends, expected_parent,
                "every published provider terminal must precede authoritative idle"
            );
            break;
        }
    }

    assert_eq!(
        parent_live.stream_starts,
        vec!["parent-tool", "parent-one", "parent-two"]
    );
    assert_eq!(
        parent_live.stream_deltas,
        vec![
            ("parent-one".to_owned(), "First ".to_owned()),
            ("parent-one".to_owned(), "response".to_owned()),
        ]
    );
    assert_eq!(parent_live.stream_ends, expected_parent);
    assert!(
        parent_live
            .stream_starts
            .iter()
            .all(|id| id != "parent-empty")
    );
    assert_eq!(parent_live.tool_requests, vec!["parent-tool"]);
    assert_eq!(
        parent_live.stream_end_tool_calls.get("parent-tool"),
        Some(&vec!["parent-tool".to_owned()])
    );
    assert_eq!(parent_live.errors, 0);
    assert_eq!(parent_live.identity_errors, 0);
    assert_eq!(parent_live.cancellations, 0);
    assert_eq!(parent_live.idle_transitions, 1);
    let settings_update: Value = serde_json::from_slice(
        &std::fs::read(&fake.settings_update).expect("read captured Codex settings update"),
    )
    .expect("parse captured Codex settings update");
    assert_eq!(
        settings_update,
        json!({
            "threadId": fake.thread_id.clone(),
            "model": "fake-codex-model",
            "effort": "high",
            "approvalPolicy": "never"
        })
    );

    let (mut late_client, late_agent, late_bootstrap) =
        connect_and_replay_agent(&fixture, &parent.agent_id, "late Codex attach").await;
    assert_ne!(late_agent.instance_stream, parent.instance_stream);
    assert_eq!(
        replayed_success_messages(&late_bootstrap, "late Codex attach replay"),
        expected_parent
    );
    assert_replayed_tool_container_declares_card(
        &late_bootstrap,
        "parent-tool",
        "late Codex attach replay",
    );
    let history_request_id = protocol::HistoryPageRequestId("fake-codex-paged-history".to_owned());
    late_client
        .fetch_session_history(
            &late_agent.instance_stream,
            FetchSessionHistoryPayload {
                agent_id: parent.agent_id.clone(),
                request_id: history_request_id.clone(),
                before_seq: None,
                limit: 100,
            },
        )
        .await
        .expect("fetch fake Codex paged history");
    let paged_history = loop {
        let env = expect_fixture_event(&mut late_client, "fake Codex paged history").await;
        if env.kind == FrameKind::SessionHistory && env.stream == late_agent.instance_stream {
            break env
                .parse_payload::<SessionHistoryPayload>()
                .expect("parse fake Codex paged history");
        }
    };
    assert_eq!(paged_history.request_id, history_request_id);
    let mut paged_observation = CodexIdentityObservation::default();
    for event in paged_history.events.clone() {
        paged_observation.observe(event);
    }
    assert_eq!(
        paged_history
            .events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Assistant { .. }) =>
                {
                    Some((
                        message
                            .message_id
                            .as_ref()
                            .expect("paged assistant message identity")
                            .0
                            .clone(),
                        message.content.clone(),
                    ))
                }
                ChatEvent::StreamEnd(end) => Some((
                    end.message
                        .message_id
                        .as_ref()
                        .expect("paged stream identity")
                        .0
                        .clone(),
                    end.message.content.clone(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![
            ("parent-two".to_owned(), "Second response".to_owned()),
            ("parent-one".to_owned(), "First response".to_owned()),
            ("parent-tool".to_owned(), String::new()),
        ]
    );
    assert_eq!(
        paged_observation.stream_end_tool_calls.get("parent-tool"),
        Some(&vec!["parent-tool".to_owned()]),
        "paged Codex history tool container must declare its card"
    );
    assert_eq!(paged_observation.tool_requests, vec!["parent-tool"]);
    drop(late_client);

    let (same_host_reconnect_client, same_host_reconnect_agent, same_host_reconnect_bootstrap) =
        connect_and_replay_agent(
            &fixture,
            &parent.agent_id,
            "fresh Codex connection to same host",
        )
        .await;
    assert_ne!(
        same_host_reconnect_agent.instance_stream,
        late_agent.instance_stream
    );
    assert_eq!(
        replayed_success_messages(
            &same_host_reconnect_bootstrap,
            "fresh Codex connection to same host replay",
        ),
        expected_parent
    );
    assert_replayed_tool_container_declares_card(
        &same_host_reconnect_bootstrap,
        "parent-tool",
        "fresh Codex connection to same host replay",
    );
    drop(same_host_reconnect_client);

    std::fs::write(&fake.followup_release, b"release")
        .expect("release queued fake Codex follow-up");

    let mut child = None;
    let mut child_live = CodexIdentityObservation::default();
    let mut parent_followup = CodexIdentityObservation::default();
    while child_live.identity_errors < 1
        || child_live.cancellations < 1
        || child_live.cancel_idle_transitions < 1
        || parent_followup.active_transitions < 1
        || parent_followup.idle_transitions < 1
        || parent_followup.stream_ends.is_empty()
    {
        let env = expect_fixture_event(&mut fixture.client, "fake Codex child identity flow").await;
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
            panic!("command error during fake Codex child flow: {error:?}");
        }
        if env.kind == FrameKind::NewAgent {
            let agent: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
            if agent.origin == AgentOrigin::BackendNative
                && agent.parent_agent_id.as_ref() == Some(&parent.agent_id)
            {
                child = Some(agent);
            }
            continue;
        }
        if env.stream == parent.instance_stream && env.kind == FrameKind::ChatEvent {
            let event: ChatEvent = env
                .parse_payload()
                .expect("parse parent follow-up ChatEvent");
            parent_followup.observe(event);
            continue;
        }
        let Some(child_agent) = child.as_ref() else {
            continue;
        };
        if env.stream != child_agent.instance_stream {
            continue;
        }
        match env.kind {
            FrameKind::AgentBootstrap => child_live
                .observe_bootstrap(env.parse_payload().expect("parse child AgentBootstrap")),
            FrameKind::ChatEvent => {
                child_live.observe(env.parse_payload().expect("parse child ChatEvent"))
            }
            _ => {}
        }
    }

    let child = child.expect("fake Codex flow must advertise backend-native child");
    assert_eq!(
        parent_followup.stream_starts,
        vec!["parent-followup", "spawn-child"]
    );
    assert!(parent_followup.stream_deltas.is_empty());
    assert_eq!(
        parent_followup.stream_ends,
        vec![
            ("parent-followup".to_owned(), "Starting child".to_owned()),
            ("spawn-child".to_owned(), String::new()),
        ]
    );
    assert_eq!(parent_followup.errors, 0);
    assert_eq!(parent_followup.identity_errors, 0);
    assert_eq!(parent_followup.cancellations, 0);
    assert_eq!(
        parent_followup.active_transitions, 1,
        "Codex must emit active only once while the turn remains active"
    );
    assert_eq!(parent_followup.idle_transitions, 1);
    assert_eq!(child_live.stream_starts, vec!["child-good", "child-active"]);
    assert_eq!(
        child_live.stream_deltas,
        vec![
            ("child-good".to_owned(), "Child response".to_owned()),
            (
                "child-active".to_owned(),
                "Valid before interleave".to_owned(),
            ),
        ]
    );
    assert!(expected_parent.iter().all(|(id, _)| id != "child-good"));
    // Corrected under the authorized finalize-before-terminate contract. This
    // previously demanded that `child-active` produce no terminal at all, encoding the
    // old behavior where strict child termination silently destroyed a live published
    // stream. Rendered evidence from the failing run:
    //
    //   left:  [("child-good", "Child response"), ("child-active", "Valid before interleave")]
    //   right: [("child-good", "Child response")]
    //
    // The extra row carries `child-active`'s accepted bytes verbatim, which is required
    // production behavior now, and the reconnect expectation below already demanded both
    // messages -- so the old live expectation contradicted this test's own replay half.
    // The assertion is narrowed, not relaxed: it now pins both ids, both contents, and
    // their order, where before it pinned one id and asserted the other away.
    assert_eq!(
        child_live.stream_ends,
        vec![
            ("child-good".to_owned(), "Child response".to_owned()),
            (
                "child-active".to_owned(),
                "Valid before interleave".to_owned(),
            ),
        ],
        "strict child termination must finalize the live published item at its own id \
         with its accepted bytes before the terminal tail"
    );
    for forbidden in ["child-foreign", "child-late-delta", "child-late-completion"] {
        assert!(
            child_live.stream_starts.iter().all(|id| id != forbidden)
                && child_live
                    .stream_deltas
                    .iter()
                    .all(|(id, _)| id != forbidden)
                && child_live.stream_ends.iter().all(|(id, _)| id != forbidden),
            "late or foreign child item {forbidden} must not resurrect: {child_live:?}"
        );
    }
    assert_eq!(child_live.errors, 1);
    assert_eq!(child_live.identity_errors, 1);
    assert_eq!(child_live.cancellations, 1);
    assert_eq!(child_live.idle_transitions, 1);
    assert_eq!(child_live.cancel_idle_transitions, 1);
    assert!(child_live.unexpected_post_cancel_events.is_empty());

    tokio::time::timeout(Duration::from_secs(5), async {
        while !fake.late_events_written.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake Codex app-server did not write late-event transport marker");

    loop {
        match tokio::time::timeout(Duration::from_millis(250), fixture.client.next_event()).await {
            Err(_) => break,
            Ok(Ok(Some(env))) if env.kind == FrameKind::BackendCapacity => {}
            Ok(Ok(Some(env))) if env.kind == FrameKind::CommandError => {
                let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
                panic!("unexpected command error during post-cancel quiet check: {error:?}");
            }
            Ok(Ok(Some(env)))
                if env.kind == FrameKind::ChatEvent
                    && (env.stream == parent.instance_stream
                        || env.stream == child.instance_stream) =>
            {
                let event: ChatEvent = env.parse_payload().expect("parse quiet-check ChatEvent");
                if let Some(kind) = prohibited_post_cancel_event(&event) {
                    panic!(
                        "unexpected {kind} after fake Codex cancellation on {}: {event:?}",
                        env.stream
                    );
                }
                if env.stream == child.instance_stream {
                    child_live.observe(event);
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => panic!("connection closed during post-cancel quiet check"),
            Ok(Err(error)) => panic!("post-cancel quiet check failed: {error:?}"),
        }
    }
    assert_eq!(child_live.errors, 1);
    assert_eq!(child_live.identity_errors, 1);
    assert_eq!(child_live.cancellations, 1);
    assert_eq!(child_live.idle_transitions, 1);
    assert_eq!(child_live.cancel_idle_transitions, 1);
    assert!(child_live.unexpected_post_cancel_events.is_empty());

    let (_child_replay_client, replayed_child, child_bootstrap) =
        connect_and_replay_agent(&fixture, &child.agent_id, "late child replay").await;
    assert_ne!(replayed_child.instance_stream, child.instance_stream);
    let replayed_child_messages = replayed_assistant_messages(&child_bootstrap);
    assert_eq!(
        replayed_child_messages,
        vec![
            ("child-good".to_owned(), "Child response".to_owned()),
            (
                "child-active".to_owned(),
                "Valid before interleave".to_owned(),
            ),
        ]
    );
    assert!(replayed_child_messages.iter().all(|(id, _)| {
        !matches!(
            id.as_str(),
            "child-foreign" | "child-late-delta" | "child-late-completion"
        )
    }));
    let mut child_replay = CodexIdentityObservation::default();
    child_replay.observe_bootstrap(child_bootstrap);
    assert_eq!(child_replay.errors, 1);
    assert_eq!(child_replay.identity_errors, 1);
    assert_eq!(child_replay.cancellations, 1);
    assert_eq!(
        child_replay.active_transitions, 0,
        "bootstrap history omits transient active state"
    );
    assert_eq!(
        child_replay.idle_transitions, 0,
        "bootstrap history omits transient idle state"
    );
    assert_eq!(child_replay.cancel_idle_transitions, 0);
    assert!(child_replay.unexpected_post_cancel_events.is_empty());
}

/// Byte-for-byte copy of the private `CODEX_SUPERSESSION_WARNING` in
/// `server/src/backend/codex.rs`. The constant is not exported, so the user-visible
/// recovery string is pinned here by value: changing the adapter's wording must
/// consciously update this expectation rather than silently drop the guarantee.
const CODEX_SUPERSESSION_WARNING_TEXT: &str = "Codex restarted part of its response mid-turn. \
The partial output above was kept and the turn continued.";

/// Deterministic fake Codex app-server for provider-item lifecycle scenarios.
///
/// Deliberately a sibling of `CodexIdentityFake` rather than an extension of it: that
/// fixture's adversarial child assertions (`identity_errors == 1`) must keep failing
/// closed, and appending turns to its script would force that test to drive extra
/// sends. Both fakes are local Python stdio programs — no API calls, no cost, and not
/// gated by `TYDE_RUN_REAL_AI_TESTS`.
struct CodexLifecycleFake {
    _dir: tempfile::TempDir,
    binary: PathBuf,
    turn_starts: PathBuf,
}

/// The exact follow-up the termination scenario expects. The fake refuses to run
/// its success path for any other input, so a synthetic retry cannot be mistaken
/// for the explicit user message.
const CODEX_LIFECYCLE_FOLLOW_UP: &str = "drive the next turn";

/// The spawn prompt, pinned so the `turn/start` ledger can be asserted exactly.
const CODEX_LIFECYCLE_SPAWN_PROMPT: &str = "drive the first provider turn";

impl CodexLifecycleFake {
    fn new(scenario: &str) -> Self {
        let dir = tempfile::tempdir().expect("create Codex lifecycle fake tempdir");
        let binary = dir.path().join("codex-lifecycle-app-server.py");
        let program = r#"#!/usr/bin/env python3
import json
import sys

SCENARIO = "__SCENARIO__"
THREAD = "lifecycle-thread"
FOLLOW_UP_TEXT = "__FOLLOW_UP__"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def note(method, params):
    send({"jsonrpc":"2.0","method":method,"params":params})

def turn_started(turn_id):
    note("turn/started", {"threadId":THREAD,"turn":{"id":turn_id}})

def turn_completed(turn_id):
    note("turn/completed", {"threadId":THREAD,"turn":{"id":turn_id,"status":"completed"}})

def item_started(item_id, item_kind):
    note("item/started", {"threadId":THREAD,"item":{"id":item_id,"type":item_kind}})

def item_delta(item_id, item_kind, text):
    if item_kind == "agentMessage":
        method = "item/agentMessage/delta"
    else:
        method = "item/reasoning/delta"
    note(method, {"threadId":THREAD,"itemId":item_id,"delta":text})

def item_completed(item_id, item_kind, text):
    body = {"id":item_id,"type":item_kind}
    if item_kind == "agentMessage":
        body["text"] = text
    else:
        body["summary"] = text
    note("item/completed", {"threadId":THREAD,"item":body})

def command(item_id):
    note("item/started", {"threadId":THREAD,"item":{"id":item_id,"type":"commandExecution","command":"pwd","cwd":"/tmp"}})
    note("item/completed", {"threadId":THREAD,"item":{"id":item_id,"type":"commandExecution","exitCode":0,"aggregatedOutput":"/tmp"}})

def publish_then_roll_over(first_id, first_kind, second_id, second_kind):
    # A publishes real content and the provider then abandons it without ever
    # sending item/completed(A) -- the captured production shape.
    item_started(first_id, first_kind)
    item_delta(first_id, first_kind, "accepted " + first_id)
    item_started(second_id, second_kind)
    item_delta(second_id, second_kind, "continued " + second_id)
    item_completed(second_id, second_kind, "continued " + second_id)
    # A compatible late completion for the superseded item: same kind, and for
    # agent messages exactly the accepted prefix. It must be absorbed silently.
    item_completed(first_id, first_kind, "accepted " + first_id)

def rollover_turn(turn_id, first_id, first_kind, second_id, second_kind):
    turn_started(turn_id)
    publish_then_roll_over(first_id, first_kind, second_id, second_kind)
    turn_completed(turn_id)
    return turn_id

def unexpected_turn(turn_count):
    # Any turn the test did not explicitly ask for is reported with a distinct
    # item id so an automatic retry can never borrow the success path.
    turn_id = "unexpected-turn-" + str(turn_count)
    turn_started(turn_id)
    item_started("unexpected-turn-start", "agentMessage")
    item_delta("unexpected-turn-start", "agentMessage", "unexpected")
    item_completed("unexpected-turn-start", "agentMessage", "unexpected")
    turn_completed(turn_id)
    return turn_id

def supersession_turn(turn_count):
    if turn_count == 1:
        # Production ordering: the command completes BEFORE the abandoned item
        # starts, matching the rollout. Never place a tool between A and B.
        turn_id = "rollover-turn-one"
        turn_started(turn_id)
        command("tool-before-rollover")
        publish_then_roll_over("reason-a", "reasoning", "reason-b", "reasoning")
        command("tool-after-rollover")
        item_started("final-answer", "agentMessage")
        item_delta("final-answer", "agentMessage", "turn survived")
        item_completed("final-answer", "agentMessage", "turn survived")
        turn_completed(turn_id)
        return turn_id
    if turn_count == 2:
        return rollover_turn("rollover-turn-two", "agent-a", "agentMessage", "agent-b", "agentMessage")
    if turn_count == 3:
        return rollover_turn("rollover-turn-three", "reason-c", "reasoning", "agent-c", "agentMessage")
    if turn_count == 4:
        return rollover_turn("rollover-turn-four", "agent-d", "agentMessage", "reason-d", "reasoning")
    return unexpected_turn(turn_count)

def termination_turn(turn_count, text):
    if turn_count == 1:
        turn_id = "termination-turn-one"
        turn_started(turn_id)
        item_started("open-item", "agentMessage")
        item_delta("open-item", "agentMessage", "partial work")
        # A foreign completion while open-item is still live. The turn must end
        # visibly and finitely; the provider deliberately never sends
        # turn/completed for it, so local state cannot depend on that arriving.
        item_completed("foreign-item", "agentMessage", "must not be attributed")
        return turn_id
    # The success path is gated on the explicit follow-up text, so a synthetic
    # retry cannot consume it and then let the real message pass unobserved.
    if text != FOLLOW_UP_TEXT:
        return unexpected_turn(turn_count)
    turn_id = "termination-turn-two"
    turn_started(turn_id)
    item_started("recovered-item", "agentMessage")
    item_delta("recovered-item", "agentMessage", "next turn works")
    item_completed("recovered-item", "agentMessage", "next turn works")
    turn_completed(turn_id)
    return turn_id

def turn_start_text(params):
    for item in params.get("input") or []:
        if item.get("type") == "text":
            return item.get("text", "")
    return ""

def record_turn_start(index, text):
    with open(__file__ + ".turn-starts", "a", encoding="utf-8") as ledger:
        entry = {"index":index,"input":text}
        ledger.write(json.dumps(entry, separators=(",", ":")) + "\n")

turn_count = 0
for line in sys.stdin:
    try:
        request = json.loads(line)
    except Exception:
        continue
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params", {})
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"userAgent":"fake-codex/lifecycle","codexHome":"/tmp/fake-codex-home","platformFamily":"unix","platformOs":"test"}})
    elif method == "model/list":
        send({"jsonrpc":"2.0","id":request_id,"result":{"data":[{"model":"fake-codex-model","isDefault":True,"supportedReasoningEfforts":[{"reasoningEffort":"high"}]}]}})
    elif method == "thread/start":
        send({"jsonrpc":"2.0","id":request_id,"result":{"thread":{"id":THREAD,"sessionId":THREAD,"turns":[]},"model":"fake-codex-model"}})
    elif method == "turn/start":
        turn_count += 1
        text = turn_start_text(params)
        record_turn_start(turn_count, text)
        if SCENARIO == "supersession":
            turn_id = supersession_turn(turn_count)
        else:
            turn_id = termination_turn(turn_count, text)
        send({"jsonrpc":"2.0","id":request_id,"result":{"turn":{"id":turn_id}}})
    elif method == "turn/interrupt":
        send({"jsonrpc":"2.0","id":request_id,"result":{}})
    elif method == "thread/settings/update":
        send({"jsonrpc":"2.0","id":request_id,"result":{}})
"#
        .replace("__SCENARIO__", scenario)
        .replace("__FOLLOW_UP__", CODEX_LIFECYCLE_FOLLOW_UP);
        std::fs::write(&binary, program).expect("write Codex lifecycle fake");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&binary)
                .expect("Codex lifecycle fake metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&binary, permissions).expect("chmod Codex lifecycle fake");
        }
        let turn_starts = PathBuf::from(format!("{}.turn-starts", binary.to_string_lossy()));
        Self {
            _dir: dir,
            binary,
            turn_starts,
        }
    }

    /// Every `turn/start` the adapter issued, in order, with the input text that
    /// reached the provider. This is the ledger that distinguishes an explicit
    /// user message from a forbidden automatic retry.
    fn turn_start_inputs(&self) -> Vec<String> {
        let Ok(contents) = std::fs::read_to_string(&self.turn_starts) else {
            return Vec::new();
        };
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let entry: Value =
                    serde_json::from_str(line).expect("parse recorded turn/start ledger entry");
                entry
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }
}

/// Ordered projection of the client-visible event stream.
///
/// `CodexIdentityObservation` keeps one vector per event kind, which cannot express
/// ordering *between* kinds — and `End(A) -> Warning -> Start(B)` is exactly a
/// cross-kind ordering contract. This type records a single ordered log instead, and
/// captures terminal reasoning, which a superseded reasoning item carries in place of
/// content.
///
/// A message has **two durable representations**, and both must be observed.
/// `record_chat_event_for_replay` (`server/src/agent/mod.rs`, `ChatEvent::StreamEnd`
/// arm) branches on `retains_explicit_stream = !message.tool_calls.is_empty()`:
///
/// - with tool calls, replay keeps the explicit `StreamStart` / delta / `StreamEnd`
///   framing — this is why the tool containers keep their stream lifecycle;
/// - without tool calls, replay records a single canonical
///   `ChatEvent::MessageAdded(message)` carrying the id, content, and reasoning.
///
/// Codex emits provider items with `tool_calls: Vec::new()`, so `reason-a`, `reason-b`,
/// and `final-answer` are assistant `MessageAdded` rows in settled history even though
/// they were live `StreamEnd`s. An observer that only recorded `StreamEnd` would see
/// live turns correctly and then find replay "empty" — which is exactly the false
/// negative this projection previously produced.
///
/// The two representations are kept as **distinct variants**, and every helper is
/// representation-specific, so an assertion always states which shape it requires.
/// Collapsing them behind a framing-agnostic accessor would make a live assertion
/// satisfiable by a canonical row and a replay assertion satisfiable by leaked live
/// framing — losing the contract in both directions at once.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexLifecycleEvent {
    StreamStart(String),
    StreamDelta(String),
    StreamReasoningDelta(String),
    /// Terminal row in live stream framing.
    StreamEnd(String),
    /// Terminal row in settled canonical history framing.
    AssistantMessage(String),
    Warning(String),
    Error(String),
    ToolRequest(String),
    ToolCompleted(String),
    Cancelled,
    Typing(bool),
}

/// Payload state is deliberately **kept per representation**. A shared map would let a
/// live assertion be satisfied by a canonical `MessageAdded`, silently weakening the
/// live wire contract: a matrix turn's B, or the recovered follow-up item, would pass
/// its payload check without ever having emitted a live `StreamEnd`. Live assertions
/// read the `stream_end_*` maps and can only be satisfied by `ChatEvent::StreamEnd`;
/// replay assertions read the `assistant_*` maps and can only be satisfied by the
/// canonical assistant `MessageAdded`. `StreamStart` populates neither.
#[derive(Debug, Default)]
struct CodexLifecycleObservation {
    ordered: Vec<CodexLifecycleEvent>,
    /// Written only by `ChatEvent::StreamEnd` — the live wire representation.
    stream_end_content: HashMap<String, String>,
    stream_end_reasoning: HashMap<String, Option<String>>,
    /// Written only by assistant `ChatEvent::MessageAdded` — settled canonical history.
    assistant_content: HashMap<String, String>,
    assistant_reasoning: HashMap<String, Option<String>>,
}

impl CodexLifecycleObservation {
    fn observe(&mut self, event: ChatEvent) {
        let observed = match event {
            ChatEvent::StreamStart(start) => {
                let id = start.message_id.expect("StreamStart needs identity");
                Some(CodexLifecycleEvent::StreamStart(id))
            }
            ChatEvent::StreamDelta(delta) => {
                let id = delta.message_id.expect("StreamDelta needs identity");
                Some(CodexLifecycleEvent::StreamDelta(id))
            }
            ChatEvent::StreamReasoningDelta(delta) => {
                let id = delta.message_id.expect("ReasoningDelta needs identity");
                Some(CodexLifecycleEvent::StreamReasoningDelta(id))
            }
            ChatEvent::StreamEnd(end) => {
                let message = end.message;
                let id = message.message_id.expect("StreamEnd needs identity").0;
                self.stream_end_content.insert(id.clone(), message.content);
                let reasoning = message.reasoning.map(|reasoning| reasoning.text);
                self.stream_end_reasoning.insert(id.clone(), reasoning);
                Some(CodexLifecycleEvent::StreamEnd(id))
            }
            ChatEvent::MessageAdded(message) => match message.sender {
                MessageSender::Warning => Some(CodexLifecycleEvent::Warning(message.content)),
                MessageSender::Error => Some(CodexLifecycleEvent::Error(message.content)),
                // The canonical terminal representation for a no-tool stream. Dropping
                // this arm is what made settled history look empty for provider items.
                MessageSender::Assistant { .. } => {
                    let id = message
                        .message_id
                        .expect("assistant terminal needs identity")
                        .0;
                    self.assistant_content.insert(id.clone(), message.content);
                    let reasoning = message.reasoning.map(|reasoning| reasoning.text);
                    self.assistant_reasoning.insert(id.clone(), reasoning);
                    Some(CodexLifecycleEvent::AssistantMessage(id))
                }
                _ => None,
            },
            ChatEvent::ToolRequest(request) => {
                Some(CodexLifecycleEvent::ToolRequest(request.tool_call_id))
            }
            ChatEvent::ToolExecutionCompleted(done) => {
                Some(CodexLifecycleEvent::ToolCompleted(done.tool_call_id))
            }
            ChatEvent::OperationCancelled(_) => Some(CodexLifecycleEvent::Cancelled),
            ChatEvent::TypingStatusChanged(active) => Some(CodexLifecycleEvent::Typing(active)),
            _ => None,
        };
        if let Some(observed) = observed {
            self.ordered.push(observed);
        }
    }

    fn observe_bootstrap(&mut self, bootstrap: AgentBootstrapPayload) {
        for event in bootstrap.events {
            if let AgentBootstrapEvent::ChatEvent(event) = event {
                self.observe(event);
            }
        }
    }

    fn position(&self, expected: &CodexLifecycleEvent) -> usize {
        self.ordered
            .iter()
            .position(|event| event == expected)
            .unwrap_or_else(|| panic!("missing {expected:?} in observed order: {:?}", self.ordered))
    }

    fn count(&self, mut predicate: impl FnMut(&CodexLifecycleEvent) -> bool) -> usize {
        self.ordered.iter().filter(|event| predicate(event)).count()
    }

    fn occurrences(&self, expected: &CodexLifecycleEvent) -> usize {
        self.count(|event| event == expected)
    }

    /// Stream ids that were opened but never terminalized.
    fn unterminated_streams(&self) -> Vec<&str> {
        self.ordered
            .iter()
            .filter_map(|event| match event {
                CodexLifecycleEvent::StreamStart(id) => Some(id.as_str()),
                _ => None,
            })
            // Pairs a start with an actual `StreamEnd`, never with an unrelated
            // durable representation: a canonicalized assistant row does not close a
            // live stream.
            .filter(|id| self.stream_end_rows(id) == 0)
            .collect()
    }

    /// Position of a message's **canonical** terminal row. Deliberately matches only
    /// `AssistantMessage`: settled history must serve a no-tool provider item that way,
    /// and accepting `StreamEnd` here would let a replay that leaked live framing pass.
    fn assistant_position(&self, message_id: &str) -> usize {
        self.ordered
            .iter()
            .position(|event| match event {
                CodexLifecycleEvent::AssistantMessage(id) => id == message_id,
                _ => false,
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing canonical assistant terminal for {message_id} in observed \
                     order: {:?}",
                    self.ordered
                )
            })
    }

    /// Canonical terminal rows for a message. Exactly one is the contract: a superseded
    /// item is terminalized once, and an absorbed late completion must not add another.
    fn assistant_rows(&self, message_id: &str) -> usize {
        self.count(|event| match event {
            CodexLifecycleEvent::AssistantMessage(id) => id == message_id,
            _ => false,
        })
    }

    /// Live-framing terminal rows for a message. Exactly one on a live turn; exactly
    /// zero in settled history for a no-tool provider item.
    fn stream_end_rows(&self, message_id: &str) -> usize {
        self.count(|event| match event {
            CodexLifecycleEvent::StreamEnd(id) => id == message_id,
            _ => false,
        })
    }

    fn warnings(&self) -> Vec<&str> {
        self.ordered
            .iter()
            .filter_map(|event| match event {
                CodexLifecycleEvent::Warning(content) => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The recovery contract: A is terminalized at its own id, the user is told once
    /// in plain language, B then opens — and none of it looks like a failed turn.
    fn assert_recovered_rollover(
        &self,
        superseded_id: &str,
        replacement_id: &str,
        accepted: &str,
        superseded_is_reasoning: bool,
        context: &str,
    ) {
        let start_a = CodexLifecycleEvent::StreamStart(superseded_id.to_owned());
        let end_a = CodexLifecycleEvent::StreamEnd(superseded_id.to_owned());
        let warning_text = CODEX_SUPERSESSION_WARNING_TEXT.to_owned();
        let start_b = CodexLifecycleEvent::StreamStart(replacement_id.to_owned());
        let superseded_end = self.position(&end_a);
        let warning = self.position(&CodexLifecycleEvent::Warning(warning_text));
        let replacement_start = self.position(&start_b);
        assert!(
            superseded_end < warning && warning < replacement_start,
            "{context} must observe End(A) -> Warning -> Start(B): {:?}",
            self.ordered
        );
        assert_eq!(
            self.warnings(),
            vec![CODEX_SUPERSESSION_WARNING_TEXT],
            "{context} must surface exactly one recovery warning, verbatim"
        );
        if superseded_is_reasoning {
            assert_eq!(
                self.stream_end_reasoning.get(superseded_id),
                Some(&Some(accepted.to_owned())),
                "{context} must preserve the superseded item's accepted reasoning"
            );
            assert_eq!(
                self.stream_end_content.get(superseded_id),
                Some(&String::new()),
                "{context} reasoning terminal carries no assistant content"
            );
        } else {
            assert_eq!(
                self.stream_end_content.get(superseded_id),
                Some(&accepted.to_owned()),
                "{context} must preserve the superseded item's accepted text"
            );
        }
        assert_eq!(
            self.count(|event| matches!(event, CodexLifecycleEvent::Error(_))),
            0,
            "{context} recovery must not report an error: {:?}",
            self.ordered
        );
        assert_eq!(
            self.count(|event| matches!(event, CodexLifecycleEvent::Cancelled)),
            0,
            "{context} recovery must not cancel the turn: {:?}",
            self.ordered
        );
        // The driver stops at the first idle, so an idle emitted mid-recovery would
        // already have surfaced as a missing End(A)/Start(B) above. This pins the
        // remaining case: idle must land after the replacement stream is open.
        let idle = self.position(&CodexLifecycleEvent::Typing(false));
        assert!(
            idle > replacement_start,
            "{context} must not go idle mid-recovery: {:?}",
            self.ordered
        );
        // The fake sends a compatible late completion for A after B completes. It
        // must be absorbed: no second lifecycle for A, and nothing new on the wire.
        assert_eq!(
            self.occurrences(&start_a),
            1,
            "{context} late completion must not reopen the superseded item: {:?}",
            self.ordered
        );
        assert_eq!(
            self.occurrences(&end_a),
            1,
            "{context} late completion must not re-terminalize A: {:?}",
            self.ordered
        );
    }
}

async fn drive_codex_agent_to_idle(
    fixture: &mut Fixture,
    stream: &StreamPath,
    observation: &mut CodexLifecycleObservation,
    context: &str,
) {
    loop {
        let env = expect_fixture_event(&mut fixture.client, context).await;
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
            panic!("command error during {context}: {error:?}");
        }
        if env.stream != *stream {
            continue;
        }
        let reached_idle = match env.kind {
            FrameKind::AgentBootstrap => {
                let bootstrap: AgentBootstrapPayload =
                    env.parse_payload().expect("parse AgentBootstrap");
                let mut reached_idle = false;
                for event in bootstrap.events {
                    if let AgentBootstrapEvent::ChatEvent(event) = event {
                        reached_idle = matches!(&event, ChatEvent::TypingStatusChanged(false));
                        observation.observe(event);
                        if reached_idle {
                            break;
                        }
                    }
                }
                reached_idle
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                let reached_idle = matches!(&event, ChatEvent::TypingStatusChanged(false));
                observation.observe(event);
                reached_idle
            }
            _ => false,
        };
        if reached_idle {
            return;
        }
    }
}

async fn spawn_fake_codex_lifecycle_agent(
    fixture: &mut Fixture,
    workspace: &Path,
    name: &str,
) -> NewAgentPayload {
    let mut session_settings = SessionSettingsValues::default();
    session_settings.0.insert(
        "model".to_owned(),
        SessionSettingValue::String("fake-codex-model".to_owned()),
    );
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace.to_string_lossy().into_owned()],
                prompt: CODEX_LIFECYCLE_SPAWN_PROMPT.to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: Some(session_settings),
            },
        })
        .await
        .expect("spawn fake Codex lifecycle agent");

    loop {
        let env = expect_fixture_event(&mut fixture.client, "fake Codex lifecycle NewAgent").await;
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
            panic!("fake Codex lifecycle spawn failed: {error:?}");
        }
        if env.kind == FrameKind::NewAgent {
            let agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
            if agent.backend_kind == BackendKind::Codex {
                return agent;
            }
        }
    }
}

/// Regression guard for the captured incident: a provider-owned item that publishes
/// real output and is then abandoned mid-turn without `item/completed` must be
/// terminalized and superseded rather than destroying the turn.
///
/// The incident lost an applied patch, a commit, and the final answer because the
/// adapter rejected the replacement item. The load-bearing assertion here is not the
/// warning ordering — it is that the tool call *after* the rollover and the final
/// assistant message still reach the client, live and on replay.
#[tokio::test]
async fn fake_codex_provider_item_supersession_recovers_live_and_on_replay() {
    init_tracing();

    let fake = CodexLifecycleFake::new("supersession");
    let _fake_guard = server::backend::codex::install_test_app_server_binary(fake.binary.clone());
    let workspace = tempfile::tempdir().expect("create Codex supersession workspace");
    std::fs::write(
        workspace.path().join("README.txt"),
        "Codex supersession test workspace",
    )
    .expect("seed Codex supersession workspace");
    let mut fixture = Fixture::new_with_real_codex_backend_and_probe_program(
        fake.binary.to_string_lossy().into_owned(),
    )
    .await;
    let agent =
        spawn_fake_codex_lifecycle_agent(&mut fixture, workspace.path(), "Codex supersession")
            .await;

    // Turn one: the production shape -- tool, published reasoning A abandoned without
    // completion, reasoning B, then a further tool and the real answer.
    let mut first_turn = CodexLifecycleObservation::default();
    drive_codex_agent_to_idle(
        &mut fixture,
        &agent.instance_stream,
        &mut first_turn,
        "fake Codex reasoning rollover turn",
    )
    .await;
    first_turn.assert_recovered_rollover(
        "reason-a",
        "reason-b",
        "accepted reason-a",
        true,
        "reasoning rollover",
    );
    for tool_call_id in ["tool-before-rollover", "tool-after-rollover"] {
        let completed = CodexLifecycleEvent::ToolCompleted(tool_call_id.to_owned());
        assert!(
            first_turn.ordered.contains(&completed),
            "the turn must keep reporting {tool_call_id}: {:?}",
            first_turn.ordered
        );
    }
    assert_eq!(
        first_turn.stream_end_content.get("final-answer"),
        Some(&"turn survived".to_owned()),
        "the answer the provider produced after the rollover must reach the client"
    );
    assert_eq!(
        first_turn.stream_end_reasoning.get("reason-b"),
        Some(&Some("continued reason-b".to_owned())),
        "the replacement item must complete normally"
    );
    let end_a = CodexLifecycleEvent::StreamEnd("reason-a".to_owned());
    let tool_after = CodexLifecycleEvent::ToolCompleted("tool-after-rollover".to_owned());
    let end_final = CodexLifecycleEvent::StreamEnd("final-answer".to_owned());
    let rollover_end = first_turn.position(&end_a);
    let later_tool = first_turn.position(&tool_after);
    let final_end = first_turn.position(&end_final);
    assert!(
        rollover_end < later_tool && later_tool < final_end,
        "the remainder of the turn must follow the recovery in order: {:?}",
        first_turn.ordered
    );

    // A client attaching afterwards must see the same recovered history, in the same
    // order, with no stream left open. This is asserted here, while the recovered turn
    // is still the whole history: `INITIAL_HISTORY_TAIL_LIMIT` bounds bootstrap replay
    // to the last 15 terminal messages, so deferring it until after the kind-pair
    // turns below would silently drop this turn out of the tail.
    let (replay_client, replayed_agent, bootstrap) =
        connect_and_replay_agent(&fixture, &agent.agent_id, "Codex supersession replay").await;
    assert_ne!(replayed_agent.instance_stream, agent.instance_stream);
    let replay_turn_active = bootstrap.turn_active;
    let mut replay = CodexLifecycleObservation::default();
    replay.observe_bootstrap(bootstrap);
    assert!(
        !replay_turn_active,
        "a fully recovered turn must not replay as still active"
    );
    assert_eq!(
        replay.count(|event| matches!(event, CodexLifecycleEvent::Error(_))),
        0,
        "replayed supersession history must contain no error: {:?}",
        replay.ordered
    );
    assert_eq!(
        replay.count(|event| matches!(event, CodexLifecycleEvent::Cancelled)),
        0,
        "replayed supersession history must contain no cancellation: {:?}",
        replay.ordered
    );
    assert_eq!(
        replay.warnings(),
        vec![CODEX_SUPERSESSION_WARNING_TEXT],
        "replay must retain the recovery warning verbatim, exactly once"
    );
    // Settled history is canonical, not a transcript of the live wire.
    // `record_chat_event_for_replay` branches on
    // `retains_explicit_stream = !message.tool_calls.is_empty()`: Codex provider items
    // carry `tool_calls: Vec::new()`, so each is recorded as ONE assistant
    // `MessageAdded`, while the tool containers -- whose terminals declare tool calls --
    // keep explicit `StreamStart`/`StreamEnd` framing.
    //
    // These assertions are representation-specific on purpose. Accepting either shape
    // would let a regression that replayed the provider items as explicit `StreamEnd`
    // rows pass, which is exactly the noncanonical shape this half exists to catch.
    let replay_warning = replay.position(&CodexLifecycleEvent::Warning(
        CODEX_SUPERSESSION_WARNING_TEXT.to_owned(),
    ));
    let replay_tool_before = replay.position(&CodexLifecycleEvent::StreamEnd(
        "tool-before-rollover".to_owned(),
    ));
    let replay_a = replay.assistant_position("reason-a");
    let replay_b = replay.assistant_position("reason-b");
    let replay_tool_after_request = replay.position(&CodexLifecycleEvent::ToolRequest(
        "tool-after-rollover".to_owned(),
    ));
    let replay_tool_after = replay.position(&CodexLifecycleEvent::StreamEnd(
        "tool-after-rollover".to_owned(),
    ));
    let replay_final = replay.assistant_position("final-answer");
    assert!(
        replay_tool_before < replay_a && replay_a < replay_warning,
        "replay must keep the superseded item terminal before the recovery warning: {:?}",
        replay.ordered
    );
    assert!(
        replay_warning < replay_b,
        "replay must keep the recovery warning before the replacement terminal: {:?}",
        replay.ordered
    );
    assert!(
        replay_b < replay_tool_after_request
            && replay_tool_after_request < replay_tool_after
            && replay_tool_after < replay_final,
        "replay must keep the post-rollover tool and answer after the replacement: {:?}",
        replay.ordered
    );
    // Each provider item must appear exactly once, and only in canonical form. Zero
    // `StreamEnd` rows is the half that pins canonicalization; exactly one
    // `AssistantMessage` is the half that rejects both a lost row and a duplicate
    // produced by the absorbed late completion for A.
    for message_id in ["reason-a", "reason-b", "final-answer"] {
        assert_eq!(
            replay.assistant_rows(message_id),
            1,
            "replay must record exactly one canonical assistant row for {message_id}: {:?}",
            replay.ordered
        );
        assert_eq!(
            replay.stream_end_rows(message_id),
            0,
            "replay must not serve {message_id} in live stream framing: {:?}",
            replay.ordered
        );
    }
    // Tool containers keep explicit stream framing, so their live representation must
    // survive replay -- the contrapositive of the provider-item assertion above.
    for tool_call_id in ["tool-before-rollover", "tool-after-rollover"] {
        assert_eq!(
            replay.stream_end_rows(tool_call_id),
            1,
            "replay must keep explicit stream framing for tool container {tool_call_id}: {:?}",
            replay.ordered
        );
        assert_eq!(
            replay.assistant_rows(tool_call_id),
            0,
            "a tool container must not be canonicalized into an assistant row: {:?}",
            replay.ordered
        );
    }
    // Content and reasoning, not just order and shape: a reconnecting client must
    // render exactly what the live client saw. Reasoning items carry their text in
    // `reasoning` with empty content; the answer carries content and no reasoning.
    assert_eq!(
        replay.assistant_content.get("reason-a"),
        Some(&String::new()),
        "a superseded reasoning item carries no assistant content"
    );
    assert_eq!(
        replay.assistant_reasoning.get("reason-a"),
        Some(&Some("accepted reason-a".to_owned())),
        "replay must retain the superseded item's accepted reasoning verbatim"
    );
    assert_eq!(
        replay.assistant_content.get("reason-b"),
        Some(&String::new()),
        "the replacement reasoning item carries no assistant content"
    );
    assert_eq!(
        replay.assistant_reasoning.get("reason-b"),
        Some(&Some("continued reason-b".to_owned())),
        "replay must retain the replacement item's reasoning verbatim"
    );
    assert_eq!(
        replay.assistant_content.get("final-answer"),
        Some(&"turn survived".to_owned()),
        "replay must retain the post-rollover answer"
    );
    assert_eq!(
        replay.assistant_reasoning.get("final-answer"),
        Some(&None),
        "the post-rollover answer carries no reasoning"
    );
    let unterminated = replay.unterminated_streams();
    assert!(
        unterminated.is_empty(),
        "replay must leave no stream open, found {unterminated:?}: {:?}",
        replay.ordered
    );
    // Close the replay subscriber before driving more turns rather than leaving an
    // unread connection accumulating events behind the remaining matrix.
    drop(replay_client);

    // Remaining provider-owned kind pairs. Each rollover gets its own turn because
    // recovery allowance and the warning latch both reset only on turn/started.
    let kind_pairs = [
        ("agent-a", "agent-b", false, false, "agentMessage rollover"),
        (
            "reason-c",
            "agent-c",
            true,
            false,
            "reasoning to agentMessage rollover",
        ),
        (
            "agent-d",
            "reason-d",
            false,
            true,
            "agentMessage to reasoning rollover",
        ),
    ];
    for (first_id, second_id, first_is_reasoning, second_is_reasoning, context) in kind_pairs {
        fixture
            .client
            .send_message(&agent.instance_stream, format!("drive {context}"))
            .await
            .unwrap_or_else(|error| panic!("queue follow-up for {context}: {error:?}"));
        let mut turn = CodexLifecycleObservation::default();
        let stream = &agent.instance_stream;
        drive_codex_agent_to_idle(&mut fixture, stream, &mut turn, context).await;
        turn.assert_recovered_rollover(
            first_id,
            second_id,
            &format!("accepted {first_id}"),
            first_is_reasoning,
            context,
        );
        let continued = format!("continued {second_id}");
        // Live turns must use live framing. Pinned explicitly so the payload check
        // below cannot be satisfied by any other durable representation.
        assert_eq!(
            turn.stream_end_rows(second_id),
            1,
            "{context} replacement must emit exactly one live StreamEnd: {:?}",
            turn.ordered
        );
        assert_eq!(
            turn.assistant_rows(second_id),
            0,
            "{context} live turn must not be observed through settled history: {:?}",
            turn.ordered
        );
        if second_is_reasoning {
            assert_eq!(
                turn.stream_end_reasoning.get(second_id),
                Some(&Some(continued)),
                "{context} replacement must complete normally"
            );
        } else {
            assert_eq!(
                turn.stream_end_content.get(second_id),
                Some(&continued),
                "{context} replacement must complete normally"
            );
        }
    }

    // Recovery is not allowed to synthesize turns: one provider turn per message
    // the client actually sent, and no retry after any of them.
    assert_eq!(
        fake.turn_start_inputs(),
        vec![
            CODEX_LIFECYCLE_SPAWN_PROMPT.to_owned(),
            "drive agentMessage rollover".to_owned(),
            "drive reasoning to agentMessage rollover".to_owned(),
            "drive agentMessage to reasoning rollover".to_owned(),
        ],
        "each recovered turn must come from exactly one explicit client message"
    );
}

/// The second half of the incident: when a conflict is genuinely unrecoverable the
/// turn must end *visibly and finitely*, without waiting on the provider.
///
/// The fake deliberately never sends `turn/completed` for the terminated turn, so a
/// bound that depended on it would hang here.
///
/// Scoped to what only an integration test can show: that the preserved content and
/// the single terminal tail reach a real client through the agent layer, and that a
/// follow-up message is still delivered afterwards. Interrupt dispatch itself is
/// pinned inline by `strict_termination_interrupts_once_without_waiting`, so it is
/// deliberately not re-asserted here.
#[tokio::test]
async fn fake_codex_identity_termination_is_finite_and_visible_to_clients() {
    init_tracing();

    let fake = CodexLifecycleFake::new("termination");
    let _fake_guard = server::backend::codex::install_test_app_server_binary(fake.binary.clone());
    let workspace = tempfile::tempdir().expect("create Codex termination workspace");
    std::fs::write(
        workspace.path().join("README.txt"),
        "Codex termination test workspace",
    )
    .expect("seed Codex termination workspace");
    let mut fixture = Fixture::new_with_real_codex_backend_and_probe_program(
        fake.binary.to_string_lossy().into_owned(),
    )
    .await;
    let agent =
        spawn_fake_codex_lifecycle_agent(&mut fixture, workspace.path(), "Codex termination").await;

    let mut terminated = CodexLifecycleObservation::default();
    drive_codex_agent_to_idle(
        &mut fixture,
        &agent.instance_stream,
        &mut terminated,
        "fake Codex identity termination turn",
    )
    .await;

    let end_open = CodexLifecycleEvent::StreamEnd("open-item".to_owned());
    let accepted_end = terminated.position(&end_open);
    let error = terminated
        .ordered
        .iter()
        .position(|event| matches!(event, CodexLifecycleEvent::Error(_)))
        .unwrap_or_else(|| {
            panic!(
                "termination must be visible to the user: {:?}",
                terminated.ordered
            )
        });
    let cancelled = terminated.position(&CodexLifecycleEvent::Cancelled);
    let idle = terminated.position(&CodexLifecycleEvent::Typing(false));
    assert!(
        accepted_end < error && error < cancelled && cancelled < idle,
        "accepted content must be terminalized before the single terminal tail: {:?}",
        terminated.ordered
    );
    assert_eq!(
        terminated.stream_end_rows("open-item"),
        1,
        "termination must finalize the live item with exactly one live StreamEnd: {:?}",
        terminated.ordered
    );
    assert_eq!(
        terminated.stream_end_content.get("open-item"),
        Some(&"partial work".to_owned()),
        "termination must preserve the accepted bytes rather than dropping them"
    );
    assert_eq!(
        terminated.count(|event| matches!(event, CodexLifecycleEvent::Error(_))),
        1,
        "termination must report exactly one error: {:?}",
        terminated.ordered
    );
    assert_eq!(
        terminated.count(|event| matches!(event, CodexLifecycleEvent::Cancelled)),
        1,
        "termination must cancel exactly once: {:?}",
        terminated.ordered
    );
    assert!(
        terminated.warnings().is_empty(),
        "an unrecoverable conflict is not a recovery: {:?}",
        terminated.ordered
    );
    assert!(
        terminated
            .stream_end_content
            .keys()
            .all(|message_id| message_id != "foreign-item"),
        "the foreign item must never be attributed to a stream: {:?}",
        terminated.ordered
    );

    // No synthetic recovery: after termination the adapter must not start another
    // provider turn on its own. Checked before the explicit send, so an automatic
    // retry cannot hide inside the follow-up's turn.
    assert_eq!(
        fake.turn_start_inputs(),
        vec![CODEX_LIFECYCLE_SPAWN_PROMPT.to_owned()],
        "a terminated turn must not be retried or resumed automatically"
    );

    // Finiteness: the terminated turn never received turn/completed, yet the session
    // must still accept a new turn and render it in full. This also exercises the
    // message-delivery contract -- a follow-up after termination must be delivered
    // once, not rejected, duplicated, or silently dropped. The fake refuses its
    // success path for any other input, so the observed turn can only be this one.
    fixture
        .client
        .send_message(&agent.instance_stream, CODEX_LIFECYCLE_FOLLOW_UP.to_owned())
        .await
        .expect("queue follow-up after terminated Codex turn");
    let mut recovered = CodexLifecycleObservation::default();
    drive_codex_agent_to_idle(
        &mut fixture,
        &agent.instance_stream,
        &mut recovered,
        "fake Codex turn after termination",
    )
    .await;
    // Live framing pinned explicitly: the recovered turn must produce a real live
    // StreamEnd, not merely a payload reachable through some other representation.
    assert_eq!(
        recovered.stream_end_rows("recovered-item"),
        1,
        "the follow-up turn must emit exactly one live StreamEnd: {:?}",
        recovered.ordered
    );
    assert_eq!(
        recovered.assistant_rows("recovered-item"),
        0,
        "a live follow-up turn must not be observed through settled history: {:?}",
        recovered.ordered
    );
    assert_eq!(
        recovered.stream_end_content.get("recovered-item"),
        Some(&"next turn works".to_owned()),
        "a terminated turn must not poison the next one: {:?}",
        recovered.ordered
    );
    assert_eq!(
        recovered.count(|event| matches!(event, CodexLifecycleEvent::Error(_))),
        0,
        "the next turn must not inherit the terminated turn's error: {:?}",
        recovered.ordered
    );
    assert_eq!(
        recovered.count(|event| matches!(event, CodexLifecycleEvent::Cancelled)),
        0,
        "the next turn must not inherit the terminated turn's cancellation: {:?}",
        recovered.ordered
    );
    let unterminated = recovered.unterminated_streams();
    assert!(
        unterminated.is_empty(),
        "the recovering turn must leave no stream open, found {unterminated:?}"
    );
    assert!(
        recovered
            .stream_end_content
            .keys()
            .all(|message_id| message_id != "unexpected-turn-start"),
        "the follow-up turn must be the one the client asked for: {:?}",
        recovered.ordered
    );

    // Drain what the server has already queued so a duplicated tail or a late retry
    // emitted after the first idle is observed rather than missed. Bounded and
    // deterministic: it reads until the connection is quiet, it does not sleep on a
    // timer hoping something arrives.
    let mut post_idle = CodexLifecycleObservation::default();
    loop {
        match tokio::time::timeout(Duration::from_millis(250), fixture.client.next_event()).await {
            Err(_) => break,
            Ok(Ok(Some(env))) if env.kind == FrameKind::CommandError => {
                let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
                panic!("unexpected command error after terminated Codex turn: {error:?}");
            }
            Ok(Ok(Some(env)))
                if env.kind == FrameKind::ChatEvent && env.stream == agent.instance_stream =>
            {
                let event: ChatEvent = env.parse_payload().expect("parse post-idle ChatEvent");
                post_idle.observe(event);
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => panic!("connection closed during post-idle drain"),
            Ok(Err(error)) => panic!("post-idle drain failed: {error:?}"),
        }
    }
    assert_eq!(
        post_idle.count(|event| matches!(event, CodexLifecycleEvent::Error(_))),
        0,
        "no terminal tail may arrive after the turn settled: {:?}",
        post_idle.ordered
    );
    assert_eq!(
        post_idle.count(|event| matches!(event, CodexLifecycleEvent::Cancelled)),
        0,
        "no second cancellation may arrive after the turn settled: {:?}",
        post_idle.ordered
    );

    // Exactly two provider turns for exactly two client messages, in order, with the
    // explicit follow-up text delivered verbatim and no retry before or after it.
    assert_eq!(
        fake.turn_start_inputs(),
        vec![
            CODEX_LIFECYCLE_SPAWN_PROMPT.to_owned(),
            CODEX_LIFECYCLE_FOLLOW_UP.to_owned(),
        ],
        "the explicit follow-up must start exactly one new turn, with no retry"
    );
}

#[tokio::test]
async fn startup_mcp_servers_follow_debug_host_setting_for_new_agents() {
    init_tracing();

    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig::default()).await;

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::TydeDebugMcpEnabled { enabled: true },
        })
        .await
        .expect("set_setting failed");
    loop {
        let env =
            expect_fixture_event(&mut fixture.client, "host settings after set_setting").await;
        if env.kind == FrameKind::HostSettings {
            break;
        }
    }

    let final_text =
        spawn_mock_agent_and_collect_turn(&mut fixture.client, BackendKind::Claude, "hello").await;
    assert!(
        final_text.contains("tyde-debug(http)"),
        "expected mock backend turn to reflect injected tyde-debug HTTP startup MCP server, got: {final_text}"
    );
}

#[tokio::test]
async fn antigravity_empty_workspace_spawn_is_accepted() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Antigravity".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "hello antigravity".to_string(),
                images: None,
                backend_kind: BackendKind::Antigravity,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("send Antigravity spawn");

    let new_agent = loop {
        let env = expect_fixture_event(&mut fixture.client, "Antigravity NewAgent").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::NewAgent => {
                break env
                    .parse_payload::<NewAgentPayload>()
                    .expect("parse NewAgent");
            }
            FrameKind::HostSettings
            | FrameKind::SessionSchemas
            | FrameKind::LaunchProfileCatalogNotify
            | FrameKind::BackendSetup
            | FrameKind::BackendCapacity
            | FrameKind::TeamPresetCatalogNotify => continue,
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected CommandError");
                panic!("empty-root Antigravity spawn must not be rejected: {error:?}");
            }
            other => panic!("unexpected event while waiting for Antigravity NewAgent: {other}"),
        }
    };
    assert_eq!(new_agent.backend_kind, BackendKind::Antigravity);

    let start = expect_fixture_agent_start(
        &mut fixture.client,
        &new_agent.instance_stream,
        "Antigravity AgentStart",
    )
    .await;
    assert_eq!(start.backend_kind, BackendKind::Antigravity);
    assert!(
        start.workspace_roots.is_empty(),
        "empty-root spawn must keep protocol workspace_roots empty"
    );
}

#[tokio::test]
async fn empty_workspace_spawn_is_accepted_for_all_backends() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let host = fixture.host_for_test();
    host.set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    host.set_session_schema_ready_for_test(BackendKind::Acp)
        .await;
    host.set_session_schema_ready_for_test(BackendKind::Hermes)
        .await;
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Acp,
        BackendKind::Tycode,
        BackendKind::Antigravity,
        BackendKind::Hermes,
    ];
    let mut session_ids = Vec::new();
    for backend_kind in backends {
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                name: Some(format!("{backend_kind:?} empty root")),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots: Vec::new(),
                    prompt: format!("hello {backend_kind:?}"),
                    launch_profile_id: None,
                    images: None,
                    backend_kind,
                    cost_hint: None,
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await
            .unwrap_or_else(|err| panic!("send {backend_kind:?} empty-root spawn: {err:?}"));

        let new_agent = loop {
            let env = expect_fixture_event(&mut fixture.client, "empty-root NewAgent").await;
            if fixture::is_builtin_team_custom_agent_notify(&env) {
                continue;
            }
            match env.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        env.parse_payload().expect("parse empty-root NewAgent");
                    if payload.backend_kind == backend_kind {
                        break payload;
                    }
                }
                FrameKind::CommandError => {
                    let error = env
                        .parse_payload::<CommandErrorPayload>()
                        .expect("parse unexpected empty-root CommandError");
                    panic!("{backend_kind:?} empty-root spawn must not be rejected: {error:?}");
                }
                _ => {}
            }
        };

        let (start, bootstrap_chat_events) = expect_fixture_agent_start_with_chat_events(
            &mut fixture.client,
            &new_agent.instance_stream,
            "empty-root AgentStart",
        )
        .await;
        assert_eq!(start.backend_kind, backend_kind);
        assert!(
            start.workspace_roots.is_empty(),
            "{backend_kind:?} empty-root spawn must keep AgentStart workspace_roots empty"
        );
        let session_id = start
            .session_id
            .clone()
            .unwrap_or_else(|| panic!("{backend_kind:?} empty-root AgentStart missing session_id"));
        session_ids.push((backend_kind, session_id));

        expect_fixture_initial_turn_completion(
            &mut fixture.client,
            &new_agent.instance_stream,
            bootstrap_chat_events,
            "empty-root ChatEvent",
        )
        .await;
    }

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions after all empty-root spawns");
    let session_list = loop {
        let env = expect_fixture_event(&mut fixture.client, "empty-root SessionList").await;
        if env.kind == FrameKind::SessionList {
            break env
                .parse_payload::<SessionListPayload>()
                .expect("parse empty-root SessionList");
        }
    };
    for (backend_kind, session_id) in session_ids {
        let session = session_list
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .unwrap_or_else(|| panic!("missing {backend_kind:?} empty-root session"));
        assert_eq!(session.backend_kind, backend_kind);
        assert!(
            session.workspace_roots.is_empty(),
            "{backend_kind:?} empty-root session summary must keep workspace_roots empty"
        );
    }
}

#[tokio::test]
async fn tycode_session_schema_exposes_default_agent() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Tycode],
            },
        })
        .await
        .expect("enable Tycode backend");

    let schemas = loop {
        let env = expect_fixture_event(&mut fixture.client, "Tycode SessionSchemas").await;
        if !matches!(
            env.kind,
            FrameKind::SessionSchemas | FrameKind::LaunchProfileCatalogNotify
        ) {
            continue;
        }
        if env.kind == FrameKind::LaunchProfileCatalogNotify {
            continue;
        }
        let payload: SessionSchemasPayload =
            env.parse_payload().expect("parse Tycode SessionSchemas");
        if payload
            .schemas
            .iter()
            .any(|schema| schema.backend_kind() == BackendKind::Tycode)
        {
            break payload;
        }
    };

    let tycode_schema = schemas
        .schemas
        .into_iter()
        .find(|schema| schema.backend_kind() == BackendKind::Tycode)
        .expect("Tycode schema should be present");
    let SessionSchemaEntry::Ready { schema } = tycode_schema else {
        panic!("expected Tycode schema to be ready");
    };
    let field = schema
        .fields
        .iter()
        .find(|field| field.key == "default_agent")
        .unwrap_or_else(|| panic!("Tycode SetRootAgent control should be exposed: {schema:?}"));
    assert!(field.use_slider);
    let protocol::SessionSettingFieldType::Select {
        options,
        default,
        nullable,
    } = &field.field_type
    else {
        panic!("default_agent should be a select field: {field:?}");
    };
    assert_eq!(default.as_deref(), Some("tycode"));
    assert!(!nullable);
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["one_shot", "tycode", "builder", "swarm"]
    );
}

#[tokio::test]
async fn tycode_explicit_invalid_default_agent_spawn_is_rejected_by_schema() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let mut session_settings = SessionSettingsValues::default();
    session_settings.0.insert(
        "default_agent".to_string(),
        SessionSettingValue::String("legacy_swarm".to_string()),
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Tycode invalid explicit root".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "must not start".to_string(),
                images: None,
                backend_kind: BackendKind::Tycode,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: Some(session_settings),
            },
        })
        .await
        .expect("send Tycode spawn with invalid explicit default_agent");

    let new_agent = loop {
        let env =
            expect_fixture_event(&mut fixture.client, "Tycode invalid explicit NewAgent").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse Tycode invalid explicit NewAgent");
                if payload.backend_kind == BackendKind::Tycode {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected Tycode invalid explicit CommandError");
                panic!(
                    "Tycode invalid explicit setting should become agent startup failure, not CommandError: {error:?}"
                );
            }
            _ => {}
        }
    };

    let bootstrap: AgentBootstrapPayload = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Tycode invalid explicit AgentBootstrap",
        )
        .await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == new_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse Tycode invalid explicit AgentBootstrap");
        }
    };
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("Tycode invalid explicit bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::Internal);
    assert!(error.fatal);
    assert!(
        error.message.contains("invalid supplied session settings"),
        "unexpected Tycode invalid explicit error: {error:?}"
    );
    assert!(
        error
            .message
            .contains("invalid session setting 'default_agent' value 'legacy_swarm'"),
        "unexpected Tycode invalid explicit error: {error:?}"
    );
}

#[tokio::test]
async fn tycode_stale_stored_invalid_default_agent_resume_is_rejected() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let session_id = SessionId("stale-tycode-invalid-default-agent".to_string());
    let session_store =
        server::store::session::SessionStore::load(fixture.store_dir().join("sessions.json"))
            .expect("load fixture session store");
    session_store
        .upsert_backend_session(
            &BackendSession {
                id: session_id.clone(),
                backend_kind: BackendKind::Tycode,
                workspace_roots: Vec::new(),
                title: Some("Stale Tycode invalid root".to_string()),
                token_count: None,
                created_at_ms: Some(1),
                updated_at_ms: Some(2),
                resumable: true,
            },
            None,
            None,
            None,
            None,
        )
        .expect("insert stale Tycode session");
    let mut stored_settings = SessionSettingsValues::default();
    stored_settings.0.insert(
        "default_agent".to_string(),
        SessionSettingValue::String("legacy_swarm".to_string()),
    );
    session_store
        .set_session_settings(&session_id, stored_settings)
        .expect("store stale invalid Tycode default_agent");

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Resume stale Tycode invalid root".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("send Tycode stale invalid resume");

    let resumed_agent = loop {
        let env =
            expect_fixture_event(&mut fixture.client, "Tycode stale invalid resume NewAgent").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = env
                .parse_payload()
                .expect("parse Tycode stale invalid resume NewAgent");
            if payload.session_id.as_ref() == Some(&session_id) {
                break payload;
            }
        }
    };

    let bootstrap: AgentBootstrapPayload = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Tycode stale invalid resume AgentBootstrap",
        )
        .await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == resumed_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse Tycode stale invalid resume AgentBootstrap");
        }
    };
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("Tycode stale invalid resume bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::Internal);
    assert!(error.fatal);
    assert!(
        error.message.contains("invalid stored session settings"),
        "unexpected Tycode stale invalid resume error: {error:?}"
    );
    assert!(
        error
            .message
            .contains("invalid session setting 'default_agent' value 'legacy_swarm'"),
        "unexpected Tycode stale invalid resume error: {error:?}"
    );
}

#[tokio::test]
async fn tycode_live_default_agent_update_is_rejected() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Tycode runtime settings".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "hello tycode".to_string(),
                images: None,
                backend_kind: BackendKind::Tycode,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn Tycode mock agent");

    let new_agent = loop {
        let env = expect_fixture_event(&mut fixture.client, "Tycode NewAgent").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = env.parse_payload().expect("parse Tycode NewAgent");
            if payload.backend_kind == BackendKind::Tycode {
                break payload;
            }
        }
    };
    expect_fixture_agent_start(
        &mut fixture.client,
        &new_agent.instance_stream,
        "Tycode AgentStart",
    )
    .await;

    loop {
        let env = expect_fixture_event(&mut fixture.client, "Tycode initial StreamEnd").await;
        if env.kind != FrameKind::ChatEvent || env.stream != new_agent.instance_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse Tycode initial ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(_)) {
            break;
        }
    }

    let mut values = SessionSettingsValues::default();
    values.0.insert(
        "default_agent".to_string(),
        SessionSettingValue::String("swarm".to_string()),
    );
    fixture
        .client
        .set_session_settings(
            &new_agent.instance_stream,
            SetSessionSettingsPayload { values },
        )
        .await
        .expect("send Tycode SetSessionSettings");

    let error = loop {
        let env = expect_fixture_event(&mut fixture.client, "Tycode live settings rejection").await;
        if env.stream != new_agent.instance_stream {
            continue;
        }
        match env.kind {
            FrameKind::AgentError => {
                break env
                    .parse_payload::<protocol::AgentErrorPayload>()
                    .expect("parse Tycode live settings AgentError");
            }
            FrameKind::SessionSettings => {
                panic!("rejected Tycode default_agent update must not emit SessionSettings")
            }
            _ => {}
        }
    };
    assert_eq!(error.code, AgentErrorCode::Internal);
    assert!(!error.fatal);
    assert!(
        error
            .message
            .contains("cannot be changed on a running session"),
        "unexpected Tycode live settings rejection: {error:?}"
    );
}

#[tokio::test]
async fn antigravity_native_uuid_session_remains_resumable_after_close() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Antigravity".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace.path().to_string_lossy().to_string()],
                prompt: "hello antigravity".to_string(),
                images: None,
                backend_kind: BackendKind::Antigravity,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn Antigravity mock agent");

    let env = expect_fixture_event(&mut fixture.client, "Antigravity NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse Antigravity NewAgent");
    assert_eq!(new_agent.backend_kind, BackendKind::Antigravity);

    let (start, bootstrap_chat_events) = expect_fixture_agent_start_with_chat_events(
        &mut fixture.client,
        &new_agent.instance_stream,
        "Antigravity AgentStart",
    )
    .await;
    assert_eq!(start.backend_kind, BackendKind::Antigravity);
    let session_id = start
        .session_id
        .clone()
        .expect("Antigravity AgentStart session_id");

    expect_fixture_initial_turn_completion(
        &mut fixture.client,
        &new_agent.instance_stream,
        bootstrap_chat_events,
        "Antigravity ChatEvent",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions after Antigravity spawn");
    let session_list = loop {
        let env = expect_fixture_event(&mut fixture.client, "Antigravity SessionList").await;
        if env.kind == FrameKind::SessionList {
            break env
                .parse_payload::<SessionListPayload>()
                .expect("parse Antigravity SessionList");
        }
    };
    let session = session_list
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("persisted Antigravity session");
    assert_eq!(session.backend_kind, BackendKind::Antigravity);
    assert!(
        !session.resumable,
        "Antigravity native UUID sessions without a backing AGY db must not be resumable"
    );

    set_stored_session_resumable(fixture.store_dir(), &session_id, false);
    let _db_guard = AntigravityConversationDbGuard::create(
        fixture.antigravity_conversations_dir(),
        &session_id,
    );
    let mut session = None;
    for _ in 0..3 {
        fixture
            .client
            .list_sessions(ListSessionsPayload::default())
            .await
            .expect("list sessions after creating fake Antigravity db");
        let session_list = loop {
            let env = expect_fixture_event(
                &mut fixture.client,
                "Antigravity SessionList with fake native db",
            )
            .await;
            if env.kind == FrameKind::SessionList {
                break env
                    .parse_payload::<SessionListPayload>()
                    .expect("parse Antigravity SessionList with fake native db");
            }
        };
        let current = session_list
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
            .expect("persisted Antigravity session with fake native db");
        if current.resumable {
            session = Some(current);
            break;
        }
        session = Some(current);
    }
    let session = session.expect("persisted Antigravity session with fake native db");
    assert!(
        session.resumable,
        "Antigravity native UUID sessions with a backing AGY db should be resumable"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Antigravity resumed from stale false".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("resume antigravity".to_string()),
            },
        })
        .await
        .expect("resume Antigravity mock agent after fake native db");
    let resumed_agent = loop {
        let env = expect_fixture_event(&mut fixture.client, "resumed Antigravity NewAgent").await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse resumed Antigravity NewAgent");
                if payload.backend_kind == BackendKind::Antigravity {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected resume CommandError");
                panic!(
                    "DB-backed Antigravity session with stale stored false must resume: {error:?}"
                );
            }
            _ => {}
        }
    };
    let resumed_start = expect_fixture_agent_start(
        &mut fixture.client,
        &resumed_agent.instance_stream,
        "resumed Antigravity AgentStart",
    )
    .await;
    assert_eq!(
        resumed_start.session_id.as_ref(),
        Some(&session_id),
        "Antigravity resume must reopen the native session id even if the stored raw resumable flag was stale false"
    );

    fixture
        .client
        .close_agent(&new_agent.instance_stream)
        .await
        .expect("close Antigravity agent");
    loop {
        let env = expect_fixture_event(&mut fixture.client, "Antigravity AgentClosed").await;
        if env.kind == FrameKind::AgentClosed {
            break;
        }
    }
    let session_list = loop {
        let env =
            expect_fixture_event(&mut fixture.client, "Antigravity SessionList after close").await;
        if env.kind == FrameKind::SessionList {
            break env
                .parse_payload::<SessionListPayload>()
                .expect("parse Antigravity SessionList after close");
        }
    };
    let session = session_list
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("persisted Antigravity session after close");
    assert!(
        session.resumable,
        "Antigravity native UUID sessions with a backing AGY db should remain resumable after close"
    );

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let restarted_session = fresh_bootstrap
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("persisted Antigravity session after fresh host restart");
    assert!(
        restarted_session.resumable,
        "fresh fixture hosts must reuse the isolated Antigravity conversations directory"
    );
}

#[tokio::test]
async fn antigravity_direct_resume_missing_native_db_reports_startup_failure() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Antigravity".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace.path().to_string_lossy().to_string()],
                prompt: "hello antigravity".to_string(),
                images: None,
                backend_kind: BackendKind::Antigravity,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn Antigravity mock agent");

    let env = expect_fixture_event(&mut fixture.client, "Antigravity NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse Antigravity NewAgent");
    let (start, bootstrap_chat_events) = expect_fixture_agent_start_with_chat_events(
        &mut fixture.client,
        &new_agent.instance_stream,
        "Antigravity AgentStart",
    )
    .await;
    let session_id = start
        .session_id
        .clone()
        .expect("Antigravity AgentStart session_id");

    expect_fixture_initial_turn_completion(
        &mut fixture.client,
        &new_agent.instance_stream,
        bootstrap_chat_events,
        "Antigravity ChatEvent",
    )
    .await;

    let db_guard = AntigravityConversationDbGuard::create(
        fixture.antigravity_conversations_dir(),
        &session_id,
    );
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions with fake Antigravity db");
    let session = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Antigravity SessionList with fake native db",
        )
        .await;
        if env.kind == FrameKind::SessionList {
            let session_list = env
                .parse_payload::<SessionListPayload>()
                .expect("parse Antigravity SessionList with fake native db");
            if let Some(session) = session_list
                .sessions
                .into_iter()
                .find(|session| session.id == session_id && session.resumable)
            {
                break session;
            }
        }
    };
    assert!(
        session.resumable,
        "test setup must first observe the Antigravity session as resumable"
    );
    drop(db_guard);

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Antigravity resume after db removal".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("resume after db removal".to_string()),
            },
        })
        .await
        .expect("send Antigravity resume after native db removal");

    let resumed_agent = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Antigravity resume-missing-db NewAgent",
        )
        .await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse Antigravity resume-missing-db NewAgent");
                if payload.backend_kind == BackendKind::Antigravity {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected resume CommandError");
                panic!(
                    "direct resume should become agent startup failure, not CommandError: {error:?}"
                );
            }
            _ => {}
        }
    };
    let bootstrap: AgentBootstrapPayload = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Antigravity resume-missing-db AgentBootstrap",
        )
        .await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == resumed_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse Antigravity resume-missing-db AgentBootstrap");
        }
    };
    let resumed_start = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentStart(start) => Some(start),
            _ => None,
        })
        .expect("Antigravity resume-missing-db bootstrap must include AgentStart");
    assert_eq!(resumed_start.session_id.as_ref(), Some(&session_id));
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("Antigravity resume-missing-db bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::Unsupported);
    assert!(error.fatal);
    assert!(
        error
            .message
            .contains("cannot resume non-resumable session"),
        "unexpected resume-missing-db error: {error:?}"
    );

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("connection should remain usable after Antigravity resume-missing-db failure");
    loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "SessionList after Antigravity resume-missing-db failure",
        )
        .await;
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
}

#[tokio::test]
async fn antigravity_direct_resume_non_resumable_without_alias_reports_startup_failure() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let session_id = SessionId(Uuid::new_v4().to_string());
    write_antigravity_session_record_without_alias(fixture.store_dir(), &session_id);

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: Some(CustomAgentId("mismatched-custom-agent".to_string())),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("resume non-resumable".to_string()),
            },
        })
        .await
        .expect("send Antigravity non-resumable resume");

    let resumed_agent = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Antigravity non-resumable no-alias NewAgent",
        )
        .await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse Antigravity non-resumable no-alias NewAgent");
                if payload.session_id.as_ref() == Some(&session_id) {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected non-resumable CommandError");
                panic!(
                    "non-resumable direct resume should become agent startup failure, not CommandError: {error:?}"
                );
            }
            _ => {}
        }
    };
    assert_eq!(resumed_agent.name, format!("Session {session_id}"));

    let bootstrap: AgentBootstrapPayload = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Antigravity non-resumable no-alias AgentBootstrap",
        )
        .await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == resumed_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse Antigravity non-resumable no-alias AgentBootstrap");
        }
    };
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("Antigravity non-resumable no-alias bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::Unsupported);
    assert!(error.fatal);
    assert!(
        error
            .message
            .contains("cannot resume non-resumable session"),
        "unexpected non-resumable no-alias error: {error:?}"
    );

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("connection should remain usable after non-resumable no-alias failure");
    loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "SessionList after non-resumable no-alias failure",
        )
        .await;
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
}

#[tokio::test]
async fn antigravity_direct_resume_missing_record_reports_startup_failure() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let session_id = SessionId(Uuid::new_v4().to_string());

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: Some("resume missing".to_string()),
            },
        })
        .await
        .expect("send missing-record resume");

    let resumed_agent = loop {
        let env = expect_fixture_event(&mut fixture.client, "missing-record resume NewAgent").await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let payload: NewAgentPayload = env
            .parse_payload()
            .expect("parse missing-record resume NewAgent");
        if payload.session_id.as_ref() == Some(&session_id) {
            break payload;
        }
    };
    assert_eq!(resumed_agent.name, format!("Session {session_id}"));

    let bootstrap: AgentBootstrapPayload = loop {
        let env =
            expect_fixture_event(&mut fixture.client, "missing-record resume AgentBootstrap").await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == resumed_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse missing-record resume AgentBootstrap");
        }
    };
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("missing-record resume bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::Unsupported);
    assert!(error.fatal);
    assert!(
        error.message.contains("cannot resume missing session"),
        "unexpected missing-record resume error: {error:?}"
    );

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("connection should remain usable after missing-record resume failure");
    loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "SessionList after missing-record resume failure",
        )
        .await;
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
}

#[tokio::test]
async fn kiro_dynamic_schema_discovery_uses_probe_models() {
    init_tracing();

    let probe_dir = tempfile::tempdir().expect("create Kiro probe tempdir");
    let probe_workspace_dir = tempfile::tempdir().expect("create Kiro probe workspace tempdir");
    let probe_program = write_fake_kiro_probe_program(&probe_dir);
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        kiro_probe_program: Some(probe_program.to_string_lossy().to_string()),
        kiro_probe_workspace_root: Some(probe_workspace_dir.path().to_path_buf()),
        ..server::HostRuntimeConfig::default()
    })
    .await;

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Acp],
            },
        })
        .await
        .expect("enable Kiro backend");

    let kiro_schema = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let env = fixture
                .client
                .next_event()
                .await
                .expect("read event while waiting for Kiro SessionSchemas")
                .expect("connection closed while waiting for Kiro SessionSchemas");
            if env.kind != FrameKind::SessionSchemas {
                continue;
            }
            let payload: SessionSchemasPayload =
                env.parse_payload().expect("parse Kiro SessionSchemas");
            let Some(kiro_schema) = payload
                .schemas
                .into_iter()
                .find(|schema| schema.backend_kind() == BackendKind::Acp)
            else {
                continue;
            };
            if !matches!(kiro_schema, SessionSchemaEntry::Pending { .. }) {
                break kiro_schema;
            }
        }
    })
    .await
    .expect("timed out waiting for Kiro schema probe result");

    let kiro_schema = match kiro_schema {
        SessionSchemaEntry::Ready { schema } => schema,
        SessionSchemaEntry::Unavailable { message, .. } => {
            assert!(
                message.contains("Kiro schema probe stage '"),
                "Kiro schema probe failure should identify its stage: {message}"
            );
            panic!("expected Kiro schema to be ready; probe became unavailable: {message}");
        }
        SessionSchemaEntry::Pending { .. } => {
            panic!("expected Kiro schema to be ready; probe remained pending")
        }
    };
    assert_eq!(kiro_schema.fields.len(), 1);
    assert_eq!(kiro_schema.fields[0].key, "model");

    match &kiro_schema.fields[0].field_type {
        SessionSettingFieldType::Select {
            options,
            default,
            nullable,
        } => {
            assert_eq!(
                options,
                &vec![
                    protocol::SelectOption {
                        value: "kiro-sonnet".to_string(),
                        label: "Kiro Sonnet".to_string(),
                    },
                    protocol::SelectOption {
                        value: "kiro-haiku".to_string(),
                        label: "Kiro Haiku".to_string(),
                    },
                ]
            );
            assert_eq!(default.as_deref(), Some("kiro-sonnet"));
            assert!(*nullable);
        }
        other => panic!("expected Kiro model field to be a Select, got {other:?}"),
    }
    let probe_cwd = std::fs::read_to_string(probe_dir.path().join("probe-cwd"))
        .expect("read fake Kiro probe cwd");
    let expected_probe_cwd =
        std::fs::canonicalize(probe_workspace_dir.path().join(".tyde/kiro-admin"))
            .expect("canonicalize isolated Kiro admin cwd");
    assert_eq!(
        PathBuf::from(probe_cwd.trim()),
        expected_probe_cwd,
        "fake Kiro probe should run from the isolated admin cwd"
    );

    let mut session_settings = SessionSettingsValues::default();
    session_settings.0.insert(
        "model".to_string(),
        SessionSettingValue::String("kiro-haiku".to_string()),
    );
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Kiro".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_string()],
                prompt: "hello".to_string(),
                images: None,
                backend_kind: BackendKind::Acp,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: Some(session_settings),
            },
        })
        .await
        .expect("spawn Kiro agent with discovered model");

    let new_agent = loop {
        let env = expect_fixture_event(&mut fixture.client, "Kiro NewAgent").await;
        if env.kind == FrameKind::NewAgent {
            break env
                .parse_payload::<NewAgentPayload>()
                .expect("parse Kiro NewAgent");
        }
    };
    let agent_stream = new_agent.instance_stream.clone();

    let (_, bootstrap_chat_events) = expect_fixture_agent_start_with_chat_events(
        &mut fixture.client,
        &agent_stream,
        "Kiro AgentStart",
    )
    .await;
    expect_fixture_initial_turn_completion(
        &mut fixture.client,
        &agent_stream,
        bootstrap_chat_events,
        "Kiro StreamEnd",
    )
    .await;
}

#[tokio::test]
async fn hermes_unavailable_dynamic_schema_with_supplied_settings_is_agent_error() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    let mut session_settings = SessionSettingsValues::default();
    session_settings.0.insert(
        "model".to_string(),
        SessionSettingValue::String("anthropic/claude-haiku-4.5 --provider openrouter".to_string()),
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Hermes unavailable schema".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "hello".to_string(),
                images: None,
                backend_kind: BackendKind::Hermes,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: Some(session_settings),
            },
        })
        .await
        .expect("send Hermes spawn with unavailable schema");

    let new_agent = loop {
        let env =
            expect_fixture_event(&mut fixture.client, "Hermes unavailable schema NewAgent").await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse Hermes unavailable schema NewAgent");
                if payload.backend_kind == BackendKind::Hermes {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected Hermes unavailable schema CommandError");
                panic!(
                    "Hermes unavailable schema should become agent startup failure, not CommandError: {error:?}"
                );
            }
            _ => {}
        }
    };

    let bootstrap: AgentBootstrapPayload = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Hermes unavailable schema AgentBootstrap",
        )
        .await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == new_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse Hermes unavailable schema AgentBootstrap");
        }
    };
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("Hermes unavailable schema bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::BackendFailed);
    assert!(error.fatal);
    assert!(
        error
            .message
            .contains("session settings schema unavailable"),
        "unexpected Hermes unavailable schema error: {error:?}"
    );

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("connection should remain usable after Hermes schema failure");
    loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "SessionList after Hermes unavailable schema failure",
        )
        .await;
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
}

#[tokio::test]
async fn hermes_unavailable_dynamic_schema_rejects_tier_configuration() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::ComplexityTiersEnabled { enabled: true },
        })
        .await
        .expect("enable complexity tiers");
    loop {
        let env = expect_fixture_event(&mut fixture.client, "complexity tiers HostSettings").await;
        if env.kind == FrameKind::HostSettings {
            break;
        }
    }

    let mut low = SessionSettingsValues::default();
    low.0.insert(
        "model".to_string(),
        SessionSettingValue::String("anthropic/claude-haiku-4.5 --provider openrouter".to_string()),
    );
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendTiers {
                backend: BackendKind::Hermes,
                config: protocol::BackendTierConfig {
                    low,
                    high: SessionSettingsValues::default(),
                },
            },
        })
        .await
        .expect("set Hermes tier config");
    let error = loop {
        let env = expect_fixture_event(&mut fixture.client, "Hermes tier CommandError").await;
        if env.kind == FrameKind::CommandError {
            break env
                .parse_payload::<CommandErrorPayload>()
                .expect("parse Hermes tier CommandError");
        }
    };
    assert_eq!(error.code, protocol::CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error
            .message
            .contains("session settings schema unavailable"),
        "unexpected Hermes tier configuration error: {error:?}"
    );
}

#[tokio::test]
async fn claude_unknown_system_frame_is_tolerated() {
    server::backend::claude::validate_system_frame(&serde_json::json!({
        "type": "system",
        "subtype": "task_started",
        "task_type": "local_agent",
        "task_id": "task-123",
    }))
    .expect("unknown Claude system subtypes should not crash parsing");
}

#[tokio::test]
async fn claude_system_frame_without_subtype_still_fails_loudly() {
    let err = server::backend::claude::validate_system_frame(&serde_json::json!({
        "type": "system",
    }))
    .expect_err("Claude system frame without subtype should be rejected");
    assert!(
        err.contains("invalid Claude system frame"),
        "expected loud Claude system-frame error, got: {err}",
    );
}

#[tokio::test]
async fn compact_turn_emits_typed_marker_and_stream_end_without_legacy_notice() {
    init_tracing();

    let mut fixture = Fixture::new().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Compact".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_string()],
                prompt: "/compact".to_string(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn compact test agent");

    let new_agent = loop {
        let env = expect_fixture_event(&mut fixture.client, "compact NewAgent").await;
        if env.kind == FrameKind::NewAgent {
            break env
                .parse_payload::<NewAgentPayload>()
                .expect("parse compact NewAgent");
        }
    };
    let agent_stream = new_agent.instance_stream.clone();

    expect_fixture_agent_start(&mut fixture.client, &agent_stream, "compact AgentStart").await;

    let mut saw_marker = false;
    let mut saw_legacy_notice = false;
    let mut saw_stream_end = false;
    let mut saw_typing_false = false;

    while !saw_typing_false {
        let env = expect_fixture_event(&mut fixture.client, "compact ChatEvent").await;
        if env.stream != agent_stream {
            continue;
        }
        assert_ne!(
            env.kind,
            FrameKind::AgentError,
            "compact turn should not emit AgentError"
        );
        if env.kind != FrameKind::ChatEvent {
            continue;
        }

        let event: ChatEvent = env.parse_payload().expect("parse compact ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if message.content == "Conversation compacted." {
                    saw_legacy_notice = true;
                }
            }
            ChatEvent::ContextCompaction(marker) => {
                assert_eq!(marker.trigger, protocol::CompactionTrigger::UserTyped);
                assert_eq!(marker.method, protocol::CompactionMethod::NativeTextCommand);
                saw_marker = true;
            }
            ChatEvent::StreamEnd(data) => {
                assert!(
                    data.message.content.is_empty(),
                    "compact turn should not fabricate assistant text"
                );
                saw_stream_end = true;
            }
            ChatEvent::TypingStatusChanged(false) => {
                saw_typing_false = true;
            }
            _ => {}
        }
    }

    assert!(saw_marker, "compact turn should emit one typed marker");
    assert!(
        !saw_legacy_notice,
        "compact turn must not emit the superseded system-message notice"
    );
    assert!(saw_stream_end, "compact turn should emit StreamEnd");
}

/// Fixture that uses real backends (not mock) so backend_kind dispatch is tested.
struct AntigravityNativeFileSnapshot {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    permissions: Option<std::fs::Permissions>,
    hash: Option<u64>,
}

impl AntigravityNativeFileSnapshot {
    fn capture(path: PathBuf) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self {
                path,
                bytes: None,
                permissions: None,
                hash: None,
            });
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("failed to snapshot {}: {error}", path.display()))?;
        let permissions = std::fs::metadata(&path)
            .map_err(|error| format!("failed to stat {}: {error}", path.display()))?
            .permissions();
        let hash = antigravity_native_bytes_hash(&bytes);
        Ok(Self {
            path,
            bytes: Some(bytes),
            permissions: Some(permissions),
            hash: Some(hash),
        })
    }

    fn restore(&self) -> Result<(), String> {
        match &self.bytes {
            Some(bytes) => {
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!(
                            "failed to recreate native parent {}: {error}",
                            parent.display()
                        )
                    })?;
                }
                std::fs::write(&self.path, bytes).map_err(|error| {
                    format!(
                        "failed to restore native file {}: {error}",
                        self.path.display()
                    )
                })?;
                if let Some(permissions) = &self.permissions {
                    std::fs::set_permissions(&self.path, permissions.clone()).map_err(|error| {
                        format!(
                            "failed to restore native mode {}: {error}",
                            self.path.display()
                        )
                    })?;
                }
            }
            None if self.path.exists() => {
                std::fs::remove_file(&self.path).map_err(|error| {
                    format!(
                        "failed to remove test-created native file {}: {error}",
                        self.path.display()
                    )
                })?;
            }
            None => {}
        }
        Ok(())
    }

    fn verify(&self) -> Result<(), String> {
        match (&self.bytes, self.hash) {
            (Some(expected), Some(expected_hash)) => {
                let actual = std::fs::read(&self.path).map_err(|error| {
                    format!(
                        "failed to verify native file {}: {error}",
                        self.path.display()
                    )
                })?;
                if actual != *expected || antigravity_native_bytes_hash(&actual) != expected_hash {
                    return Err(format!(
                        "native file did not return to its baseline: {}",
                        self.path.display()
                    ));
                }
                let readonly = std::fs::metadata(&self.path)
                    .map_err(|error| {
                        format!(
                            "failed to verify native mode {}: {error}",
                            self.path.display()
                        )
                    })?
                    .permissions()
                    .readonly();
                if self
                    .permissions
                    .as_ref()
                    .is_some_and(|permissions| permissions.readonly() != readonly)
                {
                    return Err(format!(
                        "native file mode did not return to its baseline: {}",
                        self.path.display()
                    ));
                }
            }
            (None, None) if self.path.exists() => {
                return Err(format!(
                    "test-created shared native file remains: {}",
                    self.path.display()
                ));
            }
            (None, None) => {}
            _ => {
                return Err(format!(
                    "invalid native snapshot state for {}",
                    self.path.display()
                ));
            }
        }
        Ok(())
    }
}

struct AntigravityNativeDirectorySnapshot {
    path: PathBuf,
    entries: HashSet<PathBuf>,
}

impl AntigravityNativeDirectorySnapshot {
    fn capture(path: PathBuf) -> Result<Self, String> {
        let entries = antigravity_native_directory_entries(&path)?;
        Ok(Self { path, entries })
    }

    fn verify(&self) -> Result<(), String> {
        let current = antigravity_native_directory_entries(&self.path)?;
        if current == self.entries {
            return Ok(());
        }
        let added = current
            .difference(&self.entries)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let removed = self
            .entries
            .difference(&current)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        Err(format!(
            "native directory did not return to baseline {}: added={added:?} removed={removed:?}",
            self.path.display()
        ))
    }
}

/// Protects the user's real Antigravity home during the opt-in paid regression.
///
/// `RealBackendFixture` isolates Tyde stores and roots, but `agy` still owns
/// native conversations and shared indexes under `~/.gemini`.
struct AntigravityNativeArtifactGuard {
    home: PathBuf,
    native_root: PathBuf,
    test_token: String,
    shared_files: Vec<AntigravityNativeFileSnapshot>,
    directories: Vec<AntigravityNativeDirectorySnapshot>,
    session_ids: HashSet<SessionId>,
    owned_paths: HashSet<PathBuf>,
    finalized: bool,
}

impl AntigravityNativeArtifactGuard {
    fn capture(test_token: String) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is unavailable for Antigravity native cleanup".to_string())?;
        let native_root = home.join(".gemini").join("antigravity-cli");
        let cache = native_root.join("cache");
        let summary = native_root.join("conversation_summaries.db");
        let shared_paths = [
            cache.join("projects.json"),
            cache.join("default_project_id.txt"),
            cache.join("last_conversations.json"),
            cache.join("conversation_metadata.json"),
            native_root.join("history.jsonl"),
            summary.clone(),
            PathBuf::from(format!("{}-wal", summary.display())),
            PathBuf::from(format!("{}-shm", summary.display())),
        ];
        let shared_files = shared_paths
            .into_iter()
            .map(AntigravityNativeFileSnapshot::capture)
            .collect::<Result<Vec<_>, _>>()?;
        let directories = [
            native_root.join("conversations"),
            native_root.join("brain"),
            native_root.join("implicit"),
            home.join(".tyde").join("antigravity").join("logs"),
        ]
        .into_iter()
        .map(AntigravityNativeDirectorySnapshot::capture)
        .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            home,
            native_root,
            test_token,
            shared_files,
            directories,
            session_ids: HashSet::new(),
            owned_paths: HashSet::new(),
            finalized: false,
        })
    }

    fn scratch_dir(&self) -> PathBuf {
        self.native_root.join("scratch")
    }

    fn no_root_dir(&self) -> PathBuf {
        self.home.join(".tyde").join("antigravity").join("no-root")
    }

    fn register_session(&mut self, session_id: SessionId) {
        assert!(
            Uuid::parse_str(&session_id.0).is_ok(),
            "Antigravity native session must be an exact UUID: {session_id}"
        );
        self.session_ids.insert(session_id);
    }

    fn track_owned_path(&mut self, path: PathBuf) {
        self.owned_paths.insert(path);
    }

    fn register_test_conversations_from_native_diff(&mut self) -> Result<(), String> {
        let conversations = self
            .directories
            .iter()
            .find(|snapshot| snapshot.path.ends_with("antigravity-cli/conversations"))
            .ok_or_else(|| "Antigravity conversation baseline is unavailable".to_string())?;
        let mut discovered = Vec::new();
        for path in antigravity_native_directory_entries(&conversations.path)?
            .difference(&conversations.entries)
        {
            if path.extension().and_then(|extension| extension.to_str()) != Some("db") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if Uuid::parse_str(stem).is_err() {
                continue;
            }
            let bytes = std::fs::read(path).unwrap_or_default();
            if bytes
                .windows(self.test_token.len())
                .any(|window| window == self.test_token.as_bytes())
            {
                discovered.push(SessionId(stem.to_string()));
            }
        }
        self.session_ids.extend(discovered);
        Ok(())
    }

    fn register_test_logs(&mut self) -> Result<(), String> {
        let logs = self
            .directories
            .iter()
            .find(|snapshot| snapshot.path.ends_with(".tyde/antigravity/logs"))
            .ok_or_else(|| "Antigravity log baseline is unavailable".to_string())?;
        for path in antigravity_native_directory_entries(&logs.path)?
            .difference(&logs.entries)
            .cloned()
            .collect::<Vec<_>>()
        {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains(&self.test_token)
                || self
                    .session_ids
                    .iter()
                    .any(|session_id| text.contains(&session_id.0))
            {
                self.owned_paths.insert(path);
            }
        }
        Ok(())
    }

    fn register_test_implicit_artifacts(&mut self) -> Result<(), String> {
        let implicit = self
            .directories
            .iter()
            .find(|snapshot| snapshot.path.ends_with("antigravity-cli/implicit"))
            .ok_or_else(|| "Antigravity implicit baseline is unavailable".to_string())?;
        let session_ids = self
            .session_ids
            .iter()
            .map(|session_id| session_id.0.as_bytes())
            .collect::<Vec<_>>();
        for path in antigravity_native_directory_entries(&implicit.path)?
            .difference(&implicit.entries)
            .cloned()
            .collect::<Vec<_>>()
        {
            let bytes = std::fs::read(&path).unwrap_or_default();
            let belongs_to_test = bytes
                .windows(self.test_token.len())
                .any(|window| window == self.test_token.as_bytes())
                || session_ids.iter().any(|session_id| {
                    bytes
                        .windows(session_id.len())
                        .any(|window| window == *session_id)
                });
            if belongs_to_test {
                self.owned_paths.insert(path);
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), String> {
        let result = self.cleanup_and_verify();
        self.finalized = result.is_ok();
        result
    }

    fn cleanup_and_verify(&mut self) -> Result<(), String> {
        self.register_test_conversations_from_native_diff()?;
        self.register_test_implicit_artifacts()?;
        for session_id in self.session_ids.clone() {
            let base = self
                .native_root
                .join("conversations")
                .join(format!("{}.db", session_id.0));
            self.owned_paths.insert(base.clone());
            self.owned_paths
                .insert(PathBuf::from(format!("{}-wal", base.display())));
            self.owned_paths
                .insert(PathBuf::from(format!("{}-shm", base.display())));
            self.owned_paths
                .insert(self.native_root.join("brain").join(&session_id.0));
            self.owned_paths.insert(
                self.native_root
                    .join("implicit")
                    .join(format!("{}.pb", session_id.0)),
            );
        }
        self.register_test_logs()?;
        let mut owned_paths = self.owned_paths.iter().cloned().collect::<Vec<_>>();
        owned_paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in owned_paths {
            if path.is_dir() {
                std::fs::remove_dir_all(&path).map_err(|error| {
                    format!(
                        "failed to remove test-owned native directory {}: {error}",
                        path.display()
                    )
                })?;
            } else if path.exists() {
                std::fs::remove_file(&path).map_err(|error| {
                    format!(
                        "failed to remove test-owned native file {}: {error}",
                        path.display()
                    )
                })?;
            }
        }
        for snapshot in &self.shared_files {
            snapshot.restore()?;
        }
        for snapshot in &self.shared_files {
            snapshot.verify()?;
        }
        for snapshot in &self.directories {
            snapshot.verify()?;
        }
        Ok(())
    }
}

impl Drop for AntigravityNativeArtifactGuard {
    fn drop(&mut self) {
        if !self.finalized {
            let _ = self.cleanup_and_verify();
        }
    }
}

fn antigravity_native_bytes_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn antigravity_native_directory_entries(path: &Path) -> Result<HashSet<PathBuf>, String> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    std::fs::read_dir(path)
        .map_err(|error| {
            format!(
                "failed to read native directory {}: {error}",
                path.display()
            )
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("failed to read entry in {}: {error}", path.display()))
        })
        .collect()
}

struct RealBackendFixture {
    client: ValidatedConnection,
    host: server::HostHandle,
    #[allow(dead_code)]
    session_store_dir: tempfile::TempDir,
    workspace_dir: tempfile::TempDir,
}

struct ValidatedConnection {
    inner: client::Connection,
    validator: ProtocolValidator,
    pending_bootstrap_events: VecDeque<Envelope>,
}

impl ValidatedConnection {
    async fn spawn_agent(
        &mut self,
        payload: SpawnAgentPayload,
    ) -> Result<(), protocol::FrameError> {
        self.inner.spawn_agent(payload).await
    }

    async fn list_sessions(
        &mut self,
        payload: ListSessionsPayload,
    ) -> Result<(), protocol::FrameError> {
        self.inner.list_sessions(payload).await
    }

    async fn delete_session(
        &mut self,
        payload: DeleteSessionPayload,
    ) -> Result<(), protocol::FrameError> {
        self.inner.delete_session(payload).await
    }

    async fn close_agent(&mut self, stream: &StreamPath) -> Result<(), protocol::FrameError> {
        self.inner.close_agent(stream).await
    }

    async fn next_event(&mut self) -> Result<Option<Envelope>, protocol::FrameError> {
        if let Some(envelope) = self.pending_bootstrap_events.pop_front() {
            return Ok(Some(envelope));
        }

        let Some(envelope) = self.inner.next_event().await? else {
            return Ok(None);
        };

        if let Err(error) = self.validator.validate_envelope(&envelope) {
            panic!("protocol violation while reading backend events: {error}");
        }

        self.queue_agent_bootstrap_chat_events(&envelope);

        Ok(Some(envelope))
    }

    fn queue_agent_bootstrap_chat_events(&mut self, envelope: &Envelope) {
        if envelope.kind != FrameKind::AgentBootstrap {
            return;
        }

        let payload: AgentBootstrapPayload = envelope
            .parse_payload()
            .expect("parse AgentBootstrap for replayed ChatEvents");
        for event in payload.events {
            let AgentBootstrapEvent::ChatEvent(chat_event) = event else {
                continue;
            };
            self.pending_bootstrap_events.push_back(Envelope {
                stream: envelope.stream.clone(),
                kind: FrameKind::ChatEvent,
                seq: envelope.seq,
                payload: serde_json::to_value(chat_event)
                    .expect("serialize replayed bootstrap ChatEvent"),
            });
        }
    }

    async fn interrupt(&mut self, stream: &StreamPath) -> Result<(), protocol::FrameError> {
        self.inner.interrupt(stream).await
    }

    async fn send_message(
        &mut self,
        stream: &StreamPath,
        message: String,
    ) -> Result<(), protocol::FrameError> {
        self.inner.send_message(stream, message).await
    }
}

impl RealBackendFixture {
    async fn new(backend_kind: BackendKind) -> Self {
        init_tracing();

        let session_store_dir = tempfile::tempdir().expect("create session tempdir");
        let workspace_dir = tempfile::tempdir().expect("create workspace tempdir");
        std::fs::write(
            workspace_dir.path().join("README.txt"),
            "real backend test workspace",
        )
        .expect("seed workspace tempdir");
        let session_path = session_store_dir.path().join("sessions.json");
        let project_path = session_store_dir.path().join("projects.json");
        let settings_path = session_store_dir.path().join("settings.json");
        // These tests spawn with low cost hints to keep real backend runs
        // fast and cheap. Hints are ignored unless complexity tiers are
        // enabled, so seed the settings store with the feature on.
        let mut settings = json!({
            "settings": {
                "enabled_backends": [backend_kind],
                "default_backend": backend_kind,
                "complexity_tiers_enabled": true
            }
        });
        match backend_kind {
            BackendKind::Claude => {
                settings["settings"]["backend_tier_configs"] = json!({
                    "claude": {
                        "low": {
                            "model": {"string": UNIVERSAL_CLAUDE_MODEL},
                            "effort": {"string": UNIVERSAL_CLAUDE_EFFORT}
                        }
                    }
                });
            }
            BackendKind::Codex => {
                settings["settings"]["backend_tier_configs"] = json!({
                    "codex": {
                        "low": {
                            "model": {"string": UNIVERSAL_CODEX_MODEL},
                            "reasoning_effort": {"string": UNIVERSAL_CODEX_REASONING_EFFORT}
                        }
                    }
                });
            }
            _ => {}
        }
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&settings).expect("serialize real backend settings"),
        )
        .expect("seed settings store with complexity tiers enabled");
        // Real backends — NOT mock
        let host = server::spawn_host_with_store_paths(session_path, project_path, settings_path)
            .expect("initialize host with real backends");

        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();
        let client_config = client::ClientConfig::current();

        let connection_host = host.clone();
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
            client: ValidatedConnection {
                inner: client,
                validator: ProtocolValidator::new(),
                pending_bootstrap_events: VecDeque::new(),
            },
            host,
            session_store_dir,
            workspace_dir,
        }
    }

    fn workspace_roots(&self) -> Vec<String> {
        vec![self.workspace_dir.path().to_string_lossy().to_string()]
    }

    async fn connect(&self) -> ValidatedConnection {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();
        let client_config = client::ClientConfig::current();
        let host = self.host.clone();

        tokio::spawn(async move {
            let conn = server::accept(&server_config, server_stream)
                .await
                .expect("server handshake failed");
            if let Err(err) = server::run_connection(conn, host).await {
                eprintln!("server connection loop failed: {err:?}");
            }
        });

        let client = client::connect(&client_config, client_stream)
            .await
            .expect("client handshake failed");
        ValidatedConnection {
            inner: client,
            validator: ProtocolValidator::new(),
            pending_bootstrap_events: VecDeque::new(),
        }
    }
}

async fn expect_next_event(client: &mut ValidatedConnection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(REAL_BACKEND_TIMEOUT, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };

        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
        ) {
            continue;
        }

        return env;
    }
}

async fn expect_next_event_kind(
    client: &mut ValidatedConnection,
    expected_kind: FrameKind,
    context: &str,
) -> Envelope {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == expected_kind {
            return env;
        }
    }
}

async fn expect_agent_start_on_stream(
    client: &mut ValidatedConnection,
    expected_stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == *expected_stream {
            return agent_start_from_bootstrap(env, context);
        }
    }
}

async fn expect_subagent_child_for_parent(
    client: &mut ValidatedConnection,
    parent_agent_id: &protocol::AgentId,
    context: &str,
) -> NewAgentPayload {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for backend-native child for parent {} ({context})",
            parent_agent_id
        );
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
        if matches!(
            payload.origin,
            AgentOrigin::BackendNative | AgentOrigin::AgentControl
        ) && payload.parent_agent_id.as_ref() == Some(parent_agent_id)
        {
            return payload;
        }
    }
}

async fn spawn_agent_via_protocol(
    client: &mut ValidatedConnection,
    workspace_roots: Vec<String>,
    backend_kind: BackendKind,
    name: &str,
    prompt: &str,
) -> protocol::StreamPath {
    spawn_agent_via_protocol_with_options(
        client,
        workspace_roots,
        backend_kind,
        name,
        prompt,
        None,
        cost_hint_for(backend_kind),
    )
    .await
}

async fn spawn_agent_via_protocol_with_images(
    client: &mut ValidatedConnection,
    workspace_roots: Vec<String>,
    backend_kind: BackendKind,
    name: &str,
    prompt: &str,
    images: Option<Vec<ImageData>>,
) -> protocol::StreamPath {
    spawn_agent_via_protocol_with_options(
        client,
        workspace_roots,
        backend_kind,
        name,
        prompt,
        images,
        cost_hint_for(backend_kind),
    )
    .await
}

async fn spawn_agent_via_protocol_with_options(
    client: &mut ValidatedConnection,
    workspace_roots: Vec<String>,
    backend_kind: BackendKind,
    name: &str,
    prompt: &str,
    images: Option<Vec<ImageData>>,
    cost_hint: Option<SpawnCostHint>,
) -> protocol::StreamPath {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots,
                prompt: prompt.to_owned(),
                images,
                backend_kind,
                launch_profile_id: None,
                cost_hint,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let new_agent_context = format!("{backend_kind:?} NewAgent");
    let env = expect_next_event_kind(client, FrameKind::NewAgent, &new_agent_context).await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    assert_eq!(new_agent.backend_kind, backend_kind);
    let agent_stream = new_agent.instance_stream;

    let agent_start_context = format!("{backend_kind:?} AgentStart");
    let agent_start =
        expect_agent_start_on_stream(client, &agent_stream, &agent_start_context).await;
    assert_eq!(agent_start.agent_id, new_agent.agent_id);

    agent_stream
}

async fn resume_agent_via_protocol(
    client: &mut ValidatedConnection,
    name: &str,
    session_id: protocol::SessionId,
    prompt: &str,
) -> StreamPath {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: Some(prompt.to_owned()),
            },
        })
        .await
        .expect("resume spawn_agent failed");

    let env = expect_next_event_kind(client, FrameKind::NewAgent, "resumed NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");
    let agent_stream = new_agent.instance_stream;

    let agent_start =
        expect_agent_start_on_stream(client, &agent_stream, "resumed AgentStart").await;
    assert_eq!(agent_start.agent_id, new_agent.agent_id);

    agent_stream
}

async fn resume_agent_without_prompt_via_protocol(
    client: &mut ValidatedConnection,
    name: &str,
    session_id: protocol::SessionId,
) -> StreamPath {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("resume spawn_agent failed");

    let env = expect_next_event_kind(client, FrameKind::NewAgent, "resumed NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgent");
    let agent_stream = new_agent.instance_stream;
    let agent_start =
        expect_agent_start_on_stream(client, &agent_stream, "resumed AgentStart").await;
    assert_eq!(agent_start.agent_id, new_agent.agent_id);
    agent_stream
}

async fn spawn_antigravity_with_start(
    client: &mut ValidatedConnection,
    workspace_roots: Vec<String>,
    name: &str,
    prompt: &str,
) -> (StreamPath, AgentStartPayload) {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots,
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Antigravity,
                launch_profile_id: None,
                cost_hint: Some(SpawnCostHint::Low),
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn Antigravity regression agent");

    let envelope = expect_next_event_kind(
        client,
        FrameKind::NewAgent,
        "Antigravity regression NewAgent",
    )
    .await;
    let new_agent: NewAgentPayload = envelope
        .parse_payload()
        .expect("parse Antigravity regression NewAgent");
    assert_eq!(new_agent.backend_kind, BackendKind::Antigravity);
    let start = expect_agent_start_on_stream(
        client,
        &new_agent.instance_stream,
        "Antigravity regression AgentStart",
    )
    .await;
    assert_eq!(start.backend_kind, BackendKind::Antigravity);
    (new_agent.instance_stream, start)
}

async fn resume_antigravity_with_start(
    client: &mut ValidatedConnection,
    session_id: SessionId,
    name: &str,
    prompt: &str,
) -> (StreamPath, AgentStartPayload) {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: Some(prompt.to_owned()),
            },
        })
        .await
        .expect("resume Antigravity regression agent");

    let envelope = expect_next_event_kind(
        client,
        FrameKind::NewAgent,
        "resumed Antigravity regression NewAgent",
    )
    .await;
    let new_agent: NewAgentPayload = envelope
        .parse_payload()
        .expect("parse resumed Antigravity regression NewAgent");
    assert_eq!(new_agent.backend_kind, BackendKind::Antigravity);
    let start = expect_agent_start_on_stream(
        client,
        &new_agent.instance_stream,
        "resumed Antigravity regression AgentStart",
    )
    .await;
    assert_eq!(start.backend_kind, BackendKind::Antigravity);
    (new_agent.instance_stream, start)
}

async fn close_antigravity_regression_agent(client: &mut ValidatedConnection, stream: &StreamPath) {
    client
        .close_agent(stream)
        .await
        .expect("close Antigravity regression agent");
    loop {
        let envelope = expect_next_event(client, "Antigravity regression AgentClosed").await;
        if envelope.kind == FrameKind::AgentClosed && envelope.stream == *stream {
            return;
        }
    }
}

fn antigravity_start_session(start: &AgentStartPayload) -> SessionId {
    start
        .session_id
        .clone()
        .expect("Antigravity AgentStart must carry its native UUID")
}

fn antigravity_routing_prompt(test_token: &str, marker_file: &str, probe_file: &str) -> String {
    format!(
        "Routing regression token {test_token}. Use the terminal without changing directories. \
         Run exactly: {{ printf '%s\\n' \"$PWD\"; cat '{marker_file}'; }} > '{probe_file}'; \
         cat '{probe_file}'. Then reply with exactly the command output."
    )
}

fn seed_antigravity_marker(
    guard: &mut AntigravityNativeArtifactGuard,
    root: &Path,
    marker_file: &str,
    marker: &str,
) {
    std::fs::create_dir_all(root)
        .unwrap_or_else(|error| panic!("create routing marker root {}: {error}", root.display()));
    let path = root.join(marker_file);
    assert!(
        !path.exists(),
        "unique routing marker unexpectedly exists: {}",
        path.display()
    );
    guard.track_owned_path(path.clone());
    std::fs::write(&path, marker)
        .unwrap_or_else(|error| panic!("write routing marker {}: {error}", path.display()));
}

fn track_antigravity_probe_paths(
    guard: &mut AntigravityNativeArtifactGuard,
    roots: &[&Path],
    probe_file: &str,
) {
    for root in roots {
        guard.track_owned_path(root.join(probe_file));
    }
}

fn assert_antigravity_routing_probe(
    expected_root: &Path,
    prohibited_roots: &[&Path],
    probe_file: &str,
    expected_marker: &str,
    assistant_text: &str,
) {
    let expected_probe = expected_root.join(probe_file);
    let content = std::fs::read_to_string(&expected_probe).unwrap_or_else(|error| {
        panic!(
            "routing probe was not created in primary {}: {error}; assistant={assistant_text:?}",
            expected_probe.display()
        )
    });
    assert_eq!(
        content.lines().next(),
        Some(expected_root.to_string_lossy().as_ref()),
        "routing probe recorded the wrong pwd: {content:?}"
    );
    assert!(
        content.contains(expected_marker),
        "routing probe did not read the primary marker: {content:?}"
    );
    for root in prohibited_roots {
        assert!(
            !root.join(probe_file).exists(),
            "routing probe escaped to {}",
            root.display()
        );
    }
    assert!(
        assistant_text.contains(expected_marker)
            && assistant_text.contains(expected_root.to_string_lossy().as_ref()),
        "rendered Antigravity result did not report the primary probe: {assistant_text:?}"
    );
}

async fn list_sessions_via_protocol(client: &mut ValidatedConnection) -> SessionListPayload {
    client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");

    let env = expect_next_event_kind(client, FrameKind::SessionList, "SessionList").await;
    env.parse_payload().expect("parse SessionList")
}

async fn expect_assistant_turn_after_user_echo(
    client: &mut ValidatedConnection,
    agent_stream: &StreamPath,
    prompt: &str,
) -> AssistantTurn {
    let mut got_user_message_echo = false;
    let mut got_stream_start = false;
    let mut streamed_text = String::new();
    let mut delta_count = 0usize;

    loop {
        let env = expect_next_event(client, "ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User) && message.content == prompt {
                    got_user_message_echo = true;
                } else if got_user_message_echo && matches!(message.sender, MessageSender::Error) {
                    panic!(
                        "backend returned error instead of assistant response for prompt {:?}: {}",
                        prompt, message.content
                    );
                }
            }
            ChatEvent::StreamStart(_) => {
                if !got_user_message_echo {
                    continue;
                }
                assert!(
                    got_user_message_echo,
                    "received StreamStart before MessageAdded(User) for prompt {prompt:?}"
                );
                got_stream_start = true;
            }
            ChatEvent::StreamDelta(delta) => {
                if got_stream_start {
                    delta_count += 1;
                    streamed_text.push_str(&delta.text);
                }
            }
            ChatEvent::StreamEnd(data) => {
                if !got_user_message_echo {
                    continue;
                }
                assert!(
                    got_user_message_echo,
                    "never received MessageAdded(User) echo"
                );
                assert!(got_stream_start, "received StreamEnd before StreamStart");
                if !data.message.tool_calls.is_empty() {
                    got_stream_start = false;
                    streamed_text.clear();
                    delta_count = 0;
                    continue;
                }
                let final_text = if data.message.content.trim().is_empty() {
                    std::mem::take(&mut streamed_text)
                } else {
                    data.message.content
                };
                if final_text.trim().is_empty() {
                    got_stream_start = false;
                    delta_count = 0;
                    continue;
                }
                return AssistantTurn {
                    final_text,
                    delta_count,
                };
            }
            ChatEvent::TypingStatusChanged(false) if got_user_message_echo => {
                panic!("backend became idle without a non-empty assistant response")
            }
            _ => {}
        }
    }
}

async fn expect_assistant_turn_after_user_echo_with_images(
    client: &mut ValidatedConnection,
    agent_stream: &StreamPath,
    prompt: &str,
    expected_images: &[ImageData],
) -> AssistantTurn {
    let mut got_user_message_echo = false;
    let mut got_stream_start = false;
    let mut streamed_text = String::new();
    let mut delta_count = 0usize;

    loop {
        let env = expect_next_event(client, "ChatEvent with image echo").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User)
                    && message.content == prompt
                    && message.images.as_deref() == Some(expected_images)
                {
                    got_user_message_echo = true;
                }
            }
            ChatEvent::StreamStart(_) => {
                if !got_user_message_echo {
                    continue;
                }
                got_stream_start = true;
            }
            ChatEvent::StreamDelta(delta) => {
                if got_stream_start {
                    delta_count += 1;
                    streamed_text.push_str(&delta.text);
                }
            }
            ChatEvent::StreamEnd(data) => {
                if !got_user_message_echo {
                    continue;
                }
                assert!(got_stream_start, "received StreamEnd before StreamStart");
                let final_text = if data.message.content.trim().is_empty() {
                    std::mem::take(&mut streamed_text)
                } else {
                    data.message.content
                };
                if final_text.trim().is_empty() {
                    got_stream_start = false;
                    delta_count = 0;
                    continue;
                }
                return AssistantTurn {
                    final_text,
                    delta_count,
                };
            }
            ChatEvent::TypingStatusChanged(false) if got_user_message_echo => {
                panic!("backend became idle without a non-empty image response")
            }
            _ => {}
        }
    }
}

struct AssistantTurn {
    final_text: String,
    delta_count: usize,
}

#[derive(Debug)]
struct FoldedTokenTurn {
    message: ChatMessage,
    stats_total: TokenUsage,
}

#[derive(Debug)]
struct KnownTokenTurn {
    this_turn: TokenUsage,
    agent_total: TokenUsage,
    stats_total: TokenUsage,
}

fn token_sum(first: &TokenUsage, second: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: first.input_tokens.saturating_add(second.input_tokens),
        output_tokens: first.output_tokens.saturating_add(second.output_tokens),
        total_tokens: first.total_tokens.saturating_add(second.total_tokens),
        cached_prompt_tokens: optional_token_sum(
            first.cached_prompt_tokens,
            second.cached_prompt_tokens,
        ),
        cache_creation_input_tokens: optional_token_sum(
            first.cache_creation_input_tokens,
            second.cache_creation_input_tokens,
        ),
        reasoning_tokens: optional_token_sum(first.reasoning_tokens, second.reasoning_tokens),
    }
}

fn optional_token_sum(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    match (first, second) {
        (None, None) => None,
        (first, second) => Some(first.unwrap_or(0).saturating_add(second.unwrap_or(0))),
    }
}

fn format_token_usage(usage: &TokenUsage) -> String {
    format!(
        "input={} output={} total={} cached={} cache_creation={} reasoning={}",
        usage.input_tokens,
        usage.output_tokens,
        usage.total_tokens,
        usage.cached_prompt_tokens.unwrap_or(0),
        usage.cache_creation_input_tokens.unwrap_or(0),
        usage.reasoning_tokens.unwrap_or(0)
    )
}

fn assert_token_usage_sane(context: &str, usage: &TokenUsage) {
    assert!(
        usage.total_tokens >= usage.input_tokens,
        "{context}: total tokens must be >= input tokens: {usage:?}"
    );
    assert!(
        usage.total_tokens >= usage.output_tokens,
        "{context}: total tokens must be >= output tokens: {usage:?}"
    );
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        assert!(
            usage.total_tokens >= reasoning_tokens,
            "{context}: total tokens must be >= reasoning tokens: {usage:?}"
        );
    }
}

fn fold_metadata_update_into_message(message: &mut ChatMessage, update: MessageMetadataUpdateData) {
    assert_eq!(
        message.message_id.as_ref(),
        Some(&update.message_id),
        "metadata update must target the folded assistant message"
    );

    if let Some(model_info) = update.model_info {
        message.model_info = Some(model_info);
    }
    if let Some(token_usage) = update.token_usage {
        message.token_usage = Some(token_usage);
    }
    if let Some(context_breakdown) = update.context_breakdown {
        message.context_breakdown = Some(context_breakdown);
    }
}

fn fold_pending_metadata_updates(
    message: &mut ChatMessage,
    pending_metadata_updates: &mut Vec<MessageMetadataUpdateData>,
) {
    let mut still_pending = Vec::new();
    for update in pending_metadata_updates.drain(..) {
        if message.message_id.as_ref() == Some(&update.message_id) {
            fold_metadata_update_into_message(message, update);
        } else {
            still_pending.push(update);
        }
    }
    *pending_metadata_updates = still_pending;
}

fn known_turn_from_folded(
    backend_kind: BackendKind,
    turn_index: usize,
    folded: &FoldedTokenTurn,
) -> KnownTokenTurn {
    let usage = folded.message.token_usage.as_ref().unwrap_or_else(|| {
        panic!(
            "{} turn {turn_index} missing token_usage on folded message: {:?}",
            backend_label(backend_kind),
            folded.message
        )
    });
    let Some(this_turn) = usage.turn.known_usage() else {
        panic!(
            "{} turn {turn_index} reported unavailable token usage on folded message: {:?}",
            backend_label(backend_kind),
            folded.message
        );
    };
    let Some(agent_total) = usage.cumulative.known_usage() else {
        panic!(
            "{} turn {turn_index} missing cumulative token usage on folded message: {:?}",
            backend_label(backend_kind),
            folded.message
        );
    };

    let this_turn = this_turn.clone();
    let agent_total = agent_total.clone();
    if let Some(request) = folded
        .message
        .token_usage
        .as_ref()
        .and_then(|usage| usage.request.known_usage())
    {
        assert_eq!(
            request,
            &this_turn,
            "{} turn {turn_index}: reported request usage must match this turn usage for one-request backend turn",
            backend_label(backend_kind)
        );
    }
    assert!(
        this_turn.total_tokens > 0,
        "{} turn {turn_index}: this_turn.total_tokens must be positive: {:?}",
        backend_label(backend_kind),
        this_turn
    );
    assert_token_usage_sane(
        &format!(
            "{} turn {turn_index} this_turn",
            backend_label(backend_kind)
        ),
        &this_turn,
    );
    assert_token_usage_sane(
        &format!(
            "{} turn {turn_index} agent_total",
            backend_label(backend_kind)
        ),
        &agent_total,
    );
    assert_eq!(
        folded.stats_total,
        agent_total,
        "{} turn {turn_index}: AgentActivityStats.token_usage must mirror agent_total",
        backend_label(backend_kind)
    );

    eprintln!(
        "TOKEN_USAGE {} turn {} this_turn={} agent_total={} stats_total={}",
        backend_label(backend_kind),
        turn_index,
        format_token_usage(&this_turn),
        format_token_usage(&agent_total),
        format_token_usage(&folded.stats_total)
    );

    KnownTokenTurn {
        this_turn,
        agent_total,
        stats_total: folded.stats_total.clone(),
    }
}

fn assert_unavailable_folded_turn(
    backend_kind: BackendKind,
    turn_index: usize,
    folded: &FoldedTokenTurn,
) {
    assert!(
        matches!(
            folded
                .message
                .token_usage
                .as_ref()
                .map(|usage| &usage.request),
            Some(TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            })
        ),
        "{} turn {turn_index}: non-reporting backend should not fabricate ChatMessage.token_usage: {:?}",
        backend_label(backend_kind),
        folded.message.token_usage
    );
    match folded.message.token_usage.as_ref().map(|usage| &usage.turn) {
        Some(TokenUsageScope::Unavailable {
            reason: TokenUsageUnavailableReason::BackendDidNotReport,
        }) => {}
        other => panic!(
            "{} turn {turn_index}: expected turn usage Unavailable(BackendDidNotReport), got {other:?}",
            backend_label(backend_kind)
        ),
    }
    assert_eq!(
        folded.stats_total,
        TokenUsage::default(),
        "{} turn {turn_index}: non-reporting backend should leave AgentActivityStats.token_usage at zero",
        backend_label(backend_kind)
    );
    eprintln!(
        "TOKEN_USAGE {} turn {} unavailable reason=BackendDidNotReport stats_total={}",
        backend_label(backend_kind),
        turn_index,
        format_token_usage(&folded.stats_total)
    );
}

async fn expect_folded_token_turn_after_user_echo(
    client: &mut ValidatedConnection,
    agent_stream: &StreamPath,
    prompt: &str,
    backend_kind: BackendKind,
    turn_index: usize,
) -> FoldedTokenTurn {
    let mut got_user_message_echo = false;
    let mut got_stream_start = false;
    let mut saw_typing_false = false;
    let mut streamed_text = String::new();
    let mut final_message = None::<ChatMessage>;
    let mut pending_metadata_updates = Vec::new();
    let mut latest_stats = None::<TokenUsage>;

    while !saw_typing_false {
        let context = format!(
            "{} cumulative token turn {turn_index} event",
            backend_label(backend_kind)
        );
        let env = expect_next_event(client, &context).await;
        if env.stream != *agent_stream {
            continue;
        }

        match env.kind {
            FrameKind::AgentActivityStats => {
                let payload: AgentActivityStatsPayload = env
                    .parse_payload()
                    .expect("parse AgentActivityStats payload");
                latest_stats = Some(payload.stats.token_usage);
            }
            FrameKind::ChatEvent => {
                let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                match event {
                    ChatEvent::MessageAdded(message) => {
                        if matches!(message.sender, MessageSender::User)
                            && message.content == prompt
                        {
                            got_user_message_echo = true;
                        } else if got_user_message_echo
                            && matches!(message.sender, MessageSender::Error)
                        {
                            panic!(
                                "{} returned error instead of assistant response for prompt {:?}: {}",
                                backend_label(backend_kind),
                                prompt,
                                message.content
                            );
                        }
                    }
                    ChatEvent::StreamStart(_) => {
                        if got_user_message_echo {
                            got_stream_start = true;
                            streamed_text.clear();
                        }
                    }
                    ChatEvent::StreamDelta(delta) => {
                        if got_stream_start {
                            streamed_text.push_str(&delta.text);
                        }
                    }
                    ChatEvent::StreamEnd(data) => {
                        if !got_user_message_echo {
                            continue;
                        }
                        assert!(
                            got_stream_start,
                            "{} turn {turn_index}: received StreamEnd before StreamStart",
                            backend_label(backend_kind)
                        );
                        let mut message = data.message;
                        if message.content.trim().is_empty() {
                            message.content = streamed_text.clone();
                        }
                        fold_pending_metadata_updates(&mut message, &mut pending_metadata_updates);
                        final_message = Some(message);
                    }
                    ChatEvent::MessageMetadataUpdated(update) => {
                        if !got_user_message_echo {
                            continue;
                        }
                        if let Some(message) = final_message.as_mut() {
                            if message.message_id.as_ref() == Some(&update.message_id) {
                                fold_metadata_update_into_message(message, update);
                            } else {
                                pending_metadata_updates.push(update);
                            }
                        } else {
                            pending_metadata_updates.push(update);
                        }
                    }
                    ChatEvent::TypingStatusChanged(false) if got_user_message_echo => {
                        saw_typing_false = true;
                    }
                    _ => {}
                }
            }
            FrameKind::AgentError => {
                panic!(
                    "{} turn {turn_index}: received AgentError: {:?}",
                    backend_label(backend_kind),
                    env.payload
                );
            }
            _ => {}
        }
    }

    let message = final_message.unwrap_or_else(|| {
        panic!(
            "{} turn {turn_index}: typing stopped before assistant StreamEnd",
            backend_label(backend_kind)
        )
    });
    assert!(
        !message.content.trim().is_empty(),
        "{} turn {turn_index}: expected non-empty assistant response",
        backend_label(backend_kind)
    );
    let stats_total = latest_stats.unwrap_or_else(|| {
        panic!(
            "{} turn {turn_index}: expected AgentActivityStats before typing stopped",
            backend_label(backend_kind)
        )
    });

    FoldedTokenTurn {
        message,
        stats_total,
    }
}

async fn backend_ready_or_skip(backend_kind: BackendKind) -> bool {
    if !backend_binary_available(backend_kind) {
        eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
        return false;
    }
    if !backend_runtime_available(backend_kind) {
        eprintln!(
            "SKIPPED: {} not runnable in current environment",
            backend_label(backend_kind)
        );
        return false;
    }
    if let Err(reason) = probe_backend_runtime(backend_kind).await {
        eprintln!(
            "SKIPPED: {} failed readiness probe: {}",
            backend_label(backend_kind),
            reason
        );
        return false;
    }
    true
}

async fn assert_backend_reports_cumulative_turn_token_usage(backend_kind: BackendKind) {
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    let first_prompt = "Say hi in one word.";
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "cumulative-token-usage",
        first_prompt,
    )
    .await;
    let first = expect_folded_token_turn_after_user_echo(
        &mut fixture.client,
        &agent_stream,
        first_prompt,
        backend_kind,
        1,
    )
    .await;
    let first = known_turn_from_folded(backend_kind, 1, &first);
    assert_eq!(
        first.agent_total,
        first.this_turn,
        "{} first turn agent_total must equal this_turn across all token fields",
        backend_label(backend_kind)
    );
    assert_eq!(
        first.stats_total,
        first.agent_total,
        "{} first turn stats_total must equal agent_total",
        backend_label(backend_kind)
    );

    let second_prompt = "Say bye in one word.";
    fixture
        .client
        .send_message(&agent_stream, second_prompt.to_owned())
        .await
        .expect("send second cumulative token prompt");
    let second = expect_folded_token_turn_after_user_echo(
        &mut fixture.client,
        &agent_stream,
        second_prompt,
        backend_kind,
        2,
    )
    .await;
    let second = known_turn_from_folded(backend_kind, 2, &second);
    let expected_total = token_sum(&first.this_turn, &second.this_turn);
    assert_eq!(
        second.agent_total,
        expected_total,
        "{} second turn agent_total must equal the sum of per-turn deltas",
        backend_label(backend_kind)
    );
    assert!(
        second.agent_total.total_tokens > first.agent_total.total_tokens,
        "{} second cumulative total must grow beyond the first turn: first={}, second={}",
        backend_label(backend_kind),
        first.agent_total.total_tokens,
        second.agent_total.total_tokens
    );
    assert!(
        second.agent_total.total_tokens > second.this_turn.total_tokens,
        "{} second agent_total must be cumulative, not a raw per-turn leak: this_turn={}, agent_total={}",
        backend_label(backend_kind),
        second.this_turn.total_tokens,
        second.agent_total.total_tokens
    );
}

async fn assert_backend_turn_usage_contract_if_reported(backend_kind: BackendKind) {
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    let first_prompt = "Say hi in one word.";
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "optional-cumulative-token-usage",
        first_prompt,
    )
    .await;
    let first = expect_folded_token_turn_after_user_echo(
        &mut fixture.client,
        &agent_stream,
        first_prompt,
        backend_kind,
        1,
    )
    .await;

    let second_prompt = "Say bye in one word.";
    fixture
        .client
        .send_message(&agent_stream, second_prompt.to_owned())
        .await
        .expect("send second optional token prompt");
    let second = expect_folded_token_turn_after_user_echo(
        &mut fixture.client,
        &agent_stream,
        second_prompt,
        backend_kind,
        2,
    )
    .await;

    match (
        first.message.token_usage.as_ref().map(|usage| &usage.turn),
        second.message.token_usage.as_ref().map(|usage| &usage.turn),
    ) {
        (Some(TokenUsageScope::Known { .. }), Some(TokenUsageScope::Known { .. })) => {
            let first = known_turn_from_folded(backend_kind, 1, &first);
            let second = known_turn_from_folded(backend_kind, 2, &second);
            assert_eq!(
                first.agent_total,
                first.this_turn,
                "{} first turn agent_total must equal this_turn across all token fields",
                backend_label(backend_kind)
            );
            assert_eq!(
                second.agent_total,
                token_sum(&first.this_turn, &second.this_turn),
                "{} second turn agent_total must equal the sum of per-turn deltas",
                backend_label(backend_kind)
            );
            assert!(
                second.agent_total.total_tokens > first.agent_total.total_tokens,
                "{} second cumulative total must grow beyond the first turn",
                backend_label(backend_kind)
            );
            assert!(
                second.agent_total.total_tokens > second.this_turn.total_tokens,
                "{} second agent_total must be cumulative",
                backend_label(backend_kind)
            );
        }
        (
            Some(TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            }),
            Some(TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport,
            }),
        ) => {
            assert_unavailable_folded_turn(backend_kind, 1, &first);
            assert_unavailable_folded_turn(backend_kind, 2, &second);
        }
        other => panic!(
            "{} reported inconsistent token usage availability across two turns: {other:?}",
            backend_label(backend_kind)
        ),
    }
}

struct AssistantTurnWithTyping {
    final_text: String,
    delta_count: usize,
    saw_typing_true: bool,
    saw_stream_start: bool,
    saw_stream_end: bool,
    saw_typing_false: bool,
    events: Vec<&'static str>,
}

async fn expect_assistant_turn_with_typing_after_user_echo(
    client: &mut ValidatedConnection,
    agent_stream: &StreamPath,
    prompt: &str,
) -> AssistantTurnWithTyping {
    let mut got_user_message_echo = false;
    let mut saw_typing_true = false;
    let mut saw_stream_start = false;
    let mut saw_stream_end = false;
    let mut saw_typing_false = false;
    let mut streamed_text = String::new();
    let mut final_text = None::<String>;
    let mut delta_count = 0usize;
    let mut events = Vec::new();

    loop {
        let env = expect_next_event(client, "follow-up typing/stream ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                events.push("MessageAdded");
                if matches!(message.sender, MessageSender::User) && message.content == prompt {
                    got_user_message_echo = true;
                } else if got_user_message_echo && matches!(message.sender, MessageSender::Error) {
                    panic!(
                        "backend returned error instead of assistant response for prompt {:?}: {}",
                        prompt, message.content
                    );
                }
            }
            ChatEvent::TypingStatusChanged(true) => {
                events.push("TypingStatusChanged(true)");
                if got_user_message_echo && !saw_typing_true {
                    saw_typing_true = true;
                }
            }
            ChatEvent::StreamStart(_) => {
                events.push("StreamStart");
                if !got_user_message_echo {
                    continue;
                }
                assert!(
                    saw_typing_true,
                    "StreamStart arrived before TypingStatusChanged(true) for prompt {:?}; events={events:?}",
                    prompt
                );
                saw_stream_start = true;
            }
            ChatEvent::StreamDelta(delta) => {
                events.push("StreamDelta");
                if saw_stream_start {
                    delta_count += 1;
                    streamed_text.push_str(&delta.text);
                }
            }
            ChatEvent::StreamEnd(data) => {
                events.push("StreamEnd");
                if !got_user_message_echo {
                    continue;
                }
                assert!(
                    saw_stream_start,
                    "received StreamEnd before StreamStart for prompt {:?}; events={events:?}",
                    prompt
                );
                saw_stream_end = true;
                final_text = Some(if data.message.content.trim().is_empty() {
                    streamed_text.clone()
                } else {
                    data.message.content
                });
                if saw_typing_false {
                    break;
                }
            }
            ChatEvent::TypingStatusChanged(false) => {
                events.push("TypingStatusChanged(false)");
                if got_user_message_echo && saw_typing_true {
                    saw_typing_false = true;
                    if saw_stream_end {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    AssistantTurnWithTyping {
        final_text: final_text.expect("turn completed without final text"),
        delta_count,
        saw_typing_true,
        saw_stream_start,
        saw_stream_end,
        saw_typing_false,
        events,
    }
}

#[derive(Debug)]
struct ToolTurn {
    final_text: String,
    tool_requests: Vec<ToolRequest>,
    tool_completions: Vec<ToolExecutionCompletedData>,
}

fn unique_secret() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    format!("TYDE-SECRET-{now}")
}

fn unique_project_identifier() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    format!("TYDE-PROJECT-{now}")
}

fn only_session_for_backend(
    sessions: &[SessionSummary],
    backend_kind: BackendKind,
) -> &SessionSummary {
    let matching: Vec<_> = sessions
        .iter()
        .filter(|session| session.backend_kind == backend_kind)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one {backend_kind:?} session, got {matching:?}"
    );
    matching[0]
}

async fn resume_secret_via_protocol(fixture: &mut RealBackendFixture, backend_kind: BackendKind) {
    let secret = unique_secret();
    let remember_prompt = format!(
        "For the rest of this conversation, the project codename is {secret}. Reply exactly with: noted"
    );
    let workspace_roots = fixture.workspace_roots();
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "remember-secret",
        &remember_prompt,
    )
    .await;
    let first_response =
        expect_assistant_turn_after_user_echo(&mut fixture.client, &agent_stream, &remember_prompt)
            .await;
    assert!(
        !first_response.final_text.trim().is_empty(),
        "expected non-empty initial response before resume"
    );

    let list = list_sessions_via_protocol(&mut fixture.client).await;
    let session = only_session_for_backend(&list.sessions, backend_kind);
    assert!(session.resumable, "expected stored session to be resumable");
    assert_eq!(
        session.message_count, 1,
        "expected one completed turn before resume"
    );

    let recall_prompt =
        "What is the project codename for this conversation? Reply with only the codename.";
    let resumed_stream = resume_agent_via_protocol(
        &mut fixture.client,
        "resume-secret",
        session.id.clone(),
        recall_prompt,
    )
    .await;
    let resumed_response =
        expect_assistant_turn_after_user_echo(&mut fixture.client, &resumed_stream, recall_prompt)
            .await;
    assert!(
        resumed_response.final_text.contains(&secret),
        "expected resumed response to contain secret {secret:?}, got {:?}",
        resumed_response.final_text
    );

    let list_after_resume = list_sessions_via_protocol(&mut fixture.client).await;
    let resumed_session = only_session_for_backend(&list_after_resume.sessions, backend_kind);
    assert_eq!(resumed_session.id, session.id);
    assert_eq!(
        resumed_session.message_count, 2,
        "resume should reuse the same stored session"
    );
}

async fn assert_backend_emits_stream_deltas(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let prompt = "Count from 1 to 20, one number per line, and nothing else.";
    let workspace_roots = fixture.workspace_roots();
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "stream-deltas",
        prompt,
    )
    .await;
    let response =
        expect_assistant_turn_after_user_echo(&mut fixture.client, &agent_stream, prompt).await;

    assert!(
        !response.final_text.trim().is_empty(),
        "expected non-empty streamed response for {backend_kind:?}"
    );
    assert!(
        response.delta_count > 0,
        "expected at least one StreamDelta for {backend_kind:?}"
    );
}

async fn assert_backend_emits_typing_status(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let prompt = "Reply with a single word: hello";
    let workspace_roots = fixture.workspace_roots();
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "typing-status",
        prompt,
    )
    .await;

    let mut got_user_message_echo = false;
    let mut saw_typing_true = false;
    let mut saw_stream_start = false;
    let mut saw_stream_end = false;

    // TypingStatusChanged(true) opens activity, StreamEnd completes the final
    // assistant item, and only then may TypingStatusChanged(false) publish idle.
    let saw_typing_false = loop {
        let env = expect_next_event(&mut fixture.client, "typing status ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User) && message.content == prompt {
                    got_user_message_echo = true;
                }
            }
            ChatEvent::TypingStatusChanged(true) => {
                if got_user_message_echo && !saw_typing_true {
                    saw_typing_true = true;
                }
            }
            ChatEvent::StreamStart(_) => {
                if got_user_message_echo {
                    assert!(
                        saw_typing_true,
                        "StreamStart arrived before TypingStatusChanged(true) for {backend_kind:?}"
                    );
                    saw_stream_start = true;
                }
            }
            ChatEvent::StreamEnd(_) => {
                if got_user_message_echo && saw_stream_start {
                    saw_stream_end = true;
                }
            }
            ChatEvent::TypingStatusChanged(false) if got_user_message_echo && saw_typing_true => {
                assert!(
                    saw_stream_end,
                    "TypingStatusChanged(false) arrived before final StreamEnd for {backend_kind:?}"
                );
                break true;
            }
            _ => {}
        }
    };

    assert!(
        saw_typing_true,
        "expected TypingStatusChanged(true) for {backend_kind:?}"
    );
    assert!(
        saw_stream_start,
        "expected StreamStart for {backend_kind:?}"
    );
    assert!(saw_stream_end, "expected StreamEnd for {backend_kind:?}");
    assert!(
        saw_typing_false,
        "expected TypingStatusChanged(false) for {backend_kind:?}"
    );
}

async fn assert_codex_emits_token_usage(fixture: &mut RealBackendFixture) {
    let prompt = "Reply exactly with USAGE_OK and nothing else.";
    let workspace_roots = fixture.workspace_roots();
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        BackendKind::Codex,
        "token-usage",
        prompt,
    )
    .await;

    let mut got_user_message_echo = false;
    let mut saw_typing_false = false;
    let mut answer_message_id = None;
    let mut pending_metadata_updates = Vec::new();
    let mut saw_metadata_update = false;
    let mut saw_token_usage = None;
    let mut saw_context_breakdown = None;
    let mut final_text = String::new();

    while !saw_typing_false {
        let env = expect_next_event(&mut fixture.client, "Codex token usage ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User) && message.content == prompt {
                    got_user_message_echo = true;
                } else if got_user_message_echo && matches!(message.sender, MessageSender::Error) {
                    panic!(
                        "Codex returned error instead of token usage response: {}",
                        message.content
                    );
                }
            }
            ChatEvent::StreamEnd(data) if got_user_message_echo => {
                let message_id =
                    data.message.message_id.clone().unwrap_or_else(|| {
                        panic!("expected Codex StreamEnd to include message_id")
                    });
                assert!(
                    data.message.token_usage.is_none(),
                    "Codex StreamEnd should not fabricate usage before MessageMetadataUpdated; message_id={message_id}"
                );
                assert!(
                    data.message.context_breakdown.is_none(),
                    "Codex StreamEnd should leave late context breakdown for MessageMetadataUpdated; message_id={message_id}"
                );
                let visible_text = data.message.content;
                if visible_text.contains("USAGE_OK") {
                    assert!(
                        answer_message_id.is_none(),
                        "expected one Codex answer StreamEnd for token usage turn; first_id={:?}, second_id={message_id}",
                        answer_message_id
                    );
                    final_text = visible_text;
                    for update in pending_metadata_updates.iter().filter(
                        |update: &&MessageMetadataUpdateData| update.message_id == message_id,
                    ) {
                        assert!(
                            !saw_metadata_update,
                            "expected one Codex metadata update for message_id {message_id}"
                        );
                        saw_token_usage = update.token_usage.clone();
                        saw_context_breakdown = update.context_breakdown.clone();
                        saw_metadata_update = true;
                    }
                    answer_message_id = Some(message_id);
                }
            }
            ChatEvent::MessageMetadataUpdated(update) if got_user_message_echo => {
                if let Some(message_id) = answer_message_id.as_ref() {
                    if &update.message_id == message_id {
                        assert!(
                            !saw_metadata_update,
                            "expected one Codex metadata update for message_id {message_id}"
                        );
                        saw_token_usage = update.token_usage;
                        saw_context_breakdown = update.context_breakdown;
                        saw_metadata_update = true;
                    }
                } else {
                    pending_metadata_updates.push(update);
                }
            }
            ChatEvent::TypingStatusChanged(false) if got_user_message_echo => {
                assert!(
                    answer_message_id.is_some(),
                    "Codex typing stopped before visible answer StreamEnd"
                );
                assert!(
                    saw_metadata_update,
                    "Codex typing stopped before late metadata update; final_text={final_text:?}"
                );
                saw_typing_false = true;
            }
            _ => {}
        }
    }

    let message_id = answer_message_id
        .as_ref()
        .unwrap_or_else(|| panic!("expected Codex StreamEnd before typing stopped"));
    let usage = saw_token_usage.unwrap_or_else(|| {
        panic!(
            "expected Codex MessageMetadataUpdated for {message_id} to include token usage; final_text={final_text:?}"
        )
    });
    let usage = usage
        .turn
        .known_usage()
        .unwrap_or_else(|| panic!("expected Codex metadata to include known turn usage"));
    assert!(
        usage.total_tokens > 0,
        "expected positive Codex total token usage; got {usage:?}"
    );
    assert!(
        usage.input_tokens > 0
            || usage.cached_prompt_tokens.unwrap_or_default() > 0
            || usage.cache_creation_input_tokens.unwrap_or_default() > 0,
        "expected positive Codex input/cache token usage; got {usage:?}"
    );
    let breakdown = saw_context_breakdown.unwrap_or_else(|| {
        panic!(
            "expected Codex MessageMetadataUpdated for {message_id} to include context breakdown; final_text={final_text:?}"
        )
    });
    assert!(
        breakdown.input_tokens > 0,
        "expected positive Codex context input tokens; got {breakdown:?}"
    );
    assert!(
        breakdown.context_window >= breakdown.input_tokens,
        "expected Codex context window to fit input tokens; got {breakdown:?}"
    );
}

async fn assert_backend_emits_typing_and_streaming_on_follow_up_turns(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let workspace_roots = fixture.workspace_roots();
    let prompts = [
        "Reply with exactly TURN_ONE and nothing else.",
        "Reply with exactly TURN_TWO and nothing else.",
        "Reply with exactly TURN_THREE and nothing else.",
    ];
    let expected_markers = ["TURN_ONE", "TURN_TWO", "TURN_THREE"];

    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "follow-up-thinking",
        prompts[0],
    )
    .await;

    let first_turn = expect_assistant_turn_with_typing_after_user_echo(
        &mut fixture.client,
        &agent_stream,
        prompts[0],
    )
    .await;
    assert!(
        first_turn.final_text.contains(expected_markers[0]),
        "expected first turn response to contain {:?} for {backend_kind:?}, got {:?}; events={:?}",
        expected_markers[0],
        first_turn.final_text,
        first_turn.events
    );
    assert!(
        first_turn.saw_typing_true
            && first_turn.saw_stream_start
            && first_turn.saw_stream_end
            && first_turn.saw_typing_false,
        "expected full typing/stream lifecycle on first turn for {backend_kind:?}; got events={:?}",
        first_turn.events
    );

    for (prompt, expected_marker) in prompts[1..].iter().zip(expected_markers[1..].iter()) {
        fixture
            .client
            .send_message(&agent_stream, (*prompt).to_string())
            .await
            .expect("send follow-up message");
        let turn = expect_assistant_turn_with_typing_after_user_echo(
            &mut fixture.client,
            &agent_stream,
            prompt,
        )
        .await;
        assert!(
            turn.final_text.contains(expected_marker),
            "expected follow-up turn response to contain {:?} for {backend_kind:?}, got {:?}; events={:?}",
            expected_marker,
            turn.final_text,
            turn.events
        );
        assert!(
            turn.saw_typing_true
                && turn.saw_stream_start
                && turn.saw_stream_end
                && turn.saw_typing_false,
            "expected full typing/stream lifecycle on follow-up turn {:?} for {backend_kind:?}; got events={:?}",
            prompt,
            turn.events
        );
        assert!(
            !turn.final_text.trim().is_empty(),
            "expected non-empty follow-up response for {backend_kind:?}; events={:?}",
            turn.events
        );
        assert!(
            turn.delta_count > 0,
            "expected streamed deltas on follow-up turn {:?} for {backend_kind:?}; events={:?}",
            prompt,
            turn.events
        );
    }
}

async fn assert_backend_follow_up_user_echo_not_duplicated(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let workspace_roots = fixture.workspace_roots();
    let first_prompt = "Reply with exactly FIRST_TURN and nothing else.";
    let follow_up_prompt = "Reply with exactly SECOND_TURN and nothing else.";

    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "follow-up-user-echo",
        first_prompt,
    )
    .await;
    let first_turn =
        expect_assistant_turn_after_user_echo(&mut fixture.client, &agent_stream, first_prompt)
            .await;
    assert!(
        first_turn.final_text.contains("FIRST_TURN"),
        "expected first turn response for {backend_kind:?}, got {:?}",
        first_turn.final_text
    );

    fixture
        .client
        .send_message(&agent_stream, follow_up_prompt.to_string())
        .await
        .expect("send follow-up message");

    let mut user_echo_count = 0usize;
    let mut got_stream_start = false;
    let mut streamed_text = String::new();

    loop {
        let env = expect_next_event(&mut fixture.client, "follow-up user echo ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User)
                    && message.content == follow_up_prompt
                {
                    user_echo_count += 1;
                }
            }
            ChatEvent::StreamStart(_) => {
                got_stream_start = true;
            }
            ChatEvent::StreamDelta(delta) => {
                if got_stream_start {
                    streamed_text.push_str(&delta.text);
                }
            }
            ChatEvent::StreamEnd(data) => {
                let final_text = if data.message.content.trim().is_empty() {
                    streamed_text
                } else {
                    data.message.content
                };
                assert!(
                    final_text.contains("SECOND_TURN"),
                    "expected second turn response for {backend_kind:?}, got {:?}",
                    final_text
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        user_echo_count, 1,
        "expected exactly one follow-up MessageAdded(User) echo for {backend_kind:?}"
    );
}

async fn assert_backend_describes_image_input(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let workspace_roots = fixture.workspace_roots();
    let images = vec![ImageData {
        media_type: "image/png".to_string(),
        data: SOLID_RED_PNG_BASE64.to_string(),
    }];
    let image_prompt =
        "Describe the attached image in one or two words. Reply with only the description.";
    let agent_stream = spawn_agent_via_protocol_with_images(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "image-input",
        image_prompt,
        Some(images.clone()),
    )
    .await;
    let response = expect_assistant_turn_after_user_echo_with_images(
        &mut fixture.client,
        &agent_stream,
        image_prompt,
        &images,
    )
    .await;
    let normalized = response.final_text.to_lowercase();
    assert!(
        normalized.contains("red"),
        "expected image description to mention red for {backend_kind:?}, got {:?}",
        response.final_text
    );
    assert!(
        response.delta_count > 0,
        "expected streamed image-description response for {backend_kind:?}"
    );
}

async fn assert_backend_returns_non_empty_name_for_name_prompt(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let workspace_roots = fixture.workspace_roots();
    let source_prompt = "review the auth logs for login regressions";
    let prompt = format!(
        "Return only a short 2-4 word work name for this request. No quotes, no markdown, no explanation. Request: {source_prompt}"
    );
    let agent_stream = spawn_agent_via_protocol_with_options(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "name-generator-probe",
        &prompt,
        None,
        Some(SpawnCostHint::Low),
    )
    .await;
    let response =
        expect_assistant_turn_after_user_echo(&mut fixture.client, &agent_stream, &prompt).await;
    let trimmed = response.final_text.trim();

    assert!(
        !trimmed.is_empty(),
        "expected non-empty name-generation response for {backend_kind:?}; delta_count={} response={:?}",
        response.delta_count,
        response.final_text
    );

    let word_count = trimmed.split_whitespace().count();
    assert!(
        (2..=4).contains(&word_count),
        "expected 2-4 words from name-generation response for {backend_kind:?}; got {:?}",
        response.final_text
    );
}

async fn expect_tool_turn_after_user_echo(
    client: &mut ValidatedConnection,
    agent_stream: &StreamPath,
    prompt: &str,
) -> ToolTurn {
    let mut got_user_message_echo = false;
    let mut final_text: Option<String> = None;
    let mut saw_stream_end = false;
    let mut streamed_text = String::new();
    let mut tool_requests: HashMap<String, ToolRequest> = HashMap::new();
    let mut tool_completions: HashMap<String, ToolExecutionCompletedData> = HashMap::new();

    loop {
        let env = expect_next_event(client, "tool-assisted ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != *agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User) && message.content == prompt {
                    got_user_message_echo = true;
                }
            }
            ChatEvent::StreamEnd(data) => {
                if !got_user_message_echo {
                    continue;
                }
                saw_stream_end = true;
                final_text = Some(if data.message.content.trim().is_empty() {
                    streamed_text.clone()
                } else {
                    data.message.content
                });
            }
            ChatEvent::StreamDelta(delta) => {
                if got_user_message_echo {
                    streamed_text.push_str(&delta.text);
                }
            }
            ChatEvent::ToolRequest(request) => {
                if got_user_message_echo {
                    tool_requests.insert(request.tool_call_id.clone(), request);
                }
            }
            ChatEvent::ToolExecutionCompleted(completion) if got_user_message_echo => {
                tool_completions.insert(completion.tool_call_id.clone(), completion);
            }
            _ => {}
        }

        if saw_stream_end
            && tool_requests
                .keys()
                .all(|call_id| tool_completions.contains_key(call_id))
        {
            return ToolTurn {
                final_text: final_text.unwrap_or_default(),
                tool_requests: tool_requests.into_values().collect(),
                tool_completions: tool_completions.into_values().collect(),
            };
        }
    }
}

async fn expect_tool_turn_until_output_exists(
    client: &mut ValidatedConnection,
    agent_stream: &StreamPath,
    prompt: &str,
    output_path: &std::path::Path,
) -> ToolTurn {
    let mut turn = expect_tool_turn_after_user_echo(client, agent_stream, prompt).await;
    if output_path.exists() {
        return turn;
    }

    loop {
        let maybe_env = tokio::time::timeout(Duration::from_secs(5), client.next_event()).await;
        let env = match maybe_env {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => panic!("next_event failed while waiting for file output: {err:?}"),
            Err(_) => break,
        };

        if env.kind != FrameKind::ChatEvent || env.stream != *agent_stream {
            continue;
        }

        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::StreamEnd(data) => {
                turn.final_text = data.message.content;
            }
            ChatEvent::ToolRequest(request) => {
                if !turn
                    .tool_requests
                    .iter()
                    .any(|existing| existing.tool_call_id == request.tool_call_id)
                {
                    turn.tool_requests.push(request);
                }
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                if let Some(existing) = turn
                    .tool_completions
                    .iter_mut()
                    .find(|existing| existing.tool_call_id == completion.tool_call_id)
                {
                    *existing = completion;
                } else {
                    turn.tool_completions.push(completion);
                }
            }
            _ => {}
        }

        if output_path.exists()
            && !turn.tool_requests.is_empty()
            && turn.tool_requests.iter().all(|request| {
                turn.tool_completions
                    .iter()
                    .any(|completion| completion.tool_call_id == request.tool_call_id)
            })
        {
            break;
        }
    }

    turn
}

async fn assert_backend_emits_tool_events_for_file_copy(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let input_contents = format!("TOOL-COPY-CONTENT-{}", unique_secret());
    let input_path = fixture.workspace_dir.path().join("INPUT.txt");
    let output_path = fixture.workspace_dir.path().join("OUTPUT.txt");
    let workspace_roots = fixture.workspace_roots();
    std::fs::write(&input_path, &input_contents).expect("seed input file");
    let _ = std::fs::remove_file(&output_path);

    let prompt = "Use the available tools to inspect INPUT.txt and create OUTPUT.txt in the workspace with exactly the same contents. Do not only describe a plan.";
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "tool-file-copy",
        prompt,
    )
    .await;
    let turn = expect_tool_turn_until_output_exists(
        &mut fixture.client,
        &agent_stream,
        prompt,
        &output_path,
    )
    .await;
    let mut turn = turn;

    for follow_up_prompt in [
        "You inspected INPUT.txt. Now actually create OUTPUT.txt in the workspace with exactly the same contents before you reply.",
        "Finish the task now: write OUTPUT.txt with the same contents as INPUT.txt, then confirm briefly.",
    ] {
        if output_path.exists() {
            break;
        }
        fixture
            .client
            .send_message(&agent_stream, follow_up_prompt.to_string())
            .await
            .expect("send tool follow-up message");
        let next_turn = expect_tool_turn_until_output_exists(
            &mut fixture.client,
            &agent_stream,
            follow_up_prompt,
            &output_path,
        )
        .await;
        turn.tool_requests.extend(next_turn.tool_requests);
        turn.tool_completions.extend(next_turn.tool_completions);
        if !next_turn.final_text.trim().is_empty() {
            turn.final_text = next_turn.final_text;
        }
    }

    assert!(
        !turn.tool_requests.is_empty(),
        "expected at least one ToolRequest for {backend_kind:?}"
    );
    assert_eq!(
        turn.tool_requests.len(),
        turn.tool_completions.len(),
        "expected a matching ToolExecutionCompleted for every ToolRequest for {backend_kind:?}"
    );
    assert!(
        turn.tool_completions
            .iter()
            .any(|completion| completion.success),
        "expected at least one successful ToolExecutionCompleted for {backend_kind:?}; requests={:?} completions={:?} final_text={:?}",
        turn.tool_requests,
        turn.tool_completions,
        turn.final_text
    );
    assert!(
        output_path.exists(),
        "expected OUTPUT.txt to exist after tool-assisted turn for {backend_kind:?}; requests={:?} completions={:?} final_text={:?}",
        turn.tool_requests,
        turn.tool_completions,
        turn.final_text
    );
    let output_contents = std::fs::read_to_string(&output_path).expect("read OUTPUT.txt");
    assert!(
        output_contents == input_contents || output_contents == format!("{input_contents}\n"),
        "expected OUTPUT.txt to match INPUT.txt for {backend_kind:?} (allowing one trailing newline); requests={:?} completions={:?} final_text={:?} output={:?} input={:?}",
        turn.tool_requests,
        turn.tool_completions,
        turn.final_text,
        output_contents,
        input_contents
    );
    // Live Codex may finish a tool-only turn without a final text message.
    // This test's contract is the tool lifecycle and file-copy result above.
}

async fn assert_backend_interrupts_long_running_command(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let workspace_roots = fixture.workspace_roots();
    let script_path = fixture.workspace_dir.path().join("interrupt_test.sh");
    let output_path = fixture.workspace_dir.path().join("INTERRUPT_PROOF.txt");
    let _ = std::fs::remove_file(&output_path);
    std::fs::write(
        &script_path,
        "#!/bin/sh\nsleep 60\nprintf cancelled-proof > INTERRUPT_PROOF.txt\n",
    )
    .expect("write interrupt_test.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&script_path)
            .expect("stat interrupt_test.sh")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).expect("chmod interrupt_test.sh");
    }

    let prompt = match backend_kind {
        BackendKind::Claude => {
            "Use the Bash tool exactly once to execute exactly `./interrupt_test.sh` in the current working directory. Start the command immediately. Do not use any other tools."
        }
        BackendKind::Codex => {
            "Immediately run the test harness `./interrupt_test.sh` in the current working directory using your command execution capability. Start it before any explanation, do not ask for confirmation, and do not use any other tools."
        }
        BackendKind::Antigravity => {
            "Execute exactly `./interrupt_test.sh` in the current working directory. Start the command immediately. Do not use any other tools."
        }
        BackendKind::Acp => {
            "Use the available tools to run the test harness `./interrupt_test.sh` in the current working directory immediately. Do not ask for clarification, and do not only describe a plan."
        }
        BackendKind::Tycode => {
            "Use the run_build_test tool exactly once to run the test harness `./interrupt_test.sh` in the current working directory. Start the test immediately. Do not use any other tools."
        }
        BackendKind::Hermes => {
            "Run the test harness `./interrupt_test.sh` in the current working directory immediately. Do not ask for clarification, and do not only describe a plan."
        }
    };
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "interrupt-long-command",
        prompt,
    )
    .await;

    let started_at = Instant::now();
    let mut got_user_message_echo = false;
    let tool_call_id = loop {
        let context = format!("{backend_kind:?} long-running ToolRequest");
        let env = expect_next_event(&mut fixture.client, &context).await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::MessageAdded(message) => {
                if matches!(message.sender, MessageSender::User) && message.content == prompt {
                    got_user_message_echo = true;
                }
            }
            ChatEvent::ToolRequest(request) if got_user_message_echo => {
                let ToolRequestType::RunCommand { command, .. } = &request.tool_type else {
                    continue;
                };
                if command.contains("interrupt_test.sh") {
                    break request.tool_call_id;
                }
            }
            _ => {}
        }
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    fixture
        .client
        .interrupt(&agent_stream)
        .await
        .expect("interrupt failed");

    let mut saw_operation_cancelled = false;
    let mut saw_typing_stopped = false;
    let mut saw_matching_tool_completion = false;
    let cancel_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < cancel_deadline {
        let context = format!("{backend_kind:?} interrupt outcome");
        let env = expect_next_event(&mut fixture.client, &context).await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::OperationCancelled(_) => {
                saw_operation_cancelled = true;
                if saw_typing_stopped {
                    break;
                }
            }
            ChatEvent::TypingStatusChanged(false) => {
                saw_typing_stopped = true;
                if saw_operation_cancelled {
                    break;
                }
            }
            ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == tool_call_id =>
            {
                saw_matching_tool_completion = true;
            }
            _ => {}
        }
    }

    assert!(
        saw_operation_cancelled,
        "expected OperationCancelled for {backend_kind:?} interrupt test"
    );
    assert!(
        saw_typing_stopped,
        "expected TypingStatusChanged(false) for {backend_kind:?} interrupt test"
    );
    assert!(
        started_at.elapsed() < Duration::from_secs(20),
        "interrupt test for {backend_kind:?} took too long: {:?}",
        started_at.elapsed()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !output_path.exists(),
        "expected interrupted command to avoid writing {:?} for {:?}; saw_tool_completion={}",
        output_path,
        backend_kind,
        saw_matching_tool_completion
    );

    let follow_up_prompt = "After the cancelled turn, reply with a short acknowledgement that you are ready for the next task.";
    fixture
        .client
        .send_message(&agent_stream, follow_up_prompt.to_string())
        .await
        .expect("send follow-up message after interrupt");
    let follow_up_turn =
        expect_assistant_turn_after_user_echo(&mut fixture.client, &agent_stream, follow_up_prompt)
            .await;
    assert!(
        !follow_up_turn.final_text.trim().is_empty(),
        "expected non-empty follow-up response after interrupt for {backend_kind:?}"
    );
}

// ---------------------------------------------------------------------------
// Real backend tests — opt-in because they can make real AI calls
// ---------------------------------------------------------------------------

const UNIVERSAL_REAL_BACKENDS_ENV: &str = "TYDE_REAL_BACKENDS";

fn universal_real_backends() -> Result<Vec<BackendKind>, String> {
    let configured = std::env::var(UNIVERSAL_REAL_BACKENDS_ENV)
        .unwrap_or_else(|_| "claude,codex,kiro,hermes".to_owned());
    let mut backends = Vec::new();
    for value in configured
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let backend = match value.to_ascii_lowercase().as_str() {
            "claude" => BackendKind::Claude,
            "codex" => BackendKind::Codex,
            "kiro" | "acp" => BackendKind::Acp,
            "hermes" => BackendKind::Hermes,
            "tycode" => BackendKind::Tycode,
            "antigravity" | "agy" => BackendKind::Antigravity,
            _ => {
                return Err(format!(
                    "unknown backend {value:?} in {UNIVERSAL_REAL_BACKENDS_ENV}"
                ));
            }
        };
        if !backends.contains(&backend) {
            backends.push(backend);
        }
    }
    if backends.is_empty() {
        return Err(format!(
            "{UNIVERSAL_REAL_BACKENDS_ENV} selected no backends"
        ));
    }
    Ok(backends)
}

fn universal_backend_config(backend_kind: BackendKind) -> server::backend::BackendSpawnConfig {
    let mut config = server::backend::BackendSpawnConfig {
        cost_hint: cost_hint_for(backend_kind),
        ..Default::default()
    };
    if backend_kind == BackendKind::Claude {
        let mut settings = SessionSettingsValues::default();
        settings.0.insert(
            "model".to_owned(),
            SessionSettingValue::String(UNIVERSAL_CLAUDE_MODEL.to_owned()),
        );
        settings.0.insert(
            "effort".to_owned(),
            SessionSettingValue::String(UNIVERSAL_CLAUDE_EFFORT.to_owned()),
        );
        config.session_settings = Some(settings);
    } else if backend_kind == BackendKind::Hermes {
        let provider = std::env::var("TYDE_HERMES_TEST_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HERMES_TEST_PROVIDER.to_owned());
        let model = std::env::var("TYDE_HERMES_TEST_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HERMES_TEST_MODEL.to_owned());
        let mut settings = SessionSettingsValues::default();
        settings.0.insert(
            "model".to_owned(),
            SessionSettingValue::String(format!("{model} --provider {provider}")),
        );
        settings.0.insert(
            "reasoning_effort".to_owned(),
            SessionSettingValue::String("none".to_owned()),
        );
        config.session_settings = Some(settings);
    } else if backend_kind == BackendKind::Codex {
        let mut settings = SessionSettingsValues::default();
        settings.0.insert(
            "model".to_owned(),
            SessionSettingValue::String(UNIVERSAL_CODEX_MODEL.to_owned()),
        );
        settings.0.insert(
            "reasoning_effort".to_owned(),
            SessionSettingValue::String(UNIVERSAL_CODEX_REASONING_EFFORT.to_owned()),
        );
        config.session_settings = Some(settings);
    }
    config
}

#[derive(Debug)]
struct DirectCertificationObservation {
    prompt: String,
    chat: Vec<ChatEvent>,
    request_usage: Vec<protocol::ModelRequestTokenUsage>,
}

impl DirectCertificationObservation {
    fn trace(&self) -> String {
        self.chat
            .iter()
            .enumerate()
            .map(|(index, event)| format!("{index}: {event:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn event_index(&self, predicate: impl Fn(&ChatEvent) -> bool) -> Option<usize> {
        self.chat.iter().position(predicate)
    }

    fn final_message(&self) -> ChatMessage {
        let mut message = self
            .chat
            .iter()
            .find_map(|event| match event {
                ChatEvent::StreamEnd(end) => Some(end.message.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing StreamEnd; trace:\n{}", self.trace()));
        for event in &self.chat {
            if let ChatEvent::MessageMetadataUpdated(update) = event
                && message.message_id.as_ref() == Some(&update.message_id)
            {
                if let Some(model_info) = &update.model_info {
                    message.model_info = Some(model_info.clone());
                }
                if let Some(token_usage) = &update.token_usage {
                    message.token_usage = Some(token_usage.clone());
                }
                if let Some(context_breakdown) = &update.context_breakdown {
                    message.context_breakdown = Some(context_breakdown.clone());
                }
            }
        }
        message
    }

    fn streamed_text(&self) -> String {
        self.chat
            .iter()
            .filter_map(|event| match event {
                ChatEvent::StreamDelta(delta) => Some(delta.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

async fn collect_direct_certification_observation<B: Backend>(
    backend_kind: BackendKind,
    prompt: &str,
) -> DirectCertificationObservation {
    let workspace = tempfile::tempdir().expect("create direct certification workspace");
    std::fs::write(
        workspace.path().join("README.txt"),
        "direct certification workspace",
    )
    .expect("seed direct certification workspace");
    let (backend, mut events) = B::spawn(
        vec![workspace.path().to_string_lossy().to_string()],
        universal_backend_config(backend_kind),
        protocol::SendMessagePayload {
            message: prompt.to_owned(),
            images: None,
            origin: None,
            tool_response: None,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{} spawn failed: {error}", backend_label(backend_kind)));
    let mut observation = DirectCertificationObservation {
        prompt: prompt.to_owned(),
        chat: Vec::new(),
        request_usage: Vec::new(),
    };
    tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = events.recv_observation().await {
            match event {
                BackendObservation::Chat(event) => {
                    let terminal = matches!(event, ChatEvent::TypingStatusChanged(false))
                        && observation
                            .chat
                            .iter()
                            .any(|event| matches!(event, ChatEvent::StreamEnd(_)));
                    observation.chat.push(event);
                    if terminal {
                        break;
                    }
                }
                BackendObservation::ModelRequestTokenUsage(usage) => {
                    observation.request_usage.push(usage);
                }
                BackendObservation::Other => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} direct case timed out", backend_label(backend_kind)));
    backend.shutdown().await;
    observation
}

async fn collect_direct_case(backend_kind: BackendKind) -> DirectCertificationObservation {
    const PROMPT: &str = "Reply with exactly CERTIFICATION_OK and nothing else.";
    match backend_kind {
        BackendKind::Claude => {
            collect_direct_certification_observation::<server::backend::claude::ClaudeBackend>(
                backend_kind,
                PROMPT,
            )
            .await
        }
        BackendKind::Codex => {
            collect_direct_certification_observation::<server::backend::codex::CodexBackend>(
                backend_kind,
                PROMPT,
            )
            .await
        }
        BackendKind::Acp => {
            collect_direct_certification_observation::<server::backend::kiro::KiroBackend>(
                backend_kind,
                PROMPT,
            )
            .await
        }
        BackendKind::Hermes => {
            collect_direct_certification_observation::<server::backend::hermes::HermesBackend>(
                backend_kind,
                PROMPT,
            )
            .await
        }
        BackendKind::Tycode => {
            collect_direct_certification_observation::<server::backend::tycode::TycodeBackend>(
                backend_kind,
                PROMPT,
            )
            .await
        }
        BackendKind::Antigravity => {
            collect_direct_certification_observation::<
                server::backend::antigravity::AntigravityBackend,
            >(backend_kind, PROMPT)
            .await
        }
    }
}

fn known_turn_usage(observation: &DirectCertificationObservation) -> TokenUsage {
    let message = observation.final_message();
    let usage = message.token_usage.unwrap_or_else(|| {
        panic!(
            "missing message token usage; trace:\n{}",
            observation.trace()
        )
    });
    let TokenUsageScope::Known { usage } = usage.turn else {
        panic!("turn usage was not reported as known: {usage:?}");
    };
    *usage
}

fn assert_direct_certification_case(
    backend_kind: BackendKind,
    case: CertificationCase,
    observation: &DirectCertificationObservation,
) {
    let trace = observation.trace();
    match case {
        CertificationCase::InitialInputEchoedOnce => {
            let count = observation
                .chat
                .iter()
                .filter(|event| {
                    matches!(event, ChatEvent::MessageAdded(message)
                        if matches!(message.sender, MessageSender::User)
                            && message.content == observation.prompt)
                })
                .count();
            assert_eq!(count, 1, "user echo count was {count}; trace:\n{trace}");
        }
        CertificationCase::TypingStarts => assert!(
            observation
                .chat
                .iter()
                .any(|event| matches!(event, ChatEvent::TypingStatusChanged(true))),
            "missing typing true; trace:\n{trace}"
        ),
        CertificationCase::TypingStartsOnce => assert_eq!(
            observation
                .chat
                .iter()
                .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(true)))
                .count(),
            1,
            "typing true was duplicated; trace:\n{trace}"
        ),
        CertificationCase::StreamStarts => assert!(
            observation
                .chat
                .iter()
                .any(|event| matches!(event, ChatEvent::StreamStart(_))),
            "missing StreamStart; trace:\n{trace}"
        ),
        CertificationCase::VisibleDeltaEmitted => assert!(
            observation.chat.iter().any(
                |event| matches!(event, ChatEvent::StreamDelta(delta) if !delta.text.is_empty())
            ),
            "missing visible delta; trace:\n{trace}"
        ),
        CertificationCase::StreamEnds => assert!(
            observation
                .chat
                .iter()
                .any(|event| matches!(event, ChatEvent::StreamEnd(_))),
            "missing StreamEnd; trace:\n{trace}"
        ),
        CertificationCase::StreamStartsOnce => assert_eq!(
            observation
                .chat
                .iter()
                .filter(|event| matches!(event, ChatEvent::StreamStart(_)))
                .count(),
            1,
            "StreamStart was duplicated; trace:\n{trace}"
        ),
        CertificationCase::StreamEndsOnce => assert_eq!(
            observation
                .chat
                .iter()
                .filter(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .count(),
            1,
            "StreamEnd was duplicated; trace:\n{trace}"
        ),
        CertificationCase::TypingStopsOnce => assert_eq!(
            observation
                .chat
                .iter()
                .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
                .count(),
            1,
            "typing false was duplicated; trace:\n{trace}"
        ),
        CertificationCase::NoErrorOnSuccessfulTurn => assert!(
            !observation.chat.iter().any(|event| matches!(
                event,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    ..
                })
            )),
            "successful turn emitted an error; trace:\n{trace}"
        ),
        CertificationCase::TypingStopsAfterStreamEnd => {
            let end = observation
                .event_index(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .unwrap_or_else(|| panic!("missing StreamEnd; trace:\n{trace}"));
            let idle = observation
                .event_index(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
                .unwrap_or_else(|| panic!("missing typing false; trace:\n{trace}"));
            assert!(
                idle > end,
                "typing stopped before StreamEnd; trace:\n{trace}"
            );
        }
        CertificationCase::LifecycleOrderIsValid => {
            let typing = observation
                .event_index(|event| matches!(event, ChatEvent::TypingStatusChanged(true)))
                .unwrap_or_else(|| panic!("missing typing true; trace:\n{trace}"));
            let start = observation
                .event_index(|event| matches!(event, ChatEvent::StreamStart(_)))
                .unwrap_or_else(|| panic!("missing StreamStart; trace:\n{trace}"));
            let end = observation
                .event_index(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .unwrap_or_else(|| panic!("missing StreamEnd; trace:\n{trace}"));
            let idle = observation
                .event_index(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
                .unwrap_or_else(|| panic!("missing typing false; trace:\n{trace}"));
            assert!(
                typing < start && start < end && end < idle,
                "invalid lifecycle order; trace:\n{trace}"
            );
        }
        CertificationCase::StreamIdentityIsStable => {
            let start_id = observation.chat.iter().find_map(|event| match event {
                ChatEvent::StreamStart(start) => start.message_id.as_ref(),
                _ => None,
            });
            let final_message = observation.final_message();
            assert_eq!(
                start_id,
                final_message.message_id.as_ref().map(|id| &id.0),
                "stream identity changed; trace:\n{trace}"
            );
        }
        CertificationCase::DeltasReconstructFinalMessage => {
            let streamed = observation.streamed_text();
            let final_message = observation.final_message();
            assert_eq!(
                streamed, final_message.content,
                "deltas did not reconstruct final message; trace:\n{trace}"
            );
        }
        CertificationCase::ExactResponseDelivered => assert_eq!(
            observation.final_message().content.trim(),
            "CERTIFICATION_OK",
            "unexpected response from {}",
            backend_label(backend_kind)
        ),
        CertificationCase::TurnUsagePresent => {
            let _ = known_turn_usage(observation);
        }
        CertificationCase::TurnInputTokensPositive => assert!(
            known_turn_usage(observation).input_tokens > 0,
            "reported zero input tokens"
        ),
        CertificationCase::TurnOutputTokensPositive => assert!(
            known_turn_usage(observation).output_tokens > 0,
            "reported zero output tokens"
        ),
        CertificationCase::TurnTotalConsistent => {
            assert_token_usage_sane("direct certification turn", &known_turn_usage(observation));
        }
        CertificationCase::RequestUsagePresent => assert!(
            !observation.request_usage.is_empty(),
            "missing provider-request usage; trace:\n{trace}"
        ),
        CertificationCase::RequestSequenceStartsAtOne => assert_eq!(
            observation
                .request_usage
                .first()
                .unwrap_or_else(|| panic!("missing request usage; trace:\n{trace}"))
                .request_id
                .sequence,
            1
        ),
        CertificationCase::RequestSequencesContiguous => {
            for (index, usage) in observation.request_usage.iter().enumerate() {
                assert_eq!(usage.request_id.sequence as usize, index + 1);
            }
        }
        CertificationCase::RequestUsageMatchesTurn => {
            let turn_id = &observation
                .request_usage
                .first()
                .unwrap_or_else(|| panic!("missing request usage; trace:\n{trace}"))
                .request_id
                .turn_id;
            assert!(
                observation
                    .request_usage
                    .iter()
                    .all(|usage| &usage.request_id.turn_id == turn_id),
                "request usage crossed turn identities"
            );
        }
        CertificationCase::RequestIdsUnique => {
            let ids = observation
                .request_usage
                .iter()
                .map(|usage| &usage.request_id)
                .collect::<HashSet<_>>();
            assert_eq!(ids.len(), observation.request_usage.len());
        }
        CertificationCase::RequestUsagePositive => assert!(
            observation.request_usage.iter().all(|usage| {
                usage.request.input_tokens > 0
                    && usage.request.output_tokens > 0
                    && usage.request.total_tokens > 0
            }),
            "provider request reported zero usage: {:?}",
            observation.request_usage
        ),
        CertificationCase::ContextUsagePresent => assert!(
            observation.request_usage.iter().any(|usage| {
                usage
                    .current_context_usage
                    .as_ref()
                    .is_some_and(|context| context.known().is_some())
                    || usage.model_context_window.is_some()
            }) || observation.final_message().context_breakdown.is_some(),
            "missing context usage; trace:\n{trace}"
        ),
        CertificationCase::ContextWindowValid => {
            let contexts = observation
                .request_usage
                .iter()
                .filter_map(|usage| usage.current_context_usage.as_ref()?.known())
                .collect::<Vec<_>>();
            if contexts.is_empty() {
                let final_message = observation.final_message();
                let breakdown = final_message
                    .context_breakdown
                    .as_ref()
                    .expect("missing known context usage");
                assert!(breakdown.context_window > 0);
                assert!(breakdown.input_tokens <= breakdown.context_window);
            } else {
                assert!(
                    contexts
                        .iter()
                        .all(|(input, window)| *window > 0 && input <= window)
                );
            }
        }
        _ => panic!("{} is not a direct single-turn case", case.id()),
    }
}

fn is_direct_certification_case(case: CertificationCase) -> bool {
    matches!(
        case,
        CertificationCase::InitialInputEchoedOnce
            | CertificationCase::TypingStarts
            | CertificationCase::TypingStartsOnce
            | CertificationCase::StreamStarts
            | CertificationCase::VisibleDeltaEmitted
            | CertificationCase::StreamEnds
            | CertificationCase::StreamStartsOnce
            | CertificationCase::StreamEndsOnce
            | CertificationCase::TypingStopsOnce
            | CertificationCase::NoErrorOnSuccessfulTurn
            | CertificationCase::TypingStopsAfterStreamEnd
            | CertificationCase::LifecycleOrderIsValid
            | CertificationCase::StreamIdentityIsStable
            | CertificationCase::DeltasReconstructFinalMessage
            | CertificationCase::ExactResponseDelivered
            | CertificationCase::TurnUsagePresent
            | CertificationCase::TurnInputTokensPositive
            | CertificationCase::TurnOutputTokensPositive
            | CertificationCase::TurnTotalConsistent
            | CertificationCase::RequestUsagePresent
            | CertificationCase::RequestSequenceStartsAtOne
            | CertificationCase::RequestSequencesContiguous
            | CertificationCase::RequestUsageMatchesTurn
            | CertificationCase::RequestIdsUnique
            | CertificationCase::RequestUsagePositive
            | CertificationCase::ContextUsagePresent
            | CertificationCase::ContextWindowValid
    )
}

async fn assert_backend_reports_tool_failure(
    fixture: &mut RealBackendFixture,
    backend_kind: BackendKind,
) {
    let script = fixture.workspace_dir.path().join("failure_test.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 37\n").expect("write failure_test.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script)
            .expect("stat failure_test.sh")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod failure_test.sh");
    }
    let prompt = "Use the command execution tool exactly once to run `./failure_test.sh` in the current working directory. Do not use any other tool. After it fails, reply TOOL_FAILURE_OBSERVED.";
    let workspace_roots = fixture.workspace_roots();
    let stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "tool-failure-certification",
        prompt,
    )
    .await;
    let turn = expect_tool_turn_after_user_echo(&mut fixture.client, &stream, prompt).await;
    assert!(
        turn.tool_completions
            .iter()
            .any(|completion| !completion.success),
        "{} did not report a failed completion: {:?}",
        backend_label(backend_kind),
        turn.tool_completions
    );
}

async fn assert_workspace_instructions_case(backend_kind: BackendKind) {
    const SENTINEL: &str = "WORKSPACE_INSTRUCTIONS_OK";
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    std::fs::write(
        fixture.workspace_dir.path().join("AGENTS.md"),
        format!("When asked for the workspace sentinel, reply exactly `{SENTINEL}`."),
    )
    .expect("write AGENTS.md");
    let prompt = "Reply with exactly the workspace sentinel required by AGENTS.md.";
    let roots = fixture.workspace_roots();
    let stream = spawn_agent_via_protocol(
        &mut fixture.client,
        roots,
        backend_kind,
        "workspace-instructions-case",
        prompt,
    )
    .await;
    let turn = expect_assistant_turn_after_user_echo(&mut fixture.client, &stream, prompt).await;
    assert_eq!(turn.final_text.trim(), SENTINEL);
}

async fn assert_host_steering_case(backend_kind: BackendKind) {
    const SENTINEL: &str = "HOST_STEERING_OK";
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    fixture
        .client
        .inner
        .steering_upsert(protocol::SteeringUpsertPayload {
            steering: protocol::Steering {
                id: protocol::SteeringId("live-host-steering-case".to_owned()),
                scope: protocol::SteeringScope::Host,
                title: "Live host steering case".to_owned(),
                content: format!(
                    "When asked for the steering sentinel, reply exactly `{SENTINEL}`."
                ),
            },
        })
        .await
        .expect("install host steering");
    let prompt = "Reply with exactly the sentinel required by Tyde steering.";
    let roots = fixture.workspace_roots();
    let stream = spawn_agent_via_protocol(
        &mut fixture.client,
        roots,
        backend_kind,
        "host-steering-case",
        prompt,
    )
    .await;
    let turn = expect_assistant_turn_after_user_echo(&mut fixture.client, &stream, prompt).await;
    assert_eq!(turn.final_text.trim(), SENTINEL);
}

async fn observe_skill_case(backend_kind: BackendKind) -> AssistantTurn {
    const SENTINEL: &str = "SKILL_DELIVERY_OK";
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let skill_name = format!("live-certification-skill-{}", Uuid::new_v4().simple());
    let skill_dir = fixture
        .session_store_dir
        .path()
        .join("skills")
        .join(&skill_name);
    std::fs::create_dir_all(&skill_dir).expect("create skill directory");
    let skill = protocol::Skill {
        id: protocol::SkillId(skill_name.clone()),
        name: skill_name.clone(),
        title: Some("Live certification skill".to_owned()),
        description: Some("Returns the live skill sentinel".to_owned()),
    };
    std::fs::write(
        skill_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&skill).expect("serialize skill metadata"),
    )
    .expect("write skill metadata");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("When invoked, reply exactly `{SENTINEL}`."),
    )
    .expect("write skill body");
    fixture
        .client
        .inner
        .skill_refresh(protocol::SkillRefreshPayload {})
        .await
        .expect("refresh skills");
    let prompt = match backend_kind {
        BackendKind::Codex => format!(
            "Use ${skill_name} now. Follow its instructions exactly and output only what it requires."
        ),
        _ => format!("Invoke the installed skill named {skill_name} and follow it exactly."),
    };
    let roots = fixture.workspace_roots();
    let stream = spawn_agent_via_protocol_with_options(
        &mut fixture.client,
        roots,
        backend_kind,
        "skill-delivery-case",
        &prompt,
        None,
        cost_hint_for(backend_kind),
    )
    .await;
    expect_assistant_turn_after_user_echo(&mut fixture.client, &stream, &prompt).await
}

async fn observe_mcp_case(backend_kind: BackendKind) -> ToolTurn {
    const TOOL_NAME: &str = "narrow_probe";
    const SENTINEL: &str = "NARROW_MCP_OK";
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let script = fixture.workspace_dir.path().join("narrow_mcp_probe.py");
    std::fs::write(
        &script,
        r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    request_id = request.get("id")
    if request_id is None:
        continue
    if request.get("method") == "initialize":
        result = {"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"narrow-certification","version":"1"}}
    elif request.get("method") == "tools/list":
        result = {"tools":[{"name":"narrow_probe","description":"Return NARROW_MCP_OK","inputSchema":{"type":"object","properties":{},"additionalProperties":False}}]}
    elif request.get("method") == "tools/call":
        result = {"content":[{"type":"text","text":"NARROW_MCP_OK"}],"isError":False}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":request_id,"result":result}), flush=True)
"#,
    )
    .expect("write narrow MCP server");
    fixture
        .client
        .inner
        .mcp_server_upsert(protocol::McpServerUpsertPayload {
            mcp_server: protocol::McpServerConfig {
                id: protocol::McpServerId("narrow-live-mcp".to_owned()),
                name: "narrow_certification".to_owned(),
                transport: protocol::McpTransportConfig::Stdio {
                    command: "python3".to_owned(),
                    args: vec![script.to_string_lossy().to_string()],
                    env: HashMap::new(),
                },
            },
        })
        .await
        .expect("install narrow MCP server");
    let prompt = "Call the MCP tool whose description says Return NARROW_MCP_OK, then reply with its exact result.";
    let roots = fixture.workspace_roots();
    let stream = spawn_agent_via_protocol(
        &mut fixture.client,
        roots,
        backend_kind,
        "narrow-mcp-case",
        prompt,
    )
    .await;
    let mut turn = expect_tool_turn_after_user_echo(&mut fixture.client, &stream, prompt).await;
    let complete = |turn: &ToolTurn| {
        turn.tool_requests.iter().any(|request| {
            request.tool_name.to_ascii_lowercase().contains(TOOL_NAME)
                && turn.tool_completions.iter().any(|completion| {
                    completion.tool_call_id == request.tool_call_id && completion.success
                })
        }) && turn.final_text.contains(SENTINEL)
    };
    let mut streamed_text = String::new();
    while !complete(&turn) {
        let env = expect_next_event(&mut fixture.client, "completed narrow MCP turn").await;
        if env.kind != FrameKind::ChatEvent || env.stream != stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse narrow MCP ChatEvent");
        match event {
            ChatEvent::StreamStart(_) => streamed_text.clear(),
            ChatEvent::StreamDelta(delta) => streamed_text.push_str(&delta.text),
            ChatEvent::StreamEnd(data) => {
                turn.final_text = if data.message.content.trim().is_empty() {
                    streamed_text.clone()
                } else {
                    data.message.content
                };
            }
            ChatEvent::ToolRequest(request) => {
                if !turn
                    .tool_requests
                    .iter()
                    .any(|existing| existing.tool_call_id == request.tool_call_id)
                {
                    turn.tool_requests.push(request);
                }
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                if let Some(existing) = turn
                    .tool_completions
                    .iter_mut()
                    .find(|existing| existing.tool_call_id == completion.tool_call_id)
                {
                    *existing = completion;
                } else {
                    turn.tool_completions.push(completion);
                }
            }
            ChatEvent::MessageAdded(ChatMessage {
                sender: MessageSender::Error,
                content,
                ..
            }) => panic!("narrow MCP turn failed: {content}"),
            ChatEvent::TypingStatusChanged(false) => {
                panic!("narrow MCP turn became idle before delivering its tool result: {turn:?}")
            }
            _ => {}
        }
    }
    turn
}

async fn collect_backend_text_until_idle(events: &mut server::backend::EventStream) -> String {
    let mut streamed = String::new();
    let mut final_text = String::new();
    let mut saw_end = false;
    tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = events.recv().await {
            match event {
                ChatEvent::StreamDelta(delta) => streamed.push_str(&delta.text),
                ChatEvent::StreamEnd(end) => {
                    saw_end = true;
                    final_text = end.message.content;
                }
                ChatEvent::TypingStatusChanged(false) if saw_end => break,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("backend fork case failed: {content}"),
                _ => {}
            }
        }
    })
    .await
    .expect("backend fork turn timed out");
    if final_text.trim().is_empty() {
        streamed
    } else {
        final_text
    }
}

async fn assert_backend_fork_case<B: Backend>(backend_kind: BackendKind) {
    let workspace = tempfile::tempdir().expect("fork workspace");
    let root = workspace.path().to_string_lossy().to_string();
    let identifier = unique_project_identifier();
    let first_prompt =
        format!("The project identifier is {identifier}. Reply exactly PROJECT_IDENTIFIER_STORED.");
    let (backend, mut events) = B::spawn(
        vec![root.clone()],
        universal_backend_config(backend_kind),
        protocol::SendMessagePayload {
            message: first_prompt,
            images: None,
            origin: None,
            tool_response: None,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("initial fork source spawn failed: {error}"));
    let source_session = backend.session_id();
    let initial_text = collect_backend_text_until_idle(&mut events).await;
    assert!(initial_text.contains("PROJECT_IDENTIFIER_STORED"));
    backend.shutdown().await;

    let fork_prompt = "State the project identifier, followed by FORK_OK.";
    let (fork, mut fork_events) = B::fork(
        vec![root],
        universal_backend_config(backend_kind),
        source_session.clone(),
        protocol::SendMessagePayload {
            message: fork_prompt.to_owned(),
            images: None,
            origin: None,
            tool_response: None,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("fork failed: {error:?}"));
    let fork_session = fork.session_id();
    let fork_text = collect_backend_text_until_idle(&mut fork_events).await;
    fork.shutdown().await;
    assert_ne!(
        fork_session, source_session,
        "fork reused source session id"
    );
    assert!(
        fork_text.contains(&identifier),
        "fork lost source history: {fork_text:?}"
    );
    assert!(
        fork_text.contains("FORK_OK"),
        "fork did not accept initial prompt: {fork_text:?}"
    );
}

async fn assert_fork_case(backend_kind: BackendKind) {
    match backend_kind {
        BackendKind::Claude => {
            assert_backend_fork_case::<server::backend::claude::ClaudeBackend>(backend_kind).await
        }
        BackendKind::Codex => {
            assert_backend_fork_case::<server::backend::codex::CodexBackend>(backend_kind).await
        }
        BackendKind::Acp => {
            assert_backend_fork_case::<server::backend::kiro::KiroBackend>(backend_kind).await
        }
        BackendKind::Hermes => {
            assert_backend_fork_case::<server::backend::hermes::HermesBackend>(backend_kind).await
        }
        BackendKind::Tycode => {
            assert_backend_fork_case::<server::backend::tycode::TycodeBackend>(backend_kind).await
        }
        BackendKind::Antigravity => {
            assert_backend_fork_case::<server::backend::antigravity::AntigravityBackend>(
                backend_kind,
            )
            .await
        }
    }
}

async fn run_certification_case_for_backend(backend_kind: BackendKind, case: CertificationCase) {
    if is_direct_certification_case(case) {
        let observation = collect_direct_case(backend_kind).await;
        assert_direct_certification_case(backend_kind, case, &observation);
        return;
    }

    match case {
        CertificationCase::FollowUpCompletes
        | CertificationCase::FollowUpInputEchoedOnce
        | CertificationCase::FollowUpUsesDistinctMessage => {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_emits_typing_and_streaming_on_follow_up_turns(
                &mut fixture,
                backend_kind,
            )
            .await;
            if case == CertificationCase::FollowUpInputEchoedOnce {
                assert_backend_follow_up_user_echo_not_duplicated(&mut fixture, backend_kind).await;
            }
        }
        CertificationCase::CumulativeUsageGrows => {
            assert_backend_reports_cumulative_turn_token_usage(backend_kind).await;
        }
        CertificationCase::ToolRequestEmitted
        | CertificationCase::ToolCompletionEmitted
        | CertificationCase::ToolCallIdsCorrelate
        | CertificationCase::ToolChangesWorkspace
        | CertificationCase::ToolTurnReachesIdle => {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_emits_tool_events_for_file_copy(&mut fixture, backend_kind).await;
        }
        CertificationCase::ToolFailureReported => {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_reports_tool_failure(&mut fixture, backend_kind).await;
        }
        CertificationCase::InterruptEmitsCancellation
        | CertificationCase::InterruptReturnsIdle
        | CertificationCase::InterruptStopsCommand
        | CertificationCase::FollowUpAfterInterrupt => {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_interrupts_long_running_command(&mut fixture, backend_kind).await;
        }
        CertificationCase::SessionAppearsInList
        | CertificationCase::ResumeRemembersHistory
        | CertificationCase::ResumeAcceptsFollowUp
        | CertificationCase::ResumePreservesWorkspace => {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            resume_secret_via_protocol(&mut fixture, backend_kind).await;
        }
        CertificationCase::ForkCreatesDistinctSession
        | CertificationCase::ForkPreservesHistory
        | CertificationCase::ForkAcceptsInitialPrompt => {
            assert_fork_case(backend_kind).await;
        }
        CertificationCase::WorkspaceInstructionsObserved => {
            assert_workspace_instructions_case(backend_kind).await;
        }
        CertificationCase::HostSteeringObserved => {
            assert_host_steering_case(backend_kind).await;
        }
        CertificationCase::SkillDiscovered | CertificationCase::SkillResultDelivered => {
            let turn = observe_skill_case(backend_kind).await;
            assert!(
                turn.final_text.contains("SKILL_DELIVERY_OK"),
                "skill response did not deliver the skill's sentinel: {:?}",
                turn.final_text,
            );
            if case == CertificationCase::SkillDiscovered {
                assert!(turn.delta_count > 0, "skill response did not stream");
            }
        }
        CertificationCase::McpToolDiscovered
        | CertificationCase::McpToolCalled
        | CertificationCase::McpResultDelivered
        | CertificationCase::McpEventsCorrelate => {
            let turn = observe_mcp_case(backend_kind).await;
            match case {
                CertificationCase::McpToolDiscovered | CertificationCase::McpToolCalled => {
                    assert!(turn.tool_requests.iter().any(|request| {
                        request
                            .tool_name
                            .to_ascii_lowercase()
                            .contains("narrow_probe")
                    }));
                }
                CertificationCase::McpResultDelivered => {
                    assert!(turn.final_text.contains("NARROW_MCP_OK"));
                }
                CertificationCase::McpEventsCorrelate => {
                    assert!(turn.tool_requests.iter().all(|request| {
                        turn.tool_completions
                            .iter()
                            .any(|completion| completion.tool_call_id == request.tool_call_id)
                    }));
                }
                _ => unreachable!(),
            }
        }
        CertificationCase::ImageEchoPreserved
        | CertificationCase::ImageUnderstood
        | CertificationCase::ImageResponseStreams => {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_describes_image_input(&mut fixture, backend_kind).await;
        }
        CertificationCase::NativeSubagentProjected
        | CertificationCase::NativeSubagentParentLinked => {
            assert_backend_native_subagent_visibility(backend_kind).await;
        }
        CertificationCase::BackgroundWorkKeepsAgentActive => {
            assert_eq!(backend_kind, BackendKind::Claude);
            assert_claude_agent_initiated_background_resume().await;
        }
        CertificationCase::BackgroundCompletionResumesParent
        | CertificationCase::AgentInitiatedTurnIsDistinct
        | CertificationCase::AgentInitiatedResultDelivered => match backend_kind {
            BackendKind::Claude => assert_claude_agent_initiated_background_resume().await,
            BackendKind::Codex => assert_codex_background_completion_resumes_parent().await,
            _ => unreachable!("background completion resume is not certified for {backend_kind:?}"),
        },
        CertificationCase::BackgroundCompletionReleasesAgent => {
            assert_eq!(backend_kind, BackendKind::Claude);
            assert_claude_background_command_releases_activity().await;
        }
        _ => unreachable!("direct cases returned before match: {}", case.id()),
    }
}

fn backend_supports_certification_case(
    backend_kind: BackendKind,
    capabilities: &tyde_agent_adapter::BackendCapabilities,
    case: CertificationCase,
) -> bool {
    if matches!(
        case,
        CertificationCase::BackgroundWorkKeepsAgentActive
            | CertificationCase::BackgroundCompletionReleasesAgent
            | CertificationCase::BackgroundCompletionResumesParent
            | CertificationCase::AgentInitiatedTurnIsDistinct
            | CertificationCase::AgentInitiatedResultDelivered
    ) {
        let supported_backend = match case {
            CertificationCase::BackgroundCompletionResumesParent
            | CertificationCase::AgentInitiatedTurnIsDistinct
            | CertificationCase::AgentInitiatedResultDelivered => {
                matches!(backend_kind, BackendKind::Claude | BackendKind::Codex)
            }
            _ => backend_kind == BackendKind::Claude,
        };
        return supported_backend
            && case
                .required_capability()
                .is_none_or(|capability| capabilities.contains(capability));
    }
    case.required_capability()
        .is_none_or(|capability| capabilities.contains(capability))
}

async fn run_selected_certification_case(case: CertificationCase) {
    assert!(
        real_ai_tests_enabled(),
        "set {RUN_REAL_AI_TESTS_ENV}=1 to authorize paid backend qualification"
    );
    let backends = universal_real_backends().expect("parse backend selection");
    let _hermes_python_guard = backends
        .contains(&BackendKind::Hermes)
        .then(|| {
            std::env::var("HERMES_PYTHON")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    Path::new(DEFAULT_HERMES_TEST_PYTHON)
                        .exists()
                        .then(|| DEFAULT_HERMES_TEST_PYTHON.to_owned())
                })
        })
        .flatten()
        .map(|python| EnvVarGuard::set("HERMES_PYTHON", python));
    let mut failures = Vec::new();
    let mut executions = 0usize;
    for backend_kind in backends {
        if !backend_binary_available(backend_kind) || !backend_runtime_available(backend_kind) {
            failures.push(format!(
                "{}: selected backend is not runnable",
                backend_label(backend_kind)
            ));
            continue;
        }
        let capabilities = server::backend::capabilities_for_backend_kind(backend_kind);
        if !backend_supports_certification_case(backend_kind, &capabilities, case) {
            eprintln!(
                "SKIPPED {} {}: case is not certified for backend capabilities",
                backend_label(backend_kind),
                case.id()
            );
            continue;
        }
        executions += 1;
        eprintln!("RUNNING {} {}", backend_label(backend_kind), case.id());
        let handle = tokio::spawn(run_certification_case_for_backend(backend_kind, case));
        if let Err(error) = handle.await {
            failures.push(format!(
                "{} {}: {error}",
                backend_label(backend_kind),
                case.id()
            ));
        }
    }
    assert!(
        executions > 0 || failures.is_empty(),
        "no selected backend ran"
    );
    assert!(
        failures.is_empty(),
        "backend certification case {} failed:\n{}",
        case.id(),
        failures.join("\n")
    );
}

async fn assert_backend_native_subagent_visibility(backend_kind: BackendKind) {
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    let prompt = match backend_kind {
        BackendKind::Claude => {
            "Use the Agent tool exactly once to ask a general-purpose subagent to read README.txt and return its first line. Wait for it, then reply SUBAGENT_OK."
        }
        BackendKind::Codex => {
            "Spawn exactly one native subagent to read README.txt and return its first line. Wait for it, then reply SUBAGENT_OK."
        }
        BackendKind::Hermes => {
            "Delegate to exactly one native subagent: have it read README.txt and return its first line. Wait for it, then reply SUBAGENT_OK."
        }
        _ => {
            "Use exactly one native subagent to read README.txt and return its first line. Wait for it, then reply SUBAGENT_OK."
        }
    };

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("universal-native-subagent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots,
                prompt: prompt.to_owned(),
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: cost_hint_for(backend_kind),
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn universal native-subagent parent");
    let env = expect_next_event_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "native-subagent parent NewAgent",
    )
    .await;
    let parent: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    let child = expect_subagent_child_for_parent(
        &mut fixture.client,
        &parent.agent_id,
        "universal backend-native child",
    )
    .await;
    assert!(matches!(
        child.origin,
        AgentOrigin::BackendNative | AgentOrigin::AgentControl
    ));
    assert_eq!(child.parent_agent_id.as_ref(), Some(&parent.agent_id));
    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "universal backend-native child AgentStart",
    )
    .await;
    assert_eq!(child_start.parent_agent_id.as_ref(), Some(&parent.agent_id));
}

async fn assert_claude_agent_initiated_background_resume() {
    const SENTINEL: &str = "BACKGROUND_RESUME_637";
    let backend_kind = BackendKind::Claude;
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    let prompt = "Use the Agent tool to launch a general-purpose subagent with run_in_background=true. Its only job is to compute 419 + 218 and return the number. End the initial parent turn while it works. When its completion triggers the parent to resume, reply exactly BACKGROUND_RESUME_637.";
    let stream = spawn_agent_via_protocol_with_options(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "universal-agent-initiated-resume",
        prompt,
        None,
        cost_hint_for(backend_kind),
    )
    .await;

    let mut completed_turns = 0usize;
    let mut background_running = false;
    let mut saw_idle_while_background_running = false;
    let mut saw_result = false;
    tokio::time::timeout(Duration::from_secs(240), async {
        loop {
            let env = fixture
                .client
                .next_event()
                .await
                .expect("read Claude background continuation")
                .expect("Claude background continuation stream closed");
            if env.kind != FrameKind::ChatEvent || env.stream != stream {
                continue;
            }
            let event: ChatEvent = env.parse_payload().expect("parse background ChatEvent");
            match event {
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("Claude background continuation failed: {content}"),
                ChatEvent::StreamEnd(end) => {
                    completed_turns += 1;
                    saw_result |= end.message.content.contains(SENTINEL);
                }
                ChatEvent::StreamDelta(delta) => {
                    saw_result |= delta.text.contains(SENTINEL);
                }
                ChatEvent::ToolProgress(progress) => {
                    if let protocol::ToolProgressUpdate::SubAgent(subagent) = progress.update {
                        background_running = !subagent.completed;
                    }
                }
                ChatEvent::TypingStatusChanged(false) if background_running => {
                    saw_idle_while_background_running = true;
                }
                ChatEvent::TypingStatusChanged(false) if saw_result => break,
                _ => {}
            }
        }
    })
    .await
    .expect("Claude never resumed after its background subagent completed");
    assert!(
        !saw_idle_while_background_running,
        "Claude reported itself idle while background work was still active"
    );
    assert!(saw_result, "Claude resumed without surfacing {SENTINEL}");
    assert!(
        completed_turns >= 2,
        "expected distinct initial and agent-initiated turns, got {completed_turns}"
    );
}

async fn assert_codex_background_completion_resumes_parent() {
    const LAUNCHED: &str = "CODEX_BACKGROUND_LAUNCHED_731";
    const COMPLETED: &str = "CODEX_BACKGROUND_COMPLETE_731";
    const RESUMED: &str = "CODEX_BACKGROUND_RESUMED_731";
    let backend_kind = BackendKind::Codex;
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    let prompt = format!(
        "Use command execution exactly once to run exactly `sleep 20; printf {COMPLETED}`. Set \
         its initial yield to no more than one second so the tool returns while the process is \
         still running. Do not add `&`, and do not call write_stdin, wait, poll, or any other \
         tool afterward. As soon as the tool reports a running session, end the initial turn by \
         replying exactly {LAUNCHED}. When the command's completion wakes you in a later turn, \
         reply exactly {RESUMED}."
    );
    let stream = spawn_agent_via_protocol_with_options(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "codex-background-completion-resume",
        &prompt,
        None,
        cost_hint_for(backend_kind),
    )
    .await;

    let mut background_tool_call_id = None;
    let mut saw_launched = false;
    let mut saw_idle_before_completion = false;
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let env = fixture
                .client
                .next_event()
                .await
                .expect("read Codex background launch lifecycle")
                .expect("Codex background launch stream closed");
            if env.kind != FrameKind::ChatEvent || env.stream != stream {
                continue;
            }
            let event: ChatEvent = env
                .parse_payload()
                .expect("parse Codex background launch ChatEvent");
            match event {
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("Codex background launch failed: {content}"),
                ChatEvent::StreamDelta(delta) => {
                    saw_launched |= delta.text.contains(LAUNCHED);
                }
                ChatEvent::StreamEnd(end) => {
                    saw_launched |= end.message.content.contains(LAUNCHED);
                }
                ChatEvent::ToolProgress(progress) => {
                    let protocol::ToolProgressUpdate::BackgroundTask(task) = progress.update else {
                        continue;
                    };
                    if task.status == protocol::BackgroundTaskStatus::Running
                        && task
                            .description
                            .as_deref()
                            .is_some_and(|value| value.contains(COMPLETED))
                    {
                        assert!(
                            background_tool_call_id
                                .as_ref()
                                .is_none_or(|known| known == &progress.tool_call_id),
                            "Codex changed background tool identity"
                        );
                        background_tool_call_id = Some(progress.tool_call_id);
                    }
                }
                ChatEvent::TypingStatusChanged(false)
                    if background_tool_call_id.is_some() && !saw_idle_before_completion =>
                {
                    assert!(
                        saw_launched,
                        "Codex ended its launch turn without the required launch sentinel"
                    );
                    saw_idle_before_completion = true;
                }
                ChatEvent::ToolExecutionCompleted(completion)
                    if background_tool_call_id.as_deref()
                        == Some(completion.tool_call_id.as_str()) =>
                {
                    assert!(
                        saw_idle_before_completion,
                        "Codex kept the original turn active until completion; the model likely \
                         polled instead of exercising idle wake-up"
                    );
                    assert!(
                        completion.success,
                        "background command failed: {completion:?}"
                    );
                    let ToolExecutionResult::RunCommand {
                        exit_code, stdout, ..
                    } = completion.tool_result
                    else {
                        panic!("Codex background command returned a non-command result")
                    };
                    assert_eq!(
                        exit_code, 0,
                        "Codex background command exited unsuccessfully"
                    );
                    assert!(
                        stdout.contains(COMPLETED),
                        "Codex background completion omitted {COMPLETED}: {stdout:?}"
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("Codex never completed the qualified idle background command");

    assert!(
        background_tool_call_id.is_some(),
        "Codex never reported the command as background work"
    );
    assert!(saw_launched, "Codex never emitted {LAUNCHED}");
    assert!(
        saw_idle_before_completion,
        "Codex never became idle before background completion"
    );

    let mut saw_resumed_turn = false;
    let mut saw_result = false;
    let resumed = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let env = fixture
                .client
                .next_event()
                .await
                .expect("read Codex background continuation")
                .expect("Codex background continuation stream closed");
            if env.kind != FrameKind::ChatEvent || env.stream != stream {
                continue;
            }
            let event: ChatEvent = env
                .parse_payload()
                .expect("parse Codex background continuation ChatEvent");
            match event {
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("Codex background continuation failed: {content}"),
                ChatEvent::TypingStatusChanged(true) => saw_resumed_turn = true,
                ChatEvent::StreamDelta(delta) => saw_result |= delta.text.contains(RESUMED),
                ChatEvent::StreamEnd(end) => {
                    saw_result |= end.message.content.contains(RESUMED);
                }
                ChatEvent::TypingStatusChanged(false) if saw_resumed_turn && saw_result => break,
                _ => {}
            }
        }
    })
    .await;

    assert!(
        resumed.is_ok(),
        "Codex delivered the background completion while idle but did not initiate a continuation \
         turn within 90 seconds"
    );
    assert!(saw_resumed_turn, "Codex did not start a continuation turn");
    assert!(saw_result, "Codex resumed without surfacing {RESUMED}");
}

async fn assert_claude_background_command_releases_activity() {
    let backend_kind = BackendKind::Claude;
    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    let prompt = "Use the Bash tool exactly once to run `sleep 15; printf TYDE_BACKGROUND_DONE` with run_in_background=true. Do not call TaskOutput or any other tool. After Bash reports that the command is running in the background, end your initial response by replying exactly BACKGROUND_COMMAND_LAUNCHED; do not wait for its result.";
    let stream = spawn_agent_via_protocol_with_options(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "background-command-releases-activity",
        prompt,
        None,
        cost_hint_for(backend_kind),
    )
    .await;

    let mut saw_running = false;
    let mut saw_initial_turn_end = false;
    let mut saw_terminal = false;
    let mut terminal_snapshots_before_idle = 0usize;
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let env = fixture
                .client
                .next_event()
                .await
                .expect("read Claude background command lifecycle")
                .expect("Claude background command stream closed");
            if env.kind != FrameKind::ChatEvent || env.stream != stream {
                continue;
            }
            let event: ChatEvent = env
                .parse_payload()
                .expect("parse Claude background command ChatEvent");
            match event {
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("Claude background command failed: {content}"),
                ChatEvent::StreamEnd(end)
                    if saw_running
                        && !saw_terminal
                        && end.message.tool_calls.is_empty()
                        && end.message.content.contains("BACKGROUND_COMMAND_LAUNCHED") =>
                {
                    saw_initial_turn_end = true;
                }
                ChatEvent::ToolProgress(progress) => {
                    let protocol::ToolProgressUpdate::BackgroundTask(task) = progress.update else {
                        continue;
                    };
                    match task.status {
                        protocol::BackgroundTaskStatus::Running => saw_running = true,
                        protocol::BackgroundTaskStatus::Completed
                        | protocol::BackgroundTaskStatus::Stopped
                        | protocol::BackgroundTaskStatus::Failed
                        | protocol::BackgroundTaskStatus::Unknown => {
                            assert!(
                                saw_running,
                                "background command became terminal without reporting running"
                            );
                            assert!(
                                saw_initial_turn_end,
                                "background command completed before Claude ended its initial turn"
                            );
                            assert_eq!(
                                task.status,
                                protocol::BackgroundTaskStatus::Completed,
                                "background command did not complete successfully"
                            );
                            saw_terminal = true;
                            terminal_snapshots_before_idle += 1;
                        }
                    }
                }
                ChatEvent::TypingStatusChanged(false) if saw_running && !saw_terminal => {
                    panic!("Claude became idle while its background command was still running")
                }
                ChatEvent::TypingStatusChanged(true) if saw_terminal => {
                    panic!(
                        "Claude started another turn before releasing activity for the completed background command"
                    )
                }
                ChatEvent::TypingStatusChanged(false) if saw_terminal => break,
                _ => {}
            }
        }
    })
    .await
    .expect("Claude never released activity after its background command completed");

    assert!(saw_running, "Claude did not launch a background command");
    assert!(saw_initial_turn_end, "Claude did not end its initial turn");
    assert_eq!(
        terminal_snapshots_before_idle, 1,
        "Claude retained the terminal background task until a later notification before releasing activity"
    );
}

#[tokio::test]
#[ignore = "heavy paid backend qualification; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_universal_backend_qualification_suite() {
    assert!(
        real_ai_tests_enabled(),
        "set {RUN_REAL_AI_TESTS_ENV}=1 to authorize paid backend qualification"
    );
    let backends = universal_real_backends().expect("parse universal backend selection");
    let _hermes_python_guard = backends
        .contains(&BackendKind::Hermes)
        .then(|| {
            std::env::var("HERMES_PYTHON")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    Path::new(DEFAULT_HERMES_TEST_PYTHON)
                        .exists()
                        .then(|| DEFAULT_HERMES_TEST_PYTHON.to_owned())
                })
        })
        .flatten()
        .map(|python| EnvVarGuard::set("HERMES_PYTHON", python));
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            failures.push(format!(
                "{}: backend binary is not installed",
                backend_label(backend_kind)
            ));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            failures.push(format!(
                "{}: runtime prerequisites are unavailable",
                backend_label(backend_kind)
            ));
            continue;
        }

        let capabilities = server::backend::capabilities_for_backend_kind(backend_kind);
        for case in CertificationCase::ALL {
            if !backend_supports_certification_case(backend_kind, &capabilities, case) {
                eprintln!(
                    "SKIPPED {} {}: case is not certified for backend capabilities",
                    backend_label(backend_kind),
                    case.id()
                );
                continue;
            }
            eprintln!("RUNNING {} {}", backend_label(backend_kind), case.id());
            let handle = tokio::spawn(run_certification_case_for_backend(backend_kind, case));
            if let Err(error) = handle.await {
                failures.push(format!(
                    "{} {}: {error}",
                    backend_label(backend_kind),
                    case.id()
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "universal paid backend qualification failures:\n{}",
        failures.join("\n")
    );
}

macro_rules! live_certification_tests {
    ($($name:ident => $case:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            #[ignore = "paid backend certification; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
            async fn $name() {
                run_selected_certification_case(CertificationCase::$case).await;
            }
        )+
    };
}

live_certification_tests! {
    real_cert_initial_input_echoed_once => InitialInputEchoedOnce,
    real_cert_typing_starts => TypingStarts,
    real_cert_typing_starts_once => TypingStartsOnce,
    real_cert_stream_starts => StreamStarts,
    real_cert_visible_delta_emitted => VisibleDeltaEmitted,
    real_cert_stream_ends => StreamEnds,
    real_cert_stream_starts_once => StreamStartsOnce,
    real_cert_stream_ends_once => StreamEndsOnce,
    real_cert_typing_stops_once => TypingStopsOnce,
    real_cert_no_error_on_successful_turn => NoErrorOnSuccessfulTurn,
    real_cert_typing_stops_after_stream_end => TypingStopsAfterStreamEnd,
    real_cert_lifecycle_order_is_valid => LifecycleOrderIsValid,
    real_cert_stream_identity_is_stable => StreamIdentityIsStable,
    real_cert_deltas_reconstruct_final_message => DeltasReconstructFinalMessage,
    real_cert_exact_response_delivered => ExactResponseDelivered,
    real_cert_follow_up_completes => FollowUpCompletes,
    real_cert_follow_up_input_echoed_once => FollowUpInputEchoedOnce,
    real_cert_follow_up_uses_distinct_message => FollowUpUsesDistinctMessage,
    real_cert_turn_usage_present => TurnUsagePresent,
    real_cert_turn_input_tokens_positive => TurnInputTokensPositive,
    real_cert_turn_output_tokens_positive => TurnOutputTokensPositive,
    real_cert_turn_total_consistent => TurnTotalConsistent,
    real_cert_cumulative_usage_grows => CumulativeUsageGrows,
    real_cert_request_usage_present => RequestUsagePresent,
    real_cert_request_sequence_starts_at_one => RequestSequenceStartsAtOne,
    real_cert_request_sequences_contiguous => RequestSequencesContiguous,
    real_cert_request_usage_matches_turn => RequestUsageMatchesTurn,
    real_cert_request_ids_unique => RequestIdsUnique,
    real_cert_request_usage_positive => RequestUsagePositive,
    real_cert_context_usage_present => ContextUsagePresent,
    real_cert_context_window_valid => ContextWindowValid,
    real_cert_tool_request_emitted => ToolRequestEmitted,
    real_cert_tool_completion_emitted => ToolCompletionEmitted,
    real_cert_tool_call_ids_correlate => ToolCallIdsCorrelate,
    real_cert_tool_changes_workspace => ToolChangesWorkspace,
    real_cert_tool_failure_reported => ToolFailureReported,
    real_cert_tool_turn_reaches_idle => ToolTurnReachesIdle,
    real_cert_interrupt_emits_cancellation => InterruptEmitsCancellation,
    real_cert_interrupt_returns_idle => InterruptReturnsIdle,
    real_cert_interrupt_stops_command => InterruptStopsCommand,
    real_cert_follow_up_after_interrupt => FollowUpAfterInterrupt,
    real_cert_session_appears_in_list => SessionAppearsInList,
    real_cert_resume_remembers_history => ResumeRemembersHistory,
    real_cert_resume_accepts_follow_up => ResumeAcceptsFollowUp,
    real_cert_resume_preserves_workspace => ResumePreservesWorkspace,
    real_cert_fork_creates_distinct_session => ForkCreatesDistinctSession,
    real_cert_fork_preserves_history => ForkPreservesHistory,
    real_cert_fork_accepts_initial_prompt => ForkAcceptsInitialPrompt,
    real_cert_workspace_instructions_observed => WorkspaceInstructionsObserved,
    real_cert_host_steering_observed => HostSteeringObserved,
    real_cert_skill_discovered => SkillDiscovered,
    real_cert_skill_result_delivered => SkillResultDelivered,
    real_cert_mcp_tool_discovered => McpToolDiscovered,
    real_cert_mcp_tool_called => McpToolCalled,
    real_cert_mcp_result_delivered => McpResultDelivered,
    real_cert_mcp_events_correlate => McpEventsCorrelate,
    real_cert_image_echo_preserved => ImageEchoPreserved,
    real_cert_image_understood => ImageUnderstood,
    real_cert_image_response_streams => ImageResponseStreams,
    real_cert_native_subagent_projected => NativeSubagentProjected,
    real_cert_native_subagent_parent_linked => NativeSubagentParentLinked,
    real_cert_background_work_keeps_agent_active => BackgroundWorkKeepsAgentActive,
    real_cert_background_completion_releases_agent => BackgroundCompletionReleasesAgent,
    real_cert_background_completion_resumes_parent => BackgroundCompletionResumesParent,
    real_cert_agent_initiated_turn_is_distinct => AgentInitiatedTurnIsDistinct,
    real_cert_agent_initiated_result_delivered => AgentInitiatedResultDelivered,
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_claude_cumulative_turn_token_usage() {
    let backend_kind = BackendKind::Claude;
    if !backend_ready_or_skip(backend_kind).await {
        return;
    }

    assert_backend_reports_cumulative_turn_token_usage(backend_kind).await;
}

/// Regression test for the "turn ends and never resumes" bug: when Claude
/// launches a sub-agent with `run_in_background`, the CLI completes the
/// parent turn's first `result` immediately, then — once the background
/// agent finishes — resumes the parent on its own initiative with a fresh
/// `init` + assistant + `result` sequence. Tyde must adopt that unsolicited
/// continuation as a first-class turn instead of dropping it, so the model's
/// final answer (which only exists in the resumed turn) reaches the user.
#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_claude_resumes_parent_after_background_subagent() {
    let backend_kind = BackendKind::Claude;
    if !backend_ready_or_skip(backend_kind).await {
        return;
    }

    // The sub-agent computes 419 + 218 = 637. The parent's initial
    // "I launched it / waiting" turn cannot contain 637 — only the resumed
    // turn, produced after the background agent finishes, can. Seeing 637
    // in assistant output therefore proves the resume was not dropped.
    const SENTINEL: &str = "637";
    let prompt = "Use the Task tool to launch a background sub-agent (set \
         run_in_background to true, subagent_type general-purpose) whose only job \
         is to compute 419 + 218 and return just the number. Immediately after \
         launching it, wait for it to finish, then reply with exactly: \
         'The background agent result is 637.'";

    let mut fixture = RealBackendFixture::new(backend_kind).await;
    let workspace_roots = fixture.workspace_roots();
    // Use the backend default model (no low-cost hint): reliable tool use is
    // required to actually drive a background sub-agent spawn.
    let agent_stream = spawn_agent_via_protocol_with_options(
        &mut fixture.client,
        workspace_roots,
        backend_kind,
        "background-subagent-resume",
        prompt,
        None,
        None,
    )
    .await;

    // Time budget covers spawn + background sub-agent round-trip + resume.
    const BG_TIMEOUT: Duration = Duration::from_secs(240);
    let mut assistant_stream_ends = 0usize;
    let mut saw_sentinel = false;
    let mut saw_typing_false_after_sentinel = false;
    let mut all_assistant_text = String::new();

    tokio::time::timeout(BG_TIMEOUT, async {
        loop {
            let env = match fixture.client.next_event().await {
                Ok(Some(env)) => env,
                Ok(None) => panic!("event stream closed before background resume completed"),
                Err(err) => panic!("error reading events: {err}"),
            };
            if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
                continue;
            }
            let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
            match event {
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("backend returned error: {content}"),
                ChatEvent::StreamEnd(data) => {
                    assistant_stream_ends += 1;
                    all_assistant_text.push_str(&data.message.content);
                    all_assistant_text.push('\n');
                    if data.message.content.contains(SENTINEL) {
                        saw_sentinel = true;
                    }
                }
                ChatEvent::StreamDelta(delta) => {
                    all_assistant_text.push_str(&delta.text);
                }
                ChatEvent::TypingStatusChanged(false) if saw_sentinel => {
                    saw_typing_false_after_sentinel = true;
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for parent to resume after background sub-agent; \
             stream_ends={assistant_stream_ends}, saw_sentinel={saw_sentinel}, \
             assistant_text so far: {all_assistant_text:?}"
        )
    });

    assert!(
        saw_sentinel,
        "resumed parent turn (containing {SENTINEL:?}) was never surfaced; \
         assistant_text={all_assistant_text:?}"
    );
    // The background flow always produces at least two assistant turns:
    // the initial "launched / waiting" turn and the resumed answer turn.
    // A single turn would mean the model answered inline (no background
    // spawn) and the regression path was not exercised.
    assert!(
        assistant_stream_ends >= 2,
        "expected the background spawn + resume to produce >=2 assistant turns, \
         got {assistant_stream_ends}; assistant_text={all_assistant_text:?}"
    );
    assert!(
        saw_typing_false_after_sentinel,
        "typing status never cleared after the resumed answer"
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_codex_cumulative_turn_token_usage() {
    let backend_kind = BackendKind::Codex;
    if !backend_ready_or_skip(backend_kind).await {
        return;
    }

    assert_backend_reports_cumulative_turn_token_usage(backend_kind).await;
}

/// End-to-end proof that a real ACP agent runs through the generic backend.
///
/// Deliberately does **not** call `backend_ready_or_skip`: that helper's 30s
/// readiness probe is shorter than a cold Kiro turn, so gating on it turns this
/// into a test that silently passes without ever reaching the agent. Only a
/// missing binary skips here; a Kiro that is installed but broken fails.
///
/// What this pins that unit tests cannot: the adapter-built spawn spec resolves
/// the real `kiro-cli-chat`, `initialize` negotiates real capabilities, and a
/// real prompt turn reaches `StreamEnd` with assistant text.
#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_completes_a_turn_through_the_generic_acp_backend() {
    const LIVE_TURN_BUDGET: Duration = Duration::from_secs(120);

    if !backend_binary_available(BackendKind::Acp) {
        eprintln!("SKIPPED: kiro not installed");
        return;
    }

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.txt"), "live acp workspace")
        .expect("seed workspace");

    let (_backend, mut events) = tokio::time::timeout(
        LIVE_TURN_BUDGET,
        <server::backend::kiro::KiroBackend as Backend>::spawn(
            vec![workspace.path().to_string_lossy().to_string()],
            server::backend::BackendSpawnConfig {
                // `None` exercises the built-in Kiro agent resolution, which is
                // what a migrated session resumes as.
                acp_agent: None,
                execution_mode: Default::default(),
                cost_hint: cost_hint_for(BackendKind::Acp),
                custom_agent_id: None,
                startup_mcp_servers: Vec::new(),
                session_settings: Default::default(),
                backend_config: Default::default(),
                resolved_spawn_config: Default::default(),
                provider_version: None,
                antigravity_conversations_dir: None,
            },
            protocol::SendMessagePayload {
                message: "Reply with exactly the word READY and nothing else.".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        ),
    )
    .await
    .expect("spawning a real Kiro ACP session timed out")
    .expect("spawning a real Kiro ACP session failed");

    let assistant_text = tokio::time::timeout(LIVE_TURN_BUDGET, async {
        let mut text = String::new();
        while let Some(event) = events.recv().await {
            match event {
                ChatEvent::StreamDelta(delta) => text.push_str(&delta.text),
                ChatEvent::StreamEnd(_) => return Some(text),
                _ => {}
            }
        }
        None
    })
    .await
    .expect("real Kiro turn timed out")
    .expect("stream ended without StreamEnd");

    assert!(
        assistant_text.to_ascii_uppercase().contains("READY"),
        "a real Kiro turn through the generic ACP backend produced no usable \
         assistant text; got: {assistant_text:?}"
    );
}

#[derive(Default)]
struct KiroReadObservation {
    events: Vec<ChatEvent>,
    saw_expected_user: bool,
}

impl KiroReadObservation {
    fn observe(&mut self, event: ChatEvent, expected_user: Option<&str>) {
        if let ChatEvent::MessageAdded(message) = &event {
            if matches!(&message.sender, MessageSender::User)
                && expected_user.is_some_and(|expected| message.content == expected)
            {
                self.saw_expected_user = true;
            }
            if matches!(&message.sender, MessageSender::Error) {
                panic!("Kiro read fixture returned an error: {}", message.content);
            }
        }
        self.events.push(event);
    }
}

async fn expect_live_kiro_event(client: &mut ValidatedConnection, context: &str) -> Envelope {
    const LIVE_KIRO_READ_TIMEOUT: Duration = Duration::from_secs(120);
    loop {
        let envelope = tokio::time::timeout(LIVE_KIRO_READ_TIMEOUT, client.next_event())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
            .unwrap_or_else(|error| panic!("failed waiting for {context}: {error:?}"))
            .unwrap_or_else(|| panic!("connection closed before {context}"));
        if matches!(
            envelope.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
        ) {
            continue;
        }
        return envelope;
    }
}

async fn collect_live_kiro_read_observation(
    client: &mut ValidatedConnection,
    stream: &StreamPath,
    expected_user: Option<&str>,
) -> KiroReadObservation {
    let mut observation = KiroReadObservation::default();
    loop {
        let envelope = expect_live_kiro_event(client, "Kiro read metadata turn").await;
        if envelope.kind != FrameKind::ChatEvent || envelope.stream != *stream {
            continue;
        }
        let event: ChatEvent = envelope.parse_payload().expect("parse Kiro read ChatEvent");
        let terminal = matches!(event, ChatEvent::TypingStatusChanged(false))
            && observation
                .events
                .iter()
                .any(|event| matches!(event, ChatEvent::ToolExecutionCompleted(_)))
            && (expected_user.is_none() || observation.saw_expected_user);
        observation.observe(event, expected_user);
        if terminal {
            return observation;
        }
    }
}

fn assert_measured_read_pair(events: &[ChatEvent], path: &str, bytes: u64) {
    let requests = events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::ToolRequest(request)
                if matches!(
                    &request.tool_type,
                    ToolRequestType::ReadFiles { file_paths }
                        if file_paths.len() == 1 && file_paths[0] == path
                ) =>
            {
                Some(request)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        1,
        "expected one exact read request: {events:?}"
    );
    let completions = events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == requests[0].tool_call_id =>
            {
                Some(completion)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completions.len(),
        1,
        "expected one correlated read completion: {events:?}"
    );
    assert!(completions[0].success);
    assert_eq!(
        completions[0].tool_result,
        ToolExecutionResult::ReadFiles {
            files: vec![protocol::FileInfo {
                path: path.to_string(),
                bytes,
            }]
        }
    );
}

fn assert_unmeasured_native_read_pair(events: &[ChatEvent], path: &str) {
    let request = events
        .iter()
        .find_map(|event| match event {
            ChatEvent::ToolRequest(request)
                if matches!(
                    &request.tool_type,
                    ToolRequestType::ReadFiles { file_paths }
                        if file_paths.len() == 1 && file_paths[0] == path
                ) =>
            {
                Some(request)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("native replay lost read path {path}: {events:?}"));
    let completions = events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == request.tool_call_id =>
            {
                Some(completion)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completions.len(),
        1,
        "native replay must terminalize once: {events:?}"
    );
    assert!(completions[0].success);
    assert!(matches!(
        &completions[0].tool_result,
        ToolExecutionResult::Other { .. }
    ));
    assert!(
        !serde_json::to_string(&completions[0].tool_result)
            .expect("serialize native replay result")
            .contains("\"bytes\"")
    );
}

async fn replayed_agent_events_by_name(fixture: &RealBackendFixture, name: &str) -> Vec<ChatEvent> {
    let mut client = fixture.connect().await;
    let agent_stream = loop {
        let envelope = expect_live_kiro_event(&mut client, "Kiro reload HostBootstrap").await;
        if envelope.kind != FrameKind::HostBootstrap {
            continue;
        }
        let bootstrap: HostBootstrapPayload = envelope
            .parse_payload()
            .expect("parse Kiro reload HostBootstrap");
        break bootstrap
            .agents
            .into_iter()
            .find(|agent| agent.name == name)
            .unwrap_or_else(|| panic!("Kiro reload missing agent {name}"))
            .instance_stream;
    };
    loop {
        let envelope = expect_live_kiro_event(&mut client, "Kiro reload AgentBootstrap").await;
        if envelope.kind == FrameKind::AgentBootstrap && envelope.stream == agent_stream {
            let bootstrap: AgentBootstrapPayload = envelope
                .parse_payload()
                .expect("parse Kiro reload AgentBootstrap");
            return bootstrap
                .events
                .into_iter()
                .filter_map(|event| match event {
                    AgentBootstrapEvent::ChatEvent(event) => Some(event),
                    _ => None,
                })
                .collect();
        }
    }
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_read_metadata_survives_reload_and_native_resume() {
    const FIXTURE_CONTENT: &str =
        "ACP_READ_METADATA_SENTINEL_acp-read-20260730T180905Z-24232\nline-two-π";
    const AGENT_NAME: &str = "Kiro read metadata regression";

    assert!(
        real_ai_tests_enabled(),
        "set TYDE_RUN_REAL_AI_TESTS=1 to authorize the real Kiro read fixture"
    );
    if !backend_binary_available(BackendKind::Acp) {
        eprintln!("SKIPPED: kiro not installed");
        return;
    }
    assert_eq!(FIXTURE_CONTENT.len(), 70);

    let mut fixture = RealBackendFixture::new(BackendKind::Acp).await;
    let workspace_roots = fixture.workspace_roots();
    let workspace_root = workspace_roots[0].clone();
    let first_file = fixture
        .workspace_dir
        .path()
        .join(format!("acp-read-live-{}.txt", Uuid::new_v4()));
    std::fs::write(&first_file, FIXTURE_CONTENT).expect("write first Kiro read fixture");
    let first_path = first_file.to_string_lossy().to_string();
    let first_prompt = format!(
        "Use the native file read tool exactly once to read {first_path}. Do not use a shell command or another tool. Then reply READ_DONE."
    );
    let agent_stream = spawn_agent_via_protocol(
        &mut fixture.client,
        workspace_roots,
        BackendKind::Acp,
        AGENT_NAME,
        &first_prompt,
    )
    .await;
    let first =
        collect_live_kiro_read_observation(&mut fixture.client, &agent_stream, Some(&first_prompt))
            .await;
    assert_measured_read_pair(&first.events, &first_path, 70);

    let reloaded = replayed_agent_events_by_name(&fixture, AGENT_NAME).await;
    assert_measured_read_pair(&reloaded, &first_path, 70);

    let sessions = list_sessions_via_protocol(&mut fixture.client).await;
    let session = sessions
        .sessions
        .into_iter()
        .filter(|session| {
            session.backend_kind == BackendKind::Acp
                && session
                    .workspace_roots
                    .iter()
                    .any(|root| root == &workspace_root)
        })
        .max_by_key(|session| session.updated_at_ms)
        .expect("latest Kiro session for owned workspace");
    let session_id = session.id.clone();
    let resumed_stream = resume_agent_without_prompt_via_protocol(
        &mut fixture.client,
        "Kiro read metadata native resume",
        session_id.clone(),
    )
    .await;
    let native =
        collect_live_kiro_read_observation(&mut fixture.client, &resumed_stream, None).await;
    assert_unmeasured_native_read_pair(&native.events, &first_path);

    let second_file = fixture
        .workspace_dir
        .path()
        .join(format!("acp-read-follow-up-{}.txt", Uuid::new_v4()));
    std::fs::write(&second_file, FIXTURE_CONTENT).expect("write second Kiro read fixture");
    let second_path = second_file.to_string_lossy().to_string();
    let second_prompt = format!(
        "Use the native file read tool exactly once to read {second_path}. Do not use a shell command or another tool. Then reply FOLLOW_UP_DONE."
    );
    fixture
        .client
        .send_message(&resumed_stream, second_prompt.clone())
        .await
        .expect("send Kiro read follow-up");
    let second = collect_live_kiro_read_observation(
        &mut fixture.client,
        &resumed_stream,
        Some(&second_prompt),
    )
    .await;
    assert_measured_read_pair(&second.events, &second_path, 70);

    fixture
        .client
        .delete_session(DeleteSessionPayload {
            session_id: session_id.clone(),
        })
        .await
        .expect("delete owned Kiro session");
    let remaining = list_sessions_via_protocol(&mut fixture.client).await;
    assert!(
        remaining
            .sessions
            .iter()
            .all(|session| session.id != session_id),
        "owned Kiro session must be removed"
    );
    std::fs::remove_file(&first_file).expect("remove first Kiro read fixture");
    std::fs::remove_file(&second_file).expect("remove second Kiro read fixture");
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_antigravity_keeps_selected_workspace_across_resume() {
    assert!(
        real_ai_tests_enabled(),
        "set TYDE_RUN_REAL_AI_TESTS=1 to authorize the paid Antigravity regression"
    );
    assert!(
        backend_binary_available(BackendKind::Antigravity),
        "latest installed agy is required for the Antigravity regression"
    );
    assert!(
        backend_runtime_available(BackendKind::Antigravity),
        "Antigravity runtime prerequisites are unavailable"
    );

    let _native_lock = REAL_ANTIGRAVITY_NATIVE_MUTEX.lock().await;
    let test_token = format!("tyde-antigravity-routing-{}", Uuid::new_v4());
    let mut native_guard = AntigravityNativeArtifactGuard::capture(test_token.clone())
        .expect("snapshot Antigravity native state");
    let roots_dir = tempfile::tempdir().expect("create Antigravity routing roots");
    let primary = roots_dir.path().join("primary root");
    let extra_one = roots_dir.path().join("extra one");
    let extra_two = roots_dir.path().join("extra two");
    let unrelated = roots_dir.path().join("unrelated cwd");
    let scratch = native_guard.scratch_dir();
    let no_root = native_guard.no_root_dir();
    let marker_file = format!("{test_token}-marker.txt");
    let primary_marker = format!("PRIMARY_MARKER_{test_token}");
    let extra_one_marker = format!("EXTRA_ONE_MARKER_{test_token}");
    let extra_two_marker = format!("EXTRA_TWO_MARKER_{test_token}");
    let unrelated_marker = format!("UNRELATED_MARKER_{test_token}");
    let scratch_marker = format!("SCRATCH_MARKER_{test_token}");
    let no_root_marker = format!("NO_ROOT_MARKER_{test_token}");
    for (root, marker) in [
        (primary.as_path(), primary_marker.as_str()),
        (extra_one.as_path(), extra_one_marker.as_str()),
        (extra_two.as_path(), extra_two_marker.as_str()),
        (unrelated.as_path(), unrelated_marker.as_str()),
        (scratch.as_path(), scratch_marker.as_str()),
        (no_root.as_path(), no_root_marker.as_str()),
    ] {
        seed_antigravity_marker(&mut native_guard, root, &marker_file, marker);
    }
    let workspace_roots = vec![
        primary.to_string_lossy().to_string(),
        extra_one.to_string_lossy().to_string(),
        extra_two.to_string_lossy().to_string(),
    ];
    let all_probe_roots = [
        primary.as_path(),
        extra_one.as_path(),
        extra_two.as_path(),
        unrelated.as_path(),
        scratch.as_path(),
        no_root.as_path(),
    ];

    let mut fixture = RealBackendFixture::new(BackendKind::Antigravity).await;
    let first_probe = format!("{test_token}-first.txt");
    track_antigravity_probe_paths(&mut native_guard, &all_probe_roots, &first_probe);
    let first_prompt = antigravity_routing_prompt(&test_token, &marker_file, &first_probe);
    let (first_stream, first_start) = spawn_antigravity_with_start(
        &mut fixture.client,
        workspace_roots.clone(),
        "Antigravity workspace first turn",
        &first_prompt,
    )
    .await;
    assert_eq!(first_start.workspace_roots, workspace_roots);
    let first_session = antigravity_start_session(&first_start);
    native_guard.register_session(first_session.clone());
    let first_turn = expect_assistant_turn_with_typing_after_user_echo(
        &mut fixture.client,
        &first_stream,
        &first_prompt,
    )
    .await;
    assert_antigravity_routing_probe(
        &primary,
        &[&extra_one, &extra_two, &unrelated, &scratch, &no_root],
        &first_probe,
        &primary_marker,
        &first_turn.final_text,
    );

    let follow_up_probe = format!("{test_token}-follow-up.txt");
    track_antigravity_probe_paths(&mut native_guard, &all_probe_roots, &follow_up_probe);
    let follow_up_prompt = antigravity_routing_prompt(&test_token, &marker_file, &follow_up_probe);
    fixture
        .client
        .send_message(&first_stream, follow_up_prompt.clone())
        .await
        .expect("send Antigravity routing follow-up");
    let follow_up = expect_assistant_turn_with_typing_after_user_echo(
        &mut fixture.client,
        &first_stream,
        &follow_up_prompt,
    )
    .await;
    assert_antigravity_routing_probe(
        &primary,
        &[&extra_one, &extra_two, &unrelated, &scratch, &no_root],
        &follow_up_probe,
        &primary_marker,
        &follow_up.final_text,
    );
    let sessions = list_sessions_via_protocol(&mut fixture.client).await;
    let stored = sessions
        .sessions
        .iter()
        .find(|session| session.id == first_session)
        .expect("stored Antigravity routing session");
    assert_eq!(stored.workspace_roots, workspace_roots);

    close_antigravity_regression_agent(&mut fixture.client, &first_stream).await;
    let resume_probe = format!("{test_token}-resume.txt");
    track_antigravity_probe_paths(&mut native_guard, &all_probe_roots, &resume_probe);
    let resume_prompt = antigravity_routing_prompt(&test_token, &marker_file, &resume_probe);
    let (resumed_stream, resumed_start) = resume_antigravity_with_start(
        &mut fixture.client,
        first_session.clone(),
        "Antigravity workspace History resume",
        &resume_prompt,
    )
    .await;
    assert_eq!(
        antigravity_start_session(&resumed_start),
        first_session,
        "History resume must reuse the exact native conversation UUID"
    );
    assert_eq!(
        resumed_start.workspace_roots, workspace_roots,
        "History resume must retain stored ordered roots"
    );
    let resumed_turn = expect_assistant_turn_with_typing_after_user_echo(
        &mut fixture.client,
        &resumed_stream,
        &resume_prompt,
    )
    .await;
    assert_antigravity_routing_probe(
        &primary,
        &[&extra_one, &extra_two, &unrelated, &scratch, &no_root],
        &resume_probe,
        &primary_marker,
        &resumed_turn.final_text,
    );
    close_antigravity_regression_agent(&mut fixture.client, &resumed_stream).await;

    let second_probe = format!("{test_token}-second-fresh.txt");
    track_antigravity_probe_paths(&mut native_guard, &all_probe_roots, &second_probe);
    let second_prompt = antigravity_routing_prompt(&test_token, &marker_file, &second_probe);
    let (second_stream, second_start) = spawn_antigravity_with_start(
        &mut fixture.client,
        workspace_roots.clone(),
        "Antigravity workspace second fresh turn",
        &second_prompt,
    )
    .await;
    assert_eq!(second_start.workspace_roots, workspace_roots);
    let second_session = antigravity_start_session(&second_start);
    assert_ne!(
        second_session, first_session,
        "an independent fresh spawn must create a distinct native conversation"
    );
    native_guard.register_session(second_session.clone());
    let second_turn = expect_assistant_turn_with_typing_after_user_echo(
        &mut fixture.client,
        &second_stream,
        &second_prompt,
    )
    .await;
    assert_antigravity_routing_probe(
        &primary,
        &[&extra_one, &extra_two, &unrelated, &scratch, &no_root],
        &second_probe,
        &primary_marker,
        &second_turn.final_text,
    );
    close_antigravity_regression_agent(&mut fixture.client, &second_stream).await;

    let no_root_probe = format!("{test_token}-no-root.txt");
    track_antigravity_probe_paths(&mut native_guard, &all_probe_roots, &no_root_probe);
    let no_root_prompt = antigravity_routing_prompt(&test_token, &marker_file, &no_root_probe);
    let (no_root_stream, no_root_start) = spawn_antigravity_with_start(
        &mut fixture.client,
        Vec::new(),
        "Antigravity workspace no-root turn",
        &no_root_prompt,
    )
    .await;
    assert!(
        no_root_start.workspace_roots.is_empty(),
        "no-root AgentStart must preserve empty protocol roots"
    );
    let no_root_session = antigravity_start_session(&no_root_start);
    assert_ne!(no_root_session, first_session);
    assert_ne!(no_root_session, second_session);
    native_guard.register_session(no_root_session.clone());
    let no_root_turn = expect_assistant_turn_with_typing_after_user_echo(
        &mut fixture.client,
        &no_root_stream,
        &no_root_prompt,
    )
    .await;
    assert_antigravity_routing_probe(
        &no_root,
        &[&primary, &extra_one, &extra_two, &unrelated, &scratch],
        &no_root_probe,
        &no_root_marker,
        &no_root_turn.final_text,
    );
    close_antigravity_regression_agent(&mut fixture.client, &no_root_stream).await;

    for session_id in [&first_session, &second_session, &no_root_session] {
        fixture
            .client
            .inner
            .delete_session(protocol::DeleteSessionPayload {
                session_id: (*session_id).clone(),
            })
            .await
            .unwrap_or_else(|error| {
                panic!("delete owned Antigravity session {session_id}: {error:?}")
            });
    }
    native_guard
        .finalize()
        .expect("restore Antigravity native state");
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_turn_token_usage_contract_if_reported() {
    let backend_kind = BackendKind::Acp;
    if !backend_ready_or_skip(backend_kind).await {
        return;
    }

    assert_backend_turn_usage_contract_if_reported(backend_kind).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_tycode_cumulative_turn_token_usage() {
    let backend_kind = BackendKind::Tycode;
    if !backend_ready_or_skip(backend_kind).await {
        return;
    }

    assert_backend_reports_cumulative_turn_token_usage(backend_kind).await;
}

#[tokio::test]
#[ignore = "real Hermes backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_hermes_openrouter_emits_visible_content() {
    if !real_ai_tests_enabled() {
        eprintln!("SKIPPED: real Hermes test requires {RUN_REAL_AI_TESTS_ENV}=1");
        return;
    }
    let hermes_python = std::env::var("HERMES_PYTHON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HERMES_TEST_PYTHON.to_string());
    if !Path::new(&hermes_python).exists() {
        eprintln!("SKIPPED: HERMES_PYTHON target not found: {hermes_python}");
        return;
    }
    let _hermes_python_guard = EnvVarGuard::set("HERMES_PYTHON", hermes_python);

    let provider = std::env::var("TYDE_HERMES_TEST_PROVIDER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HERMES_TEST_PROVIDER.to_string());
    let model = std::env::var("TYDE_HERMES_TEST_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HERMES_TEST_MODEL.to_string());
    let reasoning_effort = std::env::var("TYDE_HERMES_TEST_REASONING_EFFORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "none".to_string());
    eprintln!(
        "RUNNING Hermes live test with provider={provider} model={model} reasoning_effort={reasoning_effort}"
    );

    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        workspace.path().join("README.txt"),
        "Hermes live probe workspace",
    )
    .expect("seed Hermes workspace");
    let mut settings = SessionSettingsValues::default();
    settings.0.insert(
        "model".to_string(),
        SessionSettingValue::String(format!("{model} --provider {provider}")),
    );
    settings.0.insert(
        "reasoning_effort".to_string(),
        SessionSettingValue::String(reasoning_effort),
    );

    let (backend, mut events) = <server::backend::hermes::HermesBackend as Backend>::spawn(
        vec![workspace.path().to_string_lossy().to_string()],
        server::backend::BackendSpawnConfig {
            acp_agent: None,
            execution_mode: Default::default(),
            cost_hint: cost_hint_for(BackendKind::Hermes),
            custom_agent_id: None,
            startup_mcp_servers: Vec::new(),
            session_settings: Some(settings),
            provider_version: None,
            antigravity_conversations_dir: None,
            backend_config: Default::default(),
            resolved_spawn_config: Default::default(),
        },
        protocol::SendMessagePayload {
            message: "Reply exactly with ok.".to_owned(),
            images: None,
            origin: None,
            tool_response: None,
        },
    )
    .await
    .expect("spawn Hermes backend");

    let mut final_text = String::new();
    let mut delta_count = 0usize;
    let mut diagnostics = Vec::new();
    let mut saw_stream_end = false;
    let mut saw_typing_false_after_end = false;
    tokio::time::timeout(REAL_BACKEND_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            match event {
                ChatEvent::StreamDelta(delta) => {
                    delta_count += 1;
                    final_text.push_str(&delta.text);
                }
                ChatEvent::StreamEnd(end) => {
                    saw_stream_end = true;
                    if !end.message.content.trim().is_empty() {
                        final_text = end.message.content;
                    }
                }
                ChatEvent::TypingStatusChanged(false) if saw_stream_end => {
                    saw_typing_false_after_end = true;
                    break;
                }
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => {
                    panic!("Hermes live test emitted error: {content}");
                }
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Warning,
                    content,
                    ..
                }) => diagnostics.push(content),
                _ => {}
            }
        }
    })
    .await
    .expect("Hermes live response timed out");
    backend.shutdown().await;

    assert!(saw_stream_end, "Hermes live test never emitted StreamEnd");
    assert!(
        saw_typing_false_after_end,
        "Hermes live test did not clear typing after StreamEnd"
    );
    assert!(
        !final_text.trim().is_empty(),
        "Hermes live response had no visible assistant text; diagnostics={diagnostics:?}"
    );
    assert!(
        final_text.to_ascii_lowercase().contains("ok"),
        "Hermes live response should contain ok, got {final_text:?}"
    );
    assert!(
        delta_count > 0,
        "Hermes live response should stream at least one visible delta"
    );
}

#[tokio::test]
#[ignore = "real Hermes MCP test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_hermes_openrouter_calls_tyde_mcp_bridge() {
    if !real_ai_tests_enabled() {
        eprintln!("SKIPPED: real Hermes MCP test requires {RUN_REAL_AI_TESTS_ENV}=1");
        return;
    }
    let hermes_python = std::env::var("HERMES_PYTHON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HERMES_TEST_PYTHON.to_string());
    if !Path::new(&hermes_python).exists() {
        eprintln!("SKIPPED: HERMES_PYTHON target not found: {hermes_python}");
        return;
    }
    let _hermes_python_guard = EnvVarGuard::set("HERMES_PYTHON", hermes_python);
    let provider = std::env::var("TYDE_HERMES_TEST_PROVIDER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HERMES_TEST_PROVIDER.to_string());
    let model = std::env::var("TYDE_HERMES_TEST_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HERMES_TEST_MODEL.to_string());
    eprintln!("RUNNING Hermes MCP live test with provider={provider} model={model}");

    let workspace = tempfile::tempdir().expect("tempdir");
    let mcp_script = workspace.path().join("probe_mcp.py");
    std::fs::write(
        &mcp_script,
        r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    request_id = request.get("id")
    if request_id is None:
        continue
    if method == "initialize":
        result = {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "tyde-live-probe", "version": "1"}
        }
    elif method == "tools/list":
        result = {"tools": [{
            "name": "tyde_live_probe",
            "description": "Return the exact Tyde bridge verification phrase",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}
        }]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "TYDE_BRIDGE_OK"}], "isError": False}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}), flush=True)
"#,
    )
    .expect("write live MCP probe");

    let mut settings = SessionSettingsValues::default();
    settings.0.insert(
        "model".to_string(),
        SessionSettingValue::String(format!("{model} --provider {provider}")),
    );
    settings.0.insert(
        "reasoning_effort".to_string(),
        SessionSettingValue::String("none".to_string()),
    );
    let (backend, mut events) = <server::backend::hermes::HermesBackend as Backend>::spawn(
        vec![workspace.path().to_string_lossy().to_string()],
        server::backend::BackendSpawnConfig {
            acp_agent: None,
            execution_mode: Default::default(),
            cost_hint: cost_hint_for(BackendKind::Hermes),
            custom_agent_id: None,
            startup_mcp_servers: vec![server::backend::StartupMcpServer {
                name: "tyde_live_test".to_string(),
                transport: server::backend::StartupMcpTransport::Stdio {
                    command: "python3".to_string(),
                    args: vec![mcp_script.to_string_lossy().to_string()],
                    env: HashMap::new(),
                },
            }],
            session_settings: Some(settings),
            provider_version: None,
            antigravity_conversations_dir: None,
            backend_config: Default::default(),
            resolved_spawn_config: Default::default(),
        },
        protocol::SendMessagePayload {
            message: "Call mcp_tyde_tyde_live_probe. Then reply with exactly the text returned by the tool."
                .to_owned(),
            images: None,
            origin: None,
            tool_response: None,
        },
    )
    .await
    .expect("spawn Hermes backend with Tyde MCP bridge");

    let mut final_text = String::new();
    let mut diagnostics = Vec::new();
    let mut saw_bridge_tool_request = false;
    let mut saw_bridge_tool_completion = false;
    tokio::time::timeout(REAL_BACKEND_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            match event {
                ChatEvent::StreamEnd(end) => {
                    final_text = end.message.content;
                }
                ChatEvent::TypingStatusChanged(false) if !final_text.is_empty() => break,
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Error,
                    content,
                    ..
                }) => panic!("Hermes MCP live test emitted error: {content}"),
                ChatEvent::MessageAdded(ChatMessage {
                    sender: MessageSender::Warning,
                    content,
                    ..
                }) => diagnostics.push(content),
                ChatEvent::ToolRequest(request)
                    if request.tool_name == "mcp_tyde_tyde_live_probe" =>
                {
                    saw_bridge_tool_request = true;
                }
                ChatEvent::ToolExecutionCompleted(completed)
                    if completed.tool_name == "mcp_tyde_tyde_live_probe" =>
                {
                    saw_bridge_tool_completion = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("Hermes MCP live response timed out");
    backend.shutdown().await;
    assert_eq!(
        final_text.trim(),
        "TYDE_BRIDGE_OK",
        "Hermes diagnostics: {diagnostics:#?}"
    );
    assert!(
        saw_bridge_tool_request,
        "Hermes did not expose the MCP tool request"
    );
    assert!(
        saw_bridge_tool_completion,
        "Hermes did not expose the MCP tool completion"
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn resumable_real_backends_remember_secret() {
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
    ];
    let mut failures = Vec::new();

    for backend_kind in backends {
        eprintln!("RUNNING resume test for {}", backend_label(backend_kind));
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            resume_secret_via_protocol(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend resume failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_backends_emit_stream_deltas() {
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Acp,
    ];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_emits_stream_deltas(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend streaming failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_backends_emit_typing_status() {
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Acp,
    ];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_emits_typing_status(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend typing status failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_claude_first_turn_native_subagent_appears_in_host_stream() {
    let backend_kind = BackendKind::Claude;
    if !backend_binary_available(backend_kind) {
        eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
        return;
    }
    if !backend_runtime_available(backend_kind) {
        eprintln!(
            "SKIPPED: {} not runnable in current environment",
            backend_label(backend_kind)
        );
        return;
    }
    if let Err(reason) = probe_backend_runtime(backend_kind).await {
        eprintln!(
            "SKIPPED: {} failed readiness probe: {}",
            backend_label(backend_kind),
            reason
        );
        return;
    }

    let handle = tokio::spawn(async move {
        let mut fixture = RealBackendFixture::new(backend_kind).await;
        let workspace_roots = fixture.workspace_roots();
        let prompt = "Test harness: in your very first action, call the Task tool exactly once. Ask the sub-agent to read README.txt in the current working directory and reply with exactly the first line. Wait for that Task to finish. Afterward, reply exactly with: parent complete";

        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                name: Some("claude-native-child-first-turn".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots,
                    prompt: prompt.to_owned(),
                    images: None,
                    backend_kind,
                    launch_profile_id: None,
                    cost_hint: Some(SpawnCostHint::High),
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await
            .expect("spawn_agent failed");

        let env =
            expect_next_event_kind(&mut fixture.client, FrameKind::NewAgent, "parent NewAgent")
                .await;
        let parent_new: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
        assert_eq!(parent_new.origin, AgentOrigin::User);

        let parent_start = expect_agent_start_on_stream(
            &mut fixture.client,
            &parent_new.instance_stream,
            "parent AgentStart",
        )
        .await;
        assert_eq!(parent_start.agent_id, parent_new.agent_id);

        let child_new = expect_subagent_child_for_parent(
            &mut fixture.client,
            &parent_new.agent_id,
            "backend-native child NewAgent",
        )
        .await;
        assert_eq!(child_new.origin, AgentOrigin::BackendNative);
        assert_eq!(
            child_new.parent_agent_id.as_ref(),
            Some(&parent_new.agent_id)
        );

        let child_start = expect_agent_start_on_stream(
            &mut fixture.client,
            &child_new.instance_stream,
            "backend-native child AgentStart",
        )
        .await;
        assert_eq!(child_start.origin, AgentOrigin::BackendNative);
        assert_eq!(
            child_start.parent_agent_id.as_ref(),
            Some(&parent_new.agent_id)
        );
    });

    if let Err(err) = handle.await {
        panic!("{}: {}", backend_label(backend_kind), err);
    }
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_codex_emits_tool_events_for_file_copy() {
    let backends = [BackendKind::Codex];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_emits_tool_events_for_file_copy(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend tool event failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_codex_emits_token_usage() {
    let backend_kind = BackendKind::Codex;

    if !backend_binary_available(backend_kind) {
        eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
        return;
    }
    if !backend_runtime_available(backend_kind) {
        eprintln!(
            "SKIPPED: {} not runnable in current environment",
            backend_label(backend_kind)
        );
        return;
    }
    if let Err(reason) = probe_backend_runtime(backend_kind).await {
        eprintln!(
            "SKIPPED: {} failed readiness probe: {}",
            backend_label(backend_kind),
            reason
        );
        return;
    }

    let mut fixture = RealBackendFixture::new(backend_kind).await;
    assert_codex_emits_token_usage(&mut fixture).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_codex_interrupts_long_running_command() {
    let backend_kind = BackendKind::Codex;
    if !backend_ready_or_skip(backend_kind).await {
        return;
    }

    let mut fixture = RealBackendFixture::new(backend_kind).await;
    assert_backend_interrupts_long_running_command(&mut fixture, backend_kind).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_backends_interrupt_long_running_command() {
    let backends = [BackendKind::Claude, BackendKind::Codex, BackendKind::Acp];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_interrupts_long_running_command(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend interrupt failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_emits_typing_and_streaming_on_follow_up_turns() {
    let backend_kind = BackendKind::Acp;

    if !backend_binary_available(backend_kind) {
        eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
        return;
    }
    if !backend_runtime_available(backend_kind) {
        eprintln!(
            "SKIPPED: {} not runnable in current environment",
            backend_label(backend_kind)
        );
        return;
    }
    if let Err(reason) = probe_backend_runtime(backend_kind).await {
        eprintln!(
            "SKIPPED: {} failed readiness probe: {}",
            backend_label(backend_kind),
            reason
        );
        return;
    }

    let mut fixture = RealBackendFixture::new(backend_kind).await;
    assert_backend_emits_typing_and_streaming_on_follow_up_turns(&mut fixture, backend_kind).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_follow_up_user_message_echo_is_not_duplicated() {
    let backend_kind = BackendKind::Acp;

    if !backend_binary_available(backend_kind) {
        eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
        return;
    }
    if !backend_runtime_available(backend_kind) {
        eprintln!(
            "SKIPPED: {} not runnable in current environment",
            backend_label(backend_kind)
        );
        return;
    }
    if let Err(reason) = probe_backend_runtime(backend_kind).await {
        eprintln!(
            "SKIPPED: {} failed readiness probe: {}",
            backend_label(backend_kind),
            reason
        );
        return;
    }

    let mut fixture = RealBackendFixture::new(backend_kind).await;
    assert_backend_follow_up_user_echo_not_duplicated(&mut fixture, backend_kind).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_codex_describes_image_input() {
    let backends = [BackendKind::Codex];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_describes_image_input(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend image input failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_codex_low_cost_name_generation_prompt_returns_non_empty_response() {
    let backends = [BackendKind::Codex];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if !backend_binary_available(backend_kind) {
            eprintln!("SKIPPED: {} not installed", backend_label(backend_kind));
            continue;
        }
        if !backend_runtime_available(backend_kind) {
            eprintln!(
                "SKIPPED: {} not runnable in current environment",
                backend_label(backend_kind)
            );
            continue;
        }
        if let Err(reason) = probe_backend_runtime(backend_kind).await {
            eprintln!(
                "SKIPPED: {} failed readiness probe: {}",
                backend_label(backend_kind),
                reason
            );
            continue;
        }

        let handle = tokio::spawn(async move {
            let mut fixture = RealBackendFixture::new(backend_kind).await;
            assert_backend_returns_non_empty_name_for_name_prompt(&mut fixture, backend_kind).await;
        });

        if let Err(err) = handle.await {
            failures.push(format!("{}: {}", backend_label(backend_kind), err));
        }
    }

    assert!(
        failures.is_empty(),
        "real backend name-generation failures:\n{}",
        failures.join("\n")
    );
}
