use settings_model::HostBootstrapPayload;
use std::time::Duration;

use client::ClientConfig;
use protocol::{
    BackendAccessMode, BackendKind, CommandErrorCode, CommandErrorPayload, FrameKind,
    HostBrowseInitial, HostBrowseStartPayload, LaunchProfileCatalog, LaunchProfileCatalogPayload,
    LaunchProfileEntry, LaunchProfileId, LaunchProfileKind, NewAgentPayload,
    ProjectBootstrapPayload, ProjectRootPath, ReviewSummaryScope, SessionId, SessionListPageStatus,
    SessionListPayload, SessionSchemasPayload, SessionSettingValue, SessionSettingsValues,
    SettingsWriteResultPayload, SpawnAgentParams, SpawnAgentPayload, TerminalCreatePayload,
    TerminalLaunchTarget,
};
use server::backend::BackendSession;
use server::store::project::ProjectStore;
use server::store::session::SessionStore;
use settings_model::HostLaunchProfileConfig;

async fn connect_raw(host: server::HostHandle) -> client::Connection {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake");
        if let Err(err) = server::run_connection(conn, host).await {
            eprintln!("server connection failed: {err:?}");
        }
    });

    client::connect(&ClientConfig::current(), client_stream)
        .await
        .expect("client handshake")
}

async fn connect_mobile_raw(host: server::HostHandle) -> client::Connection {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake");
        if let Err(err) = server::run_mobile_connection(conn, host).await {
            eprintln!("server mobile connection failed: {err:?}");
        }
    });

    client::connect(&ClientConfig::current(), client_stream)
        .await
        .expect("mobile client handshake")
}

async fn next_env(client: &mut client::Connection, context: &str) -> protocol::Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn next_kind(
    client: &mut client::Connection,
    kind: FrameKind,
    context: &str,
) -> protocol::Envelope {
    loop {
        let env = next_env(client, context).await;
        if env.kind == kind {
            return env;
        }
    }
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        match tokio::time::timeout(deadline - now, client.next_event()).await {
            Err(_) | Ok(Ok(None)) => return,
            Ok(Ok(Some(env))) if env.kind == FrameKind::BackendCapacity => {}
            Ok(Ok(Some(env))) => panic!(
                "unexpected event before {context}: kind={} stream={}",
                env.kind, env.stream
            ),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

async fn expect_no_session_schemas(
    client: &mut client::Connection,
    duration: Duration,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        match tokio::time::timeout(deadline - now, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env))) if env.kind == FrameKind::SessionSchemas => {
                panic!(
                    "unexpected session_schemas before {context}: stream={} payload={}",
                    env.stream, env.payload
                );
            }
            Ok(Ok(Some(_))) => {}
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

fn spawn_host(dir: &tempfile::TempDir) -> server::HostHandle {
    server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host")
}

fn seed_session_store(path: &std::path::Path, count: u32) {
    let store = SessionStore::load(path.to_owned()).expect("load session store");
    for index in 0..count {
        store
            .upsert_backend_session(
                &BackendSession {
                    id: SessionId(format!("session-{index:04}")),
                    backend_kind: BackendKind::Claude,
                    workspace_roots: vec![format!("/workspace/{index}")],
                    title: Some(format!("Session {index:04}")),
                    token_count: Some(index as u64),
                    created_at_ms: Some(index as u64),
                    updated_at_ms: Some((count - index) as u64),
                    resumable: true,
                },
                None,
                None,
                None,
                None,
            )
            .expect("seed backend session");
    }
}

fn seed_session_store_with_children(path: &std::path::Path, root_count: u32, child_count: u32) {
    let store = SessionStore::load(path.to_owned()).expect("load session store");
    for index in 0..root_count {
        store
            .upsert_backend_session(
                &BackendSession {
                    id: SessionId(format!("root-session-{index:04}")),
                    backend_kind: BackendKind::Claude,
                    workspace_roots: vec![format!("/workspace/root/{index}")],
                    title: Some(format!("Root Session {index:04}")),
                    token_count: Some(index as u64),
                    created_at_ms: Some(index as u64),
                    updated_at_ms: Some((root_count - index) as u64),
                    resumable: true,
                },
                None,
                None,
                None,
                None,
            )
            .expect("seed root backend session");
    }

    let parent_id = SessionId("root-session-0000".to_owned());
    for index in 0..child_count {
        store
            .upsert_backend_session(
                &BackendSession {
                    id: SessionId(format!("child-session-{index:04}")),
                    backend_kind: BackendKind::Claude,
                    workspace_roots: vec![format!("/workspace/child/{index}")],
                    title: Some(format!("Child Session {index:04}")),
                    token_count: Some(index as u64),
                    created_at_ms: Some((root_count + index) as u64),
                    updated_at_ms: Some((root_count + child_count + index) as u64),
                    resumable: true,
                },
                Some(parent_id.clone()),
                None,
                None,
                None,
            )
            .expect("seed child backend session");
    }
}

fn write_enabled_backends_settings(path: &std::path::Path, backends: &[BackendKind]) {
    write_host_settings(path, backends, None);
}

fn write_host_settings(
    path: &std::path::Path,
    backends: &[BackendKind],
    default_backend: Option<BackendKind>,
) {
    write_host_settings_with_launch_profiles(path, backends, default_backend, Vec::new());
}

fn write_host_settings_with_launch_profiles(
    path: &std::path::Path,
    backends: &[BackendKind],
    default_backend: Option<BackendKind>,
    launch_profiles: Vec<HostLaunchProfileConfig>,
) {
    let settings = settings_model::HostSettings {
        enabled_backends: backends.to_vec(),
        default_backend,
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
        launch_profiles: launch_profiles
            .into_iter()
            .map(|profile| (profile.id.clone(), profile))
            .collect(),
        hermes_disabled_providers: Default::default(),
        voice: Default::default(),
    };
    let json = serde_json::json!({ "settings": settings });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&json).expect("serialize settings"),
    )
    .expect("write settings");
}

fn ready_launch_profile_ids(catalog: &LaunchProfileCatalog) -> Vec<String> {
    catalog
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LaunchProfileEntry::Ready { profile } => Some(profile.id.0.clone()),
            LaunchProfileEntry::Unavailable { .. } => None,
        })
        .collect()
}

fn launch_profile_entry<'a>(catalog: &'a LaunchProfileCatalog, id: &str) -> &'a LaunchProfileEntry {
    catalog
        .entries
        .iter()
        .find(|entry| entry.id().0 == id)
        .unwrap_or_else(|| panic!("missing launch profile {id} in {catalog:?}"))
}

fn hermes_claude_session_settings() -> SessionSettingsValues {
    let mut settings = SessionSettingsValues::default();
    settings.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String("high".to_owned()),
    );
    settings
        .0
        .insert("fast".to_owned(), SessionSettingValue::Bool(true));
    settings
}

fn hermes_claude_launch_profile() -> HostLaunchProfileConfig {
    HostLaunchProfileConfig {
        id: LaunchProfileId("hermes:claude".to_owned()),
        label: "Hermes: Claude".to_owned(),
        description: Some("Launch Hermes with an explicit Claude preset.".to_owned()),
        backend_kind: BackendKind::Hermes,
        session_settings: hermes_claude_session_settings(),
        acp: None,
    }
}

