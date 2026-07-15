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

---

## Tool Call Presentation

How these tool calls *render* is part of the contract, not a frontend detail.
Left untyped, every orchestration call arrives as `ToolRequestType::Other { args }`
and is dispatched to the generic renderer, which prints the MCP execution envelope
as raw JSON — call args and result — in every card. For the two tools whose payload
is human-meaningful, that was actively worse than showing nothing: a
`tyde_send_agent_message` card printed the sent message **twice**, escaped and
monospaced, and a `tyde_await_agents` card dumped JSON underneath a purpose-built
live card that was strictly more informative.

The fix is typed protocol variants, not a suppression flag in the UI. Because
`ToolRequestType` is matched exhaustively in the frontend, a typed variant simply
never reaches the generic renderer — the duplication is gone by construction, and
the compiler refuses to build until a renderer exists.

### Typed request / completion contract

`protocol/src/types.rs` owns these. They are the single source of truth; no
frontend parses tool arguments.

| Tool | `ToolRequestType` | `ToolExecutionResult` |
| --- | --- | --- |
| `tyde_send_agent_message` | `TydeSendAgentMessage { agent_id, message }` | `TydeSendAgentMessage` (unit) |
| `tyde_await_agents` | `TydeAwaitAgents { agent_ids }` | `TydeAwaitAgents { ready, still_thinking }` of `TydeAgentWaitStatus { agent_id, status }` |

The send completion is a **unit variant** on purpose: the MCP tool returns
`{"ok": true}` and nothing else, so there is no result body to render — the card's
header status carries the whole outcome. The await completion mirrors
`AwaitAgentsResult` exactly, which is status-only (see `tyde_await_agents` above).

Everything else — `tyde_spawn_agent`, `tyde_team_message_member`, `tyde_read_agent`,
`tyde_read_agent_debug`, `tyde_list_agents`, `tyde_list_launch_options`, and the
workflow tools — stays `Other`. For those, the structured result **is** the payload
the caller wants; there is no competing semantic card, and typing them would be a
broad change to generic tool rendering with no evidence behind it.

### Normalizer invariant: never a silent `Other`

The backend maps tool name + arguments into these typed variants (handling the bare,
`mcp__tyde-agent-control__…`, and `mcp__tyde-agent-await__…` spellings). If it sees a
canonical Tyde tool name and **cannot** parse its arguments, it must not quietly emit
`ToolRequestType::Other`. That is an invariant violation: it must log with the tool
name, `tool_call_id`, and the parse detail, and surface a visible error. The raw
payload may still be shown *because the failure is loud* — that is inspection, not a
fallback.

This state is near-unreachable by construction: the MCP inputs are
`#[serde(deny_unknown_fields)]` with required fields, ids are validated, and empty
messages are rejected before execution. A parse failure here means protocol drift,
which is exactly what should scream.

### Disclosure rule

`ToolOutputMode` is an existing, typed, user-driven control, so it carries this —
no new affordance, no new mental model.

| Mode | `tyde_send_agent_message` | `tyde_await_agents` |
| --- | --- | --- |
| Summary | Semantic only. Zero raw JSON. | Semantic only. Zero raw JSON. |
| Compact | Semantic only. Zero raw JSON. | Semantic only. Zero raw JSON. |
| Full | Semantic + a **closed** `Typed request` disclosure | Semantic only. **No raw, even in Full.** |

The `Full` disclosure is labeled **`Typed request`**, not "raw tool data", because
that is what it holds: the canonical typed request the server produced and the card
rendered. It is not the MCP envelope, and it must not claim to be — in the one case
where you would genuinely want the envelope (a normalization failure) the request has
already fallen back to `Other` and the generic renderer shows the real raw anyway.

Both surfaces honor these modes. Mobile is not exempt: a user who selects `Summary` to
quiet the conversation gets quiet on the phone too (see **Long content** below).

Await omits raw even in `Full` because the typed request and completion are
lossless with respect to the tool's semantics — the only residue in the raw
envelope is tycode transport metadata (`id`, `server`, `pluginId`, `durationMs`,
`appContext`), which belongs in logs, not in the conversation. Stated as a conscious
trade: `durationMs` leaves the UI for this tool.

**Errors are never hidden.** A `ToolExecutionResult::Error` short-circuits to the
error renderer before any of this, in every mode.

### The spawn prompt must stay visible

`SpawnAgentToolInput` carries `prompt` — the full task brief — and today it is
visible **only** in the raw args block. Any rule of the form "orchestration cards
suppress raw" therefore **deletes the spawn prompt from the default view**.

The rule above is keyed on **typed variant identity**, never on "is this an
agent-control card". That distinction is load-bearing: spawn stays `Other`, keeps
dispatching to the generic renderer, and keeps showing its prompt exactly as before
— zero diff on that path. A wasm test (`spawn_card_keeps_prompt_visible`) locks it.

