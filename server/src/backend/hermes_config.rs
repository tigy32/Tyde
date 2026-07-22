//! Hermes profile discovery and profile-level configuration management.
//!
//! Hermes profiles are fully independent `HERMES_HOME` directories: the
//! default profile is `~/.hermes` itself and named profiles live under
//! `~/.hermes/profiles/<name>`. Tyde selects a profile per session by
//! setting `HERMES_HOME` when spawning that session's gateway.
//!
//! This module owns the on-disk side: discovering profiles, projecting the
//! editable subset of a profile's `config.yaml` into the typed
//! [`protocol::hermes_config`] document, and writing edits back with an
//! atomic replace that preserves every unmodeled config key. Gateway-backed
//! pieces (provider probes, credential actions) live in [`super::hermes`]
//! and are composed with this module by the snapshot/persist entry points
//! there.

use std::fs;
use std::path::{Path, PathBuf};

use protocol::hermes_config::{
    HERMES_DEFAULT_PROFILE, HermesAgentConfig, HermesFallbackProvider, HermesModelConfig,
    HermesProfileConfig, HermesProviderRouting, HermesToolSearchConfig,
};
use serde_yaml::{Mapping, Value as Yaml};

pub(crate) const HERMES_HOME_ENV: &str = "HERMES_HOME";
const HERMES_PROFILES_DIR: &str = "profiles";
const HERMES_CONFIG_FILE: &str = "config.yaml";

/// One discovered Hermes profile: its name and the `HERMES_HOME` directory
/// that backs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HermesProfileRef {
    pub name: String,
    pub home_dir: PathBuf,
}

impl HermesProfileRef {
    pub(crate) fn is_default(&self) -> bool {
        self.name == HERMES_DEFAULT_PROFILE
    }
}

/// Test-only override for the Hermes home root, so tests never read the
/// machine's real `~/.hermes`. Guarded by the same serialization lock as the
/// other Hermes test overrides.
#[cfg(test)]
pub(crate) static TEST_HERMES_HOME: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// The Hermes home root: `HERMES_HOME` when set in the server's environment,
/// else `~/.hermes`.
pub(crate) fn hermes_home_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(home) = TEST_HERMES_HOME
        .lock()
        .expect("test Hermes home mutex poisoned")
        .clone()
    {
        return Ok(home);
    }
    if let Some(home) = std::env::var(HERMES_HOME_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(home));
    }
    // Mirror Hermes's own platform defaults (hermes_constants.get_hermes_home):
    // `%LOCALAPPDATA%\hermes` on native Windows, `~/.hermes` elsewhere.
    // Diverging here would make Tyde discover and edit a different home than
    // the gateway actually loads.
    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var("LOCALAPPDATA")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            return Ok(PathBuf::from(local_app_data).join("hermes"));
        }
        // Hermes's own fallback when LOCALAPPDATA is absent.
        return Ok(crate::paths::home_dir()?
            .join("AppData")
            .join("Local")
            .join("hermes"));
    }
    #[cfg(not(windows))]
    Ok(crate::paths::home_dir()?.join(".hermes"))
}

/// Hermes's own profile-name grammar (`hermes_cli/profiles.py`):
/// `^[a-z0-9][a-z0-9_-]{0,63}$`. Enforcing it keeps Tyde's view identical to
/// what `hermes -p` would accept and structurally rules out path traversal.
/// Symlinked profile directories are followed, matching Hermes's behavior —
/// creating one already requires write access to the Hermes home itself.
pub(crate) fn is_valid_profile_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if name.len() > 64 {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub(crate) fn discover_profiles() -> Result<Vec<HermesProfileRef>, String> {
    discover_profiles_in(&hermes_home_dir()?)
}

/// Enumerate profiles under one Hermes home root: the default profile (the
/// root itself) first, then named `profiles/<name>` directories sorted by
/// name. A missing `profiles/` directory means no named profiles; an
/// unreadable one is a visible error.
pub(crate) fn discover_profiles_in(home: &Path) -> Result<Vec<HermesProfileRef>, String> {
    let mut profiles = vec![HermesProfileRef {
        name: HERMES_DEFAULT_PROFILE.to_owned(),
        home_dir: home.to_path_buf(),
    }];
    let profiles_dir = home.join(HERMES_PROFILES_DIR);
    if !profiles_dir.is_dir() {
        return Ok(profiles);
    }
    let entries = fs::read_dir(&profiles_dir).map_err(|error| {
        format!(
            "Failed to list Hermes profiles in {}: {error}",
            profiles_dir.display()
        )
    })?;
    let mut named = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read a Hermes profile entry in {}: {error}",
                profiles_dir.display()
            )
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Hermes only creates grammar-conforming profile names; anything else
        // in profiles/ is not a Hermes profile (and `default` is reserved for
        // the home root itself).
        if !is_valid_profile_name(name) || name == HERMES_DEFAULT_PROFILE {
            continue;
        }
        named.push(HermesProfileRef {
            name: name.to_owned(),
            home_dir: path,
        });
    }
    named.sort_by(|a, b| a.name.cmp(&b.name));
    profiles.extend(named);
    Ok(profiles)
}

