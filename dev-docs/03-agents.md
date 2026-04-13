# Agent Protocol

This document specifies the protocol additions for agent lifecycle and chat
streaming in Tyde2. It builds on the wire protocol defined in `02-protocol.md`
and follows the philosophy in `01-philosophy.md`.

---

## 1. Overview

An **agent** is a long-lived coding assistant session managed by the server. The
client creates agents by sending input events; the server streams output events
back as the agent works. All agent state lives on the server. The client renders
whatever events arrive.

The protocol is event-driven. There are no request IDs, no response correlation,
and no request/response pairing. The client sends a `spawn_agent` event and then
listens for output events on the agent's stream. The stream URL is the
correlation mechanism.

Key principles:
- **Server owns agent state.** The client never reconstructs agent semantics.
- **Streams are the routing key.** Every agent gets its own stream per
  subscriber. Events on that stream are that agent's events.
- **`ChatEvent` is the output event model.** The battle-tested `ChatEvent` enum
  from the old protocol is carried forward exactly. It is wrapped in a single
  `FrameKind::ChatEvent` variant, not decomposed into individual frame kinds.
- **Strong typing everywhere.** IDs are newtypes (`AgentId`), states are enums,
  tool types are tagged unions.

### Relation to old Tyde protocol

Carry forward (minimal subset):
- A trimmed `ChatEvent` enum with 10 core variants and their data types
  (`ChatMessage`, `StreamStartData`, `StreamTextDeltaData`, `StreamEndData`,
  `ToolRequest`, `ToolExecutionCompletedData`, etc.)
- Typed tool request/result modeling (`ToolRequestType`, `ToolExecutionResult`)
- Runtime agent state implicit from stream events (not a separate enum)

Redesign:
- Remove `Invoke/Result` request-response framing entirely
- Replace string event dispatch (`event: "..."`) with `FrameKind` enum variants
- Use stream paths as routing/correlation (`/agent/<id>/<instance>`), not
  `conversation_id` strings
- Replace `is_running: bool` with implicit state derived from stream events
  (between `StreamStart` and `StreamEnd` = thinking, after `StreamEnd` = idle)

---

## 2. Stream URLs for Agents

### Pattern

```
/agent/<agent_id>/<instance_id>
```

Example: `/agent/a1b2c3d4-e5f6-7890-abcd-ef1234567890/f9e8d7c6-b5a4-3210-fedc-ba0987654321`

- `agent_id` — The persistent agent identity on the server (UUID,
  server-generated). This is how you address the agent.
- `instance_id` — A per-subscriber stream instance (UUID, server-generated per
  connection). This is how the server routes events to a specific subscriber.

### Rules

- The **server** generates both UUIDs. The client does not generate agent
  stream URLs.
- The server sends all output events for that agent on this stream.
- The client sends follow-up input events (e.g. `send_message`) on the same
  stream.
- Each agent stream instance has independent sequence counters for each
  direction (client→server and server→client), both starting at 0.
- An agent stream instance is tied to its connection. When the connection
  closes, the instance is gone.

### Multi-frontend and replay

Multiple frontends can connect to the same server. Each gets their own
`instance_id` for the same `agent_id`, but both receive **identical event
streams**.

When a new frontend subscribes to an existing agent, the server replays all
events from the beginning of that agent's history on the new instance stream,
then continues streaming live events. The replayed events have fresh sequence
numbers (starting at 0) on the new instance — they are not copies of sequence
numbers from other instances.

This means:
- Frontend A spawns an agent. It gets `/agent/<id>/<instance_A>`.
- Frontend B connects later and subscribes. It gets `/agent/<id>/<instance_B>`
  with a full replay of all events that have occurred so far, followed by live
  events going forward.
- Both frontends see the same logical event stream. The server stores the
  canonical event log per agent, not per subscriber.

When a new client completes the handshake, the server automatically subscribes
it to all active (non-terminated) agents. For each active agent, the server
allocates a new `instance_id`, replays the full event history, and then streams
live events. There is no explicit "subscribe" input event — subscription is
automatic on connection.

