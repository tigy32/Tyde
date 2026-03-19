# Tyde Agent MCP HTTP Server

Tyde exposes runtime agent control through an in-app MCP server over loopback HTTP.

## Enable / Disable

- The server is enabled by default.
- You can toggle it in Tyde Settings -> Agent Control -> Enable Loopback MCP Control.
- The toggle persists in `~/.tyde/app-settings.json` (`mcp_http_enabled`).

## Endpoint

The desktop app starts a Streamable HTTP MCP endpoint during startup and exports:

- `TYDE_AGENT_MCP_HTTP_URL`, for example `http://127.0.0.1:47771/mcp`

Binding rules:

- Default bind address: `127.0.0.1:47771`
- Override with `TYDE_AGENT_MCP_HTTP_BIND_ADDR`
- Non-loopback addresses are rejected and replaced with loopback
- If the requested port is busy, Tyde falls back to an ephemeral loopback port

## MCP Tools

The API is push-oriented: tools block until results are ready rather than requiring
separate poll/collect steps. There are 6 tools.

### `tyde_run_agent` — spawn + block + result (most common)

Spawn an agent and block until it finishes or needs input. Returns the agent's final
message, status, and any error in a single call.

Input: `{ workspace_roots, prompt, backend_kind?, parent_agent_id?, name? }`
Output: `{ agent_id, status, message, error, summary }`

### `tyde_spawn_agent` — fire and forget

Spawn an agent and return immediately with its `agent_id`. Use this when launching
multiple agents in parallel, then wait for them with `tyde_await_agent`.

Input: `{ workspace_roots, prompt, backend_kind?, parent_agent_id?, keep_alive_without_tab?, name?, ephemeral? }`
Output: `{ agent_id, conversation_id }`

### `tyde_await_agent` — epoll-style multi-agent wait

Block until one or more agents become idle (completed, failed, needs_input, or
cancelled). Returns the idle agents with their messages and a list of still-running
agent IDs.

Input: `{ agent_ids?, timeout_ms? }`
Output: `{ ready: [{ agent_id, status, message, error, summary }], still_running: [id, ...] }`

If `agent_ids` is omitted, watches all non-terminal agents.

### `tyde_send_agent_message` — follow-up message

Send a follow-up message to an existing agent.

Input: `{ agent_id, message }`
Output: `{ ok: true }`

### `tyde_cancel_agent` — stop an agent

Interrupt a running agent and shut down its subprocess.

Input: `{ agent_id }`
Output: `{ agent_id, status, message, error, summary }`

### `tyde_list_agents` — dashboard view

List all agents with their current status, last message, and metadata.

Input: (none)
Output: `[{ agent_id, status, summary, last_message, ... }]`

## Typical Usage Patterns

### Simple: run one agent, get the result

```
tyde_run_agent({ workspace_roots: ["/project"], prompt: "Fix the test" })
→ { agent_id: 1, status: "completed", message: "I fixed the test by..." }
```

### Parallel: fan-out to multiple agents, wait for any

```
tyde_spawn_agent({ workspace_roots: ["/project"], prompt: "Fix auth.ts" })     → { agent_id: 1 }
tyde_spawn_agent({ workspace_roots: ["/project"], prompt: "Fix billing.ts" })  → { agent_id: 2 }
tyde_spawn_agent({ workspace_roots: ["/project"], prompt: "Fix users.ts" })    → { agent_id: 3 }

tyde_await_agent({ agent_ids: [1, 2, 3] })
→ { ready: [{ agent_id: 2, status: "completed", message: "..." }], still_running: [1, 3] }

tyde_await_agent({ agent_ids: [1, 3] })
→ { ready: [{ agent_id: 1, status: "completed", ... }, { agent_id: 3, status: "failed", ... }], still_running: [] }
```

## Timeout Behavior

`tyde_run_agent` and `tyde_await_agent` use an activity-aware idle timeout. The
deadline resets whenever any watched agent shows new activity. Total wall time is
capped at 10x the idle timeout to prevent infinite waits. Default idle timeout is 60s.

## Client Setup

Point your MCP-capable client (Codex/Claude Code) to the exported URL.

For Tyde UI/log debugging tools, see `dev-docs/debug-mcp.md`.