/// Resolve a session-setting profile name to a profile ref. `None`, empty and
/// `"default"` all mean the default profile. Named profiles must exist on
/// disk and must be plain directory names (a name with a path separator is
/// rejected rather than resolved outside the profiles root).
pub(crate) fn resolve_profile_ref(name: Option<&str>) -> Result<HermesProfileRef, String> {
    let home = hermes_home_dir()?;
    resolve_profile_ref_in(&home, name)
}

pub(crate) fn resolve_profile_ref_in(
    home: &Path,
    name: Option<&str>,
) -> Result<HermesProfileRef, String> {
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    let Some(name) = name.filter(|name| *name != HERMES_DEFAULT_PROFILE) else {
        return Ok(HermesProfileRef {
            name: HERMES_DEFAULT_PROFILE.to_owned(),
            home_dir: home.to_path_buf(),
        });
    };
    if !is_valid_profile_name(name) {
        return Err(format!("invalid Hermes profile name '{name}'"));
    }
    let dir = home.join(HERMES_PROFILES_DIR).join(name);
    if !dir.is_dir() {
        return Err(format!(
            "Hermes profile '{name}' does not exist at {}",
            dir.display()
        ));
    }
    Ok(HermesProfileRef {
        name: name.to_owned(),
        home_dir: dir,
    })
}

// ── config.yaml projection ─────────────────────────────────────────────

