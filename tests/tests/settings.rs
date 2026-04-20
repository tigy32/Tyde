mod fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    BackendKind, BackendSetupPayload, BackendSetupStatus, FrameKind, HostSettings, SessionId,
};
use server::backend::BackendSession;
use server::store::session::SessionStore;
use server::store::settings::HostSettingsStore;

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

fn write_fake_tycode_binary(home: &Path) -> PathBuf {
    let path = home
        .join(".tyde")
        .join("tycode")
        .join("0.7.3")
        .join("tycode-subprocess");
    std::fs::create_dir_all(path.parent().expect("fake Tycode parent"))
        .expect("create fake Tycode install dir");
    std::fs::write(&path, "#!/bin/sh\nprintf 'tycode-subprocess 0.7.3\\n'\n")
        .expect("write fake Tycode binary");
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

async fn expect_backend_setup(
    client: &mut client::Connection,
    context: &str,
) -> BackendSetupPayload {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if env.kind == FrameKind::BackendSetup {
            return env.parse_payload().expect("parse BackendSetupPayload");
        }
    }
}

fn expected_empty_settings() -> HostSettings {
    HostSettings {
        enabled_backends: Vec::new(),
        default_backend: None,
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
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
            enabled_backends: vec![BackendKind::Kiro, BackendKind::Claude, BackendKind::Gemini,],
            default_backend: Some(BackendKind::Claude),
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
        }
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
        .upsert_backend_session(&session, None, None, None)
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
    let _env_guard = env_lock().lock().expect("lock env guard");
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    write_fake_tycode_binary(temp_home.path());
    let _home = EnvVarGuard::set("HOME", temp_home.path().to_string_lossy().to_string());

    let mut fixture = Fixture::new().await;
    let payload = expect_backend_setup(&mut fixture.client, "BackendSetup").await;

    let tycode = payload
        .backends
        .iter()
        .find(|info| info.backend_kind == BackendKind::Tycode)
        .expect("Tycode backend setup entry");
    assert_eq!(tycode.status, BackendSetupStatus::Installed);
    assert_eq!(
        tycode.installed_version.as_deref(),
        Some("tycode-subprocess 0.7.3")
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
}