fn write_fake_codex_model_probe_program(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let binary = dir.path().join("fake-codex-model-probe.py");
    let counter = dir.path().join("model-list-count");
    let script = format!(
        r#"#!/usr/bin/env python3
import json
import os
import sys

COUNTER = {}

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    request = json.loads(line)
    request_id = request.get("id")
    method = request.get("method")
    if method == "initialize":
        send({{"jsonrpc": "2.0", "id": request_id, "result": {{}}}})
    elif method == "model/list":
        count = 0
        if os.path.exists(COUNTER):
            with open(COUNTER, "r", encoding="utf-8") as counter_file:
                count = int(counter_file.read())
        with open(COUNTER, "w", encoding="utf-8") as counter_file:
            counter_file.write(str(count + 1))
        if count == 0:
            send({{
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {{"data": [{{
                    "model": "gpt-5.5",
                    "isDefault": True,
                    "supportedReasoningEfforts": [
                        {{"reasoningEffort": "low"}},
                        {{"reasoningEffort": "high"}}
                    ]
                }}]}}
            }})
        elif count == 1:
            send({{
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {{"data": [{{
                    "model": "gpt-5.6",
                    "isDefault": True,
                    "supportedReasoningEfforts": [
                        {{"reasoningEffort": "low"}},
                        {{"reasoningEffort": "xhigh"}},
                        {{"reasoningEffort": "max"}}
                    ]
                }}]}}
            }})
        else:
            send({{
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {{"code": -32000, "message": "model metadata unavailable"}}
            }})
"#,
        serde_json::to_string(&counter.to_string_lossy()).expect("counter path JSON")
    );
    install_fake_program(&binary, &script);
    binary
}

fn install_fake_program(binary: &std::path::Path, script: &str) {
    std::fs::write(binary, script).expect("write fake Codex model probe");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(binary)
            .expect("fake Codex model probe metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(binary, permissions).expect("chmod fake Codex model probe");
    }
}

/// Longer than the 45s deadline Tyde used to put on every local Codex RPC, so
/// only a client that has no deadline at all can satisfy this fixture.
const CODEX_SLOWER_THAN_OLD_DEADLINE: Duration = Duration::from_secs(50);

/// A fake app-server that answers `initialize` only after a long stall.
///
/// Real ones do this: `initialize` opens four WAL SQLite databases under
/// `CODEX_HOME`, so a home directory on a slow or networked filesystem pushes
/// the handshake far past anything a constant could sensibly bound.
fn write_slow_fake_codex_model_probe_program(
    dir: &tempfile::TempDir,
    initialize_delay: Duration,
) -> std::path::PathBuf {
    let binary = dir.path().join("slow-fake-codex-model-probe.py");
    let script = format!(
        r#"#!/usr/bin/env python3
import json
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    request = json.loads(line)
    request_id = request.get("id")
    method = request.get("method")
    if method == "initialize":
        time.sleep({delay})
        send({{"jsonrpc": "2.0", "id": request_id, "result": {{}}}})
    elif method == "model/list":
        send({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"data": [{{
                "model": "gpt-5.6",
                "isDefault": True,
                "supportedReasoningEfforts": [{{"reasoningEffort": "low"}}]
            }}]}}
        }})
"#,
        delay = initialize_delay.as_secs_f64()
    );
    install_fake_program(&binary, &script);
    binary
}

/// A fake app-server that reads `initialize` and dies without answering it.
fn write_dying_fake_codex_model_probe_program(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let binary = dir.path().join("dying-fake-codex-model-probe.py");
    let script = r#"#!/usr/bin/env python3
import sys

sys.stdin.readline()
sys.exit(1)
"#;
    install_fake_program(&binary, script);
    binary
}

/// [`next_kind`] with a caller-chosen bound, for flows that outlast the 5s one.
async fn next_kind_within(
    client: &mut client::Connection,
    kind: FrameKind,
    within: Duration,
    context: &str,
) -> protocol::Envelope {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {context}");
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) if env.kind == kind => return env,
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        }
    }
}

fn spawn_host_with_codex_probe_program(
    dir: &tempfile::TempDir,
    settings_path: std::path::PathBuf,
    probe_program: &std::path::Path,
) -> server::HostHandle {
    server::spawn_host_with_mock_backend_and_runtime_config(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
        server::HostRuntimeConfig {
            codex_probe_program: Some(probe_program.to_string_lossy().into_owned()),
            skip_real_backend_probe: true,
            ..Default::default()
        },
    )
    .expect("spawn host")
}

/// Codex model discovery must wait out a slow app-server rather than give up on
/// it. Tyde used to cap every local RPC at 45s, which turned a healthy CLI doing
/// slow startup work into "Codex model discovery initialize failed: Codex
/// request timed out for method 'initialize'" — and then discarded the reply
/// when it did arrive, because the timeout had already dropped the pending slot.
#[tokio::test]
async fn codex_schema_waits_out_an_app_server_slower_than_the_old_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Codex]);
    let fake_codex =
        write_slow_fake_codex_model_probe_program(&dir, CODEX_SLOWER_THAN_OLD_DEADLINE);
    let host = spawn_host_with_codex_probe_program(&dir, settings_path, &fake_codex);
    let mut client = connect_raw(host).await;

    let schemas_env = next_kind_within(
        &mut client,
        FrameKind::SessionSchemas,
        CODEX_SLOWER_THAN_OLD_DEADLINE + Duration::from_secs(30),
        "Codex model schema from a slow app-server",
    )
    .await;
    let schemas: SessionSchemasPayload = schemas_env
        .parse_payload()
        .expect("slow Codex SessionSchemas");

    let [protocol::SessionSchemaEntry::Ready { schema }] = schemas.schemas.as_slice() else {
        panic!("slow Codex app-server must still produce a schema: {schemas:?}");
    };
    let model_field = schema
        .fields
        .iter()
        .find(|field| field.key == "model")
        .expect("slow Codex model field");
    let protocol::SessionSettingFieldType::Select { options, .. } = &model_field.field_type else {
        panic!("Codex model field should be a select");
    };
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["gpt-5.6"],
        "the reply that arrived after the old deadline must be the one used"
    );
}

/// The guarantee that lets the deadline go: a request ends when the app-server
/// answers *or* when it can no longer answer. Without the second half, dropping
/// the timeout would trade a spurious failure for a permanent hang.
#[tokio::test]
async fn codex_schema_reports_an_app_server_that_dies_without_answering() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Codex]);
    let fake_codex = write_dying_fake_codex_model_probe_program(&dir);
    let host = spawn_host_with_codex_probe_program(&dir, settings_path, &fake_codex);
    let mut client = connect_raw(host).await;

    // The 5s bound is the assertion: process death is observed directly, so it
    // must surface immediately rather than after any deadline.
    let schemas_env = next_kind_within(
        &mut client,
        FrameKind::SessionSchemas,
        Duration::from_secs(5),
        "Codex model schema from a dead app-server",
    )
    .await;
    let schemas: SessionSchemasPayload = schemas_env.parse_payload().expect("dead Codex schemas");

    assert!(
        matches!(
            schemas.schemas.as_slice(),
            [protocol::SessionSchemaEntry::Unavailable {
                backend_kind: BackendKind::Codex,
                message,
            }] if message.contains("exited before response")
        ),
        "a dead app-server must be reported as dead, not as slow: {schemas:?}"
    );
}

