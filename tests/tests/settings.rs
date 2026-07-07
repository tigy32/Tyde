mod fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    BackendConfigSnapshotStatus, BackendConfigSnapshotsPayload, BackendConfigValues, BackendKind,
    BackendSetupDiagnosticCode, BackendSetupStatus, CodeIntelProviderId, FrameKind,
    HostExecutablePath, HostSettingValue, HostSettings, HostSettingsPayload, SessionId,
    SessionSettingValue, SetSettingPayload,
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

async fn expect_host_settings(
    client: &mut client::Connection,
    context: &str,
) -> HostSettingsPayload {
    loop {
        let env = client
            .next_event()
            .await
            .unwrap_or_else(|err| panic!("next_event failed before {context}: {err:?}"))
            .unwrap_or_else(|| panic!("connection closed before {context}"));
        if env.kind == FrameKind::HostSettings {
            return env
                .parse_payload()
                .unwrap_or_else(|err| panic!("failed to parse HostSettings for {context}: {err}"));
        }
    }
}

fn write_fake_tycode_binary(home: &Path) -> PathBuf {
    let path = home
        .join(".tyde")
        .join("tycode")
        .join("0.9.2-pre.1")
        .join("tycode-subprocess");
    std::fs::create_dir_all(path.parent().expect("fake Tycode parent"))
        .expect("create fake Tycode install dir");
    let settings = serde_json::json!({
        "active_provider": "native-provider",
        "providers": {
            "native-provider": { "type": "mock" }
        },
        "model_quality": "high",
        "reasoning_effort": "Max",
        "autonomy_level": "fully_autonomous",
        "review_level": "Task",
        "spawn_context_mode": "Fresh"
    });
    let settings_literal = serde_json::to_string(&settings.to_string()).expect("settings literal");
    let body = r#"#!/usr/bin/env python3
import json
import sys

if "--version" in sys.argv:
    print("tycode-subprocess 0.9.2-pre.1")
    sys.exit(0)

settings = json.loads(__SETTINGS__)
print(json.dumps({"kind":"SessionStarted","data":{"session_id":"fake-session"}}), flush=True)
for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    command = json.loads(line)
    if command == "GetSettings":
        print(json.dumps({"kind":"Settings","data":settings}), flush=True)
"#
    .replace("__SETTINGS__", &settings_literal);
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
    path
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
            code_intel: Default::default(),
            backend_config: std::collections::HashMap::new(),
            launch_profiles: Vec::new(),
        }
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

    let mut first = BackendConfigValues::default();
    first.0.insert(
        "default_model".to_owned(),
        SessionSettingValue::String("anthropic/claude-sonnet-5".to_owned()),
    );
    store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Hermes,
            values: first,
        })
        .expect("set Hermes default model");

    let mut second = BackendConfigValues::default();
    second.0.insert(
        "default_provider".to_owned(),
        SessionSettingValue::String("anthropic".to_owned()),
    );
    let settings = store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Hermes,
            values: second,
        })
        .expect("merge Hermes default provider");
    let values = settings
        .backend_config
        .get(&BackendKind::Hermes)
        .expect("Hermes backend config");
    assert_eq!(
        values.0.get("default_model"),
        Some(&SessionSettingValue::String(
            "anthropic/claude-sonnet-5".to_owned()
        ))
    );
    assert_eq!(
        values.0.get("default_provider"),
        Some(&SessionSettingValue::String("anthropic".to_owned()))
    );

    let mut clear_one = BackendConfigValues::default();
    clear_one
        .0
        .insert("default_model".to_owned(), SessionSettingValue::Null);
    let settings = store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Hermes,
            values: clear_one,
        })
        .expect("clear Hermes default model");
    let values = settings
        .backend_config
        .get(&BackendKind::Hermes)
        .expect("Hermes backend config after clear");
    assert!(!values.0.contains_key("default_model"));
    assert_eq!(
        values.0.get("default_provider"),
        Some(&SessionSettingValue::String("anthropic".to_owned()))
    );

    let settings = store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Hermes,
            values: BackendConfigValues::default(),
        })
        .expect("clear entire Hermes config");
    assert!(!settings.backend_config.contains_key(&BackendKind::Hermes));

    let mut tycode_provider = BackendConfigValues::default();
    tycode_provider.0.insert(
        "active_provider".to_owned(),
        SessionSettingValue::String("default".to_owned()),
    );
    let err = store
        .apply(HostSettingValue::BackendConfig {
            backend: BackendKind::Tycode,
            values: tycode_provider,
        })
        .expect_err("Tycode no longer stores native settings in Tyde host settings");
    assert!(
        err.contains("does not support backend configuration")
            || err.contains("not defined by its schema"),
        "unexpected Tycode backend-config store error: {err}"
    );
}

