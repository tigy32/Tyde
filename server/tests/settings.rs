mod fixture;

use settings_model::HostSettingsPayload;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use fixture::{
    Fixture, next_frame_matching_on, next_frame_matching_strict_on,
    routine_control_plane_noise_plus,
};
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentClosedPayload, AgentErrorPayload,
    AgentStartPayload, BackendConfigSnapshotStatus, BackendConfigSnapshotsPayload,
    BackendConfigValues, BackendKind, BackendNativeSettingsAdvisory,
    BackendNativeSettingsGroupKind, BackendSetupAction, BackendSetupDiagnosticCode,
    BackendSetupPayload, BackendSetupStatus, ChatEvent, CodeIntelProviderId, CommandErrorPayload,
    Envelope, FrameKind, ListSessionsPayload, NewAgentPayload, NewTerminalPayload,
    RunBackendSetupPayload, SessionId, SessionListPayload, SessionSchemasPayload,
    SessionSettingValue, SettingExpectation, SettingOp, SettingsErrorCode, SettingsWriteId,
    SettingsWritePayload, SettingsWriteResultPayload, SpawnAgentParams, SpawnAgentPayload,
    StreamPath, TerminalBootstrapPayload, TerminalExitPayload, TerminalOutputPayload,
};
use server::backend::BackendSession;
use server::store::session::SessionStore;
use server::store::settings::HostSettingsStore;
use settings_model::{HostExecutablePath, HostSettings};
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

fn panic_on_command_error(env: &Envelope, context: &str) {
    if env.kind == FrameKind::CommandError {
        let error: CommandErrorPayload = env
            .parse_payload()
            .unwrap_or_else(|err| panic!("failed to parse CommandError for {context}: {err}"));
        panic!("unexpected CommandError before {context}: {error:?}");
    }
}

async fn expect_backend_config_snapshots(
    client: &mut client::Connection,
    context: &str,
) -> BackendConfigSnapshotsPayload {
    next_frame_matching_on(client, context, |env| {
        panic_on_command_error(env, context);
        env.kind == FrameKind::BackendConfigSnapshots
    })
    .await
    .parse_payload()
    .unwrap_or_else(|err| panic!("failed to parse BackendConfigSnapshots for {context}: {err}"))
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
    next_frame_matching_on(client, context, |env| {
        panic_on_command_error(env, context);
        env.kind == FrameKind::SessionList
    })
    .await
    .parse_payload()
    .unwrap_or_else(|err| panic!("failed to parse SessionList for {context}: {err}"))
}

async fn expect_tycode_turn_quiescent(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) {
    let mut saw_stream_end = false;
    next_frame_matching_on(client, context, |env| {
        panic_on_command_error(env, context);
        if env.stream != *stream {
            return false;
        }
        match env.kind {
            FrameKind::ChatEvent => {
                let event: ChatEvent = env
                    .parse_payload()
                    .unwrap_or_else(|err| panic!("failed to parse ChatEvent for {context}: {err}"));
                match event {
                    ChatEvent::StreamEnd(_) => {
                        saw_stream_end = true;
                        false
                    }
                    ChatEvent::TypingStatusChanged(false) => saw_stream_end,
                    _ => false,
                }
            }
            FrameKind::AgentError => {
                let error: AgentErrorPayload = env.parse_payload().unwrap_or_else(|err| {
                    panic!("failed to parse AgentError for {context}: {err}")
                });
                panic!("unexpected AgentError before {context}: {error:?}");
            }
            _ => false,
        }
    })
    .await;
}

async fn expect_agent_closed(
    client: &mut client::Connection,
    expected_agent_id: &protocol::AgentId,
    context: &str,
) {
    let closed: AgentClosedPayload = next_frame_matching_on(client, context, |env| {
        panic_on_command_error(env, context);
        if env.kind == FrameKind::AgentError {
            let error: AgentErrorPayload = env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse AgentError for {context}: {err}"));
            panic!("unexpected AgentError before {context}: {error:?}");
        }
        env.kind == FrameKind::AgentClosed
    })
    .await
    .parse_payload()
    .unwrap_or_else(|err| panic!("failed to parse AgentClosed for {context}: {err}"));
    assert_eq!(&closed.agent_id, expected_agent_id);
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
import time
import tomllib

behavior_path = __BEHAVIOR_PATH__
log_path = __LOG_PATH__
with open(behavior_path, "r", encoding="utf-8") as behavior_file:
    behavior = json.load(behavior_file)

# Widens the window every fake-Tycode process holds open, so concurrency
# tests can deterministically overlap a second settings write with an
# in-flight refresh. Default off.
time.sleep(behavior.get("startup_delay_seconds", 0))

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
        persist_gate_path = behavior.get("persist_gate_path")
        if save.get("persist") is True and persist_gate_path:
            log({"type": "persist_gate_entered", "pid": os.getpid()})
            while not os.path.exists(persist_gate_path):
                time.sleep(0.01)
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
        emit({"kind": "StreamDelta", "data": {"text": "fake Tycode response"}})
        emit({
            "kind": "StreamEnd",
            "data": {
                "message": {
                    "message_id": "fake-message",
                    "timestamp": 1,
                    "sender": {"Assistant": {"agent": "tycode"}},
                    "content": "fake Tycode response",
                    "reasoning": None,
                    "tool_calls": [],
                    "model_info": {"model": "ClaudeSonnet46"},
                    "token_usage": None,
                    "context_breakdown": None,
                    "images": [],
                },
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
        mobile_broker_auth: Default::default(),
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
        tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
        complexity_tiers_enabled: false,
        backend_tier_configs: std::collections::HashMap::new(),
        background_agent_features: Default::default(),
        supervisor: Default::default(),
        code_intel: Default::default(),
        backend_config: std::collections::HashMap::new(),
        launch_profiles: Default::default(),
        hermes_disabled_providers: Default::default(),
        voice: Default::default(),
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
fn persisted_native_voice_settings_migrate_without_legacy_network_fields() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": [],
    "default_backend": null,
    "voice": {
      "enabled": true,
      "aws_profile": "production-profile",
      "aws_region": "us-west-2",
      "nova_model": "amazon.nova-2-sonic-v1:0",
      "availability": {"kind": "unavailable", "reason": "credentials_expired"},
      "webrtc_url": "wss://legacy.invalid",
      "udp_port": 3478,
      "stun_servers": ["stun:legacy.invalid"],
      "turn_servers": ["turn:legacy.invalid"]
    }
  }
}"#,
    )
    .expect("write current voice settings with rejected legacy network fields");

    let store = HostSettingsStore::load(path).expect("load native voice settings migration");
    let settings = store.get().expect("read migrated native voice settings");

    assert!(settings.voice.enabled);
    assert_eq!(
        settings.voice.aws_profile.as_deref(),
        Some("production-profile")
    );
    assert_eq!(settings.voice.aws_region.as_deref(), Some("us-west-2"));
    assert_eq!(settings.voice.nova_model, "amazon.nova-2-sonic-v1:0");
    assert_eq!(
        settings.voice.endpointing_sensitivity,
        settings_model::VoiceEndpointingSensitivity::Low,
        "voice settings written before endpointing control default to the patient value"
    );
    assert_eq!(
        settings.voice.availability,
        protocol::VoiceAvailability::Available,
        "persisted availability is untrusted and recomputed from current native settings"
    );
    let migrated = serde_json::to_value(settings).expect("serialize migrated settings");
    let voice = migrated.get("voice").expect("serialized voice settings");
    for removed in ["webrtc_url", "udp_port", "stun_servers", "turn_servers"] {
        assert!(
            voice.get(removed).is_none(),
            "rejected legacy network field {removed} must not enter current settings"
        );
    }
}

