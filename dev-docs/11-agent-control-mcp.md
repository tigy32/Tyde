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

### Settings-Owned MCP Is The Wrong Shape (Legacy)

The old app persisted MCP enablement inside product settings. That was wrong
**for the old app** because it made a developer/integration surface into app
runtime state.

For Tyde2, agent control is a first-class host capability, not a developer
tool. The host **should** have a setting for it because it controls whether
spawned agents can orchestrate other agents.

---

## Tyde2 Design

Agent control MCP follows the same pattern as debug MCP (`10-dev-instance-mcp.md`):
an embedded loopback HTTP MCP server inside `tyde-server`.

```text
Tyde agent (backend process)
    |
    | startup MCP injection
    v
tyde-server agent-control MCP (loopback HTTP)
    |
    | direct access
    v
tyde-server HostHandle (agent lifecycle)
```

### Core Rule

**Tyde2 agent control MCP is an embedded loopback HTTP MCP server inside
`tyde-server`, injected into agents as a startup MCP server.**

The desktop app does **not** speak MCP externally. The server owns the MCP
boundary internally and injects the loopback HTTP URL into agent spawn configs
when the setting is enabled.

This exactly follows the rewrite philosophy:

- one source of truth: `protocol`
- server owns behavior
- shell stays transport-only
- MCP ends at the server boundary

And matches the proven debug MCP pattern:

- server starts an HTTP MCP listener on `127.0.0.1:0`
- when spawning an agent, if `tyde_agent_control_mcp_enabled` is true, the
  server adds the agent-control MCP URL to `startup_mcp_servers`
- the agent discovers the MCP surface automatically

---

## Ownership

### `server`

`server` owns everything:

- the loopback HTTP MCP server (`agent_control_mcp.rs`)
- MCP tool definitions
- agent creation, input routing, output events
- canonical agent history
- wait/block semantics
- state derivation from its own agent records

This is simpler than the old external-driver design because the server already
has direct access to all agent state. No protocol connection bootstrapping, no
snapshot derivation from events, no external process coordination.

### `protocol`

`protocol` owns:

- `HostSettings.tyde_agent_control_mcp_enabled`
- `HostSettingValue::TydeAgentControlMcpEnabled`

### `frontend/tauri-shell`

The shell owns nothing for this feature. No loopback endpoint, no MCP, no
settings beyond the existing `HostSettings` event rendering.

### `frontend`

The frontend adds a toggle for the setting in the settings panel, same pattern
as the debug MCP toggle.

---

## Setting

`HostSettings` gains a new field:

```rust
pub struct HostSettings {
    pub enabled_backends: Vec<BackendKind>,
    pub default_backend: Option<BackendKind>,
    pub tyde_debug_mcp_enabled: bool,
    pub tyde_agent_control_mcp_enabled: bool,  // new
}
```

Default: **`true`**.

Agent control is a core host capability. Agents should be able to orchestrate
other agents by default. Users can disable it if they want to restrict that.

The setting is toggled via `SetSetting` with a new `HostSettingValue` variant:

```rust
TydeAgentControlMcpEnabled { enabled: bool }
```

---

## MCP Surface

The first slice keeps the legacy tool names because they were good:

- `tyde_run_agent`
- `tyde_spawn_agent`
- `tyde_send_agent_message`
- `tyde_cancel_agent`
- `tyde_list_agents`

### Input Shape

`tyde_spawn_agent` and `tyde_run_agent` should accept:

- `workspace_roots`
- `prompt`
- `backend_kind?`
- `project_id?`
- `name?`
- `cost_hint?`

`backend_kind` is optional only if the host has an explicit `default_backend`
in `HostSettings`.

`parent_agent_id` is not a tool argument. The server injects the loopback MCP
URL per agent as `/mcp?agent_id=<agent-id>`, and the HTTP handler infers the
caller from that request URL.

### Output Shape

Agents are long-lived and reusable. A completed turn does **not** mean the
agent is gone. The server derives tool-level status from its own agent records:

- `running`: currently in an active streamed turn
- `completed`: most recent turn reached `StreamEnd`; agent is idle and can take
  more input
- `cancelled`: latest observed turn emitted `OperationCancelled`
- `failed`: latest fatal `AgentError`

That preserves the old workflow semantics without pretending agents are
one-shot jobs.

---

## Implementation

### Server Structure

Create `server/src/agent_control_mcp.rs` following the same pattern as
`server/src/debug_mcp.rs`:

- `start_server()` returns an `AgentControlMcpHandle` with the HTTP URL
- loopback-only bind, reject non-loopback peers
- JSON-RPC 2.0 MCP protocol
- tool dispatch routes to agent lifecycle operations on the `HostHandle`