### Why server-generated?

The server owns agent identity. If the client generated the UUID, it would need
to optimistically assume the agent exists before the server confirms creation.
Instead: the client sends `spawn_agent` on the `/host/<uuid>` stream (the
connection control stream). The server creates the agent, generates its
`agent_id` and `instance_id`, and emits `AgentStart` (the birth certificate,
always seq 0) on the new `/agent/<agent_id>/<instance_id>` stream. The client
learns the stream URL from this event.

---

## 3. New FrameKind Variants

The `FrameKind` enum gains these variants:

```rust
pub enum FrameKind {
    // Existing handshake kinds
    Hello,
    Welcome,
    Reject,

    // Input events (client → server)
    SpawnAgent,
    SendMessage,

    // Output events (server → client)
    AgentStart,
    ChatEvent,
    AgentError,
}
```

### Input kinds

| Kind           | Stream                              | Direction        | Description |
|----------------|-------------------------------------|------------------|-------------|
| `SpawnAgent`   | `/host/<uuid>`                      | client → server  | Create a new agent |
| `SendMessage`  | `/agent/<agent_id>/<instance_id>`   | client → server  | Send follow-up message to existing agent |

### Output kinds

| Kind           | Stream                              | Direction        | Description |
|----------------|-------------------------------------|------------------|-------------|
| `AgentStart`   | `/agent/<agent_id>/<instance_id>`   | server → client  | Agent birth certificate (first frame, seq 0) |
| `ChatEvent`    | `/agent/<agent_id>/<instance_id>`   | server → client  | All chat/streaming events (wraps `ChatEvent` enum) |
| `AgentError`   | `/agent/<agent_id>/<instance_id>`   | server → client  | Agent-level error |

### Notes

- `SpawnAgent` is sent on the `/host/<uuid>` stream because the agent doesn't
  exist yet — there's no agent stream to send it on.
- `SendMessage` is sent on the agent's stream because the agent already exists.
- All output events are sent on the agent's stream, including the very first
  `AgentStart` event that tells the client the agent was created.
- `AgentStart` is always seq 0 on any agent stream — it is the first frame.
- `ChatEvent` is a single frame kind that wraps the entire `ChatEvent` enum.
  The inner enum's `kind` tag discriminates the specific event type
  (`StreamStart`, `StreamDelta`, `ToolRequest`, etc.).

---

## 4. Input Event Payloads (Client → Server)

### SpawnAgentPayload

Sent on `/host/<uuid>` with kind `SpawnAgent`.

```rust
pub struct SpawnAgentPayload {
    pub name: String,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub params: SpawnAgentParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnAgentParams {
    New {
        workspace_roots: Vec<String>,
        prompt: String,
        backend_kind: BackendKind,
        cost_hint: Option<SpawnCostHint>,
    },
    Resume {
        session_id: SessionId,
        prompt: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnCostHint {
    Low,
    #[serde(rename = "med", alias = "medium")]
    Medium,
    High,
}
```

- `name` — Human-readable label for the agent.
- `parent_agent_id` — If this is a sub-agent spawned by another agent.
- `project_id` — Optional project association for this live agent.
- `params` — Explicit `new` vs `resume` spawn mode.
- `params.new.cost_hint` — Optional backend-agnostic startup hint. `low`,
  `med`, and `high` are mapped by each backend to its own cheaper or more
  capable defaults.

### SendMessagePayload

Sent on `/agent/<agent_id>/<instance_id>` with kind `SendMessage`. The stream
path identifies the target agent — no `agent_id` field needed.

```rust
pub struct SendMessagePayload {
    pub message: String,
}
```

- `message` — The follow-up message content.

**Terminated agent rule:** If the client sends `SendMessage` to an agent that
is terminated, the server emits
`AgentError { code: internal, message: "agent not running", fatal: false }` on
that stream. It does not silently discard the message.

