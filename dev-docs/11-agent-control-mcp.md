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
and inspect their output.

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

Some names remain useful, but the Tyde2 surface intentionally drops the
synchronous run/cancel tools and makes output reads explicit. The other main
problem was where the server lived.

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

The MCP surface is deliberately small. Output is never coupled to user-message
queues, list responses, spawn responses, or await responses. Normal callers
read the latest result with `tyde_read_agent`; diagnostics use the separate
incremental `tyde_read_agent_debug` event API.

Tools:

- `tyde_spawn_agent`
- `tyde_list_agents`
- `tyde_await_agents`
- `tyde_send_agent_message`
- `tyde_read_agent`
- `tyde_read_agent_debug`

There is no `run` convenience tool and no MCP `cancel` tool in this surface.
Clients compose the primitives explicitly: spawn, await status, then read output.

### Status Model

Tool-visible agent status is the protocol `AgentControlStatus` enum with exactly three values:

- `thinking`: the agent has not completed the current turn, or has not emitted
  its initial completion yet
- `idle`: the agent is available for more input
- `failed`: the agent reached a terminal failure

Statuses are metadata only. They must not carry final messages, summaries, or
error text.

### Tool Semantics

#### `tyde_spawn_agent`

Spawns an agent and returns immediately with metadata:

- `agent_id`
- `name`
- `status`

Input:

- `workspace_roots`
- `prompt`
- `backend_kind?`
- `parent_agent_id?`
- `project_id?`
- `name?`
- `cost_hint?`

`backend_kind` is optional only if the host has an explicit `default_backend` in
`HostSettings`. Each injected agent receives a server-signed bearer credential
bound to its `AgentId`; that authenticated identity becomes the parent. Header
and query identities are claims only and must match the credential. Root/admin
spawns use the separate typed host protocol/dev-driver surface, not bare MCP.

#### `tyde_list_agents`

Lists only agents whose server-owned `parent_agent_id` is the authenticated
calling agent id. It excludes grandchildren, unrelated host agents, and
children owned by other callers. Missing, invalid, or mismatched credentials
are rejected.
It returns metadata only:

- `agent_id`
- `name`
- `backend_kind`
- `origin`
- `status`
- `workspace_roots`
- `parent_agent_id`
- `project_id`
- `created_at_ms`

It must not include latest messages, errors, summaries, or output snippets.

#### `tyde_await_agents`

Waits like `select(2)` over the supplied agent ids. It returns when any watched
agent becomes non-`thinking`. It has no tool-level timeout and accepts neither
`timeout` nor `timeout_ms`. While every watched agent is still `thinking`, the
call remains pending unless the request is cancelled or the status channel
fails. Codex otherwise applies a 300-second default per-server MCP deadline.
Tyde therefore exposes only this tool on the separate `tyde-agent-await`
endpoint and gives only that MCP server a session-scale client horizon. The
normal `tyde-agent-control` tools retain Codex's ordinary deadline. The await
tool itself has no timer or retry loop; the long client horizon is necessary
because current Codex config has no representation for an unlimited timeout.

Input:

- `agent_ids`

Output:

- `ready`: watched agents whose status is `idle` or `failed`
- `still_thinking`: watched agents that are still `thinking`

It returns status only. Call `tyde_read_agent` to inspect output.

#### `tyde_send_agent_message`

Sends a follow-up message to an existing agent. This does not return agent
output.

Input:

- `agent_id`
- `message`

Await, read, debug-read, send, and list centrally authorize targets against the
server-owned direct-child relation. Knowing another agent id is insufficient.

#### `tyde_read_agent`

Reads exactly one latest output record from one agent. The result is one of:

- `message`, containing only assistant-visible text
- `error`, containing the typed `AgentErrorPayload`
- `empty`, when there is no output record or the latest assistant message has
  no visible text

The read never falls back to an earlier message. Reasoning, tool calls,
metadata, and prior output are not returned.

Input:

- `agent_id`

Output:

- `agent_id`
- `output`

#### `tyde_read_agent_debug`

Preserves the detailed incremental event API for diagnostics.

Input:

- `agent_id`
- `after_seq?`
- `limit?`
- `max_bytes?`

Output:

- `agent_id`
- `events`
- `next_after_seq`
- `max_bytes`
- `omitted_events`
- `omitted_event_bytes`

The event stream uses protocol `Envelope` values directly. Readable output is
limited to agent output events, currently `ChatEvent` and `AgentError` frames.

### No Child-Completion Queue Injection

Child completion notices must not be auto-enqueued onto a parent as synthetic
follow-up/user messages. That was the coupling that made queued user messages and
child-agent output interleave unpredictably.

Parent agents that need child results should use the explicit MCP flow:

1. `tyde_spawn_agent`
2. `tyde_await_agents`
3. `tyde_read_agent`
4. `tyde_send_agent_message` if the parent wants to incorporate the result into
   a later turn

The server still owns all agent state and all output events; it just no longer
converts child output into hidden parent input.

## Implementation

### Server Structure

`server/src/agent_control_mcp.rs` follows the same embedded-loopback pattern as
`server/src/debug_mcp.rs`:

- `start_server()` returns normal-control and await-only HTTP URLs plus a
  private credential authority
- loopback-only bind, reject non-loopback peers
- MCP tool dispatch routes to `HostHandle` / agent actor operations
- tool responses use protocol types directly where protocol events are returned

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
    servers.push(StartupMcpServer {
        name: "tyde-agent-await".to_string(),
        transport: StartupMcpTransport::Http {
            url: agent_control_mcp.await_url.clone(),
            headers: HashMap::new(),
            bearer_token_env_var: None,
        },
    });
}
```

When the registry assigns the new `AgentId`, it injects the corresponding
signed bearer credential into both HTTP transports. The credential is not
derived from or replaceable by an `agent_id` query/header value.

### Agent Output Storage

Agent actors own a typed `AgentControlLatestOutput` record and update it by
observing output events in original source order. They never reverse-scan or
fall back through replay history. `AgentBootstrapPayload` carries this typed
record explicitly, so reconnect history cannot overwrite a newer error with an
older message. Production and dev-driver projection, debug result shape, and
byte-capping logic all come from protocol shared code.

### Advantage Over External Driver

Because the server owns agent state directly, the embedded implementation stays
simple:

- no protocol connection bootstrapping for the production MCP surface
- no snapshot derivation from client-side event streams
- no bootstrap quiescence window
- direct access to agent records, status, and actor-owned event history
- no synthetic parent queued-message path for child output

The dev-driver connection is the explicit host/admin surface: it uses Tyde's
typed host protocol rather than the caller-authenticated MCP endpoint. Its
stdio tool server scopes targets it creates; in-process test handles may opt
into the host/admin view explicitly.

## Non-Goals For This Slice

This implementation does **not** try to rebuild every legacy integration
feature.

Specifically out of scope:

- persisted UI toggles beyond `HostSettings` (no separate config file)
- tool-policy enforcement in the MCP server
- remote control / SSH tunneling
- synchronous spawn-and-read convenience tools
- MCP cancellation tools

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
- explicit await/read flow; no child-output injection into parent user queues

That keeps the workflow power while staying aligned with the rewrite
philosophy and the proven debug MCP pattern.
