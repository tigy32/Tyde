# Session Resume & Session Store

This document specifies how Tyde2 should support resumable backend sessions,
how those sessions are tracked in Tyde-owned storage, and how the host protocol
should expose session discovery and resume.

It builds on:

- `02-protocol.md` for wire framing
- `03-agents.md` for agent lifecycle and backend abstractions
- `04-host-registry.md` for the host actor and shared agent registry

---

## 1. Goals

We want four related capabilities:

1. Backends must expose resumable sessions.
2. A live backend handle must expose its current session ID.
3. Tyde must keep its own session store instead of treating backend session
   discovery as the primary source of truth.
4. The host protocol must support listing sessions and spawning an agent by
   resuming an existing session.

This is not just a backend feature. Resume only works cleanly if the server owns
the mapping between:

- session identity
- agent identity
- workspace metadata
- user-visible annotations such as aliases and parent/child relationships

---

## 2. Non-Goals

- This does not specify UI behavior in detail.
- This does not require every backend to expose identical metadata quality.
  Some backends can provide exact token counts and timestamps; others can only
  provide partial metadata.
- This does not require Tyde to mirror every backend-internal concept.
  Tyde only stores the fields needed for resume, listing, and richer metadata.
- This does not make backend discovery the primary source of truth for the UI.
  Backend discovery is a reconciliation/import path.

---

## 3. Session Identity

There is exactly one session ID in this design: the backend-native resumable
session ID.

Tyde uses that same ID everywhere:

- in the session store
- in host protocol payloads
- in backend list/resume APIs
- in the live agent-to-session binding

This removes the extra ID mapping layer entirely.

The ID should be strongly typed:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);
```

`SessionId` wraps the backend-native session/thread/conversation identifier.
Tyde does not invent a second logical session ID on top of it.

---

## 4. Design Overview

At a high level:

1. Each backend can enumerate resumable sessions and can resume one by backend
   session ID.
2. The host maintains a Tyde-owned session store on disk.
3. Session listing is store-first:
   the host lists Tyde session records, optionally reconciling them with
   backend-discovered sessions before returning results.
4. `SpawnAgent` gets an explicit enum payload for `new` vs `resume`.
5. When resuming, the host loads the existing session record by `SessionId`,
   resumes the backend using that same `SessionId`, and binds the new live
   agent to the existing session record.

This preserves a clean separation:

- backends know how to find and resume backend-native sessions
- the host knows how to map those sessions into Tyde's data model
- the protocol uses the real backend-native session ID directly instead of
  inventing an extra mapping layer

Project association is also host-owned metadata. If a session is associated
with a project, that `project_id` is stored in Tyde's session store and replayed
through session summaries and resumed agents. Backends do not infer or own that
association.

---

## 5. Backend Session Metadata

`server/src/backend/mod.rs` should define a common metadata type for resumable
sessions.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendSession {
    /// Backend-native resumable session ID used everywhere in Tyde.
    pub id: SessionId,

    /// Which backend owns this session.
    pub backend_kind: BackendKind,

    /// All known workspace roots for the session.
    pub workspace_roots: Vec<String>,

    /// Backend-provided title/summary if available.
    pub title: Option<String>,

    /// Approximate cumulative token count if available.
    pub token_count: Option<u64>,

    /// Creation time in Unix milliseconds if known.
    pub created_at_ms: Option<u64>,

    /// Last modification time in Unix milliseconds if known.
    pub updated_at_ms: Option<u64>,

    /// Whether this session is currently resumable.
    pub resumable: bool,
}
```

Rules:

- `id` is required. Without it, resume is impossible.
- `workspace_roots`, token counts, and timestamps are best effort. Missing data
  is represented as empty vectors or `None`.
- This is backend-facing metadata. It is not the full store record returned to
  the client.

If a backend can surface richer data later, this struct can grow. The important
constraint is that the common fields above exist across all backends.

---

## 6. Backend Trait Changes

The current `Backend` trait only supports creating a fresh session and sending
input. Resume requires three additional capabilities:

1. list resumable sessions
2. resume a specific backend session
3. expose the current session ID for a live handle