**Stream auth rule:** The server validates that the `instance_id` in the stream
path was issued to the current connection. A `SendMessage` on an `instance_id`
that was not issued to this connection is a protocol violation — the server
panics with diagnostics (fail fast).

---

## 5. Output Event Payloads (Server → Client)

All output events are emitted on `/agent/<agent_id>/<instance_id>`. The stream
path carries the agent identity, so `ChatEvent` payloads do not need additional
agent routing. Only `AgentStartPayload` and `AgentErrorPayload` include
`agent_id` because they carry identity or standalone error context.

**Invariant:** The `agent_id` in `AgentStartPayload` and `AgentErrorPayload`
MUST match the `<agent_id>` segment in the stream path. The receiver asserts
this — a mismatch is a bug.

### AgentStartPayload

Agent birth certificate. This is always the first event on any agent stream
(seq 0). It is emitted exactly once per stream instance — it announces the
agent's existence and provides its immutable metadata. It is not a state
change event and is never emitted again.

```rust
pub struct AgentStartPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub created_at_ms: u64,
}
```

- The `AgentStart` event is how the client learns the agent's stream URL and
  ID. After receiving it, the client renders events from this stream.
- There is no `state` field. Agent runtime state is implicit from stream
  events: between `StreamStart` and `StreamEnd` the agent is thinking, after
  `StreamEnd` the agent is idle/waiting.
- There is no `summary`, `error`, or `updated_at_ms` field. These are
  runtime-mutable concerns that don't belong in a birth certificate.
- `project_id` is included because project ownership is explicit server-owned
  state, not UI inference.

### ChatEvent (payload)

The `ChatEvent` enum is the payload for `FrameKind::ChatEvent`. It is carried
forward exactly from the old protocol. On the wire it looks like:

```json
{
  "stream": "/agent/<agent_id>/<instance_id>",
  "kind": "chat_event",
  "seq": 5,
  "payload": {
    "kind": "StreamDelta",
    "data": { "text": "Let me look at the code." }
  }
}
```

The inner `kind` + `data` structure comes from the `ChatEvent` enum's
`#[serde(tag = "kind", content = "data")]` representation. See section 6 for
the full enum definition and all referenced data types.

### AgentErrorPayload

Agent-level error. This is for errors that aren't tool failures or stream
issues — things like backend crashes, protocol errors with the subprocess,
or internal server errors related to this agent.

```rust
pub struct AgentErrorPayload {
    pub agent_id: AgentId,
    pub code: AgentErrorCode,
    pub message: String,
    pub fatal: bool,
}
```

- `agent_id` — Included for standalone error context.
- `code` — Machine-readable error category (enum).
- `fatal` — If `true`, the agent is dead. No further events will arrive on
  this stream after a fatal `AgentError`. If `false`, the stream continues
  (e.g. a recoverable backend hiccup).

---

## 6. Rust Types

All types below belong in `protocol/src/types.rs`.

### Typed IDs

```rust
/// Strongly typed agent identifier. Wraps a UUID string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
```

### BackendKind

```rust
/// Which coding agent backend to use. Enum, not string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Claude,
    Codex,
    Gemini,
}
```

Only backends that Tyde2 will initially support. The old protocol had `Tycode`
and `Kiro` — those are excluded until needed. Add variants when backends are
implemented, not preemptively.

### AgentErrorCode

```rust
/// Machine-readable agent error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorCode {
    BackendFailed,
    Internal,
}
```

Minimal — only codes that exist now. More variants will be added when error
scenarios are implemented.

### FrameKind (updated)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    // Handshake
    Hello,
    Welcome,
    Reject,

    // Input events (client → server)
    SpawnAgent,
    SendMessage,

    // Output events (server → client)
    AgentStart,
    ChatEvent,
    AgentError,
}
```

### Input payloads

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnAgentPayload {
    pub name: String,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub params: SpawnAgentParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnAgentParams {
    New {
        workspace_roots: Vec<String>,
        prompt: String,
        backend_kind: BackendKind,
        cost_hint: Option<SpawnCostHint>,
    },
    Resume {
        session_id: SessionId,
        prompt: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SpawnCostHint {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessagePayload {
    pub message: String,
}
```