/// Load the editable projection of a profile's `config.yaml`. A missing file
/// projects to all-unset (Hermes defaults); a malformed file or a modeled key
/// with an unusable type is a visible error, never silently skipped.
pub(crate) fn load_profile_config(home_dir: &Path) -> Result<HermesProfileConfig, String> {
    let path = home_dir.join(HERMES_CONFIG_FILE);
    if !path.is_file() {
        return Ok(HermesProfileConfig::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let root: Yaml = serde_yaml::from_str(&raw)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;
    profile_config_from_yaml(&root).map_err(|error| format!("{}: {error}", path.display()))
}

fn profile_config_from_yaml(root: &Yaml) -> Result<HermesProfileConfig, String> {
    let mut config = HermesProfileConfig::default();
    if let Some(model) = yaml_key(root, "model") {
        config.model = model_config_from_yaml(model)?;
    }
    if let Some(routing) = yaml_key(root, "provider_routing") {
        config.provider_routing = provider_routing_from_yaml(routing)?;
    }
    if let Some(fallbacks) = yaml_key(root, "fallback_providers") {
        config.fallback_providers = fallback_providers_from_yaml(fallbacks)?;
    }
    if let Some(agent) = yaml_key(root, "agent") {
        config.agent = agent_config_from_yaml(agent)?;
    }
    if let Some(tool_search) =
        yaml_key(root, "tools").and_then(|tools| yaml_key(tools, "tool_search"))
    {
        config.tool_search = tool_search_from_yaml(tool_search)?;
    }
    Ok(config)
}

fn model_config_from_yaml(model: &Yaml) -> Result<HermesModelConfig, String> {
    // Hermes accepts `model` as either a bare model-id string or a mapping;
    // the mapping spells the model id as `default` (with `model` accepted as
    // an alias).
    match model {
        Yaml::String(value) => Ok(HermesModelConfig {
            model: non_empty(value),
            ..HermesModelConfig::default()
        }),
        Yaml::Null => Ok(HermesModelConfig::default()),
        Yaml::Mapping(_) => Ok(HermesModelConfig {
            provider: yaml_string(model, "model.provider", "provider")?,
            model: match yaml_string(model, "model.default", "default")? {
                Some(value) => Some(value),
                None => yaml_string(model, "model.model", "model")?,
            },
            base_url: yaml_string(model, "model.base_url", "base_url")?,
            context_length: yaml_integer(model, "model.context_length", "context_length")?,
            max_tokens: yaml_integer(model, "model.max_tokens", "max_tokens")?,
        }),
        other => Err(format!(
            "config key 'model' must be a string or mapping, found {}",
            yaml_type_name(other)
        )),
    }
}

fn provider_routing_from_yaml(routing: &Yaml) -> Result<HermesProviderRouting, String> {
    if routing.is_null() {
        return Ok(HermesProviderRouting::default());
    }
    if !routing.is_mapping() {
        return Err(format!(
            "config key 'provider_routing' must be a mapping, found {}",
            yaml_type_name(routing)
        ));
    }
    Ok(HermesProviderRouting {
        sort: yaml_string(routing, "provider_routing.sort", "sort")?,
        only: yaml_string_list(routing, "provider_routing.only", "only")?,
        ignore: yaml_string_list(routing, "provider_routing.ignore", "ignore")?,
    })
}

fn fallback_providers_from_yaml(fallbacks: &Yaml) -> Result<Vec<HermesFallbackProvider>, String> {
    let entries = match fallbacks {
        Yaml::Null => return Ok(Vec::new()),
        // Hermes accepts a single mapping as shorthand for a one-entry list.
        Yaml::Mapping(_) => std::slice::from_ref(fallbacks),
        Yaml::Sequence(entries) => entries.as_slice(),
        other => {
            return Err(format!(
                "config key 'fallback_providers' must be a list, found {}",
                yaml_type_name(other)
            ));
        }
    };
    let mut parsed = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let context = format!("fallback_providers[{index}]");
        if !entry.is_mapping() {
            return Err(format!(
                "config key '{context}' must be a mapping, found {}",
                yaml_type_name(entry)
            ));
        }
        let provider = yaml_string(entry, &format!("{context}.provider"), "provider")?
            .ok_or_else(|| format!("config key '{context}' is missing 'provider'"))?;
        let model = yaml_string(entry, &format!("{context}.model"), "model")?
            .ok_or_else(|| format!("config key '{context}' is missing 'model'"))?;
        // Hermes copies arbitrary extra fields on fallback entries (e.g.
        // base_url, api_mode) — preserve them all so a save can never strip
        // one.
        let mut extra = serde_json::Map::new();
        if let Some(mapping) = entry.as_mapping() {
            for (key, value) in mapping {
                let Some(key) = key.as_str() else {
                    return Err(format!(
                        "config key '{context}' has a non-string field name"
                    ));
                };
                if key == "provider" || key == "model" {
                    continue;
                }
                extra.insert(key.to_owned(), yaml_to_json(value, &context)?);
            }
        }
        parsed.push(HermesFallbackProvider {
            provider,
            model,
            extra,
        });
    }
    Ok(parsed)
}

fn agent_config_from_yaml(agent: &Yaml) -> Result<HermesAgentConfig, String> {
    if agent.is_null() {
        return Ok(HermesAgentConfig::default());
    }
    if !agent.is_mapping() {
        return Err(format!(
            "config key 'agent' must be a mapping, found {}",
            yaml_type_name(agent)
        ));
    }
    // `coding_context` accepts "auto"/"focus"/"on"/"off" and the YAML
    // booleans Hermes normalizes to on/off.
    let coding_context = match yaml_key(agent, "coding_context") {
        None | Some(Yaml::Null) => None,
        Some(Yaml::String(value)) => non_empty(value),
        Some(Yaml::Bool(true)) => Some("on".to_owned()),
        Some(Yaml::Bool(false)) => Some("off".to_owned()),
        Some(other) => {
            return Err(format!(
                "config key 'agent.coding_context' must be a string, found {}",
                yaml_type_name(other)
            ));
        }
    };
    Ok(HermesAgentConfig {
        max_turns: yaml_integer(agent, "agent.max_turns", "max_turns")?,
        coding_context,
        disabled_toolsets: yaml_string_list(agent, "agent.disabled_toolsets", "disabled_toolsets")?,
    })
}

fn tool_search_from_yaml(tool_search: &Yaml) -> Result<HermesToolSearchConfig, String> {
    if tool_search.is_null() {
        return Ok(HermesToolSearchConfig::default());
    }
    if !tool_search.is_mapping() {
        return Err(format!(
            "config key 'tools.tool_search' must be a mapping, found {}",
            yaml_type_name(tool_search)
        ));
    }
    // `enabled` is "auto"/"on"/"off"; YAML booleans normalize to on/off.
    let enabled = match yaml_key(tool_search, "enabled") {
        None | Some(Yaml::Null) => None,
        Some(Yaml::String(value)) => non_empty(value),
        Some(Yaml::Bool(true)) => Some("on".to_owned()),
        Some(Yaml::Bool(false)) => Some("off".to_owned()),
        Some(other) => {
            return Err(format!(
                "config key 'tools.tool_search.enabled' must be a string, found {}",
                yaml_type_name(other)
            ));
        }
    };
    Ok(HermesToolSearchConfig {
        enabled,
        threshold_pct: yaml_float(
            tool_search,
            "tools.tool_search.threshold_pct",
            "threshold_pct",
        )?,
    })
}

// ── config.yaml write-back ─────────────────────────────────────────────

/// Write the editable projection back into a profile's `config.yaml`.
/// `Some` values are set, `None`/empty values remove their key, and every
/// key outside the projection is preserved byte-for-byte at the YAML value
/// level. The file is replaced atomically with owner-only permissions.
pub(crate) fn apply_profile_config(
    home_dir: &Path,
    config: &HermesProfileConfig,
) -> Result<(), String> {
    let path = home_dir.join(HERMES_CONFIG_FILE);
    let mut root = if path.is_file() {
        let raw = fs::read_to_string(&path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        serde_yaml::from_str::<Yaml>(&raw)
            .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?
    } else {
        Yaml::Mapping(Mapping::new())
    };
    let Yaml::Mapping(mapping) = &mut root else {
        return Err(format!(
            "{} does not contain a YAML mapping; refusing to rewrite it",
            path.display()
        ));
    };

    apply_model_config(mapping, &config.model);
    apply_provider_routing(mapping, &config.provider_routing);
    apply_fallback_providers(mapping, &config.fallback_providers);
    apply_agent_config(mapping, &config.agent);
    apply_tool_search(mapping, &config.tool_search);

    let rendered = serde_yaml::to_string(&root)
        .map_err(|error| format!("Failed to render {}: {error}", path.display()))?;
    write_atomic(&path, rendered.as_bytes())
}

fn apply_model_config(root: &mut Mapping, model: &HermesModelConfig) {
    // Normalize a bare-string `model` into the mapping form before editing so
    // an existing model id is kept unless the update clears it.
    let existing = root.get(yaml_str("model")).cloned();
    let mut mapping = match existing {
        Some(Yaml::Mapping(mapping)) => mapping,
        Some(Yaml::String(value)) if !value.trim().is_empty() => {
            let mut mapping = Mapping::new();
            mapping.insert(yaml_str("default"), Yaml::String(value));
            mapping
        }
        _ => Mapping::new(),
    };
    // The projection reads `default` with `model` as an alias; writing keeps
    // only the canonical `default` spelling.
    mapping.shift_remove(yaml_str("model"));
    set_or_remove_string(&mut mapping, "provider", model.provider.as_deref());
    set_or_remove_string(&mut mapping, "default", model.model.as_deref());
    set_or_remove_string(&mut mapping, "base_url", model.base_url.as_deref());
    set_or_remove_integer(&mut mapping, "context_length", model.context_length);
    set_or_remove_integer(&mut mapping, "max_tokens", model.max_tokens);
    set_or_remove_mapping(root, "model", mapping);
}

fn apply_provider_routing(root: &mut Mapping, routing: &HermesProviderRouting) {
    let mut mapping = match root.get(yaml_str("provider_routing")).cloned() {
        Some(Yaml::Mapping(mapping)) => mapping,
        _ => Mapping::new(),
    };
    set_or_remove_string(&mut mapping, "sort", routing.sort.as_deref());
    set_or_remove_string_list(&mut mapping, "only", &routing.only);
    set_or_remove_string_list(&mut mapping, "ignore", &routing.ignore);
    set_or_remove_mapping(root, "provider_routing", mapping);
}

fn apply_fallback_providers(root: &mut Mapping, fallbacks: &[HermesFallbackProvider]) {
    if fallbacks.is_empty() {
        root.shift_remove(yaml_str("fallback_providers"));
        return;
    }
    let entries = fallbacks
        .iter()
        .map(|fallback| {
            let mut entry = Mapping::new();
            entry.insert(
                yaml_str("provider"),
                Yaml::String(fallback.provider.clone()),
            );
            entry.insert(yaml_str("model"), Yaml::String(fallback.model.clone()));
            for (key, value) in &fallback.extra {
                if key == "provider" || key == "model" {
                    continue;
                }
                entry.insert(yaml_str(key), json_to_yaml(value));
            }
            Yaml::Mapping(entry)
        })
        .collect();
    root.insert(yaml_str("fallback_providers"), Yaml::Sequence(entries));
}

fn apply_agent_config(root: &mut Mapping, agent: &HermesAgentConfig) {
    let mut mapping = match root.get(yaml_str("agent")).cloned() {
        Some(Yaml::Mapping(mapping)) => mapping,
        _ => Mapping::new(),
    };
    set_or_remove_integer(&mut mapping, "max_turns", agent.max_turns);
    set_or_remove_string(
        &mut mapping,
        "coding_context",
        agent.coding_context.as_deref(),
    );
    set_or_remove_string_list(&mut mapping, "disabled_toolsets", &agent.disabled_toolsets);
    set_or_remove_mapping(root, "agent", mapping);
}

fn apply_tool_search(root: &mut Mapping, tool_search: &HermesToolSearchConfig) {
    let mut tools = match root.get(yaml_str("tools")).cloned() {
        Some(Yaml::Mapping(mapping)) => mapping,
        _ => Mapping::new(),
    };
    let mut mapping = match tools.get(yaml_str("tool_search")).cloned() {
        Some(Yaml::Mapping(mapping)) => mapping,
        _ => Mapping::new(),
    };
    set_or_remove_string(&mut mapping, "enabled", tool_search.enabled.as_deref());
    set_or_remove_float(&mut mapping, "threshold_pct", tool_search.threshold_pct);
    set_or_remove_mapping(&mut tools, "tool_search", mapping);
    set_or_remove_mapping(root, "tools", tools);
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".tyde-hermes-config-")
        .tempfile_in(dir)
        .map_err(|error| {
            format!(
                "Failed to create a temp file next to {}: {error}",
                path.display()
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to protect {}: {error}", temp.path().display()))?;
    }
    use std::io::Write as _;
    temp.write_all(contents)
        .map_err(|error| format!("Failed to write {}: {error}", temp.path().display()))?;
    temp.persist(path)
        .map_err(|error| format!("Failed to replace {}: {error}", path.display()))?;
    Ok(())
}

// ── YAML helpers ───────────────────────────────────────────────────────

fn yaml_str(key: &str) -> Yaml {
    Yaml::String(key.to_owned())
}

fn yaml_key<'a>(value: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    value
        .as_mapping()
        .and_then(|mapping| mapping.get(yaml_str(key)))
}

fn yaml_type_name(value: &Yaml) -> &'static str {
    match value {
        Yaml::Null => "null",
        Yaml::Bool(_) => "a bool",
        Yaml::Number(_) => "a number",
        Yaml::String(_) => "a string",
        Yaml::Sequence(_) => "a list",
        Yaml::Mapping(_) => "a mapping",
        Yaml::Tagged(_) => "a tagged value",
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn yaml_string(parent: &Yaml, context: &str, key: &str) -> Result<Option<String>, String> {
    match yaml_key(parent, key) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::String(value)) => Ok(non_empty(value)),
        Some(other) => Err(format!(
            "config key '{context}' must be a string, found {}",
            yaml_type_name(other)
        )),
    }
}

fn yaml_integer(parent: &Yaml, context: &str, key: &str) -> Result<Option<i64>, String> {
    match yaml_key(parent, key) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("config key '{context}' must be an integer, found {value}")),
        Some(other) => Err(format!(
            "config key '{context}' must be an integer, found {}",
            yaml_type_name(other)
        )),
    }
}