The trait should become:

```rust
pub trait Backend: Send + 'static {
    fn spawn(initial_prompt: String) -> (Self, EventStream)
    where
        Self: Sized;

    fn resume(session_id: SessionId) -> (Self, EventStream)
    where
        Self: Sized;

    fn list_sessions() -> impl Future<Output = Result<Vec<BackendSession>, String>> + Send
    where
        Self: Sized;

    fn session_id(&self) -> SessionId;

    fn send(&self, input: AgentInput) -> impl Future<Output = bool> + Send;
}
```

### Notes

- `resume(session_id)` resumes that exact backend-native session ID. The host
  uses the same `SessionId` in its store and protocol.
- `list_sessions()` is static because it operates on backend storage, not on a
  specific live handle.
- `session_id(&self)` is not optional. A backend handle must not be exposed to
  the rest of the system until the real session ID is known.
- `send()` remains unchanged.

### Why keep both `resume()` and `session_id()`?

Because they solve different problems:

- `resume()` creates a live backend handle from an existing backend session
- `session_id()` lets the host/session store observe which backend session a
  live handle is currently bound to

The host should not have to infer the live session ID solely from streamed
events.

---

## 7. Required Backend Semantics

Every resumable backend must satisfy these invariants.

### 7.1 Fresh spawn

For a brand-new session:

- `spawn()` creates a new backend session
- the constructor does not return until the backend session ID is known
- `session_id()` returns that real session ID immediately
- the backend should also emit `ChatEvent::SessionStarted` or equivalent
  metadata event if that event already exists in the event model

### 7.2 Resume

For a resumed session:

- `resume(session_id)` attaches to that exact existing backend session
- once the session is active, `session_id()` returns the same ID
- the backend must not silently create a new unrelated session during resume
- if resume fails because the session does not exist or cannot be resumed, the
  constructor fails fast

### 7.3 Session listing

`list_sessions()` should return all backend sessions that are plausible resume
candidates, including sessions created outside the current Tyde process.

This is critical for:

- importing old sessions into the Tyde session store
- recovering after local store loss or corruption
- surfacing sessions created by older Tyde builds or directly by backend CLIs

---

## 8. Tyde-Owned Session Store

Tyde needs its own persistent session store under something like
`server/src/store/session.rs`.

This should be copied and adapted from the old implementation rather than
rewritten from scratch. The old code already had the right shape:

- persistent JSON file
- Tyde session records
- parent relationships
- alias/user alias fields
- atomic writes

### Store-first principle

The session store is the primary source of truth for client-visible session
listing.

Why:

- Tyde needs metadata that backends do not own
- multiple backends expose uneven metadata
- Tyde needs relationships such as parent session, pinned alias, and custom
  annotations

Backend session enumeration is still required, but it is used to reconcile and
import data, not as the main read path for the product.

---

## 9. Session Store Data Model

