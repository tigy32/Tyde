# Host Actor & Agent Registry

Server-side components that own agent lifecycle and enable multiple
connections to share the same set of agents. Builds on `03-agents.md`.

---

## Problem

Today `run_connection` directly spawns agent actors and owns them in a
local `HashMap<AgentId, mpsc::Sender<AgentInput>>`. Each connection has
its own isolated set of agents. A second client connecting to the same
server cannot see or interact with agents spawned by the first client.

The fix: a **Host actor** that owns agent lifecycle, shared across all
connections.

---

## Host Actor

Single tokio task (actor pattern). Owns:
- The `AgentRegistry` (all agent state and handles)
- A list of connected subscribers (each with their host stream + output channel)

Connections send commands to the Host via `mpsc`. The Host processes them
sequentially — no concurrent mutation, no locks.

### HostHandle

Cloneable handle that connections use to talk to the Host:

```rust
// server/src/host.rs

#[derive(Clone)]
pub struct HostHandle {
    tx: mpsc::Sender<HostCommand>,
}
```

### HostCommand

```rust
pub(crate) enum HostCommand {
    /// A new connection wants to subscribe to host events and existing agents.
    /// Host auto-subscribes the connection to all running agents and sends
    /// replay data through the oneshot.
    Subscribe {
        host_stream: StreamPath,
        output_tx: mpsc::Sender<OutgoingFrame>,
        reply: oneshot::Sender<SubscribeResult>,
    },

    /// A connection requests spawning a new agent.
    /// Host creates the agent, registers it, and fans out NewAgent + AgentStart
    /// to ALL subscribers (including the requester) via their output channels.
    /// No oneshot reply needed — the requester learns about the agent the same
    /// way everyone else does: through the NewAgent event on their host stream.
    SpawnAgent {
        payload: SpawnAgentPayload,
    },

    /// A connection sends input to an existing agent.
    SendInput {
        agent_id: AgentId,
        input: AgentInput,
        /// Where to send an error frame if the agent is dead.
        error_tx: mpsc::Sender<OutgoingFrame>,
        error_stream: StreamPath,
    },

    /// Agent actor produced an output frame. Host logs it and fans out
    /// to all subscribers (re-stamping stream paths per subscriber).
    AgentOutput {
        agent_id: AgentId,
        frame: AgentFrame,
    },
}
```

### AgentFrame

A frame from an agent before stream-path stamping. The agent actor
doesn't know about per-connection instance streams — it just emits
kind + payload. The Host stamps the stream per subscriber on fanout.

```rust
pub(crate) struct AgentFrame {
    pub kind: FrameKind,
    pub payload: serde_json::Value,
}
```

### SubscribeResult

Returned via oneshot so the connection can replay existing agents:

```rust
pub(crate) struct SubscribeResult {
    pub agents: Vec<AgentSubscription>,
}

pub(crate) struct AgentSubscription {
    pub agent_id: AgentId,
    pub stream: StreamPath,
    pub replay: Vec<OutgoingFrame>,
}
```

### HostHandle methods

```rust
impl HostHandle {
    pub async fn subscribe(
        &self,
        host_stream: StreamPath,
        output_tx: mpsc::Sender<OutgoingFrame>,
    ) -> SubscribeResult { ... }

    pub async fn spawn_agent(&self, payload: SpawnAgentPayload) { ... }

    pub async fn send_input(
        &self,
        agent_id: AgentId,
        input: AgentInput,
        error_tx: mpsc::Sender<OutgoingFrame>,
        error_stream: StreamPath,
    ) { ... }
}
```

`subscribe` and `spawn_agent` panic if the Host is dead — fatal.
`send_input` is fire-and-forget (errors go on the error_stream).

### Host actor loop