If spawn is typed later, it needs its own semantic renderer that shows the prompt
as Markdown *before* it stops routing to the generic one. Do not "simplify" the rule
into a blanket suppression; that would silently delete real content.

### Rendering

- **Desktop** — `frontend/src/components/tool_card/tyde_send_agent_message.rs` and
  `tyde_await_agents.rs`, dispatched from `render_body`'s exhaustive match.
- **Mobile** — `mobile-frontend/src/components/tool_card.rs`. Mobile dispatches with
  `if let`, not an exhaustive `match`, so a typed variant with no mobile arm falls
  through **silently** to a Rust `Debug` dump. The compiler will not catch that. Any
  new typed orchestration variant must land its mobile arm in the same change.
- **Agent identity** — cards name a child agent by its live name from server-owned
  agent state, falling back to the raw id. Never an invented label; a bare uuid is
  unreadable, but a wrong name is worse.

### Markdown is an injection sink

The sent message is fed into `inner_html` on **both** surfaces, and it is not
necessarily authored by the agent you are talking to — agents routinely relay text they
did not write (a fetched page, a file's contents, another agent's output). Both
renderers therefore carry the same hardening contract, and any new consumer must use
one of them rather than rolling its own:

- raw block/inline HTML is downgraded to escaped text (so `<img src=x onerror=…>` and
  `<svg onload=…>` render as visible text, not as live markup with live handlers);
- link/image URLs are scheme-filtered to `http` / `https` / `mailto` / relative
  (so `javascript:` and `data:` cannot survive).

`frontend/src/markdown.rs` and `mobile-frontend/src/markdown.rs` share that contract.
They deliberately differ in *presentation* only — desktop adds syntect highlighting and
code-copy chrome; mobile keeps plain fences. Safety is shared; styling is not.

### The await card has exactly one agent roster

The live rows (from `AgentControlProgress`) name every watched agent with live status.
The completion renderer must **not** list them again: a second roster is the same
duplication the raw JSON was, just prettier. It contributes only what the live rows
cannot — they always render *now* — namely the wait's verdict at the moment it
returned: a single concise line of counts (`Wait returned · 2 ready · 1 still
thinking`), plus any agent that came back **failed**, named explicitly, since counts
alone would bury a failure and the live row beside it may since have changed.

Statuses in that verdict are the tool's own, rendered **verbatim** — never re-derived
from current agent state, which would silently rewrite history.

Mobile has no `ToolProgress` wiring, so on mobile the typed request/completion *is* the
roster. That is not a duplicate — it is the sole presentation there.

### Long content

A sent message can be thousands of characters, and it must never swallow the view.

- **Desktop** clamps the *rendered* container by `max-height` (never the Markdown
  source — truncating source breaks mid-fence and mid-table) with a `Show more` toggle.
  The overflow is measured from the DOM and **re-measured on resize** via a
  `ResizeObserver`: measured only at mount, a card narrowed afterwards (a dragged
  splitter, the 720px reflow) would clip content with no toggle to reveal it. Because
  `overflow: hidden` clips visually but leaves children in the tab order, focus inside
  the clipped region **expands** the card, so nothing is ever focused while invisible.
- **Mobile** bounds the body's height and makes it **scrollable** instead. Stated
  plainly as a deliberate divergence: the behavior differs, the guarantee does not —
  full content, always reachable, never dominating. A scroll container needs no
  measurement, no `ResizeObserver`, and can never leave a focusable child clipped out of
  sight. In `Summary`, mobile puts the message behind a **closed** disclosure so the
  mode does what it says.

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

---

## Workbench Tools

The control endpoint also exposes `tyde_list_workbenches` and
`tyde_create_workbench`. Both require an authenticated, active caller. Listing
is permitted for read-only callers; creation is mutating and is rejected for
`BackendAccessMode::ReadOnly`.

These tools are deliberately least-privilege scoped. The caller must be
assigned to a project. The server resolves that assignment to its canonical
standalone parent, and listing returns only that parent and its workbenches.
Creation requires that exact parent id; an unrelated project id is rejected.
The explicit id is retained as a confirmation guard rather than inferred from
ambient paths.

Creation accepts a branch, an optional non-blank display name, and optional
`base_ref`. The server resolves the base in every parent root before mutation,
passes only full commit SHAs to git, and reports each parent root, worktree
root, base SHA, and dirty-parent flag. Dirty parent content is never copied.

Agents enter a workbench by calling `tyde_spawn_agent` with its `project_id`.
The server derives authoritative roots from that project. Supplied roots must
match; missing/removing projects, mismatches, and a request with neither a
project nor roots fail before agent registration.
