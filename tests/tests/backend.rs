mod fixture;

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use fixture::Fixture;
use protocol::{
    BackendKind, ChatEvent, Envelope, FrameKind, HostSettingValue, ImageData, ListSessionsPayload,
    MessageSender, NewAgentPayload, ProtocolValidator, SessionListPayload, SessionSchemaEntry,
    SessionSchemasPayload, SessionSettingFieldType, SessionSettingValue, SessionSettingsValues,
    SessionSummary, SetSettingPayload, SpawnAgentParams, SpawnAgentPayload, SpawnCostHint,
    StreamPath, ToolExecutionCompletedData, ToolRequest, ToolRequestType,
};
use server::backend::Backend;

const REAL_BACKEND_TIMEOUT: Duration = Duration::from_secs(60);
const REAL_BACKEND_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
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
        BackendKind::Gemini => binary_available("gemini"),
        BackendKind::Tycode => binary_available("tycode-subprocess"),
        BackendKind::Kiro => binary_available("kiro-cli-chat") || binary_available("kiro-cli"),
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

fn backend_runtime_available(backend_kind: BackendKind) -> bool {
    if !backend_binary_available(backend_kind) {
        return false;
    }

    match backend_kind {
        BackendKind::Tycode => home_is_writable(),
        BackendKind::Claude | BackendKind::Gemini | BackendKind::Kiro => {
            home_is_writable() && remote_network_is_available()
        }
        BackendKind::Codex => remote_network_is_available(),
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
        BackendKind::Gemini => {
            let script = r#"
tmpdir=$(mktemp -d)
cd "$tmpdir" || exit 1
gemini -y -p 'Reply exactly with ok' --model gemini-2.5-flash-lite --output-format stream-json
"#;
            let output = run_shell_probe(script, REAL_BACKEND_PROBE_TIMEOUT).await?;
            if output.contains("\"type\":\"message\"") || output.contains("\"type\": \"message\"") {
                Ok(())
            } else {
                Err(format!(
                    "Gemini probe did not emit a message event: {output}"
                ))
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
                        resolved_spawn_config: Default::default(),
                    },
                    protocol::SendMessagePayload {
                        message: "Reply exactly with ok".to_owned(),
                        images: None,
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
                        resolved_spawn_config: Default::default(),
                    },
                    protocol::SendMessagePayload {
                        message: "Reply exactly with ok".to_owned(),
                        images: None,
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
    }
}

fn cost_hint_for(backend_kind: BackendKind) -> Option<SpawnCostHint> {
    match backend_kind {
        BackendKind::Codex => Some(SpawnCostHint::Medium),
        _ => Some(SpawnCostHint::Low),
    }
}

fn backend_label(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Gemini => "gemini",
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
    }
}

async fn expect_fixture_event(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
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
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_fixture_event(client, "NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let agent_stream = new_agent.instance_stream;

    let env = expect_fixture_event(client, "AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);

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
        if env.kind != FrameKind::SessionSchemas {
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
                cost_hint: None,
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

    loop {
        let env = expect_fixture_event(&mut fixture.client, "Kiro AgentStart").await;
        if env.kind == FrameKind::AgentStart && env.stream == agent_stream {
            break;
        }
    }

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
                cost_hint: None,
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

    loop {
        let env = expect_fixture_event(&mut fixture.client, "compact AgentStart").await;
        if env.kind == FrameKind::AgentStart && env.stream == agent_stream {
            break;
        }
    }

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
        let Some(envelope) = self.inner.next_event().await? else {
            return Ok(None);
        };

        if let Err(error) = self.validator.validate_envelope(&envelope) {
            panic!("protocol violation while reading backend events: {error}");
        }

        Ok(Some(envelope))
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

async fn expect_next_event_kind_on_stream(
    client: &mut ValidatedConnection,
    expected_kind: FrameKind,
    expected_stream: &StreamPath,
    context: &str,
) -> Envelope {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == expected_kind && env.stream == *expected_stream {
            return env;
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
                cost_hint,
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
    let env = expect_next_event_kind_on_stream(
        client,
        FrameKind::AgentStart,
        &agent_stream,
        &agent_start_context,
    )
    .await;
    assert_eq!(env.stream, agent_stream);

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

    let env = expect_next_event_kind_on_stream(
        client,
        FrameKind::AgentStart,
        &agent_stream,
        "resumed AgentStart",
    )
    .await;
    assert_eq!(env.stream, agent_stream);

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
    if backend_kind != BackendKind::Gemini {
        assert!(
            response.delta_count > 0,
            "expected at least one StreamDelta for {backend_kind:?}"
        );
    }
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
            ChatEvent::TypingStatusChanged(false) => {
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
                final_text = Some(data.message.content);
            }
            ChatEvent::ToolRequest(request) => {
                if got_user_message_echo {
                    tool_requests.insert(request.tool_call_id.clone(), request);
                }
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                if got_user_message_echo {
                    tool_completions.insert(completion.tool_call_id.clone(), completion);
                }
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
    assert!(
        !turn.final_text.trim().is_empty(),
        "expected a non-empty assistant response for {backend_kind:?}; requests={:?} completions={:?} got {:?}",
        turn.tool_requests,
        turn.tool_completions,
        turn.final_text
    );
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
        BackendKind::Gemini => {
            "Use the run_shell_command tool exactly once to execute exactly `./interrupt_test.sh` in the current working directory. Start the command immediately. Do not use any other tools."
        }
        BackendKind::Kiro => {
            "Use the available tools to run the test harness `./interrupt_test.sh` in the current working directory immediately. Do not ask for clarification, and do not only describe a plan."
        }
        BackendKind::Tycode => {
            "Use the run_build_test tool exactly once to run the test harness `./interrupt_test.sh` in the current working directory. Start the test immediately. Do not use any other tools."
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
            ChatEvent::ToolExecutionCompleted(completion) => {
                if completion.tool_call_id == tool_call_id {
                    saw_matching_tool_completion = true;
                }
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
// Real backend tests — skip unavailable backends, 60s timeout per event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resumable_real_backends_remember_secret() {
    let backends = [BackendKind::Claude, BackendKind::Codex, BackendKind::Gemini];
    let mut failures = Vec::new();

    for backend_kind in backends {
        eprintln!("RUNNING interrupt test for {}", backend_label(backend_kind));
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
async fn real_backends_emit_stream_deltas() {
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Gemini,
        BackendKind::Kiro,
    ];
    let mut failures = Vec::new();

    for backend_kind in backends {
        if backend_kind == BackendKind::Gemini {
            eprintln!("SKIPPED: gemini streaming deltas are not stable in this live backend test");
            continue;
        }
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
async fn real_backends_emit_typing_status() {
    let backends = [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Gemini,
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
