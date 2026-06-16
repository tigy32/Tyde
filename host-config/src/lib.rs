//! Shared host-configuration types.
//!
//! These types describe how the Tauri shell and the WASM frontend talk about
//! *configured hosts* — the user's list of local/remote endpoints and the
//! transport used to reach each one. They are persisted by the shell to
//! `~/.tyde/configured_hosts.json` and also serialized across the
//! `tauri::invoke` boundary to the WASM frontend.
//!
//! Per `dev-docs/01-philosophy.md` there must be one source of truth for any
//! wire-crossing type. These types intentionally live in their own tiny crate
//! (no tokio, no tauri, no wasm deps) so both sides can depend on them and
//! field-mismatch bugs between the shell and the frontend become impossible.
//!
//! NOTE: This is *not* the Tyde wire protocol (see `dev-docs/02-protocol.md`).
//! These types are shell-owned transport/UI config per
//! `dev-docs/12-remote-hosts.md`; they are not NDJSON frame payloads for a
//! connected host.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};

/// Identifier for the always-present local embedded host.
pub const LOCAL_HOST_ID: &str = "local";

/// Tyde release identifier without the leading GitHub tag `v`.
///
/// Stable releases look like `0.8.19`; prereleases look like
/// `0.8.20-beta.1`. Managed remotes use this value both to resolve the
/// matching GitHub release tag (`v{version}`) and as the remote install
/// directory name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TydeReleaseVersion(String);

impl TydeReleaseVersion {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
        validate_release_version(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn github_tag(&self) -> String {
        format!("v{}", self.0)
    }
}

impl fmt::Display for TydeReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TydeReleaseVersion {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for TydeReleaseVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TydeReleaseVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ReleaseVersionVisitor;

        impl<'de> Visitor<'de> for ReleaseVersionVisitor {
            type Value = TydeReleaseVersion;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Tyde release version like 0.8.19 or 0.8.20-beta.1")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                TydeReleaseVersion::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(ReleaseVersionVisitor)
    }
}

fn validate_release_version(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("release version must not be empty".to_string());
    }
    if value.contains('/') || value.contains('\\') {
        return Err("release version must not contain path separators".to_string());
    }
    if value.chars().any(char::is_whitespace) {
        return Err("release version must not contain whitespace".to_string());
    }

    let (core, prerelease) = value
        .split_once('-')
        .map_or((value, None), |(core, prerelease)| (core, Some(prerelease)));
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return Err("release version must start with numeric major.minor.patch".to_string());
    }

    if let Some(prerelease) = prerelease
        && (prerelease.is_empty()
            || prerelease.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            }))
    {
        return Err(
            "release prerelease identifiers may contain only ASCII letters, digits, and hyphens"
                .to_string(),
        );
    }
    Ok(())
}

/// Who owns the remote Tyde daemon lifecycle for an SSH host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteHostLifecycleConfig {
    /// The user-provided remote command is responsible for reaching a Tyde host.
    #[default]
    Manual,
    /// The desktop shell may install versioned Tyde binaries under
    /// `~/.tyde/bin/<version>/tyde`, maintain `~/.tyde/bin/current`, and launch
    /// the remote `tyde host --uds` daemon when needed.
    ///
    /// Managed remotes always install the exact release that built the current
    /// desktop app, so the frontend and remote server stay in lockstep.
    ManagedTyde,
}

/// How the shell should reach a configured host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostTransportConfig {
    /// In-process host running inside the shell itself. There is always
    /// exactly one of these, with id = `LOCAL_HOST_ID`.
    LocalEmbedded,
    /// Spawn `ssh <destination> [remote_command]` and speak NDJSON over its
    /// stdio streams. For persistent remote hosts, the remote command is a
    /// thin bridge like `tyde host --bridge-uds`.
    SshStdio {
        ssh_destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_command: Option<String>,
        #[serde(default)]
        lifecycle: RemoteHostLifecycleConfig,
    },
}

/// A single entry in the configured-host list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredHost {
    pub id: String,
    pub label: String,
    pub transport: HostTransportConfig,
    pub auto_connect: bool,
}

/// The full persisted list plus UI-selected host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredHostStore {
    pub hosts: Vec<ConfiguredHost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_host_id: Option<String>,
}

/// Request body for adding or updating a configured host.
///
/// Sent by the frontend, consumed by the shell's `upsert_configured_host`
/// tauri command. When `id` is `None` the shell generates one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertConfiguredHostRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub label: String,
    pub transport: HostTransportConfig,
    pub auto_connect: bool,
}

// ── Tauri bridge request/event types ────────────────────────────────────────
//
// Shared between the tauri-shell (serializes/emits) and the WASM frontend
// (deserializes/receives). One definition here makes field-mismatch bugs
// between the two sides impossible.

/// Generic single-host-id request (connect, disconnect, remove).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostIdRequest {
    pub host_id: String,
}

