mod fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentClosedPayload, AgentErrorPayload,
    AgentStartPayload, BackendConfigSnapshotStatus, BackendConfigSnapshotsPayload,
    BackendConfigValues, BackendKind, BackendNativeSettingsAdvisory,
    BackendNativeSettingsGroupKind, BackendNativeSettingsProvenance, BackendSetupAction,
    BackendSetupDiagnosticCode, BackendSetupPayload, BackendSetupStatus, ChatEvent,
    CodeIntelProviderId, CommandErrorCode, CommandErrorPayload, Envelope, FrameKind,
    HostExecutablePath, HostSettingValue, HostSettings, HostSettingsPayload, ListSessionsPayload,
    NewAgentPayload, NewTerminalPayload, RunBackendSetupPayload, SessionId, SessionListPayload,
    SessionSettingValue, SetSettingPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
    TerminalBootstrapPayload, TerminalExitPayload, TerminalOutputPayload,
    TycodeManagedProjectionRecoveryState, TycodeProjectionId, TycodeProjectionSource,
    TycodeProjectionStateHash,
};
use server::backend::BackendSession;
use server::store::session::SessionStore;
use server::store::settings::HostSettingsStore;
use tokio::sync::Mutex;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
        match self.old_value.take() {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}

async fn expect_no_backend_setup_replay(client: &mut client::Connection) {
    match tokio::time::timeout(Duration::from_millis(100), client.next_event()).await {
        Err(_) | Ok(Ok(None)) => {}
        Ok(Ok(Some(env))) if env.kind == FrameKind::BackendSetup => {
            panic!("backend_setup should be bundled in HostBootstrap, not replayed afterward")
        }
        Ok(Ok(Some(_))) => {}
        Ok(Err(err)) => panic!("next_event failed after HostBootstrap: {err:?}"),
    }
}

async fn expect_backend_config_snapshots(
    client: &mut client::Connection,
    context: &str,
) -> BackendConfigSnapshotsPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "timed out waiting for {context}");
        let env = match tokio::time::timeout(deadline - now, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if env.kind == FrameKind::BackendConfigSnapshots {
            return env.parse_payload().unwrap_or_else(|err| {
                panic!("failed to parse BackendConfigSnapshots for {context}: {err}")
            });
        }
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse CommandError for {context}: {err}"));
            panic!("unexpected CommandError before {context}: {error:?}");
        }
    }
}

async fn next_required_event(
    client: &mut client::Connection,
    deadline: tokio::time::Instant,
    context: &str,
) -> Envelope {
    let now = tokio::time::Instant::now();
    assert!(now < deadline, "timed out waiting for {context}");
    let env = match tokio::time::timeout(deadline - now, client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    };
    if env.kind == FrameKind::CommandError {
        let error: CommandErrorPayload = env
            .parse_payload()
            .unwrap_or_else(|err| panic!("failed to parse CommandError for {context}: {err}"));
        panic!("unexpected CommandError before {context}: {error:?}");
    }
    env
}

async fn expect_tycode_agent_launch(
    client: &mut client::Connection,
    fake: &FakeTycode,
    expected_new_agent_session: Option<&str>,
    context: &str,
) -> (NewAgentPayload, AgentStartPayload) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut client_trace = Vec::new();
    let mut new_agent = None;
    let mut starts = Vec::<(StreamPath, AgentStartPayload)>::new();
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for {context}; client trace: {client_trace:#?}; fake trace: {:#?}",
            fake.events()
        );
        let env = match tokio::time::timeout(deadline - now, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!(
                "connection closed before {context}; client trace: {client_trace:#?}; fake trace: {:#?}",
                fake.events()
            ),
            Ok(Err(err)) => panic!(
                "next_event failed before {context}: {err:?}; client trace: {client_trace:#?}; fake trace: {:#?}",
                fake.events()
            ),
            Err(_) => panic!(
                "timed out waiting for {context}; client trace: {client_trace:#?}; fake trace: {:#?}",
                fake.events()
            ),
        };
        client_trace.push(format!("kind={} stream={}", env.kind, env.stream));
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse CommandError for {context}: {err}"));
            panic!(
                "unexpected CommandError before {context}: {error:?}; client trace: {client_trace:#?}; fake trace: {:#?}",
                fake.events()
            );
        }
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .unwrap_or_else(|err| panic!("failed to parse NewAgent for {context}: {err}"));
                client_trace.push(format!(
                    "NewAgent agent_id={} backend={:?} session_id={:?} instance_stream={}",
                    payload.agent_id,
                    payload.backend_kind,
                    payload.session_id,
                    payload.instance_stream
                ));
                let session_matches = expected_new_agent_session.is_none_or(|session_id| {
                    payload.session_id.as_ref().map(|id| id.0.as_str()) == Some(session_id)
                });
                if payload.backend_kind == BackendKind::Tycode && session_matches {
                    new_agent = Some(payload);
                }
            }
            FrameKind::AgentStart => {
                let start: AgentStartPayload = env.parse_payload().unwrap_or_else(|err| {
                    panic!("failed to parse AgentStart for {context}: {err}")
                });
                client_trace.push(format!(
                    "AgentStart agent_id={} session_id={:?} stream={}",
                    start.agent_id, start.session_id, env.stream
                ));
                starts.push((env.stream, start));
            }
            FrameKind::AgentBootstrap => {
                let bootstrap: AgentBootstrapPayload = env.parse_payload().unwrap_or_else(|err| {
                    panic!("failed to parse AgentBootstrap for {context}: {err}")
                });
                for event in bootstrap.events {
                    match event {
                        AgentBootstrapEvent::AgentStart(start) => {
                            client_trace.push(format!(
                                "AgentBootstrap::AgentStart agent_id={} session_id={:?} stream={}",
                                start.agent_id, start.session_id, env.stream
                            ));
                            starts.push((env.stream.clone(), start));
                        }
                        AgentBootstrapEvent::AgentError(error) => {
                            panic!(
                                "unexpected AgentBootstrap::AgentError before {context}: {error:?}; client trace: {client_trace:#?}; fake trace: {:#?}",
                                fake.events()
                            );
                        }
                        _ => {}
                    }
                }
            }
            FrameKind::AgentError => {
                let error: AgentErrorPayload = env.parse_payload().unwrap_or_else(|err| {
                    panic!("failed to parse AgentError for {context}: {err}")
                });
                panic!(
                    "unexpected AgentError before {context}: {error:?}; client trace: {client_trace:#?}; fake trace: {:#?}",
                    fake.events()
                );
            }
            _ => {}
        }
        if let Some(new_agent) = new_agent.as_ref()
            && let Some(index) = starts
                .iter()
                .position(|(stream, _)| stream == &new_agent.instance_stream)
        {
            let (_, start) = starts.swap_remove(index);
            return (new_agent.clone(), start);
        }
    }
}

async fn expect_session_list(client: &mut client::Connection, context: &str) -> SessionListPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let env = next_required_event(client, deadline, context).await;
        if env.kind == FrameKind::SessionList {
            return env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse SessionList for {context}: {err}"));
        }
    }
}

async fn expect_tycode_turn_quiescent(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_stream_end = false;
    loop {
        let env = next_required_event(client, deadline, context).await;
        if env.stream != *stream {
            continue;
        }
        match env.kind {
            FrameKind::ChatEvent => {
                let event: ChatEvent = env
                    .parse_payload()
                    .unwrap_or_else(|err| panic!("failed to parse ChatEvent for {context}: {err}"));
                match event {
                    ChatEvent::StreamEnd(_) => saw_stream_end = true,
                    ChatEvent::TypingStatusChanged(false) if saw_stream_end => return,
                    _ => {}
                }
            }
            FrameKind::AgentError => {
                let error: AgentErrorPayload = env.parse_payload().unwrap_or_else(|err| {
                    panic!("failed to parse AgentError for {context}: {err}")
                });
                panic!("unexpected AgentError before {context}: {error:?}");
            }
            _ => {}
        }
    }
}

async fn expect_agent_closed(
    client: &mut client::Connection,
    expected_agent_id: &protocol::AgentId,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let env = next_required_event(client, deadline, context).await;
        if env.kind == FrameKind::AgentClosed {
            let closed: AgentClosedPayload = env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse AgentClosed for {context}: {err}"));
            assert_eq!(&closed.agent_id, expected_agent_id);
            return;
        }
        if env.kind == FrameKind::AgentError {
            let error: AgentErrorPayload = env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse AgentError for {context}: {err}"));
            panic!("unexpected AgentError before {context}: {error:?}");
        }
    }
}

struct FakeTycode {
    binary: PathBuf,
    behavior: PathBuf,
    log: PathBuf,
}

impl FakeTycode {
    fn set_behavior(&self, behavior: serde_json::Value) {
        std::fs::write(
            &self.behavior,
            serde_json::to_vec(&behavior).expect("serialize fake Tycode behavior"),
        )
        .expect("write fake Tycode behavior");
    }

    fn events(&self) -> Vec<serde_json::Value> {
        match std::fs::read_to_string(&self.log) {
            Ok(body) => body
                .lines()
                .map(|line| serde_json::from_str(line).expect("parse fake Tycode log event"))
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => panic!("read fake Tycode log: {err}"),
        }
    }
}

fn write_fake_tycode_binary(home: &Path) -> FakeTycode {
    let path = home
        .join(".tyde")
        .join("tycode")
        .join("0.10.0")
        .join("tycode-subprocess");
    std::fs::create_dir_all(path.parent().expect("fake Tycode parent"))
        .expect("create fake Tycode install dir");
    let parent = path.parent().expect("fake Tycode parent");
    let behavior = parent.join("behavior.json");
    let log = parent.join("events.jsonl");
    std::fs::write(&behavior, b"{}").expect("write default fake Tycode behavior");
    let behavior_literal =
        serde_json::to_string(&behavior.to_string_lossy()).expect("behavior path literal");
    let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log path literal");
    let python = python_with_stdlib_toml();
    let body = r#"#!__PYTHON__
import copy
import json
import os
import sys
import tomllib

behavior_path = __BEHAVIOR_PATH__
log_path = __LOG_PATH__
with open(behavior_path, "r", encoding="utf-8") as behavior_file:
    behavior = json.load(behavior_file)

def log(value):
    with open(log_path, "a", encoding="utf-8") as log_file:
        log_file.write(json.dumps(value, separators=(",", ":")) + "\n")

if "--version" in sys.argv:
    log({"type": "version", "pid": os.getpid(), "argv": sys.argv[1:]})
    print(behavior.get("version_output", "tycode-subprocess 0.10.0"))
    sys.exit(behavior.get("version_exit_code", 0))

settings_paths = [
    sys.argv[index + 1]
    for index, argument in enumerate(sys.argv[:-1])
    if argument == "--settings-path"
]
if len(settings_paths) != 1:
    print("expected exactly one --settings-path", file=sys.stderr)
    sys.exit(64)
settings_path = settings_paths[0]

log({
    "type": "spawn",
    "pid": os.getpid(),
    "argv": sys.argv[1:],
    "settings_path": settings_path,
    "settings_existed_before": os.path.exists(settings_path),
})

defaults = {
    "active_provider": None,
    "providers": {},
    "agent_models": {},
    "default_agent": "tycode",
    "model_quality": None,
    "review_level": "None",
    "max_review_rounds": 3,
    "fanout_concurrency": 4,
    "orchestration_mode": "auto",
    "orchestration_progress_messages": True,
    "swarm_models": [],
    "mcp_servers": {},
    "spawn_context_mode": "Fork",
    "disable_custom_steering": False,
    "communication_tone": "concise_and_logical",
    "autonomy_level": "fully_autonomous",
    "voice": {
        "default_tts": None,
        "default_stt": None,
        "tts_providers": {},
        "stt_providers": {},
    },
    "skills": {
        "enabled": True,
        "disabled_skills": [],
        "additional_dirs": [],
        "enable_claude_code_compat": True,
    },
    "reasoning_effort": None,
    "disable_streaming": False,
    "modules": {},
}
known_top_level = set(defaults.keys())

def normalize_provider(provider):
    if not isinstance(provider, dict):
        return None
    provider_type = provider.get("type")
    if provider_type == "bedrock" and isinstance(provider.get("profile"), str):
        normalized = {
            "type": "bedrock",
            "profile": provider["profile"],
            "region": provider.get("region", "us-west-2"),
        }
        if isinstance(provider.get("mantle_region"), str):
            normalized["mantle_region"] = provider["mantle_region"]
        return normalized
    if provider_type == "mock":
        return {
            "type": "mock",
            "behavior": provider.get("behavior", "success"),
        }
    if provider_type == "openrouter" and isinstance(provider.get("api_key"), str):
        return {"type": "openrouter", "api_key": provider["api_key"]}
    return None

def normalize_tts_provider(provider):
    if not isinstance(provider, dict):
        return None
    provider_type = provider.get("type")
    if provider_type == "aws_polly":
        return {
            "type": "aws_polly",
            "profile": provider.get("profile"),
            "region": provider.get("region", "us-west-2"),
        }
    if provider_type == "elevenlabs" and isinstance(provider.get("api_key"), str):
        return {
            "type": "elevenlabs",
            "api_key": provider["api_key"],
            "voice_id": provider.get("voice_id"),
            "model_id": provider.get("model_id"),
        }
    return None

def normalize_stt_provider(provider):
    if not isinstance(provider, dict):
        return None
    provider_type = provider.get("type")
    if provider_type == "aws_transcribe":
        return {
            "type": "aws_transcribe",
            "profile": provider.get("profile"),
            "region": provider.get("region", "us-west-2"),
        }
    if provider_type == "elevenlabs" and isinstance(provider.get("api_key"), str):
        return {
            "type": "elevenlabs",
            "api_key": provider["api_key"],
            "model_id": provider.get("model_id"),
        }
    return None

def normalize(value):
    if not isinstance(value, dict):
        raise ValueError("settings must be an object")
    normalized = {
        key: value[key]
        for key in known_top_level
        if key in value
    }
    normalized.setdefault("providers", {})
    if not isinstance(normalized["providers"], dict):
        raise ValueError("providers must be an object")
    normalized["providers"] = {
        name: recognized
        for name, provider in normalized["providers"].items()
        if (recognized := normalize_provider(provider)) is not None
    }
    for key, default in defaults.items():
        normalized.setdefault(key, copy.deepcopy(default))
    for key in ["voice", "skills"]:
        if not isinstance(normalized[key], dict):
            raise ValueError(f"{key} must be an object")
        for nested_key, nested_default in defaults[key].items():
            normalized[key].setdefault(nested_key, copy.deepcopy(nested_default))
    for key, normalizer in [
        ("tts_providers", normalize_tts_provider),
        ("stt_providers", normalize_stt_provider),
    ]:
        providers = normalized["voice"][key]
        if not isinstance(providers, dict):
            raise ValueError(f"voice.{key} must be an object")
        normalized["voice"][key] = {
            name: recognized
            for name, provider in providers.items()
            if (recognized := normalizer(provider)) is not None
        }
    return normalized

def is_empty_for_persistence(value):
    comparable = copy.deepcopy(value)
    default_agent = comparable.get("default_agent")
    if isinstance(default_agent, str) and not default_agent.strip():
        comparable["default_agent"] = defaults["default_agent"]
    return comparable == defaults

def toml_key(value):
    bare_key_characters = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-"
    if value and all(character in bare_key_characters for character in value):
        return value
    return json.dumps(value)

def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        entries = [
            f"{toml_key(key)} = {toml_value(item)}"
            for key, item in value.items()
            if item is not None
        ]
        return "{ " + ", ".join(entries) + " }"
    raise TypeError(f"unsupported TOML value: {type(value)}")

def write_toml_table(lines, prefix, table):
    if prefix:
        lines.append("[" + ".".join(toml_key(part) for part in prefix) + "]")
    for key, value in table.items():
        if value is not None and not isinstance(value, dict):
            lines.append(f"{toml_key(key)} = {toml_value(value)}")
    for key, value in table.items():
        if isinstance(value, dict):
            if lines and lines[-1] != "":
                lines.append("")
            write_toml_table(lines, prefix + [key], value)

def persist_toml(value):
    lines = []
    write_toml_table(lines, [], value)
    with open(settings_path, "w", encoding="utf-8") as settings_file:
        settings_file.write("\n".join(lines).rstrip() + "\n")

try:
    with open(settings_path, "rb") as settings_file:
        settings = normalize(tomllib.load(settings_file))
except FileNotFoundError:
    settings = copy.deepcopy(defaults)
    persist_toml(settings)

mismatch_marker = settings_path + ".verify-mismatch"
if behavior.get("mismatch_on_fresh_process") and os.path.exists(mismatch_marker):
    settings = dict(settings)
    settings["model_quality"] = "fresh-process-mismatch"
    os.remove(mismatch_marker)

groups = [
    {
        "id": "general",
        "title": "General",
        "kind": "core",
        "settings_path": [],
        "description": "General Tycode settings",
        "schema": {
            "type": "object",
            "properties": {
                "active_provider": {"type": ["string", "null"]},
                "default_agent": {"type": "string"},
                "model_quality": {"type": ["string", "null"]},
                "reasoning_effort": {"type": ["string", "null"]},
                "review_level": {"type": "string"},
                "orchestration_mode": {"type": "string"},
            },
        },
    },
    {
        "id": "providers",
        "title": "Providers",
        "kind": "core",
        "settings_path": ["providers"],
        "description": "Tycode provider settings",
        "schema": {
            "type": "object",
            "additionalProperties": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "type": {"const": "bedrock"},
                            "profile": {"type": "string"},
                            "region": {"type": "string"},
                            "mantle_region": {"type": "string"},
                        },
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": {"const": "mock"},
                            "behavior": {"type": "string"},
                        },
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": {"const": "openrouter"},
                            "api_key": {"type": "string"},
                        },
                    },
                ],
            },
        },
    },
    {
        "id": "mcp_servers",
        "title": "MCP Servers",
        "kind": "core",
        "settings_path": ["mcp_servers"],
        "description": "Tycode MCP server settings",
        "schema": {"type": "object", "additionalProperties": {"type": "object"}},
    },
    {
        "id": "agent_models",
        "title": "Agent Models",
        "kind": "core",
        "settings_path": ["agent_models"],
        "description": "Tycode agent model overrides",
        "schema": {"type": "object", "additionalProperties": {"type": "object"}},
    },
    {
        "id": "advanced",
        "title": "Advanced",
        "kind": "core",
        "settings_path": [],
        "description": "Advanced Tycode settings",
        "schema": {
            "type": "object",
            "properties": {
                "max_review_rounds": {"type": "integer"},
                "fanout_concurrency": {"type": "integer"},
                "orchestration_progress_messages": {"type": "boolean"},
                "swarm_models": {"type": "array"},
                "spawn_context_mode": {"type": "string"},
                "disable_custom_steering": {"type": "boolean"},
                "communication_tone": {"type": "string"},
                "autonomy_level": {"type": "string"},
                "voice": {"type": "object"},
                "skills": {"type": "object"},
                "disable_streaming": {"type": "boolean"},
            },
        },
    },
    {
        "id": "module:execution",
        "title": "Execution",
        "kind": "module",
        "settings_path": ["modules", "execution"],
        "description": "Execution module settings",
        "schema": {
            "type": "object",
            "properties": {
                "enabled": {"type": "boolean"},
            },
        },
    },
]