fn yaml_float(parent: &Yaml, context: &str, key: &str) -> Result<Option<f64>, String> {
    match yaml_key(parent, key) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::Number(value)) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("config key '{context}' must be a number, found {value}")),
        Some(other) => Err(format!(
            "config key '{context}' must be a number, found {}",
            yaml_type_name(other)
        )),
    }
}

fn set_or_remove_float(mapping: &mut Mapping, key: &str, value: Option<f64>) {
    match value {
        // Write whole numbers as integers so a 10 stays a 10 in the YAML.
        Some(value) if value.fract() == 0.0 && value.abs() < i64::MAX as f64 => {
            mapping.insert(yaml_str(key), Yaml::Number((value as i64).into()));
        }
        Some(value) => {
            mapping.insert(yaml_str(key), Yaml::Number(value.into()));
        }
        None => {
            mapping.shift_remove(yaml_str(key));
        }
    }
}

fn yaml_to_json(value: &Yaml, context: &str) -> Result<serde_json::Value, String> {
    serde_json::to_value(value)
        .map_err(|error| format!("config key '{context}' is not JSON-representable: {error}"))
}

fn json_to_yaml(value: &serde_json::Value) -> Yaml {
    serde_yaml::to_value(value).unwrap_or(Yaml::Null)
}

/// Semantic validation applied before a save is written anywhere: an invalid
/// document must be rejected up front, never persisted and rediscovered as a
/// broken snapshot later.
pub(crate) fn validate_profile_config(config: &HermesProfileConfig) -> Result<(), String> {
    for (index, fallback) in config.fallback_providers.iter().enumerate() {
        if fallback.provider.trim().is_empty() || fallback.model.trim().is_empty() {
            return Err(format!(
                "fallback provider #{} needs both a provider and a model",
                index + 1
            ));
        }
    }
    if let Some(threshold) = config.tool_search.threshold_pct
        && !(0.0..=100.0).contains(&threshold)
    {
        return Err(format!(
            "tool search threshold must be between 0 and 100, got {threshold}"
        ));
    }
    Ok(())
}

