# First-Class Sub-Agents

This document extends:

- `01-philosophy.md`
- `03-agents.md`
- `04-host-registry.md`
- `05-session-resume.md`

It specifies how Tyde2 should expose backend-native sub-agents as first-class,
server-owned agents with normal host fanout, normal replay, and durable session
history, without pushing lifecycle inference into the frontend.

---

## 1. Overview and Motivation

Tyde2 already has most of the pieces needed for sub-agents:

- `ClaudeBackend` and `CodexBackend` already detect native sub-agent spawns and
  completions.
- The protocol already carries `parent_agent_id` on spawn and birth-certificate
  payloads.
- The frontend already groups agents by parent and already has
  `hide_sub_agents` / `hide_child_sessions` toggles.

The missing piece is the important one: the detection code is never connected
to the host registry. `SubAgentEmitter` exists today, but
`set_subagent_emitter()` is only used in tests. As a result, backend-native
sub-agents are not real Tyde agents:

- they are not registered in the server
- they do not fan out through the normal host stream
- they do not replay to late subscribers
- they do not persist in Tyde's session history
- the frontend has to infer "programmatic" behavior from
  `parent_agent_id.is_some()`

That violates the architecture in `01-philosophy.md`. The server should own the
model, the protocol should encode provenance explicitly, and the UI should only
render what the server tells it.

The design here fixes the root cause instead of adding UI workarounds:
backend-native children become ordinary registry entries backed by lightweight
relay actors.

### v1 scope

Version 1 supports exactly one level of nesting:

- root/user-spawned agents may surface backend-native children
- relay children do not themselves get sub-agent emitters

This keeps the first implementation simple while still making native children
visible, replayable, and persisted.

---

## 2. Architecture

### 2.1 Core Model

Tyde2 should represent three different live-agent origins:

- `User`: explicitly spawned or resumed by a human user
- `AgentControl`: spawned programmatically through Tyde-owned orchestration
  such as agent-control MCP
- `BackendNative`: spawned by the backend's own native sub-agent mechanism

These are not the same thing as parentage.

- `parent_agent_id` answers: "which live agent owns this child?"
- `origin` answers: "who created this live agent?"

Both are required. A child can be user-resumable and interactive
(`AgentControl`) or backend-native and read-only (`BackendNative`).
`parent_agent_id` alone cannot encode that distinction.

### 2.2 Relay Agent Actor

Backend-native children should be represented by a new lightweight agent actor
type in `server/src/agent/mod.rs`.

Its properties are:

- it has a normal `AgentId`
- it has a normal `AgentStartPayload`
- it has a normal per-agent event log and subscriber list
- it is created with `parent_agent_id: Some(...)`
- it is created with `origin: AgentOrigin::BackendNative`
- it reads from `mpsc::UnboundedReceiver<ChatEvent>`
- it does not own a backend subprocess
- it does not have a `BackendHandle`
- it does not accept direct user input

Operationally, the relay actor behaves like a normal agent stream from the
host's perspective:

1. append `AgentStart`
2. forward each `ChatEvent` into the canonical event log
3. fan those events out to attached subscribers
4. terminate when the sub-agent event channel closes

This is the key architectural move. The host registry remains the single source
of truth, and late subscribers get replay for native children exactly the same
way they do for regular agents.

### 2.3 HostSubAgentEmitter

`HostSubAgentEmitter` lives in `server/src/host.rs`.

It must not live in backend modules. The backend's job is only to detect native
sub-agent activity and forward typed events. Registry ownership belongs to the
server.

`SubAgentEmitter` and `SubAgentHandle` should be moved out of
`server/src/backend/claude.rs` into a shared server module so that:

- `claude.rs` and `codex.rs` both import the same trait from a neutral location
- `codex.rs` no longer depends on `claude.rs` for shared types
- the emitter API is defined once

`HostSubAgentEmitter` holds:

- a host-owned spawn channel
- the parent `AgentId`
- the parent workspace roots

When a backend reports a native child spawn, the emitter sends a typed spawn
request back to the host. The host then creates the relay agent through the
registry and returns the channel handle the backend will write `ChatEvent`s
into.

The flow is:

```text
Claude/Codex backend
  -> HostSubAgentEmitter
  -> host sub-agent spawn channel
  -> AgentRegistry creates relay agent
  -> host fans out NewAgent
  -> relay actor emits AgentStart + ChatEvent replay/live events
```

The critical ownership rule is unchanged: the backend detects, but the host
registers.

### 2.4 Wiring Point

The emitter must be attached when the parent agent backend is created.

In `server/src/agent/mod.rs`, after a Claude or Codex backend is successfully
spawned or resumed, Tyde attaches a `HostSubAgentEmitter` before the backend is
hidden behind `BackendHandle`.

This is the right place because:

- the concrete backend type is still known
- the parent `AgentId` is known
- the parent workspace roots are known
- the server can wire native-child detection once, centrally

