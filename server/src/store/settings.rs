use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use protocol::{
    ACP_BACKEND, BackendKind, BackgroundAgentFeature, BrokerUrl, CodeIntelSettings,
    HostLaunchProfileConfig, HostSettingValue, HostSettings, LEGACY_KIRO_BACKEND, LaunchProfileId,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
    SUPERVISOR_RETRY_ATTEMPTS_MIN, SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX,
    SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CANONICAL_BACKENDS: [BackendKind; 6] = [
    BackendKind::Tycode,
    BackendKind::Acp,
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Antigravity,
    BackendKind::Hermes,
];

/// Preference order for choosing the initial default backend when seeding a
/// brand-new install. Most capable / most widely used first.
const DEFAULT_BACKEND_PREFERENCE: [BackendKind; 6] = [
    BackendKind::Claude,
    BackendKind::Codex,
    BackendKind::Antigravity,
    BackendKind::Hermes,
    BackendKind::Acp,
    BackendKind::Tycode,
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

#[derive(Debug)]
pub struct HostSettingsStore {
    path: PathBuf,
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

    pub fn apply(&self, setting: HostSettingValue) -> Result<HostSettings, String> {
        let mut settings = Self::read_from_disk(&self.path)?;
        apply_setting(&mut settings, setting)?;
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

    /// Rename the retired `kiro` backend kind to `acp`.
    ///
    /// Kiro stopped being a backend of its own and became the built-in
    /// `acp:kiro` launch profile. Every place the old kind was persisted is
    /// rewritten here: the enabled list, the default, the tier-config and
    /// backend-config maps (keyed by kind), and any user launch profile that
    /// targeted it.
    ///
    /// A migrated launch profile also gains the Kiro agent spec, because an
    /// `acp` profile without one fails validation.
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
                if backend.as_str() == Some(LEGACY_KIRO_BACKEND) {
                    *backend = Value::String(ACP_BACKEND.to_string());
                    changed = true;
                }
            }
            // `kiro` and `acp` could both be present if a profile was already
            // added by hand; collapse the duplicate rather than persisting it.
            let mut seen = std::collections::HashSet::new();
            let before = enabled.len();
            enabled.retain(|backend| match backend.as_str() {
                Some(name) => seen.insert(name.to_string()),
                None => true,
            });
            changed |= enabled.len() != before;
        }

        if settings.get("default_backend").and_then(Value::as_str) == Some(LEGACY_KIRO_BACKEND) {
            settings.insert(
                "default_backend".to_string(),
                Value::String(ACP_BACKEND.to_string()),
            );
            changed = true;
        }

        for map_key in ["backend_tier_configs", "backend_config"] {
            if let Some(map) = settings.get_mut(map_key).and_then(Value::as_object_mut)
                && let Some(existing) = map.remove(LEGACY_KIRO_BACKEND)
            {
                // A pre-existing `acp` entry was written deliberately by a
                // newer build; don't let the legacy value clobber it.
                map.entry(ACP_BACKEND.to_string()).or_insert(existing);
                changed = true;
            }
        }

        if let Some(profiles) = settings
            .get_mut("launch_profiles")
            .and_then(Value::as_array_mut)
        {
            for profile in profiles.iter_mut() {
                let Some(profile) = profile.as_object_mut() else {
                    continue;
                };
                if profile.get("backend_kind").and_then(Value::as_str) != Some(LEGACY_KIRO_BACKEND)
                {
                    continue;
                }
                profile.insert(
                    "backend_kind".to_string(),
                    Value::String(ACP_BACKEND.to_string()),
                );
                profile
                    .entry("acp".to_string())
                    .or_insert_with(kiro_agent_spec_value);
                changed = true;
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
            validate_mobile_broker_url_for_write(broker_url.as_ref())?;
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
                    if kind == BackendKind::Codex {
                        continue;
                    }
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
        HostSettingValue::BackendConfig { backend, values } => {
            let previous = settings.backend_config.get(&backend);
            let merged = crate::backend::merge_backend_config_update(backend, previous, &values)?;
            if merged.0.is_empty() {
                settings.backend_config.remove(&backend);
            } else {
                settings.backend_config.insert(backend, merged);
            }
        }
        HostSettingValue::BackendNativeSettings { backend, .. } => {
            return Err(format!(
                "{backend:?} native settings are owned by the backend and are not stored in Tyde host settings"
            ));
        }
        HostSettingValue::LaunchProfiles { profiles } => {
            settings.launch_profiles = validate_launch_profile_configs(profiles)?;
        }
        HostSettingValue::HermesDisabledProviders { profile, providers } => {
            let profile = profile.trim();
            if profile.is_empty() {
                return Err("hermes disabled providers need a profile name".to_owned());
            }
            let mut slugs: Vec<String> = Vec::new();
            for slug in providers {
                let slug = slug.trim().to_owned();
                if slug.is_empty() || slugs.contains(&slug) {
                    continue;
                }
                slugs.push(slug);
            }
            slugs.sort();
            // An empty list is "nothing disabled", so drop the key entirely
            // rather than persisting an empty vec that reads as configuration.
            if slugs.is_empty() {
                settings.hermes_disabled_providers.remove(profile);
            } else {
                settings
                    .hermes_disabled_providers
                    .insert(profile.to_owned(), slugs);
            }
        }
        HostSettingValue::VoiceEnabled { enabled } => settings.voice.enabled = enabled,
        HostSettingValue::VoiceAwsProfile { profile } => {
            settings.voice.aws_profile = normalize_optional_voice_setting(profile, "AWS profile")?;
        }
        HostSettingValue::VoiceAwsRegion { region } => {
            settings.voice.aws_region = normalize_optional_voice_setting(region, "AWS region")?;
        }
        HostSettingValue::VoiceNovaModel { model } => {
            settings.voice.nova_model = validate_voice_model(&model)?.to_owned();
        }
        HostSettingValue::VoiceEndpointingSensitivity { sensitivity } => {
            settings.voice.endpointing_sensitivity = sensitivity;
        }
        HostSettingValue::BackgroundAgentFeatureEnabled { feature, enabled } => match feature {
            BackgroundAgentFeature::AutoGenerateAgentNames => {
                settings.background_agent_features.auto_generate_agent_names = enabled;
            }
            BackgroundAgentFeature::AgentActivitySummaries => {
                settings.background_agent_features.agent_activity_summaries = enabled;
            }
        },
        HostSettingValue::SupervisorEnabled { enabled } => {
            settings.supervisor.enabled = enabled;
        }
        HostSettingValue::SupervisorSuperviseRestoredAgents { enabled } => {
            settings.supervisor.supervise_restored_agents = enabled;
        }
        HostSettingValue::SupervisorStallTimeoutEnabled { enabled } => {
            settings.supervisor.stall_timeout_enabled = enabled;
        }
        HostSettingValue::SupervisorStallTimeoutSeconds { seconds } => {
            if !(SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN..=SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX)
                .contains(&seconds)
            {
                return Err(format!(
                    "supervisor stall timeout must be between {} and {} seconds",
                    SUPERVISOR_STALL_TIMEOUT_SECONDS_MIN, SUPERVISOR_STALL_TIMEOUT_SECONDS_MAX,
                ));
            }
            settings.supervisor.stall_timeout_seconds = seconds;
        }
        HostSettingValue::SupervisorAutoCompactOnSuccess { enabled } => {
            settings.supervisor.auto_compact_on_success = enabled;
        }
        HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds } => {
            if !(SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN
                ..=SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX)
                .contains(&seconds)
            {
                return Err(format!(
                    "supervisor auto-compact inactivity delay must be between {} and {} seconds",
                    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN,
                    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX,
                ));
            }
            settings.supervisor.auto_compact_inactivity_delay_seconds = seconds;
        }
        HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens } => {
            settings.supervisor.auto_compact_min_context_tokens = tokens;
        }
        HostSettingValue::SupervisorMaxKicksPerTask { count } => {
            if count == 0 {
                return Err(
                    "supervisor max kicks per task must be at least 1; disable the supervisor instead of setting it to 0"
                        .to_owned(),
                );
            }
            settings.supervisor.max_kicks_per_task = count;
        }
        HostSettingValue::SupervisorRetryAttempts { count } => {
            if count > SUPERVISOR_RETRY_ATTEMPTS_MAX {
                return Err(format!(
                    "supervisor retry attempts must be between {} and {}",
                    SUPERVISOR_RETRY_ATTEMPTS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
                ));
            }
            settings.supervisor.retry_attempts = count;
        }
        HostSettingValue::SupervisorCostTier { tier } => {
            settings.supervisor.cost_tier = tier;
        }
        HostSettingValue::CodeIntelLanguageServerPath { provider, path } => match path {
            Some(path) => {
                if path.0.trim().is_empty() {
                    return Err(format!(
                        "code-intel language server path for {provider} must not be empty"
                    ));
                }
                settings
                    .code_intel
                    .language_server_paths
                    .insert(provider, path);
            }
            None => {
                settings.code_intel.language_server_paths.remove(&provider);
            }
        },
    }

    Ok(())
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
    if let Some(profiles) = settings
        .get_mut("launch_profiles")
        .and_then(serde_json::Value::as_array_mut)
    {
        profiles.retain(|profile| {
            let Some(backend) = profile.get("backend_kind") else {
                return true;
            };
            let known = is_known_backend_kind(backend);
            if !known {
                skipped.push(format!("launch_profiles backend_kind {backend}"));
            }
            known
        });
    }
    skipped
}

