//! Typed document schema for the Hermes backend-native settings snapshot.
//!
//! The document travels as the opaque `settings` value inside
//! [`crate::BackendNativeSettingsSnapshot`] (server → client) and
//! [`crate::HostSettingValue::BackendNativeSettings`] (client → server). Both
//! sides deserialize it into these types; the wire frames stay generic.
//!
//! Server snapshots describe every discovered Hermes profile (the default
//! `~/.hermes` home plus `~/.hermes/profiles/<name>` directories) together
//! with the live provider states probed from that profile's gateway. Client
//! saves send back the same document with edited per-profile `config`
//! sections and optional write-only credential `actions`. Provider states are
//! server-owned and ignored on save; credential actions are executed against
//! the profile's gateway and are never echoed back in a snapshot.

use serde::{Deserialize, Serialize};

/// Version stamp for [`HermesNativeSettingsDoc`]. Bump on breaking shape
/// changes so an old client save can be rejected instead of misapplied.
pub const HERMES_NATIVE_SETTINGS_VERSION: u32 = 1;

/// Profile name of the primary Hermes home (`~/.hermes` itself).
pub const HERMES_DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesNativeSettingsDoc {
    pub version: u32,
    /// Discovery order: the default profile first, named profiles sorted.
    pub profiles: Vec<HermesProfileSettings>,
    /// Write-only credential operations, executed before config sections are
    /// applied. Never present in server snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<HermesCredentialAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesProfileSettings {
    /// `"default"` for the primary home, else the `profiles/<name>` dir name.
    pub name: String,
    /// Absolute `HERMES_HOME` directory backing this profile. Server-owned.
    pub home_dir: String,
    /// Editable projection of this profile's `config.yaml`.
    pub config: HermesProfileConfig,
    /// On save: the unedited projection this edit was based on (the snapshot
    /// the client loaded). The server refuses the save when the profile's
    /// on-disk config no longer matches it, so a stale draft cannot silently
    /// overwrite concurrent changes. Absent in server snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_config: Option<HermesProfileConfig>,
    /// Live provider states probed from this profile's gateway. Server-owned;
    /// ignored on save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<HermesProviderState>>,
    /// Why `providers` is absent (probe failure). Server-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers_error: Option<String>,
    /// The profile's currently effective model/provider as reported by its
    /// gateway (`model.options`). Server-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
}

/// The editable subset of a Hermes profile's `config.yaml`. Every leaf is
/// optional: `None` means the key is absent from the YAML and Hermes applies
/// its own default. Saving writes `Some` values and removes `None` keys;
/// unmodeled config keys are always preserved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesProfileConfig {
    pub model: HermesModelConfig,
    pub provider_routing: HermesProviderRouting,
    pub fallback_providers: Vec<HermesFallbackProvider>,
    pub agent: HermesAgentConfig,
    pub tool_search: HermesToolSearchConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesModelConfig {
    /// Provider slug (`model.provider`).
    pub provider: Option<String>,
    /// Default model id (`model.default`).
    pub model: Option<String>,
    /// Endpoint override (`model.base_url`).
    pub base_url: Option<String>,
    /// Context window override (`model.context_length`).
    pub context_length: Option<i64>,
    /// Output token cap (`model.max_tokens`).
    pub max_tokens: Option<i64>,
}

/// OpenRouter routing preferences (top-level `provider_routing`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesProviderRouting {
    /// `price` | `throughput` | `latency`.
    pub sort: Option<String>,
    /// Upstream-provider whitelist.
    pub only: Vec<String>,
    /// Upstream-provider blacklist.
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesFallbackProvider {
    pub provider: String,
    pub model: String,
    /// Every other key on the YAML entry (`base_url`, `api_mode`, and any
    /// future Hermes fields), preserved verbatim on round-trip even though
    /// the editor UI does not surface them.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesAgentConfig {
    /// `agent.max_turns`.
    pub max_turns: Option<i64>,
    /// `agent.coding_context`: `auto` | `focus` | `on` | `off`.
    pub coding_context: Option<String>,
    /// `agent.disabled_toolsets`.
    pub disabled_toolsets: Vec<String>,
}

/// Progressive tool disclosure (`tools.tool_search`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesToolSearchConfig {
    /// `auto` | `on` | `off`.
    pub enabled: Option<String>,
    /// Percent of context length where `auto` activates. Hermes models this
    /// as a float (fractional thresholds are valid) and clamps it to 0..100.
    pub threshold_pct: Option<f64>,
}

/// One provider row from the profile gateway's `model.options` probe.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HermesProviderState {
    pub slug: String,
    pub name: String,
    pub authenticated: bool,
    /// Hermes auth mechanism, e.g. `api_key`, `oauth_device_code`. Only
    /// `api_key` providers accept [`HermesCredentialAction::SaveApiKey`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    /// Environment variable an API key would be saved under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// Setup hint Hermes attaches to unconfigured providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub model_count: u32,
}

/// Write-only credential operation, executed against the named profile's
/// gateway before config sections are applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HermesCredentialAction {
    /// `model.save_key`: store an API key in the profile's `.env`. Only valid
    /// for providers whose `auth_type` is `api_key`.
    SaveApiKey {
        profile: String,
        provider: String,
        api_key: String,
    },
    /// `model.disconnect`: remove the provider's credentials from the
    /// profile. Auto-harvested sources (e.g. Copilot via the `gh` CLI login)
    /// may be re-detected by Hermes afterwards.
    Disconnect { profile: String, provider: String },
}