#[tokio::test]
async fn connection_emits_one_host_bootstrap_without_old_initial_spam() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = spawn_host(&dir);
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    assert_eq!(env.seq, 1, "Welcome consumes host seq 0");
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert!(bootstrap.sessions.is_empty());
    assert!(bootstrap.projects.is_empty());
    assert!(matches!(
        bootstrap.mobile_access.broker_status,
        protocol::MobileBrokerStatus::Disabled
    ));

    expect_no_event(
        &mut client,
        Duration::from_millis(100),
        "old initial replay spam",
    )
    .await;
}

#[tokio::test]
async fn mobile_bootstrap_pages_large_session_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    seed_session_store(&session_path, 300);
    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_mobile_raw(host).await;

    let env = next_env(&mut client, "mobile host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let serialized_len = serde_json::to_vec(&env)
        .expect("serialize mobile HostBootstrap envelope")
        .len();
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap.session_list.scope,
        protocol::SessionListScope::RootSessions
    );
    assert_eq!(bootstrap.session_list.total_count, 300);
    assert_eq!(
        bootstrap.sessions.len(),
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT as usize
    );
    assert!(
        serialized_len < 128 * 1024,
        "mobile HostBootstrap should stay bounded, got {serialized_len} bytes"
    );
    let next_cursor = match bootstrap.session_list.status {
        SessionListPageStatus::More { next_cursor } => next_cursor,
        SessionListPageStatus::Complete => panic!("large mobile bootstrap should be paged"),
    };
    assert_eq!(
        next_cursor.offset,
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT
    );

    client
        .list_sessions(protocol::ListSessionsPayload {
            scope: Some(protocol::SessionListScope::RootSessions),
            cursor: Some(next_cursor),
            limit: Some(protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT),
        })
        .await
        .expect("request second session page");
    let env = next_kind(
        &mut client,
        FrameKind::SessionList,
        "second mobile session page",
    )
    .await;
    let page: SessionListPayload = env.parse_payload().expect("parse SessionList");
    assert_eq!(
        page.page.cursor.offset,
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT
    );
    assert_eq!(page.page.scope, protocol::SessionListScope::RootSessions);
    assert_eq!(page.page.total_count, 300);
    assert_eq!(
        page.sessions.len(),
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT as usize
    );
    assert!(matches!(
        page.page.status,
        SessionListPageStatus::More { .. }
    ));
}

/// Read until a `SessionList` arrives, failing *immediately* on a rejected
/// `list_sessions` rather than looping past it.
///
/// `next_kind` would silently drop the `CommandError` this regression is about
/// and then time out, reporting nothing about why. Here the rejection is the
/// diagnostic.
async fn next_session_list(client: &mut client::Connection, context: &str) -> SessionListPayload {
    for _ in 0..16 {
        let env = next_env(client, context).await;
        match env.kind {
            FrameKind::SessionList => {
                return env.parse_payload().expect("parse SessionList");
            }
            FrameKind::CommandError => {
                let error: CommandErrorPayload = env.parse_payload().expect("parse CommandError");
                if error.request_kind == FrameKind::ListSessions {
                    panic!(
                        "{context} was rejected: {} failed with {:?}: {}",
                        error.operation, error.code, error.message
                    );
                }
            }
            _ => {}
        }
    }
    panic!("no SessionList arrived before {context} ran out of frames");
}

/// The reported failure. A desktop host replays its whole session list, so its
/// bootstrap page applied no bound. The client re-requests that view by
/// echoing the page descriptor it was given, and the host must accept it.
///
/// Before the fix the descriptor carried the host's own session count in place
/// of "no bound", and the echo came back as
/// `session list limit 129 exceeds maximum 128` — on every desktop host with
/// more than `MAX_SESSION_LIST_PAGE_LIMIT` sessions, with no user action
/// needed.
#[tokio::test]
async fn desktop_bootstrap_page_limit_survives_being_echoed_back() {
    let session_count = protocol::MAX_SESSION_LIST_PAGE_LIMIT + 1;
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    seed_session_store(&session_path, session_count);
    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "desktop host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap.session_list.scope,
        protocol::SessionListScope::AllSessions
    );
    assert_eq!(bootstrap.session_list.total_count, session_count);
    assert_eq!(bootstrap.sessions.len(), session_count as usize);
    assert_eq!(
        bootstrap.session_list.limit, None,
        "an unbounded replay must advertise no page size rather than its own count"
    );
    assert!(
        matches!(
            bootstrap.session_list.status,
            SessionListPageStatus::Complete
        ),
        "an unbounded page has nothing left to continue with"
    );

    client
        .list_sessions(protocol::ListSessionsPayload {
            scope: Some(bootstrap.session_list.scope),
            cursor: None,
            limit: bootstrap.session_list.limit,
        })
        .await
        .expect("re-request the advertised view");

    let page = next_session_list(&mut client, "echoed desktop session list").await;
    assert_eq!(page.page.limit, None);
    assert_eq!(page.page.total_count, session_count);
    assert_eq!(page.sessions.len(), session_count as usize);
    assert!(matches!(page.page.status, SessionListPageStatus::Complete));
}

/// Mobile is paged, and stays paged. Its descriptor names a real bound, that
/// bound round-trips, and omitting it falls back to the same subscriber
/// default rather than to "unbounded" — the request field means "use my
/// default", not "no limit".
#[tokio::test]
async fn mobile_session_page_limit_round_trips_and_defaults_when_omitted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    seed_session_store(&session_path, 300);
    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_mobile_raw(host).await;

    let env = next_env(&mut client, "mobile host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap.session_list.limit,
        Some(protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT)
    );
    let next_cursor = match bootstrap.session_list.status {
        SessionListPageStatus::More { next_cursor } => next_cursor,
        SessionListPageStatus::Complete => panic!("a 300-session mobile bootstrap must be paged"),
    };

    client
        .list_sessions(protocol::ListSessionsPayload {
            scope: Some(bootstrap.session_list.scope),
            cursor: Some(next_cursor),
            limit: bootstrap.session_list.limit,
        })
        .await
        .expect("echo the advertised mobile limit");
    let page = next_session_list(&mut client, "echoed mobile session page").await;
    assert_eq!(
        page.page.limit,
        Some(protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT)
    );
    assert_eq!(
        page.sessions.len(),
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT as usize
    );

    let next_cursor = match page.page.status {
        SessionListPageStatus::More { next_cursor } => next_cursor,
        SessionListPageStatus::Complete => panic!("300 sessions must leave more pages"),
    };
    client
        .list_sessions(protocol::ListSessionsPayload {
            scope: Some(bootstrap.session_list.scope),
            cursor: Some(next_cursor),
            limit: None,
        })
        .await
        .expect("request a page without naming a limit");
    let page = next_session_list(&mut client, "defaulted mobile session page").await;
    assert_eq!(
        page.page.limit,
        Some(protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT),
        "an omitted request limit resolves to the subscriber default, never to unbounded"
    );
    assert_eq!(
        page.sessions.len(),
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT as usize
    );
}