fn is_known_backend_kind(value: &serde_json::Value) -> bool {
    serde_json::from_value::<BackendKind>(value.clone()).is_ok()
}

/// A settings value with every field at its unset default, for tests that need
/// to build one field without spelling out the rest.
#[cfg(test)]
pub(crate) fn empty_settings_for_test() -> HostSettings {
    empty_settings()
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
        background_agent_features: Default::default(),
        supervisor: Default::default(),
        code_intel: Default::default(),
        backend_config: std::collections::HashMap::new(),
        launch_profiles: Vec::new(),
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

    Ok(HostSettings {
        enabled_backends,
        default_backend: settings.default_backend,
        enable_mobile_connections: settings.enable_mobile_connections,
        mobile_broker_url: settings.mobile_broker_url,
        tyde_debug_mcp_enabled: settings.tyde_debug_mcp_enabled,
        tyde_agent_control_mcp_enabled: settings.tyde_agent_control_mcp_enabled,
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
    profiles: Vec<HostLaunchProfileConfig>,
) -> Result<Vec<HostLaunchProfileConfig>, String> {
    let mut seen = std::collections::HashSet::<LaunchProfileId>::new();
    let mut validated = Vec::with_capacity(profiles.len());
    for profile in profiles {
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
        if !seen.insert(profile.id.clone()) {
            return Err(format!("duplicate launch profile id {}", profile.id));
        }
        // An ACP profile is nothing without a command to run, and an agent spec
        // on a non-ACP profile would be silently ignored. Reject both rather
        // than persisting a profile that can't launch or that lies about what
        // it does.
        match (profile.backend_kind, profile.acp.as_ref()) {
            (BackendKind::Acp, None) => {
                return Err(format!(
                    "launch profile {} targets the ACP backend but has no agent command configured",
                    profile.id
                ));
            }
            // A named adapter knows how to find its own binary (the Kiro
            // adapter resolves `kiro-cli-chat` as a sibling of `kiro-cli`), so
            // it may leave the command blank. A stock agent cannot be
            // discovered, so its command is required.
            (BackendKind::Acp, Some(spec))
                if spec.adapter == protocol::AcpAdapterId::Stock
                    && spec.command.trim().is_empty() =>
            {
                return Err(format!(
                    "launch profile {} must specify the ACP agent command to run",
                    profile.id
                ));
            }
            (kind, Some(_)) if kind != BackendKind::Acp => {
                return Err(format!(
                    "launch profile {} configures an ACP agent but targets {kind:?}",
                    profile.id
                ));
            }
            _ => {}
        }
        validated.push(profile);
    }
    Ok(validated)
}

fn backend_slug(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Tycode => "tycode",
        BackendKind::Acp => "kiro",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Hermes => "hermes",
    }
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
    fn seeds_installed_backends_on_fresh_install_with_preferred_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let store = HostSettingsStore::load(path.clone()).expect("load empty store");

        // Codex + Claude installed; Claude is preferred as the default.
        let seeded = store
            .seed_installed_backends_if_fresh(&[BackendKind::Codex, BackendKind::Claude])
            .expect("seed");
        assert!(seeded);
        assert!(path.exists(), "seeding persists a settings file");

        let settings = store.get().expect("get settings");
        // Normalized to canonical order.
        assert_eq!(
            settings.enabled_backends,
            vec![BackendKind::Claude, BackendKind::Codex]
        );
        assert_eq!(settings.default_backend, Some(BackendKind::Claude));
    }

    /// Hermes has no provider enable/disable flag, so Tyde owns this list. It
    /// is scoped per profile, and clearing it must remove the key rather than
    /// persist an empty vec — an empty list would read as "this profile has a
    /// disable list" while disabling nothing.
    #[test]
    fn hermes_disabled_providers_are_per_profile_and_clear_to_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let store = HostSettingsStore::load(path.clone()).expect("load empty store");

        store
            .apply(HostSettingValue::HermesDisabledProviders {
                profile: "default".to_owned(),
                // Duplicates and padding are normalized, not persisted as-is.
                providers: vec![
                    " copilot ".to_owned(),
                    "copilot".to_owned(),
                    String::new(),
                    "bedrock".to_owned(),
                ],
            })
            .expect("disable providers");
        store
            .apply(HostSettingValue::HermesDisabledProviders {
                profile: "work".to_owned(),
                providers: vec!["openrouter".to_owned()],
            })
            .expect("disable providers for another profile");

        let settings = store.get().expect("get settings");
        assert_eq!(
            settings.hermes_disabled_providers.get("default"),
            Some(&vec!["bedrock".to_owned(), "copilot".to_owned()])
        );
        // Editing one profile must not disturb another's list.
        assert_eq!(
            settings.hermes_disabled_providers.get("work"),
            Some(&vec!["openrouter".to_owned()])
        );

        store
            .apply(HostSettingValue::HermesDisabledProviders {
                profile: "default".to_owned(),
                providers: Vec::new(),
            })
            .expect("re-enable everything");
        let settings = store.get().expect("get settings");
        assert!(
            !settings.hermes_disabled_providers.contains_key("default"),
            "clearing the list must drop the key, not store an empty one"
        );
        assert_eq!(
            settings.hermes_disabled_providers.get("work"),
            Some(&vec!["openrouter".to_owned()]),
            "clearing one profile must leave the others alone"
        );

        // The list has to survive a reload — it is what keeps a provider out
        // of the model picker across restarts.
        let reloaded = HostSettingsStore::load(path).expect("reload store");
        assert_eq!(
            reloaded
                .get()
                .expect("get settings")
                .hermes_disabled_providers
                .get("work"),
            Some(&vec!["openrouter".to_owned()])
        );
    }

    #[test]
    fn seeding_is_noop_once_a_settings_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        // A user who deliberately turned every backend off.
        let store = HostSettingsStore::load(path).expect("load empty store");
        store
            .apply(HostSettingValue::EnabledBackends {
                enabled_backends: vec![],
            })
            .expect("disable all backends");

        let seeded = store
            .seed_installed_backends_if_fresh(&[BackendKind::Claude])
            .expect("seed");
        assert!(!seeded, "must not re-enable backends once configured");
        assert!(store.get().expect("get").enabled_backends.is_empty());
    }

    #[test]
    fn seeding_is_noop_when_nothing_is_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let store = HostSettingsStore::load(path.clone()).expect("load empty store");

        let seeded = store.seed_installed_backends_if_fresh(&[]).expect("seed");
        assert!(!seeded);
        assert!(
            !path.exists(),
            "no file is written so a later launch can seed once a CLI is installed"
        );
    }

    #[test]
    fn mobile_broker_url_write_accepts_only_loopback_dev_brokers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let store = HostSettingsStore::load(path.clone()).expect("load empty store");

        let public = BrokerUrl::new("mqtts://broker.example.test:8883").expect("broker URL");
        let err = store
            .apply(HostSettingValue::MobileBrokerUrl {
                broker_url: Some(public),
            })
            .expect_err("public custom broker must be rejected at write time");
        assert!(err.contains("dev/test-only"), "unexpected error: {err}");
        assert!(!path.exists(), "rejected setting must not be persisted");

        let default_public =
            BrokerUrl::new(protocol::DEFAULT_MOBILE_MQTT_BROKER_URL).expect("broker URL");
        let err = store
            .apply(HostSettingValue::MobileBrokerUrl {
                broker_url: Some(default_public),
            })
            .expect_err("default public broker must be rejected at write time");
        assert!(
            err.contains("public default mobile broker"),
            "unexpected error: {err}"
        );

        let public_ipv6 = BrokerUrl::new("mqtts://[2001:db8::1]:8883").expect("broker URL");
        let err = store
            .apply(HostSettingValue::MobileBrokerUrl {
                broker_url: Some(public_ipv6),
            })
            .expect_err("non-loopback IPv6 broker must be rejected at write time");
        assert!(err.contains("dev/test-only"), "unexpected error: {err}");

        let ipv6_loopback = BrokerUrl::new("mqtts://[::1]:8883").expect("broker URL");
        let settings = store
            .apply(HostSettingValue::MobileBrokerUrl {
                broker_url: Some(ipv6_loopback.clone()),
            })
            .expect("IPv6 loopback dev broker remains allowed");
        assert_eq!(settings.mobile_broker_url, Some(ipv6_loopback));

        let loopback = BrokerUrl::new("mqtts://127.0.0.1:8883").expect("broker URL");
        let settings = store
            .apply(HostSettingValue::MobileBrokerUrl {
                broker_url: Some(loopback.clone()),
            })
            .expect("loopback dev broker remains allowed");
        assert_eq!(settings.mobile_broker_url, Some(loopback));
    }

    #[test]
    fn legacy_public_mobile_broker_url_still_loads_for_repair_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":[],"default_backend":null,"mobile_broker_url":"mqtts://broker.example.test:8883"}}"#,
        )
        .expect("write legacy public broker setting");

        let store = HostSettingsStore::load(path).expect("legacy public broker setting loads");
        let settings = store.get().expect("get settings");
        assert_eq!(
            settings.mobile_broker_url.as_ref().map(BrokerUrl::as_str),
            Some("mqtts://broker.example.test:8883")
        );
    }

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
    fn old_store_files_default_background_agent_features_safely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":["claude"],"default_backend":"claude"}}"#,
        )
        .expect("write legacy store file");

        let store = HostSettingsStore::load(path).expect("load legacy store");
        let settings = store.get().expect("get settings");
        assert!(settings.background_agent_features.auto_generate_agent_names);
        assert!(!settings.background_agent_features.agent_activity_summaries);
    }

    #[test]
    fn background_agent_feature_settings_apply_independently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            HostSettingsStore::load(dir.path().join("settings.json")).expect("load empty store");

        let settings = store
            .apply(HostSettingValue::BackgroundAgentFeatureEnabled {
                feature: BackgroundAgentFeature::AgentActivitySummaries,
                enabled: true,
            })
            .expect("enable activity summaries");
        assert!(settings.background_agent_features.agent_activity_summaries);
        assert!(settings.background_agent_features.auto_generate_agent_names);

        let settings = store
            .apply(HostSettingValue::BackgroundAgentFeatureEnabled {
                feature: BackgroundAgentFeature::AutoGenerateAgentNames,
                enabled: false,
            })
            .expect("disable generated names");
        assert!(settings.background_agent_features.agent_activity_summaries);
        assert!(!settings.background_agent_features.auto_generate_agent_names);
    }

    #[test]
    fn unknown_backend_in_enabled_backends_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":["claude","future_backend","codex"],"default_backend":"claude"}}"#,
        )
        .expect("write store file");

        let store = HostSettingsStore::load(path).expect("load store with unknown backend");
        let settings = store.get().expect("get settings");
        assert_eq!(
            settings.enabled_backends,
            vec![BackendKind::Claude, BackendKind::Codex]
        );
        assert_eq!(settings.default_backend, Some(BackendKind::Claude));
    }

    #[test]
    fn unknown_backend_tier_config_key_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":["claude"],"complexity_tiers_enabled":true,"backend_tier_configs":{"claude":{"low":{"model":{"string":"haiku"}},"high":{}},"future_backend":{"low":{"model":{"string":"Future Low"}},"high":{}}}}}"#,
        )
        .expect("write store file");

        let store = HostSettingsStore::load(path).expect("load store with unknown tier key");
        let settings = store.get().expect("get settings");
        assert!(settings.complexity_tiers_enabled);
        assert_eq!(settings.backend_tier_configs.len(), 1);
        let claude = settings
            .backend_tier_configs
            .get(&BackendKind::Claude)
            .expect("claude tier config kept");
        assert_eq!(
            claude.low.0.get("model"),
            Some(&SessionSettingValue::String("haiku".to_string()))
        );
    }

    #[test]
    fn unknown_default_backend_falls_back_to_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":["claude","future_backend"],"default_backend":"future_backend"}}"#,
        )
        .expect("write store file");

        let store = HostSettingsStore::load(path).expect("load store with unknown default");
        let settings = store.get().expect("get settings");
        assert_eq!(settings.enabled_backends, vec![BackendKind::Claude]);
        assert_eq!(settings.default_backend, None);
    }

    #[test]
    fn fully_known_settings_file_round_trips_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settings":{"enabled_backends":["claude","codex"],"default_backend":"codex","enable_mobile_connections":true,"mobile_broker_url":null,"tyde_debug_mcp_enabled":true,"tyde_agent_control_mcp_enabled":true,"complexity_tiers_enabled":true,"backend_tier_configs":{"codex":{"low":{"reasoning_effort":{"string":"low"}},"high":{"reasoning_effort":{"string":"xhigh"}}}}}}"#,
        )
        .expect("write store file");

        let store = HostSettingsStore::load(path).expect("load fully-known store");
        let before = store.get().expect("get settings");
        assert_eq!(
            before.enabled_backends,
            vec![BackendKind::Claude, BackendKind::Codex]
        );
        assert_eq!(before.default_backend, Some(BackendKind::Codex));
        assert!(before.enable_mobile_connections);
        assert!(before.tyde_debug_mcp_enabled);
        assert!(before.complexity_tiers_enabled);
        assert_eq!(
            before
                .backend_tier_configs
                .get(&BackendKind::Codex)
                .expect("codex tier config")
                .high
                .0
                .get("reasoning_effort"),
            Some(&SessionSettingValue::String("xhigh".to_string()))
        );

        // A write cycle must not drop any known entries.
        let after = store
            .apply(HostSettingValue::TydeDebugMcpEnabled { enabled: true })
            .expect("apply no-op setting");
        assert_eq!(after, before);
        assert_eq!(store.get().expect("re-read settings"), before);
    }

    #[test]
    fn migrates_gemini_settings_to_antigravity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
  "settings": {
    "enabled_backends": ["gemini", "claude", "gemini"],
    "default_backend": "gemini",
    "complexity_tiers_enabled": true,
    "backend_tier_configs": {
      "gemini": {
        "low": {"model": {"string": "legacy-low"}},
        "high": {"model": {"string": "legacy-high"}}
      }
    }
  }
}"#,
        )
        .expect("write legacy settings");

        let store = HostSettingsStore::load(path.clone()).expect("load migrated settings");
        let settings = store.get().expect("get migrated settings");
        assert_eq!(
            settings.enabled_backends,
            vec![BackendKind::Claude, BackendKind::Antigravity]
        );
        assert_eq!(settings.default_backend, Some(BackendKind::Antigravity));
        assert!(
            !settings
                .backend_tier_configs
                .contains_key(&BackendKind::Claude)
        );
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read migrated file")
                .contains("gemini")
        );
        let antigravity = settings
            .backend_tier_configs
            .get(&BackendKind::Antigravity)
            .expect("antigravity tier config seeded");
        assert_eq!(
            antigravity.low.0.get("model"),
            Some(&SessionSettingValue::String(
                "Gemini 3.5 Flash (Low)".to_string()
            ))
        );
    }

    #[test]
    fn migrates_kiro_settings_to_acp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
  "settings": {
    "enabled_backends": ["kiro", "claude"],
    "default_backend": "kiro",
    "complexity_tiers_enabled": true,
    "backend_tier_configs": {
      "kiro": {
        "low": {"model": {"string": "kiro-low"}}
      }
    },
    "launch_profiles": [
      {
        "id": "my-kiro",
        "label": "My Kiro",
        "backend_kind": "kiro",
        "session_settings": {}
      }
    ]
  }
}"#,
        )
        .expect("write legacy settings");

        let store = HostSettingsStore::load(path.clone()).expect("load migrated settings");
        let settings = store.get().expect("get migrated settings");

        assert!(
            settings.enabled_backends.contains(&BackendKind::Acp),
            "kiro must become acp in enabled_backends, got {:?}",
            settings.enabled_backends
        );
        assert_eq!(settings.default_backend, Some(BackendKind::Acp));
        let tiers = settings
            .backend_tier_configs
            .get(&BackendKind::Acp)
            .expect("tier config keyed by the old kind must be re-keyed, not dropped");
        assert_eq!(
            tiers.low.0.get("model"),
            Some(&SessionSettingValue::String("kiro-low".to_string())),
            "the migrated tier config must keep its values"
        );

        let profile = settings
            .launch_profiles
            .iter()
            .find(|profile| profile.id == LaunchProfileId("my-kiro".to_owned()))
            .expect("user launch profile survived migration");
        assert_eq!(profile.backend_kind, BackendKind::Acp);
        let spec = profile
            .acp
            .as_ref()
            .expect("migrated profile gains an agent spec so it still validates");
        assert_eq!(spec.adapter, protocol::AcpAdapterId::Kiro);

        // Assert on the backend *kind* specifically rather than the substring
        // "kiro", which legitimately survives as the adapter id
        // (`"adapter": "kiro"`) and inside the profile id `my-kiro`.
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read migrated file"))
                .expect("migrated file is valid json");
        let persisted = &raw["settings"];
        assert!(
            !persisted["enabled_backends"]
                .as_array()
                .expect("enabled_backends array")
                .iter()
                .any(|kind| kind == LEGACY_KIRO_BACKEND),
            "enabled_backends still names the retired kind: {persisted}"
        );
        assert_ne!(persisted["default_backend"], LEGACY_KIRO_BACKEND);
        assert!(
            persisted["backend_tier_configs"]
                .get(LEGACY_KIRO_BACKEND)
                .is_none(),
            "tier configs are still keyed by the retired kind"
        );
        for profile in persisted["launch_profiles"]
            .as_array()
            .expect("launch_profiles array")
        {
            assert_ne!(
                profile["backend_kind"], LEGACY_KIRO_BACKEND,
                "launch profile still targets the retired kind"
            );
        }
    }

    #[test]
    fn kiro_migration_does_not_clobber_an_existing_acp_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
  "settings": {
    "enabled_backends": ["kiro", "acp"],
    "complexity_tiers_enabled": true,
    "backend_tier_configs": {
      "kiro": {"low": {"model": {"string": "legacy"}}},
      "acp": {"low": {"model": {"string": "current"}}}
    }
  }
}"#,
        )
        .expect("write settings");

        let store = HostSettingsStore::load(path).expect("load migrated settings");
        let settings = store.get().expect("get migrated settings");

        assert_eq!(
            settings
                .enabled_backends
                .iter()
                .filter(|kind| **kind == BackendKind::Acp)
                .count(),
            1,
            "collapsing kiro into acp must not leave a duplicate entry"
        );
        let tiers = settings
            .backend_tier_configs
            .get(&BackendKind::Acp)
            .expect("acp tier config present");
        assert_eq!(
            tiers.low.0.get("model"),
            Some(&SessionSettingValue::String("current".to_string())),
            "a config written by a newer build must win over the legacy kiro one"
        );
    }

    #[test]
    fn stock_acp_launch_profile_requires_a_command() {
        let err = validate_launch_profile_configs(vec![HostLaunchProfileConfig {
            id: LaunchProfileId("custom".to_owned()),
            label: "Custom".to_owned(),
            description: None,
            backend_kind: BackendKind::Acp,
            session_settings: Default::default(),
            acp: Some(protocol::AcpAgentSpec {
                command: "   ".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
                adapter: protocol::AcpAdapterId::Stock,
            }),
        }])
        .expect_err("blank command must be rejected");
        assert!(err.contains("command"), "got: {err}");
    }

    #[test]
    fn agent_spec_on_a_non_acp_profile_is_rejected() {
        let err = validate_launch_profile_configs(vec![HostLaunchProfileConfig {
            id: LaunchProfileId("weird".to_owned()),
            label: "Weird".to_owned(),
            description: None,
            backend_kind: BackendKind::Claude,
            session_settings: Default::default(),
            acp: Some(protocol::AcpAgentSpec {
                command: "claude".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
                adapter: protocol::AcpAdapterId::Stock,
            }),
        }])
        .expect_err("agent spec on a non-ACP profile must be rejected");
        assert!(err.contains("ACP agent"), "got: {err}");
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
        assert!(
            !settings
                .backend_tier_configs
                .contains_key(&BackendKind::Codex),
            "Codex built-in tiers must resolve from live model metadata"
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

    #[test]
    fn voice_settings_validate_exact_model_without_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let store = HostSettingsStore::load(dir.path().join("settings.json")).unwrap();
        let settings = store
            .apply(HostSettingValue::VoiceNovaModel {
                model: "amazon.nova-2-sonic-v1:0".into(),
            })
            .unwrap();
        assert_eq!(settings.voice.nova_model, "amazon.nova-2-sonic-v1:0");
        assert!(
            store
                .apply(HostSettingValue::VoiceNovaModel {
                    model: "amazon.unknown".into()
                })
                .is_err()
        );
        assert_eq!(
            store.get().unwrap().voice.nova_model,
            "amazon.nova-2-sonic-v1:0"
        );

        assert_eq!(
            store.get().unwrap().voice.endpointing_sensitivity,
            protocol::VoiceEndpointingSensitivity::Low
        );
        let settings = store
            .apply(HostSettingValue::VoiceEndpointingSensitivity {
                sensitivity: protocol::VoiceEndpointingSensitivity::High,
            })
            .unwrap();
        assert_eq!(
            settings.voice.endpointing_sensitivity,
            protocol::VoiceEndpointingSensitivity::High
        );
    }
}
