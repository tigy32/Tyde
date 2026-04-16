# Remote Tyde Hosts

This document specifies how Tyde2 should support multiple configured hosts,
including remote Tyde servers reached over SSH, while keeping the existing
rewrite boundaries intact.

It builds on:

- `01-philosophy.md`
- `02-protocol.md`
- `06-projects.md`
- `07-project-stream.md`
- `08-gui-shell-boundary.md`
- `09-host-settings.md`

---

## 1. Problem

The current rewrite is structurally single-host:

- `frontend/src/app.rs` hardcodes one `"local"` host connection
- `frontend/src/state.rs` stores one `host_id`, one `host_stream`, and one
  `host_settings`
- `frontend/tauri-shell/src/lib.rs` only exposes `connect_host(host_id)` and
  always connects to the embedded local host
- `frontend/src/components/home_view.rs` and
  `frontend/src/components/project_rail.rs` render one flat local project list

At the same time, the repo already contains the pieces that show the intended
direction:

- `server` owns the real state model for projects, sessions, agents, terminals,
  and settings
- `tauri-shell` is already a raw line proxy with host-keyed routing
- `server/src/remote.rs` and several backend implementations already know about
  SSH, but only as a backend-side subprocess escape hatch
- the old app grouped projects by host and treated remote Tyde servers as
  first-class runtime entities

We now want to support **remote Tyde servers**, not just remote subprocesses.

That means:

1. the shell needs a small persisted registry of configured hosts
2. the frontend needs to connect to more than one host at once
3. project inventory must come from every connected host and be shown grouped
   by host in the Projects surface
4. project/file/git/terminal/chat actions must route to the owning host

The important architectural constraint is unchanged:

**"Remote" is another host connection, not a frontend special case and not a
new layer of business logic in Tauri.**

---

## 2. Goals

### 2.1 Product goals

- Support one embedded local host plus zero or more configured remote hosts.
- Persist configured host transport details in the shell layer.
- Allow users to add, edit, remove, connect, and disconnect hosts from the UI.
- Show projects from all connected hosts in the Projects tab, grouped by host.
- Route project-open, file-read, git, terminal, session-resume, and new-chat
  actions to the correct host automatically.

### 2.2 Architectural goals

- Keep `server` single-host. Do not turn one `HostHandle` into a multi-host
  registry.
- Keep host/product state server-owned on each host.
- Keep Tauri protocol-agnostic. It may store transport config and own transport
  lifetimes, but it must not parse Tyde frames or cache project/session state.
- Do not use `ssh://...` roots as the primary abstraction for remote hosts.
  A remote Tyde server should expose its own native paths and its own stores.

---

## 3. Non-Goals

This slice does **not** try to solve everything the old app did.

- No remote install/upgrade/orchestration UI in the first slice.
- No shell-owned project/session mirrors.
- No single global server process that proxies multiple remote hosts.
- No expansion of backend-side SSH path hacks as the long-term remote model.
- No Tauri-side understanding of `FrameKind`, projects, sessions, agents, or
  terminals.

If we need remote Tyde server bootstrap/install workflows later, they should be
added as a separate design on top of the host registry introduced here.

---

## 4. Core Decision

**Remote host support in Tyde2 should be implemented as multiple independent
host protocol connections, one per configured host.**

Each configured host is one of:

- the embedded local host inside the desktop shell
- a remote Tyde server reached through an SSH-backed byte stream

Each host remains a normal Tyde host:

- it owns its own `HostSettings`
- it owns its own `projects.json`
- it owns its own `sessions.json`
- it owns its own agents, terminals, and project streams

The desktop app aggregates those host-local states in the frontend.

This is the key difference from the current `ssh://` backend helpers:

- today: one local host sometimes reaches through SSH to run backend commands
- target: many hosts, each one authoritative for its own projects and runtime

That target is much closer to the rewrite philosophy.

---

## 5. Why The Existing `ssh://` Path Model Is Not Enough

The current repo already has SSH helpers in `server/src/remote.rs` and backend
implementations that parse `ssh://host/path` roots. That is useful background,
but it is the wrong foundation for multi-host product support.

