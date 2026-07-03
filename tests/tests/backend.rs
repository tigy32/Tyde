mod fixture;

use std::collections::{HashMap, VecDeque};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use fixture::Fixture;
use protocol::{
    AgentActivityStatsPayload, AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorCode,
    AgentOrigin, AgentStartPayload, BackendKind, ChatEvent, ChatMessage, CommandErrorPayload,
    CustomAgentId, Envelope, FrameKind, HostSettingValue, ImageData, ListSessionsPayload,
    MessageMetadataUpdateData, MessageSender, NewAgentPayload, ProtocolValidator, SessionId,
    SessionListPayload, SessionSchemaEntry, SessionSchemasPayload, SessionSettingFieldType,
    SessionSettingValue, SessionSettingsValues, SessionSummary, SetSettingPayload,
    SpawnAgentParams, SpawnAgentPayload, SpawnCostHint, StreamPath, TokenUsage, TokenUsageScope,
    TokenUsageUnavailableReason, ToolExecutionCompletedData, ToolRequest, ToolRequestType,
};
use server::backend::Backend;
use uuid::Uuid;

const REAL_BACKEND_TIMEOUT: Duration = Duration::from_secs(60);
const REAL_BACKEND_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const RUN_REAL_AI_TESTS_ENV: &str = "TYDE_RUN_REAL_AI_TESTS";
const DEFAULT_HERMES_TEST_PYTHON: &str = "/Users/mike/.hermes/tyde-hermes-python";
const DEFAULT_HERMES_TEST_PROVIDER: &str = "openrouter";
const DEFAULT_HERMES_TEST_MODEL: &str = "anthropic/claude-haiku-4.5";
const SOLID_RED_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJ0lEQVR42u3NsQkAAAjAsP7/tF7hIASyp6lTCQQCgUAgEAgEgi/BAjLD/C5w/SM9AAAAAElFTkSuQmCC";

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
        BackendKind::Kiro => binary_available("kiro-cli-chat") || binary_available("kiro-cli"),
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
        BackendKind::Claude | BackendKind::Antigravity | BackendKind::Kiro => {
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
                        cost_hint: cost_hint_for(BackendKind::Tycode),
                        custom_agent_id: None,
                        startup_mcp_servers: Vec::new(),
                        session_settings: Default::default(),
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
        BackendKind::Kiro => {
            let workspace = tempfile::tempdir().map_err(|err| format!("{err}"))?;
            std::fs::write(workspace.path().join("README.txt"), "probe workspace")
                .map_err(|err| format!("failed to seed Kiro probe workspace: {err}"))?;
            let result = tokio::time::timeout(
                REAL_BACKEND_PROBE_TIMEOUT,
                <server::backend::kiro::KiroBackend as Backend>::spawn(
                    vec![workspace.path().to_string_lossy().to_string()],
                    server::backend::BackendSpawnConfig {
                        cost_hint: cost_hint_for(BackendKind::Kiro),
                        custom_agent_id: None,
                        startup_mcp_servers: Vec::new(),
                        session_settings: Default::default(),
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
                        cost_hint: cost_hint_for(BackendKind::Hermes),
                        custom_agent_id: None,
                        startup_mcp_servers: Vec::new(),
                        session_settings: Default::default(),
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
    // default), which for Codex means xhigh reasoning — too slow for tests.
    let _ = backend_kind;
    Some(SpawnCostHint::Low)
}

fn backend_label(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
        BackendKind::Hermes => "hermes",
    }
}

struct AntigravityConversationDbGuard {
    path: PathBuf,
    remove_file: bool,
    created_dirs: Vec<PathBuf>,
}

impl AntigravityConversationDbGuard {
    fn create(session_id: &SessionId) -> Self {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
        let dirs = [
            home.join(".gemini"),
            home.join(".gemini").join("antigravity-cli"),
            home.join(".gemini")
                .join("antigravity-cli")
                .join("conversations"),
        ];
        let mut created_dirs = Vec::new();
        for dir in dirs {
            if !dir.exists() {
                std::fs::create_dir(&dir).unwrap_or_else(|err| {
                    panic!("failed to create fake Antigravity db dir {dir:?}: {err}")
                });
                created_dirs.push(dir);
            }
        }
        let path = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")
            .join(format!("{}.db", session_id.0));
        let remove_file = !path.exists();
        if remove_file {
            std::fs::write(&path, b"test conversation db").unwrap_or_else(|err| {
                panic!("failed to create fake Antigravity conversation db {path:?}: {err}")
            });
        }
        Self {
            path,
            remove_file,
            created_dirs,
        }
    }
}

impl Drop for AntigravityConversationDbGuard {
    fn drop(&mut self) {
        if self.remove_file {
            let _ = std::fs::remove_file(&self.path);
        }
        for dir in self.created_dirs.iter().rev() {
            let _ = std::fs::remove_dir(dir);
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
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

fn agent_start_from_bootstrap(env: Envelope, context: &str) -> AgentStartPayload {
    assert_eq!(env.kind, FrameKind::AgentBootstrap, "expected {context}");
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrap");
    payload
        .events
        .into_iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentStart(start) => Some(start),
            _ => None,
        })
        .unwrap_or_else(|| panic!("AgentBootstrap missing AgentStart for {context}"))
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
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Kiro,
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

        let start = expect_fixture_agent_start(
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

        loop {
            let env = expect_fixture_event(&mut fixture.client, "empty-root ChatEvent").await;
            if env.kind != FrameKind::ChatEvent || env.stream != new_agent.instance_stream {
                continue;
            }
            let event: ChatEvent = env.parse_payload().expect("parse empty-root ChatEvent");
            if matches!(event, ChatEvent::StreamEnd(_)) {
                break;
            }
        }
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

    let start = expect_fixture_agent_start(
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

    loop {
        let env = expect_fixture_event(&mut fixture.client, "Antigravity ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != new_agent.instance_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse Antigravity ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(_)) {
            break;
        }
    }

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
    let _db_guard = AntigravityConversationDbGuard::create(&session_id);
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
    let start = expect_fixture_agent_start(
        &mut fixture.client,
        &new_agent.instance_stream,
        "Antigravity AgentStart",
    )
    .await;
    let session_id = start
        .session_id
        .clone()
        .expect("Antigravity AgentStart session_id");

    loop {
        let env = expect_fixture_event(&mut fixture.client, "Antigravity ChatEvent").await;
        if env.kind != FrameKind::ChatEvent || env.stream != new_agent.instance_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse Antigravity ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(_)) {
            break;
        }
    }

    let db_guard = AntigravityConversationDbGuard::create(&session_id);
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
    let probe_program = write_fake_kiro_probe_program(&probe_dir);
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        kiro_probe_program: Some(probe_program.to_string_lossy().to_string()),
        ..server::HostRuntimeConfig::default()
    })
    .await;

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Kiro],
            },
        })
        .await
        .expect("enable Kiro backend");

    let schemas = loop {
        let env = expect_fixture_event(&mut fixture.client, "Kiro SessionSchemas").await;
        if !matches!(
            env.kind,
            FrameKind::SessionSchemas | FrameKind::LaunchProfileCatalogNotify
        ) {
            continue;
        }
        if env.kind == FrameKind::LaunchProfileCatalogNotify {
            continue;
        }
        let payload: SessionSchemasPayload = env.parse_payload().expect("parse SessionSchemas");
        if payload
            .schemas
            .iter()
            .any(|schema| schema.backend_kind() == BackendKind::Kiro)
        {
            break payload;
        }
    };

    let kiro_schema = schemas
        .schemas
        .into_iter()
        .find(|schema| schema.backend_kind() == BackendKind::Kiro)
        .expect("Kiro schema should be present");
    let SessionSchemaEntry::Ready {
        schema: kiro_schema,
    } = kiro_schema
    else {
        panic!("expected Kiro schema to be ready");
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
                backend_kind: BackendKind::Kiro,
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

    expect_fixture_agent_start(&mut fixture.client, &agent_stream, "Kiro AgentStart").await;

    loop {
        let env = expect_fixture_event(&mut fixture.client, "Kiro StreamEnd").await;
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse Kiro ChatEvent");
        if matches!(event, ChatEvent::StreamEnd(_)) {
            break;
        }
    }
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
async fn hermes_unavailable_dynamic_schema_with_tier_settings_is_agent_error() {
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
    loop {
        let env = expect_fixture_event(&mut fixture.client, "Hermes tier HostSettings").await;
        if env.kind == FrameKind::HostSettings {
            break;
        }
    }

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Hermes unavailable tier schema".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "hello".to_string(),
                images: None,
                backend_kind: BackendKind::Hermes,
                launch_profile_id: None,
                cost_hint: Some(SpawnCostHint::Low),
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("send Hermes spawn with tier settings and unavailable schema");

    let new_agent = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Hermes unavailable tier schema NewAgent",
        )
        .await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse Hermes unavailable tier schema NewAgent");
                if payload.backend_kind == BackendKind::Hermes {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error = env
                    .parse_payload::<CommandErrorPayload>()
                    .expect("parse unexpected Hermes unavailable tier schema CommandError");
                panic!(
                    "Hermes unavailable tier schema should become agent startup failure, not CommandError: {error:?}"
                );
            }
            _ => {}
        }
    };

    let bootstrap: AgentBootstrapPayload = loop {
        let env = expect_fixture_event(
            &mut fixture.client,
            "Hermes unavailable tier schema AgentBootstrap",
        )
        .await;
        if env.kind == FrameKind::AgentBootstrap && env.stream == new_agent.instance_stream {
            break env
                .parse_payload()
                .expect("parse Hermes unavailable tier schema AgentBootstrap");
        }
    };
    let error = bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentError(error) => Some(error),
            _ => None,
        })
        .expect("Hermes unavailable tier schema bootstrap must include AgentError");
    assert_eq!(error.code, AgentErrorCode::BackendFailed);
    assert!(error.fatal);
    assert!(
        error
            .message
            .contains("session settings schema unavailable"),
        "unexpected Hermes unavailable tier schema error: {error:?}"
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
async fn compact_turn_emits_system_message_and_stream_end_without_error() {
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

    let mut saw_system_message = false;
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
                if matches!(message.sender, MessageSender::System) {
                    assert_eq!(message.content, "Conversation compacted.");
                    saw_system_message = true;
                }
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

    assert!(
        saw_system_message,
        "compact turn should emit a visible system message"
    );
    assert!(saw_stream_end, "compact turn should emit StreamEnd");
}

/// Fixture that uses real backends (not mock) so backend_kind dispatch is tested.
struct RealBackendFixture {
    client: ValidatedConnection,
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
    async fn new() -> Self {
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
        std::fs::write(
            &settings_path,
            r#"{"settings":{"complexity_tiers_enabled":true}}"#,
        )
        .expect("seed settings store with complexity tiers enabled");
        // Real backends — NOT mock
        let host = server::spawn_host_with_store_paths(session_path, project_path, settings_path)
            .expect("initialize host with real backends");

        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();
        let client_config = client::ClientConfig::current();

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

        Self {
            client: ValidatedConnection {
                inner: client,
                validator: ProtocolValidator::new(),
                pending_bootstrap_events: VecDeque::new(),
            },
            session_store_dir,
            workspace_dir,
        }
    }

    fn workspace_roots(&self) -> Vec<String> {
        vec![self.workspace_dir.path().to_string_lossy().to_string()]
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

async fn expect_backend_native_child_for_parent(
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
        if payload.origin == AgentOrigin::BackendNative
            && payload.parent_agent_id.as_ref() == Some(parent_agent_id)
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
                let final_text = if data.message.content.trim().is_empty() {
                    streamed_text
                } else {
                    data.message.content
                };
                return AssistantTurn {
                    final_text,
                    delta_count,
                };
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
                    streamed_text
                } else {
                    data.message.content
                };
                return AssistantTurn {
                    final_text,
                    delta_count,
                };
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
    assert_eq!(
        folded
            .message
            .token_usage
            .as_ref()
            .and_then(|usage| usage.request.known_usage()),
        Some(&this_turn),
        "{} turn {turn_index}: request usage must match this turn usage for one-request backend turn",
        backend_label(backend_kind)
    );
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
                    ChatEvent::TypingStatusChanged(false) => {
                        if got_user_message_echo {
                            saw_typing_false = true;
                        }
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
    let mut fixture = RealBackendFixture::new().await;
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
    let mut fixture = RealBackendFixture::new().await;
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
    let mut saw_typing_false = false;

    // TypingStatusChanged(true) must arrive before StreamStart, and both StreamEnd and
    // TypingStatusChanged(false) must arrive after StreamStart.  However the relative
    // order of StreamEnd vs TypingStatusChanged(false) is backend-dependent (e.g. Codex
    // may emit TypingStatusChanged(false) before StreamEnd), so we wait until we have
    // seen *both* before breaking.
    loop {
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
                    if saw_typing_false {
                        break;
                    }
                }
            }
            ChatEvent::TypingStatusChanged(false) if got_user_message_echo && saw_typing_true => {
                saw_typing_false = true;
                if saw_stream_end {
                    break;
                }
            }
            _ => {}
        }
    }

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
                    matches!(
                        data.message
                            .token_usage
                            .as_ref()
                            .map(|usage| &usage.request),
                        Some(TokenUsageScope::Unavailable {
                            reason: TokenUsageUnavailableReason::BackendDidNotReport
                        })
                    ),
                    "Codex StreamEnd should leave late token usage for MessageMetadataUpdated; message_id={message_id}"
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
            && !tool_requests.is_empty()
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
        BackendKind::Kiro => {
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

    let mut fixture = RealBackendFixture::new().await;
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

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_turn_token_usage_contract_if_reported() {
    let backend_kind = BackendKind::Kiro;
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
            cost_hint: cost_hint_for(BackendKind::Hermes),
            custom_agent_id: None,
            startup_mcp_servers: Vec::new(),
            session_settings: Some(settings),
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
            let mut fixture = RealBackendFixture::new().await;
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
        BackendKind::Kiro,
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
            let mut fixture = RealBackendFixture::new().await;
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
        BackendKind::Kiro,
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
            let mut fixture = RealBackendFixture::new().await;
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
        let mut fixture = RealBackendFixture::new().await;
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

        let child_new = expect_backend_native_child_for_parent(
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
            let mut fixture = RealBackendFixture::new().await;
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

    let mut fixture = RealBackendFixture::new().await;
    assert_codex_emits_token_usage(&mut fixture).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_backends_interrupt_long_running_command() {
    let backends = [BackendKind::Claude, BackendKind::Codex, BackendKind::Kiro];
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
            let mut fixture = RealBackendFixture::new().await;
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
    let backend_kind = BackendKind::Kiro;

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

    let mut fixture = RealBackendFixture::new().await;
    assert_backend_emits_typing_and_streaming_on_follow_up_turns(&mut fixture, backend_kind).await;
}

#[tokio::test]
#[ignore = "real AI backend test; use --ignored and TYDE_RUN_REAL_AI_TESTS=1"]
async fn real_kiro_follow_up_user_message_echo_is_not_duplicated() {
    let backend_kind = BackendKind::Kiro;

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

    let mut fixture = RealBackendFixture::new().await;
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
            let mut fixture = RealBackendFixture::new().await;
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
            let mut fixture = RealBackendFixture::new().await;
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
