# Terminal Stream Proposal

This document proposes the terminal stream used for IDE terminals in Tyde2.

It builds on:

- `01-philosophy.md` for architectural constraints
- `02-protocol.md` for stream and event rules
- `06-projects.md` for project identity and multi-root ownership
- `07-project-stream.md` for the stream design pattern we want to follow

---

## 1. Goals

We want a dedicated terminal stream that lets the frontend render and control an
interactive shell running on the backend.

The stream must provide enough typed behavior to build:

- create terminal tabs from a project or explicit path
- send keystrokes / pasted input
- resize the PTY when the panel size changes
- render live terminal output
- detect clean exit vs signal termination
- close a running terminal

This is explicitly server-owned behavior:

- the frontend does not spawn local shells on its own
- the frontend does not know or care whether the backend is local or remote
- the server owns PTY lifecycle, process lifecycle, and terminal routing
- the frontend renders bytes and sends user input; it does not reconstruct
  shell state

This is not a project stream clone.

Projects are stable server entities with a stable identity.
Terminals are runtime entities that are born, stream output, and die.

So the design should borrow the stream model from projects, but the lifecycle
should look more like agents:

- create on the host stream
- interact on the terminal stream
- terminate once the process exits or the user closes it

---

## 2. Stream URL

Terminal streams use this path:

```text
/terminal/<terminal_id>
```

Example:

```text
/terminal/550e8400-e29b-41d4-a716-446655440000
```

Rules:

- `terminal_id` is a server-generated `TerminalId`
- the terminal stream is created only after a successful `terminal_create`
- all terminal input and output for that runtime terminal flow on that one
  stream
- each side maintains its own monotonic sequence counter for
  `/terminal/<terminal_id>`
- once a terminal exits, that stream is terminal and is never reused

Why no extra instance ID:

- unlike agents, the first version should make terminals connection-scoped
- there is one owning frontend connection for a terminal
- we do not need replay across subscribers in v1

Connection scoping is explicit:

- a terminal belongs to the host stream that created it
- another connection cannot attach to that `terminal_id`
- if the owning connection closes, the server closes all of that connection's
  live terminals

This prevents hidden cross-frontend sharing and keeps the first implementation
simple.

---

## 3. Core Model

### 3.1 Terminal identity

The terminal has a typed server-owned identity:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalId(pub String);
```

Rules:

- `TerminalId` is UUID text
- it is only meaningful inside the current connection's lifetime
- the stream path is the routing key, so terminal stream payloads do not need
  to repeat `terminal_id`

This is an intentional break from the old terminal payloads, which echoed
`terminal_id` in every event. On a dedicated stream, that duplication is
unnecessary.

### 3.2 Launch target

The terminal must launch from an explicit backend location. The frontend must
not guess paths from UI labels.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalLaunchTarget {
    Project {
        project_id: ProjectId,
        root: ProjectRootPath,
        relative_cwd: Option<String>,
    },
    Path {
        cwd: String,
    },
}
```

Why both forms:

- `Project` keeps project ownership explicit and reuses typed multi-root
  identity
- `Path` allows terminals outside project UI, such as a generic backend shell

Rules:

- `Project.project_id` must exist
- `Project.root` must belong to that project
- `Project.relative_cwd` is relative to `root`; it must not escape the root
- `Path.cwd` must be an absolute backend path

The server resolves both forms to one concrete backend cwd.

### 3.3 Terminal metadata

The frontend needs immutable metadata for rendering and bookkeeping:

```rust
pub struct TerminalStartPayload {
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
}
```

Rules:

- `project_id` and `root` are `Some` only when launched from a project target
- `cwd` is the resolved backend path actually used
- `shell` is the actual executable the server spawned
- `cols` and `rows` are the initial PTY size

The UI may display this metadata, but it must not infer later cwd changes from
shell output. If we want cwd tracking later, it needs explicit protocol support.

---

## 4. Event Model

The terminal feature uses two stream classes:

- host stream: creation and discovery
- terminal stream: ongoing terminal interaction

The client sends `terminal_create` on the host stream because the terminal does
not exist yet.

After creation:

- the server emits `new_terminal` on the host stream
- the server emits `terminal_start` as seq 0 on `/terminal/<terminal_id>`
- the client sends follow-up events on `/terminal/<terminal_id>`
- the server emits output and lifecycle events on `/terminal/<terminal_id>`

There are no request IDs and no request/response pairing.

The stream itself is the correlation mechanism:

- `terminal_send` causes later `terminal_output` events on the same stream
- `terminal_resize` changes the PTY attached to that stream
- `terminal_close` ends that terminal's runtime

The frontend does not wait for a one-shot response. It reacts to events.

---

## 5. Input Events