Why:

- Project ownership stays local, so the local host would still be pretending to
  own remote project inventory.
- Project streams in `server/src/project_stream.rs` currently use local `std::fs`
  and local `git`. They do not become remote-safe just because a root string
  contains `ssh://`.
- Terminals in `server/src/terminal_stream.rs` are local PTYs. They are not
  remote terminals.
- The frontend still cannot show host-grouped inventory because host identity
  is missing from the app model.

So this document explicitly does **not** extend the `ssh://` workspace-root
scheme into the main remote-host architecture.

Remote host support should instead rely on a real remote Tyde host connection.

---

## 6. Layer Ownership

### 6.1 `server`

`server` remains a **single-host** state owner.

It continues to own:

- host settings
- project store
- session store
- agent registry
- project streams
- terminal streams

It does **not** gain:

- a configured-host registry
- SSH alias persistence
- selected-host UI state
- cross-host aggregation logic

Those are not single-host product concerns.

### 6.2 `frontend/tauri-shell`

The shell owns only:

- the configured-host store
- transport configuration
- opening and closing host byte streams
- proxying raw NDJSON lines between frontend and host
- per-host connection lifecycle

It may know:

- host identity used by the app (`ConfiguredHostId`)
- transport type (`local_embedded`, `ssh_stdio`)
- transport settings (`ssh destination`, optional remote command override)

It must **not** know:

- protocol message kinds
- projects
- sessions
- settings semantics
- agent state
- terminal state

That remains exactly in line with `08-gui-shell-boundary.md`.

### 6.3 `frontend`

The frontend becomes the aggregation layer across many host connections.

It owns:

- per-host connection state
- per-host host-stream identity
- host-grouped derived UI state
- routing a user action to the correct host connection

It does **not** own:

- persistence of host settings or project/session data
- invented cross-host business logic

It only renders and routes host-owned state.

---

## 7. Shell-Owned Configured Host Store

We need a small shell-local store for configured hosts.

This is **not** a `server` store, but it should still live under `~/.tyde` so
the desktop-facing state stays in one coherent place.

That means the ownership boundary is logical, not directory-based:

- `settings.json`, `projects.json`, and `sessions.json` remain host-owned
  server state
- `configured_hosts.json` is shell-owned transport/UI state

Configured host transport still belongs in the shell. Co-locating it under
`~/.tyde` is a consistency choice, not a change in ownership.

### 7.1 Store model