### Output payloads

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStartPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub parent_agent_id: Option<AgentId>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentErrorPayload {
    pub agent_id: AgentId,
    pub code: AgentErrorCode,
    pub message: String,
    pub fatal: bool,
}
```

### ChatEvent and all referenced types

Minimal subset of the old protocol's `ChatEvent` enum — only the variants
needed for core agent interaction. More variants will be added as needed.
Data types are carried forward from the old protocol (minus `ts_rs`/
`VariantNames` derives).

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ChatEvent {
    MessageAdded(ChatMessage),
    StreamStart(StreamStartData),
    StreamDelta(StreamTextDeltaData),
    StreamReasoningDelta(StreamTextDeltaData),
    StreamEnd(StreamEndData),
    ToolRequest(ToolRequest),
    ToolExecutionCompleted(ToolExecutionCompletedData),
    TaskUpdate(TaskList),
    OperationCancelled(OperationCancelledData),
    RetryAttempt(RetryAttemptData),
}

// ── Message types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageSender {
    User,
    System,
    Warning,
    Error,
    Assistant { agent: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub timestamp: u64,
    pub sender: MessageSender,
    pub content: String,
    pub reasoning: Option<ReasoningData>,
    pub tool_calls: Vec<ToolUseData>,
    pub model_info: Option<ModelInfo>,
    pub token_usage: Option<TokenUsage>,
    pub context_breakdown: Option<ContextBreakdown>,
    pub images: Option<Vec<ImageData>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningData {
    pub text: String,
    pub tokens: Option<u64>,
    pub signature: Option<String>,
    pub blob: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseData {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_prompt_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub system_prompt_bytes: u64,
    pub tool_io_bytes: u64,
    pub conversation_history_bytes: u64,
    pub reasoning_bytes: u64,
    pub context_injection_bytes: u64,
    pub input_tokens: u64,
    pub context_window: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    pub media_type: String,
    pub data: String,
}

// ── Stream event data ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStartData {
    pub message_id: Option<String>,
    pub agent: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTextDeltaData {
    pub message_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndData {
    pub message: ChatMessage,
}

// ── Tool types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_type: ToolRequestType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ToolRequestType {
    ModifyFile {
        file_path: String,
        before: String,
        after: String,
    },
    RunCommand {
        command: String,
        working_directory: String,
    },
    ReadFiles {
        file_paths: Vec<String>,
    },
    SearchTypes {
        language: String,
        workspace_root: String,
        type_name: String,
    },
    GetTypeDocs {
        language: String,
        workspace_root: String,
        type_path: String,
    },
    Other {
        args: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionCompletedData {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_result: ToolExecutionResult,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ToolExecutionResult {
    ModifyFile {
        lines_added: u64,
        lines_removed: u64,
    },
    RunCommand {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    ReadFiles {
        files: Vec<FileInfo>,
    },
    SearchTypes {
        types: Vec<String>,
    },
    GetTypeDocs {
        documentation: String,
    },
    Error {
        short_message: String,
        detailed_message: String,
    },
    Other {
        result: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub bytes: u64,
}

// ── Other event data ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationCancelledData {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryAttemptData {
    pub attempt: u64,
    pub max_retries: u64,
    pub error: String,
    pub backoff_ms: u64,
}

// ── Task types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub description: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskList {
    pub title: String,
    pub tasks: Vec<Task>,
}
```

---

## 7. Backends

Agents are backed by a **backend** — the actual coding agent process that does
the work. The `Backend` trait is the abstraction boundary between the agent actor
and the underlying implementation. Each `BackendKind` variant (`Claude`, `Codex`,
`Gemini`) maps to a concrete backend type.

### The Backend trait