def emit(value):
    data = value.get("data") if isinstance(value, dict) else None
    log({
        "type": "emit",
        "pid": os.getpid(),
        "kind": value.get("kind") if isinstance(value, dict) else None,
        "session_id": data.get("session_id") if isinstance(data, dict) else None,
    })
    print(json.dumps(value, separators=(",", ":")), flush=True)

def message(sender, content):
    return {
        "kind": "MessageAdded",
        "data": {
            "timestamp": 1,
            "sender": sender,
            "content": content,
            "reasoning": None,
            "tool_calls": [],
            "model_info": None,
            "token_usage": None,
            "context_breakdown": None,
            "images": [],
        },
    }

if behavior.get("pre_session_advisory"):
    emit(message("Error", "No AI provider is configured. Configure one now."))

emit({"kind":"SessionStarted","data":{"session_id":"fake-session"}})

for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    command = json.loads(line)
    log({"type": "command", "pid": os.getpid(), "command": command})
    if command == "GetSettings":
        emit({"kind":"Settings","data":settings})
    elif command == "GetSettingsSchema":
        if behavior.get("post_command_error"):
            emit(message("Error", "schema command failed after SessionStarted"))
        elif behavior.get("exit_before_schema"):
            sys.exit(1)
        else:
            schema_settings = dict(settings)
            schema_settings["profile"] = "default"
            emit({
                "kind": "SettingsSchema",
                "data": {
                    "schema": {
                        "settings": schema_settings,
                        "groups": groups,
                    },
                },
            })
    elif isinstance(command, dict) and "SaveSettings" in command:
        save = command["SaveSettings"]
        settings = normalize({
            key: value for key, value in save["settings"].items() if key != "profile"
        })
        if save.get("persist") is True:
            if is_empty_for_persistence(settings):
                emit({"kind": "Error", "data": "Refusing to persist empty settings"})
                continue
            persist_toml(settings)
            if behavior.get("mismatch_on_fresh_process"):
                with open(mismatch_marker, "w", encoding="utf-8") as marker_file:
                    marker_file.write("mismatch")
        else:
            sessions_dir = os.path.join(os.path.dirname(settings_path), "sessions")
            os.makedirs(sessions_dir, exist_ok=True)
            with open(os.path.join(sessions_dir, "fake-session.json"), "w", encoding="utf-8") as session_file:
                json.dump({
                    "id": "fake-session",
                    "created_at": 1,
                    "last_modified": 2,
                    "messages": [],
                    "events": [],
                }, session_file, separators=(",", ":"))
    elif isinstance(command, dict) and "ChangeProvider" in command:
        emit(message("System", f"Switched to provider: {command['ChangeProvider']}"))
    elif isinstance(command, dict) and "SetRootAgent" in command:
        emit({"kind": "RootAgentChanged", "data": {"agent": command["SetRootAgent"]["agent"]}})
    elif isinstance(command, dict) and "UserInput" in command:
        sessions_dir = os.path.join(os.path.dirname(settings_path), "sessions")
        os.makedirs(sessions_dir, exist_ok=True)
        with open(os.path.join(sessions_dir, "fake-session.json"), "w", encoding="utf-8") as session_file:
            json.dump({
                "id": "fake-session",
                "created_at": 1,
                "last_modified": 2,
                "messages": [],
                "events": [],
            }, session_file, separators=(",", ":"))
        emit({"kind": "TypingStatusChanged", "data": True})
        emit({
            "kind": "StreamStart",
            "data": {
                "message_id": "fake-message",
                "agent": "tycode",
                "model": "ClaudeSonnet46",
            },
        })
        emit({"kind": "TypingStatusChanged", "data": False})
    elif isinstance(command, dict) and "ResumeSession" in command:
        emit({"kind": "SessionStarted", "data": {"session_id": command["ResumeSession"]["session_id"]}})
        emit({"kind": "ConversationCleared"})
    elif command == "ListSessions":
        emit({"kind": "SessionsList", "data": {"sessions": []}})
"#
    .replace("__PYTHON__", &python.to_string_lossy())
    .replace("__BEHAVIOR_PATH__", &behavior_literal)
    .replace("__LOG_PATH__", &log_literal);
    std::fs::write(&path, body).expect("write fake Tycode binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .expect("stat fake Tycode binary")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod fake Tycode binary");
    }
    FakeTycode {
        binary: path,
        behavior,
        log,
    }
}

fn python_with_stdlib_toml() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON
        .get_or_init(|| {
            let path = std::env::var_os("PATH").expect("PATH for fake Tycode interpreter");
            for directory in std::env::split_paths(&path) {
                for name in [
                    "python3",
                    "python3.14",
                    "python3.13",
                    "python3.12",
                    "python3.11",
                ] {
                    let candidate = directory.join(name);
                    if !candidate.is_file() {
                        continue;
                    }
                    let Ok(output) = std::process::Command::new(&candidate)
                        .args(["-c", "import tomllib"])
                        .output()
                    else {
                        continue;
                    };
                    if output.status.success() {
                        let candidate = candidate
                            .canonicalize()
                            .expect("canonicalize fake Tycode Python interpreter");
                        assert!(
                            !candidate.to_string_lossy().chars().any(char::is_whitespace),
                            "fake Tycode Python interpreter path must fit in a shebang"
                        );
                        return candidate;
                    }
                }
            }
            panic!("fake Tycode requires Python 3.11+ with the standard-library tomllib module")
        })
        .as_path()
}

fn write_shared_tycode_settings(home: &Path, settings: &str) -> Vec<u8> {
    let directory = home.join(".tycode");
    std::fs::create_dir_all(&directory).expect("create shared Tycode settings directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))
            .expect("set conventional shared Tycode settings directory mode");
    }
    let bytes = settings.as_bytes().to_vec();
    std::fs::write(directory.join("settings.toml"), &bytes).expect("write shared Tycode settings");
    bytes
}

fn tycode_native_snapshot(
    payload: &BackendConfigSnapshotsPayload,
) -> &protocol::BackendNativeSettingsSnapshot {
    payload
        .native_settings
        .iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Tycode)
        .expect("Tycode native settings snapshot")
}

fn managed_projection_fields(
    snapshot: &protocol::BackendNativeSettingsSnapshot,
) -> (
    &protocol::HostAbsPath,
    &TycodeProjectionId,
    TycodeProjectionSource,
    bool,
) {
    let BackendNativeSettingsProvenance::TycodeManagedProjection {
        managed_settings_path,
        source,
        projection_id,
        notice_pending,
        ..
    } = snapshot
        .provenance
        .as_ref()
        .expect("Tycode managed projection provenance");
    (
        managed_settings_path,
        projection_id,
        *source,
        *notice_pending,
    )
}

fn managed_recovery_fields(
    snapshot: &protocol::BackendNativeSettingsSnapshot,
) -> (&str, &TycodeProjectionId, &TycodeProjectionStateHash) {
    let TycodeManagedProjectionRecoveryState::ManagedProjectionResetRequired {
        reason,
        expected_projection_id,
        expected_state_hash,
    } = snapshot
        .managed_projection_recovery
        .as_ref()
        .expect("typed Tycode managed projection recovery state");
    (reason, expected_projection_id, expected_state_hash)
}

async fn send_host_payload<T: serde::Serialize>(
    client: &mut client::Connection,
    kind: FrameKind,
    payload: &T,
) {
    let host_stream = client
        .outgoing_seq
        .keys()
        .find(|stream| stream.0.starts_with("/host/"))
        .cloned()
        .expect("client host stream");
    let seq = client
        .outgoing_seq
        .get(&host_stream)
        .copied()
        .expect("client host stream sequence");
    let envelope = Envelope::from_payload(host_stream.clone(), kind, seq, payload)
        .expect("encode host payload");
    client.outgoing_seq.insert(host_stream, seq + 1);
    protocol::write_envelope(&mut client.writer, &envelope)
        .await
        .expect("send host payload");
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    loop {
        let env = client
            .next_event()
            .await
            .unwrap_or_else(|err| panic!("next_event failed before {context}: {err:?}"))
            .unwrap_or_else(|| panic!("connection closed before {context}"));
        if env.kind == FrameKind::CommandError {
            return env
                .parse_payload()
                .unwrap_or_else(|err| panic!("parse CommandError for {context}: {err}"));
        }
    }
}

async fn expect_command_error_and_backend_config(
    client: &mut client::Connection,
    context: &str,
) -> (CommandErrorPayload, BackendConfigSnapshotsPayload) {
    let mut error = None;
    let mut snapshots = None;
    while error.is_none() || snapshots.is_none() {
        let env = client
            .next_event()
            .await
            .unwrap_or_else(|err| panic!("next_event failed before {context}: {err:?}"))
            .unwrap_or_else(|| panic!("connection closed before {context}"));
        match env.kind {
            FrameKind::CommandError => {
                error = Some(
                    env.parse_payload()
                        .unwrap_or_else(|err| panic!("parse CommandError for {context}: {err}")),
                );
            }
            FrameKind::BackendConfigSnapshots => {
                snapshots = Some(env.parse_payload().unwrap_or_else(|err| {
                    panic!("parse BackendConfigSnapshots for {context}: {err}")
                }));
            }
            _ => {}
        }
    }
    (
        error.expect("command error collected"),
        snapshots.expect("backend snapshots collected"),
    )
}

