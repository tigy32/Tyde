use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{BackendKind, HostSettingValue, HostSettings};
use serde::{Deserialize, Serialize};

const CANONICAL_BACKENDS: [BackendKind; 5] = [
    BackendKind::Tycode,
    BackendKind::Kiro,
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Gemini,
];

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    settings: HostSettings,
}

#[derive(Debug)]
pub struct HostSettingsStore {
    path: PathBuf,
}

impl HostSettingsStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let _ = Self::read_from_disk(&path)?;
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SETTINGS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?
            .join(".tyde")
            .join("settings.json"))
    }

    pub fn get(&self) -> Result<HostSettings, String> {
        Self::read_from_disk(&self.path)
    }

    pub fn apply(&self, setting: HostSettingValue) -> Result<HostSettings, String> {
        let mut settings = Self::read_from_disk(&self.path)?;
        apply_setting(&mut settings, setting)?;
        Self::save(&self.path, &settings)?;
        Ok(settings)
    }

    fn read_from_disk(path: &Path) -> Result<HostSettings, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let store = serde_json::from_str::<StoreFile>(&contents).map_err(|err| {
                    format!("Failed to parse settings store {}: {err}", path.display())
                })?;
                validate_settings(store.settings)
                    .map_err(|err| format!("Invalid settings store {}: {err}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(empty_settings()),
            Err(err) => Err(format!(
                "Failed to read settings store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(path: &Path, settings: &HostSettings) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            settings: settings.clone(),
        })
        .map_err(|err| format!("Failed to serialize settings store: {err}"))?;

        let parent = path
            .parent()
            .ok_or_else(|| format!("Settings store path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create settings store directory: {err}"))?;

        let tmp_path = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp settings store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp settings store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp settings store file: {err}"))?;
        std::fs::rename(&tmp_path, path).map_err(|err| {
            format!(
                "Failed to atomically replace settings store {}: {err}",
                path.display()
            )
        })?;
        Ok(())
    }
}

fn apply_setting(settings: &mut HostSettings, setting: HostSettingValue) -> Result<(), String> {
    match setting {
        HostSettingValue::EnabledBackends { enabled_backends } => {
            settings.enabled_backends = normalize_backend_list(enabled_backends);
            if settings
                .default_backend
                .is_some_and(|kind| !settings.enabled_backends.contains(&kind))
            {
                settings.default_backend = None;
            }
        }
        HostSettingValue::DefaultBackend { default_backend } => {
            if default_backend.is_some_and(|kind| !settings.enabled_backends.contains(&kind)) {
                return Err(format!(
                    "default_backend {:?} must be present in enabled_backends",
                    default_backend
                ));
            }
            settings.default_backend = default_backend;
        }
        HostSettingValue::EnableMobileConnections { enabled } => {
            settings.enable_mobile_connections = enabled;
        }
        HostSettingValue::MobileBrokerUrl { broker_url } => {
            if broker_url
                .as_ref()
                .is_some_and(|url| url.as_str().trim().is_empty())
            {
                return Err("mobile_broker_url must not be empty".to_owned());
            }
            settings.mobile_broker_url = broker_url;
        }
        HostSettingValue::TydeDebugMcpEnabled { enabled } => {
            settings.tyde_debug_mcp_enabled = enabled;
        }
        HostSettingValue::TydeAgentControlMcpEnabled { enabled } => {
            settings.tyde_agent_control_mcp_enabled = enabled;
        }
        HostSettingValue::ComplexityTiersEnabled { enabled } => {
            settings.complexity_tiers_enabled = enabled;
            // Seed editable per-backend configs from the built-in defaults so
            // the settings UI always shows the actual Low/High behavior.
            if enabled {
                for kind in CANONICAL_BACKENDS {
                    settings
                        .backend_tier_configs
                        .entry(kind)
                        .or_insert_with(|| crate::backend::builtin_tier_config(kind));
                }
            }
        }
        HostSettingValue::BackendTiers { backend, config } => {
            settings.backend_tier_configs.insert(backend, config);
        }
    }

    Ok(())
}