```rust
pub struct BackendSpawnConfig {
    pub cost_hint: Option<SpawnCostHint>,
}

pub trait Backend: Send + 'static {
    fn spawn(
        workspace_roots: Vec<String>,
        initial_prompt: String,
        config: BackendSpawnConfig,
    ) -> impl Future<Output = Result<(Self, EventStream), String>> + Send
    where
        Self: Sized;

    fn send(&self, input: AgentInput) -> impl Future<Output = bool> + Send;
}
```

- `BackendSpawnConfig.cost_hint` — backend-agnostic startup hint. Real backends
  may map this to a cheaper model, lower reasoning effort, or both.
- `spawn(workspace_roots, initial_prompt, config)` — static constructor.
  Creates a new session and returns a handle (`Self`) for sending input and an
  `EventStream` for reading output. True duplex — send and receive are
  independent.
- `send(&self, input)` — send an `AgentInput` to the backend. Returns `false`
  if the backend has terminated and can't accept input.
- Backends are **not object-safe** — the agent actor knows the concrete type at
  compile time. The trait uses `impl Future` (not `async fn` in trait) by
  design.

### EventStream

Separate type for reading `ChatEvent`s from the backend:

```rust
pub struct EventStream {
    rx: mpsc::Receiver<ChatEvent>,
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<ChatEvent> { ... }
}
```

Returns `None` when the backend has terminated. The agent actor owns the
`EventStream` and reads from it independently of sending input through the
`Backend` handle.

### AgentInput

The typed input enum — the contract between the connection/host layer and the
agent actor. This is an internal type, not a wire protocol type.

```rust
pub enum AgentInput {
    SendMessage(SendMessagePayload),
}
```

Currently has one variant. Will grow as capabilities expand (`Cancel`,
`Interrupt`, etc).

### BackendKind dispatch

The protocol carries both `BackendKind` and the optional `SpawnCostHint`, so
the host/registry can choose a concrete backend and pass the startup hint
through unchanged.

Today, the host-backed integration path still uses `MockBackend` for protocol
tests and development fixtures. The direct backend wrappers used by
`tests/tests/backend.rs` already honor `BackendSpawnConfig.cost_hint`, so the
wire shape is in place and the mapping logic is backend-specific.

### MockBackend

The test/development backend. For each input message, it emits a deterministic
`StreamStart` → `StreamDelta` → `StreamEnd` turn. No network calls, no
subprocess. Useful for integration tests and UI development without a real
coding agent.

### Real backends (future)

`Claude`, `Codex`, and `Gemini` backends will each spawn a subprocess (the
respective CLI tool), communicate via stdin/stdout, and translate subprocess
output into `ChatEvent`s. Each implements the same `Backend` trait and may map
`SpawnCostHint` differently:

- `Claude`: lower or higher model family (`haiku` / `sonnet` / `opus`)
- `Codex`: lower or higher reasoning effort, and eventually model overrides
- `Gemini`: lower or higher model tier (`flash-lite` / `flash` / `pro`)
- `Kiro` / others: backend-specific model selection where supported

### Key design decisions

- **Backend doesn't know about `StreamPath` or instance IDs.** It just produces
  `ChatEvent`s. The Host stamps streams per subscriber.
- **Backend doesn't receive `BackendKind` as a parameter.** A `MockBackend`
  knows it's mock, a `ClaudeBackend` knows it's Claude. The kind is metadata in
  `AgentStartPayload`, not something passed to the backend.
- **Backend startup policy lives in `BackendSpawnConfig`.** Shared knobs like
  `cost_hint` belong in an explicit config object, not by overloading the
  prompt or `BackendKind`.
- **Backend doesn't receive the agent name.** That's metadata in
  `AgentStartPayload`, not the backend's concern.
- **The trait uses `impl Future`** (not `async fn` in trait) because backends
  are not object-safe by design. The agent actor knows the concrete type.

---

## 8. Agent Lifecycle

### Creation

