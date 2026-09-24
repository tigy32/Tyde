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
    BackendConfigSnapshotsPayload, BackendConfigValues, BackendKind, BackendSetupDiagnosticCode,
    BackendSetupStatus, CodeIntelProviderId, FrameKind, SessionId, SessionSchemasPayload,
    SessionSettingValue, SettingExpectation, SettingOp, SettingsErrorCode, SettingsWriteId,
    SettingsWritePayload, SettingsWriteResultPayload,
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

fn mock_model_discovery(model: &str, efforts: &[&str]) -> server::backend::BackendDiscovery {
    let field = |key: &str, values: &[&str]| protocol::SessionSettingField {
        key: key.to_owned(),
        label: key.to_owned(),
        description: None,
        use_slider: false,
        select_options_by_setting: None,
        field_type: protocol::SessionSettingFieldType::Select {
            options: values
                .iter()
                .map(|value| protocol::SelectOption {
                    value: (*value).to_owned(),
                    label: (*value).to_owned(),
                })
                .collect(),
            default: None,
            nullable: true,
        },
    };
    server::backend::BackendDiscovery {
        schema: protocol::SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Codex,
            fields: vec![field("model", &[model]), field("reasoning_effort", efforts)],
        },
        launch_profiles: Vec::new(),
    }
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
        resume_previous_agents: settings_model::default_resume_previous_agents(),
        review: Default::default(),
        enabled_backends: Vec::new(),
        default_backend: None,
        enable_mobile_connections: false,

        mobile_direct_hosting_enabled: false,
        mobile_direct_bind_addr: None,
        mobile_direct_public_origin: None,
        mobile_direct_bundle_dir: None,
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
        tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
        delegation_launch_profile_order: settings_model::default_delegation_launch_profile_order(),
        complexity_tiers_enabled: false,
        backend_tier_configs: std::collections::HashMap::new(),
        background_agent_features: Default::default(),
        supervisor: Default::default(),
        usage_limits: Default::default(),
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