fn write_fake_hermes_install(home: &Path) -> PathBuf {
    let project = home.join(".hermes").join("hermes-agent");
    std::fs::create_dir_all(&project).expect("create fake Hermes project");
    let python = home.join(".hermes").join("fake_python");
    let console = home.join(".hermes").join("hermes_console");
    std::fs::write(
        &python,
        "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\nexit 1\n",
    )
    .expect("write fake Hermes python");
    std::fs::write(
        &console,
        format!("#!{}\nimport sys\nsys.exit(1)\n", python.to_string_lossy()),
    )
    .expect("write fake Hermes console script");
    let hermes = home.join(".local").join("bin").join("hermes");
    std::fs::create_dir_all(hermes.parent().expect("fake Hermes bin parent"))
        .expect("create fake Hermes bin");
    std::fs::write(
        &hermes,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'Hermes Agent v9.9.9\\nProject: {}\\n'\n  exit 0\nfi\nexec '{}' \"$@\"\n",
            project.to_string_lossy(),
            console.to_string_lossy().replace('\'', "'\\''")
        ),
    )
    .expect("write fake Hermes executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&python, &console, &hermes] {
            let mut perms = std::fs::metadata(path)
                .expect("stat fake Hermes executable")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("chmod fake Hermes executable");
        }
    }
    hermes
}

fn write_unusable_hermes_cli(home: &Path) -> PathBuf {
    let project = home.join(".hermes").join("hermes-agent");
    std::fs::create_dir_all(&project).expect("create unusable Hermes project");
    let hermes = home.join(".local").join("bin").join("hermes");
    std::fs::create_dir_all(hermes.parent().expect("fake Hermes bin parent"))
        .expect("create fake Hermes bin");
    std::fs::write(
        &hermes,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'Hermes Agent v9.9.9\\nProject: {}\\n'\n  exit 0\nfi\nexit 1\n",
            project.to_string_lossy()
        ),
    )
    .expect("write unusable Hermes executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hermes)
            .expect("stat fake Hermes executable")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hermes, perms).expect("chmod fake Hermes executable");
    }
    hermes
}

fn expected_empty_settings() -> HostSettings {
    HostSettings {
        enabled_backends: Vec::new(),
        default_backend: None,
        enable_mobile_connections: false,
        mobile_broker_url: None,
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
        complexity_tiers_enabled: false,
        backend_tier_configs: std::collections::HashMap::new(),
        background_agent_features: Default::default(),
        supervisor: Default::default(),
        code_intel: Default::default(),
        backend_config: std::collections::HashMap::new(),
        launch_profiles: Vec::new(),
    }
}

#[test]
fn missing_store_returns_empty_settings() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");

    let store = HostSettingsStore::load(path.clone()).expect("load missing settings store");

    assert_eq!(
        store.get().expect("read settings from missing store"),
        expected_empty_settings()
    );
    assert!(
        !path.exists(),
        "loading a missing settings store should not write a file"
    );
}

#[test]
fn persisted_empty_settings_are_valid() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": [],
    "default_backend": null
  }
}"#,
    )
    .expect("write empty settings store");

    let store = HostSettingsStore::load(path).expect("load empty settings store");

    assert_eq!(
        store.get().expect("read empty settings"),
        expected_empty_settings()
    );
}

#[test]
fn invalid_persisted_default_backend_is_rejected() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": ["claude"],
    "default_backend": "codex"
  }
}"#,
    )
    .expect("write invalid settings store");

    let err = HostSettingsStore::load(path).expect_err("invalid settings store should fail");

    assert!(
        err.contains("default_backend Some(Codex) must be present in enabled_backends"),
        "unexpected error: {err}"
    );
}

#[test]
fn persisted_backend_lists_are_canonicalized_but_not_defaulted() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": ["gemini", "claude", "kiro", "claude"],
    "default_backend": "claude"
  }
}"#,
    )
    .expect("write settings store");

    let store = HostSettingsStore::load(path).expect("load settings store");

    assert_eq!(
        store.get().expect("read canonicalized settings"),
        HostSettings {
            enabled_backends: vec![
                BackendKind::Kiro,
                BackendKind::Claude,
                BackendKind::Antigravity,
            ],
            default_backend: Some(BackendKind::Claude),
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            supervisor: Default::default(),
            code_intel: Default::default(),
            backend_config: std::collections::HashMap::new(),
            launch_profiles: Vec::new(),
        }
    );
}

#[test]
fn supervisor_settings_default_apply_and_validate() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    let store = HostSettingsStore::load(path).expect("load empty settings store");

    let defaults = store.get().expect("read empty settings").supervisor;
    assert!(!defaults.enabled, "supervisor must default off");
    assert!(
        !defaults.auto_compact_on_success,
        "auto-compact must default off"
    );
    assert_eq!(defaults.max_kicks_per_task, 3);
    assert_eq!(defaults.retry_attempts, 1);

    let settings = store
        .apply(HostSettingValue::SupervisorEnabled { enabled: true })
        .expect("enable supervisor");
    assert!(settings.supervisor.enabled);
    let settings = store
        .apply(HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: true })
        .expect("enable auto-compact");
    assert!(settings.supervisor.auto_compact_on_success);
    let settings = store
        .apply(HostSettingValue::SupervisorMaxKicksPerTask { count: 5 })
        .expect("set kick limit");
    assert_eq!(settings.supervisor.max_kicks_per_task, 5);
    let settings = store
        .apply(HostSettingValue::SupervisorRetryAttempts { count: 0 })
        .expect("retries can be disabled entirely");
    assert_eq!(settings.supervisor.retry_attempts, 0);
    assert_eq!(
        settings.supervisor.cost_tier,
        protocol::SupervisorCostTier::Low,
        "the verdict tier defaults to the cheap tier"
    );
    let settings = store
        .apply(HostSettingValue::SupervisorCostTier {
            tier: protocol::SupervisorCostTier::High,
        })
        .expect("set verdict tier");
    assert_eq!(
        settings.supervisor.cost_tier,
        protocol::SupervisorCostTier::High
    );

    store
        .apply(HostSettingValue::SupervisorMaxKicksPerTask { count: 0 })
        .expect_err("a zero kick limit must be rejected, not stored");

    let persisted = store.get().expect("re-read persisted settings").supervisor;
    assert!(persisted.enabled);
    assert!(persisted.auto_compact_on_success);
    assert_eq!(
        persisted.max_kicks_per_task, 5,
        "the rejected zero write must not clobber the stored kick limit"
    );
    assert_eq!(persisted.retry_attempts, 0);
    assert_eq!(
        persisted.cost_tier,
        protocol::SupervisorCostTier::High,
        "the verdict tier must survive the read-modify-write cycle"
    );
}

#[test]
fn code_intel_language_server_paths_default_set_and_clear() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    let store = HostSettingsStore::load(path).expect("load empty settings store");
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());
    let executable = HostExecutablePath("/opt/rust-analyzer/bin/rust-analyzer".to_owned());

    assert!(
        store
            .get()
            .expect("read empty settings")
            .code_intel
            .language_server_paths
            .is_empty(),
        "code-intel language server paths should default empty"
    );

    let settings = store
        .apply(HostSettingValue::CodeIntelLanguageServerPath {
            provider: provider.clone(),
            path: Some(executable.clone()),
        })
        .expect("set rust-analyzer path");
    assert_eq!(
        settings.code_intel.language_server_paths.get(&provider),
        Some(&executable)
    );
    assert_eq!(
        store
            .get()
            .expect("re-read set path")
            .code_intel
            .language_server_paths
            .get(&provider),
        Some(&executable)
    );

    let settings = store
        .apply(HostSettingValue::CodeIntelLanguageServerPath {
            provider: provider.clone(),
            path: None,
        })
        .expect("clear rust-analyzer path");
    assert!(
        settings.code_intel.language_server_paths.is_empty(),
        "clearing the path should remove the provider entry"
    );
    assert!(
        store
            .get()
            .expect("re-read cleared path")
            .code_intel
            .language_server_paths
            .is_empty(),
        "cleared path should persist"
    );
}

#[test]
fn backend_config_updates_merge_and_clear_explicitly_in_store() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    let store = HostSettingsStore::load(path).expect("load empty settings store");

    // No built-in backend publishes a typed deep-config schema anymore
    // (Hermes moved to backend-native settings), so storing values for one
    // is a visible refusal instead of a silent write.
    let mut values = BackendConfigValues::default();
    values.0.insert(
        "default_model".to_owned(),
        SessionSettingValue::String("anthropic/claude-sonnet-5".to_owned()),
    );
    let err = store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Hermes,
            values,
        })
        .expect_err("schema-less backend config writes must be refused");
    assert!(
        err.contains("does not support backend configuration"),
        "{err}"
    );

    // An explicit empty update still clears any stored entry and persists.
    let settings = store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Hermes,
            values: BackendConfigValues::default(),
        })
        .expect("empty update clears backend config");
    assert!(!settings.backend_config.contains_key(&BackendKind::Hermes));
}
#[tokio::test]
async fn backend_config_updates_are_refused_over_client_events() {
    let mut fixture = Fixture::new().await;

    // No built-in backend accepts typed deep-config writes anymore (Hermes
    // moved to backend-native settings), so a BackendConfig set must come
    // back as a typed CommandError instead of a settings update.
    let mut model = BackendConfigValues::default();
    model.0.insert(
        "default_model".to_owned(),
        SessionSettingValue::String("anthropic/claude-sonnet-5".to_owned()),
    );
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendConfig {
                backend: BackendKind::Hermes,
                values: model,
            },
        })
        .await
        .expect("send Hermes backend config set");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "timed out waiting for the refusal");
        let env = match tokio::time::timeout(deadline - now, fixture.client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before the refusal"),
            Ok(Err(err)) => panic!("next_event failed before the refusal: {err:?}"),
            Err(_) => panic!("timed out waiting for the refusal"),
        };
        match env.kind {
            FrameKind::CommandError => {
                let error: CommandErrorPayload = env
                    .parse_payload()
                    .expect("parse CommandError for Hermes backend config refusal");
                assert!(
                    error
                        .message
                        .contains("does not support backend configuration"),
                    "unexpected refusal message: {error:?}"
                );
                break;
            }
            FrameKind::HostSettings => {
                let settings: HostSettingsPayload = env
                    .parse_payload()
                    .expect("parse HostSettings while awaiting the refusal");
                assert!(
                    !settings
                        .settings
                        .backend_config
                        .contains_key(&BackendKind::Hermes),
                    "refused Hermes backend config must never reach host settings"
                );
            }
            _ => {}
        }
    }
}
#[test]
fn generated_alias_never_overrides_user_alias() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("sessions.json");
    let store = SessionStore::load(path).expect("load session store");
    let session = BackendSession {
        id: SessionId("session-1".to_string()),
        backend_kind: BackendKind::Claude,
        workspace_roots: vec!["/tmp/test".to_string()],
        title: Some("Chat".to_string()),
        token_count: None,
        created_at_ms: Some(1),
        updated_at_ms: Some(1),
        resumable: true,
    };
    store
        .upsert_backend_session(&session, None, None, None, None)
        .expect("upsert backend session");

    assert!(
        store
            .set_generated_alias_if_no_user_alias(&session.id, "Generated Name".to_string())
            .expect("set generated alias"),
        "generated alias should apply when no user alias exists"
    );
    assert_eq!(
        store.effective_name(&session.id).as_deref(),
        Some("Generated Name")
    );

    store
        .set_user_alias(&session.id, "Manual Name".to_string())
        .expect("set user alias");
    assert!(
        !store
            .set_generated_alias_if_no_user_alias(&session.id, "Later Generated".to_string())
            .expect("generated alias after manual rename"),
        "generated alias should be rejected once a user alias exists"
    );
    assert_eq!(
        store.effective_name(&session.id).as_deref(),
        Some("Manual Name")
    );
}

#[tokio::test]
async fn backend_setup_payload_uses_sign_in_command_and_versioned_tycode_probe() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    write_fake_tycode_binary(temp_home.path());
    let fake_hermes = write_fake_hermes_install(temp_home.path());
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes = EnvVarGuard::set(
        "HERMES_EXECUTABLE",
        fake_hermes.to_string_lossy().to_string(),
    );
    let _hermes_python = EnvVarGuard::set("HERMES_PYTHON", "".to_string());

    let mut fixture = Fixture::new_with_real_backend_probe_for_enabled_backends(Vec::new()).await;
    let payload = fixture.bootstrap.backend_setup.clone();
    expect_no_backend_setup_replay(&mut fixture.client).await;

    let tycode = payload
        .backends
        .iter()
        .find(|info| info.backend_kind == BackendKind::Tycode)
        .expect("Tycode backend setup entry");
    assert_eq!(tycode.status, BackendSetupStatus::Installed);
    assert_eq!(
        tycode.installed_version.as_deref(),
        Some("tycode-subprocess 0.10.0")
    );
    assert!(
        tycode.diagnostic.is_none(),
        "Tycode setup diagnostics should report install/setup issues only"
    );
    assert!(tycode.sign_in_command.is_none());

    let tycode_value = serde_json::to_value(tycode).expect("serialize Tycode BackendSetupInfo");
    assert!(
        tycode_value.get("follow_up_commands").is_none(),
        "BackendSetupInfo should no longer expose follow_up_commands"
    );

    let install = tycode
        .install_command
        .as_ref()
        .expect("Tycode install command should exist");
    assert!(install.command.contains("uname -s"));
    assert!(install.command.contains("uname -m"));
    assert!(install.command.contains("curl -fL"));
    assert!(install.command.contains("tar -xJf"));
    assert!(
        install
            .command
            .contains("INSTALL_ROOT=\"${HOME_DIR}/.tyde/tycode\"")
    );
    assert!(install.command.contains("EXPECTED_SHA256="));
    assert!(install.command.contains("tycode-subprocess.tmp.$$"));
    assert!(
        install
            .command
            .contains("mv -f \"$STAGED_BINARY\" \"$FINAL_BINARY\"")
    );

    let claude = payload
        .backends
        .iter()
        .find(|info| info.backend_kind == BackendKind::Claude)
        .expect("Claude backend setup entry");
    assert!(
        claude.sign_in_command.is_some(),
        "Installed CLI affordance should be exposed as sign_in_command"
    );
    let claude_value = serde_json::to_value(claude).expect("serialize Claude BackendSetupInfo");
    assert!(
        claude_value.get("follow_up_commands").is_none(),
        "BackendSetupInfo should not serialize follow_up_commands"
    );

    let hermes = payload
        .backends
        .iter()
        .find(|info| info.backend_kind == BackendKind::Hermes)
        .expect("Hermes backend setup entry");
    assert_eq!(hermes.status, BackendSetupStatus::Installed);
    assert_eq!(
        hermes.installed_version.as_deref(),
        Some("Hermes Agent v9.9.9")
    );
    assert!(
        hermes.diagnostic.is_none(),
        "installed fake Hermes should not report diagnostics"
    );
    let hermes_sign_in = hermes
        .sign_in_command
        .as_ref()
        .expect("Hermes sign-in should use resolved executable");
    let expected_hermes_setup = format!("{} setup", fake_hermes.to_string_lossy());
    assert_eq!(
        hermes_sign_in.display_command.as_deref(),
        Some(expected_hermes_setup.as_str())
    );
    assert!(
        hermes_sign_in
            .command
            .contains(&fake_hermes.to_string_lossy().to_string()),
        "Hermes sign-in command should include resolved executable: {}",
        hermes_sign_in.command
    );
}

