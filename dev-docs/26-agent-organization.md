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

The feature now has two pillars:

1. **Pillar A: persistent Agents-view preferences.** This has landed and is the
   substrate for all future organization work.
2. **Pillar B: Smart Views + Tags + Pins.** This replaces the earlier manual
   folder design.

The previous folder-based model is superseded. Do not implement
`AgentFolder`, `AgentOrganizationContainer`, `AgentPlacement`,
`AgentOrganizationSnapshot`, `AgentOrganizationNotify`, or drag-into-folder
behavior from older drafts. Agents are ephemeral and attribute-rich; manually
filing them into a durable hierarchy creates high upkeep and stale organization.
Auto-organizing views, lightweight manual tags, and pins fit the data model
better while keeping the server as the single source of truth.

---

## 1. Goals

- Preserve the Phase 1a flicker fix: filters, sort, group, density,
  hide-finished, and manual order are server-owned durable preferences replayed
  by the primary local host.
- Make the Agents Center at least as capable as the sidebar Agents panel while
  keeping both surfaces projections of the same protocol state.
- Add **Smart Views** for reusable saved queries over agents, using the same
  `AgentsViewFilters`, `AgentSortMode`, `AgentGroupMode`, and
  `hide_finished` fields that already drive the Agents Center.
- Add **manual tags** for user-controlled labels and **system tags** derived by
  the server from typed agent attributes such as origin, backend, workflow,
  parentage, and project.
- Add **pins** for important agents/sessions so they float above the current
  projection without changing project/session/backend semantics.
- Keep search ephemeral. Search narrows the current projection but is never
  saved in preferences or Smart Views.
- Keep all durable organization state in server-owned stores and typed protocol
  payloads. The frontend may keep only ephemeral interaction state and
  short-lived optimistic overlays.
- Preserve existing parent/child sub-agent nesting as an inner display rule;
  Smart Views, tags, and pins must not infer provenance from
  `parent_agent_id` beyond server-emitted tags/grouping.

## 2. Non-goals

- No manual folders, hierarchical filing, folder drag/drop, folder persistence,
  or folder migration.
- No remote push, PR, tag, or release behavior.
- No change to backend-native project/session semantics.
- No mutation of `agent.project_id`, `SessionRecord.project_id`, backend
  session metadata, or workflow metadata when tagging, pinning, or switching
  views.
- No frontend-only persistence in `localStorage`, indexed DB, or Tauri-only
  state.
- No frontend reconstruction of tag taxonomy or organization semantics. If the
  UI needs a label, filter dimension, or grouping fact, add it to protocol state
  and let the server emit it.
- No automatic deletion of agents or sessions as a side effect of deleting a tag,
  removing a pin, or cleaning transient organization state.
- No real-AI tests for this work. The test plan uses client -> server -> mock
  backend flows.

---

## 3. Landed Phase 1a substrate

Phase 1a has landed. Future work must build on it instead of redesigning it.
The relevant source anchors are below.

### 3.1 Protocol surface

- `PROTOCOL_VERSION` is currently `15` (`protocol/src/types.rs:16`).
- `FrameKind::SetAgentsViewPreferences` and
  `FrameKind::AgentsViewPreferencesNotify` exist on the protocol enum
  (`protocol/src/types.rs:487-489`, `protocol/src/types.rs:569-577`) and are
  serialized as `set_agents_view_preferences` /
  `agents_view_preferences_notify` (`protocol/src/types.rs:643-644`,
  `protocol/src/types.rs:727-728`).
- `HostBootstrapPayload.agents_view_preferences` is an optional,
  serde-defaulted primary-host snapshot (`protocol/src/types.rs:1080-1107`).
- `HostFilterId`, `AgentsViewPreferences`, `AgentsViewFilters`,
  `AgentProjectFilter`, `AgentSortMode`, `AgentGroupMode`,
  `AgentListDensity`, `AgentStatusFilter`, `AgentOrderKey`,
  `AgentsViewPreferencesUpdate`, `SetAgentsViewPreferencesPayload`,
  `AgentsViewPreferencesStoreError*`, `AgentsViewPreferencesSnapshot`, and
  `AgentsViewPreferencesNotifyPayload` are canonical protocol types
  (`protocol/src/types.rs:1109-1248`).
- The landed `AgentGroupMode` variants are `Flat`, `Status`, `Backend`, and
  `Project` (`protocol/src/types.rs:1167-1175`). `Tag` is a Phase 2b addition;
  folder group modes are not present and should not be reintroduced.

### 3.2 Store and host ownership

- `AgentsViewPreferencesStore` loads defaults on missing files, records typed
  load errors for corrupt/unsupported data, snapshots full state, applies typed
  updates, and writes atomically (`server/src/store/agents_view_preferences.rs:21-80`,
  `server/src/store/agents_view_preferences.rs:153-185`).
- The store canonicalizes filters and validates manual order without silently
  accepting empty or duplicate keys
  (`server/src/store/agents_view_preferences.rs:197-240`,
  `server/src/store/agents_view_preferences.rs:242-335`).
- `HostRuntimeConfig.agents_view_preferences_primary` decides whether a host owns
  the store; the default local host is primary
  (`server/src/host.rs:147-177`).
- `HostState` stores the preferences store as `Option<Arc<Mutex<_>>>`, so remote
  or non-primary hosts have no competing store (`server/src/host.rs:240-247`).