#[test]
fn persisted_legacy_supervisor_uses_default_compaction_minimum() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": [],
    "default_backend": null,
    "supervisor": {
      "enabled": true,
      "auto_compact_on_success": true,
      "max_kicks_per_task": 3,
      "retry_attempts": 1,
      "cost_tier": "low"
    }
  }
}"#,
    )
    .expect("write legacy supervisor settings store");

    let supervisor = HostSettingsStore::load(path)
        .expect("load legacy supervisor settings")
        .get()
        .expect("read legacy supervisor settings")
        .supervisor;
    assert_eq!(supervisor.auto_compact_min_context_tokens, 200_000);
    assert_eq!(supervisor.auto_compact_inactivity_delay_seconds, 300);
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
            mobile_broker_auth: Default::default(),
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            supervisor: Default::default(),
            code_intel: Default::default(),
            backend_config: std::collections::HashMap::new(),
            launch_profiles: Default::default(),
            hermes_disabled_providers: Default::default(),
            voice: Default::default(),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn supervisor_settings_apply_and_validate_over_protocol() {
    let mut fixture = Fixture::new().await;
    let defaults = &fixture.bootstrap.settings.supervisor;
    assert!(!defaults.enabled);
    assert!(!defaults.auto_compact_on_success);
    assert_eq!(defaults.auto_compact_min_context_tokens, 200_000);
    assert_eq!(defaults.auto_compact_inactivity_delay_seconds, 300);
    assert_eq!(defaults.max_kicks_per_task, 3);
    assert_eq!(defaults.retry_attempts, 1);

    let valid = [
        (
            "supervisor-enabled",
            "/supervisor/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "supervisor-auto",
            "/supervisor/auto_compact_on_success",
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "supervisor-delay-min",
            "/supervisor/auto_compact_inactivity_delay_seconds",
            serde_json::json!(1),
            serde_json::json!(300),
        ),
        (
            "supervisor-delay-max",
            "/supervisor/auto_compact_inactivity_delay_seconds",
            serde_json::json!(86_400),
            serde_json::json!(1),
        ),
        (
            "supervisor-delay-final",
            "/supervisor/auto_compact_inactivity_delay_seconds",
            serde_json::json!(17),
            serde_json::json!(86_400),
        ),
        (
            "supervisor-context-nonzero",
            "/supervisor/auto_compact_min_context_tokens",
            serde_json::json!(275_000),
            serde_json::json!(200_000),
        ),
        (
            "supervisor-context-zero",
            "/supervisor/auto_compact_min_context_tokens",
            serde_json::json!(0),
            serde_json::json!(275_000),
        ),
        (
            "supervisor-kicks",
            "/supervisor/max_kicks_per_task",
            serde_json::json!(5),
            serde_json::json!(3),
        ),
        (
            "supervisor-retries-zero",
            "/supervisor/retry_attempts",
            serde_json::json!(0),
            serde_json::json!(1),
        ),
        (
            "supervisor-retries-max",
            "/supervisor/retry_attempts",
            serde_json::json!(5),
            serde_json::json!(0),
        ),
        (
            "supervisor-tier",
            "/supervisor/cost_tier",
            serde_json::json!("high"),
            serde_json::json!("low"),
        ),
    ];
    for (write_id, path, value, expected) in valid {
        send_settings_write(
            &mut fixture.client,
            write_id,
            vec![replace_op(path, value, expected)],
        )
        .await;
        let fanout = expect_host_settings_frame(&mut fixture.client, write_id).await;
        let result = expect_settings_write_result(&mut fixture.client, write_id, write_id).await;
        assert!(result.applied, "{write_id}: {:?}", result.field_errors);
        assert_eq!(result.current_etag, fanout.etag);
    }

    for (write_id, path, value, expected) in [
        (
            "supervisor-delay-zero",
            "/supervisor/auto_compact_inactivity_delay_seconds",
            serde_json::json!(0),
            serde_json::json!(17),
        ),
        (
            "supervisor-delay-over",
            "/supervisor/auto_compact_inactivity_delay_seconds",
            serde_json::json!(86_401),
            serde_json::json!(17),
        ),
        (
            "supervisor-kicks-zero",
            "/supervisor/max_kicks_per_task",
            serde_json::json!(0),
            serde_json::json!(5),
        ),
        (
            "supervisor-retries-over",
            "/supervisor/retry_attempts",
            serde_json::json!(6),
            serde_json::json!(5),
        ),
    ] {
        send_settings_write(
            &mut fixture.client,
            write_id,
            vec![replace_op(path, value, expected)],
        )
        .await;
        let result = expect_settings_write_result(&mut fixture.client, write_id, write_id).await;
        assert!(!result.applied, "{write_id}");
        assert!(
            result
                .field_errors
                .iter()
                .any(|error| error.pointer == path && error.code == SettingsErrorCode::Invalid)
        );
    }

    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let persisted = bootstrap.settings.supervisor;
    assert!(persisted.enabled);
    assert!(persisted.auto_compact_on_success);
    assert_eq!(persisted.auto_compact_inactivity_delay_seconds, 17);
    assert_eq!(persisted.auto_compact_min_context_tokens, 0);
    assert_eq!(persisted.max_kicks_per_task, 5);
    assert_eq!(persisted.retry_attempts, 5);
    assert_eq!(
        persisted.cost_tier,
        settings_model::SupervisorCostTier::High
    );
}

#[test]
fn invalid_persisted_supervisor_inactivity_delay_is_rejected() {
    for seconds in [0_u32, 86_401] {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            format!(
                r#"{{
  "settings": {{
    "enabled_backends": [],
    "default_backend": null,
    "supervisor": {{
      "auto_compact_inactivity_delay_seconds": {seconds}
    }}
  }}
}}"#
            ),
        )
        .expect("write invalid supervisor settings store");
        let error = HostSettingsStore::load(path)
            .expect_err("invalid persisted inactivity delay must fail load");
        assert!(
            error.contains("inactivity delay must be between 1 and 86400 seconds"),
            "unexpected error for {seconds}: {error}"
        );
    }
}

#[test]
fn invalid_persisted_supervisor_retry_attempts_is_rejected() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": [],
    "default_backend": null,
    "supervisor": { "retry_attempts": 6 }
  }
}"#,
    )
    .expect("write invalid supervisor settings store");
    let error =
        HostSettingsStore::load(path).expect_err("invalid persisted retry attempts must fail load");
    assert!(error.contains("retry attempts must be between 0 and 5"));
}

#[tokio::test(start_paused = true)]
async fn code_intel_language_server_path_sets_and_clears_over_protocol() {
    let mut fixture = Fixture::new().await;
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());
    let executable = HostExecutablePath("/opt/rust-analyzer/bin/rust-analyzer".to_owned());
    assert!(
        fixture
            .bootstrap
            .settings
            .code_intel
            .language_server_paths
            .is_empty()
    );

    send_settings_write(
        &mut fixture.client,
        "code-intel-set",
        vec![replace_op(
            "/code_intel/language_server_paths/rust-analyzer",
            serde_json::to_value(&executable).expect("serialize executable path"),
            serde_json::Value::Null,
        )],
    )
    .await;
    let set = expect_host_settings_frame(&mut fixture.client, "set code-intel path").await;
    assert_eq!(
        set.settings.code_intel.language_server_paths.get(&provider),
        Some(&executable)
    );
    assert!(
        expect_settings_write_result(
            &mut fixture.client,
            "code-intel-set",
            "set code-intel result"
        )
        .await
        .applied
    );

    send_settings_write(
        &mut fixture.client,
        "code-intel-clear",
        vec![SettingOp::Remove {
            path: "/code_intel/language_server_paths/rust-analyzer".to_owned(),
            expected: SettingExpectation::Value {
                value: serde_json::to_value(&executable).expect("serialize executable path"),
            },
        }],
    )
    .await;
    let cleared = expect_host_settings_frame(&mut fixture.client, "clear code-intel path").await;
    assert!(cleared.settings.code_intel.language_server_paths.is_empty());
    assert!(
        expect_settings_write_result(
            &mut fixture.client,
            "code-intel-clear",
            "clear code-intel result"
        )
        .await
        .applied
    );
}