```
Client                          Server
  │                               │
  │─── SpawnAgent ───────────────→│  (on /host/<uuid>)
  │    { workspace_roots,         │
  │      prompt, backend_kind,    │
  │      name }                   │
  │                               │
  │                               │  Server allocates agent_id and instance_id,
  │                               │  creates /agent/<agent_id>/<instance_id>,
  │                               │  then starts backend subprocess and sends
  │                               │  initial prompt.
  │                               │
  │←── AgentStart ───────────────│  (on /agent/<agent_id>/<instance_id>, seq 0)
  │    { agent_id, name,          │
  │      backend_kind, ... }      │
  │                               │
  │←── ChatEvent ────────────────│  (seq 1)
  │    { kind: "StreamStart",     │
  │      data: { agent: "..." }}  │
  │                               │
```

The client now knows the agent exists and its stream URL. It renders events
from `/agent/<agent_id>/<instance_id>`. The `StreamStart` event signals the
agent is thinking.

**Stream allocation ordering:** The server MUST allocate the agent stream
(`/agent/<agent_id>/<instance_id>`) before attempting backend startup. If
backend startup fails, the error events are delivered on this already-allocated
stream. The stream is never allocated lazily.

If startup fails, the client sees the agent stream open with:
1. `AgentStart { ... }` (seq 0 — always emitted)
2. `AgentError { fatal: true, code: backend_failed, ... }` (stream is dead)

### Streaming a response turn

```
  │←── ChatEvent ────────────────│
  │    { kind: "StreamStart",     │
  │      data: { agent: "...",    │
  │        model: "claude-..." }} │
  │                               │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamDelta",     │
  │      data: { text: "Let " }} │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamDelta",     │
  │      data: { text: "me " }}  │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamDelta",     │
  │      data: { text: "look." }}│
  │                               │
  │←── ChatEvent ────────────────│
  │    { kind: "ToolRequest",     │
  │      data: { tool_call_id:   │
  │        "tc_1", ... }}         │
  │                               │
  │←── ChatEvent ────────────────│
  │    { kind: "ToolExecution-    │
  │      Completed", data: {     │
  │        tool_call_id: "tc_1", │
  │        ... }}                 │
  │                               │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamDelta",     │
  │      data: { text: "Based "}}│
  │                               │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamEnd",       │
  │      data: { message: ... }} │
  │                               │
```

A single response turn goes: `StreamStart` → N × (`StreamDelta` |
`StreamReasoningDelta` | `ToolRequest` | `ToolExecutionCompleted`) →
`StreamEnd`. The `StreamStart`/`StreamEnd` bookends delineate turn boundaries.
Sequence numbers provide ordering. One active turn at a time per agent stream.

The agent may produce multiple turns if it's working autonomously (each turn is
a complete `StreamStart`...`StreamEnd` cycle).

### State transitions during a turn

Agent runtime state is implicit from stream events:

- **Agent starts working** → `ChatEvent(StreamStart)` — agent is thinking.
- **Agent finishes a turn** → `ChatEvent(StreamEnd)` — agent is idle/waiting
  (unless another `StreamStart` follows immediately for autonomous work).
- **Agent hits fatal error** → `AgentError { fatal: true }` (stream is dead,
  no further events)

There is no separate state enum or typing status event. The
`StreamStart`/`StreamEnd` bookends are the single source of truth for whether
the agent is actively working or idle.

### Follow-up messages

```
  │─── SendMessage ──────────────→│  (on /agent/<agent_id>/<instance_id>)
  │    { message: "Now refactor" }│
  │                               │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamStart" ... }│
  │    ...                        │
  │←── ChatEvent ────────────────│
  │    { kind: "StreamEnd" ... }  │
  │                               │
```

### Termination

On fatal error:

```
  │←── AgentError ───────────────│
  │    { code: backend_failed,    │
  │      message: "...",          │
  │      fatal: true }            │
  │                               │
```

After `AgentError { fatal: true }`, the stream is dead. No further events on
either side. There is no terminal status event — the fatal error *is* the
terminal event.

### Multiple turns (autonomous work)

When an agent works autonomously (tool use → reasoning → more tool use), the
server emits multiple `StreamStart`...`StreamEnd` cycles on the same stream,
all wrapped in `ChatEvent` frames.