- Host bootstrap emits `Some(snapshot)` only when that optional store exists
  (`server/src/host.rs:668-671`, `server/src/host.rs:723-745`).
- `HostHandle::set_agents_view_preferences` rejects non-primary hosts, applies
  the store mutation, and fans out a full notify from the single owner
  (`server/src/host.rs:4415-4435`).
- Manual order canonicalization rewrites live local transient keys to session
  keys, drops unverifiable remote transient keys, and deduplicates deterministically
  (`server/src/host.rs:4452-4508`).
- Host startup wires the preferences path into normal and test store paths, and
  only constructs the store when the runtime config marks the host primary
  (`server/src/host.rs:8145-8170`, `server/src/host.rs:8229-8247`,
  `server/src/host.rs:8287-8305`, `server/src/host.rs:8323-8325`,
  `server/src/host.rs:8391-8398`).
- Fanout follows the existing single-owner host-subscriber pattern and drops dead
  streams (`server/src/host.rs:9614-9637`, `server/src/host.rs:9833-9842`).

### 3.3 Frontend projection and optimistic overlay

- The frontend constant `PRIMARY_LOCAL_HOST_ID` is `local`, and comments make it
  the only host allowed to own Agents-view preferences (`frontend/src/state.rs:108-112`).
- `AgentsViewOverlay` is explicitly short-lived, non-persisted, layered over the
  server snapshot, and reconciled by dropping it on any authoritative snapshot
  (`frontend/src/state.rs:1033-1051`).
- `AppState` holds the server snapshot, owning host id, pending overlay, and
  overlay generation (`frontend/src/state.rs:1338-1355`), initialized to defaults
  in `AppState::new` (`frontend/src/state.rs:1614-1620`).
- `effective_agents_view_preferences()` derives the rendered preferences from
  server snapshot + overlay (`frontend/src/state.rs:2029-2043`).
- `apply_agents_view_snapshot()` ignores non-primary snapshots, installs the
  authoritative snapshot, and clears the overlay wholesale
  (`frontend/src/state.rs:2046-2075`).
- `set_agents_view_overlay()` installs a local optimistic domain change and arms
  the stale-overlay timeout; `agents_view_overlay_pending()` reports pending
  state (`frontend/src/state.rs:2078-2135`).
- Host runtime cleanup intentionally does not prune preferences or the overlay,
  because pruning frontend-local state was the root flicker/reset bug
  (`frontend/src/state.rs:2652-2658`).
- `AgentMonitorView` maps live rows to durable `AgentOrderKey`s
  (`frontend/src/components/agent_monitor_view.rs:47-81`), filters with the
  protocol `AgentsViewFilters` plus ephemeral search
  (`frontend/src/components/agent_monitor_view.rs:95-130`), sorts and groups from
  protocol modes (`frontend/src/components/agent_monitor_view.rs:155-255`), sends
  preference updates to the primary host (`frontend/src/components/agent_monitor_view.rs:489-525`),
  and renders from the effective preference memo (`frontend/src/components/agent_monitor_view.rs:583-631`).
- Existing wasm tests cover immediate optimistic UI, persistence across host
  churn, and drop-on-notify reconciliation (`frontend/src/components/agent_monitor_view.rs:1846-1884`,
  `frontend/src/components/agent_monitor_view.rs:1887-1924`,
  `frontend/src/components/agent_monitor_view.rs:1926-1971`).

### 3.4 Testing constraints

Native tests for new organization behavior must follow `tests/TESTING.md`:
client-level end-to-end tests exercise client -> server -> mock backend and
assert observable protocol responses/events, not internals
(`tests/TESTING.md:5-8`, `tests/TESTING.md:52-64`). The fixture pattern uses a
real server with a mock backend (`tests/TESTING.md:14-29`), and tests should be
comprehensive flows without fallbacks (`tests/TESTING.md:65-70`).

---

## 4. Decisions and rationale

### 4.1 The UI remains a pure projection

`dev-docs/01-philosophy.md` requires one source of truth, server-owned behavior,
state through events, explicit ownership, and protocol types end-to-end
(`dev-docs/01-philosophy.md:13-64`). It also requires the UI to be a pure
projection of signal state, with no hidden caches or stale snapshots
(`dev-docs/01-philosophy.md:126-146`).

Therefore Smart Views, tag definitions, tag assignments, computed system tags,
and pins are server-owned typed state. The frontend can hold:

- focused inputs,
- open menus,
- in-progress chip editing,
- drag/hover gestures for row affordances,
- and optimistic overlays that are dropped by the next authoritative snapshot.

It must not hold durable Smart Views, tag assignments, pinned sets, or derived
system-tag taxonomy as local storage or frontend-only maps.

### 4.2 Pillar A is unchanged and remains the substrate

`AgentsViewPreferences` stays the live active query for the Agents Center. It
continues to own:

- filters,
- sort mode,
- group mode,
- density,
- hide-finished,
- manual order.

Search stays out of this object. Smart Views save and restore only the reusable
query fields: `filters`, `sort_mode`, `group_mode`, and `hide_finished`. Density
and manual order remain global active-view preferences because they are display
style and row-order state rather than named query semantics.

### 4.3 Manual folders are superseded

The older design tried to make durable folders and placements an outer hierarchy.
That is no longer the chosen model.

Reasons:

- Live agents are often short-lived, restarted, resumed, or replaced by multiple
  live views of the same session.
- Useful organization dimensions already exist as typed attributes: host,
  project, backend, origin, workflow, team/custom-agent metadata, parentage,
  status, and session id.
