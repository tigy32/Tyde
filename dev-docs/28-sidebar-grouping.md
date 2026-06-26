# Sidebar Agent Grouping

Phase 1 keeps this feature intentionally small and frontend-focused. It does
not add frames, does not introduce an `AgentGroups` model, and does not bump the
protocol version.

## Phase 1 behavior

The right sidebar Agents panel now renders live agents as:

```text
Host
  Project
    Parent agent
      Sub-agent
```

Host headers use configured-host labels, falling back to the host id. Project
headers use server-emitted project names, falling back to the project id; agents
without a project render under `No project`. Parent/child sub-agent nesting is
preserved inside each host/project leaf. If the active sidebar scope hides other
projects, the tree simply contains the active project's subtree.

## Sidebar toggles

Kept:

- Search: ephemeral text narrowing.
- Hide inactive: hides idle agents while keeping initializing, streaming, or
  turn-active agents visible.
- Hide sub-agents: removes child rows before grouping.
- Show other projects: controls whether projects outside the active project are
  included in the Host → Project tree. Per-project in-session toggle memory is
  unchanged.

Removed:

- Hide finished.

There is no durable "finished" lifecycle for normal agents: `AgentClosed`
removes them from frontend state. Fatal/terminated rows are still real visible
state and can be filtered explicitly in Agents Center by status. The old
`hide_finished` preference and Smart View field remain serialized for protocol
20 compatibility, but current UI no longer exposes or applies them.

## Agents Center change

Agents Center also no longer renders Hide finished and no longer applies
`hide_finished` when filtering rows. Smart Views may still contain the field from
older persisted data, but it is ignored by the frontend projection.

## Phase 2 plan

A later phase can add server-owned custom groups:

- Add an `AgentGroups` model owned by the server.
- Add a typed `SetAgentGroups` frame with a protocol bump.
- Support drag and keyboard grouping in the UI.
- Enforce single group membership per agent.
- Moving a parent moves its children with it.
- Empty custom groups auto-delete.

Phase 2 must keep the server as the source of truth: the frontend may use only
short-lived interaction state or optimistic overlays and must reconcile from
server snapshots.