fn empty_settings() -> HostSettings {
    HostSettings {
        enabled_backends: Vec::new(),
        default_backend: None,
        enable_mobile_connections: false,
        mobile_broker_url: None,
        tyde_debug_mcp_enabled: false,
        tyde_agent_control_mcp_enabled: true,
        complexity_tiers_enabled: false,
        backend_tier_configs: std::collections::HashMap::new(),
    }
}

fn validate_settings(settings: HostSettings) -> Result<HostSettings, String> {
    let enabled_backends = normalize_backend_list(settings.enabled_backends);
    if settings
        .default_backend
        .is_some_and(|kind| !enabled_backends.contains(&kind))
    {
        return Err(format!(
            "default_backend {:?} must be present in enabled_backends",
            settings.default_backend
        ));
    }

    if settings
        .mobile_broker_url
        .as_ref()
        .is_some_and(|url| url.as_str().trim().is_empty())
    {
        return Err("mobile_broker_url must not be empty".to_owned());
    }

    Ok(HostSettings {
        enabled_backends,
        default_backend: settings.default_backend,
        enable_mobile_connections: settings.enable_mobile_connections,
        mobile_broker_url: settings.mobile_broker_url,
        tyde_debug_mcp_enabled: settings.tyde_debug_mcp_enabled,
        tyde_agent_control_mcp_enabled: settings.tyde_agent_control_mcp_enabled,
        complexity_tiers_enabled: settings.complexity_tiers_enabled,
        backend_tier_configs: settings.backend_tier_configs,
    })
}

fn normalize_backend_list(backends: Vec<BackendKind>) -> Vec<BackendKind> {
    CANONICAL_BACKENDS
        .into_iter()
        .filter(|kind| backends.contains(kind))
        .collect()
}

#[cfg(test)]
mod tests {
    use protocol::SessionSettingValue;

    use super::*;

    #[test]
    fn old_store_files_without_tier_fields_load_with_tiers_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":["claude"],"default_backend":"claude","enable_mobile_connections":false,"mobile_broker_url":null,"tyde_debug_mcp_enabled":false,"tyde_agent_control_mcp_enabled":true}}"#,
        )
        .expect("write legacy store file");

        let store = HostSettingsStore::load(path).expect("load legacy store");
        let settings = store.get().expect("get settings");
        assert!(!settings.complexity_tiers_enabled);
        assert!(settings.backend_tier_configs.is_empty());
    }

    #[test]
    fn enabling_complexity_tiers_seeds_builtin_configs_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            HostSettingsStore::load(dir.path().join("settings.json")).expect("load empty store");

        let settings = store
            .apply(HostSettingValue::ComplexityTiersEnabled { enabled: true })
            .expect("enable tiers");
        assert!(settings.complexity_tiers_enabled);
        let claude = settings
            .backend_tier_configs
            .get(&BackendKind::Claude)
            .expect("claude config seeded");
        assert_eq!(
            claude.low.0.get("model"),
            Some(&SessionSettingValue::String("haiku".to_string()))
        );
        assert_eq!(
            claude.high.0.get("model"),
            Some(&SessionSettingValue::String("opus".to_string()))
        );
        assert_eq!(
            claude.high.0.get("effort"),
            Some(&SessionSettingValue::String("max".to_string()))
        );

        // User edits survive a disable/enable cycle (no re-seeding over them).
        let mut edited = claude.clone();
        edited.high.0.insert(
            "model".to_string(),
            SessionSettingValue::String("fable".to_string()),
        );
        store
            .apply(HostSettingValue::BackendTiers {
                backend: BackendKind::Claude,
                config: edited.clone(),
            })
            .expect("store edited config");
        store
            .apply(HostSettingValue::ComplexityTiersEnabled { enabled: false })
            .expect("disable tiers");
        let settings = store
            .apply(HostSettingValue::ComplexityTiersEnabled { enabled: true })
            .expect("re-enable tiers");
        assert_eq!(
            settings.backend_tier_configs.get(&BackendKind::Claude),
            Some(&edited)
        );
    }
}