#[tokio::test]
async fn backend_setup_payload_reports_found_unusable_hermes_cli() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake_hermes = write_unusable_hermes_cli(temp_home.path());
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes = EnvVarGuard::set(
        "HERMES_EXECUTABLE",
        fake_hermes.to_string_lossy().to_string(),
    );
    let _hermes_python = EnvVarGuard::set("HERMES_PYTHON", "".to_string());

    let mut fixture = Fixture::new_with_real_backend_probe_for_enabled_backends(Vec::new()).await;
    let payload = fixture.bootstrap.backend_setup.clone();
    expect_no_backend_setup_replay(&mut fixture.client).await;

    let hermes = payload
        .backends
        .iter()
        .find(|info| info.backend_kind == BackendKind::Hermes)
        .expect("Hermes backend setup entry");
    assert_eq!(hermes.status, BackendSetupStatus::Unavailable);
    assert_eq!(hermes.installed_version, None);
    assert!(hermes.sign_in_command.is_none());
    let diagnostic = hermes.diagnostic.as_ref().expect("Hermes diagnostic");
    assert_eq!(
        diagnostic.code,
        BackendSetupDiagnosticCode::MissingGatewayPython
    );
    assert!(
        diagnostic.message.contains("Hermes Agent v9.9.9")
            && diagnostic
                .message
                .contains(&fake_hermes.to_string_lossy().to_string()),
        "diagnostic should name the found CLI and version: {}",
        diagnostic.message
    );
    assert!(
        !diagnostic.message.contains("so `hermes` is on PATH")
            && !diagnostic.message.contains("set HERMES_EXECUTABLE"),
        "found-unusable diagnostic should not recommend PATH/HERMES_EXECUTABLE remedies: {}",
        diagnostic.message
    );
    assert!(
        diagnostic.message.contains("Re-run the Hermes installer")
            && diagnostic.message.contains("HERMES_PYTHON"),
        "found-unusable diagnostic should include an actionable gateway-Python remedy: {}",
        diagnostic.message
    );
}

#[tokio::test]
async fn backend_config_snapshots_expose_tycode_grouped_native_settings() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    assert!(fake.binary.is_file(), "fake Tycode binary should exist");
    let source_bytes = write_shared_tycode_settings(
        temp_home.path(),
        r#"active_provider = "native-bedrock"
default_agent = "builder"
model_quality = "high"
reasoning_effort = "Max"
autonomy_level = "fully_autonomous"
review_level = "Task"
spawn_context_mode = "Fresh"

[providers.native-bedrock]
type = "bedrock"
profile = "integration-profile"
region = "eu-west-1"
mantle_region = "us-east-1"

[providers.openrouter-empty]
type = "openrouter"
api_key = ""

[providers.native-mock]
type = "mock"

[modules.execution]
enabled = true

[unsupported_voice_provider]
api_key = "shared-only-secret"
"#,
    );
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let setup = fixture
        .bootstrap
        .backend_setup
        .backends
        .iter()
        .find(|backend| backend.backend_kind == BackendKind::Tycode)
        .expect("Tycode setup status for grouped native settings");
    assert_eq!(
        setup.status,
        BackendSetupStatus::Installed,
        "fake Tycode must pass the installed-artifact version probe: {:?}",
        setup.diagnostic
    );
    assert_eq!(
        setup.installed_version.as_deref(),
        Some("tycode-subprocess 0.10.0")
    );
    let payload =
        expect_backend_config_snapshots(&mut fixture.client, "Tycode grouped native settings")
            .await;

    assert!(
        payload
            .snapshots
            .iter()
            .all(|snapshot| snapshot.backend_kind != BackendKind::Tycode),
        "Tycode should no longer expose the legacy hardcoded backend-config subset"
    );
    let tycode = tycode_native_snapshot(&payload);
    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Ready);
    assert!(tycode.message.is_none());
    let settings = tycode.settings.as_ref().expect("current Tycode settings");
    assert_eq!(settings["profile"], "default");
    assert_eq!(settings["active_provider"], "native-bedrock");
    assert_eq!(settings["default_agent"], "builder");
    assert_eq!(settings["providers"]["native-bedrock"]["type"], "bedrock");
    assert_eq!(
        settings["providers"]["native-bedrock"]["profile"],
        "integration-profile"
    );
    assert_eq!(
        settings["providers"]["native-bedrock"]["region"],
        "eu-west-1"
    );
    assert_eq!(
        settings["providers"]["native-bedrock"]["mantle_region"],
        "us-east-1"
    );
    assert_eq!(
        settings["providers"]["openrouter-empty"]["type"],
        "openrouter"
    );
    assert_eq!(settings["providers"]["openrouter-empty"]["api_key"], "");
    assert_eq!(settings["providers"]["native-mock"]["behavior"], "success");
    assert_eq!(settings["model_quality"], "high");
    assert_eq!(settings["modules"]["execution"]["enabled"], true);
    assert!(
        tycode.groups.iter().any(|group| {
            group.kind == BackendNativeSettingsGroupKind::Core && group.settings_path.is_empty()
        }),
        "Tycode native settings should expose a top-level core group: {:?}",
        tycode.groups
    );
    assert!(
        tycode.groups.iter().any(|group| {
            group.kind == BackendNativeSettingsGroupKind::Module
                && group.settings_path == vec!["modules".to_string(), "execution".to_string()]
        }),
        "Tycode native settings should expose a nested module group: {:?}",
        tycode.groups
    );
    let (managed_path, _, source, notice_pending) = managed_projection_fields(tycode);
    assert_eq!(source, TycodeProjectionSource::SharedSettings);
    assert!(notice_pending);
    assert_eq!(
        managed_path.0,
        temp_home
            .path()
            .join(".tycode/tyde-settings.toml")
            .to_string_lossy()
    );
    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("re-read shared Tycode settings"),
        source_bytes,
        "automatic managed projection creation must leave the shared source byte-identical"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(temp_home.path().join(".tycode"))
                .expect("stat Tycode settings directory")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        for path in [
            temp_home.path().join(".tycode/tyde-settings.toml"),
            temp_home
                .path()
                .join(".tycode/tyde-settings.provenance.json"),
        ] {
            assert_eq!(
                std::fs::metadata(&path)
                    .unwrap_or_else(|err| panic!("stat private projection file {path:?}: {err}"))
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    let response = serde_json::to_string(&payload).expect("serialize typed settings response");
    assert!(
        !response.contains("shared-only-secret"),
        "unsupported source-only secrets must not enter typed responses"
    );
    let initial_spawns = fake
        .events()
        .into_iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(
        initial_spawns.len(),
        6,
        "non-default import must initialize and verify defaults, probe/normalize/verify the source copy, then probe the published path"
    );
    assert_eq!(
        initial_spawns[0]["settings_path"],
        initial_spawns[1]["settings_path"]
    );
    assert_ne!(initial_spawns[0]["pid"], initial_spawns[1]["pid"]);
    assert_eq!(initial_spawns[0]["settings_existed_before"], false);
    assert_eq!(initial_spawns[1]["settings_existed_before"], true);
    assert_eq!(
        initial_spawns[2]["settings_path"],
        initial_spawns[3]["settings_path"]
    );
    assert_eq!(
        initial_spawns[3]["settings_path"],
        initial_spawns[4]["settings_path"]
    );
    assert_ne!(initial_spawns[2]["pid"], initial_spawns[3]["pid"]);
    assert_ne!(initial_spawns[3]["pid"], initial_spawns[4]["pid"]);
    assert_eq!(initial_spawns[5]["settings_path"], managed_path.0.as_str());
    let shared_path = temp_home
        .path()
        .join(".tycode/settings.toml")
        .to_string_lossy()
        .into_owned();
    for spawn in initial_spawns {
        assert_ne!(spawn["settings_path"].as_str(), Some(shared_path.as_str()));
        let argv = spawn["argv"].as_array().expect("initial fake Tycode argv");
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.as_str() == Some("--settings-path"))
                .count(),
            1
        );
    }
    let persistent_normalization = fake
        .events()
        .into_iter()
        .find(|event| {
            event["type"] == "command" && event["command"]["SaveSettings"]["persist"] == true
        })
        .expect("non-default source normalization SaveSettings command");
    let normalized = &persistent_normalization["command"]["SaveSettings"]["settings"];
    assert_eq!(normalized["default_agent"], "builder");
    assert_eq!(normalized["providers"]["native-bedrock"]["type"], "bedrock");
    assert_eq!(
        normalized["providers"]["native-bedrock"]["mantle_region"],
        "us-east-1"
    );
    assert_eq!(normalized["providers"]["openrouter-empty"]["api_key"], "");
    let managed_toml = std::fs::read_to_string(temp_home.path().join(".tycode/tyde-settings.toml"))
        .expect("read persisted normalized v0.10 settings");
    assert!(managed_toml.contains("default_agent = \"builder\""));
    assert!(managed_toml.contains("type = \"bedrock\""));
    assert!(managed_toml.contains("mantle_region = \"us-east-1\""));
    assert!(managed_toml.contains("type = \"openrouter\""));
}

#[cfg(unix)]
#[tokio::test]
async fn tycode_owned_conventional_directory_is_accepted_but_writable_mode_is_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let _env_guard = env_lock().lock().await;

    let conventional_home = tempfile::tempdir().expect("create conventional-mode HOME");
    let conventional_fake = write_fake_tycode_binary(conventional_home.path());
    let conventional_source = write_shared_tycode_settings(
        conventional_home.path(),
        r#"active_provider = "native-provider"

[providers.native-provider]
type = "mock"
"#,
    );
    assert_eq!(
        std::fs::metadata(conventional_home.path().join(".tycode"))
            .expect("stat conventional Tycode directory before probe")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    {
        let _home = EnvVarGuard::set(
            "HOME",
            conventional_home.path().to_string_lossy().to_string(),
        );
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let payload = expect_backend_config_snapshots(
            &mut fixture.client,
            "owned conventional 0755 Tycode directory",
        )
        .await;
        assert_eq!(
            tycode_native_snapshot(&payload).status,
            BackendConfigSnapshotStatus::Ready
        );
    }
    assert_eq!(
        std::fs::read(conventional_home.path().join(".tycode/settings.toml"))
            .expect("read conventional-mode shared settings after projection"),
        conventional_source
    );
    assert_eq!(
        std::fs::metadata(conventional_home.path().join(".tycode"))
            .expect("stat conventional Tycode directory after probe")
            .permissions()
            .mode()
            & 0o777,
        0o755,
        "Tyde must accept, not silently chmod, an owned conventional directory"
    );
    assert!(
        conventional_fake
            .events()
            .iter()
            .any(|event| event["type"] == "spawn"),
        "accepted conventional directory should reach the pinned actor"
    );

    let writable_home = tempfile::tempdir().expect("create writable-mode HOME");
    let writable_fake = write_fake_tycode_binary(writable_home.path());
    let writable_source =
        write_shared_tycode_settings(writable_home.path(), "# unsafe directory sentinel\n");
    std::fs::set_permissions(
        writable_home.path().join(".tycode"),
        std::fs::Permissions::from_mode(0o775),
    )
    .expect("make Tycode directory group-writable");
    {
        let _home = EnvVarGuard::set("HOME", writable_home.path().to_string_lossy().to_string());
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let payload = expect_backend_config_snapshots(
            &mut fixture.client,
            "group-writable Tycode directory rejection",
        )
        .await;
        let tycode = tycode_native_snapshot(&payload);
        assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
        assert!(tycode.settings.is_none());
        assert!(tycode.groups.is_empty());
        assert!(
            tycode
                .message
                .as_deref()
                .is_some_and(|message| message.contains("group- or world-writable")),
            "unsafe-mode diagnostic should identify the rejected condition: {:?}",
            tycode.message
        );
    }
    assert_eq!(
        std::fs::read(writable_home.path().join(".tycode/settings.toml"))
            .expect("read rejected shared settings"),
        writable_source
    );
    assert_eq!(
        std::fs::metadata(writable_home.path().join(".tycode"))
            .expect("stat rejected Tycode directory")
            .permissions()
            .mode()
            & 0o777,
        0o775
    );
    for name in [
        "tyde-settings.toml",
        "tyde-settings.provenance.json",
        "tyde-settings.transaction.json",
        "tyde-settings.recovery.json",
    ] {
        assert!(
            !writable_home.path().join(".tycode").join(name).exists(),
            "unsafe directory must not publish managed artifact {name}"
        );
    }
    assert!(
        writable_fake
            .events()
            .iter()
            .all(|event| event["type"] != "spawn"),
        "unsafe directory must fail before actor startup"
    );
}