- Manual hierarchy makes the user keep stale folders clean as agents close,
  sessions resolve, projects disappear, or workflows complete.
- Query/tag/pin models are lower-upkeep: automatic views follow attributes,
  manual tags capture durable user intent, and pins highlight exceptional rows
  without changing ownership.

### 4.4 Smart Views are saved queries over the landed preferences

A Smart View is a named reusable query:

```rust
pub struct SmartView {
    pub id: SmartViewId,
    pub name: String,
    pub filters: AgentsViewFilters,
    pub sort_mode: AgentSortMode,
    pub group_mode: AgentGroupMode,
    pub hide_finished: bool,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SmartViewId {
    BuiltIn { id: BuiltInSmartViewId },
    User { id: UserSmartViewId },
}

#[serde(rename_all = "snake_case")]
pub enum BuiltInSmartViewId {
    All,
    Active,
    FailedTerminated,
}

#[serde(transparent)]
pub struct UserSmartViewId(pub String);
```

Rules:

- Built-in views are emitted by the server, non-deletable, and ordered before
  user views.
- User views are persisted in the primary-local-host store in user-defined order.
- The active Smart View id is persisted by the same owner.
- Selecting a Smart View updates active `AgentsViewPreferences` by copying the
  view's `filters`, `sort_mode`, `group_mode`, and `hide_finished` into the
  active preference snapshot. It does not copy search, density, or manual order.
- The frontend uses the existing optimistic-overlay path for the copied
  preference domains, then drops the overlay when the server emits the full
  authoritative snapshot.
- Search remains ephemeral and is never saved into a Smart View.

Recommended built-ins for Phase 2a:

- **All**: empty filters, `ManualThenActivity`, `Flat`, `hide_finished = false`.
- **Active**: statuses for initializing/thinking/compacting/idle, no terminated
  rows, default sort/group.
- **Failed/terminated**: terminated status, `hide_finished = false` so the view
  can show the rows it is explicitly asking for.

The exact label/copy can change, but the ids must be stable protocol values.

### 4.5 Tags are two-tier: manual persisted, system derived

Tags have two origins:

1. **Manual tags** are created by the user and persisted by the primary local
   host. Assignments are session-keyed whenever possible and transient-agent-keyed
   only while a live agent has no session id.
2. **System tags** are computed by the server and not written to the tag store.
   They are derived from typed agent/project/workflow/backend/origin state, for
   example `workflow`, `codex`, `sub-agent`, or the project name.

Manual and system tags are displayed distinctly. Manual tags are editable;
system tags are read-only facts emitted by the server.

