use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::BTreeMap, mem};

use protocol::{BackendKind, BrokerUrl, KIRO_BACKEND, LEGACY_ACP_BACKEND, LaunchProfileId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use settings_model::{
    CodeIntelSettings, HostLaunchProfileConfig, HostSettings,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
    SUPERVISOR_RETRY_ATTEMPTS_MIN, SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX,
    SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN, TYDE_AGENT_CONTROL_MAX_DEPTH_MAX,
    TYDE_AGENT_CONTROL_MAX_DEPTH_MIN,
};

const CANONICAL_BACKENDS: [BackendKind; 5] = [
    BackendKind::Kiro,
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Antigravity,
    BackendKind::Hermes,
];

/// Preference order for choosing the initial default backend when seeding a
/// brand-new install. Most capable / most widely used first.
const DEFAULT_BACKEND_PREFERENCE: [BackendKind; 5] = [
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Antigravity,
    BackendKind::Hermes,
    BackendKind::Kiro,
];

/// The agent spec a migrated Kiro launch profile receives.
///
/// An empty command is intentional: the Kiro adapter resolves `kiro-cli-chat`
/// as a sibling of `kiro-cli`, which is more reliable than whatever absolute
/// path happened to be correct when the profile was first written.
fn kiro_agent_spec_value() -> Value {
    serde_json::json!({
        "command": "",
        "args": ["acp"],
        "adapter": "kiro",
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    settings: HostSettings,
}

/// The random per-host key secret version tokens are derived with. Never
/// printed: a leaked key would let an attacker verify guessed secret values
/// against published tokens.
struct SecretTokenKey([u8; 32]);

impl std::fmt::Debug for SecretTokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretTokenKey(..)")
    }
}

#[derive(Debug)]
pub struct HostSettingsStore {
    path: PathBuf,
    secret_key: std::sync::OnceLock<SecretTokenKey>,
}

impl HostSettingsStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        // Order matters. Each migration below round-trips the file through
        // typed `HostSettings`, which rejects any legacy kind still present —
        // so every raw rename has to happen before the first typed pass.
        // `migrate_legacy_kiro_settings` is a pure JSON rewrite for exactly
        // that reason. It must also precede `read_from_disk`, which strips
        // unrecognized backend kinds and would drop "kiro" rather than rename
        // it.
        Self::migrate_legacy_kiro_settings(&path)?;
        Self::migrate_legacy_gemini_settings(&path)?;
        Self::migrate_launch_profiles_to_map(&path)?;
        let _ = Self::read_from_disk(&path)?;
        Ok(Self {
            path,
            secret_key: std::sync::OnceLock::new(),
        })
    }

    /// The persistent random key secret version tokens are keyed with.
    /// Created lazily beside the settings store on first use (no secret
    /// settings exist → the file is never created), stable across restarts
    /// so tokens survive them without spurious conflicts.
    pub fn secret_token_key(&self) -> Result<[u8; 32], String> {
        if let Some(key) = self.secret_key.get() {
            return Ok(key.0);
        }
        let key_path = self
            .path
            .parent()
            .ok_or_else(|| format!("Settings store path has no parent: {}", self.path.display()))?
            .join("settings.secret-token.key");
        let key = match std::fs::read_to_string(&key_path) {
            Ok(contents) => parse_secret_token_key(contents.trim())
                .ok_or_else(|| format!("Corrupt secret token key file {}", key_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut fresh = [0_u8; 32];
                fresh[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                fresh[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                let encoded: String = fresh.iter().map(|byte| format!("{byte:02x}")).collect();
                std::fs::create_dir_all(key_path.parent().expect("key path has a parent"))
                    .map_err(|err| format!("Failed to create settings directory: {err}"))?;
                let tmp_path = key_path.with_extension("key.tmp");
                std::fs::write(&tmp_path, &encoded)
                    .map_err(|err| format!("Failed to write secret token key: {err}"))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|err| format!("Failed to restrict secret token key: {err}"))?;
                }
                std::fs::rename(&tmp_path, &key_path)
                    .map_err(|err| format!("Failed to install secret token key: {err}"))?;
                fresh
            }
            Err(err) => {
                return Err(format!(
                    "Failed to read secret token key {}: {err}",
                    key_path.display()
                ));
            }
        };
        let _ = self.secret_key.set(SecretTokenKey(key));
        Ok(self.secret_key.get().expect("secret key just set").0)
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

    /// First-run convenience: when no settings file exists yet, enable every
    /// backend that is already installed on this host and pick a sensible
    /// default, so a brand-new user can start chatting immediately instead of
    /// landing on an empty backend list and a silently broken "New Chat".
    ///
    /// Deliberately a no-op once a settings file exists (a user who turns every
    /// backend off is respected) and when nothing is installed (the install is
    /// left fresh so a later launch can seed once a CLI is installed). Returns
    /// `true` only when it actually seeded.
    pub fn seed_installed_backends_if_fresh(
        &self,
        installed: &[BackendKind],
    ) -> Result<bool, String> {
        if self.path.exists() {
            return Ok(false);
        }
        let enabled = normalize_backend_list(installed.to_vec());
        if enabled.is_empty() {
            return Ok(false);
        }
        let default_backend = DEFAULT_BACKEND_PREFERENCE
            .into_iter()
            .find(|kind| enabled.contains(kind));
        let mut settings = empty_settings();
        settings.enabled_backends = enabled;
        settings.default_backend = default_backend;
        Self::save(&self.path, &settings)?;
        Ok(true)
    }

    /// Replaces the whole settings document (the generic `SettingsWrite`
    /// path). The candidate runs through the same validation/normalization
    /// as a load, so an invalid document is rejected with nothing written.
    pub fn replace(&self, settings: HostSettings) -> Result<HostSettings, String> {
        let settings = validate_settings(settings)?;
        Self::save(&self.path, &settings)?;
        Ok(settings)
    }

    fn migrate_legacy_gemini_settings(path: &Path) -> Result<(), String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(format!(
                    "Failed to read settings store {}: {err}",
                    path.display()
                ));
            }
        };
        let mut value = serde_json::from_str::<Value>(&contents)
            .map_err(|err| format!("Failed to parse settings store {}: {err}", path.display()))?;
        let settings = value
            .get_mut("settings")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                format!(
                    "Failed to migrate settings store {}: settings must be an object",
                    path.display()
                )
            })?;

        let mut changed = false;
        let mut migrated_to_antigravity = false;
        if let Some(enabled) = settings
            .get_mut("enabled_backends")
            .and_then(Value::as_array_mut)
        {
            for backend in enabled {
                if backend.as_str() == Some("gemini") {
                    *backend = Value::String("antigravity".to_string());
                    changed = true;
                    migrated_to_antigravity = true;
                }
            }
        }

        let mut ensure_antigravity_enabled = false;
        if settings.get("default_backend").and_then(Value::as_str) == Some("gemini") {
            settings.insert(
                "default_backend".to_string(),
                Value::String("antigravity".to_string()),
            );
            ensure_antigravity_enabled = true;
            changed = true;
            migrated_to_antigravity = true;
        }
        if ensure_antigravity_enabled {
            let enabled = settings
                .entry("enabled_backends".to_string())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| {
                    format!(
                        "Failed to migrate settings store {}: enabled_backends must be an array",
                        path.display()
                    )
                })?;
            if !enabled
                .iter()
                .any(|backend| backend.as_str() == Some("antigravity"))
            {
                enabled.push(Value::String("antigravity".to_string()));
                changed = true;
            }
        }

        let tiers_enabled = settings
            .get("complexity_tiers_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let configs = settings
            .entry("backend_tier_configs".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| {
                format!(
                    "Failed to migrate settings store {}: backend_tier_configs must be an object",
                    path.display()
                )
            })?;
        if configs.remove("gemini").is_some() {
            changed = true;
            migrated_to_antigravity = true;
        }
        if tiers_enabled && migrated_to_antigravity && !configs.contains_key("antigravity") {
            configs.insert(
                "antigravity".to_string(),
                serde_json::to_value(crate::backend::builtin_tier_config(
                    BackendKind::Antigravity,
                ))
                .map_err(|err| {
                    format!(
                        "Failed to serialize Antigravity tier defaults while migrating settings store {}: {err}",
                        path.display()
                    )
                })?,
            );
            changed = true;
        }

        if changed {
            let store = serde_json::from_value::<StoreFile>(value).map_err(|err| {
                format!(
                    "Failed to parse migrated settings store {}: {err}",
                    path.display()
                )
            })?;
            let settings = validate_settings(store.settings).map_err(|err| {
                format!("Invalid migrated settings store {}: {err}", path.display())
            })?;
            Self::save(path, &settings)?;
        }
        Ok(())
    }

    /// Normalize the Kiro backend kind onto its canonical `kiro` spelling.
    ///
    /// Two legacy spellings exist. `kiro` is what the kind was called before it
    /// was renamed for the protocol it speaks; `acp` is what it was called
    /// after. `kiro` is canonical again, so the `acp` spelling is rewritten
    /// everywhere it was persisted: the enabled list, the default, the
    /// tier-config and backend-config maps (keyed by kind), and any user launch
    /// profile that targeted it.
    ///
    /// A profile still on the older `kiro` spelling gains the Kiro agent spec,
    /// because it predates the spec and a Kiro profile without one fails
    /// validation.
    fn migrate_legacy_kiro_settings(path: &Path) -> Result<(), String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(format!(
                    "Failed to read settings store {}: {err}",
                    path.display()
                ));
            }
        };
        let mut value = serde_json::from_str::<Value>(&contents)
            .map_err(|err| format!("Failed to parse settings store {}: {err}", path.display()))?;
        let Some(settings) = value.get_mut("settings").and_then(Value::as_object_mut) else {
            // An unreadable shape is the Gemini migration's problem to report;
            // there is nothing here to rename.
            return Ok(());
        };

        let mut changed = false;

        if let Some(enabled) = settings
            .get_mut("enabled_backends")
            .and_then(Value::as_array_mut)
        {
            for backend in enabled.iter_mut() {
                if backend.as_str() == Some(LEGACY_ACP_BACKEND) {
                    *backend = Value::String(KIRO_BACKEND.to_string());
                    changed = true;
                }
            }
            // `acp` and `kiro` could both be present if a profile was already
            // added by hand; collapse the duplicate rather than persisting it.
            let mut seen = std::collections::HashSet::new();
            let before = enabled.len();
            enabled.retain(|backend| match backend.as_str() {
                Some(name) => seen.insert(name.to_string()),
                None => true,
            });
            changed |= enabled.len() != before;
        }

        if settings.get("default_backend").and_then(Value::as_str) == Some(LEGACY_ACP_BACKEND) {
            settings.insert(
                "default_backend".to_string(),
                Value::String(KIRO_BACKEND.to_string()),
            );
            changed = true;
        }

        for map_key in ["backend_tier_configs", "backend_config"] {
            if let Some(map) = settings.get_mut(map_key).and_then(Value::as_object_mut)
                && let Some(newer) = map.remove(LEGACY_ACP_BACKEND)
            {
                // Precedence is the other way round from the previous rename:
                // an `acp` entry is the *newer* of the two spellings, so it
                // wins over a `kiro` entry left behind by a build old enough to
                // predate the first rename.
                map.insert(KIRO_BACKEND.to_string(), newer);
                changed = true;
            }
        }

        if let Some(profiles) = settings.get_mut("launch_profiles") {
            let profiles: Vec<&mut Value> = match profiles {
                Value::Array(profiles) => profiles.iter_mut().collect(),
                Value::Object(profiles) => profiles.values_mut().collect(),
                _ => Vec::new(),
            };
            for profile in profiles {
                let Some(profile) = profile.as_object_mut() else {
                    continue;
                };
                // Same split as the session store: `acp` needs only the
                // spelling fixed, while `kiro` predates the agent spec and is
                // the shape that needs one synthesized.
                match profile.get("backend_kind").and_then(Value::as_str) {
                    Some(LEGACY_ACP_BACKEND) => {
                        profile.insert(
                            "backend_kind".to_string(),
                            Value::String(KIRO_BACKEND.to_string()),
                        );
                        changed = true;
                    }
                    Some(KIRO_BACKEND) => {
                        if !profile.contains_key("acp") {
                            profile
                                .entry("acp".to_string())
                                .or_insert_with(kiro_agent_spec_value);
                            changed = true;
                        }
                    }
                    _ => continue,
                }
            }
        }

        if changed {
            // Deliberately a raw write, not `save`: the document may still hold
            // other legacy kinds that a typed round-trip would reject. Later
            // migrations and `read_from_disk` do the validating.
            Self::save_raw(path, &value)?;
        }
        Ok(())
    }

    fn migrate_launch_profiles_to_map(path: &Path) -> Result<(), String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(format!(
                    "Failed to read settings store {}: {err}",
                    path.display()
                ));
            }
        };
        let mut value = serde_json::from_str::<Value>(&contents)
            .map_err(|err| format!("Failed to parse settings store {}: {err}", path.display()))?;
        let Some(profiles) = value
            .get_mut("settings")
            .and_then(Value::as_object_mut)
            .and_then(|settings| settings.get_mut("launch_profiles"))
        else {
            return Ok(());
        };
        let Value::Array(entries) = profiles else {
            return Ok(());
        };

        let entries = mem::take(entries);
        let mut keyed = serde_json::Map::new();
        for (index, profile) in entries.into_iter().enumerate() {
            let id = profile
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "Failed to migrate settings store {}: launch_profiles[{index}] has no valid id",
                        path.display()
                    )
                })?
                .to_owned();
            if keyed.insert(id.clone(), profile).is_some() {
                return Err(format!(
                    "Failed to migrate settings store {}: duplicate launch profile id {id}",
                    path.display()
                ));
            }
        }
        *profiles = Value::Object(keyed);
        Self::save_raw(path, &value)
    }

    /// Write a settings document verbatim, without validating it as typed
    /// `HostSettings`. Only migrations that run before validation use this.
    fn save_raw(path: &Path, value: &Value) -> Result<(), String> {
        let json = serde_json::to_string_pretty(value)
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
        std::fs::rename(&tmp_path, path)
            .map_err(|err| format!("Failed to replace settings store: {err}"))?;
        Ok(())
    }

    fn read_from_disk(path: &Path) -> Result<HostSettings, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let mut value =
                    serde_json::from_str::<serde_json::Value>(&contents).map_err(|err| {
                        format!("Failed to parse settings store {}: {err}", path.display())
                    })?;
                // Other builds/branches may know backend kinds this build
                // doesn't yet. Skip those entries instead of refusing to
                // load the whole file. A later save rewrites the file
                // without them — acceptable loss; everything else survives.
                let skipped = strip_unknown_backend_kinds(&mut value);
                if !skipped.is_empty() {
                    tracing::warn!(
                        "Settings store {} references backend kinds unknown to this build; skipped: {}",
                        path.display(),
                        skipped.join(", ")
                    );
                }
                let store = serde_json::from_value::<StoreFile>(value).map_err(|err| {
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

/// Removes backend kinds this build doesn't know from everywhere they can
/// appear in a raw settings file, returning a description of each skipped
/// entry. Works on the raw JSON rather than `BackendKind` so a fake
/// "unknown" variant never has to leak into that widely-used enum. An
/// unknown `default_backend` becomes null; `validate_settings` then
/// re-normalizes the result as usual.
fn strip_unknown_backend_kinds(value: &mut serde_json::Value) -> Vec<String> {
    let mut skipped = Vec::new();
    let Some(settings) = value.get_mut("settings") else {
        return skipped;
    };
    if let Some(entries) = settings
        .get_mut("enabled_backends")
        .and_then(serde_json::Value::as_array_mut)
    {
        entries.retain(|entry| {
            let known = is_known_backend_kind(entry);
            if !known {
                skipped.push(format!("enabled_backends entry {entry}"));
            }
            known
        });
    }
    if let Some(default) = settings.get_mut("default_backend")
        && !default.is_null()
        && !is_known_backend_kind(default)
    {
        skipped.push(format!("default_backend {default}"));
        *default = serde_json::Value::Null;
    }
    if let Some(configs) = settings
        .get_mut("backend_tier_configs")
        .and_then(serde_json::Value::as_object_mut)
    {
        configs.retain(|key, _| {
            let known = is_known_backend_kind(&serde_json::Value::String(key.clone()));
            if !known {
                skipped.push(format!("backend_tier_configs key \"{key}\""));
            }
            known
        });
    }
    if let Some(configs) = settings
        .get_mut("backend_config")
        .and_then(serde_json::Value::as_object_mut)
    {
        configs.retain(|key, _| {
            let known = is_known_backend_kind(&serde_json::Value::String(key.clone()));
            if !known {
                skipped.push(format!("backend_config key \"{key}\""));
            }
            known
        });
    }
    if let Some(profiles) = settings.get_mut("launch_profiles") {
        match profiles {
            Value::Array(profiles) => {
                profiles.retain(|profile| launch_profile_backend_is_known(profile, &mut skipped))
            }
            Value::Object(profiles) => {
                profiles.retain(|_, profile| launch_profile_backend_is_known(profile, &mut skipped))
            }
            _ => {}
        }
    }
    skipped
}

fn launch_profile_backend_is_known(profile: &Value, skipped: &mut Vec<String>) -> bool {
    let Some(backend) = profile.get("backend_kind") else {
        return true;
    };
    let known = is_known_backend_kind(backend);
    if !known {
        skipped.push(format!("launch_profiles backend_kind {backend}"));
    }
    known
}

fn is_known_backend_kind(value: &serde_json::Value) -> bool {
    serde_json::from_value::<BackendKind>(value.clone()).is_ok()
}

fn empty_settings() -> HostSettings {
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
        launch_profiles: BTreeMap::new(),
        hermes_disabled_providers: std::collections::HashMap::new(),
        voice: Default::default(),
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

    if !(TYDE_AGENT_CONTROL_MAX_DEPTH_MIN..=TYDE_AGENT_CONTROL_MAX_DEPTH_MAX)
        .contains(&settings.tyde_agent_control_max_depth)
    {
        return Err(format!(
            "Tyde sub-agent depth must be between {} and {}",
            TYDE_AGENT_CONTROL_MAX_DEPTH_MIN, TYDE_AGENT_CONTROL_MAX_DEPTH_MAX,
        ));
    }

    if !(SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN
        ..=SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX)
        .contains(&settings.supervisor.auto_compact_inactivity_delay_seconds)
    {
        return Err(format!(
            "supervisor auto-compact inactivity delay must be between {} and {} seconds",
            SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN,
            SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX,
        ));
    }

    if !(SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN..=SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX)
        .contains(&settings.supervisor.stall_timeout_seconds)
    {
        return Err(format!(
            "supervisor stall timeout must be between {} and {} seconds",
            SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN, SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX,
        ));
    }

    if settings.supervisor.retry_attempts > SUPERVISOR_RETRY_ATTEMPTS_MAX {
        return Err(format!(
            "supervisor retry attempts must be between {} and {}",
            SUPERVISOR_RETRY_ATTEMPTS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
        ));
    }

    let code_intel = validate_code_intel_settings(settings.code_intel)?;
    let launch_profiles = validate_launch_profile_configs(settings.launch_profiles)?;
    // Normalize on load exactly as `apply_setting` does on write, so a
    // hand-edited store file cannot leave blank slugs or an empty list behind
    // — an empty list would read as "this profile has a disable list" in the
    // UI while disabling nothing.
    let hermes_disabled_providers = settings
        .hermes_disabled_providers
        .into_iter()
        .filter_map(|(profile, slugs)| {
            let profile = profile.trim().to_owned();
            if profile.is_empty() {
                return None;
            }
            let mut slugs: Vec<String> = slugs
                .into_iter()
                .map(|slug| slug.trim().to_owned())
                .filter(|slug| !slug.is_empty())
                .collect();
            slugs.sort();
            slugs.dedup();
            (!slugs.is_empty()).then_some((profile, slugs))
        })
        .collect();

    // Sanitize each backend's persisted deep config against its current schema
    // so a value that is no longer valid (renamed key, changed options) is
    // dropped on load rather than surfacing at spawn time.
    let backend_config = settings
        .backend_config
        .into_iter()
        .filter_map(|(backend, values)| {
            let sanitized = crate::backend::sanitize_backend_config_values(backend, &values);
            (!sanitized.0.is_empty()).then_some((backend, sanitized))
        })
        .collect();

    let mut voice = settings.voice;
    voice.aws_profile = normalize_optional_voice_setting(voice.aws_profile, "AWS profile")?;
    voice.aws_region = normalize_optional_voice_setting(voice.aws_region, "AWS region")?;
    voice.dictation_region =
        normalize_optional_voice_setting(voice.dictation_region, "AWS Transcribe region")?;
    voice.dictation_language_code =
        validate_dictation_language_code(&voice.dictation_language_code)?.to_owned();
    voice.nova_model = validate_voice_model(&voice.nova_model)?.to_owned();
    // Availability is runtime-derived and must never be trusted from disk.
    voice.availability = if !voice.enabled {
        protocol::VoiceAvailability::Unavailable {
            reason: protocol::VoiceUnavailableReason::NotEnabled,
        }
    } else if voice.aws_region.is_none() {
        protocol::VoiceAvailability::Unavailable {
            reason: protocol::VoiceUnavailableReason::RegionNotConfigured,
        }
    } else {
        protocol::VoiceAvailability::Available
    };
    voice.dictation_availability = if !voice.dictation_enabled {
        protocol::VoiceAvailability::Unavailable {
            reason: protocol::VoiceUnavailableReason::NotEnabled,
        }
    } else if voice.dictation_region.is_none() {
        protocol::VoiceAvailability::Unavailable {
            reason: protocol::VoiceUnavailableReason::RegionNotConfigured,
        }
    } else {
        protocol::VoiceAvailability::Available
    };

    Ok(HostSettings {
        enabled_backends,
        default_backend: settings.default_backend,
        enable_mobile_connections: settings.enable_mobile_connections,
        mobile_broker_url: settings.mobile_broker_url,
        mobile_broker_auth: settings.mobile_broker_auth,
        tyde_debug_mcp_enabled: settings.tyde_debug_mcp_enabled,
        tyde_agent_control_mcp_enabled: settings.tyde_agent_control_mcp_enabled,
        tyde_agent_control_max_depth: settings.tyde_agent_control_max_depth,
        complexity_tiers_enabled: settings.complexity_tiers_enabled,
        backend_tier_configs: settings.backend_tier_configs,
        background_agent_features: settings.background_agent_features,
        supervisor: settings.supervisor,
        code_intel,
        backend_config,
        launch_profiles,
        hermes_disabled_providers,
        voice,
    })
}

fn normalize_optional_voice_setting(
    value: Option<String>,
    name: &str,
) -> Result<Option<String>, String> {
    let Some(value) = value else { return Ok(None) };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 256 || value.chars().any(char::is_control) {
        return Err(format!("{name} is invalid"));
    }
    Ok(Some(value.to_owned()))
}

fn validate_voice_model(model: &str) -> Result<&str, String> {
    match model.trim() {
        "amazon.nova-2-sonic-v1:0" | "amazon.nova-sonic-v1:0" => Ok(model.trim()),
        _ => Err("unsupported Nova Sonic model".to_owned()),
    }
}

fn validate_dictation_language_code(code: &str) -> Result<&str, String> {
    let code = code.trim();
    let bytes = code.as_bytes();
    if bytes.len() == 5
        && bytes[0..2].iter().all(u8::is_ascii_lowercase)
        && bytes[2] == b'-'
        && bytes[3..5].iter().all(u8::is_ascii_uppercase)
    {
        Ok(code)
    } else {
        Err("AWS Transcribe language code must use a value such as en-US".to_owned())
    }
}

fn validate_code_intel_settings(settings: CodeIntelSettings) -> Result<CodeIntelSettings, String> {
    for (provider, path) in &settings.language_server_paths {
        if path.0.trim().is_empty() {
            return Err(format!(
                "code-intel language server path for {provider} must not be empty"
            ));
        }
    }
    Ok(settings)
}

fn validate_launch_profile_configs(
    profiles: BTreeMap<LaunchProfileId, HostLaunchProfileConfig>,
) -> Result<BTreeMap<LaunchProfileId, HostLaunchProfileConfig>, String> {
    let mut validated = BTreeMap::new();
    for (id, profile) in profiles {
        if id != profile.id {
            return Err(format!(
                "launch profile map key {id} does not match embedded id {}",
                profile.id
            ));
        }
        if profile.id.0.trim().is_empty() {
            return Err("launch profile id must not be empty".to_owned());
        }
        if profile.label.trim().is_empty() {
            return Err(format!(
                "launch profile {} label must not be empty",
                profile.id
            ));
        }
        if CANONICAL_BACKENDS.into_iter().any(|backend| {
            profile.id == LaunchProfileId(format!("{}:default", backend_slug(backend)))
        }) {
            return Err(format!(
                "launch profile {} conflicts with a reserved default profile id",
                profile.id
            ));
        }
        if profile.id.0 == protocol::KIRO_LAUNCH_PROFILE_ID {
            return Err(format!(
                "launch profile {} conflicts with the built-in Kiro agent profile",
                profile.id
            ));
        }
        if profile
            .id
            .0
            .starts_with(crate::host::HERMES_PROFILE_LAUNCH_ID_PREFIX)
        {
            return Err(format!(
                "launch profile {} conflicts with the server-synthesized Hermes profile namespace",
                profile.id
            ));
        }
        // An ACP profile is nothing without a command to run, and an agent spec
        // on a non-ACP profile would be silently ignored. Reject both rather
        // than persisting a profile that can't launch or that lies about what
        // it does.
        match (profile.backend_kind, profile.acp.as_ref()) {
            (BackendKind::Kiro, None) => {
                return Err(format!(
                    "launch profile {} targets the ACP backend but has no agent command configured",
                    profile.id
                ));
            }
            // A named adapter knows how to find its own binary (the Kiro
            // adapter resolves `kiro-cli-chat` as a sibling of `kiro-cli`), so
            // it may leave the command blank. A stock agent cannot be
            // discovered, so its command is required.
            (BackendKind::Kiro, Some(spec))
                if spec.adapter == protocol::AcpAdapterId::Stock
                    && spec.command.trim().is_empty() =>
            {
                return Err(format!(
                    "launch profile {} must specify the ACP agent command to run",
                    profile.id
                ));
            }
            (kind, Some(_)) if kind != BackendKind::Kiro => {
                return Err(format!(
                    "launch profile {} configures an ACP agent but targets {kind:?}",
                    profile.id
                ));
            }
            _ => {}
        }
        validated.insert(id, profile);
    }
    Ok(validated)
}

fn backend_slug(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Hermes => "hermes",
    }
}

fn parse_secret_token_key(encoded: &str) -> Option<[u8; 32]> {
    if encoded.len() != 64 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0_u8; 32];
    for (index, chunk) in encoded.as_bytes().chunks(2).enumerate() {
        key[index] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(key)
}

pub(crate) fn validate_mobile_broker_url_for_write(
    broker_url: Option<&BrokerUrl>,
) -> Result<(), String> {
    let Some(url) = broker_url else {
        return Ok(());
    };
    if url.as_str().trim().is_empty() {
        return Err("mobile_broker_url must not be empty".to_owned());
    }
    mqtt_transport::validate_broker_url(url).map_err(|err| err.to_string())?;
    if url.as_str() == protocol::DEFAULT_MOBILE_MQTT_BROKER_URL {
        return Err(
            "the public default mobile broker is no longer supported; pair through tycode.dev"
                .to_owned(),
        );
    }
    if !is_loopback_broker_url(url) {
        return Err(
            "custom mobile broker URLs are dev/test-only; production mobile access uses tycode.dev"
                .to_owned(),
        );
    }
    Ok(())
}

fn is_loopback_broker_url(url: &BrokerUrl) -> bool {
    url::Url::parse(url.as_str())
        .ok()
        .is_some_and(|parsed| is_loopback_url(&parsed))
}

fn is_loopback_url(parsed: &url::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<IpAddr>()
                    .map(|addr| addr.is_loopback())
                    .unwrap_or(false)
        }
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        None => false,
    }
}

pub(crate) fn normalize_backend_list(backends: Vec<BackendKind>) -> Vec<BackendKind> {
    CANONICAL_BACKENDS
        .into_iter()
        .filter(|kind| backends.contains(kind))
        .collect()
}