#[tokio::test]
async fn mobile_session_pages_use_stable_snapshot_when_sessions_reorder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    seed_session_store(&session_path, 130);
    let host = server::spawn_host_with_mock_backend(
        session_path.clone(),
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_mobile_raw(host).await;

    let env = next_env(&mut client, "mobile host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap
            .sessions
            .first()
            .map(|session| session.id.0.as_str()),
        Some("session-0000")
    );
    assert_eq!(
        bootstrap
            .sessions
            .last()
            .map(|session| session.id.0.as_str()),
        Some("session-0019")
    );
    let mut all_ids = bootstrap
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    let first_generation = bootstrap.session_list.cursor.generation;
    let mut next_cursor = match bootstrap.session_list.status {
        SessionListPageStatus::More { next_cursor } => next_cursor,
        SessionListPageStatus::Complete => panic!("large mobile bootstrap should be paged"),
    };

    let store = SessionStore::load(session_path).expect("reload session store");
    store
        .update(&SessionId("session-0100".to_owned()), |record| {
            record.updated_at_ms = 1_000_000;
        })
        .expect("reorder a later session between page requests");

    loop {
        client
            .list_sessions(protocol::ListSessionsPayload {
                scope: Some(protocol::SessionListScope::RootSessions),
                cursor: Some(next_cursor),
                limit: Some(protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT),
            })
            .await
            .expect("request next session page");
        let env = next_kind(&mut client, FrameKind::SessionList, "next session page").await;
        let page: SessionListPayload = env.parse_payload().expect("parse SessionList");
        assert_eq!(
            page.page.cursor.generation, first_generation,
            "continuation pages must come from the original snapshot"
        );
        if page.page.cursor.offset == protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT {
            assert_eq!(
                page.sessions.first().map(|session| session.id.0.as_str()),
                Some("session-0020"),
                "fresh offset paging would duplicate session-0019 and silently skip a later session"
            );
        }
        all_ids.extend(page.sessions.into_iter().map(|session| session.id));
        match page.page.status {
            SessionListPageStatus::More { next_cursor: next } => next_cursor = next,
            SessionListPageStatus::Complete => break,
        }
    }

    let unique_ids = all_ids.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(all_ids.len(), 130);
    assert_eq!(unique_ids.len(), 130);
    assert!(
        unique_ids.contains(&SessionId("session-0129".to_owned())),
        "stable snapshot paging must not silently truncate the old tail"
    );
}

#[tokio::test]
async fn mobile_session_lists_default_to_root_scope_and_can_request_all_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    seed_session_store_with_children(&session_path, 25, 5);
    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_mobile_raw(host).await;

    let env = next_env(&mut client, "mobile host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap.session_list.scope,
        protocol::SessionListScope::RootSessions
    );
    assert_eq!(bootstrap.session_list.total_count, 25);
    assert_eq!(
        bootstrap.sessions.len(),
        protocol::DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT as usize
    );
    assert!(
        bootstrap
            .sessions
            .iter()
            .all(|session| session.parent_id.is_none()),
        "mobile bootstrap must exclude child sessions by default"
    );
    let next_cursor = match bootstrap.session_list.status {
        SessionListPageStatus::More { next_cursor } => next_cursor,
        SessionListPageStatus::Complete => panic!("root session bootstrap should be paged"),
    };

    client
        .list_sessions(protocol::ListSessionsPayload {
            scope: Some(protocol::SessionListScope::RootSessions),
            cursor: Some(next_cursor),
            limit: None,
        })
        .await
        .expect("request root continuation page");
    let env = next_kind(
        &mut client,
        FrameKind::SessionList,
        "root continuation SessionList",
    )
    .await;
    let root_page: SessionListPayload = env.parse_payload().expect("parse root SessionList");
    assert_eq!(
        root_page.page.scope,
        protocol::SessionListScope::RootSessions
    );
    assert_eq!(root_page.page.total_count, 25);
    assert_eq!(root_page.sessions.len(), 5);
    assert!(
        root_page
            .sessions
            .iter()
            .all(|session| session.parent_id.is_none())
    );
    assert!(matches!(
        root_page.page.status,
        SessionListPageStatus::Complete
    ));

    client
        .list_sessions(protocol::ListSessionsPayload {
            scope: Some(protocol::SessionListScope::AllSessions),
            cursor: None,
            limit: Some(40),
        })
        .await
        .expect("request all session page");
    let env = next_kind(&mut client, FrameKind::SessionList, "all SessionList").await;
    let all_page: SessionListPayload = env.parse_payload().expect("parse all SessionList");
    assert_eq!(all_page.page.scope, protocol::SessionListScope::AllSessions);
    assert_eq!(all_page.page.total_count, 30);
    assert_eq!(all_page.sessions.len(), 30);
    assert!(
        all_page
            .sessions
            .iter()
            .any(|session| session.parent_id.is_some()),
        "explicit all-session scope should include child sessions"
    );
}

#[tokio::test]
async fn desktop_session_bootstrap_and_default_list_still_include_children() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    seed_session_store_with_children(&session_path, 2, 1);
    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "desktop host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap.session_list.scope,
        protocol::SessionListScope::AllSessions
    );
    assert_eq!(bootstrap.session_list.total_count, 3);
    assert_eq!(bootstrap.sessions.len(), 3);
    assert!(
        bootstrap
            .sessions
            .iter()
            .any(|session| session.parent_id.is_some()),
        "desktop bootstrap should keep the historical all-session behavior"
    );

    client
        .list_sessions(protocol::ListSessionsPayload::default())
        .await
        .expect("request desktop default session list");
    let env = next_kind(
        &mut client,
        FrameKind::SessionList,
        "desktop default SessionList",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse desktop SessionList");
    assert_eq!(list.page.scope, protocol::SessionListScope::AllSessions);
    assert_eq!(list.page.total_count, 3);
    assert_eq!(list.sessions.len(), 3);
    assert!(
        list.sessions
            .iter()
            .any(|session| session.parent_id.is_some()),
        "desktop default ListSessions should keep returning all sessions"
    );
}

#[tokio::test]
async fn host_bootstrap_includes_backend_config_schema_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Claude]);
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap.settings.enabled_backends,
        vec![BackendKind::Claude]
    );
    // No built-in backend publishes a typed deep-config schema anymore
    // (Hermes moved to backend-native settings), so the catalog ships empty
    // rather than advertising a schema no backend serves.
    assert!(
        bootstrap.backend_config_schemas.is_empty(),
        "unexpected deep-config schemas: {:?}",
        bootstrap.backend_config_schemas
    );
    assert!(bootstrap.backend_config_snapshots.is_empty());
}

#[tokio::test]
async fn explicit_hermes_launch_profile_is_unavailable_until_schema_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_host_settings_with_launch_profiles(
        &settings_path,
        &[BackendKind::Hermes],
        Some(BackendKind::Hermes),
        vec![hermes_claude_launch_profile()],
    );
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    match launch_profile_entry(&bootstrap.launch_profile_catalog, "hermes:claude") {
        LaunchProfileEntry::Unavailable { kind, message, .. } => {
            assert_eq!(*kind, LaunchProfileKind::Custom);
            assert!(
                message.contains("still loading"),
                "unexpected initial Hermes profile message: {message}"
            );
        }
        LaunchProfileEntry::Ready { profile } => {
            panic!("Hermes profile should wait for dynamic schema refresh: {profile:?}");
        }
    }
}