#[tokio::test]
async fn backend_config_updates_merge_through_client_events() {
    let mut fixture = Fixture::new().await;

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
        .expect("set Hermes default model");
    let settings = expect_host_settings(&mut fixture.client, "Hermes model setting").await;
    assert_eq!(
        settings
            .settings
            .backend_config
            .get(&BackendKind::Hermes)
            .and_then(|values| values.0.get("default_model")),
        Some(&SessionSettingValue::String(
            "anthropic/claude-sonnet-5".to_owned()
        ))
    );

    let mut provider = BackendConfigValues::default();
    provider.0.insert(
        "default_provider".to_owned(),
        SessionSettingValue::String("anthropic".to_owned()),
    );
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendConfig {
                backend: BackendKind::Hermes,
                values: provider,
            },
        })
        .await
        .expect("merge Hermes default provider");
    let settings = expect_host_settings(&mut fixture.client, "Hermes provider setting").await;
    let values = settings
        .settings
        .backend_config
        .get(&BackendKind::Hermes)
        .expect("Hermes backend config after merge");
    assert_eq!(
        values.0.get("default_model"),
        Some(&SessionSettingValue::String(
            "anthropic/claude-sonnet-5".to_owned()
        ))
    );
    assert_eq!(
        values.0.get("default_provider"),
        Some(&SessionSettingValue::String("anthropic".to_owned()))
    );

    let mut clear = BackendConfigValues::default();
    clear
        .0
        .insert("default_model".to_owned(), SessionSettingValue::Null);
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackendConfig {
                backend: BackendKind::Hermes,
                values: clear,
            },
        })
        .await
        .expect("clear Hermes default model");
    let settings = expect_host_settings(&mut fixture.client, "Hermes clear setting").await;
    let values = settings
        .settings
        .backend_config
        .get(&BackendKind::Hermes)
        .expect("Hermes backend config after explicit clear");
    assert!(!values.0.contains_key("default_model"));
    assert_eq!(
        values.0.get("default_provider"),
        Some(&SessionSettingValue::String("anthropic".to_owned()))
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

    let mut fixture = Fixture::new_with_real_backend_probe().await;
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
        Some("tycode-subprocess 0.9.2-pre.1")
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

    let mut fixture = Fixture::new_with_real_backend_probe().await;
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
async fn backend_config_snapshots_report_tycode_settings_schema_release_blocker() {
    let _env_guard = env_lock().lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    write_fake_tycode_binary(temp_home.path());
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());
    let _hermes_python =
        EnvVarGuard::set("HERMES_PYTHON", "/definitely/not/hermes-python".to_string());

    let mut fixture = Fixture::new_with_real_backend_probe().await;
    let payload = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("next_event while waiting for BackendConfigSnapshots")
            .expect("connection closed before BackendConfigSnapshots");
        if env.kind == FrameKind::BackendConfigSnapshots {
            break env
                .parse_payload::<BackendConfigSnapshotsPayload>()
                .expect("BackendConfigSnapshots payload");
        }
    };

    assert!(
        payload
            .snapshots
            .iter()
            .all(|snapshot| snapshot.backend_kind != BackendKind::Tycode),
        "Tycode should no longer expose the legacy hardcoded backend-config subset"
    );
    let tycode = payload
        .native_settings
        .iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Tycode)
        .expect("Tycode native settings snapshot");
    assert_eq!(tycode.status, BackendConfigSnapshotStatus::Unavailable);
    let message = tycode.message.as_deref().expect("Tycode blocker message");
    assert!(
        message.contains("GetSettingsSchema")
            && message.contains("0.9.2-pre.1")
            && message.contains("0.10.0"),
        "Tycode native settings snapshot should surface release blocker: {message}"
    );
    assert!(tycode.settings.is_none());
    assert!(tycode.groups.is_empty());
}