Suggested shell types:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConfiguredHostId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfiguredHost {
    pub id: ConfiguredHostId,
    pub label: String,
    pub transport: HostTransportConfig,
    pub auto_connect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostTransportConfig {
    LocalEmbedded,
    SshStdio {
        /// Usually an ssh config alias such as "workbox" or "prod-devbox".
        ssh_destination: String,

        /// Optional remote command override. If absent, the shell uses the
        /// default Tyde host-stdio command.
        remote_command: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfiguredHostStore {
    pub hosts: Vec<ConfiguredHost>,
    pub selected_host_id: Option<ConfiguredHostId>,
}
```

`ConfiguredHostId` is the same identity carried today as `host_id` through the
bridge/router layer and existing shell events. We should keep that alignment
instead of inventing a second shell-local routing key.

### 7.2 Persistence

The store should live at:

- `~/.tyde/configured_hosts.json`

And it should follow the same disk-first pattern as the other stores in the
rewrite:

- do not keep an in-memory cache as the source of truth
- read from disk for each operation
- write atomically when mutating

### 7.3 Default local host

The shell should guarantee one local host record exists:

```rust
ConfiguredHost {
    id: ConfiguredHostId("local".to_string()),
    label: "Local".to_string(),
    transport: HostTransportConfig::LocalEmbedded,
    auto_connect: true,
}
```

Rules:

- the local host cannot be removed
- the local host may be renamed later if we want, but v1 can keep `"Local"`
- `selected_host_id` defaults to `local` when missing

### 7.4 Store semantics

The shell store is intentionally narrow.

It contains:

- display label
- transport type
- SSH destination / alias
- auto-connect behavior
- selected host for host-scoped settings surfaces

It does **not** contain:

- enabled backends
- default backend
- project lists
- session lists
- connection-derived status snapshots

Those remain elsewhere.

---

## 8. Shell API Changes

The shell boundary stays protocol-agnostic, but it needs host-registry commands.

### 8.1 New Tauri commands

Suggested additions:

- `list_configured_hosts`
- `upsert_configured_host`
- `remove_configured_host`
- `set_selected_host`
- `connect_configured_host`
- `disconnect_host`
- `send_host_line`

The existing `send_host_line` shape is still correct because it is already
host-keyed and raw-line based.

### 8.2 Host registry events

Suggested shell events:

- `tyde://configured-hosts-changed`
- `tyde://host-connection-state`
- existing `tyde://host-line`
- existing `tyde://host-disconnected`
- existing `tyde://host-error`

`host-connection-state` should be transport-level only:

```rust
pub enum HostConnectionState {
    Connecting,
    Connected,
    Disconnected,
    Error { message: String },
}
```

That gives the frontend enough information to render grouped host sections even
before a protocol handshake completes.

### 8.3 Router behavior

`frontend/tauri-shell/src/router.rs` already keys connections by `host_id`.
That is the right shape and should be preserved.

The router should generalize its connect path:

- `LocalEmbedded`:
  use the current in-process duplex wiring into `server::accept` and
  `server::run_connection`
- `SshStdio`:
  spawn `ssh -T <ssh_destination> <remote_command>` and hand the child
  stdin/stdout to the same `connection_actor`

The important point is that both transports end up as the same raw line stream.

---

## 9. Remote Transport Contract

Remote host support requires a remote Tyde host endpoint that speaks the normal
Tyde wire protocol over a byte stream.

For the first slice, the cleanest contract is:

```text
ssh -T <destination> <tyde-host-stdio-command>
```

Where the remote command is something like:

- `tyde-server host-stdio`
- or `tyde host --stdio`

The exact binary/CLI name can be decided separately, but the architectural
requirement is clear:

**the remote endpoint must be a real Tyde host server, not a remote Tauri shell
and not a backend-specific SSH shim.**

This aligns directly with `02-protocol.md`, which already defines the transport
in terms of a generic bidirectional byte stream carrying NDJSON frames.

### 9.1 Why stdio-over-SSH

It fits the current architecture well:

- one byte stream
- no extra TCP exposure requirement
- no protocol changes
- easy to proxy in `tauri-shell`

### 9.2 Remote path semantics

Once connected to a remote host:

- project roots are plain remote-native paths like `/home/mike/src/app`
- session workspace roots are remote-native paths
- terminals launch on the remote host
- project streams read the remote filesystem because the remote server owns them

The UI does **not** construct `ssh://...` paths for remote-host project flows.

---

## 10. Frontend State Model

The current frontend state is single-host and must be split into per-host state.

### 10.1 New app-level shape

Suggested direction:

```rust
pub struct HostUiState {
    pub connection_status: ConnectionStatus,
    pub host_stream: Option<StreamPath>,
    pub host_settings: Option<HostSettings>,
    pub projects: Vec<Project>,
    pub agents: Vec<AgentInfo>,
    pub sessions: Vec<SessionSummary>,
    pub terminals: Vec<TerminalInfo>,
}

pub struct HostProjectRef {
    pub host_id: ConfiguredHostId,
    pub project_id: ProjectId,
}

pub struct HostAgentRef {
    pub host_id: ConfiguredHostId,
    pub agent_id: AgentId,
}
```

And `AppState` should move from singular fields to maps keyed by configured host
ID, for example:

- `configured_hosts`
- `selected_host_id`
- `hosts: HashMap<ConfiguredHostId, HostUiState>`
- `active_project: Option<HostProjectRef>`
- `active_agent: Option<HostAgentRef>`

The existing single global `projects`, `sessions`, `agents`, `host_settings`,
and `host_stream` signals are not sufficient.

### 10.2 Host-scoped routing

Any action that currently reads:

- `state.host_id`
- `state.host_stream`
- `state.active_project_id`
- `state.active_agent_id`

must instead route through a host-scoped reference.

Examples:

- opening a project file
- refreshing a project stream
- staging a file
- creating a terminal
- spawning a project chat
- resuming a session

If the active project belongs to host `workbox`, the action must use that
host's connection and stream namespace automatically.

---

## 11. Sequence Number Namespacing

Multi-host support introduces a subtle but important correctness issue.

Today:

- outbound sequence state in `frontend/src/send.rs` is keyed only by
  `StreamPath`
- inbound sequence validation in `frontend/src/dispatch.rs` is also keyed only
  by `StreamPath`

That works only because the app currently has one connection.

With multiple hosts, stream paths will collide naturally:

- every connection has its own `/host/<uuid>`
- different hosts can both have `/project/<project_id>`
- different hosts can both have `/agent/<agent_id>/<instance_id>`

So sequence tracking must be keyed by:

```text
(configured_host_id, stream_path)
```

Both outbound and inbound validators must use that composite key.

This is a required change for correctness, not a cleanup item.

---

## 12. Protocol Impact

### 12.1 First-slice protocol changes

No wire-format changes are strictly required for the first useful slice.

Why:

- each connection still represents exactly one host
- host routing already happens at the shell/frontend boundary via `host_id`
- all existing host-owned payloads (`HostSettings`, `ProjectNotify`, agent
  events, session lists) remain valid per connection

That lets us land multi-host support without blocking on a protocol migration.

### 12.2 Important note on ownership

`01-philosophy.md` says host ownership should be explicit.

For this slice, that ownership is explicit in the **connection identity** and
frontend host-scoped wrappers, not duplicated into every wire payload.

If we later need host identity to survive detached logs, persisted frontend
caches, or cross-connection replay independent of the shell routing key, then a
follow-up protocol document can add a server-native `HostId`.

That is a valid follow-up, but it is not required to land the grouped-host UI.

---

## 13. Projects UI

### 13.1 Projects tab

The legacy app had a dedicated Projects surface. The rewrite should bring that
back instead of trying to force grouped multi-host behavior into one flat list.

The Home surface should gain:

- `Projects` tab
- `Agents` tab

The Projects tab should render:

```text
Local
  Project A
  Project B

workbox
  Project C
  Project D
```

Each host section should show:

- host label
- connection status
- zero-state / error-state messaging
- host-local projects
- host-local "Add Project" action

### 13.2 Add project flow

Add-project becomes host-scoped:

- local host: prompt for a local path
- remote host: prompt for a path that exists on that remote host

The frontend then sends the existing `ProjectCreate` frame to that host's host
stream.

The server-side `ProjectCreatePayload` can remain unchanged:

```rust
pub struct ProjectCreatePayload {
    pub name: String,
    pub roots: Vec<String>,
}
```

The meaning of the paths is simply "paths on the connected host".

### 13.3 Project rail

The project rail should also become host-aware.

It does not need to expose the full Projects-tab layout, but it should at least
group project entries by host and preserve host section headers, with local
first and remotes in configured-host order.

### 13.4 Disconnect behavior

When a host disconnects:

- keep the configured host section visible
- clear its live project inventory from the rendered grouped list
- show disconnected state instead of stale remote project cards

This avoids stale remote state being mistaken for live reachable state.

---

## 14. Settings UI

`09-host-settings.md` explicitly left host registry and multi-host selection out
of scope. This document adds that missing layer.

### 14.1 New settings structure

The Settings overlay should gain a `Hosts` tab and a selected-host control.

Suggested shape:

- `Hosts`
- `Backends`
- `Appearance`
- `General`

### 14.2 Hosts tab

The Hosts tab should allow:

- list configured hosts
- add remote host
- edit remote host label / SSH destination
- remove remote host
- connect / disconnect a host
- choose the selected host for host-scoped settings

For the first slice, the remote-host form can be intentionally small:

- `Label`
- `SSH Destination`
- optional `Remote Command` advanced field
- `Auto-connect`

This matches the user's requested "small store in the Tauri layer" exactly.

### 14.3 Backends tab becomes selected-host scoped

`HostSettings` remains server-owned and per-host.

So the Backends tab should render the `HostSettings` for the currently selected
configured host, not one implicit global host.

Rules:

- opening Settings requests `DumpSettings` for the selected connected host
- changing enabled/default backends sends `SetSetting` to that selected host
- if the selected host is disconnected, the Backends tab shows a disconnected
  state instead of pretending settings are available

This preserves the existing `09-host-settings.md` model while making it
multi-host aware.

---

## 15. Runtime Flow

### 15.1 Startup

On app startup:

1. shell loads `ConfiguredHostStore`
2. frontend loads configured hosts from shell
3. frontend asks the shell to connect every host with `auto_connect = true`
4. each connection gets its own generated `/host/<uuid>` stream
5. frontend sends `hello` independently on each host connection

### 15.2 Initial replay per host

Per host, existing replay behavior remains:

1. `welcome`
2. `host_settings`
3. project replay
4. agent replay

The frontend stores those events under the owning configured host.

### 15.3 Host-local actions

When the user:

- opens a project
- launches a terminal
- creates a new chat from a project
- resumes a session

the frontend resolves the owning `ConfiguredHostId` first, then uses that
connection's stream state.

No fallback host guessing is allowed.

---

## 16. Interaction With Existing Stores

This design intentionally does **not** create one global multi-host project or
session store in the desktop app.

Instead:

- local host keeps using its own local `projects.json`, `sessions.json`,
  `settings.json`
- each remote Tyde server uses its own remote copies of those same stores

That is cleaner than the old app's global host-tagged inventory because it
preserves the rewrite's single-host server ownership model.

It also means the missing `host_id` field in the rewrite's current
`SessionSummary` and `Project` types is not a blocker for storage, because the
store boundary is already per host.

The host association only has to exist in the desktop aggregation layer.

---

## 17. Implementation Slices

### Slice 1: shell host registry

- add `ConfiguredHostStore` to `frontend/tauri-shell`
- guarantee local host presence
- add shell commands/events for host CRUD and selection

### Slice 2: generalized host transport

- extend router connect path to use `HostTransportConfig`
- keep local embedded transport
- add SSH stdio transport

### Slice 3: frontend multi-host state

- replace singular host state with per-host maps
- namespace inbound/outbound sequence tracking by `(host_id, stream)`
- dispatch envelopes using the emitting host ID

### Slice 4: Projects tab and grouped project rail

- add grouped Projects tab to home
- add per-host project sections
- make add/open project host-scoped

### Slice 5: host-scoped settings and actions

- add Hosts settings tab
- make Backends settings selected-host scoped
- route file/git/terminal/chat/session actions through owning host

### Slice 6: optional protocol/server follow-ups

- optional server-native `HostId`
- standalone remote host stdio command polish
- reconnect policy and richer per-host status

---

## 18. Open Questions

### 18.1 Remote host command name

The architecture requires a remote Tyde host stdio entrypoint, but this repo
does not yet define the exact binary/CLI shape. That needs a small follow-up
decision.

### 18.2 Auto-reconnect policy

This document assumes connect/disconnect behavior and basic status reporting,
but not a final reconnect strategy. We can add retry/backoff later without
changing the host registry model.

### 18.3 Project grouping only, or sessions/agents too?

The user request is specifically about projects grouped by host. Once the app
is host-aware, sessions and agents can also be grouped or filtered by host, but
that is a separate UI decision.

---

## 19. Summary

The right way to support remote Tyde servers is:

- keep each `server` instance single-host
- store configured host transport in the shell
- connect to many hosts in parallel
- aggregate host-owned state in the frontend
- show projects grouped by host
- route all host-owned actions through explicit host-scoped references

That preserves the rewrite boundaries instead of breaking them:

- server owns behavior
- shell owns transport
- frontend renders host-owned state
- remote is just another host connection