### Startup MCP Injection

In `startup_mcp_servers_for_settings()`, add the agent-control MCP URL when
`settings.tyde_agent_control_mcp_enabled` is true:

```rust
if settings.tyde_agent_control_mcp_enabled {
    servers.push(StartupMcpServer {
        name: "tyde-agent-control".to_string(),
        transport: StartupMcpTransport::Http {
            url: agent_control_mcp.url.clone(),
            headers: HashMap::new(),
            bearer_token_env_var: None,
        },
    });
}
```

### Auto-propagation

Child completion notices are **not** a separate MCP wait surface anymore.
Instead, the server auto-enqueues them onto the parent's queued-message state.

Rules:

- When a child reaches idle because `TypingStatusChanged(false)` arrives after
  a `StreamEnd`, and the child has `parent_agent_id`, the server formats a
  completion notice and enqueues it as a normal queued follow-up on the parent.
- When a child emits `OperationCancelled` or enters a fatal `AgentError`, the
  server enqueues the same notice shape with outcome `cancelled` or `failed`.
- Idle with no final output does not enqueue anything.
- If the parent is already idle, the actor still enqueues first and then
  immediately dispatches the queued message so the parent auto-resumes.
- Backend-native relay sub-agents use the host-owned
  `HostSubAgentEmitter::on_subagent_completed` callback to emit the same notice
  format into the parent's queue path.

Exact preamble:

```text
[TYDE CHILD AGENT UPDATE]
This is an automatic system-generated child completion notice, not a user instruction.
Child name: {child_name}
Child id: {child_id}
Child state: idle
Child outcome: {completed|cancelled|failed}

Child message:
{verbatim final message or synthesized status text}
[END TYDE CHILD AGENT UPDATE]
```

This keeps the queued-message model server-owned and avoids a separate
tool-level wait/control protocol for fan-out completions.

### `tyde_run_agent`

`tyde_run_agent` remains as the synchronous one-shot convenience:

1. spawn
2. wait for that agent's next non-running state
3. return the derived result

### Advantage Over External Driver

Because the server owns agent state directly, the implementation is simpler:

- no protocol connection bootstrapping
- no snapshot derivation from event streams
- no bootstrap quiescence window
- no external process lifecycle
- direct access to agent records, status, and history
- child completion propagation can use internal actor commands instead of
  reconstructing parent/child coordination in the MCP caller

---

## Non-Goals For This Slice

The first implementation does **not** try to rebuild every legacy integration
feature.

Specifically out of scope:

- persisted UI toggles beyond `HostSettings` (no separate config file)
- tool-policy enforcement in the MCP server
- automatic MCP injection into spawned backend sessions (agents spawned by
  agents — that is a future recursive capability)
- remote control / SSH tunneling

Those can come later, but they are separate features.

The first slice is:

- embedded loopback HTTP MCP server in `tyde-server`
- `HostSettings` toggle (default on)
- startup MCP injection into agents
- useful spawn/run/message/cancel/list tools

---

## Migration From `tyde-dev-driver agent-control`

The existing `dev-driver/src/agent_control.rs` implementation has the right
tool semantics and wait logic. The migration is:

1. Move the tool definitions and dispatch logic into
   `server/src/agent_control_mcp.rs`, adapting from protocol-event-derived
   state to direct server state access.
2. Replace the `client::Connection` + `SnapshotState` pattern with direct
   `HostHandle` calls for agent lifecycle.
3. Replace protocol-event-based child waiting with server-internal queued
   completion propagation plus the retained `tyde_run_agent` one-shot wait.
4. Remove the `agent-control` subcommand from `tyde-dev-driver`.
5. Remove the `TYDE_AGENT_CONTROL_HOST_BIND_ADDR` env var and related shell
   loopback endpoint code (if any was added for agent control specifically).

---

## Future Work

Once the embedded shape is stable, the next useful additions are:

1. Recursive agent control: agents spawned via agent-control MCP themselves
   receive the agent-control MCP surface.
2. Host-owned cost/concurrency limits for MCP-spawned agents.
3. Tool-policy enforcement (restrict which tools spawned agents can use).

---

## Summary

Agent control MCP follows the same pattern as debug MCP:

- embedded loopback HTTP MCP server in `tyde-server`
- injected into agents as a startup MCP server
- `HostSettings.tyde_agent_control_mcp_enabled` (default: true)
- no external process, no shell involvement, no separate protocol connection
- server has direct access to agent lifecycle — simpler implementation

That keeps the workflow power while staying aligned with the rewrite
philosophy and the proven debug MCP pattern.