#[tokio::test]
async fn backend_config_snapshots_report_tycode_native_settings_probe_failure() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    fake.set_behavior(serde_json::json!({ "exit_before_schema": true }));
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let payload = expect_backend_config_snapshots(
        &mut fixture.client,
        "Tycode native settings probe failure",
    )
    .await;
    let tycode = payload
        .native_settings
        .iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Tycode)
        .expect("Tycode native settings failure snapshot");

    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
    let message = tycode
        .message
        .as_deref()
        .expect("Tycode probe failure message");
    assert_eq!(
        message,
        "Cannot start Tycode native settings probe without a verified managed settings projection: \
         Tycode process exited during managed projection verification: verifying SettingsSchema in a fresh process",
        "Tycode projection failure should identify the exact failed phase"
    );
    assert!(tycode.settings.is_none());
    assert!(tycode.groups.is_empty());
    assert!(
        !temp_home.path().join(".tycode/tyde-settings.toml").exists(),
        "failed normalization must not publish a managed projection"
    );
    for path in [
        temp_home
            .path()
            .join(".tycode/tyde-settings.provenance.json"),
        temp_home
            .path()
            .join(".tycode/tyde-settings.transaction.json"),
    ] {
        assert!(
            !path.exists(),
            "failed creation must not partially publish {path:?}"
        );
    }
    let partials = std::fs::read_dir(temp_home.path().join(".tycode"))
        .expect("inspect failed projection directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tyde-settings.") && name.ends_with(".txn"))
        .collect::<Vec<_>>();
    assert!(
        partials.is_empty(),
        "partial transaction artifacts: {partials:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tycode_requires_installed_artifact_and_successful_exact_version_exit() {
    let _env_guard = env_lock().lock().await;

    {
        let home = tempfile::tempdir().expect("create PATH-imposter HOME");
        let fake_bin = home.path().join("fake-bin");
        std::fs::create_dir_all(&fake_bin).expect("create PATH-imposter directory");
        let imposter = fake_bin.join("tycode-subprocess");
        let imposter_log = home.path().join("path-imposter-invoked");
        std::fs::write(
            &imposter,
            format!(
                "#!/bin/sh\necho invoked > '{}'\necho 'tycode-subprocess 0.10.0'\n",
                imposter_log.display()
            ),
        )
        .expect("write PATH Tycode imposter");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&imposter, std::fs::Permissions::from_mode(0o755))
                .expect("chmod PATH Tycode imposter");
        }
        let path = format!(
            "{}:{}",
            fake_bin.to_string_lossy(),
            std::env::var("PATH").expect("test PATH")
        );
        let _home = EnvVarGuard::set("HOME", home.path().to_string_lossy().to_string());
        let _path = EnvVarGuard::set("PATH", path);
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let setup = fixture
            .bootstrap
            .backend_setup
            .backends
            .iter()
            .find(|backend| backend.backend_kind == BackendKind::Tycode)
            .expect("Tycode setup status without installed artifact");
        assert_eq!(setup.status, BackendSetupStatus::NotInstalled);
        let snapshots = expect_backend_config_snapshots(
            &mut fixture.client,
            "PATH-only Tycode native settings refusal",
        )
        .await;
        let tycode = tycode_native_snapshot(&snapshots);
        assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
        assert_eq!(
            tycode.message.as_deref(),
            Some(
                "Cannot start Tycode native settings probe without a verified managed settings projection: Cannot start Tycode managed projection verification: tycode-subprocess not found"
            )
        );
        assert!(
            !imposter_log.exists(),
            "runtime must not execute a PATH-only Tycode imposter"
        );
    }

    {
        let home = tempfile::tempdir().expect("create nonzero-version HOME");
        let fake = write_fake_tycode_binary(home.path());
        fake.set_behavior(serde_json::json!({ "version_exit_code": 9 }));
        let _home = EnvVarGuard::set("HOME", home.path().to_string_lossy().to_string());
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let setup = fixture
            .bootstrap
            .backend_setup
            .backends
            .iter()
            .find(|backend| backend.backend_kind == BackendKind::Tycode)
            .expect("Tycode setup status for nonzero version exit");
        assert_eq!(setup.status, BackendSetupStatus::Unavailable);
        let setup_message = setup
            .diagnostic
            .as_ref()
            .expect("nonzero version diagnostic")
            .message
            .as_str();
        assert!(
            setup_message.contains("exited unsuccessfully") && setup_message.contains('9'),
            "{setup_message}"
        );
        let snapshots = expect_backend_config_snapshots(
            &mut fixture.client,
            "nonzero exact-version Tycode refusal",
        )
        .await;
        let tycode = tycode_native_snapshot(&snapshots);
        assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
        let message = tycode.message.as_deref().expect("nonzero version failure");
        assert!(
            message.contains("exited unsuccessfully") && message.contains('9'),
            "{message}"
        );
        let events = fake.events();
        assert!(events.iter().any(|event| event["type"] == "version"));
        assert!(
            events.iter().all(|event| event["type"] != "spawn"),
            "failed version identity must stop before any actor process: {events:#?}"
        );
    }

    {
        let home = tempfile::tempdir().expect("create wrong-version HOME");
        let fake = write_fake_tycode_binary(home.path());
        fake.set_behavior(serde_json::json!({
            "version_output": "tycode-subprocess 0.9.9"
        }));
        let _home = EnvVarGuard::set("HOME", home.path().to_string_lossy().to_string());
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let setup = fixture
            .bootstrap
            .backend_setup
            .backends
            .iter()
            .find(|backend| backend.backend_kind == BackendKind::Tycode)
            .expect("Tycode setup status for wrong version");
        assert_eq!(setup.status, BackendSetupStatus::Unavailable);
        let snapshots = expect_backend_config_snapshots(
            &mut fixture.client,
            "wrong installed Tycode version refusal",
        )
        .await;
        let tycode = tycode_native_snapshot(&snapshots);
        assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
        let message = tycode.message.as_deref().expect("wrong-version failure");
        assert!(
            message.contains("0.9.9") && message.contains("0.10.0"),
            "{message}"
        );
        let events = fake.events();
        assert!(events.iter().any(|event| event["type"] == "version"));
        assert!(events.iter().all(|event| event["type"] != "spawn"));
    }
}

#[tokio::test]
async fn set_setting_backend_native_settings_persists_and_refreshes_tycode_snapshot() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    let source_bytes = write_shared_tycode_settings(
        temp_home.path(),
        r#"active_provider = "native-provider"
model_quality = "high"

[providers.native-provider]
type = "mock"

[unmodellable]
secret = "shared-save-secret"
"#,
    );
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let initial =
        expect_backend_config_snapshots(&mut fixture.client, "initial Tycode native settings")
            .await;
    let events_before_save = fake.events().len();
    let initial_snapshot = tycode_native_snapshot(&initial);
    assert_eq!(
        initial_snapshot.status,
        BackendConfigSnapshotStatus::Ready,
        "the typed v0.10 native settings contract must be ready before saving: {:?}",
        initial_snapshot.message
    );
    let mut updated_settings = initial_snapshot
        .settings
        .clone()
        .expect("initial current Tycode settings");
    updated_settings
        .as_object_mut()
        .expect("Tycode settings object")
        .insert("model_quality".to_string(), serde_json::json!("low"));

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: updated_settings.clone(),
            },
        })
        .await
        .expect("save Tycode native settings");
    let refreshed =
        expect_backend_config_snapshots(&mut fixture.client, "refreshed Tycode native settings")
            .await;
    let tycode = refreshed
        .native_settings
        .iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Tycode)
        .expect("refreshed Tycode native settings snapshot");

    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Ready);
    let refreshed_settings = tycode
        .settings
        .as_ref()
        .expect("refreshed current settings");
    assert_eq!(refreshed_settings["model_quality"], "low");
    assert_eq!(refreshed_settings["profile"], "default");
    assert_eq!(
        refreshed_settings["providers"], updated_settings["providers"],
        "save and refresh must preserve unrelated provider settings"
    );
    assert_eq!(
        refreshed_settings["modules"], updated_settings["modules"],
        "save and refresh must preserve unrelated module settings"
    );
    assert!(tycode.message.is_none());
    assert!(
        tycode
            .groups
            .iter()
            .any(|group| group.kind == BackendNativeSettingsGroupKind::Core)
            && tycode
                .groups
                .iter()
                .any(|group| group.kind == BackendNativeSettingsGroupKind::Module),
        "refreshed Tycode snapshot should retain grouped schemas: {:?}",
        tycode.groups
    );
    let save_events = fake.events();
    let spawns = save_events[events_before_save..]
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(
        spawns.len(),
        3,
        "save must use a save process, a fresh verifier, and an authoritative managed-path probe"
    );
    assert_eq!(spawns[0]["settings_path"], spawns[1]["settings_path"]);
    assert_ne!(
        spawns[0]["pid"], spawns[1]["pid"],
        "post-save verification must run in a fresh process"
    );
    let managed = temp_home.path().join(".tycode/tyde-settings.toml");
    assert_ne!(
        spawns[0]["settings_path"],
        managed.to_string_lossy().as_ref()
    );
    assert_eq!(
        spawns[2]["settings_path"],
        managed.to_string_lossy().as_ref()
    );
    for spawn in spawns {
        let argv = spawn["argv"].as_array().expect("fake Tycode argv");
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.as_str() == Some("--settings-path"))
                .count(),
            1,
            "every actor-bearing process must receive exactly one settings path: {argv:?}"
        );
    }

    let events_before_second_save = fake.events().len();
    let mut second_update = refreshed_settings.clone();
    second_update
        .as_object_mut()
        .expect("refreshed Tycode settings object")
        .insert("model_quality".to_string(), serde_json::json!("high"));
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: second_update.clone(),
            },
        })
        .await
        .expect("persist a second Tycode native settings update through client");
    let second_refresh = expect_backend_config_snapshots(
        &mut fixture.client,
        "native snapshot after second Tycode native settings persist",
    )
    .await;
    let second_snapshot = tycode_native_snapshot(&second_refresh);
    assert_eq!(second_snapshot.status, BackendConfigSnapshotStatus::Ready);
    let second_settings = second_snapshot
        .settings
        .as_ref()
        .expect("second refreshed current settings");
    assert_eq!(second_settings["model_quality"], "high");
    assert_eq!(second_settings["providers"], second_update["providers"]);
    assert_eq!(second_settings["modules"], second_update["modules"]);
    let second_events = fake.events();
    let second_spawns = second_events[events_before_second_save..]
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(
        second_spawns.len(),
        3,
        "a subsequent native save must still use a save process, fresh verifier, and authoritative managed-path probe"
    );
    assert_eq!(
        second_spawns[0]["settings_path"],
        second_spawns[1]["settings_path"]
    );
    assert_ne!(second_spawns[0]["pid"], second_spawns[1]["pid"]);
    assert_ne!(
        second_spawns[0]["settings_path"],
        managed.to_string_lossy().as_ref()
    );
    assert_eq!(
        second_spawns[2]["settings_path"],
        managed.to_string_lossy().as_ref()
    );
    for spawn in second_spawns {
        let argv = spawn["argv"]
            .as_array()
            .expect("second-save fake Tycode argv");
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.as_str() == Some("--settings-path"))
                .count(),
            1
        );
    }
    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("re-read shared settings after repeated native saves"),
        source_bytes,
        "repeated native saves must leave the shared source byte-identical"
    );
}

#[tokio::test]
async fn tycode_pre_session_advisory_is_ready_but_post_command_error_is_unavailable() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    fake.set_behavior(serde_json::json!({ "pre_session_advisory": true }));
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let ready = expect_backend_config_snapshots(
        &mut fixture.client,
        "Tycode Ready snapshot after pre-session advisory",
    )
    .await;
    let tycode = tycode_native_snapshot(&ready);
    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Ready);
    assert!(tycode.settings.is_some());
    assert!(!tycode.groups.is_empty());
    assert!(tycode.advisories.iter().any(|advisory| matches!(
        advisory,
        BackendNativeSettingsAdvisory::NoProviderConfigured { message }
            if message.contains("No AI provider is configured")
    )));
    let (_, projection_id, _, _) = managed_projection_fields(tycode);
    let projection_id = projection_id.clone();

    fake.set_behavior(serde_json::json!({
        "pre_session_advisory": true,
        "post_command_error": true
    }));
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::AcknowledgeTycodeProjectionNotice {
                backend: BackendKind::Tycode,
                projection_id,
            },
        })
        .await
        .expect("acknowledge projection notice before failing refresh");
    let unavailable = expect_backend_config_snapshots(
        &mut fixture.client,
        "Tycode Unavailable snapshot after post-command error",
    )
    .await;
    let tycode = tycode_native_snapshot(&unavailable);
    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
    let message = tycode.message.as_deref().expect("typed failure message");
    assert!(
        message.contains("waiting for SettingsSchema")
            && message.contains("schema command failed after SessionStarted")
            && message.contains("earlier advisory"),
        "post-command errors must remain fatal with phase and advisory context: {message}"
    );
}

