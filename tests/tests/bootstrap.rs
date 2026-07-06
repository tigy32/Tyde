use std::time::Duration;

use client::ClientConfig;
use protocol::{
    BackendAccessMode, BackendConfigPersistenceMode, BackendKind, CommandErrorCode,
    CommandErrorPayload, FrameKind, HostBootstrapPayload, HostBrowseInitial,
    HostBrowseStartPayload, HostLaunchProfileConfig, HostSettingValue, LaunchProfileCatalog,
    LaunchProfileCatalogPayload, LaunchProfileEntry, LaunchProfileId, LaunchProfileKind,
    NewAgentPayload, ProjectBootstrapPayload, ProjectRootPath, ReviewSummaryScope, SessionId,
    SessionListPageStatus, SessionListPayload, SessionSchemasPayload, SessionSettingValue,
    SessionSettingsValues, SetSettingPayload, SpawnAgentParams, SpawnAgentPayload,
    TerminalCreatePayload, TerminalLaunchTarget,
};
use server::backend::BackendSession;
use server::store::project::ProjectStore;
use server::store::session::SessionStore;

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
    match tokio::time::timeout(duration, client.next_event()).await {
        Err(_) => {}
        Ok(Ok(None)) => {}
        Ok(Ok(Some(env))) => panic!(
            "unexpected event before {context}: kind={} stream={}",
            env.kind, env.stream
        ),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
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
    let settings = protocol::HostSettings {
        enabled_backends: backends.to_vec(),
        default_backend,
        enable_mobile_connections: false,
        mobile_broker_url: None,
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
        complexity_tiers_enabled: false,
        backend_tier_configs: std::collections::HashMap::new(),
        background_agent_features: Default::default(),
        code_intel: Default::default(),
        backend_config: std::collections::HashMap::new(),
        launch_profiles,
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
    }
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
    assert_eq!(bootstrap.session_list.total_count, 300);
    assert_eq!(
        bootstrap.sessions.len(),
        protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT as usize
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
        protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT
    );

    client
        .list_sessions(protocol::ListSessionsPayload {
            cursor: Some(next_cursor),
            limit: Some(protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT),
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
        protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT
    );
    assert_eq!(page.page.total_count, 300);
    assert_eq!(
        page.sessions.len(),
        protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT as usize
    );
    assert!(matches!(
        page.page.status,
        SessionListPageStatus::More { .. }
    ));
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
        Some("session-0063")
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
                cursor: Some(next_cursor),
                limit: Some(protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT),
            })
            .await
            .expect("request next session page");
        let env = next_kind(&mut client, FrameKind::SessionList, "next session page").await;
        let page: SessionListPayload = env.parse_payload().expect("parse SessionList");
        assert_eq!(
            page.page.cursor.generation, first_generation,
            "continuation pages must come from the original snapshot"
        );
        if page.page.cursor.offset == protocol::DEFAULT_SESSION_LIST_PAGE_LIMIT {
            assert_eq!(
                page.sessions.first().map(|session| session.id.0.as_str()),
                Some("session-0064"),
                "fresh offset paging would duplicate session-0063 and silently skip a later session"
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
    assert_eq!(
        bootstrap
            .backend_config_schemas
            .iter()
            .map(|schema| schema.backend_kind)
            .collect::<Vec<_>>(),
        vec![BackendKind::Tycode, BackendKind::Hermes]
    );
    assert_eq!(
        bootstrap.backend_config_schemas[0].persistence_mode,
        BackendConfigPersistenceMode::BackendNative
    );
    assert_eq!(
        bootstrap.backend_config_schemas[1].persistence_mode,
        BackendConfigPersistenceMode::TydeSettingsStore
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
    write_host_settings_with_launch_profiles(
        &settings_path,
        &[BackendKind::Claude, BackendKind::Codex],
        Some(BackendKind::Claude),
        vec![HostLaunchProfileConfig {
            id: LaunchProfileId("claude:haiku".to_owned()),
            label: "Claude Haiku".to_owned(),
            description: Some("Launch Claude with Haiku.".to_owned()),
            backend_kind: BackendKind::Claude,
            session_settings: profile_settings,
        }],
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
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude, BackendKind::Codex],
            },
        })
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
    write_enabled_backends_settings(&settings_path, &[BackendKind::Kiro]);
    let missing_kiro = dir.path().join("missing-kiro-cli-chat");
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        dir.path().join("sessions.json"),
        dir.path().join("projects.json"),
        settings_path,
        server::HostRuntimeConfig {
            kiro_probe_program: Some(missing_kiro.to_string_lossy().into_owned()),
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
    assert!(
        matches!(
            first_schemas.schemas.first(),
            Some(protocol::SessionSchemaEntry::Unavailable { .. })
        ),
        "test expects the fake Kiro probe to settle to an unavailable schema"
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

    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude, BackendKind::Codex],
            },
        })
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
