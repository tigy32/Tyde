# Agent Control MCP

External MCP control surface for Tyde2 agents. This builds on:

- `01-philosophy.md`
- `02-protocol.md`
- `03-agents.md`
- `10-dev-instance-mcp.md`

---

## Problem

The old Tyde had a genuinely useful capability: an MCP client such as Codex or
Claude Code could ask Tyde to spawn other agents, wait for them, message them,
and cancel them.

That enabled workflows like:

- Codex spawning a Claude helper for a bounded subtask
- a parent agent fanning work out across multiple agents
- blocking until the first child finishes
- keeping Tyde as the owner of long-lived agent sessions instead of each tool
  caller inventing its own subprocess model

We want that capability in Tyde2, but we do **not** want to repeat the old
ownership mistakes.

---

## Legacy Reference

Relevant old-app references:

- `~/Tyde/dev-docs/agent-mcp.md`
- `~/Tyde/src-tauri/src/agent_mcp_http.rs`

The legacy server exposed these tools:

- `tyde_run_agent`
- `tyde_spawn_agent`
- `tyde_await_agent`
- `tyde_send_agent_message`
- `tyde_cancel_agent`
- `tyde_list_agents`

Those names were good. The main problem was where the server lived.

---

## What Went Wrong Before

The old app put the MCP server inside Tauri. That violated the rewrite
philosophy in the same way the old dev-instance MCP did.

### MCP Lived Inside The Product Shell

The shell owned:

- MCP tool definitions
- MCP transport lifetime
- agent wait logic
- dashboard/list state derivation
- app settings for enabling/disabling the server

That is not transport work. It is external integration logic.

### The Control Surface Bypassed The New Boundary We Actually Want

Tyde2 already has the correct internal boundary:

- `server` owns agent lifecycle
- `protocol` defines typed host/agent events
- `client` connects to that protocol

If agent control is rebuilt, it should sit **on top of** that boundary, not
next to it.

### Settings-Owned MCP Is The Wrong Shape

The old app persisted MCP enablement inside product settings. That made a
developer/integration surface into app runtime state.

Tyde2 should treat agent-control MCP the same way `10-dev-instance-mcp.md`
treats dev-instance MCP:

- external capability
- explicit endpoint configuration
- no product-owned MCP toggle

---

## Tyde2 Design

The correct shape is:

```text
MCP client
    |
    v
tyde-dev-driver agent-control
    |
    v
Tyde host protocol connection
    |
    v
tyde-server HostHandle
```

### Core Rule

**Tyde2 agent control MCP must be an external MCP server implemented in
`tyde-dev-driver`.**

The desktop app does **not** speak MCP.

The shell exposes a loopback host endpoint. The external driver connects using
the real Tyde wire protocol and derives all agent state from protocol events.

This exactly follows the rewrite philosophy:

- one source of truth: `protocol`
- server owns behavior
- shell stays transport-only
- MCP ends at the external boundary

---

## Ownership

### `server`

`server` continues to own:

- agent creation
- agent input routing
- agent output events
- canonical agent history

No MCP-specific logic belongs here.

### `frontend/tauri-shell`

The shell may own exactly one thing for this feature:

- a loopback host listener that forwards raw stream connections into
  `server::accept` and `server::run_connection`

That is transport work, so it belongs in the shell.

The shell must **not** own:

- MCP tools
- spawn/wait/list orchestration logic
- agent dashboard derivation
- app settings for this feature

### `tyde-dev-driver`

The driver owns:

- MCP stdio server lifecycle
- connection to the Tyde host endpoint
- state derivation from protocol events
- wait semantics (`run`, `await`, `list`)
- agent-control tool definitions

That is the correct place for an external integration surface.

---

## Child Endpoint

The desktop shell exposes a loopback TCP host endpoint when explicitly
configured.

First-slice env contract:

- `TYDE_AGENT_CONTROL_HOST_BIND_ADDR=127.0.0.1:<port>`

Compatibility alias:

- `TYDE_DEV_HOST_BIND_ADDR`

Rules:

- only loopback addresses are allowed
- no product settings are persisted
- the shell only forwards Tyde protocol frames