#[tokio::test]
async fn tycode_defaults_are_ready_without_creating_shared_settings() {
    let _env_guard = env_lock().lock().await;

    let defaults_home = tempfile::tempdir().expect("create defaults HOME");
    let fake = write_fake_tycode_binary(defaults_home.path());
    let _home = EnvVarGuard::set("HOME", defaults_home.path().to_string_lossy().to_string());
    let _hermes = EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut defaults_fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let defaults =
        expect_backend_config_snapshots(&mut defaults_fixture.client, "Tycode defaults projection")
            .await;
    let tycode = tycode_native_snapshot(&defaults);
    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Ready);
    let default_settings = tycode
        .settings
        .clone()
        .expect("Tycode nonexistent-path default settings");
    let (_, _, source, _) = managed_projection_fields(tycode);
    assert_eq!(source, TycodeProjectionSource::Defaults);
    assert!(tycode.advisories.iter().any(|advisory| matches!(
        advisory,
        BackendNativeSettingsAdvisory::NoProviderConfigured { .. }
    )));
    assert!(
        !defaults_home.path().join(".tycode/settings.toml").exists(),
        "defaults projection must not create or use the shared settings page"
    );
    let spawns = fake
        .events()
        .into_iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(spawns.len(), 3);
    assert_eq!(spawns[0]["settings_existed_before"], false);
    assert_eq!(spawns[1]["settings_existed_before"], true);
    assert_eq!(spawns[0]["settings_path"], spawns[1]["settings_path"]);
    assert_ne!(spawns[0]["pid"], spawns[1]["pid"]);
    assert_eq!(
        spawns[2]["settings_path"],
        defaults_home
            .path()
            .join(".tycode/tyde-settings.toml")
            .to_string_lossy()
            .as_ref()
    );
    assert!(fake.events().iter().all(|event| {
        event["type"] != "command" || event["command"].get("SaveSettings").is_none()
    }));
    let managed = std::fs::read_to_string(defaults_home.path().join(".tycode/tyde-settings.toml"))
        .expect("read Tycode-created default TOML");
    assert!(managed.contains("default_agent = \"tycode\""));
    assert!(managed.contains("review_level = \"None\""));
    assert!(managed.contains("orchestration_mode = \"auto\""));
    assert!(managed.contains("spawn_context_mode = \"Fork\""));
    assert!(!managed.contains("model_quality"));
    assert!(!managed.contains("reasoning_effort"));
    assert!(!managed.trim_start().starts_with('{'));
    defaults_fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: default_settings,
            },
        })
        .await
        .expect("send explicit persistent default save");
    let refusal = expect_command_error(
        &mut defaults_fixture.client,
        "v0.10 persistent semantic-default refusal",
    )
    .await;
    assert_eq!(refusal.code, CommandErrorCode::Internal);
    assert!(
        refusal
            .message
            .contains("Refusing to persist empty settings")
    );
    assert_eq!(
        std::fs::read_to_string(defaults_home.path().join(".tycode/tyde-settings.toml"))
            .expect("read managed default after refused save"),
        managed
    );
    assert!(!defaults_home.path().join(".tycode/settings.toml").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(defaults_home.path().join(".tycode"))
                .expect("stat automatically created Tycode directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn tycode_semantic_default_toml_sources_use_verified_nonexistent_path_artifact() {
    let _env_guard = env_lock().lock().await;
    let cases = [
        ("empty", ""),
        (
            "comments",
            "# Tycode CLI settings are intentionally empty\n",
        ),
        ("blank-default-agent", "default_agent = \"   \"\n"),
        (
            "explicit-defaults",
            r#"default_agent = "tycode"
autonomy_level = "fully_autonomous"
review_level = "None"
max_review_rounds = 3
fanout_concurrency = 4
orchestration_mode = "auto"
orchestration_progress_messages = true
swarm_models = []
spawn_context_mode = "Fork"
disable_custom_steering = false
communication_tone = "concise_and_logical"
disable_streaming = false

[providers]

[agent_models]

[mcp_servers]

[voice]

[voice.tts_providers]

[voice.stt_providers]

[skills]
enabled = true
disabled_skills = []
additional_dirs = []
enable_claude_code_compat = true

[modules]
"#,
        ),
        (
            "pruned-to-default",
            r#"[providers.legacy]
type = "unsupported-v09-provider"
api_key = "never-return-this-secret"

[unmodelled]
value = "discarded only from Tyde's projection"
"#,
        ),
    ];

    for (label, source) in cases {
        let home = tempfile::tempdir().expect("create semantic-default HOME");
        let fake = write_fake_tycode_binary(home.path());
        let source_bytes = write_shared_tycode_settings(home.path(), source);
        {
            let _home = EnvVarGuard::set("HOME", home.path().to_string_lossy().to_string());
            let _hermes =
                EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
            let mut fixture = Fixture::new_with_real_backend_probe_for_enabled_backends(vec![
                BackendKind::Tycode,
            ])
            .await;
            let payload = expect_backend_config_snapshots(
                &mut fixture.client,
                &format!("{label} semantic-default projection"),
            )
            .await;
            let tycode = tycode_native_snapshot(&payload);
            assert_eq!(tycode.status, BackendConfigSnapshotStatus::Ready, "{label}");
            let (_, _, source, _) = managed_projection_fields(tycode);
            assert_eq!(source, TycodeProjectionSource::SharedSettings, "{label}");
            assert!(tycode.advisories.iter().any(|advisory| matches!(
                advisory,
                BackendNativeSettingsAdvisory::NoProviderConfigured { .. }
            )));
            let serialized =
                serde_json::to_string(&payload).expect("serialize semantic-default typed response");
            assert!(!serialized.contains("never-return-this-secret"));
        }

        assert_eq!(
            std::fs::read(home.path().join(".tycode/settings.toml"))
                .expect("re-read semantic-default shared TOML"),
            source_bytes,
            "{label} source bytes"
        );
        let events = fake.events();
        let spawns = events
            .iter()
            .filter(|event| event["type"] == "spawn")
            .collect::<Vec<_>>();
        assert_eq!(spawns.len(), 4, "{label} spawns: {spawns:#?}");
        assert_eq!(spawns[0]["settings_existed_before"], false, "{label}");
        assert_eq!(spawns[1]["settings_existed_before"], true, "{label}");
        assert_eq!(spawns[0]["settings_path"], spawns[1]["settings_path"]);
        assert_ne!(spawns[0]["pid"], spawns[1]["pid"]);
        assert_ne!(spawns[2]["settings_path"], spawns[0]["settings_path"]);
        assert_eq!(
            spawns[3]["settings_path"],
            home.path()
                .join(".tycode/tyde-settings.toml")
                .to_string_lossy()
                .as_ref()
        );
        assert!(
            events.iter().all(|event| {
                event["type"] != "command" || event["command"].get("SaveSettings").is_none()
            }),
            "{label} must not use persistent SaveSettings: {events:#?}"
        );
    }
}

#[tokio::test]
async fn tycode_voice_provider_pruning_and_recognized_fields_match_v010() {
    let _env_guard = env_lock().lock().await;

    let unsupported_home = tempfile::tempdir().expect("create unsupported-voice HOME");
    let unsupported_fake = write_fake_tycode_binary(unsupported_home.path());
    let unsupported_source = write_shared_tycode_settings(
        unsupported_home.path(),
        r#"[voice.tts_providers.legacy-tts]
type = "unsupported-tts"
api_key = "unsupported-tts-secret"

[voice.stt_providers.legacy-stt]
type = "unsupported-stt"
api_key = "unsupported-stt-secret"
"#,
    );
    {
        let _home = EnvVarGuard::set(
            "HOME",
            unsupported_home.path().to_string_lossy().to_string(),
        );
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let payload = expect_backend_config_snapshots(
            &mut fixture.client,
            "unsupported-only voice settings semantic default",
        )
        .await;
        let snapshot = tycode_native_snapshot(&payload);
        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        let settings = snapshot
            .settings
            .as_ref()
            .expect("settings after unsupported voice pruning");
        assert_eq!(settings["voice"]["tts_providers"], serde_json::json!({}));
        assert_eq!(settings["voice"]["stt_providers"], serde_json::json!({}));
        assert!(snapshot.advisories.iter().any(|advisory| matches!(
            advisory,
            BackendNativeSettingsAdvisory::NoProviderConfigured { .. }
        )));
        let serialized =
            serde_json::to_string(&payload).expect("serialize unsupported-only voice response");
        assert!(!serialized.contains("unsupported-tts-secret"));
        assert!(!serialized.contains("unsupported-stt-secret"));
    }
    assert_eq!(
        std::fs::read(unsupported_home.path().join(".tycode/settings.toml"))
            .expect("read unsupported-only voice source"),
        unsupported_source
    );
    let unsupported_events = unsupported_fake.events();
    let unsupported_log =
        serde_json::to_string(&unsupported_events).expect("serialize unsupported voice fake log");
    assert!(!unsupported_log.contains("unsupported-tts-secret"));
    assert!(!unsupported_log.contains("unsupported-stt-secret"));
    let unsupported_spawns = unsupported_events
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(unsupported_spawns.len(), 4, "{unsupported_spawns:#?}");
    assert!(unsupported_events.iter().all(|event| {
        event["type"] != "command" || event["command"].get("SaveSettings").is_none()
    }));
    let unsupported_managed =
        std::fs::read_to_string(unsupported_home.path().join(".tycode/tyde-settings.toml"))
            .expect("read default projection for unsupported-only voice source");
    assert!(!unsupported_managed.contains("legacy-tts"));
    assert!(!unsupported_managed.contains("legacy-stt"));
    assert!(!unsupported_managed.contains("unsupported-tts-secret"));
    assert!(!unsupported_managed.contains("unsupported-stt-secret"));

    let recognized_home = tempfile::tempdir().expect("create recognized-voice HOME");
    let recognized_fake = write_fake_tycode_binary(recognized_home.path());
    let recognized_source = write_shared_tycode_settings(
        recognized_home.path(),
        r#"[voice]
default_tts = "polly"
default_stt = "transcribe"

[voice.tts_providers.polly]
type = "aws_polly"
profile = "tts-profile"

[voice.tts_providers.eleven]
type = "elevenlabs"
api_key = ""
voice_id = "voice-id"
model_id = "tts-model"

[voice.stt_providers.transcribe]
type = "aws_transcribe"
profile = "stt-profile"
region = "eu-central-1"

[voice.stt_providers.eleven]
type = "elevenlabs"
api_key = ""
model_id = "stt-model"
"#,
    );
    {
        let _home = EnvVarGuard::set("HOME", recognized_home.path().to_string_lossy().to_string());
        let _hermes =
            EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
        let mut fixture =
            Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode])
                .await;
        let payload = expect_backend_config_snapshots(
            &mut fixture.client,
            "recognized v0.10 voice provider preservation",
        )
        .await;
        let settings = tycode_native_snapshot(&payload)
            .settings
            .as_ref()
            .expect("recognized voice settings");
        assert_eq!(settings["voice"]["default_tts"], "polly");
        assert_eq!(settings["voice"]["default_stt"], "transcribe");
        assert_eq!(
            settings["voice"]["tts_providers"]["polly"]["type"],
            "aws_polly"
        );
        assert_eq!(
            settings["voice"]["tts_providers"]["polly"]["profile"],
            "tts-profile"
        );
        assert_eq!(
            settings["voice"]["tts_providers"]["polly"]["region"],
            "us-west-2"
        );
        assert_eq!(
            settings["voice"]["tts_providers"]["eleven"]["voice_id"],
            "voice-id"
        );
        assert_eq!(
            settings["voice"]["tts_providers"]["eleven"]["model_id"],
            "tts-model"
        );
        assert_eq!(
            settings["voice"]["stt_providers"]["transcribe"]["type"],
            "aws_transcribe"
        );
        assert_eq!(
            settings["voice"]["stt_providers"]["transcribe"]["profile"],
            "stt-profile"
        );
        assert_eq!(
            settings["voice"]["stt_providers"]["transcribe"]["region"],
            "eu-central-1"
        );
        assert_eq!(
            settings["voice"]["stt_providers"]["eleven"]["model_id"],
            "stt-model"
        );
    }
    assert_eq!(
        std::fs::read(recognized_home.path().join(".tycode/settings.toml"))
            .expect("read recognized voice source"),
        recognized_source
    );
    let recognized_events = recognized_fake.events();
    let recognized_spawns = recognized_events
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(recognized_spawns.len(), 6, "{recognized_spawns:#?}");
    assert_ne!(recognized_spawns[3]["pid"], recognized_spawns[4]["pid"]);
    assert_eq!(
        recognized_spawns[3]["settings_path"],
        recognized_spawns[4]["settings_path"]
    );
    let normalized = recognized_events
        .iter()
        .find_map(|event| event["command"].get("SaveSettings"))
        .expect("recognized voice normalization save");
    assert_eq!(normalized["persist"], true);
    assert_eq!(
        normalized["settings"]["voice"]["tts_providers"]["polly"]["region"],
        "us-west-2"
    );
    assert_eq!(
        normalized["settings"]["voice"]["tts_providers"]["eleven"]["voice_id"],
        "voice-id"
    );
    assert_eq!(
        normalized["settings"]["voice"]["stt_providers"]["transcribe"]["profile"],
        "stt-profile"
    );
    assert_eq!(
        normalized["settings"]["voice"]["stt_providers"]["eleven"]["model_id"],
        "stt-model"
    );
    let recognized_managed =
        std::fs::read_to_string(recognized_home.path().join(".tycode/tyde-settings.toml"))
            .expect("read normalized recognized voice settings");
    assert!(recognized_managed.contains("type = \"aws_polly\""));
    assert!(recognized_managed.contains("region = \"us-west-2\""));
    assert!(recognized_managed.contains("type = \"aws_transcribe\""));
    assert!(recognized_managed.contains("voice_id = \"voice-id\""));
    assert!(recognized_managed.contains("model_id = \"stt-model\""));
}

#[tokio::test]
async fn tycode_dangling_provider_is_ready_without_secret_or_shared_fallback() {
    let _env_guard = env_lock().lock().await;
    let dangling_home = tempfile::tempdir().expect("create dangling-provider HOME");
    write_fake_tycode_binary(dangling_home.path());
    let source_bytes = write_shared_tycode_settings(
        dangling_home.path(),
        r#"active_provider = "legacy-provider"

[providers.legacy-provider]
type = "unsupported-v09-provider"
api_key = "shared-only-secret"

[voice_provider]
type = "unsupported-voice"
credential = "shared-only-voice-secret"
"#,
    );
    let _home = EnvVarGuard::set("HOME", dangling_home.path().to_string_lossy().to_string());
    let _hermes = EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut dangling_fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let dangling = expect_backend_config_snapshots(
        &mut dangling_fixture.client,
        "Tycode dangling-provider projection",
    )
    .await;
    let tycode = tycode_native_snapshot(&dangling);
    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Ready);
    assert_eq!(
        tycode.settings.as_ref().expect("dangling settings")["active_provider"],
        "legacy-provider"
    );
    let advisory_message = tycode
        .advisories
        .iter()
        .find_map(|advisory| match advisory {
            BackendNativeSettingsAdvisory::UnsupportedActiveProvider { provider, message }
                if provider == "legacy-provider" =>
            {
                Some(message.as_str())
            }
            _ => None,
        })
        .expect("typed unsupported-active-provider advisory");
    assert!(advisory_message.contains("cannot model"));
    assert!(
        !advisory_message.contains("is unchanged")
            && !advisory_message.contains("still contains")
            && !advisory_message.contains("still there"),
        "the typed protocol advisory must not claim the shared file's current contents: {advisory_message}"
    );
    assert_eq!(
        std::fs::read(dangling_home.path().join(".tycode/settings.toml"))
            .expect("re-read dangling-provider source"),
        source_bytes
    );
    let response = serde_json::to_string(&dangling).expect("serialize dangling response");
    assert!(!response.contains("shared-only-secret"));
    assert!(!response.contains("shared-only-voice-secret"));
}