#[tokio::test]
async fn host_bootstrap_includes_launch_profile_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    let mut profile_settings = SessionSettingsValues::default();
    profile_settings.0.insert(
        "model".to_owned(),
        SessionSettingValue::String("haiku".to_owned()),
    );
    let profile = HostLaunchProfileConfig {
        id: LaunchProfileId("claude:haiku".to_owned()),
        label: "Claude Haiku".to_owned(),
        description: Some("Launch Claude with Haiku.".to_owned()),
        backend_kind: BackendKind::Claude,
        session_settings: profile_settings,
        acp: None,
    };
    write_host_settings_with_launch_profiles(
        &settings_path,
        &[BackendKind::Claude, BackendKind::Codex],
        Some(BackendKind::Claude),
        vec![profile.clone()],
    );
    let mut legacy: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&settings_path).expect("read keyed settings fixture"),
    )
    .expect("parse keyed settings fixture");
    legacy["settings"]["launch_profiles"] = serde_json::json!([profile]);
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&legacy).expect("serialize legacy launch profile array"),
    )
    .expect("write legacy launch profile array");
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path.clone(),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(
        bootstrap
            .launch_profile_catalog
            .default_profile_id
            .as_ref()
            .map(|id| id.0.as_str()),
        Some("claude:default")
    );
    assert_eq!(
        ready_launch_profile_ids(&bootstrap.launch_profile_catalog),
        vec![
            "claude:default".to_owned(),
            "codex:default".to_owned(),
            "claude:haiku".to_owned()
        ]
    );
    assert_eq!(
        launch_profile_entry(&bootstrap.launch_profile_catalog, "claude:default").kind(),
        LaunchProfileKind::BackendDefault
    );
    assert_eq!(
        launch_profile_entry(&bootstrap.launch_profile_catalog, "claude:haiku").kind(),
        LaunchProfileKind::Custom
    );
    let migrated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings_path).expect("read migrated settings"))
            .expect("parse migrated settings");
    assert!(
        migrated["settings"]["launch_profiles"]
            .as_object()
            .is_some_and(|profiles| profiles.contains_key("claude:haiku")),
        "legacy launch profile arrays must migrate to keyed resources: {migrated:?}"
    );

    let duplicate_dir = tempfile::tempdir().expect("duplicate tempdir");
    let duplicate_path = duplicate_dir.path().join("settings.json");
    let profile = migrated["settings"]["launch_profiles"]["claude:haiku"].clone();
    let mut duplicate = migrated;
    duplicate["settings"]["launch_profiles"] =
        serde_json::Value::Array(vec![profile.clone(), profile]);
    std::fs::write(
        &duplicate_path,
        serde_json::to_vec_pretty(&duplicate).expect("serialize duplicate legacy profiles"),
    )
    .expect("write duplicate legacy profiles");
    let error = match server::spawn_host_with_mock_backend(
        duplicate_dir.path().join("sessions.json"),
        duplicate_dir.path().join("projects.json"),
        duplicate_path,
    ) {
        Ok(_) => panic!("duplicate legacy launch profile ids must fail host startup"),
        Err(error) => error,
    };
    assert!(
        error.contains("duplicate launch profile id claude:haiku"),
        "migration failure must identify the duplicate resource: {error}"
    );
}

#[tokio::test]
async fn enabled_backend_change_emits_deduped_launch_profile_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Claude]);
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let bootstrap_env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(bootstrap_env.kind, FrameKind::HostBootstrap);

    client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Claude, BackendKind::Codex],
            vec![BackendKind::Claude],
        )
        .await
        .expect("set enabled backends");

    let catalog_env = next_kind(
        &mut client,
        FrameKind::LaunchProfileCatalogNotify,
        "launch profile catalog update",
    )
    .await;
    let payload: LaunchProfileCatalogPayload = catalog_env
        .parse_payload()
        .expect("LaunchProfileCatalog payload");
    assert_eq!(
        ready_launch_profile_ids(&payload.catalog),
        vec!["claude:default".to_owned(), "codex:default".to_owned()]
    );

    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline - now, client.next_event()).await {
            Err(_) => break,
            Ok(Ok(None)) => break,
            Ok(Ok(Some(env))) if env.kind == FrameKind::LaunchProfileCatalogNotify => {
                panic!("duplicate launch profile catalog notify: {}", env.payload);
            }
            Ok(Ok(Some(_))) => {}
            Ok(Err(err)) => panic!("next_event failed after launch catalog: {err:?}"),
        }
    }
}

#[tokio::test]
async fn stable_reconnect_does_not_emit_unchanged_session_schemas_after_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Acp]);
    let missing_kiro = dir.path().join("missing-kiro-cli-chat");
    let kiro_workspace = tempfile::tempdir().expect("Kiro probe workspace tempdir");
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
        server::HostRuntimeConfig {
            kiro_probe_program: Some(missing_kiro.to_string_lossy().into_owned()),
            kiro_probe_workspace_root: Some(kiro_workspace.path().to_path_buf()),
            skip_real_backend_probe: true,
            ..Default::default()
        },
    )
    .expect("spawn host");

    let mut first = connect_raw(host.clone()).await;
    let first_bootstrap = next_env(&mut first, "first host bootstrap").await;
    assert_eq!(first_bootstrap.kind, FrameKind::HostBootstrap);
    let first_live = next_kind(
        &mut first,
        FrameKind::SessionSchemas,
        "first Kiro schema refresh",
    )
    .await;
    let first_schemas: SessionSchemasPayload =
        first_live.parse_payload().expect("first SessionSchemas");
    let kiro_schema = first_schemas
        .schemas
        .iter()
        .find(|schema| schema.backend_kind() == BackendKind::Acp)
        .expect("Kiro schema should be present");
    let protocol::SessionSchemaEntry::Unavailable { message, .. } = kiro_schema else {
        panic!("missing Kiro executable should make its schema unavailable: {kiro_schema:?}");
    };
    assert!(
        message.starts_with("ACP schema probe stage 'acp_spawn' failed:")
            && message.contains("Failed to start Kiro executable"),
        "shared stage context must be neutral while nested adapter detail remains specific: {message}"
    );
    assert!(
        message.contains(missing_kiro.to_string_lossy().as_ref()),
        "Kiro schema failure should identify the missing executable: {message}"
    );
    assert!(
        kiro_workspace.path().join(".tyde/kiro-admin").is_dir(),
        "Kiro probe should create its admin cwd under the isolated workspace"
    );

    let mut second = connect_raw(host).await;
    let second_bootstrap_env = next_env(&mut second, "second host bootstrap").await;
    assert_eq!(second_bootstrap_env.kind, FrameKind::HostBootstrap);
    let second_bootstrap: HostBootstrapPayload = second_bootstrap_env
        .parse_payload()
        .expect("second HostBootstrap");
    assert_eq!(second_bootstrap.session_schemas, first_schemas.schemas);

    expect_no_session_schemas(
        &mut second,
        Duration::from_millis(500),
        "stable reconnect duplicate schema replay",
    )
    .await;
}