The important detail is architectural, not syntactic: do the wiring before type
erasure. Do not box the backend first and then try to recover concrete
sub-agent APIs later.

---

## 3. Protocol Changes

### 3.1 `AgentOrigin`

Add a strongly typed enum in `protocol/src/types.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOrigin {
    User,
    AgentControl,
    BackendNative,
}
```

This is the canonical provenance field for live agents.

### 3.2 Extend Agent Birth-Certificate Payloads

Add `origin: AgentOrigin` to both:

- `AgentStartPayload`
- `NewAgentPayload`

These payloads already define the immutable metadata for a live agent. Origin
belongs there because it is fixed at creation time and the frontend must not
guess it.

### 3.3 Keep `parent_agent_id`

`parent_agent_id` stays on all existing payloads.

It is still needed for:

- parent-child grouping in the agents panel
- child-session filtering in the sessions panel
- cascade termination rules
- explicit ownership of native-child relay agents

`origin` does not replace `parent_agent_id`. They encode different facts.

### 3.4 Frontend Semantics

The frontend must switch from:

- "if `parent_agent_id.is_some()` then this is a programmatic child"

to:

- "read `origin` from the server and render accordingly"

That removes the current frontend inference bug. A child can now be:

- grouped under a parent because it has `parent_agent_id`
- interactive or non-interactive based on `origin`

### 3.5 Validation Rules

Protocol validation should enforce these invariants:

- `BackendNative` agents must have `parent_agent_id: Some(...)`
- `User` and `AgentControl` agents may or may not have a parent
- the frontend must never derive origin from parentage

These rules make the model explicit instead of heuristic.

---

## 4. Server Changes

### 4.1 Shared Sub-Agent Types

Move `SubAgentEmitter` and `SubAgentHandle` into a shared server module and
import them from both Claude and Codex backends.

The shared API should be protocol-facing, not backend-JSON-facing:

- the relay boundary should carry `ChatEvent`
- backend-specific detection stays inside backend modules
- the host/registry only sees typed agent events

That keeps the boundary consistent with `01-philosophy.md`: use protocol types
end-to-end unless the field is a runtime-only transport detail.

### 4.2 Relay Spawn Path in the Registry

`server/src/agent/registry.rs` should gain a relay-agent spawn path alongside
the existing backend-backed spawn path.

The relay spawn path creates:

- an `AgentStartPayload` with `origin = BackendNative`
- a new `AgentHandle` that supports snapshot/attach but not user input
- a relay actor that drains `mpsc::UnboundedReceiver<ChatEvent>`

From the host's point of view, replay and subscription behavior stays exactly
the same because the relay actor still owns an event log and attached streams.

### 4.3 Host-Side Native Child Creation

`server/src/host.rs` should own the typed request that comes from
`HostSubAgentEmitter`.

When the host receives a native-child spawn request, it:

1. resolves the parent agent snapshot from the registry
2. creates a relay agent with the parent's `AgentId`
3. inherits the parent's workspace roots and project association
4. sets `origin = BackendNative`
5. persists the child session record with the parent's session as `parent_id`
6. fans out `NewAgent` to current host subscribers

No frontend code participates in this lifecycle. The server creates a normal
agent entry and the UI learns about it through the normal event flow.

### 4.4 Emitter Wiring in `agent/mod.rs`

The current bug is that Claude and Codex already know how to detect sub-agents
but never receive a real emitter in production.

Fix that in `server/src/agent/mod.rs`:

- after `ClaudeBackend::spawn` / `ClaudeBackend::resume`, attach
  `HostSubAgentEmitter`
- after `CodexBackend::spawn` / `CodexBackend::resume`, attach
  `HostSubAgentEmitter`
- do not attach an emitter for other backends
- do not attach an emitter to relay agents in v1

This is the minimal change that turns the existing detection code into a real
feature.

### 4.5 Cascade Termination

Tyde distinguishes two child lifecycles.

For user-spawned children:

- when a parent is cancelled or terminated, its Tyde-owned children also
  terminate
- because v1 only supports one level, this is a flat lookup by
  `parent_agent_id`

For backend-native children:

- lifecycle remains owned by the parent backend
- the relay actor terminates when the backend drops the sub-agent event sender
- no extra backend subprocess cancellation path is needed

This keeps ownership clean. Tyde owns Tyde-created children; backend-native
children remain projections of backend-owned execution.

---

## 5. Frontend Changes

### 5.1 State and Dispatch

Extend frontend agent state with `origin`.

`frontend/src/dispatch.rs` should stop using
`parent_agent_id.is_some()` as a proxy for "programmatic."

Instead:

- `User` agents may auto-open and take focus
- `AgentControl` agents should not steal focus automatically
- `BackendNative` agents should not steal focus automatically

This preserves current behavior for programmatic children while making the rule
explicit and correct.

### 5.2 Agents Panel