fn yaml_string_list(parent: &Yaml, context: &str, key: &str) -> Result<Vec<String>, String> {
    match yaml_key(parent, key) {
        None | Some(Yaml::Null) => Ok(Vec::new()),
        Some(Yaml::Sequence(entries)) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| match entry {
                Yaml::String(value) => non_empty(value)
                    .ok_or_else(|| format!("config key '{context}[{index}]' must be non-empty")),
                other => Err(format!(
                    "config key '{context}[{index}]' must be a string, found {}",
                    yaml_type_name(other)
                )),
            })
            .collect(),
        // Hermes accepts a single string as a one-entry list shorthand.
        Some(Yaml::String(value)) => Ok(non_empty(value).into_iter().collect()),
        Some(other) => Err(format!(
            "config key '{context}' must be a list of strings, found {}",
            yaml_type_name(other)
        )),
    }
}

fn set_or_remove_string(mapping: &mut Mapping, key: &str, value: Option<&str>) {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => {
            mapping.insert(yaml_str(key), Yaml::String(value.to_owned()));
        }
        None => {
            mapping.shift_remove(yaml_str(key));
        }
    }
}

fn set_or_remove_integer(mapping: &mut Mapping, key: &str, value: Option<i64>) {
    match value {
        Some(value) => {
            mapping.insert(yaml_str(key), Yaml::Number(value.into()));
        }
        None => {
            mapping.shift_remove(yaml_str(key));
        }
    }
}