```
  │←── ChatEvent (StreamStart) ──│  Turn 1 (agent is thinking)
  │←── ChatEvent (StreamDelta) ──│
  │←── ChatEvent (ToolRequest) ──│
  │←── ChatEvent (ToolExec...) ──│
  │←── ChatEvent (StreamEnd) ────│
  │                               │
  │←── ChatEvent (StreamStart) ──│  Turn 2 (agent is thinking)
  │←── ChatEvent (StreamDelta) ──│
  │←── ChatEvent (StreamEnd) ────│  Done, agent idle
  │                               │
```

### Multi-frontend replay

```
Frontend A                      Server                      Frontend B
  │                               │                               │
  │─── SpawnAgent ───────────────→│                               │
  │←── AgentStart ───────────────│                               │
  │←── ChatEvent(StreamStart) ───│                               │
  │←── ChatEvent(StreamDelta) ───│                               │
  │←── ChatEvent(StreamEnd) ─────│                               │
  │                               │                               │
  │                               │  Frontend B connects, server  │
  │                               │  allocates new instance_id    │
  │                               │                               │
  │                               │──── AgentStart ──────────────→│  (replay, seq 0)
  │                               │──── ChatEvent(StreamStart) ──→│  (replay, seq 1)
  │                               │──── ChatEvent(StreamDelta) ──→│  (replay, seq 2)
  │                               │──── ChatEvent(StreamEnd) ────→│  (replay, seq 3)
  │                               │                               │
  │─── SendMessage ──────────────→│                               │
  │←── ChatEvent(StreamStart) ───│──── ChatEvent(StreamStart) ──→│  (live)
  │    ...                        │    ...                        │
```

Both frontends see identical logical event streams. Frontend B's replay has
fresh sequence numbers starting at 0 on its own instance stream.

---

## 9. What Changes in Server and Client Crates

### protocol crate

- Add all types from section 6 to `types.rs`.
- Add new `FrameKind` variants.
- Update `FrameKind::Display` impl.

### server crate

- **Agent actor.** Each agent runs as a single tokio task (actor pattern per
  philosophy). The actor owns: the backend subprocess handle, the agent's
  state, and the canonical event log for that agent. It receives messages via
  an `mpsc` channel from the connection handler.
- **Event log.** Each agent actor maintains an ordered log of all events
  (`AgentStart`, `ChatEvent`, and `AgentError` frames) emitted since creation.
  This log is used for replay when new subscribers connect.
- **Agent registry.** A registry mapping
  `AgentId → mpsc::Sender<AgentCommand>` so the connection handler can route
  `SendMessage` events to the right agent actor.
- **Subscriber management.** Each agent tracks its active subscribers (one per
  connected frontend). When a new subscriber connects, the agent replays its
  full event log on the new instance stream, then adds the subscriber to the
  live fanout list.
- **Connection handler changes:**
  - After handshake, enter a message dispatch loop that reads envelopes and
    routes them based on `kind`:
    - `SpawnAgent` → create a new agent actor, register it, allocate the
      instance stream, send the `AgentStart` event (seq 0).
    - `SendMessage` → look up the agent by stream path, forward to its actor.
  - The connection handler also multiplexes outgoing events from all agent
    actors onto the single wire (agents send events via an `mpsc` channel back
    to the connection handler, which writes them as envelopes).
- **Outgoing sequence tracking.** The connection handler tracks per-stream
  outgoing sequence numbers and stamps them on envelopes before writing.
  Instance streams have independent sequence counters.
- **Fail fast** on protocol violations (kind/stream mismatch, unexpected
  frame kinds).

### client crate

- **Post-handshake event loop.** After `connect()`, the client needs to read
  envelopes in a loop and dispatch them. The client library should expose an
  event stream (e.g. `async fn next_event() -> Event`) rather than raw
  envelopes.
- **Sending input events.** Helper methods like `spawn_agent(payload)` and
  `send_message(stream, text)` that construct and send the right envelopes
  with correct sequence numbers.