#[tokio::test]
async fn tycode_notice_acknowledgement_conflicts_stale_id_and_never_reimports_source() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    let source_bytes = write_shared_tycode_settings(
        temp_home.path(),
        r#"active_provider = "native-provider"
model_quality = "high"

[providers.native-provider]
type = "mock"
"#,
    );
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let initial =
        expect_backend_config_snapshots(&mut fixture.client, "initial Tycode projection").await;
    let tycode = tycode_native_snapshot(&initial);
    let (_, projection_id, _, notice_pending) = managed_projection_fields(tycode);
    assert!(notice_pending);
    let projection_id = projection_id.clone();
    let managed = temp_home.path().join(".tycode/tyde-settings.toml");
    let provenance = temp_home
        .path()
        .join(".tycode/tyde-settings.provenance.json");
    let managed_before_conflict =
        std::fs::read(&managed).expect("read managed settings before stale acknowledgement");
    let provenance_before_conflict =
        std::fs::read(&provenance).expect("read provenance before stale acknowledgement");
    let invocations_before_conflict = fake.events().len();

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::AcknowledgeTycodeProjectionNotice {
                backend: BackendKind::Tycode,
                projection_id: TycodeProjectionId("stale-projection-id".to_string()),
            },
        })
        .await
        .expect("send stale Tycode projection acknowledgement");
    let (conflict, conflict_refresh) = expect_command_error_and_backend_config(
        &mut fixture.client,
        "stale projection acknowledgement conflict and refresh",
    )
    .await;
    assert_eq!(conflict.request_kind, FrameKind::SetSetting);
    assert_eq!(conflict.code, CommandErrorCode::Conflict);
    assert!(!conflict.fatal);
    let conflict_snapshot = tycode_native_snapshot(&conflict_refresh);
    let (_, refreshed_projection_id, _, refreshed_notice_pending) =
        managed_projection_fields(conflict_snapshot);
    assert_eq!(refreshed_projection_id, &projection_id);
    assert!(refreshed_notice_pending);
    assert_eq!(
        std::fs::read(&managed).expect("managed settings after stale acknowledgement"),
        managed_before_conflict
    );
    assert_eq!(
        std::fs::read(&provenance).expect("provenance after stale acknowledgement"),
        provenance_before_conflict
    );
    let conflict_events = fake.events();
    let conflict_spawns = conflict_events[invocations_before_conflict..]
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(conflict_spawns.len(), 1);
    assert_eq!(
        conflict_spawns[0]["settings_path"],
        managed.to_string_lossy().as_ref()
    );

    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("read source after stale acknowledgement conflict"),
        source_bytes,
        "stale notice acknowledgement must preserve the initial shared source bytes"
    );

    let changed_source = write_shared_tycode_settings(
        temp_home.path(),
        r#"active_provider = "native-provider"
model_quality = "low"

[providers.native-provider]
type = "mock"
"#,
    );
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::AcknowledgeTycodeProjectionNotice {
                backend: BackendKind::Tycode,
                projection_id,
            },
        })
        .await
        .expect("acknowledge current Tycode projection notice");
    let acknowledged = expect_backend_config_snapshots(
        &mut fixture.client,
        "acknowledged Tycode projection snapshot",
    )
    .await;
    let tycode = tycode_native_snapshot(&acknowledged);
    let (_, _, _, notice_pending) = managed_projection_fields(tycode);
    assert!(!notice_pending);
    assert_eq!(
        tycode.settings.as_ref().expect("managed settings")["model_quality"],
        "high",
        "an existing projection must never re-import later shared-file changes"
    );
    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("read deliberately changed shared settings"),
        changed_source
    );
}

#[tokio::test]
async fn tycode_malformed_transaction_journal_surfaces_resettable_typed_recovery() {
    let _env_guard = env_lock().lock().await;
    let home = tempfile::tempdir().expect("create malformed-journal HOME");
    write_fake_tycode_binary(home.path());
    let source_bytes = write_shared_tycode_settings(
        home.path(),
        r#"active_provider = "native-provider"
default_agent = "builder"

[providers.native-provider]
type = "mock"
"#,
    );
    let _home = EnvVarGuard::set("HOME", home.path().to_string_lossy().to_string());
    let _hermes = EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let initial =
        expect_backend_config_snapshots(&mut fixture.client, "malformed-journal initial state")
            .await;
    let initial_snapshot = tycode_native_snapshot(&initial);
    let (_, projection_id, _, _) = managed_projection_fields(initial_snapshot);
    let projection_id = projection_id.clone();
    let directory = home.path().join(".tycode");
    let journal = directory.join("tyde-settings.transaction.json");
    let malformed = b"{ not a Tycode transaction journal\n";
    std::fs::write(&journal, malformed).expect("write malformed transaction journal");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600))
            .expect("secure malformed transaction journal");
    }

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::AcknowledgeTycodeProjectionNotice {
                backend: BackendKind::Tycode,
                projection_id,
            },
        })
        .await
        .expect("request refresh that encounters malformed journal");
    let (discovery_error, recovery_payload) = expect_command_error_and_backend_config(
        &mut fixture.client,
        "malformed journal typed recovery",
    )
    .await;
    assert_eq!(discovery_error.code, CommandErrorCode::Internal);
    let snapshot = tycode_native_snapshot(&recovery_payload);
    assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Unavailable);
    assert!(snapshot.settings.is_none());
    assert!(snapshot.groups.is_empty());
    let (reason, expected_id, expected_hash) = managed_recovery_fields(snapshot);
    assert!(
        reason.contains("Failed to parse Tycode transaction journal"),
        "malformed journal should be reported as the recovery reason: {reason}"
    );
    assert_eq!(
        std::fs::read(&journal).expect("read preserved malformed journal"),
        malformed,
        "recovery discovery must preserve malformed evidence until exact reset"
    );

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::ResetTycodeManagedProjection {
                backend: BackendKind::Tycode,
                expected_projection_id: expected_id.clone(),
                expected_state_hash: expected_hash.clone(),
            },
        })
        .await
        .expect("send exact reset for malformed journal recovery");
    let rederived = expect_backend_config_snapshots(
        &mut fixture.client,
        "malformed journal exact reset rederivation",
    )
    .await;
    let rederived = tycode_native_snapshot(&rederived);
    assert_eq!(rederived.status, BackendConfigSnapshotStatus::Ready);
    assert!(rederived.managed_projection_recovery.is_none());
    assert_eq!(
        rederived
            .settings
            .as_ref()
            .expect("rederived settings after malformed journal reset")["default_agent"],
        "builder"
    );
    assert!(!journal.exists());
    assert_eq!(
        std::fs::read(home.path().join(".tycode/settings.toml"))
            .expect("read shared settings after malformed journal reset"),
        source_bytes
    );
}

#[tokio::test]
async fn tycode_typed_reset_conflicts_without_deletion_then_rederives_current_source() {
    let _env_guard = env_lock().lock().await;
    let home = tempfile::tempdir().expect("create reset HOME");
    let fake = write_fake_tycode_binary(home.path());
    let original_source = write_shared_tycode_settings(
        home.path(),
        r#"active_provider = "native-provider"
model_quality = "high"

[providers.native-provider]
type = "mock"
"#,
    );
    let _home = EnvVarGuard::set("HOME", home.path().to_string_lossy().to_string());
    let _hermes = EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let initial =
        expect_backend_config_snapshots(&mut fixture.client, "reset initial projection").await;
    let initial_snapshot = tycode_native_snapshot(&initial);
    let (_, projection_id, _, _) = managed_projection_fields(initial_snapshot);
    let projection_id = projection_id.clone();
    let mut tyde_only_settings = initial_snapshot
        .settings
        .clone()
        .expect("initial reset settings");
    tyde_only_settings["model_quality"] = serde_json::json!("low");
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: tyde_only_settings,
            },
        })
        .await
        .expect("save Tyde-only edit before reset");
    let saved =
        expect_backend_config_snapshots(&mut fixture.client, "Tyde-only save before reset").await;
    assert_eq!(
        tycode_native_snapshot(&saved)
            .settings
            .as_ref()
            .expect("saved Tyde settings")["model_quality"],
        "low"
    );
    assert_eq!(
        std::fs::read(home.path().join(".tycode/settings.toml"))
            .expect("read source after Tyde-only save"),
        original_source
    );

    let current_source = write_shared_tycode_settings(
        home.path(),
        r#"active_provider = "native-provider"
model_quality = "current-source"

[providers.native-provider]
type = "mock"

[shared_only]
secret = "reset-shared-secret"
"#,
    );
    let directory = home.path().join(".tycode");
    let managed = directory.join("tyde-settings.toml");
    let provenance = directory.join("tyde-settings.provenance.json");
    let recovery = directory.join("tyde-settings.recovery.json");
    let journal = directory.join("tyde-settings.transaction.json");
    let lock = directory.join("tyde-settings.lock");
    let mut tampered = std::fs::read(&managed).expect("read managed settings before tamper");
    tampered.extend_from_slice(b"\n# reset integration tamper\n");
    std::fs::write(&managed, &tampered).expect("tamper managed settings for reset state");

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::AcknowledgeTycodeProjectionNotice {
                backend: BackendKind::Tycode,
                projection_id,
            },
        })
        .await
        .expect("request refresh that discovers managed corruption");
    let (discovery_error, recovery_payload) = expect_command_error_and_backend_config(
        &mut fixture.client,
        "typed reset-required discovery",
    )
    .await;
    assert_eq!(discovery_error.code, CommandErrorCode::Internal);
    let recovery_snapshot = tycode_native_snapshot(&recovery_payload);
    assert_eq!(
        recovery_snapshot.status,
        BackendConfigSnapshotStatus::Unavailable
    );
    assert!(recovery_snapshot.settings.is_none());
    assert!(recovery_snapshot.groups.is_empty());
    let (reason, expected_id, expected_hash) = managed_recovery_fields(recovery_snapshot);
    assert!(reason.contains("integrity check failed"));
    let expected_id = expected_id.clone();
    let expected_hash = expected_hash.clone();
    let serialized =
        serde_json::to_string(&recovery_payload).expect("serialize typed reset-required response");
    assert!(serialized.contains("managed_projection_reset_required"));
    assert!(!serialized.contains("reset-shared-secret"));

    let preserved_before_conflicts = [
        std::fs::read(&managed).expect("read managed reset inventory"),
        std::fs::read(&provenance).expect("read provenance reset inventory"),
        std::fs::read(&recovery).expect("read recovery reset inventory"),
    ];
    let stale_id_setting = SetSettingPayload {
        setting: HostSettingValue::ResetTycodeManagedProjection {
            backend: BackendKind::Tycode,
            expected_projection_id: TycodeProjectionId("stale-projection".to_string()),
            expected_state_hash: expected_hash.clone(),
        },
    };
    let stale_id_json =
        serde_json::to_value(&stale_id_setting).expect("serialize typed stale-id reset command");
    assert_eq!(
        stale_id_json["setting"]["kind"],
        "reset_tycode_managed_projection"
    );
    assert_eq!(
        stale_id_json["setting"]["expected_state_hash"],
        expected_hash.0
    );
    fixture
        .client
        .set_setting(stale_id_setting)
        .await
        .expect("send stale projection reset");
    let (stale_id_error, _) = expect_command_error_and_backend_config(
        &mut fixture.client,
        "stale projection reset conflict",
    )
    .await;
    assert_eq!(stale_id_error.code, CommandErrorCode::Conflict);
    assert_eq!(
        [
            std::fs::read(&managed).expect("managed after stale id"),
            std::fs::read(&provenance).expect("provenance after stale id"),
            std::fs::read(&recovery).expect("recovery after stale id"),
        ],
        preserved_before_conflicts
    );

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::ResetTycodeManagedProjection {
                backend: BackendKind::Tycode,
                expected_projection_id: expected_id.clone(),
                expected_state_hash: TycodeProjectionStateHash("sha256:stale-state".to_string()),
            },
        })
        .await
        .expect("send stale state-hash reset");
    let (stale_hash_error, _) = expect_command_error_and_backend_config(
        &mut fixture.client,
        "stale state hash reset conflict",
    )
    .await;
    assert_eq!(stale_hash_error.code, CommandErrorCode::Conflict);
    assert_eq!(
        [
            std::fs::read(&managed).expect("managed after stale hash"),
            std::fs::read(&provenance).expect("provenance after stale hash"),
            std::fs::read(&recovery).expect("recovery after stale hash"),
        ],
        preserved_before_conflicts
    );

    let transaction_owned = [
        ".tyde-settings.atomic-integration-reset.txn",
        ".tyde-settings.prejournal-source-integration-reset.txn",
        ".tyde-settings.integration-reset.managed-stage.txn",
        ".tyde-settings.integration-reset.provenance-stage.txn",
        ".tyde-settings.integration-reset.managed-backup.txn",
        ".tyde-settings.integration-reset.provenance-backup.txn",
    ]
    .map(|name| directory.join(name));
    for path in &transaction_owned {
        std::fs::write(path, b"transaction-owned reset artifact")
            .unwrap_or_else(|err| panic!("write transaction-owned reset artifact {path:?}: {err}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in &transaction_owned {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap_or_else(
                |err| panic!("secure transaction-owned reset artifact {path:?}: {err}"),
            );
        }
    }
    let unrelated = directory.join("user-owned.keep");
    std::fs::write(&unrelated, b"not managed by the reset transaction")
        .expect("write unrelated sentinel");
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::ResetTycodeManagedProjection {
                backend: BackendKind::Tycode,
                expected_projection_id: expected_id,
                expected_state_hash: expected_hash,
            },
        })
        .await
        .expect("send reset made stale by inventory change");
    let (inventory_conflict, refreshed_recovery_payload) = expect_command_error_and_backend_config(
        &mut fixture.client,
        "inventory-changing reset conflict",
    )
    .await;
    assert_eq!(inventory_conflict.code, CommandErrorCode::Conflict);
    assert!(transaction_owned.iter().all(|path| path.is_file()));
    assert!(unrelated.is_file());
    let (_, refreshed_id, refreshed_hash) =
        managed_recovery_fields(tycode_native_snapshot(&refreshed_recovery_payload));
    let exact_reset = SetSettingPayload {
        setting: HostSettingValue::ResetTycodeManagedProjection {
            backend: BackendKind::Tycode,
            expected_projection_id: refreshed_id.clone(),
            expected_state_hash: refreshed_hash.clone(),
        },
    };
    let exact_reset_json =
        serde_json::to_string(&exact_reset).expect("serialize exact typed reset command");
    assert!(exact_reset_json.contains("reset_tycode_managed_projection"));
    assert!(!exact_reset_json.contains("reset-shared-secret"));
    let events_before_reset = fake.events().len();
    fixture
        .client
        .set_setting(exact_reset)
        .await
        .expect("send exact-token managed projection reset");
    let rederived = expect_backend_config_snapshots(
        &mut fixture.client,
        "authoritative snapshot after exact reset",
    )
    .await;
    let rederived_snapshot = tycode_native_snapshot(&rederived);
    assert_eq!(
        rederived_snapshot.status,
        BackendConfigSnapshotStatus::Ready
    );
    assert!(rederived_snapshot.managed_projection_recovery.is_none());
    assert_eq!(
        rederived_snapshot
            .settings
            .as_ref()
            .expect("rederived current-source settings")["model_quality"],
        "current-source",
        "confirmed reset must lose the old Tyde-only edit and lazily derive the current source"
    );
    let (_, rederived_id, source, _) = managed_projection_fields(rederived_snapshot);
    assert_ne!(rederived_id, refreshed_id);
    assert_eq!(source, TycodeProjectionSource::SharedSettings);
    assert_eq!(
        std::fs::read(home.path().join(".tycode/settings.toml"))
            .expect("read shared settings after exact reset"),
        current_source
    );
    assert!(transaction_owned.iter().all(|path| !path.exists()));
    assert!(unrelated.is_file());
    assert!(
        lock.is_file(),
        "reset must preserve the serialization lock file"
    );
    assert!(!journal.exists());
    assert!(!recovery.exists());
    let remaining_transaction_artifacts = std::fs::read_dir(&directory)
        .expect("inspect reset directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tyde-settings.") && name.ends_with(".txn"))
        .collect::<Vec<_>>();
    assert!(
        remaining_transaction_artifacts.is_empty(),
        "reset left managed transaction artifacts: {remaining_transaction_artifacts:?}"
    );
    let response = serde_json::to_string(&rederived).expect("serialize rederived response");
    assert!(!response.contains("reset-shared-secret"));
    let shared_path = home
        .path()
        .join(".tycode/settings.toml")
        .to_string_lossy()
        .into_owned();
    for event in &fake.events()[events_before_reset..] {
        if event["type"] != "spawn" {
            continue;
        }
        assert_ne!(event["settings_path"].as_str(), Some(shared_path.as_str()));
        let argv = event["argv"].as_array().expect("reset rederive argv");
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.as_str() == Some("--settings-path"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn tycode_save_verification_version_and_hash_fail_closed_without_fallback() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    let source_bytes = write_shared_tycode_settings(
        temp_home.path(),
        r#"active_provider = "native-provider"
model_quality = "high"

[providers.native-provider]
type = "mock"
"#,
    );
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let initial =
        expect_backend_config_snapshots(&mut fixture.client, "initial verified projection").await;
    let mut update = tycode_native_snapshot(&initial)
        .settings
        .clone()
        .expect("initial managed settings");
    update["model_quality"] = serde_json::json!("low");
    let managed = temp_home.path().join(".tycode/tyde-settings.toml");
    let provenance = temp_home
        .path()
        .join(".tycode/tyde-settings.provenance.json");
    let managed_before = std::fs::read(&managed).expect("read managed settings before failures");

    fake.set_behavior(serde_json::json!({ "mismatch_on_fresh_process": true }));
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: update.clone(),
            },
        })
        .await
        .expect("send save that fails fresh-process verification");
    let verification_error =
        expect_command_error(&mut fixture.client, "fresh-process verification failure").await;
    assert_eq!(verification_error.code, CommandErrorCode::Internal);
    assert!(
        verification_error
            .message
            .contains("Fresh-process verification"),
        "unexpected verification failure: {}",
        verification_error.message
    );
    assert_eq!(
        std::fs::read(&managed).expect("read managed settings after failed verification"),
        managed_before,
        "failed fresh-process verification must retain the prior authoritative projection"
    );
    fake.set_behavior(serde_json::json!({}));

    let provenance_before = std::fs::read(&provenance).expect("read projection provenance");
    let mut version_tamper: serde_json::Value =
        serde_json::from_slice(&provenance_before).expect("parse projection provenance");
    version_tamper["provenance"]["tycode_version"]["minor"] = serde_json::json!(9);
    std::fs::write(
        &provenance,
        serde_json::to_vec_pretty(&version_tamper).expect("serialize version tamper"),
    )
    .expect("write version-tampered provenance");
    let events_before_version = fake.events().len();
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: update.clone(),
            },
        })
        .await
        .expect("send save against wrong-version provenance");
    let version_error = expect_command_error(&mut fixture.client, "version fail closed").await;
    assert_eq!(version_error.code, CommandErrorCode::Internal);
    assert!(version_error.message.contains("pinned version 0.10.0"));
    assert_eq!(fake.events().len(), events_before_version);

    std::fs::write(&provenance, &provenance_before).expect("restore exact provenance bytes");
    let mut hash_tamper = managed_before.clone();
    hash_tamper.extend_from_slice(b"\n# tampered");
    std::fs::write(&managed, hash_tamper).expect("tamper managed settings bytes");
    let events_before_hash = fake.events().len();
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: BackendKind::Tycode,
                settings: update,
            },
        })
        .await
        .expect("send save against hash-tampered projection");
    let hash_error = expect_command_error(&mut fixture.client, "hash fail closed").await;
    assert_eq!(hash_error.code, CommandErrorCode::Internal);
    assert!(hash_error.message.contains("integrity check failed"));
    assert_eq!(fake.events().len(), events_before_hash);
    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("re-read shared settings after failures"),
        source_bytes,
        "verification, version, and hash failures must never fall back to or rewrite the source"
    );
}

