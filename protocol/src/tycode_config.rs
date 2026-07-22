//! Typed document schema for the Tycode backend-native settings snapshot.
//!
//! The document travels as the opaque `settings` value inside
//! [`crate::BackendNativeSettingsSnapshot`] (server → client) and
//! [`crate::HostSettingValue::BackendNativeSettings`] (client → server). Both
//! sides deserialize it into these types; the wire frames stay generic.
//!
//! Server snapshots describe every discovered Tycode profile: the shared
//! `~/.tycode/settings.toml` (the default profile) plus every
//! `~/.tycode/profiles/<name>.toml` file. Each profile's `settings` object is
//! exactly what the pinned Tycode subprocess reports for that file — Tyde
//! never rewrites, projects, or copies Tycode settings; edits are validated
//! and persisted by the Tycode subprocess against the real file. The form
//! schema for `settings` rides separately in the snapshot's generic `groups`
//! field (it is identical for every profile because one pinned binary serves
//! them all).

use serde::{Deserialize, Serialize};

/// Version stamp for [`TycodeNativeSettingsDoc`]. Bump on breaking shape
/// changes so an old client save can be rejected instead of misapplied.
pub const TYCODE_NATIVE_SETTINGS_VERSION: u32 = 1;

/// Profile name of the shared settings file (`~/.tycode/settings.toml`).
pub const TYCODE_DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TycodeNativeSettingsDoc {
    pub version: u32,
    /// Discovery order: the default profile first, named profiles sorted.
    pub profiles: Vec<TycodeProfileSettings>,
    /// Profile file operations, executed before per-profile settings saves.
    /// Never present in server snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<TycodeProfileAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TycodeProfileSettings {
    /// `"default"` for the shared settings file, else the
    /// `profiles/<name>.toml` file stem.
    pub name: String,
    /// Absolute settings file backing this profile. Server-owned.
    pub settings_path: String,
    /// The profile's current settings exactly as reported by the pinned
    /// Tycode subprocess for its settings file.
    pub settings: serde_json::Value,
    /// On save: the unedited settings this edit was based on (the snapshot
    /// the client loaded). The server refuses the save when the profile's
    /// current settings no longer match it, so a stale draft cannot silently
    /// overwrite concurrent changes. Absent in server snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_settings: Option<serde_json::Value>,
}

/// Write-only profile file operation, executed before settings saves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TycodeProfileAction {
    /// Create `profiles/<name>.toml` as a byte-for-byte copy of the
    /// `copy_from` profile's settings file (default: the default profile).
    CreateProfile {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        copy_from: Option<String>,
    },
    /// Delete `profiles/<name>.toml`. The default profile cannot be deleted.
    DeleteProfile { name: String },
}
