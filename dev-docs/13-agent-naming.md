# Agent Naming

This document extends:

- `01-philosophy.md`
- `03-agents.md`
- `05-session-resume.md`

It specifies how Tyde2 should support user-controlled agent names and
server-generated names without exposing internal helper agents to the client.

---

## 1. Goals

We want three behaviors:

1. A user can set the name of any live agent.
2. That user-set name overrides any previously visible name.
3. If `spawn_agent` omits a name, the server generates a short 2-4 word name
   from the initial request by running an internal ephemeral agent.

The design must preserve the existing architectural rules:

- the agent actor owns live naming state, while the session store owns durable alias fields
- the frontend only renders server-emitted state
- there is one typed protocol model
- internal helper work must not leak into user-visible agent/session state

---

## 2. Non-Goals

- No frontend-generated fallback titles.
- No protocol-visible concept of an ephemeral helper agent.
- No rename history UI.
- No best-effort random fallback if automatic name generation fails.
- No backend-specific naming semantics visible to the client.

If automatic naming fails, `spawn_agent` fails visibly. The caller can retry
with an explicit name.

---

## 3. Current Problems

Today the stack has the wrong ownership boundary for naming:

- `SpawnAgentPayload.name` is required, so the client must invent a name before
  the server can create an agent.
- `dev-driver` currently fills missing names with a local
  `backend-kind + random suffix` fallback. That violates the rule that the
  server owns behavior.
- Live agent name state and persisted session naming are only loosely aligned.
- Session runtime updates currently overwrite `alias` from task titles, which
  would clobber any generated default name.
- There is no event for changing the name of an existing live agent.

The result is that naming policy is partly client-owned and partly accidental.

---

## 4. Naming Model

Tyde should treat agent naming as agent-owned live metadata with explicit
durable precedence in the session store.

### 4.1 Name Sources

There are only two user-visible naming fields in the durable model:

- `alias`: server-managed default display name
- `user_alias`: explicit user override

Their meanings become:

- `alias`
  - set by Tyde when it auto-generates a name for a new agent
  - may also be seeded from imported/backend metadata for old sessions
  - is never user-authored
- `user_alias`
  - set only by an explicit user action, including `spawn_agent { name: ... }`
  - always overrides `alias`

### 4.2 Effective Display Name

The effective display name is:

```rust
effective_name = user_alias.unwrap_or(alias)
```

For live agents, the agent actor owns the resolved effective name that the
frontend should render. For persisted sessions, the session store retains both
fields.

### 4.3 Overwrite Rules

- A user-set name always wins over the previous effective name.
- A generated default name is stable once chosen.
- Backend/task-derived titles may seed `alias` only when `alias` is not already
  set.
- Backend/task-derived titles must never overwrite `user_alias`.
- Backend/task-derived titles must never overwrite a Tyde-generated default
  `alias`.

This keeps the naming rule simple: explicit user names override everything;
generated names are durable defaults, not temporary placeholders.

---

## 5. Protocol Changes

The protocol must represent two things:

1. `spawn_agent` may omit a name
2. an existing live agent may be renamed

### 5.1 `SpawnAgentPayload`

Change:

```rust
pub struct SpawnAgentPayload {
    pub name: Option<String>,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub params: SpawnAgentParams,
}
```

Rules:

- `None` means "server, choose the default name".
- `Some(name)` means "user-specified name".
- `Some("")` and whitespace-only names are rejected.

`AgentStartPayload.name` and `NewAgentPayload.name` remain required `String`
fields because the live agent is never exposed before the effective name is
resolved.

### 5.2 New Frame Kinds

Add:

```rust
pub enum FrameKind {
    // input
    SetAgentName,

    // output
    AgentRenamed,
}
```

`SetAgentName` is sent on the target agent stream
(`/agent/<agent_id>/<instance_id>`). The request arrives through an agent
subscription boundary, but the effect is still global for that live agent.

### 5.3 Payloads

```rust
pub struct SetAgentNamePayload {
    pub name: String,
}

pub struct AgentRenamedPayload {
    pub agent_id: AgentId,
    pub name: String,
}
```

Rules:

- `SetAgentNamePayload.name` must be non-empty after trimming.
- `AgentRenamedPayload.name` is the new effective display name.

