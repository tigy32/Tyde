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
//! `dev-docs/12-remote-hosts.md`; the protocol crate is reserved for NDJSON
//! envelope framing over a connected host.

use serde::{Deserialize, Serialize};

/// Identifier for the always-present local embedded host.
pub const LOCAL_HOST_ID: &str = "local";

/// How the shell should reach a configured host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostTransportConfig {
    /// In-process host running inside the shell itself. There is always
    /// exactly one of these, with id = `LOCAL_HOST_ID`.
    LocalEmbedded,
    /// Spawn `ssh <destination> [remote_command]` and speak NDJSON over its
    /// stdio streams.
    SshStdio {
        ssh_destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_command: Option<String>,
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