/// The Kiro backend has been persisted under two names: `kiro` before it was
/// renamed for the protocol it speaks, then `acp`. `kiro` is canonical again,
/// so a store written by *either* released build has to load and normalize onto
/// it — including the map keys and launch profiles, which are separate code
/// paths from the backend list.
#[test]
fn the_legacy_acp_backend_spelling_is_migrated_to_kiro() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{
  "settings": {
    "enabled_backends": ["acp", "claude"],
    "default_backend": "acp",
    "backend_tier_configs": { "acp": { "low": { "model": { "string": "haiku" } } } }
  }
}"#,
    )
    .expect("write settings store");

    let store = HostSettingsStore::load(path.clone()).expect("load settings store");
    let settings = store.get().expect("read canonicalized settings");

    assert_eq!(
        settings.enabled_backends,
        vec![BackendKind::Kiro, BackendKind::Claude],
        "the legacy acp spelling must load as the Kiro backend"
    );
    assert_eq!(settings.default_backend, Some(BackendKind::Kiro));
    // A map keyed by backend kind is a separate code path from the list, and
    // the one that silently loses data: an unmigrated key is dropped as an
    // unknown backend rather than failing the load.
    assert!(
        settings
            .backend_tier_configs
            .contains_key(&BackendKind::Kiro),
        "the per-backend tier-config map is keyed by kind and must migrate too, got: {:?}",
        settings.backend_tier_configs.keys().collect::<Vec<_>>()
    );

    // The rename is persisted, not just applied in memory: the next build to
    // read this file must not have to migrate it again.
    let on_disk = fs::read_to_string(&path).expect("re-read settings store");
    assert!(
        on_disk.contains("\"kiro\"") && !on_disk.contains("\"acp\""),
        "the store must be rewritten in the canonical spelling, got: {on_disk}"
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
    "enabled_backends": ["gemini", "claude", "tycode", "kiro", "claude"],
    "default_backend": "claude"
  }
}"#,
    )
    .expect("write settings store");

    let store = HostSettingsStore::load(path).expect("load settings store");

    assert_eq!(
        store.get().expect("read canonicalized settings"),
        HostSettings {
            resume_previous_agents: settings_model::default_resume_previous_agents(),
            review: Default::default(),
            enabled_backends: vec![
                BackendKind::Kiro,
                BackendKind::Claude,
                BackendKind::Antigravity,
            ],
            default_backend: Some(BackendKind::Claude),
            enable_mobile_connections: false,

            mobile_direct_hosting_enabled: false,
            mobile_direct_bind_addr: None,
            mobile_direct_public_origin: None,
            mobile_direct_bundle_dir: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
            delegation_launch_profile_order:
                settings_model::default_delegation_launch_profile_order(),
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            supervisor: Default::default(),
            usage_limits: Default::default(),
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

    assert!(!fixture.bootstrap.settings.usage_limits.enabled);
    assert!(!fixture.bootstrap.settings.usage_limits.compact_enabled);
    assert!(
        !fixture
            .bootstrap
            .settings
            .usage_limits
            .auto_start_short_windows
    );
    assert!(
        !fixture
            .bootstrap
            .settings
            .usage_limits
            .auto_start_weekly_windows
    );
    let valid = [
        (
            "usage-wake-short",
            "/usage_limits/auto_start_short_windows",
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "usage-wake-weekly",
            "/usage_limits/auto_start_weekly_windows",
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "usage-enable",
            "/usage_limits/enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "usage-compact",
            "/usage_limits/compact_enabled",
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "usage-stop",
            "/usage_limits/stop_used_percent",
            serde_json::json!(85),
            serde_json::json!(90),
        ),
        (
            "usage-context",
            "/usage_limits/compact_context_percent",
            serde_json::json!(70),
            serde_json::json!(60),
        ),
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
            "usage-stop-zero",
            "/usage_limits/stop_used_percent",
            serde_json::json!(0),
            serde_json::json!(85),
        ),
        (
            "usage-context-over",
            "/usage_limits/compact_context_percent",
            serde_json::json!(101),
            serde_json::json!(70),
        ),
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
async fn backend_setup_refresh_republishes_without_reconnecting() {
    let mut fixture = Fixture::new().await;
    for _ in 0..2 {
        fixture
            .client
            .backend_setup_refresh()
            .await
            .expect("request setup refresh");
        let envelope = next_frame_matching_on(
            &mut fixture.client,
            "fresh backend installation state",
            |env| env.kind == FrameKind::BackendSetup,
        )
        .await;
        let payload: protocol::BackendSetupPayload =
            envelope.parse_payload().expect("backend setup");
        assert_eq!(payload, fixture.bootstrap.backend_setup);
    }
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
async fn retired_transport_secrets_cannot_be_written() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .settings_write(SettingsWritePayload {
            write_id: SettingsWriteId("retired-secret".to_owned()),
            ops: vec![SettingOp::Replace {
                path: "/mobile_broker_auth/password".to_owned(),
                value: serde_json::json!("must-not-be-stored"),
                expected: SettingExpectation::Absent,
            }],
        })
        .await
        .expect("attempt retired setting write");
    let result = expect_settings_write_result(
        &mut fixture.client,
        "retired-secret",
        "retired secret rejection",
    )
    .await;
    assert!(!result.applied);
    assert!(!result.field_errors.is_empty());
    let observer = fixture.connect().await;
    drop(observer);
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
/// draft-clear correlation. A slow Hermes probe holds
/// the first write's refresh window open deterministically while the
/// second write arrives.
#[tokio::test]
async fn settings_write_lifecycle_is_serialized_across_writers() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture = Fixture::new_with_real_backend_probe_for_enabled_backends(Vec::new()).await;
    let mut second = fixture.connect().await;
    // Applied after fixture startup so only the write-triggered refresh
    // probes are slowed.
    let slow_hermes = temp_home.path().join("slow-hermes-python");
    std::fs::write(&slow_hermes, "#!/bin/sh\nsleep 1\nexit 1\n").expect("write slow Hermes probe");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&slow_hermes)
            .expect("stat slow Hermes probe")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&slow_hermes, permissions)
            .expect("make slow Hermes probe executable");
    }
    let _slow_hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", slow_hermes.to_string_lossy().to_string());

    // W1: opens a multi-second refresh window (real Hermes probes).
    send_settings_write(
        &mut fixture.client,
        "w-serial-hermes",
        vec![replace_op(
            "/enabled_backends",
            serde_json::json!(["hermes"]),
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
                        payload.write_id.0, "w-serial-hermes",
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
        vec![BackendKind::Hermes]
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
        vec![BackendKind::Hermes]
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
        vec![BackendKind::Hermes]
    );
    assert!(bootstrap.settings.supervisor.enabled);
    assert_eq!(bootstrap.settings_etag, w2_result.current_etag);
}

/// Enabling a backend refreshes both session schemas and config snapshots.
#[tokio::test]
async fn settings_write_enabled_backends_refreshes_schemas_and_config_snapshots() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture = Fixture::new_with_runtime_config_and_real_backend_probe_for_enabled_backends(
        server::HostRuntimeConfig {
            mock_backend_discovery: [(
                BackendKind::Codex,
                server::backend::mock::MockDiscovery::new(vec![Ok(mock_model_discovery(
                    "gpt-test",
                    &["medium"],
                ))]),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        Vec::new(),
    )
    .await;
    assert!(fixture.bootstrap.settings.enabled_backends.is_empty());

    next_frame_matching_on(
        &mut fixture.client,
        "initial Hermes native settings snapshot",
        |env| {
            if env.kind != FrameKind::BackendConfigSnapshots {
                return false;
            }
            let payload: BackendConfigSnapshotsPayload = env
                .parse_payload()
                .expect("parse initial BackendConfigSnapshots");
            payload
                .native_settings
                .iter()
                .any(|snapshot| snapshot.backend_kind == BackendKind::Hermes)
        },
    )
    .await;
    let hermes_home = temp_home.path().join(".hermes");
    fs::create_dir_all(&hermes_home).expect("create Hermes home");
    fs::write(hermes_home.join("config.yaml"), "model: refreshed-model\n")
        .expect("update Hermes native settings");

    send_settings_write(
        &mut fixture.client,
        "w-enable-codex",
        vec![replace_op(
            "/enabled_backends",
            serde_json::json!(["codex"]),
            serde_json::json!([]),
        )],
    )
    .await;

    let fanout = expect_host_settings_frame(&mut fixture.client, "Codex enable fanout").await;
    assert_eq!(fanout.settings.enabled_backends, vec![BackendKind::Codex]);

    // The write must produce all three of: a SessionSchemas refresh carrying
    // Codex, a BackendConfigSnapshots refresh carrying the Hermes native
    // probe, and the requester-scoped result. The result rides the control
    // lane and may overtake the bulk-lane refresh frames, so collect them in
    // any order; a dropped refresh coupling means its frame never arrives
    // and the wait times out.
    let mut saw_codex_schemas = false;
    let mut saw_hermes_snapshot = false;
    let mut result: Option<SettingsWriteResultPayload> = None;
    while !(saw_codex_schemas && saw_hermes_snapshot && result.is_some()) {
        next_frame_matching_on(
            &mut fixture.client,
            "session-schema + config-snapshot refreshes and result after enabling Codex",
            |env| match env.kind {
                FrameKind::SessionSchemas => {
                    let payload: SessionSchemasPayload = env
                        .parse_payload()
                        .expect("parse SessionSchemas after enabling Codex");
                    let has_codex = payload
                        .schemas
                        .iter()
                        .any(|entry| entry.backend_kind() == BackendKind::Codex);
                    saw_codex_schemas |= has_codex;
                    has_codex
                }
                FrameKind::BackendConfigSnapshots => {
                    let payload: BackendConfigSnapshotsPayload = env
                        .parse_payload()
                        .expect("parse BackendConfigSnapshots after enabling Hermes");
                    let has_refreshed_hermes = payload.native_settings.iter().any(|snapshot| {
                        snapshot.backend_kind == BackendKind::Hermes
                            && snapshot
                                .settings
                                .as_ref()
                                .and_then(|settings| {
                                    settings.pointer("/profiles/0/config/model/model")
                                })
                                .and_then(serde_json::Value::as_str)
                                == Some("refreshed-model")
                    });
                    saw_hermes_snapshot |= has_refreshed_hermes;
                    has_refreshed_hermes
                }
                FrameKind::SettingsWriteResult => {
                    let payload: SettingsWriteResultPayload = env
                        .parse_payload()
                        .expect("parse SettingsWriteResult after enabling Codex");
                    assert_eq!(payload.write_id.0, "w-enable-codex");
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

fn codex_schema_offers(entries: &[protocol::SessionSchemaEntry], model: &str) -> bool {
    entries.iter().any(|entry| match entry {
        protocol::SessionSchemaEntry::Ready { schema } => {
            schema.backend_kind == BackendKind::Codex
                && schema.fields.iter().any(|field| {
                    field.key == "model"
                        && matches!(
                            &field.field_type,
                            protocol::SessionSettingFieldType::Select { options, .. }
                                if options.iter().any(|option| option.value == model)
                        )
                })
        }
        _ => false,
    })
}

async fn wait_for_codex_catalog(fixture: &mut Fixture, model: &str) {
    if codex_schema_offers(&fixture.bootstrap.session_schemas, model) {
        return;
    }
    next_frame_matching_on(&mut fixture.client, "Codex catalog", |env| {
        env.kind == FrameKind::SessionSchemas
            && codex_schema_offers(
                &env.parse_payload::<SessionSchemasPayload>()
                    .expect("parse SessionSchemas")
                    .schemas,
                model,
            )
    })
    .await;
}

/// The user-visible flow: a tier was saved, then the backend's model catalog
/// changed under it (a CLI upgrade, a provider switch). Every tier edit writes
/// both tiers, so revalidating the untouched stale tier rejected every edit —
/// including the ones that would repair it. Only the tier an edit changes is
/// validated; a changed tier is still held to the current catalog.
#[tokio::test]
async fn tier_edit_is_not_blocked_by_a_stale_untouched_tier() {
    let mut fixture = Fixture::new_with_runtime_config_and_settings_file(
        server::HostRuntimeConfig {
            mock_backend_discovery: [(
                BackendKind::Codex,
                server::backend::mock::MockDiscovery::new(vec![Ok(mock_model_discovery(
                    "gpt-new",
                    &["low", "high"],
                ))]),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        r#"{
  "settings": {
    "enabled_backends": ["codex"],
    "default_backend": "codex",
    "complexity_tiers_enabled": true,
    "backend_tier_configs": {
      "codex": {
        "low": { "model": { "string": "gpt-old" }, "reasoning_effort": { "string": "low" } },
        "high": { "model": { "string": "gpt-old" }, "reasoning_effort": { "string": "max" } }
      }
    }
  }
}"#,
    )
    .await;
    wait_for_codex_catalog(&mut fixture, "gpt-new").await;
    let tier = |model: &str, effort: &str| {
        serde_json::json!({
            "model": { "string": model },
            "reasoning_effort": { "string": effort }
        })
    };
    let stale_high = tier("gpt-old", "max");
    let fresh_low = tier("gpt-new", "high");

    send_settings_write(
        &mut fixture.client,
        "w-tier-low-only",
        vec![replace_op(
            "/backend_tier_configs/codex",
            serde_json::json!({ "low": fresh_low, "high": stale_high }),
            serde_json::json!({ "low": tier("gpt-old", "low"), "high": stale_high }),
        )],
    )
    .await;
    let result =
        expect_settings_write_result(&mut fixture.client, "w-tier-low-only", "Low-only edit").await;
    assert!(
        result.applied,
        "a Low edit must not be rejected for the untouched High tier: {:?}",
        result.field_errors
    );

    send_settings_write(
        &mut fixture.client,
        "w-tier-high-stale",
        vec![replace_op(
            "/backend_tier_configs/codex",
            serde_json::json!({ "low": fresh_low, "high": tier("gpt-old", "low") }),
            serde_json::json!({ "low": fresh_low, "high": stale_high }),
        )],
    )
    .await;
    let result = expect_settings_write_result(
        &mut fixture.client,
        "w-tier-high-stale",
        "High edit to a dropped model",
    )
    .await;
    assert!(!result.applied, "a changed tier is still validated");
    let messages = result
        .field_errors
        .iter()
        .map(|error| error.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec!["invalid High tier: invalid session setting 'model' value 'gpt-old'"],
        "only the changed High tier is rejected, naming the dropped model"
    );

    send_settings_write(
        &mut fixture.client,
        "w-tier-high-repair",
        vec![replace_op(
            "/backend_tier_configs/codex",
            serde_json::json!({ "low": fresh_low, "high": tier("gpt-new", "high") }),
            serde_json::json!({ "low": fresh_low, "high": stale_high }),
        )],
    )
    .await;
    let result = expect_settings_write_result(
        &mut fixture.client,
        "w-tier-high-repair",
        "High repaired to the current catalog",
    )
    .await;
    assert!(result.applied, "{:?}", result.field_errors);
}

async fn save_hermes_doc(
    fixture: &mut Fixture,
    doc: &protocol::hermes_config::HermesNativeSettingsDoc,
    context: &str,
) -> SettingsWriteResultPayload {
    let write_id = fixture
        .client
        .backend_native_settings_write(BackendKind::Hermes, doc)
        .await
        .unwrap_or_else(|err| panic!("send Hermes native settings write for {context}: {err:?}"));
    expect_settings_write_result(&mut fixture.client, &write_id.0, context).await
}

/// The user-visible flow: one Hermes profile's `config.yaml` carries a value
/// Tyde's editor refuses (`threshold_pct: 150`, hand-edited or written by the
/// Hermes CLI, which clamps it). Every Hermes save carries every profile's
/// config, so revalidating that untouched stale value rejected every save —
/// edits to other profiles, other fields of the same profile, and profile
/// create/delete. Only the fields a save changes are validated; a changed
/// threshold is still held to 0..100, and repairing it saves.
#[tokio::test]
async fn hermes_save_is_not_blocked_by_a_stale_untouched_profile_value() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    let hermes_home = temp_home.path().join(".hermes");
    let stale_profile_home = hermes_home.join("profiles").join("stale");
    fs::create_dir_all(&stale_profile_home).expect("create stale Hermes profile");
    fs::write(hermes_home.join("config.yaml"), "model: default-model\n")
        .expect("write default Hermes config");
    fs::write(
        stale_profile_home.join("config.yaml"),
        "model: stale-model\ntools:\n  tool_search:\n    threshold_pct: 150\n",
    )
    .expect("write stale Hermes profile config");
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_home = EnvVarGuard::set("HERMES_HOME", hermes_home.to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture = Fixture::new_with_runtime_config_and_real_backend_probe_for_enabled_backends(
        server::HostRuntimeConfig::default(),
        vec![BackendKind::Hermes],
    )
    .await;
    let mut snapshot = None;
    next_frame_matching_on(
        &mut fixture.client,
        "Hermes native settings snapshot with the stale profile",
        |env| {
            if env.kind != FrameKind::BackendConfigSnapshots {
                return false;
            }
            let payload: BackendConfigSnapshotsPayload =
                env.parse_payload().expect("parse BackendConfigSnapshots");
            snapshot = payload
                .native_settings
                .into_iter()
                .find(|snapshot| snapshot.backend_kind == BackendKind::Hermes)
                .and_then(|snapshot| snapshot.settings)
                .map(|settings| {
                    serde_json::from_value::<protocol::hermes_config::HermesNativeSettingsDoc>(
                        settings,
                    )
                    .expect("parse Hermes native settings doc")
                })
                .filter(|doc| doc.profiles.iter().any(|profile| profile.name == "stale"));
            snapshot.is_some()
        },
    )
    .await;
    let mut doc = snapshot.expect("Hermes snapshot");
    let stale = doc
        .profiles
        .iter()
        .find(|profile| profile.name == "stale")
        .expect("stale profile in snapshot");
    assert_eq!(
        stale.config.tool_search.threshold_pct,
        Some(150.0),
        "the stale value loads and is shown to the user"
    );
    let profile_index = |doc: &protocol::hermes_config::HermesNativeSettingsDoc, name: &str| {
        doc.profiles
            .iter()
            .position(|profile| profile.name == name)
            .unwrap_or_else(|| panic!("profile {name} in Hermes doc"))
    };
    let default_index = profile_index(&doc, "default");
    let stale_index = profile_index(&doc, "stale");
    let rebase = |doc: &mut protocol::hermes_config::HermesNativeSettingsDoc| {
        for profile in &mut doc.profiles {
            profile.base_config = Some(profile.config.clone());
        }
        doc.profile_actions.clear();
    };
    rebase(&mut doc);

    // Editing another profile.
    doc.profiles[default_index].config.model.model = Some("edited-default".to_owned());
    let result = save_hermes_doc(&mut fixture, &doc, "edit the default profile").await;
    assert!(
        result.applied,
        "an edit to another profile must not be rejected for the untouched stale threshold: {:?}",
        result.field_errors
    );
    assert!(
        fs::read_to_string(hermes_home.join("config.yaml"))
            .expect("read default Hermes config")
            .contains("edited-default")
    );
    rebase(&mut doc);

    // Creating a profile.
    doc.profile_actions = vec![
        protocol::hermes_config::HermesProfileAction::CreateProfile {
            name: "fresh".to_owned(),
            copy_config_from: None,
        },
    ];
    let result = save_hermes_doc(&mut fixture, &doc, "create a profile").await;
    assert!(
        result.applied,
        "creating a profile must not be rejected for the untouched stale threshold: {:?}",
        result.field_errors
    );
    assert!(hermes_home.join("profiles").join("fresh").is_dir());
    rebase(&mut doc);

    // Editing a different field of the stale profile itself.
    doc.profiles[stale_index].config.model.model = Some("edited-stale".to_owned());
    let result = save_hermes_doc(
        &mut fixture,
        &doc,
        "edit another field of the stale profile",
    )
    .await;
    assert!(
        result.applied,
        "an edit to another field must not be rejected for the untouched stale threshold: {:?}",
        result.field_errors
    );
    let stale_yaml =
        fs::read_to_string(stale_profile_home.join("config.yaml")).expect("read stale config");
    assert!(stale_yaml.contains("edited-stale"), "{stale_yaml}");
    assert!(
        stale_yaml.contains("threshold_pct: 150"),
        "the untouched stale value is preserved, not rewritten: {stale_yaml}"
    );
    rebase(&mut doc);

    // A changed threshold is still validated.
    doc.profiles[stale_index].config.tool_search.threshold_pct = Some(250.0);
    let result = save_hermes_doc(&mut fixture, &doc, "edit the threshold out of range").await;
    assert!(!result.applied, "a changed threshold is still validated");
    let messages = result
        .field_errors
        .iter()
        .map(|error| error.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec![
            "invalid Hermes settings for profile 'stale': tool search threshold must be \
             between 0 and 100, got 250"
        ]
    );

    // Repairing the stale value saves.
    doc.profiles[stale_index].config.tool_search.threshold_pct = Some(50.0);
    let result = save_hermes_doc(&mut fixture, &doc, "repair the stale threshold").await;
    assert!(result.applied, "{:?}", result.field_errors);
    let stale_yaml =
        fs::read_to_string(stale_profile_home.join("config.yaml")).expect("read stale config");
    assert!(stale_yaml.contains("threshold_pct: 50"), "{stale_yaml}");
}

fn kiro_model_discovery(models: &[&str]) -> server::backend::BackendDiscovery {
    let mut discovery = mock_model_discovery(models[0], &[]);
    discovery.schema.backend_kind = BackendKind::Kiro;
    discovery.schema.fields = vec![protocol::SessionSettingField {
        key: "model".to_owned(),
        label: "model".to_owned(),
        description: None,
        use_slider: false,
        select_options_by_setting: None,
        field_type: protocol::SessionSettingFieldType::Select {
            options: models
                .iter()
                .map(|model| protocol::SelectOption {
                    value: (*model).to_owned(),
                    label: (*model).to_owned(),
                })
                .collect(),
            default: None,
            nullable: true,
        },
    }];
    discovery
}

fn offered_models(entry: &protocol::SessionSchemaEntry) -> Vec<String> {
    let Some(schema) = entry.ready_schema() else {
        return Vec::new();
    };
    schema
        .fields
        .iter()
        .filter(|field| field.key == "model")
        .flat_map(|field| match &field.field_type {
            protocol::SessionSettingFieldType::Select { options, .. } => options
                .iter()
                .map(|option| option.value.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

fn custom_profile_models(
    catalog: &protocol::LaunchProfileCatalog,
    profile_id: &str,
) -> Option<Vec<String>> {
    catalog
        .custom_profile_schemas
        .iter()
        .find(|entry| entry.launch_profile_id.0 == profile_id)
        .map(|entry| offered_models(&entry.schema))
}

fn profile_is_ready(catalog: &protocol::LaunchProfileCatalog, profile_id: &str) -> bool {
    catalog.entries.iter().any(|entry| {
        entry.id().0 == profile_id && matches!(entry, protocol::LaunchProfileEntry::Ready { .. })
    })
}

async fn wait_for_launch_profile_catalog(
    fixture: &mut Fixture,
    context: &str,
    ready: impl Fn(&protocol::LaunchProfileCatalog) -> bool,
) -> protocol::LaunchProfileCatalog {
    if ready(&fixture.bootstrap.launch_profile_catalog) {
        return fixture.bootstrap.launch_profile_catalog.clone();
    }
    let mut catalog = None;
    next_frame_matching_on(&mut fixture.client, context, |env| {
        if env.kind != FrameKind::LaunchProfileCatalogNotify {
            return false;
        }
        let payload: protocol::LaunchProfileCatalogPayload = env
            .parse_payload()
            .expect("parse LaunchProfileCatalogNotify");
        if !ready(&payload.catalog) {
            return false;
        }
        catalog = Some(payload.catalog);
        true
    })
    .await;
    catalog.expect("matched catalog")
}

/// A custom ACP profile discovers its model catalog from its own agent
/// command, and the server validates the profile against that catalog, not the
/// default Kiro one. The launch profile editor can only offer the options the
/// profile is held to if the server emits that per-profile schema; otherwise
/// the profile's real model reads as unavailable and the editor offers models
/// the profile's agent rejects.
#[tokio::test]
async fn custom_acp_profile_schema_is_emitted_for_its_editor() {
    let mut fixture = Fixture::new_with_runtime_config_and_settings_file(
        server::HostRuntimeConfig {
            mock_backend_discovery: [(
                BackendKind::Kiro,
                server::backend::mock::MockDiscovery::new(vec![
                    Ok(kiro_model_discovery(&["kiro-default-model"])),
                    Ok(kiro_model_discovery(&["custom-a", "custom-b"])),
                ]),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        r#"{
  "settings": {
    "enabled_backends": ["kiro"],
    "default_backend": "kiro",
    "launch_profiles": {
      "kiro:custom": {
        "id": "kiro:custom",
        "label": "Custom ACP",
        "backend_kind": "kiro",
        "session_settings": { "model": { "string": "custom-a" } },
        "acp": { "command": "custom-acp-agent" }
      }
    }
  }
}"#,
    )
    .await;

    let catalog = wait_for_launch_profile_catalog(
        &mut fixture,
        "custom ACP profile ready with its own schema",
        |catalog| {
            profile_is_ready(catalog, "kiro:custom")
                && custom_profile_models(catalog, "kiro:custom").is_some()
        },
    )
    .await;
    assert_eq!(
        custom_profile_models(&catalog, "kiro:custom"),
        Some(vec!["custom-a".to_owned(), "custom-b".to_owned()]),
        "the profile's editor schema must be the catalog its own agent reported, \
         not the default Kiro catalog"
    );

    let profile = |model: &str| {
        serde_json::json!({
            "id": "kiro:custom",
            "label": "Custom ACP",
            "backend_kind": "kiro",
            "session_settings": { "model": { "string": model } },
            "acp": { "command": "custom-acp-agent", "args": [], "adapter": "stock" }
        })
    };
    send_settings_write(
        &mut fixture.client,
        "w-custom-acp-model",
        vec![replace_op(
            "/launch_profiles/kiro:custom",
            profile("custom-b"),
            profile("custom-a"),
        )],
    )
    .await;
    let result = expect_settings_write_result(
        &mut fixture.client,
        "w-custom-acp-model",
        "custom ACP profile model edit",
    )
    .await;
    assert!(result.applied, "{:?}", result.field_errors);
    let catalog = wait_for_launch_profile_catalog(
        &mut fixture,
        "custom ACP profile edited to another model its agent offers",
        |catalog| {
            catalog.entries.iter().any(|entry| match entry {
                protocol::LaunchProfileEntry::Ready { profile } => {
                    profile.id.0 == "kiro:custom"
                        && profile.session_settings.0.get("model")
                            == Some(&SessionSettingValue::String("custom-b".to_owned()))
                }
                _ => false,
            })
        },
    )
    .await;
    assert_eq!(
        custom_profile_models(&catalog, "kiro:custom"),
        Some(vec!["custom-a".to_owned(), "custom-b".to_owned()]),
        "a model picked from the emitted profile schema keeps the profile ready"
    );
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

#[tokio::test]
async fn delegation_preference_normalizes_only_invalid_and_duplicate_ids() {
    let mut fixture = Fixture::new().await;
    let defaults = settings_model::default_delegation_launch_profile_order();
    send_settings_write(
        &mut fixture.client,
        "w-delegation-order",
        vec![replace_op(
            "/delegation_launch_profile_order",
            serde_json::json!([
                " missing:saved ",
                "claude:default",
                "missing:saved",
                "",
                "bad\nprofile",
                "codex:default"
            ]),
            serde_json::to_value(defaults).expect("serialize default preference"),
        )],
    )
    .await;
    let fanout = expect_host_settings_frame(&mut fixture.client, "delegation order fanout").await;
    assert_eq!(
        fanout.settings.delegation_launch_profile_order,
        vec![
            protocol::LaunchProfileId("missing:saved".to_owned()),
            protocol::LaunchProfileId("claude:default".to_owned()),
            protocol::LaunchProfileId("codex:default".to_owned()),
        ],
        "missing profile ids survive while whitespace, invalid ids, and later duplicates normalize"
    );
    let result = expect_settings_write_result(
        &mut fixture.client,
        "w-delegation-order",
        "delegation order result",
    )
    .await;
    assert!(result.applied, "{:?}", result.field_errors);

    let (_, replay) = fixture.connect_fresh_host_with_bootstrap().await;
    assert_eq!(
        replay.settings.delegation_launch_profile_order,
        fanout.settings.delegation_launch_profile_order,
        "the normalized order must persist across a real host restart"
    );
}

fn codex_schema_model(schema: &protocol::SessionSettingsSchema) -> Vec<String> {
    schema
        .fields
        .iter()
        .filter(|field| field.key == "model")
        .flat_map(|field| match &field.field_type {
            protocol::SessionSettingFieldType::Select { options, .. } => options
                .iter()
                .map(|option| option.value.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

async fn expect_agent_session_settings(
    client: &mut client::Connection,
    stream: &protocol::StreamPath,
    context: &str,
) -> protocol::SessionSettingsPayload {
    fixture::next_logical_frame_matching_on(client, context, |env| {
        &env.stream == stream && env.kind == FrameKind::SessionSettings
    })
    .await
    .parse_payload()
    .unwrap_or_else(|err| panic!("parse SessionSettings for {context}: {err}"))
}

/// A running agent validates session-setting edits against the schema it
/// started with, not the host's current catalog. The agent's settings carry
/// that schema, so a client renders what this agent accepts: after the
/// catalog drops `gpt-old` and gains `gpt-new`, an unrelated edit keeps the
/// agent's `gpt-old`, and `gpt-new` is rejected — exactly as the emitted
/// schema says.
#[tokio::test]
async fn running_agent_settings_carry_the_schema_the_agent_validates() {
    let mut fixture = Fixture::new_with_runtime_config_and_settings_file(
        server::HostRuntimeConfig {
            mock_backend_discovery: [(
                BackendKind::Codex,
                server::backend::mock::MockDiscovery::new(vec![
                    Ok(mock_model_discovery("gpt-old", &["low", "high"])),
                    Ok(mock_model_discovery("gpt-new", &["low", "high"])),
                ]),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        r#"{
  "settings": {
    "enabled_backends": ["codex"],
    "default_backend": "codex"
  }
}"#,
    )
    .await;
    wait_for_codex_catalog(&mut fixture, "gpt-old").await;
    let values = |model: &str, effort: &str| {
        protocol::SessionSettingsValues(
            [
                (
                    "model".to_owned(),
                    SessionSettingValue::String(model.to_owned()),
                ),
                (
                    "reasoning_effort".to_owned(),
                    SessionSettingValue::String(effort.to_owned()),
                ),
            ]
            .into_iter()
            .collect(),
        )
    };
    let (agent, _) = fixture
        .spawn_with(protocol::SpawnAgentPayload {
            name: Some("Catalog drift".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: protocol::SpawnAgentParams::New {
                workspace_roots: vec!["/tmp".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: protocol::BackendAccessMode::Unrestricted,
                session_settings: Some(values("gpt-old", "low")),
            },
        })
        .await;
    let started =
        expect_agent_session_settings(&mut fixture.client, &agent.stream, "started settings").await;
    assert_eq!(started.values, values("gpt-old", "low"));
    let agent_schema = started
        .schema
        .expect("a running agent's settings carry its session schema");
    assert_eq!(codex_schema_model(&agent_schema), vec!["gpt-old"]);

    send_settings_write(
        &mut fixture.client,
        "w-enable-claude",
        vec![replace_op(
            "/enabled_backends",
            serde_json::json!(["codex", "claude"]),
            serde_json::json!(["codex"]),
        )],
    )
    .await;
    let result =
        expect_settings_write_result(&mut fixture.client, "w-enable-claude", "refresh catalog")
            .await;
    assert!(result.applied, "{:?}", result.field_errors);
    wait_for_codex_catalog(&mut fixture, "gpt-new").await;

    fixture
        .client
        .set_session_settings(
            &agent.stream,
            protocol::SetSessionSettingsPayload {
                values: values("gpt-old", "high"),
            },
        )
        .await
        .expect("send unrelated edit");
    let edited =
        expect_agent_session_settings(&mut fixture.client, &agent.stream, "unrelated edit").await;
    assert_eq!(
        edited.values,
        values("gpt-old", "high"),
        "an unrelated edit keeps the agent's model even though the host catalog dropped it"
    );
    assert_eq!(
        edited.schema.as_ref(),
        Some(&agent_schema),
        "the agent's schema does not follow the host catalog"
    );

    fixture
        .client
        .set_session_settings(
            &agent.stream,
            protocol::SetSessionSettingsPayload {
                values: values("gpt-new", "high"),
            },
        )
        .await
        .expect("send catalog-only model");
    let error = next_frame_matching_on(&mut fixture.client, "rejected model", |env| {
        env.stream == agent.stream && env.kind == FrameKind::AgentError
    })
    .await
    .parse_payload::<protocol::AgentErrorPayload>()
    .expect("parse AgentError");
    assert!(
        error.message.contains("invalid session setting 'model'"),
        "a model outside the agent's emitted schema is rejected: {error:?}"
    );
    let reverted =
        expect_agent_session_settings(&mut fixture.client, &agent.stream, "rejected echo").await;
    assert_eq!(reverted.values, values("gpt-old", "high"));
    assert_eq!(reverted.schema.as_ref(), Some(&agent_schema));
}