/// Request to send a line of text to a connected host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendHostLineRequest {
    pub host_id: String,
    pub line: String,
}

/// Request to change the UI-selected host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSelectedHostRequest {
    pub host_id: Option<String>,
}

/// Tauri event payload: a line of NDJSON from a connected host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostLineEvent {
    pub host_id: String,
    pub line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_instance_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<u64>,
}

/// Tauri event payload: a host connection was dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDisconnectedEvent {
    pub host_id: String,
}

/// Tauri event payload: a host connection encountered an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostErrorEvent {
    pub host_id: String,
    pub message: String,
}

/// The remote operating system/architecture pair probed over SSH.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePlatform {
    pub os: RemoteOperatingSystem,
    pub arch: RemoteArchitecture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOperatingSystem {
    Linux,
    Macos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteArchitecture {
    X86_64,
    Aarch64,
}

/// Running-state information for the managed remote daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTydeRunningState {
    NotRunning,
    /// Running daemon launched by Tyde's managed lifecycle path.
    Managed {
        version: TydeReleaseVersion,
    },
    /// A socket exists, but the shell cannot prove it owns the daemon.
    UnknownSocket,
}

/// Point-in-time status of a managed remote Tyde installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHostLifecycleSnapshot {
    pub target_version: TydeReleaseVersion,
    pub installed_target: bool,
    pub current_link_version: Option<TydeReleaseVersion>,
    pub running: RemoteTydeRunningState,
    pub platform: RemotePlatform,
}

/// Concrete lifecycle step surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteHostLifecycleStep {
    ProbePlatform,
    ResolveRelease,
    ProbeInstallation,
    DownloadAsset,
    InstallBinary,
    StopOldServer,
    LaunchServer,
    VerifyRunning,
    Connect,
}

/// UI-visible lifecycle status for a configured host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteHostLifecycleStatus {
    Idle,
    Running {
        step: RemoteHostLifecycleStep,
        target_version: Option<TydeReleaseVersion>,
    },
    Snapshot {
        snapshot: RemoteHostLifecycleSnapshot,
    },
    Error {
        message: String,
    },
}

/// Tauri event payload: managed remote lifecycle state changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostLifecycleEvent {
    pub host_id: String,
    pub status: RemoteHostLifecycleStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions_accept_prereleases() -> Result<(), Box<dyn std::error::Error>> {
        let stable = TydeReleaseVersion::parse("v0.8.19")?;
        assert_eq!(stable.as_str(), "0.8.19");
        assert_eq!(stable.github_tag(), "v0.8.19");

        let beta = TydeReleaseVersion::parse("0.8.20-beta.1")?;
        assert_eq!(beta.to_string(), "0.8.20-beta.1");
        assert_eq!(beta.github_tag(), "v0.8.20-beta.1");

        assert!(TydeReleaseVersion::parse("0.8").is_err());
        assert!(TydeReleaseVersion::parse("../0.8.19").is_err());
        assert!(TydeReleaseVersion::parse("0.8.19 beta").is_err());
        Ok(())
    }

    #[test]
    fn managed_lifecycle_accepts_legacy_release_field() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{"kind":"managed_tyde","release":{"kind":"latest"}}"#;
        let decoded: RemoteHostLifecycleConfig = serde_json::from_str(json)?;
        assert_eq!(decoded, RemoteHostLifecycleConfig::ManagedTyde);
        let encoded = serde_json::to_string(&decoded)?;
        assert_eq!(encoded, r#"{"kind":"managed_tyde"}"#);

        let pinned = r#"{"kind":"managed_tyde","release":{"kind":"version","version":{"major":0,"minor":8,"patch":7}}}"#;
        let decoded: RemoteHostLifecycleConfig = serde_json::from_str(pinned)?;
        assert_eq!(decoded, RemoteHostLifecycleConfig::ManagedTyde);
        Ok(())
    }

    #[test]
    fn host_line_delivery_id_is_backward_compatible() -> Result<(), Box<dyn std::error::Error>> {
        let legacy = r#"{"hostId":"h1","line":"{}"}"#;
        let decoded: HostLineEvent = serde_json::from_str(legacy)?;
        assert_eq!(decoded.connection_instance_id, None);
        assert_eq!(decoded.delivery_id, None);
        let encoded = serde_json::to_string(&decoded)?;
        assert!(!encoded.contains("connectionInstanceId"));
        assert!(!encoded.contains("deliveryId"));

        let event = HostLineEvent {
            host_id: "h1".to_owned(),
            line: "{}".to_owned(),
            connection_instance_id: Some(3),
            delivery_id: Some(7),
        };
        let encoded = serde_json::to_string(&event)?;
        assert!(encoded.contains(r#""connectionInstanceId":3"#));
        assert!(encoded.contains(r#""deliveryId":7"#));
        Ok(())
    }
}