#[tokio::test]
async fn codex_session_schema_refresh_replaces_models_and_surfaces_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    let mut profile_settings = SessionSettingsValues::default();
    profile_settings.0.insert(
        "model".to_owned(),
        SessionSettingValue::String("gpt-5.6".to_owned()),
    );
    write_host_settings_with_launch_profiles(
        &settings_path,
        &[BackendKind::Codex],
        Some(BackendKind::Codex),
        vec![HostLaunchProfileConfig {
            id: LaunchProfileId("codex:gpt-5.6".to_owned()),
            label: "Codex GPT-5.6".to_owned(),
            description: None,
            backend_kind: BackendKind::Codex,
            session_settings: profile_settings,
            acp: None,
        }],
    );
    let fake_codex = write_fake_codex_model_probe_program(&dir);
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
        server::HostRuntimeConfig {
            codex_probe_program: Some(fake_codex.to_string_lossy().into_owned()),
            skip_real_backend_probe: true,
            ..Default::default()
        },
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let bootstrap_env = next_env(&mut client, "Codex host bootstrap").await;
    let bootstrap: HostBootstrapPayload = bootstrap_env
        .parse_payload()
        .expect("Codex HostBootstrap payload");
    assert!(matches!(
        bootstrap.session_schemas.as_slice(),
        [protocol::SessionSchemaEntry::Pending {
            backend_kind: BackendKind::Codex
        }]
    ));
    assert!(matches!(
        launch_profile_entry(&bootstrap.launch_profile_catalog, "codex:gpt-5.6"),
        LaunchProfileEntry::Unavailable { message, .. } if message.contains("still loading")
    ));

    let first_schemas_env = next_kind(
        &mut client,
        FrameKind::SessionSchemas,
        "initial Codex model schema",
    )
    .await;
    let first_schemas: SessionSchemasPayload = first_schemas_env
        .parse_payload()
        .expect("initial Codex SessionSchemas");
    let protocol::SessionSchemaEntry::Ready {
        schema: first_schema,
    } = &first_schemas.schemas[0]
    else {
        panic!("initial Codex schema should be ready: {first_schemas:?}");
    };
    let first_model_field = first_schema
        .fields
        .iter()
        .find(|field| field.key == "model")
        .expect("initial Codex model field");
    let protocol::SessionSettingFieldType::Select {
        options,
        default,
        nullable,
    } = &first_model_field.field_type
    else {
        panic!("Codex model field should be a select");
    };
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["gpt-5.5"]
    );
    assert_eq!(default, &None, "Auto must remain the model default");
    assert!(*nullable, "Auto must remain representable as null");
    let first_reasoning_field = first_schema
        .fields
        .iter()
        .find(|field| field.key == "reasoning_effort")
        .expect("initial Codex reasoning field");
    assert_eq!(
        first_reasoning_field
            .select_options(&SessionSettingsValues::default())
            .expect("initial default-model reasoning options")
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["low", "high"]
    );

    let first_catalog_env = next_kind(
        &mut client,
        FrameKind::LaunchProfileCatalogNotify,
        "initial Codex launch profile catalog",
    )
    .await;
    let first_catalog: LaunchProfileCatalogPayload = first_catalog_env
        .parse_payload()
        .expect("initial Codex LaunchProfileCatalog");
    assert!(matches!(
        launch_profile_entry(&first_catalog.catalog, "codex:gpt-5.6"),
        LaunchProfileEntry::Unavailable { message, .. } if message.contains("invalid session setting 'model' value 'gpt-5.6'")
    ));

    let mut invalid_low = SessionSettingsValues::default();
    invalid_low.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String("max".to_owned()),
    );
    let invalid_config = settings_model::BackendTierConfig {
        low: invalid_low,
        high: SessionSettingsValues::default(),
    };
    let write_id = client
        .replace_setting(
            "/backend_tier_configs/codex",
            &invalid_config,
            serde_json::Value::Null,
        )
        .await
        .expect("write invalid Codex tier config");
    let tier_error = next_kind(
        &mut client,
        FrameKind::SettingsWriteResult,
        "invalid Codex tier SettingsWriteResult",
    )
    .await
    .parse_payload::<SettingsWriteResultPayload>()
    .expect("parse invalid Codex tier SettingsWriteResult");
    assert_eq!(tier_error.write_id, write_id);
    assert!(!tier_error.applied);
    let tier_message = &tier_error.field_errors[0].message;
    assert!(tier_message.contains("invalid Low tier"));
    assert!(tier_message.contains("reasoning_effort"));
    assert!(tier_message.contains("max"));

    client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Codex],
            vec![BackendKind::Codex],
        )
        .await
        .expect("refresh Codex session schema");

    let refreshed_schemas_env = next_kind(
        &mut client,
        FrameKind::SessionSchemas,
        "refreshed Codex model schema",
    )
    .await;
    let refreshed_schemas: SessionSchemasPayload = refreshed_schemas_env
        .parse_payload()
        .expect("refreshed Codex SessionSchemas");
    let protocol::SessionSchemaEntry::Ready {
        schema: refreshed_schema,
    } = &refreshed_schemas.schemas[0]
    else {
        panic!("refreshed Codex schema should be ready: {refreshed_schemas:?}");
    };
    let refreshed_model_field = refreshed_schema
        .fields
        .iter()
        .find(|field| field.key == "model")
        .expect("refreshed Codex model field");
    let protocol::SessionSettingFieldType::Select { options, .. } =
        &refreshed_model_field.field_type
    else {
        panic!("refreshed Codex model field should be a select");
    };
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["gpt-5.6"],
        "refreshed metadata must replace the old process-lifetime model list"
    );
    let refreshed_reasoning_field = refreshed_schema
        .fields
        .iter()
        .find(|field| field.key == "reasoning_effort")
        .expect("refreshed Codex reasoning field");
    assert_eq!(
        refreshed_reasoning_field
            .select_options(&SessionSettingsValues::default())
            .expect("refreshed default-model reasoning options")
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["low", "xhigh", "max"],
        "Codex reasoning options must preserve model metadata order and max"
    );

    let refreshed_catalog_env = next_kind(
        &mut client,
        FrameKind::LaunchProfileCatalogNotify,
        "refreshed Codex launch profile catalog",
    )
    .await;
    let refreshed_catalog: LaunchProfileCatalogPayload = refreshed_catalog_env
        .parse_payload()
        .expect("refreshed Codex LaunchProfileCatalog");
    assert!(matches!(
        launch_profile_entry(&refreshed_catalog.catalog, "codex:gpt-5.6"),
        LaunchProfileEntry::Ready { .. }
    ));

    client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Codex],
            vec![BackendKind::Codex],
        )
        .await
        .expect("refresh failing Codex session schema");

    let unavailable_schemas_env = next_kind(
        &mut client,
        FrameKind::SessionSchemas,
        "unavailable Codex model schema",
    )
    .await;
    let unavailable_schemas: SessionSchemasPayload = unavailable_schemas_env
        .parse_payload()
        .expect("unavailable Codex SessionSchemas");
    assert!(matches!(
        unavailable_schemas.schemas.as_slice(),
        [protocol::SessionSchemaEntry::Unavailable {
            backend_kind: BackendKind::Codex,
            message,
        }] if message.contains("model/list RPC failed") && message.contains("model metadata unavailable")
    ));
}

