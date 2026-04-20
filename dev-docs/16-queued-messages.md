# Queued Messages

This document specifies queued follow-up messages for busy agents in Tyde2. It
builds on `01-philosophy.md`, `03-agents.md`, and `14-session-settings.md`.

---

## 1. Overview and Motivation

Tyde v1 had useful queued-message UX, but it lived entirely in the frontend:
`queuedMessages[]` in `ConversationView`, local queue rendering above the input,
and client-side "send now" steering via `pendingSteer`. That preserved the
interaction, but it violated Tyde2's architecture philosophy:

- The frontend owned queue semantics.
- Queue state was not part of the typed protocol.
- Multiple subscribers could not converge on one canonical queue.

Tyde2 should keep the interaction and move the ownership to the server.

The design is:

- The client still sends `SendMessage`. There is no separate `QueueMessage`
  command.
- The server decides whether that message executes immediately or becomes a
  queued follow-up.
- The server emits queue state as a typed event stream snapshot.
- The frontend only renders the current queue state and sends queue-management
  commands by queued-message ID.

This keeps the UX from v1 while following the Tyde2 rules: one source of
truth, server-owned behavior, typed protocol, and state flowing through events.

### Goals

- Allow users to submit follow-up messages while an agent is busy.
- Preserve v1's visible queue, cancel, and "send now" behavior.
- Make queued state converge across all subscribers to the same live agent.
- Keep queued state out of transcript history.

### Non-goals

- Persist queued messages in the session store.
- Restore queued messages after agent termination or session resume.
- Ship inline editing in the first version of the feature.
- Add client-side heuristics for deciding when a message should queue.

---

## 2. Protocol Changes

### 2.1 `SendMessage` stays unchanged

`SendMessage` remains the only client event for "send this user input to the
agent." The client does not branch between "send" and "queue." It always sends
`SendMessage` on the agent stream, and the server decides:

- If the agent is idle, execute immediately.
- If the agent is in a turn, enqueue it and emit updated queue state.

This is the correct boundary. Busy-vs-idle is server-owned runtime state, so
the server must own the decision.

### 2.2 New protocol types