### 5.1 `terminal_create`

Sent on `/host/<uuid>`.

Creates a new interactive shell terminal on the backend.

```rust
pub struct TerminalCreatePayload {
    pub target: TerminalLaunchTarget,
    pub cols: u16,
    pub rows: u16,
}
```

Rules:

- `cols >= 2`
- `rows >= 1`
- the server chooses the shell
- on Unix, the shell should be launched as a login shell for parity with a
  normal terminal session
- the server should set `TERM=xterm-256color`

Expected outputs:

- `new_terminal` on the host stream
- `terminal_start` on `/terminal/<terminal_id>`
- later `terminal_output`

The create payload intentionally does not include arbitrary command execution in
v1. The first version is an IDE shell, not a generic subprocess API.

### 5.2 `terminal_send`

Sent on `/terminal/<terminal_id>`.

Writes raw user input into the PTY.

```rust
pub struct TerminalSendPayload {
    pub data: String,
}
```

Rules:

- `data` is written exactly as provided
- the frontend is responsible for including `\r`, `\n`, escape sequences, paste
  payloads, and control characters as needed
- the server must not reinterpret the input as high-level commands

Expected outputs:

- zero or more `terminal_output`
- optionally `terminal_error` if the terminal is no longer running

### 5.3 `terminal_resize`

Sent on `/terminal/<terminal_id>`.

Updates the PTY size.

```rust
pub struct TerminalResizePayload {
    pub cols: u16,
    pub rows: u16,
}
```

Rules:

- `cols >= 2`
- `rows >= 1`
- the server applies the resize to the PTY immediately

Expected outputs:

- usually none
- optionally `terminal_error` if the terminal is no longer running

### 5.4 `terminal_close`

Sent on `/terminal/<terminal_id>`.

Closes a running terminal and releases server resources.

```rust
pub struct TerminalClosePayload {}
```

Expected outputs:

- `terminal_exit`

Rules:

- this is terminal termination, not tab hiding
- if the terminal has already exited, the stream is already terminal and the
  client should not send more input

---

## 6. Output Events

### 6.1 `new_terminal`

Sent on `/host/<uuid>`.

Announces that a terminal was created and tells the client which stream to use.

```rust
pub struct NewTerminalPayload {
    pub terminal_id: TerminalId,
    pub stream: StreamPath,
}
```

Rules:

- `stream` must equal `/terminal/<terminal_id>`
- `new_terminal` is live-only; it is not replayed to future connections
- the client should register outgoing sequence state for this stream before it
  attempts `terminal_send` or `terminal_resize`

Why keep `new_terminal` if the stream path is derivable:

- it matches the existing runtime-stream pattern used for agents
- it gives the client an explicit discovery event on the host stream
- it avoids ad hoc string construction at the UI boundary

### 6.2 `terminal_start`

Sent on `/terminal/<terminal_id>`.

Birth certificate for the terminal stream. This is always the first terminal
stream event.

```rust
pub struct TerminalStartPayload {
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
}
```

Rules:

- `terminal_start` is always seq 0 on a terminal stream
- it is emitted exactly once
- it contains immutable launch metadata, not mutable runtime state

### 6.3 `terminal_output`

Sent on `/terminal/<terminal_id>`.

Carries terminal text output in arrival order.

```rust
pub struct TerminalOutputPayload {
    pub data: String,
}
```

Rules:

- `data` is an opaque terminal text chunk; the frontend writes it directly into
  the terminal emulator
- the server must preserve chunk order
- the server should not split a chunk in the middle of a UTF-8 code point
- the server should not split a trailing incomplete ANSI escape sequence

That last rule is worth calling out because the old implementation already hit
this edge. We should carry that lesson forward.

What the frontend must not do:

- parse prompts to discover cwd
- infer command completion from output text
- repair broken escape sequence boundaries

### 6.4 `terminal_exit`

Sent on `/terminal/<terminal_id>`.

Marks terminal termination.

```rust
pub struct TerminalExitPayload {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}
```

Rules:

- emitted exactly once
- after `terminal_exit`, no more `terminal_output` may be emitted
- `exit_code: Some(_)` means the process exited normally
- `signal: Some(_)` means the process was terminated by signal where the
  platform can report it
- both may be `None` only if the platform cannot determine the reason

### 6.5 `terminal_error`

Sent on `/terminal/<terminal_id>`.

Reports terminal-level failures that are not protocol violations.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalErrorCode {
    NotRunning,
    IoFailed,
    Internal,
}