fn set_or_remove_string_list(mapping: &mut Mapping, key: &str, values: &[String]) {
    if values.is_empty() {
        mapping.shift_remove(yaml_str(key));
        return;
    }
    let entries = values
        .iter()
        .map(|value| Yaml::String(value.clone()))
        .collect();
    mapping.insert(yaml_str(key), Yaml::Sequence(entries));
}

fn set_or_remove_mapping(parent: &mut Mapping, key: &str, mapping: Mapping) {
    if mapping.is_empty() {
        parent.shift_remove(yaml_str(key));
    } else {
        parent.insert(yaml_str(key), Yaml::Mapping(mapping));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("tyde-hermes-config-test-")
            .tempdir()
            .expect("temp hermes home")
    }

    #[test]
    fn discovery_lists_default_then_named_profiles_sorted() {
        let home = temp_home();
        fs::create_dir_all(home.path().join("profiles/grok")).unwrap();
        fs::create_dir_all(home.path().join("profiles/claude")).unwrap();
        fs::create_dir_all(home.path().join("profiles/.hidden")).unwrap();
        fs::write(home.path().join("profiles/notes.txt"), "not a profile").unwrap();

        let profiles = discover_profiles_in(home.path()).unwrap();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["default", "claude", "grok"]);
        assert_eq!(profiles[0].home_dir, home.path());
        assert_eq!(profiles[2].home_dir, home.path().join("profiles/grok"));
    }

    #[test]
    fn discovery_without_profiles_dir_yields_only_default() {
        let home = temp_home();
        let profiles = discover_profiles_in(home.path()).unwrap();
        assert_eq!(profiles.len(), 1);
        assert!(profiles[0].is_default());
    }

    #[test]
    fn profile_resolution_validates_names_and_existence() {
        let home = temp_home();
        fs::create_dir_all(home.path().join("profiles/claude")).unwrap();

        assert!(
            resolve_profile_ref_in(home.path(), None)
                .unwrap()
                .is_default()
        );
        assert!(
            resolve_profile_ref_in(home.path(), Some("default"))
                .unwrap()
                .is_default()
        );
        assert!(
            resolve_profile_ref_in(home.path(), Some("  "))
                .unwrap()
                .is_default()
        );
        let claude = resolve_profile_ref_in(home.path(), Some("claude")).unwrap();
        assert_eq!(claude.home_dir, home.path().join("profiles/claude"));

        let missing = resolve_profile_ref_in(home.path(), Some("gpt")).unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
        let traversal = resolve_profile_ref_in(home.path(), Some("../evil")).unwrap_err();
        assert!(
            traversal.contains("invalid Hermes profile name"),
            "{traversal}"
        );
    }

    #[test]
    fn missing_config_projects_to_defaults() {
        let home = temp_home();
        assert_eq!(
            load_profile_config(home.path()).unwrap(),
            HermesProfileConfig::default()
        );
    }

    #[test]
    fn load_projects_modeled_keys_and_accepts_string_model() {
        let home = temp_home();
        fs::write(
            home.path().join("config.yaml"),
            concat!(
                "model:\n",
                "  provider: openrouter\n",
                "  default: minimax/minimax-m3\n",
                "provider_routing:\n",
                "  only:\n",
                "    - minimax\n",
                "fallback_providers:\n",
                "  - provider: anthropic\n",
                "    model: claude-sonnet-5\n",
                "agent:\n",
                "  max_turns: 90\n",
                "tools:\n",
                "  tool_search:\n",
                "    enabled: auto\n",
                "    threshold_pct: 10\n",
            ),
        )
        .unwrap();
        let config = load_profile_config(home.path()).unwrap();
        assert_eq!(config.model.provider.as_deref(), Some("openrouter"));
        assert_eq!(config.model.model.as_deref(), Some("minimax/minimax-m3"));
        assert_eq!(config.provider_routing.only, vec!["minimax".to_owned()]);
        assert_eq!(config.fallback_providers.len(), 1);
        assert_eq!(config.agent.max_turns, Some(90));
        assert_eq!(config.tool_search.enabled.as_deref(), Some("auto"));
        assert_eq!(config.tool_search.threshold_pct, Some(10.0));

        let home2 = temp_home();
        fs::write(home2.path().join("config.yaml"), "model: gpt-5.2\n").unwrap();
        let config2 = load_profile_config(home2.path()).unwrap();
        assert_eq!(config2.model.model.as_deref(), Some("gpt-5.2"));
        assert_eq!(config2.model.provider, None);
    }

    #[test]
    fn load_rejects_wrong_types_visibly() {
        let home = temp_home();
        fs::write(
            home.path().join("config.yaml"),
            "agent:\n  max_turns: many\n",
        )
        .unwrap();
        let error = load_profile_config(home.path()).unwrap_err();
        assert!(error.contains("agent.max_turns"), "{error}");
    }

    #[test]
    fn apply_round_trips_and_preserves_unmodeled_keys() {
        let home = temp_home();
        fs::write(
            home.path().join("config.yaml"),
            concat!(
                "model: old/model\n",
                "toolsets:\n",
                "  - hermes-cli\n",
                "display:\n",
                "  busy_input_mode: queue\n",
                "agent:\n",
                "  max_turns: 90\n",
                "  verify_guidance: false\n",
            ),
        )
        .unwrap();

        let mut config = load_profile_config(home.path()).unwrap();
        assert_eq!(config.model.model.as_deref(), Some("old/model"));
        config.model.provider = Some("anthropic".to_owned());
        config.model.model = Some("claude-sonnet-5".to_owned());
        config.agent.max_turns = None;
        config.tool_search.enabled = Some("off".to_owned());
        apply_profile_config(home.path(), &config).unwrap();

        let raw = fs::read_to_string(home.path().join("config.yaml")).unwrap();
        let reloaded = load_profile_config(home.path()).unwrap();
        assert_eq!(reloaded.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(reloaded.model.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(reloaded.agent.max_turns, None);
        assert_eq!(reloaded.tool_search.enabled.as_deref(), Some("off"));
        // Unmodeled keys survive, including inside a partially modeled section.
        assert!(raw.contains("hermes-cli"), "{raw}");
        assert!(raw.contains("busy_input_mode"), "{raw}");
        assert!(raw.contains("verify_guidance"), "{raw}");
        // The cleared key is gone rather than left stale.
        assert!(!raw.contains("max_turns"), "{raw}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(home.path().join("config.yaml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn apply_on_missing_config_creates_only_set_keys() {
        let home = temp_home();
        let config = HermesProfileConfig {
            model: HermesModelConfig {
                provider: Some("xai".to_owned()),
                model: Some("grok-4".to_owned()),
                ..HermesModelConfig::default()
            },
            ..HermesProfileConfig::default()
        };
        apply_profile_config(home.path(), &config).unwrap();
        let raw = fs::read_to_string(home.path().join("config.yaml")).unwrap();
        assert!(raw.contains("provider: xai"), "{raw}");
        assert!(raw.contains("default: grok-4"), "{raw}");
        assert!(!raw.contains("provider_routing"), "{raw}");
        assert!(!raw.contains("tool_search"), "{raw}");
    }

    #[test]
    fn fallback_extras_round_trip_and_float_threshold_loads() {
        let home = temp_home();
        fs::write(
            home.path().join("config.yaml"),
            concat!(
                "fallback_providers:\n",
                "  - provider: openai\n",
                "    model: gpt-5\n",
                "    api_mode: responses\n",
                "    future_field: keep-me\n",
                "tools:\n",
                "  tool_search:\n",
                "    threshold_pct: 7.5\n",
            ),
        )
        .unwrap();

        let mut config = load_profile_config(home.path()).unwrap();
        assert_eq!(config.tool_search.threshold_pct, Some(7.5));
        assert_eq!(
            config.fallback_providers[0]
                .extra
                .get("api_mode")
                .and_then(|v| v.as_str()),
            Some("responses")
        );

        // Editing an unrelated field must not strip the fallback extras.
        config.agent.max_turns = Some(120);
        apply_profile_config(home.path(), &config).unwrap();
        let raw = fs::read_to_string(home.path().join("config.yaml")).unwrap();
        assert!(raw.contains("api_mode: responses"), "{raw}");
        assert!(raw.contains("future_field: keep-me"), "{raw}");
        assert!(raw.contains("threshold_pct: 7.5"), "{raw}");
        let reloaded = load_profile_config(home.path()).unwrap();
        assert_eq!(reloaded.fallback_providers, config.fallback_providers);
    }

    #[test]
    fn profile_names_follow_hermes_grammar() {
        for valid in ["claude", "gpt-4", "a", "work_2"] {
            assert!(is_valid_profile_name(valid), "{valid}");
        }
        for invalid in ["", "Claude", "-lead", ".hidden", "a b", "../evil", "a/b"] {
            assert!(!is_valid_profile_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn config_validation_rejects_half_filled_fallbacks_and_bad_threshold() {
        let mut config = HermesProfileConfig::default();
        config.fallback_providers.push(HermesFallbackProvider {
            provider: "anthropic".to_owned(),
            model: String::new(),
            extra: Default::default(),
        });
        let error = validate_profile_config(&config).unwrap_err();
        assert!(error.contains("provider and a model"), "{error}");

        let mut config = HermesProfileConfig::default();
        config.tool_search.threshold_pct = Some(250.0);
        let error = validate_profile_config(&config).unwrap_err();
        assert!(error.contains("between 0 and 100"), "{error}");
    }

    #[test]
    fn apply_refuses_non_mapping_config() {
        let home = temp_home();
        fs::write(home.path().join("config.yaml"), "- not\n- a\n- mapping\n").unwrap();
        let error = apply_profile_config(home.path(), &HermesProfileConfig::default()).unwrap_err();
        assert!(error.contains("refusing to rewrite"), "{error}");
    }
}