All types belong in `protocol/src/types.rs`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueuedMessageId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessageEntry {
    pub id: QueuedMessageId,
    pub message: String,
    pub images: Vec<ImageInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessagesPayload {
    pub messages: Vec<QueuedMessageEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditQueuedMessagePayload {
    pub id: QueuedMessageId,
    pub message: String,
    pub images: Vec<ImageInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelQueuedMessagePayload {
    pub id: QueuedMessageId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendQueuedMessageNowPayload {
    pub id: QueuedMessageId,
}
```

Rules:

- `QueuedMessageId` is server-generated. The client never invents queue IDs.
- `QueuedMessageEntry.images` is a concrete `Vec`, not `Option<Vec<_>>`.
  Empty vector means "no images." The actor can normalize
  `SendMessagePayload.images` into this shape when it queues a message.
- `ImageInput` here means the canonical input-side image type. If the protocol
  still uses a differently named but identical input-image struct, rename and
  reuse that type instead of keeping two mirror types.

### 2.3 New `FrameKind` variants

```rust
pub enum FrameKind {
    // Existing variants ...

    // Input events (client -> server) on agent streams
    EditQueuedMessage,
    CancelQueuedMessage,
    SendQueuedMessageNow,

    // Output events (server -> client) on agent streams
    QueuedMessages,
}
```

| Kind | Stream | Direction | Description |
|------|--------|-----------|-------------|
| `EditQueuedMessage` | `/agent/<agent_id>/<instance_id>` | client -> server | Replace one queued entry by ID |
| `CancelQueuedMessage` | `/agent/<agent_id>/<instance_id>` | client -> server | Remove one queued entry by ID |
| `SendQueuedMessageNow` | `/agent/<agent_id>/<instance_id>` | client -> server | Make one queued entry the next message to run |
| `QueuedMessages` | `/agent/<agent_id>/<instance_id>` | server -> client | Full queued-message snapshot |

### 2.4 `QueuedMessages` is control-plane state, not chat history

`QueuedMessages` must be a standalone `FrameKind`, not a `ChatEvent` variant.

Rationale:

- The queue is server-owned control state, like `SessionSettings`.
- It is not transcript history and should not appear as a chat message.
- The frontend should not reconstruct current queue state by replaying deltas.
- A full snapshot lets any subscriber replace its local render state in one
  step.

`QueuedMessages` therefore follows the same shape as other server-owned
snapshots:

- The payload is always the full current queue.
- The server emits a new full snapshot on every queue change.
- The server also replays the latest snapshot to new subscribers.
- Empty queue is represented explicitly as `QueuedMessages { messages: [] }`.

---

## 3. Server Agent Actor Changes

The queue belongs in `server/src/agent/mod.rs`. The actor already owns agent
runtime state and serializes inputs, so it is the correct home for queue
ordering and drain behavior.

### 3.1 New actor state

Add:

```rust
queue: VecDeque<QueuedMessageEntry>,
in_turn: bool,
```

`queue` is the server-owned backlog for that live agent. `in_turn` is the
actor's runtime scheduling flag for whether the agent is currently processing a
turn.

`in_turn` is not a frontend concept. The UI still uses
`ChatEvent::TypingStatusChanged(bool)` as the visible activity signal. The
server uses `in_turn` only to decide whether new `SendMessage` input should
queue or execute.

### 3.2 `SendMessage` handling

When the actor receives `SendMessage`:

- If `in_turn` is `true`, generate a `QueuedMessageId`, normalize the payload
  into `QueuedMessageEntry`, push it to the back of `queue`, and broadcast a
  fresh `QueuedMessages` snapshot.
- If `in_turn` is `false`, set `in_turn = true` and forward the original
  `SendMessage` payload to the backend immediately.

Setting `in_turn = true` before forwarding closes a race: without that, a
second `SendMessage` could arrive before the backend emits
`TypingStatusChanged(true)` and accidentally bypass the queue.

### 3.3 `TypingStatusChanged(false)` drains the queue

The actor should continue treating `TypingStatusChanged(bool)` as the
authoritative signal for overall agent activity. Queue drain happens only on
`TypingStatusChanged(false)`, not on `StreamEnd`.

When the backend emits `ChatEvent::TypingStatusChanged(false)`:

- Set `in_turn = false`.
- If `queue` is empty, do nothing else.
- If `queue` is non-empty, pop the front entry.
- Broadcast an updated `QueuedMessages` snapshot reflecting the removal.
- Set `in_turn = true`.
- Convert the popped entry back into `SendMessagePayload` and forward it to the
  backend.

The actor must not wait for a later `TypingStatusChanged(true)` before marking
itself busy again. Once it has decided to dispatch the next queued message, the
turn is logically re-opened.

This also matches the existing agent protocol: `TypingStatusChanged(false)` is
the only reliable "agent is idle now" signal. Backends may differ on the exact
relative ordering of `StreamEnd` vs `TypingStatusChanged(false)`, so queue
drain must key off `TypingStatusChanged(false)` only.

### 3.4 Queue-management commands

Add handlers in the agent actor for the new input events:

`EditQueuedMessage`

- Find the entry by `QueuedMessageId`.
- Replace `message` and `images`.
- Broadcast a fresh `QueuedMessages` snapshot.

`CancelQueuedMessage`

- Remove the matching entry.
- Broadcast a fresh `QueuedMessages` snapshot.

`SendQueuedMessageNow`

- Find the matching entry.
- Move it to the front of the queue.
- Broadcast a fresh `QueuedMessages` snapshot.
- If `in_turn` is `true`, interrupt the backend. When the current turn later
  reaches `TypingStatusChanged(false)`, the front entry drains next.
- If `in_turn` is `false`, remove that entry from the queue and dispatch it
  immediately instead of waiting for an idle signal that has already happened.

### 3.5 Invalid IDs must fail visibly

Queue commands use server-issued IDs, but stale IDs are still possible because
multiple subscribers can race each other.

If the actor receives `EditQueuedMessage`, `CancelQueuedMessage`, or
`SendQueuedMessageNow` for an unknown ID, it should emit a non-fatal
`AgentError` on that agent stream. It should not silently no-op.

That keeps failure visible without treating a stale queue click as a protocol
panic.

### 3.6 Queue snapshots are replay state, not session history

Queued-message snapshots should replay to new subscribers, but they must not
append to the durable transcript history.

The actor therefore needs the same split it already uses for other current
state:

- Live subscribers receive a `QueuedMessages` frame every time the queue
  changes.
- Replay state stores only the latest queue snapshot, including the empty
  snapshot.
- When the queue changes again, that replay snapshot is overwritten in place.
- The session store is unchanged. Queue contents are never persisted there.

This is important for architecture:

- A new subscriber to a live agent should immediately see the current queue.
- A resumed or restarted agent should not resurrect stale queued messages from a
  previous runtime.

### 3.7 Router and validator updates

`server/src/router.rs` must route the three new client frame kinds on agent
streams to the target agent actor.

`protocol/src/validator.rs` must accept:

- `QueuedMessages` on agent streams.
- `EditQueuedMessage`, `CancelQueuedMessage`, and `SendQueuedMessageNow` on
  agent streams.

No parallel frontend-only command model should be introduced. Extend the
existing typed protocol path end-to-end.

---

## 4. Frontend Changes

### 4.1 State

Add to `frontend/src/state.rs`:

```rust
pub agent_message_queue: RwSignal<HashMap<AgentId, Vec<QueuedMessageEntry>>>,
```

This is the frontend's reactive projection of the last server snapshot, not a
local ownership model. The frontend never computes queue membership or ordering
on its own.

### 4.2 Dispatch

Add `FrameKind::QueuedMessages` handling in `frontend/src/dispatch.rs`.

Behavior:

- Resolve the `AgentId` from the agent stream.
- Parse `QueuedMessagesPayload`.
- Replace `agent_message_queue[agent_id]` with `payload.messages`.

This must be replace-in-full, not append/patch logic. The frame is already a
full snapshot from the server.

### 4.3 Chat input UI

Update `frontend/src/components/chat_input.rs`:

- Render the queue area above the textarea.
- Each row shows a short preview of the queued message plus:
  `[Send Now ↑] [Cancel ×]`.
- Clicking "send now" sends `SendQueuedMessageNow`.
- Clicking "cancel" sends `CancelQueuedMessage`.
- Submitting the main input while an agent is busy still sends plain
  `SendMessage`. The input component does not decide whether it queued.

### 4.4 Reactivity rules for the queue list

The queue UI must follow the rules in `01-philosophy.md`:

- Key queue rows by `QueuedMessageId`, never by array index.
- Do not snapshot queue entry fields into a same-key row.
- If queue rows become their own component, pass the stable ID and look up the
  current `QueuedMessageEntry` reactively inside the row.

That keeps future same-ID updates, including `EditQueuedMessage`, reactive.

### 4.5 Inline edit is not part of phase 1

Tyde v1 did not support inline editing of queued items, and phase 1 should not
block on adding it now.

Phase 1 ships:

- queue rendering
- cancel
- send now

If inline edit is added later, the edit draft belongs in local component state
the same way the textarea draft does. The canonical queued entry still belongs
to the server and only changes when `EditQueuedMessage` is sent.

---

## 5. Queue Lifecycle

### 5.1 Queueing

When the user submits input:

- If the agent is idle, the message executes immediately.
- If the agent is busy, the server enqueues it at the back and emits a new
  `QueuedMessages` snapshot.

### 5.2 Reordering

When the user clicks "send now":

- The selected queued entry moves to the front of the queue.
- The server broadcasts the updated snapshot.
- If the current turn is still running, the server interrupts it so the newly
  front-most queued entry becomes the next message to run.

### 5.3 Draining

When the agent becomes idle (`TypingStatusChanged(false)`):

- If the queue is empty, nothing happens.
- If the queue is non-empty, the front entry is removed from the queue,
  broadcast out via a new snapshot, and sent to the backend as the next turn.

### 5.4 Clearing

The queue clears when:

- the last queued item is cancelled
- the last queued item drains
- the agent terminates

Whenever it clears during the agent's lifetime, the server emits
`QueuedMessages { messages: [] }`.

### 5.5 Lifetime

The queue is:

- per-agent
- runtime-only
- not persisted to the session store
- lost when that live agent terminates

If the user later resumes the underlying session as a new live agent, that new
agent starts with an empty queue.

---

## 6. Edge Cases

### Agent dies with queued messages

Queued messages are not recoverable. On fatal termination, the queue is dropped.
The server should emit `QueuedMessages { messages: [] }` before the terminal
`AgentError` when possible so the UI clears immediately. Regardless, the queue
must not be written to the session store.

### Multiple `SendQueuedMessageNow` clicks

The actor processes commands serially. Each `SendQueuedMessageNow` request
reorders the current queue state at the time it is handled. The last processed
request wins.

### Interrupt failure during "send now"

If the backend cannot interrupt, the actor should emit a non-fatal `AgentError`
and keep the reordered queue. The requested item remains at the front and will
run when the current turn naturally reaches `TypingStatusChanged(false)`.

### Stale queue IDs

Another subscriber may cancel or drain an item before this client acts on it.
Unknown queue IDs should produce non-fatal `AgentError`, not silent no-op.

### Multiple subscribers

Because the queue is server-owned and emitted as full snapshots, all connected
frontends converge on the same queue ordering. One client's cancel or send-now
action updates every other client through the same `QueuedMessages` frame.

### Image-only queued messages

If `SendMessage` allows image-only input, the queue must preserve that exactly.
An empty `message` with non-empty `images` is valid if the original
`SendMessagePayload` was valid.

### `StreamEnd` ordering differences

Some backends may not place `TypingStatusChanged(false)` at exactly the same
point relative to `StreamEnd`. Queue drain still keys only off
`TypingStatusChanged(false)`, because that is the server's authoritative idle
signal in the agent protocol.

---

## 7. Implementation Order

1. Update `protocol/src/types.rs` with the new ID, entry, payload, and
   `FrameKind` variants. Update codegen outputs if needed.
2. Update `protocol/src/validator.rs` to accept the new frame kinds on agent
   streams.
3. Extend the agent input path in `server/src/router.rs` and
   `server/src/agent/mod.rs` so the actor can receive queue-management
   commands.
4. Add actor queue state, snapshot broadcast helpers, drain-on-idle logic, and
   queue clearing on termination.
5. Add server tests for:
   queue while busy,
   drain on `TypingStatusChanged(false)`,
   cancel,
   send-now reordering,
   replay of current queue snapshot to new subscribers,
   termination clearing.
6. Add `agent_message_queue` to `frontend/src/state.rs` and full-snapshot
   replacement in `frontend/src/dispatch.rs`.
7. Add the queue UI to `frontend/src/components/chat_input.rs` with send-now and
   cancel controls.
8. Ship phase 1 without inline edit UI. `EditQueuedMessage` can land now as
   protocol/server support or remain the first follow-up slice, but it must not
   block the initial queue feature.
