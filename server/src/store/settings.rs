use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use protocol::{
    BackendKind, BackgroundAgentFeature, BrokerUrl, CodeIntelSettings, HostLaunchProfileConfig,
    HostSettingValue, HostSettings, LaunchProfileId,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
    SUPERVISOR_RETRY_ATTEMPTS_MIN,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CANONICAL_BACKENDS: [BackendKind; 6] = [
    BackendKind::Tycode,
    BackendKind::Kiro,
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
    BackendKind::Kiro,
    BackendKind::Tycode,
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

    if settings.supervisor.retry_attempts > SUPERVISOR_RETRY_ATTEMPTS_MAX {
        return Err(format!(
            "supervisor retry attempts must be between {} and {}",
            SUPERVISOR_RETRY_ATTEMPTS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
        ));
    }

    let code_intel = validate_code_intel_settings(settings.code_intel)?;
    let launch_profiles = validate_launch_profile_configs(settings.launch_profiles)?;

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
    })
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
        validated.push(profile);
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
}
