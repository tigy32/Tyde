# Sidebar Agent Grouping

Phase 1 changed the right sidebar from a flat list into the default live-agent
tree:

```text
Host
  Project
    Parent agent
      Sub-agent
```

Phase 2 adds dedicated, server-owned custom groups above that default tree. The
server remains the source of truth; the sidebar renders the latest
`AgentsViewPreferencesSnapshot.groups` state and sends typed mutations to the
primary local host.

## Default tree

The default tree is still Host → Project → parent/child. Host headers use
configured-host labels, falling back to the host id. Project headers use
server-emitted project names, falling back to the project id; agents without a
project render under `No project`.

Custom group membership overrides default placement. A grouped agent appears in
its custom group only and is not duplicated under Host → Project. Ungrouped
agents continue to render in the default tree. Parent/child nesting is preserved
inside both custom groups and default project leaves whenever the parent and
child are visible in the same projection.

## Sidebar toggles

Kept:

- Search: ephemeral text narrowing.
- Hide inactive: hides idle agents while keeping initializing, streaming, or
  turn-active agents visible.
- Hide sub-agents: removes child rows before grouping.
- Show other projects: controls whether projects outside the active project are
  included. Per-project in-session toggle memory is unchanged.

Removed:

- Hide finished.

There is no durable "finished" lifecycle for normal agents: `AgentClosed`
removes them from frontend state. Fatal/terminated rows are still real visible
state and can be filtered explicitly in Agents Center by status. The old
`hide_finished` preference and Smart View field remain serialized for
compatibility, but current UI no longer exposes or applies them.

## Phase 2 protocol model

Protocol 21 adds custom groups to the existing Agents-view snapshot/fanout path:

- `AgentGroupId(String)`
- `AgentGroup { id, name }`
- `AgentGroupAssignment { group_id, target: AgentAnnotationTarget }`
- `AgentGroupsSnapshot { groups, assignments }`
- `AgentsViewPreferencesSnapshot.groups: AgentGroupsSnapshot` with
  `#[serde(default)]`
- `FrameKind::SetAgentGroups`
- `SetAgentGroupsPayload { update: AgentGroupsUpdate }`

`AgentGroupsUpdate` variants:

- `CreateGroup { name, targets }`
- `RenameGroup { id, name }`
- `DeleteGroup { id }`
- `MoveTargets { group_id: Option<AgentGroupId>, targets }`

`group_id: None` means ungroup. Assignments reuse the same
`AgentAnnotationTarget::{Session, TransientAgent}` shape as tags and pins, so
session-backed assignments persist while pre-session live agents use transient
targets until promotion.

## Server behavior

The primary local host owns groups in the existing
`AgentsViewPreferencesStore`. Remote/non-primary hosts emit no preferences
snapshot and reject `SetAgentGroups`, matching preferences, Smart Views, tags,
and pins.

Rules enforced by the store/host path:

- Single membership: assigning a target to a group removes any prior group
  assignment for that target.
- Moving a parent expands the move to its live descendant sub-agents before
  persistence.
- Empty groups auto-delete after moves, ungrouping, cleanup, or canonicalization.
- Delete-group removes the group and its assignments only; agents/sessions stay
  alive and return to default Host → Project placement.
- Transient targets are promoted to session targets when a session id resolves.
- Sessionless transient assignments are removed on agent close.
- Session assignments are removed on session delete.
- Every successful mutation/cleanup fans out a full
  `AgentsViewPreferencesNotify` snapshot.

Group ids are generated server-side from the initial group name and remain stable
across renames.

## Frontend behavior

The sidebar computes its projection from:

```text
filtered live agents + server snapshot groups
```

Filters apply before grouping. A custom group renders only members that pass the
active sidebar search/toggles; if none of its members pass, the group is hidden
for that projection. There is no durable frontend group map.

Drag-and-drop behavior:

- Drag an agent onto another ungrouped agent: create a new custom group
  containing both. The UI chooses an automatic name such as
  `<AgentA> + <AgentB>` and opens the new group's inline rename field after the
  authoritative snapshot arrives.
- Drag an agent onto a group header/body: move it to that group.
- Drag an agent from one group to another: move it; single membership is enforced
  by the server.
- Drag a grouped agent onto the default tree or the explicit `Ungroup` target:
  remove its group assignment.
- Drop onto an agent that already belongs to a group: move the dragged agent into
  that existing group; it does not create nested groups.

Keyboard fallback:

