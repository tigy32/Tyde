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

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("settings.json"))
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
        HostSettingValue::TydeDebugMcpEnabled { enabled } => {
            settings.tyde_debug_mcp_enabled = enabled;
        }
    }

    Ok(())
}

fn empty_settings() -> HostSettings {
    HostSettings {
        enabled_backends: Vec::new(),
        default_backend: None,
        tyde_debug_mcp_enabled: false,
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

    Ok(HostSettings {
        enabled_backends,
        default_backend: settings.default_backend,
        tyde_debug_mcp_enabled: settings.tyde_debug_mcp_enabled,
    })
}

fn normalize_backend_list(backends: Vec<BackendKind>) -> Vec<BackendKind> {
    CANONICAL_BACKENDS
        .into_iter()
        .filter(|kind| backends.contains(kind))
        .collect()
}