#[tokio::test]
async fn changed_session_schemas_still_emit_live_after_host_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Claude]);
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let bootstrap_env = next_env(&mut client, "host bootstrap").await;
    assert_eq!(bootstrap_env.kind, FrameKind::HostBootstrap);
    let bootstrap: HostBootstrapPayload = bootstrap_env.parse_payload().expect("HostBootstrap");
    assert_eq!(bootstrap.session_schemas.len(), 1);
    assert_eq!(
        bootstrap.session_schemas[0].backend_kind(),
        BackendKind::Claude
    );
    let protocol::SessionSchemaEntry::Ready { schema } = &bootstrap.session_schemas[0] else {
        panic!("Claude session schema should be ready");
    };
    let effort_field = schema
        .fields
        .iter()
        .find(|field| field.key == "effort")
        .expect("Claude effort field");
    let protocol::SessionSettingFieldType::Select { options, .. } = &effort_field.field_type else {
        panic!("Claude effort should be a select field");
    };
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        vec!["low", "medium", "high", "xhigh", "max"]
    );

    client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Claude, BackendKind::Codex],
            vec![BackendKind::Claude],
        )
        .await
        .expect("set enabled backends");

    let schemas_env = next_kind(
        &mut client,
        FrameKind::SessionSchemas,
        "changed session schemas",
    )
    .await;
    let schemas: SessionSchemasPayload =
        schemas_env.parse_payload().expect("SessionSchemas payload");
    assert_eq!(
        schemas
            .schemas
            .iter()
            .map(protocol::SessionSchemaEntry::backend_kind)
            .collect::<Vec<_>>(),
        vec![BackendKind::Claude, BackendKind::Codex]
    );
}

#[tokio::test]
async fn claude_xhigh_tier_round_trips_and_invalid_effort_preserves_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions_path = dir.path().join("sessions.json");
    let projects_path = dir.path().join("projects.json");
    let settings_path = dir.path().join("settings.json");
    write_enabled_backends_settings(&settings_path, &[BackendKind::Claude]);

    let mut xhigh = SessionSettingsValues::default();
    xhigh.0.insert(
        "effort".to_owned(),
        SessionSettingValue::String("xhigh".to_owned()),
    );
    let expected_config = settings_model::BackendTierConfig {
        low: SessionSettingsValues::default(),
        high: xhigh.clone(),
    };

    {
        let host = server::spawn_host_with_mock_backend(
            sessions_path.clone(),
            projects_path.clone(),
            settings_path.clone(),
        )
        .expect("spawn host");
        let mut client = connect_raw(host).await;
        let _ = next_env(&mut client, "initial host bootstrap").await;

        let saved_write_id = client
            .replace_setting(
                "/backend_tier_configs/claude",
                &expected_config,
                serde_json::Value::Null,
            )
            .await
            .expect("save Claude xhigh tier");
        let saved = next_kind(
            &mut client,
            FrameKind::HostSettings,
            "Claude xhigh HostSettings",
        )
        .await
        .parse_payload::<settings_model::HostSettingsPayload>()
        .expect("parse Claude xhigh HostSettings");
        assert_eq!(
            saved
                .settings
                .backend_tier_configs
                .get(&BackendKind::Claude),
            Some(&expected_config)
        );
        assert_eq!(
            saved.settings.backend_tier_configs[&BackendKind::Claude]
                .high
                .0
                .get("effort"),
            Some(&SessionSettingValue::String("xhigh".to_owned()))
        );
        let saved_result = next_kind(
            &mut client,
            FrameKind::SettingsWriteResult,
            "Claude xhigh SettingsWriteResult",
        )
        .await
        .parse_payload::<SettingsWriteResultPayload>()
        .expect("parse Claude xhigh SettingsWriteResult");
        assert_eq!(saved_result.write_id, saved_write_id);
        assert!(saved_result.applied, "{:?}", saved_result.field_errors);

        let mut ultra = SessionSettingsValues::default();
        ultra.0.insert(
            "effort".to_owned(),
            SessionSettingValue::String("ultra".to_owned()),
        );
        let invalid_config = settings_model::BackendTierConfig {
            low: SessionSettingsValues::default(),
            high: ultra,
        };
        let write_id = client
            .replace_setting(
                "/backend_tier_configs/claude",
                &invalid_config,
                &expected_config,
            )
            .await
            .expect("send invalid Claude tier");
        let error = next_kind(
            &mut client,
            FrameKind::SettingsWriteResult,
            "invalid Claude effort SettingsWriteResult",
        )
        .await
        .parse_payload::<SettingsWriteResultPayload>()
        .expect("parse invalid Claude effort SettingsWriteResult");
        assert_eq!(error.write_id, write_id);
        assert!(!error.applied);
        assert!(error.field_errors[0].message.contains("effort"));
        assert!(error.field_errors[0].message.contains("ultra"));

        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, client.next_event()).await {
                Err(_) | Ok(Ok(None)) => break,
                Ok(Ok(Some(event))) => assert_ne!(
                    event.kind,
                    FrameKind::HostSettings,
                    "rejected Claude effort must not emit substituted or unset settings"
                ),
                Ok(Err(error)) => panic!("client failed after rejected Claude effort: {error:?}"),
            }
        }
    }

    let host = server::spawn_host_with_mock_backend(sessions_path, projects_path, settings_path)
        .expect("reload host");
    let mut client = connect_raw(host).await;
    let bootstrap = next_env(&mut client, "reloaded host bootstrap")
        .await
        .parse_payload::<HostBootstrapPayload>()
        .expect("parse reloaded HostBootstrap");
    assert_eq!(
        bootstrap
            .settings
            .backend_tier_configs
            .get(&BackendKind::Claude),
        Some(&expected_config)
    );
    assert_eq!(
        bootstrap.settings.backend_tier_configs[&BackendKind::Claude]
            .high
            .0
            .get("effort"),
        Some(&SessionSettingValue::String("xhigh".to_owned()))
    );
}

#[tokio::test]
async fn host_bootstrap_includes_session_summaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("sessions.json");
    let store = SessionStore::load(session_path.clone()).expect("load session store");
    store
        .upsert_backend_session(
            &BackendSession {
                id: SessionId("session-1".to_owned()),
                backend_kind: BackendKind::Claude,
                workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                title: Some("Existing session".to_owned()),
                token_count: Some(42),
                created_at_ms: Some(10),
                updated_at_ms: Some(20),
                resumable: true,
            },
            None,
            None,
            None,
            None,
        )
        .expect("insert session");

    let host = server::spawn_host_with_mock_backend(
        session_path,
        dir.path().join("projects.json"),
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(bootstrap.sessions.len(), 1);
    assert_eq!(bootstrap.sessions[0].id.0, "session-1");
    assert_eq!(
        bootstrap.sessions[0].alias.as_deref(),
        Some("Existing session")
    );
    assert_eq!(bootstrap.sessions[0].token_count, Some(42));
}

#[tokio::test]
async fn project_subscription_starts_with_project_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = tempfile::tempdir().expect("project root");
    let project_path = dir.path().join("projects.json");
    let project = ProjectStore::load(project_path.clone())
        .expect("load project store")
        .create(
            "Existing project".to_owned(),
            vec![ProjectRootPath(root.path().to_string_lossy().to_string())],
        )
        .expect("create project");

    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        project_path,
        dir.path().join("settings.json"),
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;

    let env = next_env(&mut client, "host bootstrap").await;
    let host_bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    assert_eq!(host_bootstrap.projects.len(), 1);
    assert_eq!(host_bootstrap.projects[0].id, project.id);

    let env = next_env(&mut client, "project bootstrap").await;
    assert_eq!(env.kind, FrameKind::ProjectBootstrap);
    assert_eq!(env.stream.0, format!("/project/{}", project.id.0));
    assert_eq!(env.seq, 0);
    let bootstrap: ProjectBootstrapPayload =
        env.parse_payload().expect("project bootstrap payload");
    assert_eq!(bootstrap.project.id, project.id);
    assert_eq!(bootstrap.review_summaries.len(), 1);
    assert_eq!(
        bootstrap.review_summaries[0].scope,
        ReviewSummaryScope::Workspace
    );
    assert!(matches!(
        bootstrap.review_summaries[0].status,
        protocol::ReviewStatus::Draft
    ));
}

