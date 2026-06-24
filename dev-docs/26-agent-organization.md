# Agent Organization

This document is the authoritative design for agent organization in Tyde. It
builds on:

- `01-philosophy.md`: server owns behavior; the UI renders server state.
- `03-agents.md`: agents are server-managed runtime objects.
- `06-projects.md`: projects are server-owned, durable inventory.
- `09-host-settings.md`: settings are typed protocol state, persisted by the
  server, replayed at bootstrap, and updated through host-stream events.
- `13-agent-naming.md`: internal helper work must not leak into visible agent
  state.
- `15-sub-agents.md`: parentage and origin are distinct; the frontend must not
  infer provenance from `parent_agent_id`.
- `25-workflows.md`: the recent pattern for server-owned catalog state and
  bootstrap-backed frontend projection.

The feature has two pillars:

1. Persistent Agents-tab view settings.
2. Server-owned agent folders and placements.

Both pillars are server-owned. The frontend may keep ephemeral interaction state
such as a focused input or an in-progress drag gesture, but persistent view
preferences, folder membership, folder order, and placement order are protocol
state emitted by the server.

---

## 1. Goals

- Stop the Agents tab from flickering, changing order, or resetting filters when
  a view remounts, a host reconnects, or the user switches workspaces.
- Make the Agents Center at least as capable as the sidebar Agents panel.
- Introduce a folder model that works consistently in both the Agents Center and
  the sidebar.
- Keep organization display-only: moving an agent into a folder must not mutate
  the agent's `project_id` or session metadata other than organization
  assignments.
- Preserve the existing parent/child sub-agent nesting while adding folders as
  the outer grouping.
- Keep all durable organization state in server-owned stores and protocol
  payloads.

## 2. Non-goals

- No remote push, PR, tag, or release behavior.
- No change to backend-native project/session semantics.
- No mutation of `agent.project_id` when moving agents between folders.
- No frontend-only persistence in `localStorage`, indexed DB, or Tauri-only
  state.
- No automatic deletion of agents or sessions as a side effect of deleting a
  folder or cleaning organization state.
- No real-AI tests for this work. The test plan uses client -> server -> mock
  backend flows.

---

## 3. Current state

### 3.1 The Agent Monitor owns ordering locally today

The center Agent Monitor is opened as a center tab from `center_zone.rs` and
renders `AgentMonitorView` when a tab's content is `TabContent::AgentMonitor`
(`frontend/src/components/center_zone.rs:617-624`,
`frontend/src/components/center_zone.rs:420-447`).

Its ordering is currently frontend-local:

- `AppState` declares `agent_monitor_order` as "Session-local manual ordering"
  and `agents_panel_filters` as another frontend map
  (`frontend/src/state.rs:1288-1293`).
- Those signals are initialized as empty in `AppState::new`
  (`frontend/src/state.rs:1551-1553`).
- Host runtime cleanup prunes those maps on disconnect/reset
  (`frontend/src/state.rs:2475-2484`).
- The Agent Monitor derives a default order from current agents, overlays the
  local manual order, and stores changes back into `state.agent_monitor_order`
  (`frontend/src/components/agent_monitor_view.rs:38-118`,
  `frontend/src/components/agent_monitor_view.rs:196-210`,
  `frontend/src/components/agent_monitor_view.rs:254-268`).
- An effect prunes missing agents out of the local manual order
  (`frontend/src/components/agent_monitor_view.rs:270-286`).
- Reset simply clears the frontend-local order
  (`frontend/src/components/agent_monitor_view.rs:330-339`).

The current drag-and-drop and keyboard reorder implementation is useful as a UI
seam, but it mutates frontend state directly:

- HTML drag events set a local `dragged_key` and call `apply_manual_reorder`
  on drop (`frontend/src/components/agent_monitor_view.rs:460-523`).
- Up/down buttons and `Alt+ArrowUp` / `Alt+ArrowDown` call the same local
  reorder helper (`frontend/src/components/agent_monitor_view.rs:530-617`).

This is the root of the flicker/reset complaint. The rendered order is a product
of current render inputs plus local state that is not replayed by the server.
When the component remounts, the host reconnects, or the active workspace changes
far enough to clear/prune local maps, the UI can re-derive a different order.

### 3.2 The sidebar Agents panel owns filters locally today

The right dock mounts `AgentsPanel` under the `Agents` tab
(`frontend/src/components/dock_zone.rs:77-96`). The panel already has search,
filters, status badges, backend badges, rename/compact/close actions, and
parent/child sub-agent nesting:

- `search` is a component-local `RwSignal` (`frontend/src/components/agents_panel.rs:150-154`).
- `AgentsPanelFilters` contains `hide_sub_agents`, `hide_inactive`, and
  `show_other_projects` (`frontend/src/state.rs:997-1016`).
- The active filter set is looked up in `state.agents_panel_filters`, keyed by
  `Option<ActiveProjectRef>`, with per-project defaults
  (`frontend/src/components/agents_panel.rs:161-184`).
- Filtering includes sub-agent, inactive, project, and name search checks
  (`frontend/src/components/agents_panel.rs:16-49`).
- Parent/child nesting is built from `parent_agent_id`
  (`frontend/src/components/agents_panel.rs:220-265`).
- Filter controls and the search input are rendered above the list
  (`frontend/src/components/agents_panel.rs:290-324`).
- Agent cards expose status, age, optional side-question/workflow/custom-agent
  badges, backend badge, rename, compact, and close actions
  (`frontend/src/components/agents_panel.rs:593-783`).

Those controls are session-only and scoped to frontend active-project memory, not
server-owned user preferences.

### 3.3 The protocol and server already have the right patterns

The current protocol already models the ingredients this feature should reuse:

- `PROTOCOL_VERSION` is an explicit constant
  (`protocol/src/types.rs:16`).
- `BackendKind` and `AgentOrigin` are strong enums
  (`protocol/src/types.rs:389-398`, `protocol/src/types.rs:423-440`).
- `AgentStartPayload` and `NewAgentPayload` carry `origin`, `backend_kind`,
  `project_id`, `parent_agent_id`, optional `session_id`, and creation time
  (`protocol/src/types.rs:1764-1784`, `protocol/src/types.rs:1797-1818`).
- `Project` has a durable `sort_order`
  (`protocol/src/types.rs:2377-2384`).
- `ProjectNotifyPayload` uses a tagged `Upsert` / `Delete` shape
  (`protocol/src/types.rs:2472-2477`).
- `HostBootstrapPayload` already carries host-scoped snapshots: settings,
  sessions, projects, agents, workflow state, teams, and other inventories
  (`protocol/src/types.rs:1075-1101`).
- `HostSettings`, `SetSettingPayload`, `HostSettingValue`, and
  `HostSettingsPayload` are the existing typed-settings pattern to mirror
  (`protocol/src/types.rs:1162-1239`).