- Each agent has a move handle.
- Space/Enter picks up the focused agent from the handle.
- Tab, ArrowDown/ArrowRight, and ArrowUp/ArrowLeft move through group, agent, and
  Ungroup drop targets.
- Space/Enter on a target drops the picked-up agent.
- Escape cancels the move.
- The UI uses `aria-grabbed`, `aria-dropeffect`, a live status region, and visible
  focus/drop outlines.

Project and custom-group headings have a keyboard-accessible chevron disclosure
and a count of agents matching the current filters. Collapsing removes the
member rows while leaving the heading and group actions available, including
group drop targets. Choices are saved in local browser storage by stable group
or host/project identity, independent of the active project. They survive panel
remounts, app reloads, filtering, and group renames. Switching to a view whose
agents are all collapsed preserves that choice; visible headings let the user
expand them explicitly. This is a device-local presentation preference, not a
server-owned membership or filtering preference.

Group headers support inline rename and delete. Delete ungroups members and does
not close or kill agents. The sidebar does not use `window.confirm`,
`window.alert`, or `window.prompt`.

## Agents Center note

Agents Center still no longer renders Hide finished and no longer applies
`hide_finished` when filtering rows. Smart Views may still contain the field from
older persisted data, but it is ignored by the frontend projection.

## Desktop presentation modes and layout

The existing Agents panel is shared by **Vibe** (Agents and History) and
**Dinosaur** (the existing developer and advanced panels too). The top chrome
selects the mode and exposes Settings and the existing Feedback flow. Neither
mode changes chat tabs, the composer, transcripts, or server subscriptions.

Sidebar tab headers can move between left and right, or reorder within a side.
Their tab menu offers the same operations; Ctrl+Shift+Left/Right moves sides
and Ctrl+Shift+Up/Down reorders. Escape or an abandoned drag makes no change.
An empty side retains its tab-header drop target. This is separate from agent
row grouping, project ordering, and center/bottom docking.

`sidebar_layout::ClientLayouts` owns presentation. The versioned client-local
`tyde.desktop-layout.v1` record stores the last mode and independent tab order,
side, active tab, visibility, and preferred width for each mode. The known legacy
`tyde.agents.collapsed-folders.v1` record is explicitly parsed as its original
set of folder keys. Those clients retain the previous fixed pane arrangement
as Dinosaur (Files left, Agents right, both 320px); the folder record stays
on its existing canonical path. Clients with neither record start in Vibe.
Project/session history and unrelated preferences never determine migration.
Once v1 exists, its explicit last mode wins. Invalid/unknown records
produce a visible error and block writes rather than guessing a replacement.
Settings → Appearance → Reset sidebar layout resets the current mode and
unblocks persistence. Server-owned agent filters, pins, project membership,
and selection keep their existing canonical paths. Persistent panel mounts
use CSS grid placement, not panel copies or remounts, when moving or hiding.

Agent rows use the existing vector backend glyphs, with status on the right.
The selected conversation has the strongest flat tint; related rows have a
quieter shared tint, without backend-colored rails or a group outline.
Subagents start collapsed. The count is the number of immediate children with
an authoritative active turn; expanding shows those children, with a separate
inactive disclosure. It never derives parentage or activity from transcripts.

A pencil immediately after a project heading's name opens the shared project
editor, also reachable through the project rail menu. Keyboard focus reveals
the pencil; touch keeps it visible. The editor uses existing rename and root
commands and the host folder browser, rendering roots from project events
without optimistic domain copies. Removing a source root removes its project
association, not its directory. Workbench/parent root restrictions still apply.

## Global pins and host order

The shared Agents panel has one **Pinned** section before every host, in both
modes. It projects the primary server's existing `AgentPinsSnapshot` onto all
currently known live agents using the full host-scoped annotation target. Each
pin shows its configured host label. Opening keeps the agent's host/project;
unpin mutations keep the host-scoped target and use the existing primary
preferences owner. There is no second pin store and no history-derived or
fabricated disconnected-host roster.

Explicit pins bypass automatic inactive, subagent and other-project hiding;
text search still narrows them. Unpinned descendants stay with their tree;
independently pinned children float out exactly once and return on unpin.
Pinned trees are absent from ordinary host/project/custom groups. Within each
remaining host, **No project** comes first, followed by the existing project
order, then custom groups. Empty pin/host/project/custom groups are omitted.
Existing vector backend identities, status glyphs, active/inactive child
revelation and project/root actions are unchanged.
If the canonical configured-host list cannot provide a pin's label, its badge
shows an explicit error linking to Settings → Hosts, not an opaque-ID substitute.
