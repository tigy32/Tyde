# Frontend / Shell Boundary

This document freezes the frontend boundary for Tyde2.

It follows `01-philosophy.md` directly:

- one source of truth
- server owns behavior
- transport layers stay dumb
- frontend renders state

## Crate Roles

### `frontend`

The `frontend` crate is the Rust frontend.

It owns:

- protocol types from the `protocol` crate
- construction of protocol messages
- parsing of protocol messages
- frontend state derived from protocol events
- all UI behavior and rendering

If the frontend wants to send `hello`, `spawn_agent`, `project_refresh`, or any
future protocol frame, the `frontend` crate builds that message itself.

### `tauri-shell`

The `tauri-shell` crate is a Tauri transport shell nested under `frontend/`.

It owns only:

- opening connections to hosts
- closing connections to hosts
- forwarding raw newline-delimited JSON lines from the GUI to a host
- forwarding raw newline-delimited JSON lines from a host back to the GUI

It does **not** own:

- `FrameKind`
- `Envelope`
- payload structs
- sequence counters
- agent/project/session state
- protocol branching
- backend semantics

The shell is a byte/line proxy with host connection ownership.

## API Shape

The shell API is intentionally protocol-agnostic:

- `connect_host`
- `disconnect_host`
- `send_host_line`
- emit `tyde://host-line`
- emit `tyde://host-disconnected`
- emit `tyde://host-error`

The payload crossing the shell boundary is just:

- host identity
- transport config
- raw NDJSON line text

That is the whole point. If the shell can understand a Tyde protocol frame,
the boundary has already been violated.

## Consequence

If a future feature needs new protocol data:

1. add it in `protocol`
2. implement it in `server`
3. handle it in `frontend`

Do **not** add interpretation logic to `tauri-shell`.