#[tokio::test]
async fn tycode_client_spawn_resume_and_session_storage_use_managed_sibling_path() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    let source_bytes = write_shared_tycode_settings(
        temp_home.path(),
        r#"active_provider = "native-provider"

[providers.native-provider]
type = "mock"
"#,
    );
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut fixture = Fixture::new_with_real_tycode_backend().await;
    let _ = expect_backend_config_snapshots(
        &mut fixture.client,
        "initial native probe before real Tycode spawn",
    )
    .await;
    let events_before_spawn = fake.events().len();

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("managed-path-new-session".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![temp_home.path().to_string_lossy().to_string()],
                prompt: "record managed settings path".to_string(),
                images: None,
                backend_kind: BackendKind::Tycode,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn Tycode through client");
    let (new_agent, start) = expect_tycode_agent_launch(
        &mut fixture.client,
        &fake,
        None,
        "Tycode new-session NewAgent and AgentStart",
    )
    .await;
    assert_eq!(
        start.session_id.as_ref().map(|id| id.0.as_str()),
        Some("fake-session")
    );
    expect_tycode_turn_quiescent(
        &mut fixture.client,
        &new_agent.instance_stream,
        "original Tycode StreamEnd and idle transition",
    )
    .await;
    fixture
        .client
        .close_agent(&new_agent.instance_stream)
        .await
        .expect("close new Tycode agent");
    expect_agent_closed(
        &mut fixture.client,
        &new_agent.agent_id,
        "original Tycode AgentClosed",
    )
    .await;

    let managed = temp_home
        .path()
        .join(".tycode/tyde-settings.toml")
        .to_string_lossy()
        .to_string();
    let spawn_events = fake.events();
    let new_session_spawn = spawn_events[events_before_spawn..]
        .iter()
        .find(|event| event["type"] == "spawn")
        .expect("new-session fake process spawn");
    assert_eq!(new_session_spawn["settings_path"], managed);
    assert!(
        temp_home
            .path()
            .join(".tycode/sessions/fake-session.json")
            .is_file(),
        "Tycode sessions must remain under the unchanged ~/.tycode root"
    );

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list Tycode sessions through client");
    let listed = expect_session_list(&mut fixture.client, "Tycode SessionList").await;
    assert!(listed.sessions.iter().any(|session| {
        session.id.0 == "fake-session" && session.backend_kind == BackendKind::Tycode
    }));

    let events_before_resume = fake.events().len();
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("managed-path-resume".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: SessionId("fake-session".to_string()),
                prompt: None,
            },
        })
        .await
        .expect("resume Tycode through client");
    eprintln!("ResumeAgent client send accepted session_id=fake-session");
    let (resumed, resumed_start) = expect_tycode_agent_launch(
        &mut fixture.client,
        &fake,
        Some("fake-session"),
        "resumed Tycode NewAgent and AgentStart",
    )
    .await;
    assert_eq!(
        resumed_start.session_id.as_ref().map(|id| id.0.as_str()),
        Some("fake-session")
    );
    fixture
        .client
        .close_agent(&resumed.instance_stream)
        .await
        .expect("close resumed Tycode agent");
    expect_agent_closed(
        &mut fixture.client,
        &resumed.agent_id,
        "resumed Tycode AgentClosed",
    )
    .await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list Tycode sessions after resumed close");
    let listed_after_resume = expect_session_list(
        &mut fixture.client,
        "Tycode SessionList after resumed close",
    )
    .await;
    assert!(listed_after_resume.sessions.iter().any(|session| {
        session.id.0 == "fake-session" && session.backend_kind == BackendKind::Tycode
    }));
    let resume_events = fake.events();
    let resume_spawn = resume_events[events_before_resume..]
        .iter()
        .find(|event| event["type"] == "spawn")
        .expect("resume fake process spawn");
    assert_eq!(resume_spawn["settings_path"], managed);
    let resume_commands = resume_events[events_before_resume..]
        .iter()
        .filter(|event| event["type"] == "command")
        .collect::<Vec<_>>();
    let resume_command_index = resume_commands
        .iter()
        .position(|event| event["command"]["ResumeSession"]["session_id"] == "fake-session")
        .expect("fake ResumeSession command with persisted session identity");
    let replay_sentinel_index = resume_commands
        .iter()
        .position(|event| event["command"] == "ListSessions")
        .expect("fake ListSessions replay sentinel command");
    assert!(
        resume_command_index < replay_sentinel_index,
        "ResumeSession must precede the ListSessions replay sentinel: {resume_commands:#?}"
    );
    assert_eq!(
        new_session_spawn["settings_path"], resume_spawn["settings_path"],
        "new-session and resume must receive the identical managed settings path"
    );
    assert!(
        !temp_home
            .path()
            .join(".tycode/tyde-settings/sessions")
            .exists(),
        "the managed filename must not become a profile session root"
    );
    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("re-read shared settings after spawn and resume"),
        source_bytes,
        "new-session and resume must leave the shared source byte-identical"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_tycode_setup_exit_refreshes_setup_session_and_native_snapshots() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    write_fake_tycode_binary(temp_home.path());
    let fake_bin = temp_home.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake setup PATH");
    let fake_curl = fake_bin.join("curl");
    std::fs::write(
        &fake_curl,
        "#!/bin/sh\necho trusted-setup-curl-failure >&2\nexit 23\n",
    )
    .expect("write failing fake curl");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake curl");
    }
    let path = format!(
        "{}:{}",
        fake_bin.to_string_lossy(),
        std::env::var("PATH").expect("test PATH")
    );
    let _path = EnvVarGuard::set("PATH", path);
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());
    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
    let _ = expect_backend_config_snapshots(
        &mut fixture.client,
        "initial native snapshot before trusted setup",
    )
    .await;

    let request = RunBackendSetupPayload {
        backend_kind: BackendKind::Tycode,
        action: BackendSetupAction::Install,
    };
    let request_json = serde_json::to_value(&request).expect("serialize setup request");
    assert_eq!(
        request_json
            .as_object()
            .expect("setup request object")
            .len(),
        2
    );
    assert!(request_json.get("program").is_none());
    assert!(request_json.get("arguments").is_none());
    send_host_payload(&mut fixture.client, FrameKind::RunBackendSetup, &request).await;

    let terminal = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read event before setup NewTerminal")
            .expect("connection closed before setup NewTerminal");
        if env.kind == FrameKind::NewTerminal {
            break env
                .parse_payload::<NewTerminalPayload>()
                .expect("parse setup NewTerminal");
        }
    };
    let bootstrap = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read event before setup TerminalBootstrap")
            .expect("connection closed before setup TerminalBootstrap");
        if env.kind == FrameKind::TerminalBootstrap && env.stream == terminal.stream {
            break env
                .parse_payload::<TerminalBootstrapPayload>()
                .expect("parse setup TerminalBootstrap");
        }
    };
    assert_eq!(bootstrap.terminal_id, terminal.terminal_id);
    assert_eq!(bootstrap.start.shell, "/bin/sh");

    let mut output = String::new();
    let exit = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read trusted setup terminal event")
            .expect("connection closed before trusted setup exit");
        if env.stream != terminal.stream {
            continue;
        }
        match env.kind {
            FrameKind::TerminalOutput => {
                let payload: TerminalOutputPayload =
                    env.parse_payload().expect("parse setup TerminalOutput");
                output.push_str(&payload.data);
            }
            FrameKind::TerminalExit => {
                break env
                    .parse_payload::<TerminalExitPayload>()
                    .expect("parse setup TerminalExit");
            }
            FrameKind::TerminalError => panic!("trusted setup emitted TerminalError"),
            _ => {}
        }
    };
    assert_eq!(exit.exit_code, Some(23));
    assert!(output.contains("$ /bin/sh '"));
    assert!(!output.contains("/bin/sh -l"));
    assert!(output.contains("trusted-setup-curl-failure"));
    let staged_start = output
        .find("$ /bin/sh '")
        .expect("truthful staged setup invocation")
        + "$ /bin/sh '".len();
    let staged_end = output[staged_start..]
        .find('\'')
        .expect("closing quote for staged setup path")
        + staged_start;
    let staged_path = PathBuf::from(&output[staged_start..staged_end]);

    let mut refresh_order = Vec::new();
    let mut refreshed_setup = None;
    let mut refreshed_native = None;
    while refreshed_native.is_none() {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read post-setup refresh event")
            .expect("connection closed before post-setup refresh completed");
        match env.kind {
            FrameKind::BackendSetup => {
                refresh_order.push(FrameKind::BackendSetup);
                refreshed_setup = Some(
                    env.parse_payload::<BackendSetupPayload>()
                        .expect("parse refreshed BackendSetup"),
                );
            }
            FrameKind::SessionSchemas => refresh_order.push(FrameKind::SessionSchemas),
            FrameKind::BackendConfigSnapshots => {
                refresh_order.push(FrameKind::BackendConfigSnapshots);
                refreshed_native = Some(
                    env.parse_payload::<BackendConfigSnapshotsPayload>()
                        .expect("parse refreshed BackendConfigSnapshots"),
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        refresh_order,
        vec![
            FrameKind::BackendSetup,
            FrameKind::SessionSchemas,
            FrameKind::BackendConfigSnapshots
        ]
    );
    let setup = refreshed_setup.expect("forced BackendSetup refresh");
    assert!(setup.backends.iter().any(|backend| {
        backend.backend_kind == BackendKind::Tycode
            && backend.status == BackendSetupStatus::Installed
    }));
    let native = refreshed_native.expect("forced native settings refresh");
    assert_eq!(
        tycode_native_snapshot(&native).status,
        BackendConfigSnapshotStatus::Ready
    );
    assert!(
        !staged_path.exists(),
        "trusted setup script must be removed after terminal exit and refresh"
    );
}