- **Agent stream tracking.** The client tracks which agent streams exist (from
  `AgentStart` events) and validates incoming sequence numbers per-stream.

### tests crate

- The `Fixture` needs to evolve: the server side must run a full message loop
  (not just handshake), and the fixture should expose helpers for sending
  input events and collecting output events.
- New test cases:
  - Spawn an agent → receive `AgentStart` (seq 0) on the new stream.
  - Send a message → receive `ChatEvent(StreamStart)`...
    `ChatEvent(StreamEnd)` cycle.
  - Fatal error → receive `AgentError(fatal: true)`, then no more events.
  - Sequence number validation across interleaved `/host/*` and multiple
    `/agent/*` streams.
  - Multi-frontend replay: two clients connect, second client receives full
    replay (starting with `AgentStart` at seq 0) with fresh sequence numbers,
    then both receive identical live events.

---

## 10. Decisions and Rationale

### Why `ChatEvent` as a single FrameKind instead of individual variants?

The `ChatEvent` enum is battle-tested from the old protocol. It exactly matches
what backends actually emit. Decomposing it into individual `FrameKind` variants
would create a second representation of the same event taxonomy — two enums
that must stay in sync, with translation code between them. One `FrameKind`
variant wrapping the existing enum is simpler, has zero translation overhead,
and is immediately compatible with all existing backend event handling code.

### Why no `TurnId`?

Sequence numbers already provide strict ordering. `StreamStart`/`StreamEnd`
bookends are sufficient to delineate turn boundaries — the client knows a turn
started when it sees `StreamStart` and ended when it sees `StreamEnd`. Adding a
`TurnId` to every event within a turn is redundant with this structural
guarantee and adds a field that must be generated, propagated, and validated for
no additional information.

### Why no `AgentState` enum?

Agent runtime state (thinking vs idle) is implicit from stream events: between
`StreamStart` and `StreamEnd` the agent is thinking, after `StreamEnd` the
agent is idle. Adding a separate state enum would create a second source of
truth. Instead, the `StreamStart`/`StreamEnd` bookends — which must exist
anyway for turn delineation — double as the state signal. Agent death is
signaled by `AgentError { fatal: true }` — no terminal status event needed.

### Why `/agent/<agent_id>/<instance_id>` instead of `/agent/<agent_id>`?

Multiple frontends can connect to the same server simultaneously. Each needs
its own stream with independent sequence numbers (per the wire protocol spec).
The `instance_id` is the per-subscriber discriminator. The `agent_id` is the
persistent identity used for addressing the agent across connections.

### Why not `spawn_agent` on the agent stream?

The agent doesn't exist yet. The client can't address a stream that doesn't
have a UUID. The `SpawnAgent` event goes on the host stream (which already
exists from handshake). The server responds on the *new* agent stream.

### Why `AgentStart` as a one-shot birth certificate?

`AgentStart` carries immutable metadata (name, backend_kind, workspace_roots)
and is emitted exactly once per stream at seq 0. It is not a state change
event. This is simpler than a combined status/metadata payload that also carries
mutable state — the birth certificate never changes, so the client never has to
diff or merge it. Runtime state changes flow through `ChatEvent` like all other
chat events.

### Why no `agent_id` in `ChatEvent` payloads?

The stream path `/agent/<agent_id>/<instance_id>` carries the agent identity.
Duplicating it in every `StreamDelta` payload is redundant and violates "one
source of truth." Only `AgentStartPayload` (the birth certificate) and
`AgentErrorPayload` (standalone error context) carry `agent_id`.

### Why `AgentErrorCode` enum?

Strong typing over strings. Only `BackendFailed` and `Internal` for now.
Add variants when error scenarios are implemented, not preemptively.

### What about `interrupt_agent` / `cancel_agent` / `terminate_agent`?

Deferred. Will be designed when needed.

### What about `reasoning` deltas?

Already handled — `ChatEvent::StreamReasoningDelta` is part of the carried-
forward `ChatEvent` enum.