pub struct TerminalErrorPayload {
    pub code: TerminalErrorCode,
    pub message: String,
    pub fatal: bool,
}
```

Examples:

- `terminal_send` after exit -> `not_running`, `fatal: false`
- PTY read/write failure while the terminal is active -> `io_failed`,
  `fatal: true`

Protocol violations are still fail-fast bugs, not `terminal_error` events.
Examples of protocol violations:

- malformed `/terminal/...` stream path
- sending to a terminal owned by another connection
- invalid launch target that claims a root belongs to a project when it does not

Those should panic with diagnostics.

---

## 7. Lifecycle

### 7.1 Creation flow

Recommended flow:

1. client sends `terminal_create` on `/host/<uuid>`
2. server validates target and initial size
3. server allocates `TerminalId`
4. server spawns the PTY-backed shell
5. server emits `new_terminal` on `/host/<uuid>`
6. server emits `terminal_start` as seq 0 on `/terminal/<terminal_id>`
7. server emits `terminal_output` as bytes arrive

### 7.2 Runtime flow

While running:

- client sends `terminal_send`
- client sends `terminal_resize`
- server emits `terminal_output`

On termination:

- process exits on its own, or
- client sends `terminal_close`

Then:

- server emits one `terminal_exit`
- server drops terminal resources
- stream is done forever

### 7.3 Connection close

When the owning frontend connection closes:

- the server kills any still-running terminals owned by that connection
- no terminal streams are replayed to future connections

This is deliberate for v1.

Terminals are interactive runtime resources, not durable state like projects or
sessions.

---

## 8. Server Architecture

The philosophy doc is explicit here: prefer actors over locks.

So the terminal system should be built as:

- one connection-owned terminal registry actor
- one actor per terminal
- typed messages from the router into that actor

### 8.1 Terminal actor responsibilities

One terminal actor owns:

- `TerminalId`
- resolved launch metadata
- PTY master handle
- PTY writer
- child process handle
- current exit state
- output stream handle back to the owning connection

The terminal actor handles:

- write input
- resize PTY
- close terminal
- emit output
- emit exit

This actor serializes all terminal mutations. No other task writes terminal
state directly.

### 8.2 Reader task

Reading from the PTY is blocking / stream-driven work, so it is reasonable to
have one helper task that reads PTY output and forwards typed messages into the
terminal actor.

Important:

- the helper task does not own terminal state
- it does not emit protocol frames directly
- it only forwards read chunks and EOF/error notifications to the actor

That keeps output ordering and lifecycle decisions in one place.

### 8.3 Registry ownership

The registry must track terminal ownership by connection.

That means:

- lookups for `/terminal/<terminal_id>` are resolved against the current
  connection's registry
- a connection cannot send events to another connection's terminal
- cleanup on disconnect is deterministic

This should not be a global shared terminal map with implicit caller trust.

---

## 9. Ordering and Invariants

These should be asserted hard:

- `new_terminal` is emitted before any server event on `/terminal/<terminal_id>`
- `terminal_start` must be seq 0 on `/terminal/<terminal_id>`
- `terminal_output` must preserve PTY read order
- `terminal_exit` is emitted exactly once
- no `terminal_output` after `terminal_exit`
- no client input on a terminal stream before `new_terminal`
- no client input on a terminal stream owned by another connection

If any of these are violated, that is a bug in the architecture or routing.

---

## 10. Failure Model

We should distinguish three classes of failure:

### 10.1 Protocol violations

These are impossible-state bugs and should panic loudly:

- malformed stream path
- cross-connection terminal access
- invalid enum payloads
- impossible ownership mismatches

### 10.2 User-visible terminal failures

These are runtime failures on a valid stream and should emit `terminal_error`:

- PTY I/O failure
- sending input after exit

### 10.3 Terminal process termination

This is normal lifecycle, not an error:

- shell exited 0
- shell exited non-zero
- shell was killed by signal
- user closed terminal

Those should always be represented by `terminal_exit`.

---

## 11. Relation to the Old Implementation

Useful things to carry forward from `old/*`:

- PTY-backed terminal process, not pipe-only subprocess I/O
- default shell selection by platform
- login shell behavior on Unix
- `TERM=xterm-256color`
- guarding chunk boundaries so we do not split UTF-8 or incomplete ANSI escapes
- explicit resize validation

Things not to carry forward:

- global terminal IDs echoed in every payload
- request/response command semantics
- terminal state hidden behind ad hoc mutable maps without explicit stream
  ownership

The old code is a behavior reference, not the architecture to preserve.

---

## 12. Non-Goals

Not part of the first version:

- terminal persistence across reconnect
- multi-subscriber attach to one live terminal
- server-side prompt parsing
- cwd tracking from shell output
- terminal titles derived from escape sequences
- arbitrary subprocess launch API
- scrollback replay protocol for late subscribers
- copy/paste history, search, or shell integration metadata

If we need any of those later, they should be added as explicit protocol types
and events, not inferred in the frontend.
