# Host Settings

Server-owned host settings delivered over the `/host/*` stream. Builds on
`01-philosophy.md`, `02-protocol.md`, and `04-host-registry.md`.

Managed mobile broker access has an additional product/service boundary:
`30-mobile-managed-broker.md` owns the Tyggs Pass, `tycode.dev`, and AWS IoT
contract. Host settings may expose server-owned mobile controls, but they must
not store Tyggs account data, pass proofs, billing state, or production broker
fallbacks that bypass `tycode.dev`.

---

## Problem

The rewrite had a settings overlay, but backend configuration was still local
frontend state:

- The frontend chose a default backend with a local signal.
- The backend settings page rendered static cards.
- New-chat behavior depended on frontend-local state rather than host state.
- There was no protocol event for current host settings and no persistence in
  `tyde-server`.

That violated the design rules:

- The server must own behavior; the UI only renders state.
- State flows through events, not hidden caches.
- Everything must use protocol types end-to-end.

The fix is a typed host settings model in `settings-model`, persisted and owned
by `tyde-server`, replayed on connect, and updated through compare-and-swap
host-stream operations.

---

## Scope

The persisted host-settings document covers:

- `enabled_backends`
- `default_backend`
- native voice enablement, AWS profile/region selectors, and exact Nova model

These are host-level settings, not per-session settings or frontend
preferences. Backend-native settings are also exposed on the host stream, but
remain a separate backend-owned domain and are never written into the host
settings store.

Out of scope for this slice:

- MCP settings
- agent definitions
- notifications
- advanced settings
- host registry / multi-host selection

---

## Protocol Model

Host settings are strongly typed in `settings-model/src/lib.rs`; protocol owns
only the generic transport payloads.

### Types

```rust
pub struct HostSettings {
    pub enabled_backends: Vec<BackendKind>,
    pub default_backend: Option<BackendKind>,
    pub voice: VoiceSettings,
}
```

`BackendKind` is already an enum, so no stringly-typed backend identifiers are
introduced here.

### Input Events

Clients update settings on `/host/<uuid>` with `FrameKind::SettingsWrite`.
Each `SettingOp::Replace` or `SettingOp::Remove` carries an RFC 6901 path and a
mandatory value, version-token, or absent compare-and-swap expectation. A
write is atomic: overlapping operations or any failed expectation reject the
whole batch.

Profile create/delete and similar backend lifecycle operations use
`FrameKind::InvokeSettingsAction`, not document writes.

### Output Event

The server emits:

```rust
FrameKind::HostSettings
```

Payload:

```rust
pub struct HostSettingsPayload<HostSettings> {
    pub settings: HostSettings,
    pub etag: String,
    pub configured_secrets: Vec<ConfiguredSecret>,
}
```

`HostBootstrap` supplies initial state; `HostSettings` is the authoritative
live-update shape.

---

## Semantics

### Bootstrap and snapshots

`HostBootstrap` carries the initial redacted settings document, its etag, the
build-static JSON Schema, and revision tokens for configured write-only paths.
An applied write fans out `HostSettings` with the new full redacted snapshot,
etag, and configured-secret tokens.

### `settings_write`

Under one settings-apply lock, the host checks path knowledge, secret rules,
overlap, and every CAS expectation; applies the batch to a candidate document;
deserializes and validates the typed model; commits the host store; propagates
backend effects; and publishes the snapshot. The requester alone receives a
correlated `SettingsWriteResult`. Semantic rejection is a typed result, not a
`CommandError`.

The client does not optimistically replace server-owned settings. It waits for
the authoritative fanout.

## Invariants

The server enforces:

- `enabled_backends` may be empty
- if `default_backend` is set, it must be present in `enabled_backends`

Normalization rules:

- backend lists are canonicalized in fixed enum order
- duplicate/unknown values are not preserved
- if the store file is missing, the server returns an empty settings snapshot
- invalid persisted settings fail load instead of being silently repaired

There is no protocol-level backend default in this slice.

If `enabled_backends` is changed such that the current `default_backend` is no
longer enabled, the host clears `default_backend` to `null` and emits that
updated snapshot.

---

## Persistence

Host settings are persisted in a dedicated server-owned store:

- default path: `~/.tyde/settings.json`
- override: `TYDE_SETTINGS_STORE_PATH`

This lives beside the existing session and project stores, but remains a
separate file because it is a separate domain model.

Current store shape:

```json
{
  "settings": {
    "enabled_backends": [],
    "default_backend": null
  }
}
```

The store is read on demand and replaced atomically on write, matching the
existing store pattern used by sessions and projects.

---

## Server Ownership

The host actor owns the settings lifecycle.

### Registration Replay

When a host stream is registered, the host replays:

1. `host_settings`
2. existing projects
3. existing agents

That order is intentional:

- settings are general host state
- projects are host-owned inventory
- agents are runtime instances

This keeps startup state replay aligned with the "events in, events out" model.

### Mutation Fanout

When settings change, the host fans out the latest `host_settings` snapshot to
all connected host subscribers.

No special-case frontend refresh logic is required beyond normal event handling.

---

## Frontend Data Flow

The frontend stores host-keyed settings, schemas, and configured-secret tokens.
They are seeded by `HostBootstrap`; settings and secret tokens are replaced by
each `HostSettings` fanout.

### Settings Overlay

The settings overlay renders complex editors from the typed model and primitive
rows from the server-published schema. Edits emit path-scoped `SettingsWrite`
operations against the selected host.

### Runtime Behavior

`spawn_new_chat` reads `host_settings.default_backend` when creating a new
agent. If no default is configured, the frontend does not invent one.

That is the intended direction: settings should affect runtime via server-owned
state, not isolated frontend preferences.

---

## Why This Design

This design follows the philosophy document directly:

- One source of truth: `HostSettings` lives in `settings-model`.
- Server owns behavior: persistence, validation, and fanout all happen in
  `tyde-server`.
- UI only renders state: the frontend does not own backend settings anymore.
- Bootstrap supplies initial state; `host_settings` supplies authoritative
  updates.
- The protocol transports generic JSON operations while server and clients share
  the typed settings model.

---

## Next Steps

Natural extensions add typed fields to `HostSettings`, expose primitive rows
through its schema annotations, and keep complex editors in Rust. No new wire
enum is needed for ordinary fields. Settings remain server-owned, and schema
validation on the client is advisory; the server is authoritative.