This is intentionally the same boundary the dev-instance driver uses in
`10-dev-instance-mcp.md`.

---

## MCP Surface

The first slice keeps the legacy tool names because they were good:

- `tyde_run_agent`
- `tyde_spawn_agent`
- `tyde_await_agent`
- `tyde_send_agent_message`
- `tyde_cancel_agent`
- `tyde_list_agents`

### Input Shape

`tyde_spawn_agent` and `tyde_run_agent` should accept:

- `workspace_roots`
- `prompt`
- `backend_kind?`
- `parent_agent_id?`
- `project_id?`
- `name?`
- `cost_hint?`

`backend_kind` is optional only if the connected host has an explicit
`default_backend` in `HostSettings`.

### Output Shape

The old surface returned `status`, `message`, `error`, and summary-like data.
That is still useful, but Tyde2 has one important semantic difference:

- agents are long-lived and reusable
- a completed turn does **not** mean the agent is gone

So the driver derives tool-level status from the latest protocol events:

- `running`: currently in an active streamed turn
- `completed`: most recent turn reached `StreamEnd`; agent is idle and can take
  more input
- `cancelled`: latest observed turn emitted `OperationCancelled`
- `failed`: latest fatal `AgentError`

That preserves the old workflow semantics without pretending agents are
one-shot jobs.

---

## State Derivation

The driver derives its state from:

- `HostSettings`
- `NewAgent`
- `AgentStart`
- `ChatEvent`
- `AgentError`

It does **not** query Tauri internals or maintain an app-private RPC.

Per agent, the driver tracks:

- immutable metadata from `NewAgent`/`AgentStart`
- the instance stream for this connection
- current derived status
- last completed/cancelled message
- last error
- driver-side activity counters used for wait semantics

This state is driver-owned and derived. The source of truth remains the Tyde
protocol stream.

---

## Wait Semantics

`tyde_run_agent` and `tyde_await_agent` keep the legacy usability:

- block until one or more watched agents stop running
- reset the idle timeout whenever a watched agent shows new activity
- cap total wall time to avoid infinite blocking

Recommended defaults:

- idle timeout: `60_000ms`
- wall cap: `10 * idle_timeout`

`tyde_run_agent` is just:

1. spawn
2. wait for that agent's next non-running state
3. return the derived result

---

## Bootstrap Constraint

Today the host protocol does **not** emit an explicit "initial replay complete"
event for a newly connected subscriber.

So the first slice of the driver does this during startup:

1. connect to the host
2. require initial `HostSettings`
3. keep consuming replayed host/agent events until a short quiet window
4. mark the derived snapshot as bootstrapped

That bootstrap quiet window is a driver concern only. It does not alter product
behavior or invent a parallel protocol.

Future improvement:

- add an explicit host replay-complete event so the driver can remove this
  startup quiescence rule

That would be cleaner, but it is not required for the first useful slice.

---

## Non-Goals For This Slice

The first implementation does **not** try to rebuild every legacy integration
feature.

Specifically out of scope:

- in-app MCP HTTP server
- persisted UI toggles for this feature
- tool-policy enforcement in the MCP server
- automatic MCP injection into spawned backend sessions
- remote control / SSH tunneling

Those can come later, but they are separate features.

The first slice is:

- external MCP server
- explicit loopback host endpoint
- real host protocol connection
- useful spawn/run/await/message/cancel/list tools

---

## Future Work

Once the external shape is stable, the next useful additions are:

1. Explicit replay-complete host event.
2. Host-owned startup MCP configuration so spawned Tyde agents can themselves
   receive the agent-control MCP surface.
3. Remote launch/connection support in the driver only.

That sequence preserves the right architecture while still recovering the
powerful workflows the old app enabled.

---

## Summary

The old agent MCP server had the right user-facing behavior and the wrong
ownership model.

Tyde2 should rebuild it like this:

- external MCP server: `tyde-dev-driver agent-control`
- child app exposes only a loopback Tyde host endpoint
- all state derived from the real protocol
- no MCP inside the shell
- no persisted product settings for the MCP server

That keeps the workflow power while staying aligned with the rewrite
philosophy.