#[tokio::test(start_paused = true)]
async fn backend_config_updates_are_refused_over_client_events() {
    let mut fixture = Fixture::new().await;

    let mut model = BackendConfigValues::default();
    model.0.insert(
        "default_model".to_owned(),
        SessionSettingValue::String("anthropic/claude-sonnet-5".to_owned()),
    );
    send_settings_write(
        &mut fixture.client,
        "hermes-config-refusal",
        vec![replace_op(
            "/backend_config/hermes",
            serde_json::to_value(model).expect("serialize backend config"),
            serde_json::Value::Null,
        )],
    )
    .await;
    let error = expect_settings_write_result(
        &mut fixture.client,
        "hermes-config-refusal",
        "Hermes backend config refusal",
    )
    .await;
    assert!(!error.applied);
    assert!(
        error.field_errors.iter().any(|field| field
            .message
            .contains("does not support backend configuration")),
        "unexpected refusal message: {error:?}"
    );
    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        !bootstrap
            .settings
            .backend_config
            .contains_key(&BackendKind::Hermes)
    );
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
    let doc = tycode.settings.as_ref().expect("current Tycode settings");
    assert_eq!(doc["version"], 1);
    let doc_profiles = doc["profiles"].as_array().expect("Tycode profiles");
    assert_eq!(doc_profiles.len(), 1, "only the default profile exists");
    assert_eq!(doc_profiles[0]["name"], "default");
    assert_eq!(
        doc_profiles[0]["settings_path"].as_str(),
        Some(
            temp_home
                .path()
                .join(".tycode/settings.toml")
                .to_string_lossy()
                .as_ref()
        )
    );
    let settings = &doc_profiles[0]["settings"];
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
    // Direct probing must never create or touch a Tyde-managed copy.
    for retired in [
        ".tycode/tyde-settings.toml",
        ".tycode/tyde-settings.provenance.json",
        ".tycode/tyde-settings.transaction.json",
        ".tycode/tyde-settings.lock",
    ] {
        assert!(
            !temp_home.path().join(retired).exists(),
            "retired managed projection artifact {retired} must not be created"
        );
    }
    assert_eq!(
        std::fs::read(temp_home.path().join(".tycode/settings.toml"))
            .expect("re-read shared Tycode settings"),
        source_bytes,
        "probing must leave the shared settings byte-identical"
    );
    let response = serde_json::to_string(&payload).expect("serialize typed settings response");
    assert!(
        !response.contains("shared-only-secret"),
        "unrecognized source-only tables are dropped by Tycode's own settings parser and must \
         not enter typed responses"
    );
    let initial_spawns = fake
        .events()
        .into_iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    let shared_path = temp_home
        .path()
        .join(".tycode/settings.toml")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        initial_spawns.len(),
        1,
        "direct probing needs exactly one subprocess against the shared settings file"
    );
    assert_eq!(
        initial_spawns[0]["settings_path"].as_str(),
        Some(shared_path.as_str())
    );
    assert_eq!(initial_spawns[0]["settings_existed_before"], true);
    let argv = initial_spawns[0]["argv"]
        .as_array()
        .expect("initial fake Tycode argv");
    assert_eq!(
        argv.iter()
            .filter(|argument| argument.as_str() == Some("--settings-path"))
            .count(),
        1
    );
    assert!(
        !fake.events().iter().any(|event| {
            event["type"] == "command" && event["command"]["SaveSettings"]["persist"] == true
        }),
        "probing must never persist settings"
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
        message, "Tycode process exited during native settings probe: waiting for SettingsSchema",
        "Tycode probe failure should identify the exact failed phase"
    );
    assert!(tycode.settings.is_none());
    assert!(tycode.groups.is_empty());
    assert!(
        !temp_home.path().join(".tycode/tyde-settings.toml").exists(),
        "no Tyde-managed settings copy may ever be created"
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
            Some("Cannot start Tycode native settings probe: tycode-subprocess not found")
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
async fn backend_native_settings_persist_and_profile_actions_publish_resources() {
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
        "the typed native settings contract must be ready before saving: {:?}",
        initial_snapshot.message
    );
    let initial_doc = initial_snapshot
        .settings
        .clone()
        .expect("initial current Tycode settings");
    assert_eq!(initial_doc["profiles"][0]["name"], "default");
    let base_settings = initial_doc["profiles"][0]["settings"].clone();
    let mut edited_settings = base_settings.clone();
    edited_settings
        .as_object_mut()
        .expect("Tycode settings object")
        .insert("model_quality".to_string(), serde_json::json!("low"));
    let save_doc = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "default",
            "settings_path": initial_doc["profiles"][0]["settings_path"],
            "settings": edited_settings,
            "base_settings": base_settings,
        }],
    });

    fixture
        .client
        .backend_native_settings_write(BackendKind::Tycode, save_doc)
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
    let refreshed_doc = tycode
        .settings
        .as_ref()
        .expect("refreshed current settings");
    let refreshed_settings = &refreshed_doc["profiles"][0]["settings"];
    assert_eq!(refreshed_settings["model_quality"], "low");
    assert_eq!(refreshed_settings["profile"], "default");
    assert_eq!(
        refreshed_settings["providers"], base_settings["providers"],
        "save and refresh must preserve unrelated provider settings"
    );
    assert_eq!(
        refreshed_settings["modules"], base_settings["modules"],
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
    let shared_path = temp_home
        .path()
        .join(".tycode/settings.toml")
        .to_string_lossy()
        .into_owned();
    let save_events = fake.events();
    let spawns = save_events[events_before_save..]
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(
        spawns.len(),
        3,
        "a save uses a stale-check probe, a save process, and the refresh probe"
    );
    for spawn in &spawns {
        assert_eq!(
            spawn["settings_path"].as_str(),
            Some(shared_path.as_str()),
            "every settings process must target the real shared settings file"
        );
        let argv = spawn["argv"].as_array().expect("fake Tycode argv");
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.as_str() == Some("--settings-path"))
                .count(),
            1
        );
    }
    assert_ne!(spawns[0]["pid"], spawns[1]["pid"]);
    let shared_toml = std::fs::read_to_string(temp_home.path().join(".tycode/settings.toml"))
        .expect("read shared settings after direct native save");
    assert!(
        shared_toml.contains("model_quality = \"low\""),
        "a direct save must write the real shared settings file: {shared_toml}"
    );
    assert_ne!(
        shared_toml.as_bytes(),
        source_bytes.as_slice(),
        "direct saves edit the user's file instead of a managed copy"
    );
    assert!(
        !temp_home.path().join(".tycode/tyde-settings.toml").exists(),
        "no Tyde-managed settings copy may be created by a save"
    );

    // An unchanged document round-trips without writing the file again.
    let events_before_second_save = fake.events().len();
    let noop_doc = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "default",
            "settings_path": refreshed_doc["profiles"][0]["settings_path"],
            "settings": refreshed_settings,
            "base_settings": refreshed_settings,
        }],
    });
    fixture
        .client
        .backend_native_settings_write(BackendKind::Tycode, noop_doc)
        .await
        .expect("persist an unchanged Tycode native settings document");
    let second_refresh = expect_backend_config_snapshots(
        &mut fixture.client,
        "native snapshot after unchanged Tycode native settings persist",
    )
    .await;
    let second_snapshot = tycode_native_snapshot(&second_refresh);
    assert_eq!(second_snapshot.status, BackendConfigSnapshotStatus::Ready);
    let second_events = fake.events();
    let second_spawns = second_events[events_before_second_save..]
        .iter()
        .filter(|event| event["type"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(
        second_spawns.len(),
        2,
        "an unchanged save needs only the stale-check probe and the refresh probe"
    );
    assert!(
        !second_events[events_before_second_save..]
            .iter()
            .any(|event| {
                event["type"] == "command" && event["command"]["SaveSettings"]["persist"] == true
            }),
        "an unchanged save must not rewrite the settings file"
    );

    let create_id = fixture
        .client
        .invoke_settings_action(
            BackendKind::Tycode,
            "profiles",
            "create",
            serde_json::json!({ "name": "review", "copy_from": "default" }),
        )
        .await
        .expect("create Tycode profile resource");
    let (created, create_result) = expect_settings_action_outcome(
        &mut fixture.client,
        &create_id,
        "Tycode snapshot after profile creation",
    )
    .await;
    let created_doc = tycode_native_snapshot(&created)
        .settings
        .as_ref()
        .expect("created profile settings document");
    let created_profiles = created_doc["profiles"]
        .as_array()
        .expect("authoritative Tycode profile list");
    assert!(
        created_profiles
            .iter()
            .any(|profile| profile["name"] == "review"),
        "created resource must appear in the authoritative profile list: {created_doc:?}"
    );
    assert!(created_doc["tombstones"].is_null());
    assert!(create_result.applied, "{:?}", create_result.field_errors);

    let delete_id = fixture
        .client
        .invoke_settings_action(
            BackendKind::Tycode,
            "profiles/review",
            "delete",
            serde_json::json!({}),
        )
        .await
        .expect("delete Tycode profile resource");
    let (deleted, delete_result) = expect_settings_action_outcome(
        &mut fixture.client,
        &delete_id,
        "Tycode snapshot after profile deletion",
    )
    .await;
    let deleted_doc = tycode_native_snapshot(&deleted)
        .settings
        .as_ref()
        .expect("deleted profile settings document");
    assert!(
        deleted_doc["profiles"]
            .as_array()
            .is_some_and(|profiles| profiles.iter().all(|profile| profile["name"] != "review")),
        "deleted resource must be absent from the authoritative list: {deleted_doc:?}"
    );
    assert_eq!(
        deleted_doc["tombstones"],
        serde_json::json!(["profiles/review"]),
        "the deletion snapshot must identify the retired resource"
    );
    assert!(delete_result.applied, "{:?}", delete_result.field_errors);
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
    drop(fixture);
    fake.set_behavior(serde_json::json!({
        "pre_session_advisory": true,
        "post_command_error": true
    }));
    let mut fixture =
        Fixture::new_with_real_backend_probe_for_enabled_backends(vec![BackendKind::Tycode]).await;
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
async fn tycode_client_spawn_resume_and_session_storage_use_shared_settings_path() {
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

    let shared = temp_home
        .path()
        .join(".tycode/settings.toml")
        .to_string_lossy()
        .to_string();
    let spawn_events = fake.events();
    let new_session_spawn = spawn_events[events_before_spawn..]
        .iter()
        .find(|event| event["type"] == "spawn")
        .expect("new-session fake process spawn");
    assert_eq!(new_session_spawn["settings_path"], shared);
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
    assert_eq!(resume_spawn["settings_path"], shared);
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
        "new-session and resume must receive the identical shared settings path"
    );
    assert!(
        !temp_home.path().join(".tycode/tyde-settings.toml").exists(),
        "no Tyde-managed settings copy may be created for sessions"
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

    let terminal: NewTerminalPayload =
        next_frame_matching_on(&mut fixture.client, "setup NewTerminal", |env| {
            env.kind == FrameKind::NewTerminal
        })
        .await
        .parse_payload()
        .expect("parse setup NewTerminal");
    let bootstrap: TerminalBootstrapPayload =
        next_frame_matching_on(&mut fixture.client, "setup TerminalBootstrap", |env| {
            env.kind == FrameKind::TerminalBootstrap && env.stream == terminal.stream
        })
        .await
        .parse_payload()
        .expect("parse setup TerminalBootstrap");
    assert_eq!(bootstrap.terminal_id, terminal.terminal_id);
    assert_eq!(bootstrap.start.shell, "/bin/sh");

    let mut output = String::new();
    let exit: TerminalExitPayload =
        next_frame_matching_on(&mut fixture.client, "trusted setup TerminalExit", |env| {
            if env.stream != terminal.stream {
                return false;
            }
            match env.kind {
                FrameKind::TerminalOutput => {
                    let payload: TerminalOutputPayload =
                        env.parse_payload().expect("parse setup TerminalOutput");
                    output.push_str(&payload.data);
                    false
                }
                FrameKind::TerminalExit => true,
                FrameKind::TerminalError => panic!("trusted setup emitted TerminalError"),
                _ => false,
            }
        })
        .await
        .parse_payload()
        .expect("parse setup TerminalExit");
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
    let native: BackendConfigSnapshotsPayload =
        next_frame_matching_on(&mut fixture.client, "post-setup refresh", |env| {
            match env.kind {
                FrameKind::BackendSetup => {
                    refresh_order.push(FrameKind::BackendSetup);
                    refreshed_setup = Some(
                        env.parse_payload::<BackendSetupPayload>()
                            .expect("parse refreshed BackendSetup"),
                    );
                    false
                }
                FrameKind::SessionSchemas => {
                    refresh_order.push(FrameKind::SessionSchemas);
                    false
                }
                FrameKind::BackendConfigSnapshots => {
                    refresh_order.push(FrameKind::BackendConfigSnapshots);
                    true
                }
                _ => false,
            }
        })
        .await
        .parse_payload()
        .expect("parse refreshed BackendConfigSnapshots");
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
    assert_eq!(
        tycode_native_snapshot(&native).status,
        BackendConfigSnapshotStatus::Ready
    );
    assert!(
        !staged_path.exists(),
        "trusted setup script must be removed after terminal exit and refresh"
    );
}

fn replace_op(path: &str, value: serde_json::Value, expected: serde_json::Value) -> SettingOp {
    SettingOp::Replace {
        path: path.to_owned(),
        value,
        expected: SettingExpectation::Value { value: expected },
    }
}

async fn send_settings_write(client: &mut client::Connection, write_id: &str, ops: Vec<SettingOp>) {
    client
        .settings_write(SettingsWritePayload {
            write_id: SettingsWriteId(write_id.to_owned()),
            ops,
        })
        .await
        .unwrap_or_else(|err| panic!("send settings_write {write_id}: {err:?}"));
}

/// Waits for the requester-scoped result of `write_id`, skipping unrelated
/// frames (fanouts, refreshes) but never a foreign write's result.
async fn expect_settings_write_result(
    client: &mut client::Connection,
    write_id: &str,
    context: &str,
) -> SettingsWriteResultPayload {
    let mut result = None;
    next_frame_matching_on(client, context, |env| {
        if env.kind != FrameKind::SettingsWriteResult {
            return false;
        }
        let payload: SettingsWriteResultPayload = env
            .parse_payload()
            .unwrap_or_else(|err| panic!("parse SettingsWriteResult for {context}: {err}"));
        assert_eq!(
            payload.write_id.0, write_id,
            "a SettingsWriteResult for a write this connection never issued is a \
             requester-scoping violation ({context})"
        );
        result = Some(payload);
        true
    })
    .await;
    result.expect("matched settings write result")
}

async fn expect_settings_action_outcome(
    client: &mut client::Connection,
    write_id: &SettingsWriteId,
    context: &str,
) -> (BackendConfigSnapshotsPayload, SettingsWriteResultPayload) {
    let mut snapshots = None;
    let mut result = None;
    next_frame_matching_on(client, context, |env| {
        match env.kind {
            FrameKind::BackendConfigSnapshots => {
                snapshots = Some(
                    env.parse_payload::<BackendConfigSnapshotsPayload>()
                        .expect("parse action BackendConfigSnapshots"),
                );
            }
            FrameKind::SettingsWriteResult => {
                let payload = env
                    .parse_payload::<SettingsWriteResultPayload>()
                    .expect("parse action SettingsWriteResult");
                assert_eq!(&payload.write_id, write_id);
                result = Some(payload);
            }
            _ => {}
        }
        snapshots.is_some() && result.is_some()
    })
    .await;
    (
        snapshots.expect("action snapshots"),
        result.expect("action result"),
    )
}

/// The next `HostSettings` fanout frame, parsed.
async fn expect_host_settings_frame(
    client: &mut client::Connection,
    context: &str,
) -> HostSettingsPayload {
    next_frame_matching_on(client, context, |env| env.kind == FrameKind::HostSettings)
        .await
        .parse_payload()
        .unwrap_or_else(|err| panic!("parse HostSettings for {context}: {err}"))
}

/// Ambient host-stream frames a settings apply may legitimately fan out to
/// every subscriber, for strict waits that must reject a leaked
/// `SettingsWriteResult`.
fn settings_apply_noise() -> Vec<FrameKind> {
    routine_control_plane_noise_plus(&[
        FrameKind::MobileAccessState,
        FrameKind::VoiceCapabilities,
        FrameKind::BackendConfigSnapshots,
        FrameKind::BackendConfigSchemas,
        FrameKind::SessionList,
        FrameKind::TaskTokenUsage,
        FrameKind::AgentsViewPreferencesNotify,
    ])
}

#[tokio::test(start_paused = true)]
async fn settings_write_scalar_applies_advances_etag_and_fans_out() {
    let mut fixture = Fixture::new().await;
    let (mut observer, observer_bootstrap) = fixture.connect_with_bootstrap().await;

    let initial_etag = fixture.bootstrap.settings_etag.clone();
    assert!(
        !initial_etag.is_empty(),
        "bootstrap must carry the settings etag"
    );
    assert_eq!(
        observer_bootstrap.settings_etag, initial_etag,
        "every bootstrap of the same document must carry the same etag"
    );
    assert!(
        fixture
            .bootstrap
            .settings_schema
            .get("properties")
            .and_then(|properties| properties.get("supervisor"))
            .is_some(),
        "bootstrap must carry the host settings JSON Schema (missing supervisor property): {}",
        fixture.bootstrap.settings_schema
    );
    assert!(
        fixture.bootstrap.configured_secrets.is_empty(),
        "no secret-bearing host settings exist yet"
    );
    assert!(!fixture.bootstrap.settings.supervisor.enabled);

    send_settings_write(
        &mut fixture.client,
        "w-scalar",
        vec![replace_op(
            "/supervisor/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        )],
    )
    .await;

    let fanout = expect_host_settings_frame(&mut fixture.client, "scalar write fanout").await;
    assert!(fanout.settings.supervisor.enabled);
    assert!(!fanout.etag.is_empty());
    assert_ne!(
        fanout.etag, initial_etag,
        "an applied write must advance the etag"
    );

    let result =
        expect_settings_write_result(&mut fixture.client, "w-scalar", "scalar write result").await;
    assert!(result.applied, "field_errors: {:?}", result.field_errors);
    assert!(result.field_errors.is_empty());
    assert_eq!(
        result.current_etag, fanout.etag,
        "draft-clear rule: the result's etag must equal the broadcast snapshot's etag"
    );

    // The non-requesting subscriber sees the same fanout with the same etag,
    // and never the requester-scoped result (strict wait: a leaked
    // SettingsWriteResult panics).
    let observed: HostSettingsPayload = next_frame_matching_strict_on(
        &mut observer,
        "observer fanout for scalar write",
        &settings_apply_noise(),
        |env| env.kind == FrameKind::HostSettings,
    )
    .await
    .parse_payload()
    .expect("parse observer HostSettings");
    assert!(observed.settings.supervisor.enabled);
    assert_eq!(observed.etag, result.current_etag);
}

#[tokio::test(start_paused = true)]
async fn secret_settings_publish_tokens_and_reject_stale_replacement() {
    let mut fixture = Fixture::new().await;
    let mut second = fixture.connect().await;
    let path = "/mobile_broker_auth/password";

    fixture
        .client
        .settings_write(SettingsWritePayload {
            write_id: SettingsWriteId("secret-a".to_owned()),
            ops: vec![SettingOp::Replace {
                path: path.to_owned(),
                value: serde_json::json!("first-secret"),
                expected: SettingExpectation::Absent,
            }],
        })
        .await
        .expect("configure secret from absent");
    let first = expect_host_settings_frame(&mut fixture.client, "secret A fanout").await;
    assert!(
        serde_json::to_value(&first.settings)
            .expect("serialize redacted host settings")
            .pointer(path)
            .is_none(),
        "secret values must be absent from HostSettings"
    );
    let first_secret = first
        .configured_secrets
        .iter()
        .find(|secret| secret.pointer == path)
        .expect("configured secret token after absent-to-A")
        .clone();
    assert!(
        expect_settings_write_result(&mut fixture.client, "secret-a", "secret A result")
            .await
            .applied
    );
    let second_first: HostSettingsPayload =
        next_frame_matching_on(&mut second, "second client secret A fanout", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await
        .parse_payload()
        .expect("parse second client HostSettings");
    assert_eq!(second_first.configured_secrets, first.configured_secrets);

    fixture
        .client
        .settings_write(SettingsWritePayload {
            write_id: SettingsWriteId("secret-b".to_owned()),
            ops: vec![SettingOp::Replace {
                path: path.to_owned(),
                value: serde_json::json!("second-secret"),
                expected: SettingExpectation::Version {
                    token: first_secret.token.clone(),
                },
            }],
        })
        .await
        .expect("replace secret A with B");
    let second_value = expect_host_settings_frame(&mut fixture.client, "secret B fanout").await;
    let second_secret = second_value
        .configured_secrets
        .iter()
        .find(|secret| secret.pointer == path)
        .expect("configured secret token after A-to-B");
    assert_ne!(second_secret.token, first_secret.token);
    assert_ne!(second_value.etag, first.etag);
    assert!(
        expect_settings_write_result(&mut fixture.client, "secret-b", "secret B result")
            .await
            .applied
    );

    second
        .settings_write(SettingsWritePayload {
            write_id: SettingsWriteId("secret-stale".to_owned()),
            ops: vec![SettingOp::Replace {
                path: path.to_owned(),
                value: serde_json::json!("stale-secret"),
                expected: SettingExpectation::Version {
                    token: first_secret.token,
                },
            }],
        })
        .await
        .expect("submit stale secret replacement");
    let stale = expect_settings_write_result(
        &mut second,
        "secret-stale",
        "stale secret replacement result",
    )
    .await;
    assert!(!stale.applied);
    assert_eq!(stale.field_errors[0].pointer, path);
    assert_eq!(stale.field_errors[0].code, SettingsErrorCode::Conflict);
    assert_eq!(stale.current_etag, second_value.etag);

    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_eq!(bootstrap.settings_etag, second_value.etag);
    assert_eq!(
        bootstrap.configured_secrets,
        second_value.configured_secrets
    );
    assert!(
        serde_json::to_value(&bootstrap.settings)
            .expect("serialize redacted bootstrap settings")
            .pointer(path)
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn settings_write_cas_conflict_applies_nothing() {
    let mut fixture = Fixture::new().await;
    let mut second = fixture.connect().await;

    send_settings_write(
        &mut fixture.client,
        "w-first",
        vec![replace_op(
            "/supervisor/max_kicks_per_task",
            serde_json::json!(5),
            serde_json::json!(3),
        )],
    )
    .await;
    let first_fanout = expect_host_settings_frame(&mut fixture.client, "first writer fanout").await;
    assert_eq!(first_fanout.settings.supervisor.max_kicks_per_task, 5);
    let first_result =
        expect_settings_write_result(&mut fixture.client, "w-first", "first writer result").await;
    assert!(first_result.applied);

    // The second client saw the same fanout but submits with the stale
    // expectation it started from.
    let second_fanout = expect_host_settings_frame(&mut second, "second client fanout").await;
    assert_eq!(second_fanout.settings.supervisor.max_kicks_per_task, 5);
    send_settings_write(
        &mut second,
        "w-stale",
        vec![replace_op(
            "/supervisor/max_kicks_per_task",
            serde_json::json!(4),
            serde_json::json!(3),
        )],
    )
    .await;
    let stale_result =
        expect_settings_write_result(&mut second, "w-stale", "stale writer result").await;
    assert!(!stale_result.applied, "a stale CAS write must not apply");
    assert_eq!(
        stale_result.field_errors.len(),
        1,
        "{:?}",
        stale_result.field_errors
    );
    let error = &stale_result.field_errors[0];
    assert_eq!(error.pointer, "/supervisor/max_kicks_per_task");
    assert_eq!(error.code, SettingsErrorCode::Conflict);
    assert_eq!(
        stale_result.current_etag, first_result.current_etag,
        "a rejected write must report the unchanged current etag"
    );

    // Nothing applied: the next fanout both clients see still carries the
    // first writer's value. (If the stale write had applied, the second
    // client's next HostSettings frame would carry 4.)
    send_settings_write(
        &mut fixture.client,
        "w-after",
        vec![replace_op(
            "/supervisor/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        )],
    )
    .await;
    let after = expect_host_settings_frame(&mut second, "fanout after rejected write").await;
    assert_eq!(
        after.settings.supervisor.max_kicks_per_task, 5,
        "the rejected write must not have clobbered the stored value"
    );
    assert!(after.settings.supervisor.enabled);
}

#[tokio::test(start_paused = true)]
async fn settings_write_overlapping_ops_are_rejected_whole() {
    let mut fixture = Fixture::new().await;
    let voice_doc = serde_json::to_value(&fixture.bootstrap.settings.voice)
        .expect("serialize current voice settings");

    send_settings_write(
        &mut fixture.client,
        "w-overlap",
        vec![
            replace_op("/voice", serde_json::json!({"enabled": true}), voice_doc),
            replace_op(
                "/voice/enabled",
                serde_json::json!(true),
                serde_json::json!(false),
            ),
        ],
    )
    .await;
    let result =
        expect_settings_write_result(&mut fixture.client, "w-overlap", "overlap result").await;
    assert!(!result.applied);
    let pointers: Vec<&str> = result
        .field_errors
        .iter()
        .map(|error| error.pointer.as_str())
        .collect();
    assert!(
        pointers.contains(&"/voice") && pointers.contains(&"/voice/enabled"),
        "both overlapping pointers must be named: {:?}",
        result.field_errors
    );
    assert!(
        result
            .field_errors
            .iter()
            .all(|error| error.code == SettingsErrorCode::OverlapRejected),
        "{:?}",
        result.field_errors
    );

    // Nothing applied: a fresh subscriber's bootstrap still carries the
    // untouched document under the same etag the result reported.
    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(!bootstrap.settings.voice.enabled);
    assert_eq!(bootstrap.settings_etag, result.current_etag);
}

#[tokio::test(start_paused = true)]
async fn settings_write_multi_op_is_all_or_nothing() {
    let mut fixture = Fixture::new().await;
    let initial_etag = fixture.bootstrap.settings_etag.clone();

    send_settings_write(
        &mut fixture.client,
        "w-multi",
        vec![
            replace_op(
                "/supervisor/enabled",
                serde_json::json!(true),
                serde_json::json!(false),
            ),
            replace_op(
                "/supervisor/retry_attempts",
                serde_json::json!(99),
                serde_json::json!(1),
            ),
        ],
    )
    .await;
    let result =
        expect_settings_write_result(&mut fixture.client, "w-multi", "multi-op result").await;
    assert!(
        !result.applied,
        "one invalid op must reject the whole write"
    );
    assert!(
        result
            .field_errors
            .iter()
            .any(|error| error.code == SettingsErrorCode::Invalid),
        "{:?}",
        result.field_errors
    );
    assert_eq!(result.current_etag, initial_etag);

    // Neither op applied — including the individually-valid first one.
    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(!bootstrap.settings.supervisor.enabled);
    assert_eq!(bootstrap.settings.supervisor.retry_attempts, 1);
    assert_eq!(bootstrap.settings_etag, result.current_etag);
}

#[tokio::test(start_paused = true)]
async fn settings_write_unknown_pointer_is_rejected() {
    let mut fixture = Fixture::new().await;

    send_settings_write(
        &mut fixture.client,
        "w-unknown",
        vec![
            replace_op(
                "/no_such_setting",
                serde_json::json!(true),
                serde_json::json!(null),
            ),
            replace_op(
                "/supervisor/nonexistent",
                serde_json::json!(1),
                serde_json::json!(null),
            ),
        ],
    )
    .await;
    let result =
        expect_settings_write_result(&mut fixture.client, "w-unknown", "unknown-path result").await;
    assert!(!result.applied);
    assert_eq!(result.field_errors.len(), 2, "{:?}", result.field_errors);
    for (error, pointer) in result
        .field_errors
        .iter()
        .zip(["/no_such_setting", "/supervisor/nonexistent"])
    {
        assert_eq!(error.pointer, pointer);
        assert_eq!(error.code, SettingsErrorCode::UnknownPath);
    }
}

/// Named side-effect requirement (a): disabling a Hermes provider through
/// the generic write path must re-probe the Hermes session schema, exactly
/// like the typed path does. The probe counter is the sim-observable form of
/// that re-probe (the fixture's static schema does not change shape, so no
/// differing SessionSchemas frame exists to wait for); dropping the coupling
/// from the new path leaves the counter flat and fails this test.
#[tokio::test(start_paused = true)]
async fn settings_write_hermes_provider_disable_reprobes_session_schema() {
    let mut fixture = Fixture::new().await;

    /// The Hermes model option values a `SessionSchemas` frame publishes,
    /// when its Hermes entry is Ready with a model Select.
    fn hermes_model_options(payload: &SessionSchemasPayload) -> Option<Vec<String>> {
        let schema = payload
            .schemas
            .iter()
            .find(|entry| entry.backend_kind() == BackendKind::Hermes)?
            .ready_schema()?;
        let field = schema.fields.iter().find(|field| field.key == "model")?;
        match &field.field_type {
            protocol::SessionSettingFieldType::Select { options, .. } => {
                Some(options.iter().map(|option| option.value.clone()).collect())
            }
            _ => None,
        }
    }

    /// Collects, in any order (the result rides the control lane and may
    /// overtake bulk-lane schema frames), the write's fanout, its result,
    /// and a `SessionSchemas` frame whose Hermes model options satisfy
    /// `schema_matches` — the user-visible consequence of the re-probe.
    async fn collect_write_outcome(
        client: &mut client::Connection,
        write_id: &str,
        context: &str,
        mut schema_matches: impl FnMut(&[String]) -> bool,
    ) -> (HostSettingsPayload, SettingsWriteResultPayload) {
        let mut fanout: Option<HostSettingsPayload> = None;
        let mut result: Option<SettingsWriteResultPayload> = None;
        let mut saw_schema = false;
        while !(fanout.is_some() && result.is_some() && saw_schema) {
            next_frame_matching_on(client, context, |env| match env.kind {
                FrameKind::HostSettings if fanout.is_none() => {
                    fanout = Some(env.parse_payload().expect("parse HostSettings"));
                    true
                }
                FrameKind::SettingsWriteResult => {
                    let payload: SettingsWriteResultPayload =
                        env.parse_payload().expect("parse SettingsWriteResult");
                    assert_eq!(payload.write_id.0, write_id);
                    result = Some(payload);
                    true
                }
                FrameKind::SessionSchemas => {
                    let payload: SessionSchemasPayload =
                        env.parse_payload().expect("parse SessionSchemas");
                    let matched = hermes_model_options(&payload)
                        .is_some_and(|options| schema_matches(&options));
                    saw_schema |= matched;
                    matched
                }
                _ => false,
            })
            .await;
        }
        (
            fanout.expect("collected fanout"),
            result.expect("collected result"),
        )
    }

    // Enabling Hermes publishes its session schema with the full mock model
    // catalog: both providers offered.
    send_settings_write(
        &mut fixture.client,
        "w-enable-hermes",
        vec![replace_op(
            "/enabled_backends",
            serde_json::json!(["hermes"]),
            serde_json::json!([]),
        )],
    )
    .await;
    let (fanout, enable_result) = collect_write_outcome(
        &mut fixture.client,
        "w-enable-hermes",
        "hermes enable fanout + schema + result",
        |options| {
            options.iter().any(|value| value.contains("mock-openai"))
                && options.iter().any(|value| value.contains("mock-anthropic"))
        },
    )
    .await;
    assert_eq!(fanout.settings.enabled_backends, vec![BackendKind::Hermes]);
    assert!(enable_result.applied, "{:?}", enable_result.field_errors);

    let probes_before = fixture
        .host_for_test()
        .session_schema_probe_count_for_test()
        .await;

    // Disabling a provider must re-probe and PUBLISH the filtered schema:
    // the disabled provider's model option disappears from the
    // SessionSchemas frame every client receives, while other providers
    // survive. Dropping the coupling means no such frame ever arrives.
    send_settings_write(
        &mut fixture.client,
        "w-disable-provider",
        vec![replace_op(
            "/hermes_disabled_providers/default",
            serde_json::json!(["mock-openai"]),
            serde_json::json!(null),
        )],
    )
    .await;
    let (fanout, result) = collect_write_outcome(
        &mut fixture.client,
        "w-disable-provider",
        "provider disable fanout + filtered schema + result",
        |options| {
            !options.iter().any(|value| value.contains("mock-openai"))
                && options.iter().any(|value| value.contains("mock-anthropic"))
        },
    )
    .await;
    assert_eq!(
        fanout
            .settings
            .hermes_disabled_providers
            .get("default")
            .map(Vec::as_slice),
        Some(["mock-openai".to_owned()].as_slice())
    );
    assert!(result.applied, "{:?}", result.field_errors);

    // Supplemental diagnostics only: the decisive oracle above is the
    // published filtered schema, not this counter.
    let probes_after = fixture
        .host_for_test()
        .session_schema_probe_count_for_test()
        .await;
    assert!(
        probes_after > probes_before,
        "disabling a Hermes provider over the generic write path must re-probe the Hermes \
         session schema (probes before: {probes_before}, after: {probes_after})"
    );
}

#[tokio::test(start_paused = true)]
async fn settings_write_keyed_launch_profiles_preserve_disjoint_edits() {
    let mut fixture = Fixture::new().await;
    let mut second = fixture.connect().await;

    for (client, write_id, profile_id) in [
        (&mut fixture.client, "w-profile-a", "profile-a"),
        (&mut second, "w-profile-b", "profile-b"),
    ] {
        client
            .settings_write(SettingsWritePayload {
                write_id: SettingsWriteId(write_id.to_owned()),
                ops: vec![SettingOp::Replace {
                    path: format!("/launch_profiles/{profile_id}"),
                    value: serde_json::json!({
                        "id": profile_id,
                        "label": profile_id,
                        "backend_kind": "claude",
                    }),
                    expected: SettingExpectation::Absent,
                }],
            })
            .await
            .expect("send profile settings_write");
        let result = expect_settings_write_result(client, write_id, "profile apply").await;
        assert!(result.applied, "{:?}", result.field_errors);
    }

    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_eq!(bootstrap.settings.launch_profiles.len(), 2);
    assert!(
        bootstrap
            .settings
            .launch_profiles
            .contains_key(&protocol::LaunchProfileId("profile-a".to_owned()))
    );
    assert!(
        bootstrap
            .settings
            .launch_profiles
            .contains_key(&protocol::LaunchProfileId("profile-b".to_owned()))
    );
}

/// The settings-apply lifecycle (CAS read, commit, propagation, fanout,
/// refreshes, result) is serialized across writers: while one write's slow
/// post-commit refresh window is open, a second write must not commit or
/// fan out. Otherwise subscribers could finish on a superseded snapshot and
/// the first writer's `current_etag` would be stale — breaking the
/// draft-clear correlation. The fake Tycode binary's startup delay holds
/// the first write's refresh window open deterministically while the
/// second write arrives.
#[tokio::test]
async fn settings_write_lifecycle_is_serialized_across_writers() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture = Fixture::new_with_real_backend_probe_for_enabled_backends(Vec::new()).await;
    let mut second = fixture.connect().await;
    // Applied after fixture startup so only the write-triggered refresh
    // probes are slowed.
    fake.set_behavior(serde_json::json!({ "startup_delay_seconds": 1.0 }));

    // W1: opens a multi-second refresh window (real fake-Tycode probes).
    send_settings_write(
        &mut fixture.client,
        "w-serial-tycode",
        vec![replace_op(
            "/enabled_backends",
            serde_json::json!(["tycode"]),
            serde_json::json!([]),
        )],
    )
    .await;
    // W2 arrives while W1's window is open.
    tokio::time::sleep(Duration::from_millis(300)).await;
    send_settings_write(
        &mut second,
        "w-serial-scalar",
        vec![replace_op(
            "/supervisor/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        )],
    )
    .await;

    // W1's issuer: exactly ONE fanout (W1's own document) may precede W1's
    // result, and the result's etag must equal that fanout's etag — the
    // draft-clear correlation the serialization exists to protect. If W2
    // were allowed to commit inside W1's window, its fanout would arrive
    // here first and this count would be 2.
    let mut pre_result_fanouts: Vec<HostSettingsPayload> = Vec::new();
    let mut w1_result: Option<SettingsWriteResultPayload> = None;
    while w1_result.is_none() {
        next_frame_matching_on(
            &mut fixture.client,
            "W1 fanout/result lifecycle",
            |env| match env.kind {
                FrameKind::HostSettings => {
                    pre_result_fanouts.push(env.parse_payload().expect("parse HostSettings"));
                    true
                }
                FrameKind::SettingsWriteResult => {
                    let payload: SettingsWriteResultPayload =
                        env.parse_payload().expect("parse SettingsWriteResult");
                    assert_eq!(
                        payload.write_id.0, "w-serial-tycode",
                        "a foreign write's result on this connection is a requester-scoping \
                         violation"
                    );
                    w1_result = Some(payload);
                    true
                }
                _ => false,
            },
        )
        .await;
    }
    let w1_result = w1_result.expect("W1 result");
    assert!(w1_result.applied, "{:?}", w1_result.field_errors);
    assert_eq!(
        pre_result_fanouts.len(),
        1,
        "no other settings write may fan out inside W1's apply window"
    );
    assert_eq!(
        pre_result_fanouts[0].settings.enabled_backends,
        vec![BackendKind::Tycode]
    );
    assert!(!pre_result_fanouts[0].settings.supervisor.enabled);
    assert_eq!(
        w1_result.current_etag, pre_result_fanouts[0].etag,
        "W1's result must report the etag of the snapshot that was current when it was emitted"
    );

    // After W1's lifecycle completes, W2's fanout follows on the same
    // connection.
    let w2_fanout_on_first =
        expect_host_settings_frame(&mut fixture.client, "W2 fanout after W1 completes").await;
    assert!(w2_fanout_on_first.settings.supervisor.enabled);
    assert_ne!(w2_fanout_on_first.etag, w1_result.current_etag);

    // W2's issuer observes the same serialized order: W1's snapshot, then
    // W2's snapshot, then W2's result carrying W2's etag.
    let first_on_second =
        expect_host_settings_frame(&mut second, "W1 fanout on second client").await;
    assert_eq!(
        first_on_second.settings.enabled_backends,
        vec![BackendKind::Tycode]
    );
    assert!(!first_on_second.settings.supervisor.enabled);
    let second_on_second = expect_host_settings_frame(&mut second, "W2 fanout on its issuer").await;
    assert!(second_on_second.settings.supervisor.enabled);
    let w2_result = expect_settings_write_result(&mut second, "w-serial-scalar", "W2 result").await;
    assert!(w2_result.applied, "{:?}", w2_result.field_errors);
    assert_eq!(w2_result.current_etag, second_on_second.etag);
    assert_eq!(w2_result.current_etag, w2_fanout_on_first.etag);

    // Durable state: a fresh bootstrap carries both writes under W2's etag.
    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        bootstrap.settings.enabled_backends,
        vec![BackendKind::Tycode]
    );
    assert!(bootstrap.settings.supervisor.enabled);
    assert_eq!(bootstrap.settings_etag, w2_result.current_etag);
}

#[tokio::test]
async fn native_save_serializes_with_generic_settings_write() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let fake = write_fake_tycode_binary(temp_home.path());
    write_shared_tycode_settings(
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
        expect_backend_config_snapshots(&mut fixture.client, "initial Tycode settings").await;
    let initial_doc = tycode_native_snapshot(&initial)
        .settings
        .clone()
        .expect("initial Tycode settings document");
    let base_settings = initial_doc["profiles"][0]["settings"].clone();
    let mut edited_settings = base_settings.clone();
    edited_settings
        .as_object_mut()
        .expect("Tycode settings object")
        .insert("model_quality".to_owned(), serde_json::json!("low"));
    let native_save = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "default",
            "settings_path": initial_doc["profiles"][0]["settings_path"],
            "settings": edited_settings,
            "base_settings": base_settings,
        }],
    });

    let persist_gate = temp_home.path().join("release-native-persist");
    fake.set_behavior(serde_json::json!({
        "persist_gate_path": persist_gate.to_string_lossy(),
    }));
    let mut second = fixture.connect().await;
    fixture
        .client
        .backend_native_settings_write(BackendKind::Tycode, native_save)
        .await
        .expect("start native save");
    let gate_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !fake
        .events()
        .iter()
        .any(|event| event["type"] == "persist_gate_entered")
    {
        assert!(
            tokio::time::Instant::now() < gate_deadline,
            "native save never reached the persistence gate"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send_settings_write(
        &mut second,
        "w-during-native-save",
        vec![replace_op(
            "/supervisor/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        )],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&persist_gate, b"release").expect("release native persistence gate");

    let mut saw_native_refresh = false;
    let fanout: HostSettingsPayload = next_frame_matching_on(
        &mut second,
        "native refresh before generic fanout",
        |env| match env.kind {
            FrameKind::BackendConfigSnapshots => {
                let payload: BackendConfigSnapshotsPayload = env
                    .parse_payload()
                    .expect("parse native refresh during collision");
                if let Some(settings) = tycode_native_snapshot(&payload).settings.as_ref() {
                    saw_native_refresh =
                        settings["profiles"][0]["settings"]["model_quality"] == "low";
                }
                false
            }
            FrameKind::HostSettings => {
                assert!(
                    saw_native_refresh,
                    "a generic settings write must wait for the in-flight native save and its refresh"
                );
                true
            }
            _ => false,
        },
    )
    .await
    .parse_payload()
    .expect("parse generic HostSettings fanout");
    assert!(fanout.settings.supervisor.enabled);
    let result = expect_settings_write_result(
        &mut second,
        "w-during-native-save",
        "generic result after native save",
    )
    .await;
    assert!(result.applied, "{:?}", result.field_errors);
    assert_eq!(result.current_etag, fanout.etag);

    let persisted = std::fs::read_to_string(temp_home.path().join(".tycode/settings.toml"))
        .expect("read Tycode settings after serialized writes");
    assert!(persisted.contains("model_quality = \"low\""), "{persisted}");
    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(bootstrap.settings.supervisor.enabled);
    assert_eq!(bootstrap.settings_etag, result.current_etag);
}

/// Named side-effect requirement (b): an EnabledBackends change through the
/// generic write path must refresh session schemas and backend config
/// snapshots. Dropping either refresh from the new path means the expected
/// frame never arrives and the wait times out.
#[tokio::test]
async fn settings_write_enabled_backends_refreshes_schemas_and_config_snapshots() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    write_fake_tycode_binary(temp_home.path());
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture = Fixture::new_with_real_backend_probe_for_enabled_backends(Vec::new()).await;
    assert!(fixture.bootstrap.settings.enabled_backends.is_empty());

    send_settings_write(
        &mut fixture.client,
        "w-enable-tycode",
        vec![replace_op(
            "/enabled_backends",
            serde_json::json!(["tycode"]),
            serde_json::json!([]),
        )],
    )
    .await;

    let fanout = expect_host_settings_frame(&mut fixture.client, "tycode enable fanout").await;
    assert_eq!(fanout.settings.enabled_backends, vec![BackendKind::Tycode]);

    // The write must produce all three of: a SessionSchemas refresh carrying
    // Tycode, a BackendConfigSnapshots refresh carrying the Tycode native
    // probe, and the requester-scoped result. The result rides the control
    // lane and may overtake the bulk-lane refresh frames, so collect them in
    // any order; a dropped refresh coupling means its frame never arrives
    // and the wait times out.
    let mut saw_tycode_schemas = false;
    let mut saw_tycode_snapshot = false;
    let mut result: Option<SettingsWriteResultPayload> = None;
    while !(saw_tycode_schemas && saw_tycode_snapshot && result.is_some()) {
        next_frame_matching_on(
            &mut fixture.client,
            "session-schema + config-snapshot refreshes and result after enabling Tycode",
            |env| match env.kind {
                FrameKind::SessionSchemas => {
                    let payload: SessionSchemasPayload = env
                        .parse_payload()
                        .expect("parse SessionSchemas after enabling Tycode");
                    let has_tycode = payload
                        .schemas
                        .iter()
                        .any(|entry| entry.backend_kind() == BackendKind::Tycode);
                    saw_tycode_schemas |= has_tycode;
                    has_tycode
                }
                FrameKind::BackendConfigSnapshots => {
                    let payload: BackendConfigSnapshotsPayload = env
                        .parse_payload()
                        .expect("parse BackendConfigSnapshots after enabling Tycode");
                    let has_tycode = payload
                        .native_settings
                        .iter()
                        .any(|snapshot| snapshot.backend_kind == BackendKind::Tycode);
                    saw_tycode_snapshot |= has_tycode;
                    has_tycode
                }
                FrameKind::SettingsWriteResult => {
                    let payload: SettingsWriteResultPayload = env
                        .parse_payload()
                        .expect("parse SettingsWriteResult after enabling Tycode");
                    assert_eq!(payload.write_id.0, "w-enable-tycode");
                    result = Some(payload);
                    true
                }
                _ => false,
            },
        )
        .await;
    }
    let result = result.expect("collected settings write result");
    assert!(result.applied, "{:?}", result.field_errors);
}

#[tokio::test(start_paused = true)]
async fn sequential_settings_writes_preserve_prior_fields() {
    let mut fixture = Fixture::new().await;

    send_settings_write(
        &mut fixture.client,
        "w-first",
        vec![replace_op(
            "/supervisor/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        )],
    )
    .await;
    let first_fanout = expect_host_settings_frame(&mut fixture.client, "first fanout").await;
    assert!(first_fanout.settings.supervisor.enabled);
    assert!(
        !first_fanout.etag.is_empty(),
        "the first fanout must carry an etag"
    );
    let first_result =
        expect_settings_write_result(&mut fixture.client, "w-first", "first result").await;
    assert!(first_result.applied, "{:?}", first_result.field_errors);

    send_settings_write(
        &mut fixture.client,
        "w-alongside",
        vec![replace_op(
            "/supervisor/auto_compact_on_success",
            serde_json::json!(true),
            serde_json::json!(false),
        )],
    )
    .await;
    let fanout = expect_host_settings_frame(&mut fixture.client, "generic write fanout").await;
    assert!(fanout.settings.supervisor.enabled);
    assert!(fanout.settings.supervisor.auto_compact_on_success);
    assert_ne!(fanout.etag, first_fanout.etag);
    let result =
        expect_settings_write_result(&mut fixture.client, "w-alongside", "alongside result").await;
    assert!(result.applied, "{:?}", result.field_errors);
    assert_eq!(result.current_etag, fanout.etag);
}