---

## 6. Spawn Without a Name

When `spawn_agent` omits `name`, the server resolves the default name before it
creates the live agent.

### 6.1 New Spawn Flow

For `SpawnAgentParams::New`:

1. Validate the spawn payload.
2. Resolve backend settings and workspace roots normally.
3. If `name` is `Some`, use it as `user_alias`.
4. If `name` is `None`, run the internal name-generation flow.
5. Create the real live agent using the resolved effective name.
6. Persist the session record with:
   - `user_alias = Some(name)` for explicit user names
   - `alias = Some(name)` for generated names
7. Fan out `NewAgent` and replay `AgentStart` with that resolved name.

The key point is ordering: the real agent is not created until the name is
known. That guarantees that the first visible agent event already contains the
correct name.

### 6.2 Resume Flow

For `SpawnAgentParams::Resume`:

- if `name` is `Some`, it becomes the new `user_alias`
- if `name` is `None`, reuse the persisted effective name from the session
  store

Resume does not run the ephemeral naming flow. The session store is the durable
source of truth for resumed sessions.

If a resumed session has neither `user_alias` nor `alias`, that is invalid
state and the resume should fail loudly instead of inventing a new title from
partial context.

---

## 7. Internal Ephemeral Name Generator

The automatic naming path is a server-internal helper. It is not part of the
agent protocol.

### 7.1 Required Behavior

The helper:

- takes the initial prompt plus backend/workspace context
- runs a single short naming turn
- returns one sanitized 2-4 word name
- shuts down immediately

The helper must not:

- allocate a user-visible `AgentId`
- register in `AgentRegistry`
- emit `NewAgent`
- emit `AgentStart`
- emit `ChatEvent`
- emit `AgentError`
- attach to any frontend stream
- write to `SessionStore`
- create `agent_id -> session_id` bindings

It is an implementation detail of `HostHandle::spawn_agent`, not a protocol
entity.

### 7.2 Backend Selection

The internal naming helper should use the same `backend_kind` as the real
target spawn, with a forced low-cost startup policy where supported.

Reasons:

- no new client-visible routing parameter is needed
- local vs remote behavior stays inside the server/backend boundary
- naming quality stays aligned with the actual backend family

The helper should use the same workspace roots for cwd/transport correctness,
but it should not expose extra frontend-visible state or create a resumable
session.

### 7.3 Prompt Contract

The naming prompt should be strict and server-owned. The backend should be told
to return only a short title, for example:

- 2-4 words
- no quotes
- no markdown
- no explanation
- based only on the user's initial request

The server then sanitizes the returned text:

- trim outer whitespace
- strip surrounding quotes
- collapse internal whitespace
- validate the final result is 2-4 words

If the result is invalid, the whole spawn fails.

### 7.4 Backend Interface

This should stay internal to the server. We do not add an `ephemeral` flag to
`SpawnAgentPayload`.

A minimal internal shape is:

```rust
pub struct GenerateAgentNameRequest {
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub prompt: String,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
}

pub async fn generate_agent_name(
    request: GenerateAgentNameRequest,
) -> Result<String, String>;
```

This helper may dispatch to backend-specific ephemeral spawn support already
present in the server layer.

---

## 8. Renaming a Live Agent

Renaming must update both the live in-memory agent model and the persisted
session model.

### 8.1 Flow

1. Client sends `SetAgentName { name }` on the target agent stream.
2. The router resolves `agent_id` from that stream and forwards the request to
   the agent actor.
3. The agent actor validates and applies the rename as the live source of truth.
4. The agent actor persists `SessionStore.user_alias = Some(name)` for its bound
   `SessionId`.
5. The agent actor updates its replayed `AgentStart` snapshot so future
   subscribers see the new name immediately.
6. The agent actor emits a live `AgentRenamed { agent_id, name }` to current
   subscribers on that agent stream.
7. The host fans out a refreshed `SessionList` snapshot so session UIs update
   without a manual refresh.

This is last-write-wins. If two frontends rename the same agent, whichever
rename the host processes last becomes the effective name.

### 8.2 Why Agent Actor Ownership

The request arrives via one agent stream instance, but the rename still affects
all subscribers to that agent. That does not make it host-owned metadata. The
important boundary is source of truth:

- the agent actor owns the current live name
- the session store owns the durable alias fields
- the host/router only forwards the request and fans out derived host-level
  views like `SessionList`

This avoids a split-brain model where the agent stream and a host registry could
disagree about the current name.

---

## 9. Replay Consistency

This feature introduces an important replay requirement:

> A newly connected client must never see one current name on `NewAgent` and a
> different current name in replayed `AgentStart`.

To satisfy that, renaming must update both:

- the agent actor's current start snapshot used for replay
- any host-level listing path that synthesizes `NewAgent` from live agent
  snapshots

The agent actor may keep the rest of its event log unchanged. We do not need
rename history on the agent stream for this feature. We only need the replayed
starting snapshot to reflect the current effective name.

---

## 10. Session Store Rules

The session store remains the durable source of truth for persisted naming.

Required operations already implied by `05-session-resume.md` become necessary
for this feature:

- `set_alias_if_missing(session_id, alias)`
- `set_user_alias(session_id, user_alias)`
- `effective_name(session_id) -> Option<String>`

Additional rules:

- automatic name generation writes `alias`
- explicit user naming writes `user_alias`
- runtime task updates may seed `alias` only if `alias` is currently `None`
- runtime task updates must not overwrite generated aliases

This avoids the current bug where a backend task title could silently replace
the server-generated default name.

---

## 11. Frontend Behavior

The frontend still owns no naming semantics. It only reacts to host events.

Required behavior:

- `NewAgent` inserts or upserts the live agent with the current effective name
- `AgentRenamed` updates the existing `AgentInfo.name`
- any streaming UI state that caches `agent_name` for display must also update
  on `AgentRenamed`

The frontend must not:

- generate fallback names
- preserve local rename state not confirmed by the server
- infer effective names from backend/tool content

For session views, the server should fan out a fresh `SessionList` snapshot
after name changes so the session list remains reactive without a manual
refresh.

---

## 12. Affected Areas

Expected implementation touch points:

- `protocol/src/types.rs`
  - `SpawnAgentPayload.name: Option<String>`
  - add `SetAgentNamePayload`
  - add `AgentRenamedPayload`
  - add `FrameKind::SetAgentName`
  - add `FrameKind::AgentRenamed`
- `server/src/router.rs`
  - validate optional spawn names
  - route `SetAgentName`
- `server/src/host.rs`
  - resolve generated names before spawn
  - persist `alias` vs `user_alias` correctly
  - synthesize host-facing views from live agent snapshots
  - push refreshed `SessionList` snapshots after naming changes
- `server/src/agent/registry.rs`
  - stop being authoritative for mutable agent names
- `server/src/agent/mod.rs`
  - own live renaming
  - persist `user_alias`
  - mutate the stored `AgentStartPayload` replay snapshot
  - emit live `AgentRenamed` events
- `server/src/store/session.rs`
  - add explicit alias/user_alias mutation helpers
  - stop overwriting `alias` when already set
- `client` and `frontend`
  - accept optional spawn names
  - handle `AgentRenamed`
- `dev-driver/src/agent_control.rs`
  - remove client-side fallback name generation

---

## 13. Tests

At minimum we should add coverage for:

- spawning with `name: Some(...)` persists `user_alias` and skips ephemeral
  naming
- spawning with `name: None` generates an internal name and exposes only the
  real agent
- automatic naming does not create session-store records for the helper
- automatic naming does not emit `NewAgent`/`AgentStart` for the helper
- renaming a live agent updates current subscribers
- renaming a live agent updates late-subscriber replay consistency
- renaming persists across `Resume`
- backend task updates do not overwrite generated names
- invalid generated names fail the spawn

---

## 14. Implementation Order

Recommended order:

1. Make `SpawnAgentPayload.name` optional end-to-end.
2. Remove client-side fallback name generation.
3. Add agent-stream `SetAgentName` / `AgentRenamed`.
4. Add session-store helpers and fix alias-overwrite behavior.
5. Make the agent actor own rename and replay snapshot mutation.
6. Add the internal ephemeral name-generation helper.
7. Add session snapshot fanout after naming changes.
8. Add tests for explicit names, generated names, rename persistence, and
   replay consistency.

This keeps the protocol and persistence rules clear before introducing the
internal helper path.