#[tokio::test]
async fn live_agent_reconnect_starts_with_agent_bootstrap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = spawn_host(&dir);
    let mut first = connect_raw(host.clone()).await;
    let _ = next_env(&mut first, "initial host bootstrap").await;

    first
        .spawn_agent(SpawnAgentPayload {
            name: Some("Bootstrap Agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent");

    let new_agent_env = next_kind(&mut first, FrameKind::NewAgent, "new agent").await;
    let new_agent: NewAgentPayload = new_agent_env.parse_payload().expect("new agent payload");
    loop {
        let env = next_env(&mut first, "agent start replay").await;
        match env.kind {
            FrameKind::AgentBootstrap => {
                let bootstrap: protocol::AgentBootstrapPayload =
                    env.parse_payload().expect("agent bootstrap payload");
                if bootstrap
                    .events
                    .iter()
                    .any(|event| matches!(event, protocol::AgentBootstrapEvent::AgentStart(_)))
                {
                    break;
                }
            }
            FrameKind::AgentStart => break,
            _ => {}
        }
    }

    let mut second = connect_raw(host).await;
    let env = next_env(&mut second, "host bootstrap").await;
    let host_bootstrap: HostBootstrapPayload = env.parse_payload().expect("host bootstrap payload");
    let bootstrapped_agent = host_bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .expect("live agent in HostBootstrap");

    let env = loop {
        let env = next_env(&mut second, "agent bootstrap").await;
        if env.stream == bootstrapped_agent.instance_stream {
            break env;
        }
    };
    assert_eq!(env.kind, FrameKind::AgentBootstrap);
    assert_eq!(env.seq, 0);
    let bootstrap: protocol::AgentBootstrapPayload =
        env.parse_payload().expect("agent bootstrap payload");
    assert!(
        bootstrap
            .events
            .iter()
            .any(|event| matches!(event, protocol::AgentBootstrapEvent::AgentStart(_))),
        "AgentBootstrap should carry the replayed AgentStart"
    );
}

#[tokio::test]
async fn spawn_agent_accepts_launch_profile_id_and_records_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    write_host_settings(
        &settings_path,
        &[BackendKind::Claude],
        Some(BackendKind::Claude),
    );
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;
    let _ = next_env(&mut client, "host bootstrap").await;

    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Profile Agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                prompt: "hello from profile".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: Some(LaunchProfileId("claude:default".to_owned())),
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent");

    let new_agent_env = next_kind(&mut client, FrameKind::NewAgent, "new agent").await;
    let new_agent: NewAgentPayload = new_agent_env.parse_payload().expect("new agent");
    assert_eq!(
        new_agent.launch_profile_id.as_ref().map(|id| id.0.as_str()),
        Some("claude:default")
    );

    let session_list_env = next_kind(&mut client, FrameKind::SessionList, "session list").await;
    let session_list: SessionListPayload = session_list_env.parse_payload().expect("session list");
    let summary = session_list
        .sessions
        .iter()
        .find(|summary| summary.user_alias.as_deref() == Some("Profile Agent"))
        .expect("profile-launched session summary");
    assert_eq!(
        summary.launch_profile_id.as_ref().map(|id| id.0.as_str()),
        Some("claude:default")
    );
}

#[tokio::test]
async fn launch_profile_errors_are_visible_command_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    let mut invalid_settings = SessionSettingsValues::default();
    invalid_settings.0.insert(
        "not_a_claude_setting".to_owned(),
        SessionSettingValue::String("x".to_owned()),
    );
    write_host_settings_with_launch_profiles(
        &settings_path,
        &[BackendKind::Claude, BackendKind::Codex, BackendKind::Hermes],
        None,
        vec![HostLaunchProfileConfig {
            id: LaunchProfileId("claude:invalid".to_owned()),
            label: "Invalid Claude Profile".to_owned(),
            description: None,
            backend_kind: BackendKind::Claude,
            session_settings: invalid_settings,
            acp: None,
        }],
    );
    let host = server::spawn_host_with_mock_backend(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
    )
    .expect("spawn host");
    let mut client = connect_raw(host).await;
    let _ = next_env(&mut client, "host bootstrap").await;

    for (profile_id, backend_kind, expected_code, expected_message) in [
        (
            "missing:profile",
            BackendKind::Claude,
            CommandErrorCode::InvalidInput,
            "unknown launch_profile_id",
        ),
        (
            "codex:default",
            BackendKind::Claude,
            CommandErrorCode::Conflict,
            "targets Codex",
        ),
        (
            "claude:invalid",
            BackendKind::Claude,
            CommandErrorCode::InvalidInput,
            "unavailable",
        ),
        (
            "hermes:claude",
            BackendKind::Hermes,
            CommandErrorCode::InvalidInput,
            "unknown launch_profile_id",
        ),
    ] {
        client
            .spawn_agent(SpawnAgentPayload {
                name: Some(format!("Bad profile {profile_id}")),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots: vec![dir.path().to_string_lossy().to_string()],
                    prompt: "this should fail".to_owned(),
                    images: None,
                    backend_kind,
                    launch_profile_id: Some(LaunchProfileId(profile_id.to_owned())),
                    cost_hint: None,
                    access_mode: BackendAccessMode::Unrestricted,
                    session_settings: None,
                },
            })
            .await
            .expect("write spawn");

        let error_env = next_kind(
            &mut client,
            FrameKind::CommandError,
            "profile command error",
        )
        .await;
        let error: CommandErrorPayload = error_env.parse_payload().expect("command error");
        assert_eq!(error.request_kind, FrameKind::SpawnAgent);
        assert_eq!(error.code, expected_code);
        assert!(
            error.message.contains(expected_message),
            "expected {expected_message:?} in {}",
            error.message
        );
    }
}

#[tokio::test]
async fn browse_and_terminal_streams_start_with_bootstraps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = spawn_host(&dir);
    let mut client = connect_raw(host).await;
    let _ = next_env(&mut client, "host bootstrap").await;

    let browse_stream = protocol::StreamPath(format!("/browse/{}", uuid::Uuid::new_v4()));
    client
        .host_browse_start(HostBrowseStartPayload {
            browse_stream: browse_stream.clone(),
            initial: HostBrowseInitial::Path {
                path: protocol::HostAbsPath(dir.path().to_string_lossy().to_string()),
            },
            include_hidden: false,
        })
        .await
        .expect("start browse");
    let browse = next_env(&mut client, "browse bootstrap").await;
    assert_eq!(browse.kind, FrameKind::BrowseBootstrap);
    assert_eq!(browse.stream, browse_stream);
    assert_eq!(browse.seq, 0);

    client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::HostDefault,
            cols: 80,
            rows: 24,
        })
        .await
        .expect("create terminal");
    let terminal = next_kind(&mut client, FrameKind::NewTerminal, "new terminal").await;
    let new_terminal: protocol::NewTerminalPayload =
        terminal.parse_payload().expect("new terminal");
    let terminal_bootstrap = next_env(&mut client, "terminal bootstrap").await;
    assert_eq!(terminal_bootstrap.kind, FrameKind::TerminalBootstrap);
    assert_eq!(terminal_bootstrap.stream, new_terminal.stream);
    assert_eq!(terminal_bootstrap.seq, 0);
}