Because the store is client-global and the projection can include remote hosts,
every per-agent organization key is scoped by stable `HostFilterId`:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentAnnotationTarget {
    Session {
        host_id: HostFilterId,
        session_id: SessionId,
    },
    TransientAgent {
        host_id: HostFilterId,
        agent_id: AgentId,
    },
}
```

The durable part is still session-keyed: once a session id exists, manual tag and
pin state must be rewritten to the `Session` target and the transient target must
be pruned. The `host_id` prevents collisions across host inventories and keeps
`HostFilterId` stability from Phase 1a.

### 4.6 Tags become a filter and group dimension

Phase 2b extends `AgentsViewFilters` additively:

```rust
pub struct AgentsViewFilters {
    // landed fields...
    #[serde(default)]
    pub tags: Vec<AgentTagRef>,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentTagRef {
    Manual { tag_id: AgentManualTagId },
    System { tag_id: AgentSystemTagId },
}

#[serde(transparent)]
pub struct AgentManualTagId(pub String);

#[serde(transparent)]
pub struct AgentSystemTagId(pub String);
```

An empty tag filter means no tag constraint. A non-empty tag filter means the
agent must have at least one selected tag unless a later UX explicitly adds
"match all" semantics as a typed filter mode.

Phase 2b also adds:

```rust
pub enum AgentGroupMode {
    Flat,
    Status,
    Backend,
    Project,
    Tag,
}
```

`Tag` grouping is server-defined in terms of emitted tag assignments. Agents with
multiple tags appear under each matching tag group in grouped display; untagged
agents appear in an explicit `Untagged` group. Pins remain an outer section above
tag grouping.

### 4.7 Pins are lightweight per-agent emphasis

Pins are a set of `AgentAnnotationTarget`s owned by the primary local host.
Pinned agents float above the current projection without changing their project,
session, tag, or backend state.

Display rule:

- Render a top **Pinned** section before the normal projection.
- Inside the pinned section, apply the active filters first; a pinned row that
  does not match the current Smart View/filter/search is not forced visible.
- Within pinned and unpinned sections, preserve the active sort/group rules.

Persistence and cleanup use the same session-keyed/transient-target rules as
manual tags.

### 4.8 Full snapshots reconcile optimistic overlays

Future Smart View, tag, and pin overlays must follow the Phase 1a discipline:

- install local overlay only for immediate feedback,
- send a typed host-stream frame to the primary local host,
- drop the overlay on any authoritative snapshot for that domain,
- show server errors explicitly,
- never persist overlays or use them as durable local truth.

The drop-on-any-authoritative-snapshot rule is already implemented for
preferences in `AppState` (`frontend/src/state.rs:2046-2075`). New overlays should
match that behavior rather than adding request ids or equality-only reconcile.

---

## 5. Pillar A: landed persistent Agents-view preferences

Pillar A is not being redesigned. It remains the base query and display state for
all Agents Center projections.

### 5.1 Server model

The existing `AgentsViewPreferencesStore` remains the primary-local-host-owned
store at `~/.tyde/agents_view_preferences.json`, with
`TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH` as the override
(`server/src/store/agents_view_preferences.rs:44-55`). Missing files return
healthy defaults; corrupt files report typed load errors but do not prevent host
registration (`server/src/store/agents_view_preferences.rs:83-151`). Valid
mutations rewrite the file atomically and clear load errors
(`server/src/store/agents_view_preferences.rs:64-80`,
`server/src/store/agents_view_preferences.rs:153-185`).

Keep these rules:

- Primary local host owns and emits preferences.
- Remote hosts emit `None` for `agents_view_preferences` and reject mutations.
- `HostFilterId` is the stable configured-host id, not a stream path.
- Manual order uses `SessionId` whenever possible and transient agent keys only
  before session resolution.
- Store validation rejects empty ids and duplicate manual-order keys.
- Corrupt preference data is surfaced as typed `load_error`; it must not panic or
  block host connectivity.

### 5.2 Frontend model

The frontend continues rendering from:

```text
server snapshot base + non-persisted optimistic overlay
```

The effective preference object is produced by
`effective_agents_view_preferences()` (`frontend/src/state.rs:2029-2043`). The
server snapshot is replaced only from primary-local-host bootstrap/notify; any
non-primary snapshot is ignored (`frontend/src/state.rs:2057-2065`). A new
snapshot drops the entire overlay (`frontend/src/state.rs:2066-2075`).

Future work must not reintroduce durable `agent_monitor_order`,
`agents_panel_filters`, `localStorage`, or Tauri-only caches for this state.

### 5.3 Active preference semantics

Current landed modes:

- Sort: `ManualThenActivity`, `NewestFirst`, `OldestFirst`, `NameAsc`, `Status`,
  `Backend`, `Project` (`protocol/src/types.rs:1154-1165`).
- Group: `Flat`, `Status`, `Backend`, `Project`
  (`protocol/src/types.rs:1167-1175`).
- Density: `Comfortable`, `Compact` (`protocol/src/types.rs:1177-1183`).
- Status filters: `Initializing`, `Thinking`, `Compacting`, `Idle`,
  `Terminated` (`protocol/src/types.rs:1185-1193`).

Phase 2a Smart Views use exactly these modes. Phase 2b adds tag filters and
`AgentGroupMode::Tag`.

---

## 6. Pillar B: Smart Views + Tags + Pins

Pillar B is server-owned user organization state layered on top of Pillar A.
The primary local host owns all durable Pillar B state, because the view is
client-global across the connected host projection and remote hosts must not emit
competing snapshots.

### 6.1 Snapshot shape

Recommended clean protocol shape: extend the existing
`AgentsViewPreferencesSnapshot` and reuse `AgentsViewPreferencesNotify` as the
full authoritative snapshot for Pillar A and Pillar B.

```rust
pub struct AgentsViewPreferencesSnapshot {
    pub preferences: AgentsViewPreferences,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<AgentsViewPreferencesStoreError>,
    #[serde(default)]
    pub smart_views: AgentsSmartViewsSnapshot,
    #[serde(default)]
    pub tags: AgentTagsSnapshot,
    #[serde(default)]
    pub pins: AgentPinsSnapshot,
}
```

Rationale: Smart Views, tags, and pins all affect the same Agents Center
projection and share the same owner, bootstrap field, no-panic store rules, and
optimistic-overlay reconciliation. Reusing the existing notify avoids parallel
frontend sources of truth and keeps bootstrap/live updates isomorphic.

`AgentsViewPreferencesNotify` remains a full snapshot. Any mutation in
preferences, Smart Views, tags, or pins emits the full updated snapshot.

### 6.2 Smart View snapshot

```rust
pub struct AgentsSmartViewsSnapshot {
    #[serde(default)]
    pub built_in_views: Vec<SmartView>,
    #[serde(default)]
    pub user_views: Vec<SmartView>,
    pub active_view_id: SmartViewId,
}
```

Built-ins are generated by the server for every snapshot and are not stored as
user records. User views and `active_view_id` are persisted by the primary local
host.

### 6.3 Tag snapshot

```rust
pub struct AgentTagsSnapshot {
    #[serde(default)]
    pub descriptors: Vec<AgentTagDescriptor>,
    #[serde(default)]
    pub manual_assignments: Vec<AgentManualTagAssignment>,
    #[serde(default)]
    pub system_assignments: Vec<AgentSystemTagAssignment>,
}

pub struct AgentTagDescriptor {
    pub tag: AgentTagRef,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<AgentTagColor>,
    pub origin: AgentTagOrigin,
}

#[serde(rename_all = "snake_case")]
pub enum AgentTagOrigin {
    Manual,
    System,
}

#[serde(transparent)]
pub struct AgentTagColor(pub String);

pub struct AgentManualTagAssignment {
    pub target: AgentAnnotationTarget,
    pub tag_ids: Vec<AgentManualTagId>,
}

pub struct AgentSystemTagAssignment {
    pub target: AgentAnnotationTarget,
    pub tag_ids: Vec<AgentSystemTagId>,
}
```

Only manual descriptors and manual assignments are stored. System descriptors and
assignments are computed from current server state for each snapshot/notify.
Manual tag colors should be validated as a constrained color string, for example
hex RGB/RGBA, before persisting.

### 6.4 Pin snapshot

```rust
pub struct AgentPinsSnapshot {
    #[serde(default)]
    pub pinned: Vec<AgentAnnotationTarget>,
}
```

The server canonicalizes pinned targets on every mutation:

1. Drop duplicates after the first occurrence.
2. Rewrite transient targets to session targets when the session id is known.
3. Drop transient targets for closed sessionless agents.
4. Retain session targets across agent close/resume.

### 6.5 Store strategy

Phase 2a may extend `AgentsViewPreferencesStore` directly with Smart Views, or
split the persisted file into an internal versioned record that still emits the
same `AgentsViewPreferencesSnapshot`. Phase 2b may continue that store extension
or introduce a sibling primary-local-host store. Either implementation is valid
only if it preserves these externally visible rules:

- one primary-local-host owner,
- no competing remote snapshots,
- missing-file defaults,
- corrupt/unsupported file reported as a typed load error without panic,
- atomic writes,
- full snapshot on bootstrap and notify,
- validation before persistence,
- no durable frontend-local fallback.

If a sibling store is introduced for tags/pins, merge its load errors into the
snapshot as typed domain errors rather than panicking. Do not use `HostSettings`
for this state.

### 6.6 Cleanup hooks

Manual tag and pin cleanup is keyed by `AgentAnnotationTarget`:

- **Session resolution:** promote any matching `TransientAgent` tag assignments
  or pins to `Session` and emit one full snapshot.
- **Agent close before session resolution:** remove transient assignments and
  transient pins for that agent and emit a full snapshot.
- **Session delete:** remove manual tag assignments and pins for that session and
  emit a full snapshot. Do not delete tag definitions unless the user deletes the
  tag.
- **Project delete/rename:** system project tags are recomputed from project
  state. Manual tags and pins do not move or change.
- **Tag delete:** remove the manual tag definition and strip that tag id from all
  manual assignments. Agents and sessions are not deleted.

---

## 7. UX design

### 7.1 Agents Center layout

The Agents Center is the primary management surface for live agents.

Layout:

1. Header: title, agent count, current host/project scope summary.
2. Smart View switcher: built-in views plus user views. Tabs are acceptable for
   a small set; a dropdown is acceptable when user views exceed the available
   width.
3. Toolbar:
   - search input (ephemeral, never persisted),
   - "Save current view as…",
   - host filter,
   - project filter,
   - status filter,
   - backend filter,
   - origin filter,
   - tag filter after Phase 2b,
   - "Hide finished",
   - sort selector,
   - group selector,
   - density selector,
   - reset active preferences action.
4. Manage Smart Views menu:
   - rename user view,
   - update user view from current preferences,
   - delete user view,
   - reorder user views.
5. Agent list:
   - pinned section first after Phase 2b,
   - current grouping (`Flat`, `Status`, `Backend`, `Project`, later `Tag`),
   - rows/cards with tag chips and pin toggles after Phase 2b.

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
- manual and system tag chips after Phase 2b,
- pin toggle after Phase 2b,
- actions: Open, Rename, Compact when eligible, Close.

### 7.2 Smart View UX

- Selecting a built-in or user view immediately overlays its query fields into
  the active preferences and sends the Smart View set-active frame.
- "Save current view as…" opens a name input. The saved view captures current
  `filters`, `sort_mode`, `group_mode`, and `hide_finished`; it omits search,
  density, and manual order.
- "Update view" overwrites the selected user view's query fields from current
  effective preferences.
- Built-ins can be selected but not renamed, updated, deleted, or reordered.
- If the active user view is deleted, the server selects `All` and copies the
  `All` query into active preferences in the same authoritative snapshot.

### 7.3 Tags UX

- Manual tags render as editable chips. System tags render as read-only chips
  with distinct visual treatment.
- Row affordances allow adding/removing manual tags. Creating a tag can be an
  inline "Create tag" action in the tag picker.
- Tag filter chips live with other filter chips. A selected tag filter matches
  agents with that manual or system tag.
- `Group by tag` appears only when the client/server protocol supports
  `AgentGroupMode::Tag`.
- Tag management allows create, rename, color change, and delete. Delete copy
  must say that agents and sessions are not deleted.

### 7.4 Pins UX

- A pin toggle appears on each row/card.
- Pinned rows appear in a top `Pinned` section if they also match the current
  Smart View/filter/search.
- Pin/unpin uses the same optimistic-overlay discipline: immediate UI feedback,
  then server snapshot reconciliation.

### 7.5 Sidebar parity

The sidebar may keep a reduced toolbar, but it must read the same server-owned
preferences, Smart View active query, tag assignments, and pins. It must not keep
an independent durable filter map or pinned/tagged state. Parent/child sub-agent
nesting remains an inner display rule within whatever projection the server
state produces.

---

## 8. Phase 1a status and retired Phase 1b

### 8.1 Phase 1a is landed

Phase 1a delivered persistent Agents-view preferences and the flicker fix. Future
work starts from the landed protocol/store/host/frontend model cited in §3.

No Phase 1a redesign is in scope for Smart Views, tags, or pins. Only additive
changes are allowed.

### 8.2 The old Phase 1b is retired

The previous Phase 1b plan for virtual default groups and
`AgentOrganizationSnapshot` is retired with the folder design. Do not add
folder containers, virtual folder/project placement records, folder group modes,
or folder drag/drop seams.

Project and no-project grouping already exists as `AgentGroupMode::Project` in
Pillar A. Future project/system organization should be represented through
Smart View filters and system tags, not through placement snapshots.

---

## 9. Phase 2a: Smart Views

Phase 2a is the highest-priority follow-up because it is closest to the landed
preferences model. It is independently shippable and does not require tags or
pins.

### 9.1 Phase 2a protocol additions

Extend the existing snapshot and notify:

```rust
pub struct AgentsViewPreferencesSnapshot {
    pub preferences: AgentsViewPreferences,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<AgentsViewPreferencesStoreError>,
    #[serde(default)]
    pub smart_views: AgentsSmartViewsSnapshot,
}

pub struct AgentsSmartViewsSnapshot {
    #[serde(default)]
    pub built_in_views: Vec<SmartView>,
    #[serde(default)]
    pub user_views: Vec<SmartView>,
    pub active_view_id: SmartViewId,
}
```

Add one input frame that mirrors `SetAgentsViewPreferences` rather than one frame
per operation:

```rust
FrameKind::SetAgentsSmartViews

pub struct SetAgentsSmartViewsPayload {
    pub update: AgentsSmartViewsUpdate,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentsSmartViewsUpdate {
    SaveCurrent {
        name: String,
        view: SmartViewQuery,
        set_active: bool,
    },
    Rename {
        id: UserSmartViewId,
        name: String,
    },
    Update {
        id: UserSmartViewId,
        view: SmartViewQuery,
    },
    Delete {
        id: UserSmartViewId,
    },
    Reorder {
        user_view_ids: Vec<UserSmartViewId>,
    },
    SetActive {
        id: SmartViewId,
    },
}

pub struct SmartViewQuery {
    pub filters: AgentsViewFilters,
    pub sort_mode: AgentSortMode,
    pub group_mode: AgentGroupMode,
    pub hide_finished: bool,
}
```

`SetActive` is a server-side compound mutation: set the active view id and copy
that view's query into active `AgentsViewPreferences`. The emitted
`AgentsViewPreferencesNotify` contains the updated active preferences and the
updated Smart View snapshot.

Protocol version handling:

- Additive snapshot fields must use `#[serde(default)]` for synthetic test
  builders and staged implementation.
- `FrameKind::SetAgentsSmartViews` requires a `PROTOCOL_VERSION` bump if Phase
  2a ships separately from later work.
- Keep `HostBootstrapPayload.agents_view_preferences` as the one optional
  primary-local-host bootstrap field. Remote hosts still emit `None`.

### 9.2 Phase 2a server changes

- Extend `AgentsViewPreferencesStore` or a same-owner sibling store to persist:
  - ordered user Smart Views,
  - active Smart View id,
  - existing active `AgentsViewPreferences`.
- Generate built-in Smart Views for every snapshot; do not persist built-ins.
- Validate user Smart View names as non-empty after trim.
- Validate user view ids are unique and stable.
- Validate `Reorder` contains every user view id exactly once and no built-in ids.
- Reject rename/update/delete/reorder operations against built-ins.
- On `SetActive`, copy only query fields into active preferences and emit one
  full snapshot.
- Preserve the Phase 1a no-panic corrupt-store behavior: host registration
  succeeds with defaults plus typed load error, and the next valid mutation
  rewrites a clean store.
- Fan out the existing `AgentsViewPreferencesNotify` from the primary local host
  only, using the single-owner subscriber pattern already used by Phase 1a.
- Add router and validator cases for `SetAgentsSmartViews` on the host stream.

### 9.3 Phase 2a frontend changes

- Generate bindings for `SmartView*`, `AgentsSmartViewsSnapshot`, and
  `SetAgentsSmartViewsPayload` from protocol.
- Extend `AppState` to hold the server-emitted Smart View snapshot as part of
  the existing `agents_view_preferences` snapshot, plus a non-persisted Smart
  View overlay if needed for instant view switching/manage actions.
- Reuse the existing preference overlay when selecting a Smart View: overlay
  `filters`, `sort_mode`, `group_mode`, and `hide_finished` immediately, send
  `SetAgentsSmartViews::SetActive`, then drop overlays on the next full snapshot.
- Add a view switcher above the Agents Center toolbar.
- Add "Save current view as…". It must capture effective preferences but never
  include search, density, or manual order.
- Add manage actions for rename/update/delete/reorder user views.
- Render built-ins as non-editable and non-deletable.
- Keep the UI reactive: keyed rows/switcher entries pass stable ids and look up
  current records from signals rather than snapshotting records in closures.
- Do not create any durable frontend-local Smart View source of truth.

### 9.4 Phase 2a testing

Native client-level tests must use the public client against a real server with
mock backend and assert observable protocol events/responses.

Add or extend tests for:

1. Bootstrap:
   - connect to the primary host,
   - assert `HostBootstrap.agents_view_preferences.smart_views` contains the
     built-ins and active `All`,
   - assert a non-primary/remote host emits `None` and cannot mutate Smart Views.
2. Save current view:
   - set non-default filters/sort/group/hide-finished,
   - send `SetAgentsSmartViews::SaveCurrent`,
   - observe `AgentsViewPreferencesNotify`,
   - reconnect and assert the user view persists.
3. Set active:
   - create a user view,
   - send `SetActive`,
   - assert the notify updates both active id and active preferences in one
     snapshot.
4. Manage lifecycle:
   - rename, update, reorder, and delete a user view,
   - assert each notify has the expected ordered user view list.
5. Validation errors:
   - reject empty names,
   - reject built-in rename/delete/reorder,
   - reject reorder with missing/duplicate ids,
   - surface errors as observable protocol/command errors.
6. Corrupt store:
   - write invalid Smart View store data,
   - connect successfully,
   - assert defaults plus typed load error,
   - send a valid mutation and assert the load error clears.

Wasm/component tests:

- View switcher renders built-ins and user views from server snapshot.
- Selecting a view updates rows immediately via the preference overlay and emits
  the correct typed frame.
- Search text is not included in a saved Smart View payload.
- Rename/delete/reorder user views update only via overlays and server snapshots;
  no durable local view list is mutated.
- A server notify with canonicalized/different query values drops the overlay and
  the DOM reflects the server snapshot.

---

## 10. Phase 2b: Tags and Pins

Phase 2b is independently shippable after Phase 2a. It adds manual tags,
server-derived system tags, tag filtering/grouping, and pins.

### 10.1 Phase 2b protocol additions

Extend filters and grouping:

```rust
pub struct AgentsViewFilters {
    // existing fields...
    #[serde(default)]
    pub tags: Vec<AgentTagRef>,
}

pub enum AgentGroupMode {
    Flat,
    Status,
    Backend,
    Project,
    Tag,
}
```

Extend the full snapshot:

```rust
pub struct AgentsViewPreferencesSnapshot {
    pub preferences: AgentsViewPreferences,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<AgentsViewPreferencesStoreError>,
    #[serde(default)]
    pub smart_views: AgentsSmartViewsSnapshot,
    #[serde(default)]
    pub tags: AgentTagsSnapshot,
    #[serde(default)]
    pub pins: AgentPinsSnapshot,
}
```

Add one input frame for tags:

```rust
FrameKind::SetAgentTags

pub struct SetAgentTagsPayload {
    pub update: AgentTagsUpdate,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentTagsUpdate {
    CreateTag {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<AgentTagColor>,
    },
    RenameTag {
        tag_id: AgentManualTagId,
        name: String,
    },
    SetTagColor {
        tag_id: AgentManualTagId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<AgentTagColor>,
    },
    DeleteTag {
        tag_id: AgentManualTagId,
    },
    AssignTag {
        target: AgentAnnotationTarget,
        tag_id: AgentManualTagId,
    },
    RemoveTag {
        target: AgentAnnotationTarget,
        tag_id: AgentManualTagId,
    },
}
```

Add one input frame for pins:

```rust
FrameKind::SetAgentPins

pub struct SetAgentPinsPayload {
    pub update: AgentPinsUpdate,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentPinsUpdate {
    Pin { target: AgentAnnotationTarget },
    Unpin { target: AgentAnnotationTarget },
}
```

Protocol version handling:

- `AgentsViewFilters.tags`, `AgentsViewPreferencesSnapshot.tags`, and
  `AgentsViewPreferencesSnapshot.pins` must be serde-defaulted.
- `AgentGroupMode::Tag` is a new enum variant and the tag/pin input frames are
  new frame kinds, so Phase 2b requires a `PROTOCOL_VERSION` bump if it ships
  after Phase 2a.
- If Phase 2a and 2b land in one implementation branch, one protocol bump is
  sufficient, but all additive fields still need defaults.

### 10.2 Phase 2b server changes

- Extend the primary-local-host store or add a sibling same-owner store for:
  - manual tag definitions,
  - manual tag assignments,
  - pinned targets.
- Compute system tag descriptors and assignments from server state on every
  snapshot. Do not persist system tags.
- Use `HostFilterId` + `SessionId` for durable assignments/pins and
  `HostFilterId` + `AgentId` only as transient pre-session state.
- Promote transient manual tag assignments and pins when a session id resolves.
- Remove transient assignments/pins when a sessionless live agent closes.
- Remove session assignments/pins when a session is deleted.
- Delete-tag must strip the tag from all assignments and emit a full snapshot;
  agents/sessions remain untouched.
- Project/workflow/backend/origin changes recompute system tags and emit a full
  snapshot when visible tags change.
- Validate:
  - non-empty tag names,
  - unique manual tag names if the UX requires uniqueness,
  - valid color strings,
  - known manual tag ids for assign/remove/rename/delete,
  - non-empty target ids,
  - no manual mutation of system tags,
  - primary-local-host ownership.
- Canonicalize filters including the new tag filter dimension in the same store
  path that already canonicalizes hosts/projects/statuses/backends/origins.
- Fan out the existing full `AgentsViewPreferencesNotify` after every successful
  tag or pin mutation/cleanup.
- Add router and validator cases for `SetAgentTags` and `SetAgentPins`.

### 10.3 Phase 2b frontend changes

- Generate protocol bindings for tag/pin types and the extended filters/grouping.
- Extend effective preferences to include `filters.tags` and `AgentGroupMode::Tag`.
- Add a tag/pin optimistic overlay, or extend the existing overlay, but keep it
  non-persisted and drop it on any full authoritative snapshot.
- Render manual and system tag chips on rows with distinct classes.
- Add a tag picker for assigning/removing manual tags.
- Add tag management UI for create/rename/color/delete.
- Add tag filter chips and `Group by tag` controls.
- Add pin toggles on rows and a top `Pinned` section.
- Ensure pinned rows and tag groups are deterministic functions of server
  snapshot + overlay + ephemeral search; no local durable maps.
- Update sidebar rendering to show tag chips and pin state from the same server
  snapshot. The sidebar may omit management controls if the Agents Center owns
  the full management UX.

### 10.4 Phase 2b testing

Native client-level tests:

1. Manual tag lifecycle:
   - create a tag,
   - observe full `AgentsViewPreferencesNotify`,
   - rename and recolor it,
   - delete it and assert assignments are stripped without deleting sessions.
2. Session-keyed assignment:
   - spawn an agent through the mock backend until a session id is known,
   - assign a manual tag,
   - close and resume/list the session,
   - assert the assignment persists by session target.
3. Pre-session transient promotion:
   - assign a tag before session id resolution,
   - resolve the session,
   - assert the notify promotes the assignment to the session target and removes
     the transient target.
4. Sessionless close cleanup:
   - assign a tag or pin to a sessionless live agent,
   - close before session resolution,
   - assert transient state disappears and no persisted assignment remains.
5. System tags:
   - create agents with backend/origin/workflow/project attributes,
   - assert bootstrap/notify includes the expected system tag descriptors and
     assignments,
   - assert no system tag records are written as manual definitions.
6. Tag filter and group:
   - set `AgentsViewFilters.tags`,
   - observe filtered rows through public client events or component-visible
     state,
   - set `AgentGroupMode::Tag` and assert grouped projection behavior.
7. Pins:
   - pin/unpin a session-backed agent,
   - reconnect/resume and assert the pin persists,
   - assert pins float to the top while still respecting active filters/search.
8. Validation errors:
   - unknown tag id,
   - system tag mutation,
   - duplicate/empty invalid reorder-like payloads if added later,
   - non-primary host mutation,
   - invalid target ids.

Wasm/component tests:

- Manual and system tag chips render with distinct styles from server snapshot.
- Tag picker assign/remove emits `SetAgentTags` and updates immediately via
  overlay without mutating durable local state.
- Tag filters and `Group by tag` react to effective preferences.
- Search remains ephemeral and does not alter persisted tag filters.
- Pin toggle emits `SetAgentPins`, immediately floats/unfloats the row via
  overlay, and reconciles on full notify.
- A notify whose server value differs from the optimistic tag/pin overlay drops
  the overlay and renders the server value.
- Sidebar rows show the same tag/pin state as the Agents Center.

---

## 11. Protocol and compatibility notes

- New frame kinds require a `PROTOCOL_VERSION` bump. The current constant is
  `protocol/src/types.rs:16`.
- Additive bootstrap/snapshot fields must use `#[serde(default)]`, even with a
  protocol bump, because tests and staged implementation frequently construct
  partial payloads.
- Do not reuse `SetSetting` / `HostSettings` for Agents view preferences,
  Smart Views, tags, or pins. This is client-global Agents-view state, not host
  runtime settings.
- Do not send Smart View, tag, or pin mutations over agent streams. They are
  primary-local-host-owned view/organization state, so frames route on the host
  stream like `SetAgentsViewPreferences`.
- Validator additions are required for every new host-stream frame.
- Frontend dispatch should treat `AgentsViewPreferencesNotify` as the full
  authoritative snapshot for preferences, Smart Views, tags, and pins. Apply it
  to state, drop overlays, and let reactive views render.
- Pending overlays are frontend interaction state, not protocol state. They must
  be non-persistent, domain-scoped, and reconciled by server notifies/bootstrap.
- Search is never persisted and must not appear in Smart View, tag, pin, or
  preference payloads.

---

## 12. Open risks

### 12.1 Auto-derived tag taxonomy

The first system-tag set needs product decisions: exact ids, labels, colors, and
sources for backend, origin, workflow, sub-agent, team/custom-agent, and project
tags. These ids must be stable enough to persist in `AgentsViewFilters.tags`.

### 12.2 Built-in Smart View semantics

The built-in labels are clear, but exact query semantics may need user approval:
which statuses count as `Active`, whether terminated includes all fatal rows, and
whether built-ins should use `Flat` or preserve the user's current group mode.

### 12.3 Tags and pins for agents that never get a session id

The decided rule is transient-by-agent until session resolution, then
session-keyed persistence. Agents that close without a session id lose transient
tag/pin state. That is consistent, but the UX should avoid implying such state
survives restart before a session exists.

### 12.4 Active Smart View and optimistic overlays

`SetActive` changes both active view id and active preferences. The frontend must
avoid two independent overlays fighting each other. Preferred behavior is one
view-selection overlay that sets the copied preference domains and pending active
view id, then drops both on the full snapshot.

### 12.5 Remote host target stability

Phase 1a drops unverifiable remote transient manual-order keys
(`server/src/host.rs:4487-4492`). Tags and pins should avoid the same ambiguity by
including `HostFilterId` in both session and transient targets. The remaining
risk is ensuring every connected host has a stable configured id before tag/pin
mutations are enabled.

### 12.6 Store boundaries and load errors

Smart Views can cleanly extend `AgentsViewPreferencesStore`. Tags and pins may be
large enough to justify a sibling store. If split, the protocol still needs one
merged full snapshot and typed per-domain load errors. The implementation must
not panic or silently discard one store's corrupt state.

### 12.7 Tag grouping duplication

Agents can have multiple manual and system tags. `Group by tag` duplicates rows
under multiple groups unless the UX chooses a primary-tag rule. Duplication is
more transparent, but it may surprise users and should be validated in UI tests.