The old `SessionRecord` is a good starting point. The new version should live in
Tyde2's store module and use the backend-native `SessionId` directly.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,

    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,

    /// Tyde-generated or backend-imported title.
    pub alias: Option<String>,

    /// User-controlled override.
    pub user_alias: Option<String>,

    pub parent_id: Option<String>,

    pub created_at_ms: u64,
    pub updated_at_ms: u64,

    /// Session-level message count maintained by Tyde.
    pub message_count: u32,

    /// Best-effort aggregate token count for list views.
    pub token_count: Option<u64>,
}
```

This deliberately overlaps with `BackendSession`, but the roles are different:

- `BackendSession` is a raw import/discovery shape
- `SessionRecord` is Tyde's durable, annotated model

### Required store operations

At minimum:

- `load(path) -> SessionStore`
- `list() -> Vec<SessionRecord>`
- `get(session_id) -> Option<SessionRecord>`
- `create(...) -> SessionRecord`
- `set_alias(...)`
- `set_user_alias(...)`
- `set_parent(...)`
- `increment_message_count(...)`
- `touch_updated_at(...)`
- `delete(...)`

The old code already implements most of this and should be reused directly
where possible.

---

## 10. Session Reconciliation

Because the session store is primary but backends still own resumable session
discovery, the host needs a reconciliation path.

### Reconciliation algorithm

When the host reconciles backend-discovered sessions into the store:

1. Load all Tyde session records from the session store.
2. Ask each backend for `list_sessions()`.
3. For each discovered `BackendSession`:
   - if a `SessionRecord` already exists for `session_id`, merge newer metadata
     into that record
   - otherwise create a new `SessionRecord` seeded from backend metadata
4. Persist the updated store.
5. Return Tyde `SessionRecord`s to the client, sorted by `updated_at_ms`
   descending.

### Why merge instead of replacing?

Because store fields such as:

- `user_alias`
- `parent_id`
- Tyde-generated relationships
- future annotations

must survive backend rescans.

The backend is authoritative for backend-native facts like session existence or
backend timestamps. Tyde is authoritative for Tyde-owned annotations.

---

## 11. Live Agent to Session Binding

The host/agent layer also needs a live mapping:

- `AgentId -> SessionId`

The old `conversation_sessions.rs` logic should be adapted for Tyde2.

This registry has two jobs:

1. bind a newly spawned live agent to a Tyde session record
2. update that record as chat events arrive

### Fresh spawn flow

1. Host receives `SpawnAgent::New`.
2. Host spawns the backend via `Backend::spawn(...)`.
3. Host reads `backend.session_id()`.
4. Host creates or updates the session record keyed by that same `SessionId`.
5. Host binds the live `AgentId` to that `SessionId`.
6. As chat events arrive, update alias/message count/timestamps.

### Resume flow

1. Host receives `SpawnAgent::Resume`.
2. Host loads the existing `SessionRecord`.
3. Host reads `record.id` and `record.backend_kind`.
4. Host spawns the backend via `Backend::resume(record.id.clone())`.
5. Host asserts `backend.session_id() == record.id`.
6. Host binds the new live `AgentId` to the existing session record.
7. As new events arrive, update that same record's timestamps/counters.

This is the core rule: resuming a session creates a new live agent instance, but
it does not create a new session record.

---

## 12. Protocol Changes

The wire protocol needs host-level session APIs plus a way to request resume via
`SpawnAgent`.

### 12.1 New `FrameKind` variants

Add host-level session frames:

```rust
pub enum FrameKind {
    // existing
    Hello,
    Welcome,
    Reject,
    SpawnAgent,
    SendMessage,
    AgentStart,
    ChatEvent,
    AgentError,

    // new host/session management
    ListSessions,
    SessionList,
}
```

These frames travel on the `/host/<uuid>` stream.

### 12.2 `SpawnAgentPayload`

Do not model this as a pile of optional fields. Use an enum so `new` and
`resume` are distinct shapes.

```rust
pub struct SpawnAgentPayload {
    pub name: String,
    pub parent_agent_id: Option<AgentId>,
    pub params: SpawnAgentParams,
}

pub enum SpawnAgentParams {
    New {
        workspace_roots: Vec<String>,
        prompt: String,
        backend_kind: BackendKind,
    },
    Resume {
        session_id: SessionId,
        prompt: Option<String>,
    },
}
```

Rules:

- `SpawnAgentParams::New` creates a new backend session and a new store record
- `SpawnAgentParams::Resume` resumes the existing session record with that same
  `SessionId`
- `prompt` becomes optional so callers can resume a session without forcing an
  immediate user message

If `prompt` is present during resume, the host resumes the backend first and
then sends the prompt as the first new turn.

### 12.3 `ListSessionsPayload`

```rust
pub struct ListSessionsPayload {
    // Intentionally empty for now.
}
```

### 12.4 `SessionListPayload`

The response returned on the same host stream:

```rust
pub struct SessionSummary {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub alias: Option<String>,
    pub user_alias: Option<String>,
    pub parent_id: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: u32,
    pub token_count: Option<u64>,
    pub resumable: bool,
}