The current frontend dispatcher follows that pattern:

- `HostSettings` replaces the per-host settings signal
  (`frontend/src/dispatch.rs:580-592`).
- `ProjectNotify::Upsert` and `ProjectNotify::Delete` mutate the project signal
  from server events (`frontend/src/dispatch.rs:1153-1220`).
- `HostBootstrap` replaces host-keyed snapshots without opening tabs or stealing
  focus (`frontend/src/dispatch.rs:4030-4195`).
- `NewAgent` is the only live-event arm that performs new-agent side effects;
  bootstrap only upserts snapshots (`frontend/src/dispatch.rs:729-943`,
  `frontend/src/dispatch.rs:4152-4195`).
- `AgentClosed` currently removes the live agent from frontend state
  (`frontend/src/dispatch.rs:981-990`, `frontend/src/dispatch.rs:3434-3498`).

The current server has matching store, bootstrap, route, and fanout patterns:

- `HostState` owns store handles for projects, settings, sessions, and other
  domains (`server/src/host.rs:220-245`).
- Host registration loads settings, projects, sessions, current agent snapshots,
  and then emits one `HostBootstrapPayload`
  (`server/src/host.rs:532-568`, `server/src/host.rs:647-718`).
- Project mutations write the project store and fan out `ProjectNotify`
  (`server/src/host.rs:2721-2792`, `server/src/host.rs:2877-2943`).
- `set_setting` applies a typed setting update, persists it, and fans out the
  latest settings snapshot (`server/src/host.rs:4366-4385`).
- Project and settings fanout both iterate host subscribers, drop dead streams,
  and send typed frame payloads (`server/src/host.rs:9073-9092`,
  `server/src/host.rs:9451-9469`, `server/src/host.rs:9576-9585`,
  `server/src/host.rs:9653-9664`).
- Router inputs for settings and project mutations arrive on the host stream
  (`server/src/router.rs:41-52`, `server/src/router.rs:112-146`).
- The protocol validator parses host-stream settings/project payloads and must be
  extended for new organization frames (`protocol/src/validator.rs:249-320`,
  `protocol/src/validator.rs:371-388`).

Persistence patterns to mirror:

- `HostSettingsStore` has a default path, missing-file defaults, an `apply`
  method, validation, and atomic save (`server/src/store/settings.rs:37-57`,
  `server/src/store/settings.rs:90-95`, `server/src/store/settings.rs:214-272`).
- `ProjectStore` has a versioned file, ordered records, reorder validation, and
  atomic save (`server/src/store/project.rs:12-18`,
  `server/src/store/project.rs:103-125`,
  `server/src/store/project.rs:194-248`,
  `server/src/store/project.rs:472-590`).
- `SessionStore` persists session records, clears project references on project
  deletion, deletes sessions explicitly, and uses read-modify-write atomic saves
  (`server/src/store/session.rs:20-58`, `server/src/store/session.rs:226-246`,
  `server/src/store/session.rs:473-528`).

---

## 4. Decisions and rationale

### 4.1 View preferences are client-global preferences owned by the primary local host

Agents-tab view preferences are global across workspaces and projects for one
Tyde client. They are not keyed by session, active project, current workspace
root, center-tab instance, or remote host. Because each remote host runs its own
server and has its own `~/.tyde` directory, "global across hosts" cannot mean
"every host owns a competing copy." Instead, there is exactly one authoritative
preference store: the primary local host for this client owns and emits the
snapshot. Remote hosts contribute agents/projects to the projection, but they do
not own or emit competing `AgentsViewPreferences` snapshots.

The durable object is:

```rust
pub struct AgentsViewPreferences {
    pub filters: AgentsViewFilters,
    pub sort_mode: AgentSortMode,
    pub group_mode: AgentGroupMode,
    pub density: AgentListDensity,
    pub hide_finished: bool,
    pub manual_order: Vec<AgentOrderKey>,
}

pub struct AgentsViewFilters {
    pub host_ids: Vec<HostFilterId>,
    pub project_ids: Vec<AgentProjectFilter>,
    pub statuses: Vec<AgentStatusFilter>,
    pub backends: Vec<BackendKind>,
    pub origins: Vec<AgentOrigin>,
}

pub struct AgentProjectFilter {
    pub host_id: HostFilterId,
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostFilterId(pub String);

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOrderKey {
    Session { session_id: SessionId },
    TransientAgent { host_id: HostFilterId, agent_id: AgentId },
}
```

`HostFilterId` is a typed wrapper around the stable configured-connection id for
the host. It must not wrap a `/host/<uuid>` stream path, an agent stream path, or
any per-connection instance id. Persisted filters and transient agent order keys
must survive reconnects, so they reference the durable configured-host identity
that the bridge/host registry already uses for configured hosts.

Manual ordering is session-keyed whenever a `SessionId` is known. The
`TransientAgent` variant is only for live agents that have not resolved a session
id yet. When the server/client learns the session id, the durable preference is
rewritten from `TransientAgent` to `Session` and the transient key is pruned on
that mutation.

`search` is deliberately not persisted. Persistent search makes agents disappear
after restart and is surprising. The search input remains an ephemeral UI input
that narrows the current projection only. If a future design wants search to
survive remounts, it should be added as server-emitted transient view state, not
folded into the durable preference object.

**Rationale:** the user's pain is settings that "flicker around" and reset. A
single durable primary-host preference removes the frontend-local sources of
truth that currently re-derive order and filters, while still applying to agents
from every connected host in the client projection.

### 4.2 View preferences use a dedicated store

Use a dedicated store rather than extending `HostSettings`.

Default path:

```text
~/.tyde/agents_view_preferences.json
```

Override:

```text
TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH
```

Store shape:

```json
{
  "version": 1,
  "preferences": {
    "filters": {
      "host_ids": [],
      "project_ids": [],
      "statuses": [],
      "backends": [],
      "origins": []
    },
    "sort_mode": "manual_then_activity",
    "group_mode": "flat",
    "density": "comfortable",
    "hide_finished": false,
    "manual_order": []
  }
}
```

Missing file means default preferences with no error. A corrupt file is different:
it must produce a typed preference-store error, fall back to defaults for this
preference store only, and keep host registration/connectivity alive. A UI
preference file must not panic the host or block remote connectivity. The next
valid preference mutation, including Reset, writes a fresh valid file and clears
the load error.

Validation still matters on writes: invalid enum values, duplicate manual-order
keys, empty ids, impossible filter values, and unsupported versions are rejected
with typed errors instead of being silently repaired.

**Rationale:** these are user-interface preferences, not backend host settings.
A separate store keeps the settings domain small while mirroring the settings
store's apply/save/fanout pattern, but it is intentionally less fatal on corrupt
load than core host settings.

### 4.3 View preference protocol is full-snapshot notify with a pending overlay

Add host-stream frames on the primary local host stream:

```rust
FrameKind::SetAgentsViewPreferences
FrameKind::AgentsViewPreferencesNotify
```

Payloads:

```rust
pub struct SetAgentsViewPreferencesPayload {
    pub update: AgentsViewPreferencesUpdate,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentsViewPreferencesUpdate {
    SetFilters { filters: AgentsViewFilters },
    SetSortMode { sort_mode: AgentSortMode },
    SetGroupMode { group_mode: AgentGroupMode },
    SetDensity { density: AgentListDensity },
    SetHideFinished { hide_finished: bool },
    SetManualOrder { manual_order: Vec<AgentOrderKey> },
    Reset,
}

#[serde(rename_all = "snake_case")]
pub enum AgentsViewPreferencesStoreErrorKind {
    Corrupt,
    UnsupportedVersion,
    Io,
}

pub struct AgentsViewPreferencesStoreError {
    pub kind: AgentsViewPreferencesStoreErrorKind,
    pub message: String,
}

pub struct AgentsViewPreferencesSnapshot {
    pub preferences: AgentsViewPreferences,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<AgentsViewPreferencesStoreError>,
}

pub struct AgentsViewPreferencesNotifyPayload {
    pub snapshot: AgentsViewPreferencesSnapshot,
}
```

Add this to bootstrap:

```rust
pub struct HostBootstrapPayload {
    // existing fields...
    #[serde(default)]
    pub agents_view_preferences: Option<AgentsViewPreferencesSnapshot>,
}
```

Only the primary local host emits `Some(snapshot)` and accepts
`SetAgentsViewPreferences`. Remote host bootstraps use the serde default `None`;
they do not overwrite the client-global preference signal.

Durable truth remains server-owned, but the frontend is allowed to render from:

```text
server snapshot base + short-lived pending overlay
```

The overlay is local, non-persisted, and keyed to the in-flight semantic mutation
(e.g. `SetManualOrder`, `SetFilters`, or `SetDensity`). It is dropped when a
notify/bootstrap snapshot either contains the expected value or supersedes that
same preference domain with a different server value. No request id is added to
the wire protocol; matching is by the updated preference domain and expected
value. If the server value differs, the server wins and the overlay is discarded
with an inline error/toast when an error was emitted.

Use the overlay for interactions where pure server-wait would visibly rubber-band
or feel laggy, especially drag reorder and remote/SSH hosts. Without it, a drop
would snap back until the notify returns, and local writes would still wait on a
synchronous durable store write. Low-frequency local discrete toggles may wait
for the server snapshot without an overlay.

**Why this kills flicker:** the durable base comes from a server snapshot, not an
init-empty frontend vector/map. The pending overlay is never persisted and is
cleared on notify, reconnect, or host cleanup. It cannot become the new source of
truth and cannot be replayed as stale state, but it gives instant feedback while
the server persists and re-emits the canonical snapshot.

### 4.4 Agent organization is per host

Folders and placements are per host. A host owns its live agent registry, project
store, session store, and organization store. The organization model must not
cross host boundaries.

Protocol model:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentFolderId(pub String);

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOrganizationContainer {
    HostRoot,
    Project { project_id: ProjectId },
    NoProject,
    Folder { folder_id: AgentFolderId },
}