The existing agents panel already groups by `parent_agent_id` and already has a
global `hide_sub_agents` toggle.

Add two presentation changes:

- `collapsed_parents: RwSignal<HashSet<AgentId>>` for per-parent
  collapse/expand
- a child-count badge on each parent card

These are pure UI state and should remain purely derived from server-emitted
agent records. The grouping rule still comes from `parent_agent_id`, not a
frontend cache.

### 5.3 Chat Input

Backend-native relay agents do not accept direct user messages.

For `origin == BackendNative`, the chat input becomes read-only:

- disable the textarea
- disable send
- disable interrupt/steer

This is the correct UI for a relay actor. It renders the child's stream but
does not pretend the user can talk to the backend-native subprocess directly.

### 5.4 Sessions Panel

The sessions panel already has `hide_child_sessions`.

No new frontend session-grouping logic is needed. The existing behavior remains
correct once the server persists child sessions and sends accurate
`resumable` values:

- filter by `summary.parent_id`
- disable resume for non-resumable native children

The important fix is in the server and protocol, not in the panel.

---

## 6. Session Persistence Rules

Tyde should persist all three session categories, but not all of them are
resumable.

### 6.1 User-Spawned Root

- persisted
- visible in session history
- resumable

### 6.2 User-Spawned Child

- persisted
- linked to parent through `parent_id`
- filtered by parent in the sessions panel
- resumable

### 6.3 Backend-Native Child

- persisted
- linked to parent through `parent_id`
- filtered by parent in the sessions panel
- not resumable

### 6.4 Store Changes

`server/src/store/session.rs` currently hardcodes `resumable: true` in
`SessionStore::summaries()`.

That must change. The session store should persist the resumability of each
record and return the stored value in `SessionSummary`.

For native children, the stored value is always `false`.

Per `05-session-resume.md`, Tyde should still store the backend's own child
session/thread identity in `SessionId`. `resumable = false` means "do not
reopen this record," not "invent a second Tyde-only session ID."

This matters for two reasons:

- the sessions panel already disables resume when `summary.resumable` is false
- the server must reject resume attempts for backend-native child sessions

Non-resumable is a server-owned fact, not a frontend convention.

---

## 7. Edge Cases

### 7.1 Late Subscribers and Replay

Relay agents must replay exactly like normal agents. Because they live in the
registry and own a normal event log, a newly connected frontend should receive:

- `NewAgent` on the host stream
- a fresh instance stream
- `AgentStart`
- the replayed `ChatEvent` history

No special frontend replay logic is needed.

### 7.2 Missing Parent

If the host cannot resolve the parent agent for a backend-native spawn request,
it must fail visibly and refuse to create the child.

Tyde should not create orphan native children and let the frontend guess how to
display them later.

### 7.3 Direct Input to a Native Child

The frontend should not send input to a backend-native relay, but the server
must still treat such input as invalid.

The relay actor does not own a backend handle, so there is no valid place to
route `SendMessage` or `Interrupt`. The request should fail visibly rather than
silently disappearing.

### 7.4 Resume of a Native Child Session

Backend-native child sessions are persisted for history, not for reopening.

If a caller tries to resume one anyway, the server must reject it based on the
stored `resumable = false` value.

### 7.5 Parent Cancellation

User-spawned children cascade from the parent.

Backend-native children do not need a separate cascade mechanism. The parent
backend owns their lifetime, and the relay actor exits when its event channel
closes.

### 7.6 One-Level Nesting

V1 stops at one level by construction:

- relay agents are not backed by Claude/Codex subprocesses
- Tyde does not attach a sub-agent emitter to relay agents

So a native child cannot produce another first-class Tyde child in v1.

---

## 8. Implementation Order

Recommended order:

1. Add `AgentOrigin` to `protocol/src/types.rs` and extend
   `AgentStartPayload` / `NewAgentPayload`.
2. Update protocol validation and generated frontend types so origin is
   available end-to-end.
3. Move `SubAgentEmitter` / `SubAgentHandle` into a shared server module and
   make the relay boundary use `ChatEvent`.
4. Add a relay-agent actor path in `server/src/agent/mod.rs` and
   `server/src/agent/registry.rs`.
5. Add `HostSubAgentEmitter` and the host-owned native-child spawn channel in
   `server/src/host.rs`.
6. Wire `HostSubAgentEmitter` into Claude and Codex backend creation in
   `server/src/agent/mod.rs`.
7. Persist per-session `resumable` state and make backend-native child sessions
   store `resumable = false`.
8. Update frontend dispatch, agents panel, and chat input to use `origin`
   instead of inferring behavior from `parent_agent_id`.
9. Add the per-parent collapse state and child-count badge in the agents panel.
10. Add tests for emitter wiring, relay replay, non-resumable native child
    sessions, read-only native-child chat input, and one-level nesting.

This sequence keeps the protocol and server model authoritative before making
the frontend render the new state.