pub struct SessionListPayload {
    pub sessions: Vec<SessionSummary>,
}
```

This is intentionally Tyde-centric. It is derived from the session store, not
returned directly from backend metadata.

`ListSessions` is still event-driven:

- client sends one `ListSessions` event on `/host/<uuid>`
- server sends one `SessionList` event on `/host/<uuid>`
- `SessionListPayload.sessions` contains the full vector
- never emit one event per session

---

## 13. Host Actor Changes

`server/src/host.rs` needs session-aware commands in addition to spawn/send.

Conceptually:

```rust
pub(crate) enum HostCommand {
    Subscribe { ... },
    SpawnAgent { payload: SpawnAgentPayload },
    SendInput { ... },
    ListSessions {
        payload: ListSessionsPayload,
        host_stream: StreamPath,
        output_tx: mpsc::Sender<OutgoingFrame>,
    },
    AgentOutput { ... },
}
```

The host now owns:

- `AgentRegistry`
- `SessionStore`
- live `AgentId -> SessionId` bindings

### `spawn_agent` behavior

The host's spawn path becomes:

1. If `params == New`:
   - spawn the requested backend with `Backend::spawn`
   - read `backend.session_id()`
   - create or update the store record keyed by that `SessionId`
   - bind `agent_id -> session_id`
2. If `params == Resume`:
   - load the session record by `SessionId`
   - resume with `Backend::resume(session_id)`
   - assert the resumed handle reports the same `SessionId`
   - bind the new `agent_id -> existing session_id`
3. Emit the normal `AgentStart` event for the new live agent instance.

### `list_sessions` behavior

1. Load the session store.
2. Reconcile against `Backend::list_sessions()` when needed.
3. Emit one `SessionList` event on the host stream with all sessions in a vec.

The host remains the sole owner of session lifecycle decisions.

---

## 14. Agent Registry Changes

The registry currently only stores live agents. It should remain focused on live
agent handles, but spawn should become session-aware.

At minimum the live agent entry should retain:

- `AgentStartPayload`
- `AgentHandle`
- bound `SessionId`

The canonical persisted session metadata still belongs in the session store, not
the in-memory registry.

---

## 15. Failure Modes

The implementation should fail loudly in these cases:

- `SpawnAgentParams::Resume { session_id }` does not exist in the session store
- backend resume returns a different `SessionId` than requested
- the same `SessionId` appears under multiple backend kinds
- backend session listing returns duplicate session IDs for the same backend

These are data-integrity bugs or operator-visible failures, not cases to paper
over.

---

## 16. Migration Strategy

Recommended implementation order:

1. Add the session store module by adapting the old implementation.
2. Add live agent/session binding logic by adapting the old
   `conversation_sessions.rs` logic.
3. Extend `backend/mod.rs` with `BackendSession`, `resume()`, `list_sessions()`,
   and `session_id()`.
4. Implement the new backend trait surface backend-by-backend.
5. Extend protocol types with `ListSessions`, `SessionList`, and
   `SpawnAgentParams`.
6. Update `host.rs` and `agent/registry.rs` to use the session store and resume
   path.

This order minimizes churn because the store and binding logic define the data
model the backend and host layers need to target.

---

## 17. Open Questions

These can be resolved during implementation, but they should be tracked
explicitly.

### Should `Backend::resume()` and `Backend::spawn()` become async constructors?

Some backends may eventually need async setup for process negotiation before the
stream is usable. If that becomes awkward with synchronous constructors, we
should switch both constructors to return futures rather than making only
`resume()` special.

### Should backend reconciliation happen on every `ListSessions` call?

Probably not. With an empty `ListSessionsPayload`, reconciliation policy is a
server-side decision for now. We can add an explicit refresh knob later if it is
actually needed.

---

## 18. Summary

Session resume in Tyde2 should be built around a store-owned data model, not
around raw backend discovery.

That means:

- backends expose `list_sessions`, `resume`, and `session_id`
- Tyde persists `SessionRecord`s keyed by the real backend-native `SessionId`
- the host resumes sessions using that same `SessionId`
- the protocol exposes host-level `ListSessions` plus enum-shaped
  `SpawnAgentParams`

This gives us a stable contract for implementation without tying the product to
the quirks of any single backend CLI.
