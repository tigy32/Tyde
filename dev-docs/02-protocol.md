# Wire Protocol

This document specifies the Tyde2 wire protocol: message envelope, stream
multiplexing, connection handshake, and version negotiation.

See `01-philosophy.md` for the design decisions that shaped this spec.

---

## 1. Transport Assumptions

- A single bidirectional channel per connection (WebSocket, SSH stream, etc.).
- All messages are UTF-8 JSON, one JSON object per message.
- Framing is **newline-delimited JSON (NDJSON)**: each message is serialized as
  compact JSON (no pretty-printing) followed by `\n`. The receiver splits on
  `\n` to get individual messages. This works over any byte stream — WebSocket,
  raw TCP, SSH, stdio pipes.
- serde_json's compact serializer already escapes `\n` inside strings to `\\n`,
  so the only bare `\n` in the output is the delimiter.

---

## 2. Message Envelope

Every message on the wire is a JSON object with this shape:

```json
{
  "stream": "/host/550e8400-e29b-41d4-a716-446655440000",
  "kind": "hello",
  "seq": 0,
  "payload": { ... }
}
```

### Fields

| Field     | Type          | Required | Description |
|-----------|---------------|----------|-------------|
| `stream`  | `StreamPath`  | yes      | URL-like path identifying the logical stream |
| `kind`    | `FrameKind`   | yes      | Message type discriminator (enum) |
| `seq`     | `u64`         | yes      | Monotonic sequence number, per-stream per-sender |
| `payload` | object        | yes      | Kind-specific data (may be `{}`) |

### Sequence Numbers

Each stream has its own monotonic counter per sender, starting at 0. The first
message on a stream has `seq: 0`, the second has `seq: 1`, etc. Each side
tracks its own counter independently — the client's seq and server's seq on the
same stream are separate counters.

The receiver asserts:

- `seq` is exactly `last_seen_seq + 1` for that stream+sender (or 0 for the
  first message on that stream from that sender)
- Any gap or out-of-order delivery is a bug — panic with the expected vs actual
  seq, stream, and kind for diagnostics

This is not flow control or retry logic. It's a tripwire that catches transport
bugs, serialization errors, or dropped messages immediately.

---

## 3. Stream URLs

Streams are URL-like paths that uniquely identify a logical channel within a
connection. They are always absolute paths (start with `/`).

### Format

`/<topic>/<scope>/<uuid>`

- Segments are URL-safe tokens: `[A-Za-z0-9._:-]+`
- The last segment is always a UUIDv4, making every stream globally unique
- Case-sensitive

### Reserved Streams

| Pattern              | Purpose                  | Example |
|----------------------|--------------------------|---------|
| `/host/<uuid>`       | Handshake and connection control | `/host/550e8400-...` |

Additional stream patterns will be defined as features are added.

### Lifecycle Rules

- The initiator generates the stream UUID (client for client-initiated streams,
  server for server-initiated streams)
- A stream is used for one logical purpose
- Once a stream reaches a terminal state, it is never reused
- Messages for many streams interleave freely on the same channel

---

## 4. Handshake

The first exchange on any connection **must** be a handshake. No other messages
are valid until the handshake completes.

### 4.1 Hello (client → server)

```json
{
  "stream": "/host/550e8400-e29b-41d4-a716-446655440000",
  "kind": "hello",
  "seq": 0,
  "payload": {
    "protocol_version": 1,
    "tyde_version": { "major": 2, "minor": 0, "patch": 0 },
    "client_name": "tyde-desktop",
    "platform": "macos"
  }
}
```

### 4.2 Welcome (server → client)

```json
{
  "stream": "/host/550e8400-e29b-41d4-a716-446655440000",
  "kind": "welcome",
  "seq": 0,
  "payload": {
    "protocol_version": 1,
    "tyde_version": { "major": 2, "minor": 1, "patch": 0 },
    "bootstrap": {}
  }
}
```

After `welcome`, the connection is established. Both sides may send messages on
other streams.

### 4.3 Reject (server → client)

```json
{
  "stream": "/host/550e8400-e29b-41d4-a716-446655440000",
  "kind": "reject",
  "seq": 0,
  "payload": {
    "code": "incompatible_protocol",
    "message": "Server requires protocol version 2, client sent 1",
    "server_protocol_version": 2,
    "server_tyde_version": { "major": 3, "minor": 0, "patch": 0 }
  }
}
```

After `reject`, the server closes the connection. The client reads the
rejection, surfaces the message, and does not retry without updating.

### 4.4 Handshake Rules

- The first client message **must** be `hello` on `/host/<uuid>`
- Before handshake completes, no non-handshake messages are allowed
- Server responds on the same stream with `welcome` or `reject`
- `reject` always closes the connection
- If the server does not receive `hello` within 10s, it closes the connection
- If the client does not receive a response within 10s, connection has failed

---

## 5. Version Negotiation

### Protocol Version (`protocol_version: u32`)

- Starts at `1`
- Bumped when the envelope format changes incompatibly
- Server supports exactly one version at a time
- If `client.protocol_version != server.protocol_version` → `reject` with
  code `incompatible_protocol`