```rust
pub fn spawn_host() -> HostHandle {
    let (tx, mut rx) = mpsc::channel::<HostCommand>(256);

    tokio::spawn(async move {
        let mut registry = AgentRegistry::new();
        // Each subscriber: (host_stream, output_tx, agent_streams)
        // agent_streams maps AgentId → StreamPath for this subscriber
        let mut subscribers: Vec<Subscriber> = Vec::new();

        while let Some(cmd) = rx.recv().await {
            match cmd {
                HostCommand::Subscribe { host_stream, output_tx, reply } => {
                    // For each active agent:
                    //   1. Allocate fresh instance_id
                    //   2. Build stream path /agent/<agent_id>/<instance_id>
                    //   3. Clone event log, re-stamp stream paths
                    //   4. Register subscriber for this agent
                    // Return replay data via oneshot
                }
                HostCommand::SpawnAgent { payload } => {
                    // 1. registry.spawn(payload) -> (agent_id, start)
                    // 2. For each subscriber:
                    //    a. Allocate instance_id, build stream path
                    //    b. Send NewAgent on their host_stream via output_tx
                    //    c. Register subscriber for this agent's fanout
                    // AgentStart is emitted by the agent actor itself
                    // and arrives as AgentOutput -> gets fanned out normally
                }
                HostCommand::SendInput { agent_id, input, error_tx, error_stream } => {
                    // Forward to the agent's input channel
                    // If agent is dead, emit AgentError on error_stream
                }
                HostCommand::AgentOutput { agent_id, frame } => {
                    // 1. Append to event log (canonical, stream-agnostic)
                    // 2. For each subscriber of this agent:
                    //    Stamp with their instance stream path, send via output_tx
                    // 3. Dead subscribers (send fails) get removed
                }
            }
        }
    });

    HostHandle { tx }
}
```

### Lifetime

One Host per server process. Created before the accept loop, cloned to
each connection handler.

---

## AgentRegistry

Lives in `server/src/agent/registry.rs`. Plain struct owned by Host.

```rust
pub(crate) struct AgentRegistry {
    agents: HashMap<AgentId, AgentEntry>,
}

struct AgentEntry {
    input_tx: mpsc::Sender<AgentInput>,
    start: AgentStartPayload,
    event_log: Vec<AgentFrame>,
}
```

### Registry API

```rust
impl AgentRegistry {
    pub fn new() -> Self { ... }

    /// Spawn a new agent. Allocates AgentId, creates agent actor,
    /// returns (AgentId, AgentStartPayload).
    /// The host_tx is the Host's own command channel — the agent actor
    /// sends AgentOutput commands through it.
    pub fn spawn(
        &mut self,
        payload: SpawnAgentPayload,
        host_tx: mpsc::Sender<HostCommand>,
    ) -> (AgentId, AgentStartPayload) { ... }

    /// Send input to an agent. Returns false if the agent is dead.
    pub async fn send_input(&self, agent_id: &AgentId, input: AgentInput) -> bool { ... }

    /// Append a frame to an agent's event log.
    pub fn log_frame(&mut self, agent_id: &AgentId, frame: AgentFrame) { ... }

    /// Get the event log for replay.
    pub fn event_log(&self, agent_id: &AgentId) -> &[AgentFrame] { ... }

    /// List active agent IDs.
    pub fn list_agents(&self) -> Vec<&AgentId> { ... }

    /// Get the AgentStartPayload for an agent.
    pub fn agent_start(&self, agent_id: &AgentId) -> &AgentStartPayload { ... }
}
```

### How spawn works

1. Generate `agent_id` (UUID).
2. Build `AgentStartPayload` from the `SpawnAgentPayload`.
3. Spawn the agent actor with `host_tx` as its output channel.
   The agent sends `HostCommand::AgentOutput { agent_id, frame }` for
   every frame it produces (AgentStart, ChatEvent, AgentError).
4. Store `AgentEntry { input_tx, start, event_log: vec![] }`.
5. Return `(agent_id, start)`.

---

## Agent Actor Changes

The agent actor's output_tx changes from `mpsc::Sender<OutgoingFrame>`
to `mpsc::Sender<HostCommand>`. It wraps every frame in
`HostCommand::AgentOutput`:

```rust
pub(crate) fn spawn_agent_actor(
    agent_id: AgentId,
    start: AgentStartPayload,
    initial_prompt: String,
    host_tx: mpsc::Sender<HostCommand>,
) -> mpsc::Sender<AgentInput> {
    // ...
    // Emit AgentStart:
    let frame = AgentFrame::from_payload(FrameKind::AgentStart, &start);
    host_tx.send(HostCommand::AgentOutput { agent_id, frame }).await;

    // Emit ChatEvents from backend:
    let frame = AgentFrame::from_payload(FrameKind::ChatEvent, &event);
    host_tx.send(HostCommand::AgentOutput { agent_id, frame }).await;
}
```