pub struct AgentFolder {
    pub id: AgentFolderId,
    pub name: String,
    pub parent: AgentOrganizationContainer,
    pub sort_order: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[serde(rename_all = "snake_case")]
pub enum AgentPlacementSource {
    Default,
    SessionAssignment,
    TransientAssignment,
}

pub struct AgentPlacement {
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub container: AgentOrganizationContainer,
    pub sort_order: u64,
    pub source: AgentPlacementSource,
}

pub struct AgentOrganizationSnapshot {
    #[serde(default)]
    pub folders: Vec<AgentFolder>,
    #[serde(default)]
    pub placements: Vec<AgentPlacement>,
}
```

Add this to bootstrap:

```rust
pub struct HostBootstrapPayload {
    // existing fields...
    #[serde(default)]
    pub agent_organization: AgentOrganizationSnapshot,
}
```

**Rationale:** this follows the existing project shape: typed ids, explicit
parent/container fields, durable sort order, bootstrap snapshot, and live notify
frames.

### 4.5 Default groups are virtual and server-emitted

The server emits default placements for every organizable live agent:

- `Project { project_id }` if the agent has a valid project id on that host.
- `NoProject` otherwise.

`HostRoot`, `Project { .. }`, and `NoProject` are virtual containers. They are
not stored as `AgentFolder` records. Custom folders may be parented under
`HostRoot`, a virtual project container, `NoProject`, or another custom folder.

A default placement is display-only. It never mutates `AgentStartPayload.project_id`,
`NewAgentPayload.project_id`, `SessionRecord.project_id`, or project store data.

**Rationale:** projects already have domain meaning. Folders are an organization
view over agents, not a new project/session ownership model.

### 4.6 Placement precedence is explicit

For each live agent, placement is resolved in this order:

1. Persisted session assignment keyed by `SessionId`: `SessionAssignment`.
2. Live pre-session move keyed by `AgentId`: `TransientAssignment`.
3. Computed default from the agent's current project: `Default`.

When an agent that has a transient assignment later resolves a `SessionId`, the
host promotes that assignment to a persisted session assignment and emits a
placement notify. The transient assignment is removed.

When multiple live agents share one `SessionId`, the session assignment applies
to all of them unless a live agent has a transient assignment. This is the Phase 2
rule, not an open question. Current host state maps `AgentId -> SessionId` and
has no reverse-uniqueness invariant (`server/src/host.rs:231`,
`server/src/host.rs:5727-5730`), so session-keyed organization intentionally
means "all live views of this session move together." A future per-live-agent
exception would require a new placement key, not a silent reinterpretation of
`SessionAssignment`.

### 4.7 Terminated, closed, and helper agents

Phase 1a scopes "Hide finished" to state that exists today: live agents whose
visible derived state is terminated because `fatal_error` is set. `AgentClosed`
removes the live agent from the frontend (`frontend/src/dispatch.rs:981-990`,
`frontend/src/dispatch.rs:3434-3498`), so a closed-but-successfully-finished
agent is not currently available to grey out in the Agents tab.

Rules for existing state:

- Live fatal/terminated agents stay in their current container and may render
  greyed out. The "Hide finished" filter hides those rows but does not move them.
- Closing an agent drops the live placement from the current snapshot, matching
  the current `AgentClosed` live-agent removal path. If the placement had been
  promoted to a session assignment, the assignment remains in the organization
  store and is used if the session is resumed later.
- Sessionless transient assignments vanish when the live agent closes.
- Internal/ephemeral helper agents are not organizable. They must not appear in
  `AgentOrganizationSnapshot`, and organization mutation frames that reference
  them fail with a typed command error.

A richer typed lifecycle field is required before Tyde can show true
successfully-finished-but-visible history rows. That is deferred beyond Phase 1a/1b;
do not infer it in the frontend from missing streams or idle status.

### 4.8 Project deletion reparents custom folders

When a project is deleted:

- The virtual `Project { project_id }` group disappears because it is computed.
- Custom folders whose parent is `Project { project_id }` are reparented to
  `NoProject`, preserving relative `sort_order` where possible.
- Placements inside those folders remain in those folders.
- Agents and sessions are never deleted by organization cleanup.

This mirrors the existing project delete principle: project deletion detaches
metadata instead of deleting sessions (`server/src/host.rs:2915-2920`) and then
removes the project (`server/src/host.rs:2927-2943`). Reparenting preserves user
organization data better than deleting folder subtrees.

---

## 5. Pillar A: persistent Agents-tab view settings

### 5.1 Server model

Add `server/src/store/agents_view_preferences.rs` with the same write lifecycle
shape as `HostSettingsStore`, but with non-fatal corrupt-load behavior:

- `load(path) -> Self` records either a healthy store or a typed load error.
- `default_path()` returns `TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH` when set,
  otherwise `~/.tyde/agents_view_preferences.json` on the primary local host.
- `snapshot() -> AgentsViewPreferencesSnapshot` returns defaults plus
  `load_error: Some(...)` when the file is corrupt, and defaults with no error
  when the file is missing.
- `apply(update) -> Result<AgentsViewPreferencesSnapshot, String>`
  read-modify-writes the file atomically and returns the full snapshot. If the
  current file was corrupt, a valid mutation overwrites it with a fresh valid
  store and clears the load error.

Validation rules:

- Enum lists are canonicalized in enum order where order is not meaningful.
- Manual order has no duplicate keys.
- `AgentOrderKey::TransientAgent` requires non-empty stable `HostFilterId` and
  non-empty `AgentId`.
- `AgentOrderKey::Session` requires non-empty `SessionId`.
- `TransientAgent` keys are allowed only when no `SessionId` is known for that
  live agent.
- Host/project filters must reference typed stable configured-connection ids, not
  labels, stream paths, or stream instances.
- Unsupported store versions are reported as typed load errors and defaulted at
  bootstrap; unsupported write input is rejected.
- Missing file returns defaults with no error.

Add an `agents_view_preferences_store` field to the primary local `HostState`,
load it in `spawn_host_inner`, include `Some(snapshot)` in that host's
`HostBootstrapPayload`, and fan out `AgentsViewPreferencesNotify` after every
successful update. Remote hosts should either omit the field (`None`) or ignore
it if older code still fills defaults; they must not replace the client-global
preference signal.

### 5.2 Frontend model

Replace frontend-owned durable preference signals with a server snapshot plus a
non-persisted pending overlay:

- Remove `agent_monitor_order` as a durable local source of truth.
- Replace `agents_panel_filters` with the primary-host server snapshot for
  persisted filters.
- Keep search as ephemeral component input only.
- Add one `agents_view_preferences` snapshot signal owned by primary-host
  bootstrap/notify.
- Add a short-lived `pending_agents_view_overlay` signal for in-flight
  preference mutations that need instant feedback.
- Update both `AgentsPanel` and `AgentMonitorView` to render from
  `effective_preferences = server_snapshot.preferences + pending_overlay`.
- On user changes, send `SetAgentsViewPreferences`. For drag/manual reorder and
  remote-host interactions, install a pending overlay immediately. For local
  low-frequency toggles, either install the overlay or wait for notify; both are
  valid as long as durable state is not mutated locally.
- Drop/reconcile the overlay when `AgentsViewPreferencesNotify` or a new primary
  bootstrap snapshot arrives for the same preference domain.

The same effective preference controls must drive both the center and sidebar
surfaces so that "Hide finished" or backend/origin/status filters do not differ
by surface. The sidebar may use a compact subset of controls, but it reads and
writes the same server preference object.

### 5.3 Sort/group/density semantics

Sort modes:

```rust
pub enum AgentSortMode {
    ManualThenActivity,
    NewestFirst,
    OldestFirst,
    NameAsc,
    Status,
    Backend,
    Project,
}
```

Group modes:

```rust
pub enum AgentGroupMode {
    Flat,
    Status,
    Backend,
    Project,
    Folders,
    FoldersThenStatus,
    FoldersThenBackend,
    FoldersThenProject,
}
```

Density:

```rust
pub enum AgentListDensity {
    Comfortable,
    Compact,
}
```

Phase 1a supports the non-folder group modes (`Flat`, `Status`, `Backend`, and
`Project`) and can persist folder modes without rendering them if a staged client
already knows them. Phase 1b enables `Folders` and `FoldersThen*` once the server
emits `AgentOrganizationSnapshot`. When folder modes are active, folders are the
outer grouping; the suffix chooses the secondary grouping inside each folder.
Sort is applied within the innermost group. Manual order is applied only where it
can be unambiguous: inside the resolved container/group after filters are
applied.

### 5.4 Manual order semantics

Manual order in preferences is stored by session id whenever possible. A live
agent id is only a transient key while that agent has no known session id. That
matches organization placement precedence and avoids losing order when a session
is resumed.

When a manual order update references visible live agents, the frontend sends the
visible order as `Vec<AgentOrderKey>` using `Session` for agents with a known
session id and `TransientAgent` only for unresolved agents. The primary host
stores the canonical list and emits the full preference snapshot.

Stale-key cleanup is deterministic and happens on every successful manual-order
mutation:

1. Drop duplicate keys after the first occurrence.
2. Drop `TransientAgent` keys whose stable host id or live agent id is no longer
   known to the current client projection.
3. Rewrite a `TransientAgent` key to `Session` when that live agent's session id
   is known at mutation time.
4. Keep `Session` keys even when no live agent currently references the session,
   so resumed sessions retain their order.

This rule replaces any vague migration window. The stored order after each
mutation is canonical and reproducible.

### 5.5 Why this fixes flicker

Today the center order is recomputed from live signals and a local vector. The
sidebar filters are per-project frontend maps. Both are initialized empty and can
be pruned on host cleanup. That creates observable resets.

After Pillar A:

1. The primary host bootstrap carries the durable preference snapshot.
2. Live preference mutations return the same full snapshot shape.
3. The frontend renders from one server-fed base signal plus an optional
   non-persisted pending overlay.
4. Remounts and workspace switches re-read the same server base instead of
   creating a new local map/vector.
5. The overlay is cleared on notify, bootstrap, disconnect, or semantic
   supersession; it is never stored and cannot become stale startup state.
6. New agents are inserted by server-defined sort/manual-order rules, not by
   component construction order.

There is no durable second source of truth left to flicker, while drag reorder
and remote-host interactions still get immediate visual feedback.

---

## 6. Pillar B: agent folders

### 6.1 Store model

Add `server/src/store/agent_organization.rs`.

Default path:

```text
~/.tyde/agent_organization.json
```

Override:

```text
TYDE_AGENT_ORGANIZATION_STORE_PATH
```

Store shape:

```json
{
  "version": 1,
  "folders": {
    "<folder-id>": {
      "id": "<folder-id>",
      "name": "Research",
      "parent": { "kind": "project", "project_id": "..." },
      "sort_order": 0,
      "created_at_ms": 1760000000000,
      "updated_at_ms": 1760000000000
    }
  },
  "session_assignments": {
    "<session-id>": {
      "session_id": "<session-id>",
      "container": { "kind": "folder", "folder_id": "..." },
      "sort_order": 0,
      "updated_at_ms": 1760000000000
    }
  }
}
```

Transient assignments are host-runtime state only, keyed by `AgentId`. They are
not written to disk. They are promoted to `session_assignments` when the server
learns the session id.

Validation rules:

- Folder names must be non-empty after trim.
- Folder ids and referenced project/session ids must be non-empty.
- Folder parent graph must be acyclic.
- A folder cannot parent itself directly or indirectly.
- A folder parented under `Project { id }` must reference an existing project at
  load/mutation time. Project deletion reparenting is the cleanup hook.
- `sort_order` is durable and rewritten on reorder.
- Missing file means empty custom folders and no session assignments, so all
  agents fall back to computed defaults.
- Unknown store versions or cyclic graphs fail loudly.

### 6.2 Host state and bootstrap

Add `agent_organization_store: Arc<Mutex<AgentOrganizationStore>>` to
`HostState`. Add an in-memory `transient_agent_assignments` map keyed by
`AgentId`.

During host registration, build `AgentOrganizationSnapshot` after projects,
sessions, and live agent snapshots are known:

1. Load custom folders from the organization store.
2. Build persisted session assignments from the store.
3. For each organizable live agent, resolve placement by precedence.
4. Emit default placements for agents without an assignment.
5. Exclude internal/ephemeral helpers.
6. Include the snapshot in `HostBootstrapPayload`.

The frontend never reconstructs default containers on its own. It may derive a
render tree from `AgentOrganizationSnapshot`, `projects`, and `agents`, but the
container and placement facts are server facts.

### 6.3 Mutation frames

Add host-stream input frames:

```rust
FrameKind::AgentFolderCreate
FrameKind::AgentFolderRename
FrameKind::AgentFolderDelete
FrameKind::AgentFolderReorder
FrameKind::AgentMoveToFolder
```

Payloads:

```rust
pub struct AgentFolderCreatePayload {
    pub name: String,
    pub parent: AgentOrganizationContainer,
}

pub struct AgentFolderRenamePayload {
    pub folder_id: AgentFolderId,
    pub name: String,
}

pub struct AgentFolderDeletePayload {
    pub folder_id: AgentFolderId,
    pub delete_children: bool,
}

pub struct AgentFolderReorderPayload {
    pub parent: AgentOrganizationContainer,
    pub folder_ids: Vec<AgentFolderId>,
}

pub struct AgentMoveToFolderPayload {
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub container: AgentOrganizationContainer,
    pub sort_order: u64,
}
```

`AgentMoveToFolderPayload.container` may be `Project`, `NoProject`, or `Folder`.
Moving to `HostRoot` is invalid for an agent placement because `HostRoot` is only
the root of folders/default groups.

`AgentFolderDeletePayload` should default in UI to "move children and placements
to the deleted folder's parent". `delete_children = true` deletes only custom
folder records recursively; it still reparents placements to the nearest
surviving parent. Agents/sessions are never deleted.

### 6.4 Notify frame

Add host-stream output frame:

```rust
FrameKind::AgentOrganizationNotify
```

Use an Upsert/Delete record shape that mirrors `ProjectNotify` while supporting
both folders and placements:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOrganizationRecord {
    Folder { folder: AgentFolder },
    Placement { placement: AgentPlacement },
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOrganizationNotifyPayload {
    Upsert { record: AgentOrganizationRecord },
    Delete { record: AgentOrganizationRecord },
}
```

Deletes carry the full deleted record for the same reason project deletes carry
the deleted project: the frontend should not have to look elsewhere for the last
known payload.

Use repeated `Upsert` events for reorder operations. Use `Delete` for a removed
custom folder record or a removed explicit placement. If a placement delete
reveals a computed default, follow it with an `Upsert` for the default placement
or include the default in the next snapshot. Prefer the explicit follow-up
`Upsert` so live UIs converge without a rescan.

### 6.5 Cleanup hooks

Session deletion:

- Remove the session assignment for the deleted `SessionId`.
- Fan out `AgentOrganizationNotify::Delete` for the removed placement if it
  existed.
- Do not delete folders.

Project deletion:

- Before or immediately after `ProjectNotify::Delete`, ask `AgentOrganizationStore`
  to reparent custom folders under `Project { deleted }` to `NoProject`.
- Remove or rewrite explicit placements that reference the deleted virtual
  project directly. Session assignments in custom folders survive.
- Emit folder/placement notifies for every rewritten record.
- Never delete agents or sessions.

Agent close:

- Remove transient assignment keyed by the live `AgentId`.
- Emit placement delete for the live placement if it was transient or default.
- Keep persisted session assignment intact.

Session resolve:

- If the agent has a transient assignment, promote it to a session assignment,
  delete the transient placement, and emit the new `SessionAssignment` placement.

---

## 7. UX design

### 7.1 Improved Agents Center

The Agents Center becomes the primary management surface for live agents. It must
match or beat the sidebar, which already has search and filter toggles.

Layout:

1. Header: title, agent count, current host/project scope summary.
2. Toolbar:
   - search input (ephemeral, not persisted),
   - host filter,
   - project filter,
   - status filter,
   - backend filter,
   - origin filter,
   - "Hide finished",
   - sort selector,
   - group selector,
   - density selector,
   - reset preferences action.
3. Folder tree/list area:
   - virtual Host/Project/No Project groups,
   - custom folders,
   - empty folder states,
   - drag/drop targets and keyboard move affordances.

Each row/card shows:

- status icon and label,
- agent name,
- backend badge,
- host label,
- project label or "No project",
- origin label,
- age / created time,
- optional custom-agent/team/workflow/side-question badges where already
  available,
- actions: Open, Rename, Compact when eligible, Close, Move to folder.

In Phase 1a/1b, "Hide finished" means "hide live fatal/terminated rows" because
that is the only present-but-done lifecycle signal currently available. Those
rows render greyed out with their folder unchanged when visible. True
successfully-finished-but-visible history rows require a later typed lifecycle
field.

### 7.2 Folder UX in both surfaces

Both the Agents Center and sidebar render the same organization model:

- Virtual groups:
  - Host root.
  - One project group per project that has agents/folders or is explicitly
    expanded by user action.
  - `NoProject` for unprojected agents/folders.
- Custom folders:
  - create under Host root, Project, No Project, or another folder,
  - rename inline or via context menu,
  - delete with confirmation,
  - show empty states,
  - keep collapsed/expanded UI state local and ephemeral unless a later design
    adds server-owned expansion preferences.

Folder delete copy must be explicit: deleting a folder removes only the folder
record. Agents and sessions are not deleted.

### 7.3 Sidebar nesting rule

The sidebar uses folders as the outer tree and existing parent/child sub-agent
nesting as the inner tree:

```text
Project: Tyde
  Folder: Release work
    Parent agent
      Child sub-agent
      Child sub-agent
    Standalone agent
  Folder: Research
No Project
  Agent
```

This preserves the current sidebar behavior that builds child buckets from
`parent_agent_id` and keeps collapsible parent rows. Folder collapse and
parent-agent collapse are separate states.

### 7.4 Drag-and-drop and keyboard fallback

Use the existing Leptos/DOM drag seam in the Agent Monitor as the starting point,
but change the mutation boundary:

- Drag state (`dragged_agent`, hovered target, pointer placement) remains
  ephemeral component state.
- Drop sends a protocol mutation and installs a non-persisted pending overlay for
  immediate visual feedback.
- Durable frontend state is not rewritten. The overlay is reconciled when
  `AgentOrganizationNotify`, `AgentsViewPreferencesNotify`, or bootstrap updates
  the relevant server snapshot.
- If the server rejects or supersedes the mutation, discard the overlay and show
  the error; the server snapshot wins.

Drop targets:

- folder header: move agent into folder at end,
- empty folder body: move agent into folder at order 0,
- between rows: move agent into the target row's container before/after target,
- virtual Project / No Project group header: move to the virtual container,
- folder header between siblings: reorder folders via `AgentFolderReorder`.

Mutations:

- Agent drop into a container sends `AgentMoveToFolder`.
- Agent row reorder within the same container sends `AgentMoveToFolder` with the
  same container and new `sort_order` / relative target calculation.
- Folder reorder sends `AgentFolderReorder`.
- Phase 1a manual row reorder, before folders exist, sends
  `SetAgentsViewPreferences::SetManualOrder` and uses the same pending-overlay
  projection to avoid rubber-banding.

Visual feedback:

- highlight valid folder drop targets,
- show before/after insertion bars,
- mark invalid targets with a not-allowed cursor,
- use an `aria-live` region for "Moving X to Y" while the overlay is pending and
  "Moved X to Y" after notify reconciles,
- show a pending state if a mutation has been sent and the notify has not arrived.

Keyboard fallback:

- focused row has "Move" action that opens a folder picker,
- `Alt+ArrowUp` / `Alt+ArrowDown` reorders within the current container,
- `Alt+Shift+ArrowLeft` moves to parent container when valid,
- `Alt+Shift+ArrowRight` opens the folder picker or moves into the next folder
  when focus is on a folder header,
- all actions send the same protocol frames as drag/drop and use the same pending
  overlay.

### 7.5 CSS work

Current CSS covers existing Agent Monitor rows and agent cards only
(`frontend/styles.css:4442-4664`, `frontend/styles.css:4664-4864`). Folder UX
needs new classes, at minimum:

- `.agent-folder-tree`
- `.agent-folder-section`
- `.agent-folder-header`
- `.agent-folder-name`
- `.agent-folder-actions`
- `.agent-folder-empty`
- `.agent-folder-drop-target`
- `.agent-folder-drop-invalid`
- `.agent-row-finished`
- `.agents-density-compact`
- `.agents-density-comfortable`
- `.agent-folder-create-button`
- `.agent-folder-rename-input`

### 7.6 Filter and group edge cases

Filtering is layered inside the server-owned organization projection:

- Virtual project groups and `NoProject` are hidden when they have no visible
  agents, visible folders, or visible descendant folders after filters/search.
- Custom folders with no agents are visible in the unfiltered view and show an
  empty-folder state.
- When filters/search are active, a custom folder remains visible if it contains
  at least one visible agent, contains a visible descendant folder, or the folder
  name itself matches the search query. A visible folder with zero matching agent
  rows shows a muted "No matching agents" state instead of disappearing.
- A filtered-out parent agent must not hide a child that independently matches
  the filters/search. Keep the child in its resolved folder/container and promote
  it to that container's top-level rows with a subtle "Parent hidden by filters"
  badge. This preserves the existing orphan behavior in the sidebar grouping
  logic while making the reason explicit.
- If `hide_sub_agents` is active, child agents are filtered out before the orphan
  rule runs.
- Search matches agent name, visible badges/labels, and folder name. It does not
  persist and does not change server organization state.

---

## 8. Phase 1a: persistent preferences and the flicker fix

Phase 1a is independently shippable and fully fixes the current flicker/reset
pain without introducing `AgentOrganizationSnapshot` or folders. It persists the
Agents-tab view preferences, rewrites the center/sidebar to render from the
primary-host server snapshot, and adds the non-persisted pending overlay for
instant reorder feedback.

### 8.1 Phase 1a protocol additions

Add:

- `HostFilterId` wrapping the stable configured-connection id.
- `AgentsViewPreferences` and supporting enums.
- `AgentOrderKey::{Session, TransientAgent}`.
- `AgentsViewPreferencesStoreErrorKind`.
- `AgentsViewPreferencesStoreError`.
- `AgentsViewPreferencesSnapshot`.
- `SetAgentsViewPreferencesPayload`.
- `AgentsViewPreferencesNotifyPayload`.
- `FrameKind::SetAgentsViewPreferences`.
- `FrameKind::AgentsViewPreferencesNotify`.
- `HostBootstrapPayload.agents_view_preferences:
  Option<AgentsViewPreferencesSnapshot>` with `#[serde(default)]`.

Only the primary local host emits a non-`None` bootstrap field and accepts the
set frame. Remote hosts do not own a competing copy.

Bump `PROTOCOL_VERSION` because new frame kinds are added. Keep the new bootstrap
field serde-defaulted so tests and staged clients can construct minimal bootstrap
payloads during the transition.

### 8.2 Phase 1a server changes

- Add `AgentsViewPreferencesStore` on the primary local host.
- Add store path wiring to `HostStorePaths`, `spawn_host`, and test spawn helpers
  for the primary/local host path.
- Load the preference store in `spawn_host_inner` without panicking on corrupt
  preference data.
- Include `Some(snapshot)` in the primary host bootstrap; include `None` or omit
  the field on remote hosts.
- Implement `HostHandle::set_agents_view_preferences`:
  1. validate update,
  2. deterministically prune/rewrite manual-order keys,
  3. apply to store atomically,
  4. fan out `AgentsViewPreferencesNotify` to primary-host subscribers.
- Add host-stream router and validator cases on `/host/<uuid>` for the primary
  host route.
- On corrupt preference file load, emit defaults plus typed `load_error`; never
  block host registration.

### 8.3 Phase 1a frontend changes

- Add frontend generated bindings for the new preference protocol types.
- Add one `agents_view_preferences` snapshot signal populated only from the
  primary local host bootstrap/notify.
- Add `pending_agents_view_overlay` for in-flight preference updates.
- Resolve every host filter/order reference through stable `HostFilterId` values,
  not stream paths or connection instance ids.
- Rewrite `AgentMonitorView` to:
  - read persisted filters/sort/group/density/hide-finished from
    `effective_preferences`,
  - support non-folder grouping (`Flat`, `Status`, `Backend`, `Project`),
  - keep search ephemeral,
  - send typed preference updates,
  - use the pending overlay for manual drag/keyboard reorder,
  - no longer mutate `agent_monitor_order` locally.
- Update `AgentsPanel` to read the same effective preference filters. If the
  sidebar keeps a reduced toolbar, its toggles still write the same preference
  object.
- Scope Phase 1a `hide_finished` to existing fatal/terminated rows only.

### 8.4 Phase 1a testing

Native tests follow `tests/TESTING.md`: drive the public client through a real
server with a mock backend and assert observable events, not internals
(`tests/TESTING.md:5-8`, `tests/TESTING.md:52-64`).

Add or extend client-level tests for:

1. Preference update flow:
   - connect to the primary host,
   - send `SetAgentsViewPreferences`,
   - observe `AgentsViewPreferencesNotify`,
   - reconnect,
   - assert `HostBootstrap` carries the updated preference snapshot.
2. Corrupt preference file:
   - write invalid preference JSON,
   - connect,
   - assert host registration succeeds,
   - assert bootstrap contains defaults plus typed load error,
   - send Reset or another valid mutation,
   - assert the load error clears after a valid notify.
3. Stable host ids:
   - save a host filter using `HostFilterId`,
   - reconnect the host with a new stream path,
   - assert the filter still applies.
4. Manual order canonicalization:
   - send duplicate/stale/transient manual keys,
   - assert the notify contains deterministic pruned/reordered keys.
5. Pending overlay behavior:
   - perform a manual reorder,
   - assert the rendered order changes immediately from the overlay,
   - apply the notify,
   - assert the overlay clears and the server snapshot remains.

Wasm/component tests:

- Agents Center renders server preferences after bootstrap.
- Toggling a filter sends a preference update; local toggles may wait for notify,
  while manual reorder uses the pending overlay.
- Search input filters current rows but is not present in the persisted
  preference payload.
- Density classes switch from effective preference state.
- `hide_finished` hides only existing fatal/terminated rows in Phase 1a.
- Existing keyboard reorder tests should assert the pending-overlay mutation seam
  rather than local durable vector mutation.

---

## 9. Phase 1b: virtual default groups

Phase 1b is independently shippable after Phase 1a. It adds server-emitted
virtual organization state for `Project { id }` and `NoProject` groups, but still
has no custom folders or durable folder placement store.

### 9.1 Phase 1b protocol additions

Add:

- `AgentOrganizationContainer`.
- `AgentPlacementSource`.
- `AgentPlacement`.
- `AgentOrganizationSnapshot`.
- `AgentOrganizationRecord`.
- `AgentOrganizationNotifyPayload`.
- `FrameKind::AgentOrganizationNotify`.
- `HostBootstrapPayload.agent_organization` with `#[serde(default)]`.

Phase 1b may define `AgentFolderId` and `AgentFolder` even if the server emits an
empty `folders` list. That avoids another generated-type churn in Phase 2.

Bump `PROTOCOL_VERSION` if Phase 1b ships separately from Phase 1a.

### 9.2 Phase 1b server changes

- Build a virtual-only `AgentOrganizationSnapshot` from current live agents:
  - no custom folders,
  - one default placement per organizable live agent,
  - `Project { id }` for a valid project id,
  - `NoProject` otherwise,
  - `source: Default`.
- Include the snapshot in every host bootstrap because organization is per host.
- Emit organization placement upserts when new agents appear, session ids resolve,
  or project associations change enough to alter default placement.
- Exclude internal/ephemeral helper agents.
- Add host-stream validator parsing for `AgentOrganizationNotify`.

### 9.3 Phase 1b frontend changes

- Add per-host `agent_organization` snapshot signals.
- Dispatch `HostBootstrapPayload.agent_organization` and
  `AgentOrganizationNotify` into organization state.
- Render folder-first virtual groups in the Agents Center using the same
  effective preferences from Phase 1a.
- Enable the `Folders` and `FoldersThen*` group modes now that organization
  snapshots exist.
- Keep the sidebar behavior unchanged or minimally grouped until Phase 2 if that
  is needed to ship Phase 1b cleanly; center virtual grouping is the required
  surface for this phase.
- Apply the filter/group edge cases from §7.6.

### 9.4 Phase 1b testing

Native client-level tests:

1. Virtual organization defaults:
   - create a project,
   - spawn one project agent and one no-project agent,
   - assert bootstrap/notify exposes `Project { id }` and `NoProject`
     placements.
2. Project delete default behavior:
   - delete the project,
   - assert default placements for still-live affected agents become `NoProject`
     or disappear if the live agent was closed; agents/sessions are not deleted.
3. Protocol validation:
   - invalid organization notify payloads fail validator parsing.

Wasm/component tests:

- Virtual Project and No Project groups render from organization state.
- Empty virtual groups hide according to §7.6.
- A filtered-out parent with a visible child renders the child as an orphan row
  with the explanatory badge.

---

## 10. Phase 2: custom folders, drag/drop, persistence, sidebar parity

Phase 2 is independently shippable on top of Phase 1b. It adds user-created
folders, durable placements, drag/drop, keyboard move commands, cleanup hooks,
and full sidebar parity.

### 10.1 Phase 2 protocol additions

Add:

- `AgentFolderCreatePayload`.
- `AgentFolderRenamePayload`.
- `AgentFolderDeletePayload`.
- `AgentFolderReorderPayload`.
- `AgentMoveToFolderPayload`.
- `FrameKind::AgentFolderCreate`.
- `FrameKind::AgentFolderRename`.
- `FrameKind::AgentFolderDelete`.
- `FrameKind::AgentFolderReorder`.
- `FrameKind::AgentMoveToFolder`.

`AgentOrganizationNotify` and its record payload are already present from Phase
1b; Phase 2 starts emitting folder upsert/delete records in addition to placement
records.

Bump `PROTOCOL_VERSION` again if Phase 2 ships separately from Phase 1b. If
Phase 1a, Phase 1b, and Phase 2 land in one implementation branch, a single
protocol bump is sufficient, but every new bootstrap field still needs serde
defaults.

### 10.2 Phase 2 server changes

- Add `AgentOrganizationStore` with versioned file, validation, atomic writes,
  and missing-file empty defaults.
- Add store path wiring to normal and test host startup.
- Add in-memory transient placement map to `HostState`.
- Implement folder create/rename/delete/reorder methods on `HostHandle`.
- Implement `move_agent_to_folder` with placement precedence:
  - if payload has a valid `session_id`, persist session assignment,
  - else if live agent has a known session id, persist session assignment,
  - else create/update transient assignment keyed by `AgentId`.
- Promote transient assignments when session registration completes.
- Remove transient assignment on agent close.
- Remove session assignment on session delete.
- Reparent project-scoped folders and rewrite affected placements on project
  delete.
- Fan out `AgentOrganizationNotify` records after each mutation or cleanup.
- Validate router inputs and protocol validator payload parsing on `/host/<uuid>`.
- Reject organization mutations for unknown agents, unknown folders, invalid
  containers, cyclic folder moves, duplicate reorder ids, deleted projects, and
  internal/ephemeral helpers.

### 10.3 Phase 2 frontend changes

- Render custom folder tree in the Agents Center.
- Add folder create, rename, delete, and empty-state affordances.
- Add drag/drop for agents and folders.
- Add keyboard fallback for all drag/drop mutations.
- Update sidebar to folder-outer / sub-agent-inner rendering.
- Preserve existing parent collapse behavior inside folders.
- Add CSS listed in §7.5.
- Remove any remaining local durable order/filter state.
- Surface command errors near the relevant folder/row when mutations fail.

### 10.4 Phase 2 testing

Native client-level tests:

1. Folder lifecycle:
   - create folder,
   - observe `AgentOrganizationNotify::Upsert`,
   - reconnect and assert bootstrap includes it,
   - rename and reorder,
   - delete and assert delete notify.
2. Moving a resolved-session agent:
   - spawn through mock backend until session id is known,
   - move to folder,
   - close agent,
   - resume session,
   - assert placement persists.
3. Moving one of multiple live agents sharing a session:
   - arrange two live agents with the same `SessionId`,
   - move one to a folder,
   - assert both live views render in the session-assigned container.
4. Moving a pre-session live agent:
   - move before session id resolves,
   - assert transient placement notify,
   - resolve session,
   - assert promotion to session assignment.
5. Sessionless close:
   - move sessionless agent,
   - close before session id,
   - assert transient placement disappears and no persisted assignment remains.
6. Project deletion cleanup:
   - create project-scoped folder,
   - move agent/session there,
   - delete project,
   - assert folder is reparented to `NoProject`, agent/session are not deleted,
     and session project metadata follows existing detach behavior.
7. Invalid mutations:
   - cyclic folder parent,
   - duplicate reorder ids,
   - unknown agent/folder,
   - move to `HostRoot`,
   - internal helper agent.
   Each should produce an observable protocol error/command error.

Wasm/component tests:

- Center renders custom folders, virtual groups, and empty-folder states.
- Sidebar renders folder-outer / parent-child-inner nesting.
- Fatal/terminated rows are greyed and hidden only when `hide_finished` is true;
  richer finished history waits for a lifecycle field.
- Drag/drop seam builds the correct `AgentMoveToFolder` and
  `AgentFolderReorder` payloads.
- Keyboard move/folder picker sends the same mutations as drag/drop.
- Delete-folder confirmation copy makes clear that agents are not deleted.

---

## 11. Protocol and compatibility notes

- New frame kinds require a `PROTOCOL_VERSION` bump. The current constant is in
  `protocol/src/types.rs:16`.
- New bootstrap fields should use `#[serde(default)]` even when protocol version
  bumps, because this repository has synthetic bootstrap builders in tests and
  phased implementation work often constructs partial payloads.
- Do not reuse `SetSetting` / `HostSettings` for Agents view preferences. That
  would blur host runtime settings with client-global view preferences.
- Do not send organization mutations over agent streams. Folders are host-owned
  organization state, so frames route on `/host/<uuid>` like projects.
- Validator additions are required for every new host-stream frame. Current
  host-frame parsing already lives in `protocol/src/validator.rs` near settings
  and project handling (`protocol/src/validator.rs:249-320`,
  `protocol/src/validator.rs:371-388`).
- Frontend dispatch should treat preference and organization notifies like
  project/settings notifies for the server-owned base: replace or upsert protocol
  state, then let reactive views render the base plus any pending overlay. Do not
  add a refresh button as a stale-state workaround.
- Pending overlays are frontend interaction state, not protocol state. They must
  be non-persistent, domain-scoped, and reconciled by server notifies/bootstrap.

---

## 12. Open risks

### 12.1 Primary-host preference ownership

The preference ownership rule is decided for Phase 1a: one primary local host owns
the client-global Agents view preference store. Remote hosts do not emit
competing snapshots and do not accept `SetAgentsViewPreferences`. The remaining
implementation risk is plumbing: startup must identify the primary local host
before remote bootstraps can arrive, and dispatch must ignore/diagnose accidental
remote preference snapshots instead of replacing the global signal.

### 12.2 Stable configured-host identity

Host filters and transient `AgentOrderKey::TransientAgent` keys must use
`HostFilterId`, a protocol newtype wrapping the stable configured-connection id.
Current frontend agent keys include `host_id: String`
(`frontend/src/state.rs:52-60`, `frontend/src/state.rs:72-88`), but persisted
preferences must not use stream paths or per-connection ids. Phase 1a is not
shippable until the bridge/host-registry path can provide the durable configured
id to preference rendering and mutation payloads.

### 12.3 Finished-agent semantics

Current frontend status is derived from `started`, `fatal_error`, streaming maps,
turn-active maps, and compaction maps. There is no explicit server-emitted
"finished but still visible" lifecycle state. If the UX needs greyed finished
agents beyond fatal/terminated rows, add a typed lifecycle field instead of
encoding it as frontend inference.

### 12.4 Project deletion reparenting

Reparenting project-scoped folders to `NoProject` preserves user organization but
may surprise users who expected project folders to disappear with the project.
The delete confirmation and release notes should state this clearly.

### 12.5 Drag/drop accessibility and mobile behavior

HTML drag/drop is already used in the Agent Monitor, but it is weak on touch
surfaces and inaccessible without keyboard alternatives. Phase 2 is not done
until keyboard and screen-reader flows are tested.

### 12.6 Multiple live agents sharing a session

The Phase 2 rule is decided: durable placement is keyed by `SessionId`, so all
live agents that share a session move together. This matches the absence of a
reverse-uniqueness invariant in current host state and keeps the persistence
model simple. Phase 2 must add a client-level test for this shared-move behavior.
If users later need per-live-agent divergence for duplicated session views, that
requires a new explicit placement key and migration plan.

### 12.7 Notify ordering during session promotion

A move can happen before `AgentStart` supplies a session id. Promotion from
transient assignment to session assignment must not briefly render the agent in
its default group. The server should emit delete/upsert notifies in one serialized
host-state path, and the frontend should apply them in sequence.