### Tyde Version (`tyde_version: Version`)

- Strongly typed semver: `{ major, minor, patch }`
- Bumped for application-level changes (new stream types, new payload fields)
- **Informational during handshake** — not a compatibility gate
- Exchanged so both sides can log it and so the server can surface warnings in
  `bootstrap` if versions are far apart

---

## 6. Error Handling

Tyde distinguishes sharply between:

- **protocol violations / impossible invariants** — bugs in Tyde itself
- **operational failures** — filesystem, git, process, backend, permission, or
  environment errors while serving a valid request

These must not be handled the same way.

### 6.1 Protocol violations

Protocol violations are fail-fast bugs. The server should panic with
diagnostics when it detects any of these:

- frame kind on the wrong stream
- malformed stream path
- message sent on a stream owned by another connection
- impossible internal state that indicates Tyde bookkeeping corruption

These are not user-facing runtime errors. They indicate a broken implementation
or a broken caller and should not be silently downgraded into normal stream
events.

### 6.2 Operational failures

Once a stream is valid and established, request handling failures must be
surfaced on **that stream**, not by crashing the whole server process.

Rules:

- The server emits a typed error event on the owning stream.
- If the error is recoverable, the error payload sets `fatal: false` and the
  stream remains usable.
- If the stream can no longer make progress, the error payload sets
  `fatal: true`; after that, no further frames are emitted on that stream.
- `reject` is only for handshake / connection establishment failure. It is not
  a substitute for stream-local runtime errors after the connection is live.

Examples:

- `terminal_send` after exit -> `terminal_error { fatal: false }`
- backend turn failure on an agent stream -> `agent_error { fatal: false|true }`
- project file read or directory listing failure -> `project_error { fatal: false }`
- project subscription becomes invalid because the project was deleted ->
  `project_error { fatal: true }`

### 6.3 Stream ownership

Each stream family defines its own typed error payload because the useful error
context differs by domain:

- host stream: `reject` during handshake only
- agent stream: `agent_error`
- terminal stream: `terminal_error`
- host browse stream: `host_browse_error`
- project stream: `project_error`

The common contract is:

- errors are scoped to the owning stream
- `fatal` means the stream is dead after the error
- non-fatal errors do not close the stream

### 6.4 Non-goal

The protocol must not swallow errors. Surfacing an error on a stream is not
"softening" it; it is preserving diagnostics without turning a routine runtime
failure into a connection-wide or process-wide outage.

---

## 7. Rust Types

These types belong in the `protocol` crate.

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The current protocol version. Bump when envelope format changes.
pub const PROTOCOL_VERSION: u32 = 1;

// ── Primitives ──────────────────────────────────────────────────────

/// Semver version, strongly typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// A stream path, e.g. "/host/550e8400-..."
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamPath(pub String);

/// Message type discriminator — always an enum, never a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Hello,
    Welcome,
    Reject,
}

// ── Envelope ────────────────────────────────────────────────────────

/// Every message on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub stream: StreamPath,
    pub kind: FrameKind,
    pub seq: u64,
    pub payload: Value,
}

// ── Handshake payloads ──────────────────────────────────────────────

/// Client → Server: first message on a new connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    pub protocol_version: u32,
    pub tyde_version: Version,
    pub client_name: String,
    pub platform: String,
}

/// Server → Client: handshake accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomePayload {
    pub protocol_version: u32,
    pub tyde_version: Version,
    pub bootstrap: BootstrapData,
}

/// Bootstrap data sent with Welcome. Starts minimal, will grow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootstrapData {}

/// Server → Client: handshake rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectPayload {
    pub code: RejectCode,
    pub message: String,
    pub server_protocol_version: u32,
    pub server_tyde_version: Version,
}

/// Machine-readable rejection reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    IncompatibleProtocol,
    InvalidHandshake,
}
```

### Notes on the types

- `FrameKind` only contains handshake kinds for now. Post-handshake kinds will
  be added as features are designed (see decision log in `01-philosophy.md`).
- `BootstrapData` is an empty struct, not `Value`. It will grow with typed
  fields — never a bag of untyped JSON.
- `StreamPath` wraps `String` for type safety. It will gain validation
  (must start with `/`, valid segments) when we add stream creation logic.

---

## 7. Communication Model

The protocol is **events in, events out** — not request/response.

- The client sends events to the server (e.g. "send message to agent",
  "cancel operation"). These are fire-and-forget from the client's perspective.
- The server sends events to the client (e.g. "stream delta", "typing status
  changed", "agent registered"). The UI subscribes and renders based on what
  it receives.
- There are no request IDs and no response correlation. Streams are the
  correlation mechanism — events on the same stream are related.
- Both sides can send events on a stream at any time. There is no
  request→response pairing.

Post-handshake event kinds will be designed as features are added.

---

## 8. What's Not Here Yet

These are explicitly deferred until the features that need them:

- Post-handshake event kinds
- Stream creation and teardown protocol
- Per-stream error handling