The agent no longer knows about StreamPath or instance IDs. It just
emits frames tagged with its agent_id. The Host handles stream routing.

---

## NewAgent Host Event

When a new agent is created, all connected clients are notified.
This is a **host-level event** on each connection's `/host/<uuid>` stream.

### New FrameKind

```rust
FrameKind::NewAgent,  // host stream, server → client
```

### NewAgentPayload

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAgentPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub parent_agent_id: Option<AgentId>,
    pub created_at_ms: u64,
    /// The agent stream path for THIS subscriber.
    /// Each connection gets a unique instance_id.
    pub instance_stream: StreamPath,
}
```

### Flow

```
Client A                    Host                    Client B
  │                          │                          │
  │── SpawnAgent ───────────→│                          │
  │                          │  creates agent           │
  │                          │                          │
  │←── NewAgent ────────────│──── NewAgent ───────────→│
  │    (on /host/A)          │    (on /host/B)          │
  │    instance_stream:      │    instance_stream:      │
  │    /agent/<id>/<instA>   │    /agent/<id>/<instB>   │
  │                          │                          │
  │←── AgentStart ──────────│──── AgentStart ─────────→│
  │    (on instance stream)  │    (on instance stream)  │
  │                          │                          │
  │←── ChatEvent ───────────│──── ChatEvent ──────────→│
```

### Why both NewAgent and AgentStart?

- `NewAgent` is host-level notification: "a new agent exists, here's
  your stream." Goes on the host stream where the client already listens.
- `AgentStart` is the agent-stream birth certificate: always seq 0,
  immutable metadata, part of the replay log.

### Late-joining client

When a new connection subscribes, the Host sends a `NewAgent` frame on
the host stream for each active agent, then the connection replays
that agent's event log on the agent's instance stream. The late-joiner
sees the exact same events that the first client saw.

---

## Connection Changes

`run_connection` takes a `HostHandle` parameter. It no longer owns agents.

```rust
pub async fn run_connection(
    mut connection: Connection,
    host: HostHandle,
) -> Result<(), FrameError> {
    let host_stream = /* from connection.outgoing_seq */;
    let (output_tx, mut output_rx) = mpsc::channel::<OutgoingFrame>(256);

    // Subscribe to existing agents
    let sub = host.subscribe(host_stream.clone(), output_tx.clone()).await;
    for agent in sub.agents {
        // Send NewAgent on host stream
        // Replay event log on agent's instance stream
    }

    loop {
        tokio::select! {
            // Outgoing: Host fanout → wire
            maybe_outgoing = output_rx.recv() => { ... }

            // Incoming: wire → Host
            incoming = read_envelope(&mut connection.reader) => {
                match envelope.kind {
                    FrameKind::SpawnAgent => {
                        host.spawn_agent(payload).await;
                        // NewAgent comes back through output_rx
                    }
                    FrameKind::SendMessage => {
                        // Validate instance_id (per-connection auth)
                        host.send_input(agent_id, input, ...).await;
                    }
                }
            }
        }
    }
}
```

### instance_id tracking

Connections still validate that a `SendMessage` targets an instance_id
that was issued to this connection. The connection learns about
instance_ids from:
1. `NewAgent` events (which contain `instance_stream`)
2. Replay data from `subscribe()`

---

## Subscriber cleanup

When a connection drops, its `output_tx` is dropped. The Host detects
failed sends during fanout and removes dead subscribers. No explicit
unregister command needed.

---

## File Structure

```
server/src/
├── lib.rs              # re-exports, ServerConfig, Connection
├── acceptor.rs         # accept(), listen_uds() — takes HostHandle
├── connection.rs       # run_connection(conn, host) — thin dispatcher
├── host.rs             # NEW: Host actor, HostHandle, HostCommand, spawn_host()
├── agent/
│   ├── mod.rs          # spawn_agent_actor() — outputs to Host channel
│   └── registry.rs     # NEW: AgentRegistry, AgentEntry
└── backend/
    ├── mod.rs          # Backend trait, EventStream (unchanged)
    └── mock.rs         # MockBackend (unchanged)

protocol/src/
└── types.rs            # + FrameKind::NewAgent, + NewAgentPayload
```

---

## Protocol Types Summary

New additions to `protocol/src/types.rs`:

```rust
FrameKind::NewAgent,

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAgentPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub parent_agent_id: Option<AgentId>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
}
```
