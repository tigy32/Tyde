# Host Settings

Server-owned host settings delivered over the `/host/*` stream. Builds on
`01-philosophy.md`, `02-protocol.md`, and `04-host-registry.md`.

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

The fix is a typed host settings model in `protocol`, persisted and owned by
`tyde-server`, replayed on connect, and updated through typed host-stream
events.

---

## Scope

This first slice only covers:

- `enabled_backends`
- `default_backend`

These are host-level settings, not per-session settings and not frontend
preferences.

Out of scope for this slice:

- provider settings
- MCP settings
- agent definitions
- notifications
- advanced settings
- host registry / multi-host selection

---

## Protocol Model

Host settings are strongly typed in `protocol/src/types.rs`.

### Types

```rust
pub struct HostSettings {
    pub enabled_backends: Vec<BackendKind>,
    pub default_backend: BackendKind,
}
```

`BackendKind` is already an enum, so no stringly-typed backend identifiers are
introduced here.

### Input Events

These are sent on the connection's `/host/<uuid>` stream:

```rust
FrameKind::DumpSettings
FrameKind::SetSetting
```

Payloads:

```rust
pub struct DumpSettingsPayload {}

pub struct SetSettingPayload {
    pub setting: HostSettingValue,
}

pub enum HostSettingValue {
    EnabledBackends { enabled_backends: Vec<BackendKind> },
    DefaultBackend { default_backend: BackendKind },
}
```

### Output Event

The server emits:

```rust
FrameKind::HostSettings
```

Payload:

```rust
pub struct HostSettingsPayload {
    pub settings: HostSettings,
}
```

There is exactly one settings snapshot event shape. Initial state and updates
use the same event model.

---

## Semantics

### `dump_settings`

Client asks the host to emit the current settings snapshot on the same host
stream.

This is not a request/response abstraction layered outside the protocol. It is
just an input event that causes the server to emit the current state as a normal
output event.

### `set_setting`

Client sends a typed mutation request for one setting domain.

The host:

1. loads current settings
2. applies the typed update
3. validates invariants
4. persists the updated snapshot
5. emits `host_settings` with the latest full snapshot to all subscribers

The client does not optimistically mutate backend settings locally. It waits for
the resulting `host_settings` event.

### `host_settings`

This is the source of truth for the frontend. The latest event fully replaces
previous frontend state.

---

## Invariants

The server enforces:

- `enabled_backends` must not be empty
- `default_backend` must be present in `enabled_backends`

Normalization rules:

- backend lists are canonicalized in fixed enum order
- duplicate/unknown values are not preserved
- if persisted data is missing or invalid, defaults are normalized to a valid
  snapshot

Defaults for this slice:

- enabled: `claude`, `codex`, `gemini`
- default: `claude`

If `enabled_backends` is changed such that the current `default_backend` is no
longer enabled, the host automatically selects the first enabled backend as the
new default and emits that updated snapshot.

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
    "enabled_backends": ["claude", "codex", "gemini"],
    "default_backend": "claude"
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

The frontend stores:

```rust
pub host_settings: RwSignal<Option<HostSettings>>
```

That signal is populated only from `FrameKind::HostSettings`.

### Settings Overlay

When the settings overlay opens, the frontend sends `dump_settings` on the host
stream. The Backends tab renders from `host_settings` and sends typed
`set_setting` events for:

- toggling backend enablement
- selecting the default backend

### Runtime Behavior

`spawn_new_chat` reads `host_settings.default_backend` when creating a new
agent. This is the first place where host settings directly affect runtime
behavior in the rewrite.

That is the intended direction: settings should affect runtime via server-owned
state, not isolated frontend preferences.

---

## Why This Design

This design follows the philosophy document directly:

- One source of truth: `HostSettings` lives in `protocol`.
- Server owns behavior: persistence, validation, and fanout all happen in
  `tyde-server`.
- UI only renders state: the frontend does not own backend settings anymore.
- Initial state and live updates share one event shape: `host_settings`.
- No parallel mirror types: the same protocol types are used across protocol,
  server, and frontend.

---

## Next Steps

Natural extensions of this design:

- add more typed host settings fields to `HostSettings`
- add more `HostSettingValue` variants for partial typed updates
- wire host settings into more runtime decisions beyond new-chat
- expand the Backends tab with dependency/install state once that domain exists
- introduce host registry and selected-host state above this layer without
  changing the per-host settings model

The important constraint is unchanged: new settings must be added as typed
protocol fields/events and remain server-owned.
