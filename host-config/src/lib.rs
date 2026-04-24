//! Shared host-configuration types.
//!
//! These types describe how the Tauri shell and the WASM frontend talk about
//! *configured hosts* — the user's list of local/remote endpoints and the
//! transport used to reach each one. They are persisted by the shell to
//! `~/.tyde/configured_hosts.json` and also serialized across the
//! `tauri::invoke` boundary to the WASM frontend. The only protocol type reused
//! here is `Version`, so semver values stay strongly typed everywhere.
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

use protocol::Version;
use serde::{Deserialize, Serialize};

/// Identifier for the always-present local embedded host.
pub const LOCAL_HOST_ID: &str = "local";

/// Which Tyde release the shell should install for a managed remote host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TydeReleaseTarget {
    /// Resolve the current latest GitHub release at lifecycle time.
    #[default]
    Latest,
    /// Install and launch one exact release version.
    Version { version: Version },
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
    ManagedTyde {
        #[serde(default)]
        release: TydeReleaseTarget,
    },
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
        version: Version,
    },
    /// A socket exists, but the shell cannot prove it owns the daemon.
    UnknownSocket,
}

/// Point-in-time status of a managed remote Tyde installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHostLifecycleSnapshot {
    pub target_version: Version,
    pub installed_target: bool,
    pub current_link_version: Option<Version>,
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
        target_version: Option<Version>,
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
