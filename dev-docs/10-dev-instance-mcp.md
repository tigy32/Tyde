# Dev Instance MCP

Developer tooling for launching isolated Tyde2 desktop instances and inspecting
them from an external MCP client. This builds on:

- `01-philosophy.md`
- `02-protocol.md`
- `08-gui-shell-boundary.md`

---

## Problem

We need the same core capability the old app had:

- start a fresh Tyde desktop instance from code
- wait until it is actually ready
- inspect product state
- inspect and drive the rendered UI
- run multiple instances at once
- clean them up reliably

That capability is valuable for:

- Codex/Claude-driven implementation work
- interactive debugging of the desktop app
- visual verification and screenshot-based review
- automated integration tests that need a real desktop instance

The first Tyde had this, but it was built in the wrong place and with the wrong
ownership model.

---

## Legacy Design

These are the legacy reference points:

- `~/Tyde/src-tauri/src/dev_instance.rs`
- `~/Tyde/src-tauri/src/driver_mcp_http.rs`
- `~/Tyde/src-tauri/src/debug_mcp_http.rs`
- `~/Tyde/dev-docs/debug-mcp.md`

### What The Old Version Did

The old app had two MCP servers:

1. a parent-side "driver" MCP server
2. a child-instance "debug" MCP server

The parent-side driver MCP server exposed tools like:

- `tyde_dev_instance_start`
- `tyde_dev_instance_stop`
- `tyde_dev_instance_list`
- `tyde_debug_snapshot`
- `tyde_debug_events_since`
- `tyde_debug_evaluate`
- `tyde_debug_query_screenshot`

`tyde_dev_instance_start` did roughly this:

1. choose free ports
2. spawn `npx tauri dev`
3. inject env vars so the child instance started its own loopback debug MCP
   server
4. wait for the child `/healthz`
5. store process handles and proxy state in a registry
6. return an instance ID plus debug MCP URL

The child-instance debug MCP server exposed tools for:

- debug snapshot
- event log inspection
- DOM/UI inspection
- screenshots
- clicks, typing, keypress, scroll, wait
- raw JS evaluation

The parent driver then proxied MCP `tools/call` JSON-RPC requests into the
child debug MCP server and added a screenshot-question helper on top.

The old version also supported remote launching over SSH by:

- spawning the remote dev instance over SSH
- opening an SSH tunnel back to the child's debug MCP port
- treating the tunneled loopback URL as the child endpoint

### What Is Worth Preserving

The useful behaviors were:

- one tool call to start a real desktop instance
- multiple concurrent instances with explicit IDs
- ready/not-ready detection instead of blind sleeps
- loopback-only access
- DOM-level control through evaluation
- visual inspection through screenshots
- automatic cleanup tied to instance lifetime
- an external MCP surface that tools could discover and use

Those are the parts we should rebuild.

---

## What Went Wrong

The old implementation violated the rewrite philosophy in several concrete ways.

### Tauri Owned Too Much Behavior

The Tauri layer owned all of this:

- dev-instance lifecycle
- port selection
- process management
- remote SSH branching
- MCP server startup
- JSON-RPC proxy state
- app debug snapshots
- UI debug request routing

That is far beyond "deserialize, dispatch, serialize" or transport ownership.

### Product State Was Not Read Through The Real Protocol

The debug MCP server answered questions about the app by reaching directly into
Tauri-side state and helper functions.

That bypassed the real source of truth:

- `tyde-server` owned product behavior
- the frontend rendered protocol events

Instead of observing the same event stream the product uses, the debug path
invented a parallel introspection surface.

### The Child Ran Its Own MCP Server

The child instance exposed MCP directly, and the parent app then proxied MCP to
MCP.

That created:

- extra transport layers
- extra session state
- extra error cases
- extra handwritten JSON plumbing

The child does not need to understand MCP at all. Only the external tool
boundary needs MCP.

### The UI Debug Bridge Was Ad Hoc

Legacy UI debugging used string action names plus free-form `serde_json::Value`
payloads, then forwarded those through a Tauri event bridge into frontend code.

That is exactly the kind of parallel, weakly typed protocol the rewrite is
trying to remove.

### Local vs Remote Lived In The Wrong Layer

The old Tauri code contained explicit SSH launch and tunneling logic.

That violated the "local and remote are the same abstraction" rule. Launch
strategy belongs in the driver, not in the product shell.

### Dev Tool Settings Lived Inside The Product

The old app had settings and autoload toggles for the driver/debug MCP servers.

That made developer tooling a runtime behavior of the shipped app. The new
design should not do that.

---

## Tyde2 Design

The replacement should keep the external MCP experience while moving ownership
to the right places.

The key rule is:

**Tyde2 should have exactly one MCP server for dev instances: the external
driver. The child desktop instance should expose typed dev transports, not MCP.**

### High-Level Shape

```text
MCP client
    |
    v
tyde-dev-driver
    | \
    |  \__ UI debug transport
    |
    \_____ Tyde protocol transport

Tyde dev instance
    |- server::HostHandle
    |- frontend
    \- tauri-shell
```

### Three Layers

#### 1. `tyde-dev-driver`

A separate dev-only binary/crate. This is the only MCP server in the design.

It owns:

- MCP tool definitions
- instance lifecycle
- process and tunnel management
- multi-instance registry
- readiness checks
- screenshot-question helper
- derived debug event log

It does **not** own product behavior.

#### 2. Child Dev Instance Endpoints

The launched Tyde instance exposes two dev-only loopback endpoints:

- a host protocol endpoint
- a UI debug endpoint

The host protocol endpoint speaks the real Tyde wire protocol from
`protocol/src/types.rs`.

The UI debug endpoint speaks a separate typed devtools protocol for DOM/webview
operations only.

The child does **not** expose MCP.

#### 3. Existing Product Layers

- `server` remains the source of truth for product state
- `frontend` remains the UI that renders protocol events
- `tauri-shell` remains a transport shell

The dev-instance feature must not reintroduce app semantics into the shell.

---

## Ownership

### Server Ownership

Product state inspection must come from the real host protocol.

That means the driver should connect to the child instance the same way any
other client would:

- handshake
- use the normal `/host/<uuid>` connection stream
- observe typed output events

If the driver wants to know:

- current host settings
- known projects
- active agents
- terminals
- future server-owned state

it should learn that through the protocol and nothing else.

### Shell Ownership

`tauri-shell` may own:

- accepting a loopback dev host connection and wiring it into
  `server::accept`/`run_connection`
- exposing a loopback UI debug bridge
- passing typed debug requests between Rust and the webview

It may **not** own:

- MCP tools
- screenshot-question orchestration
- instance registries
- SSH logic
- product state snapshots

That keeps it within the "dumb transport shell" rule.

### Frontend Ownership

The frontend owns only webview-local UI behavior:

- DOM queries
- JS evaluation
- screenshots
- element interaction, if we keep it

Even here, the interface must be typed. No string action router and no
free-form payload maps.

---

## Protocols

There are two different protocols in this design. That is fine because they
serve different boundaries.

### 1. Product Protocol

This is the existing Tyde wire protocol in `protocol/`.

Use it unchanged for:

- host state
- agent state
- project state
- terminal state

The driver should reuse the `client` crate to connect to a dev instance's host
endpoint and observe the same frames the frontend observes.

### 2. UI Debug Protocol

Create a small dedicated crate for the child UI debug transport, for example:

- `devtools-protocol`

This crate is the source of truth for driver-to-instance UI debug messages.

Do **not** put these messages in `protocol/`, because they are not product
protocol frames.

Do **not** handwrite parallel JSON shapes in both ends.

Initial message set should stay minimal:

```rust
pub enum UiDebugRequest {
    Ping,
    Evaluate {
        expression: String,
        timeout_ms: Option<u64>,
    },
    CaptureScreenshot {
        max_dimension: Option<u32>,
    },
}

pub enum UiDebugResponse {
    Pong,
    Ready,
    EvaluateResult {
        value: serde_json::Value,
    },
    CaptureScreenshotResult {
        png_base64: String,
        width: u32,
        height: u32,
    },
    Error {
        message: String,
    },
}
```

That is intentionally narrow.

`Evaluate` is the main primitive. If callers need click/type/scroll/wait, they
can do it through evaluation first. We should not rebuild a large bespoke DOM
tool API unless there is a proven need.

---

## External MCP Surface

Keep the MCP boundary small and stable.

Recommended first-slice tools:

- `tyde_dev_instance_start`
- `tyde_dev_instance_stop`
- `tyde_dev_instance_list`
- `tyde_debug_events_since`
- `tyde_debug_snapshot`
- `tyde_debug_evaluate`
- `tyde_debug_query_screenshot`

### Why Keep These Names

These names already existed in the old workflow and match how external agents
want to use the system.

The important change is internal:

- MCP ends at the driver
- the child speaks typed internal transports
- product state comes from the real Tyde protocol

### Tool Semantics

#### `tyde_dev_instance_start`

Input:

- `project_dir`
- optional `workspace_path`
- optional `launch_target`

Output:

- `instance_id`
- `status`
- metadata needed for debugging

This tool:

1. reserves ports
2. spawns the instance
3. waits for both child endpoints to be ready
4. opens a host protocol connection
5. starts background event capture
6. returns only when the instance is actually usable

#### `tyde_debug_events_since`

Returns a driver-owned debug log with a monotonic debug sequence number.

This log should contain typed records for:

- instance lifecycle events
- raw Tyde protocol envelopes observed from the child host connection
- UI debug requests/responses
- launcher failures or disconnects

This replaces the old in-app debug event log.

#### `tyde_debug_snapshot`

This is a **derived** snapshot owned by the driver, not a child-instance RPC.

It should summarize:

- instance metadata
- process status
- endpoint readiness
- connection status
- last seen host stream info
- currently known projects/agents/terminals as derived from protocol events

No hidden child-side snapshot helper is needed.

#### `tyde_debug_evaluate`

This proxies to the child UI debug endpoint's `Evaluate` request.

#### `tyde_debug_query_screenshot`

This should remain a driver-side helper:

1. capture screenshot through the child UI debug endpoint
2. ask a model the visual question
3. return the answer

The child instance should not know anything about this workflow.

---

## Instance Lifecycle

The driver should own lifecycle in a single actor, for example
`InstanceRegistry`.

### Registry State

```rust
pub struct DevInstanceRecord {
    pub instance_id: u64,
    pub project_dir: PathBuf,
    pub launch_target: LaunchTarget,
    pub frontend_port: u16,
    pub host_port: u16,
    pub ui_debug_port: u16,
    pub process: ChildHandle,
    pub tunnel: Option<ChildHandle>,
    pub host_client: HostClientHandle,
    pub ui_client: UiDebugClient,
}
```

Use an actor, not shared mutable state scattered behind unrelated locks.

### Launch Target

Define the abstraction now:

```rust
pub enum LaunchTarget {
    Local,
    Ssh { host: String },
}
```

The initial implementation can support only `Local`, but the type should exist
from day one so remote support does not leak into higher layers later.

### Launch Flow

`tyde_dev_instance_start` should do this:

1. allocate a new `instance_id`
2. choose a frontend dev port, host port, and UI debug port
3. build one canonical launcher command for this repo
4. pass only launch config through env/config overrides
5. spawn the child in its own process group
6. wait for host endpoint readiness
7. wait for UI debug endpoint readiness
8. open the host protocol connection with the `client` crate
9. start background event capture
10. register the instance in the actor

### Stop Flow

`tyde_dev_instance_stop` should:

1. remove the instance from the registry
2. stop background tasks
3. kill tunnel process if present
4. kill the full child process group
5. return a typed stopped result

No in-app cleanup hooks are required for the driver itself.

---

## Child Dev Endpoints

These endpoints are enabled only when the instance is launched in dev-instance
mode.

Use env vars or a generated config file at launch time. Do not persist these as
app settings.

Recommended env shape:

- `TYDE_DEV_INSTANCE=1`
- `TYDE_DEV_HOST_BIND_ADDR=127.0.0.1:<port>`
- `TYDE_DEV_UI_DEBUG_BIND_ADDR=127.0.0.1:<port>`
- `TYDE_DEV_OPEN_WORKSPACE=<path>` when needed

### Host Endpoint

This should be implemented in `tauri-shell` as a loopback listener that accepts
raw stream connections and passes them into the same `server::HostHandle` used
by the GUI.

This is transport work, so it belongs in the shell.

It must:

- bind loopback only
- reject non-loopback bind requests
- expose no extra semantics beyond the real Tyde protocol

### UI Debug Endpoint

This is also loopback-only and dev-only.

It exists because DOM inspection is not part of the Tyde product protocol.

It should:

- accept typed `UiDebugRequest` messages
- return typed `UiDebugResponse` messages
- be implemented with one request actor
- use request IDs only if the transport needs multiplexing

The shell may pass requests into the webview, but the message boundary must
stay typed end-to-end.

---

## Launcher Command

The old version hardcoded `npx tauri dev`. We should not repeat that as
scattered string concatenation.

The new driver should have one repo-specific launcher module that owns the exact
command shape.

For this repo, the launcher must account for:

- `frontend/tauri-shell/tauri.conf.json`
- `beforeDevCommand`
- `devUrl`
- the frontend dev server port

That likely means one of these approaches:

1. invoke the Tauri dev command with a generated config override that rewrites
   both `beforeDevCommand` and `devUrl`
2. run the frontend dev server and shell process directly in a controlled way

Either is acceptable. The important rule is:

**one code path, in the driver, with explicit inputs.**

Do not spread launcher assumptions across the shell, frontend, and driver.

---

## What We Should Not Rebuild

Do **not** rebuild these legacy choices:

- an MCP server inside the child dev instance
- debug/dev toggles persisted in Tyde app settings
- MCP-to-MCP proxying
- ad hoc JSON action names for UI debug requests
- product-state snapshots read from Tauri internals
- SSH branching inside `tauri-shell`
- large bespoke click/type/scroll APIs before `Evaluate` proves insufficient

These are the exact kinds of mistakes the rewrite is trying to remove.

---

## Rollout Plan

### Phase 1: Local-only, Correct Shape

1. Add `devtools-protocol` with typed UI debug messages.
2. Add a loopback host endpoint in `frontend/tauri-shell` that wires external
   connections into `server::accept` and `server::run_connection`.
3. Add a loopback UI debug endpoint in `frontend/tauri-shell` plus a typed
   frontend bridge.
4. Build `tyde-dev-driver` as the only MCP server.
5. Implement lifecycle tools plus:
   - `events_since`
   - `snapshot`
   - `evaluate`
   - `query_screenshot`
6. Add integration tests that start a real instance and assert readiness.

### Phase 2: Remote Launch

Add `LaunchTarget::Ssh` in the driver only.

Remote support should reuse the same public MCP tools and the same child
endpoint contracts. The only difference is how the driver starts the process and
reaches the loopback ports.

### Phase 3: Broader UI Helpers

Only if `Evaluate` is not enough, add typed higher-level UI requests such as:

- `WaitForSelector`
- `CaptureElementScreenshot`

Do this based on actual usage, not preemptively.

---

## Tests

Minimum coverage for the rebuild:

- `devtools-protocol` round-trip serialization tests
- shell host endpoint integration test using the real `client` crate handshake
- shell UI debug endpoint smoke test (`Ping`, `Evaluate`)
- driver lifecycle test: start, list, stop
- multi-instance routing test with distinct `instance_id`s
- failure test when child process exits before readiness
- failure test when a requested bind address is non-loopback

The important check is architectural, not just behavioral:

- product state must come through the Tyde protocol
- UI debug must come through the typed devtools protocol
- MCP must exist only at the external driver boundary

---

## Summary

The old dev-instance MCP had the right user-facing capability but the wrong
architecture.

Tyde2 should rebuild it like this:

- one external MCP server: `tyde-dev-driver`
- no MCP server inside the child app
- product-state inspection through the real Tyde protocol
- UI inspection through a small typed devtools protocol
- lifecycle, remote/local launch, and screenshot helpers owned by the driver
- `tauri-shell` kept as a transport shell instead of becoming a second backend

That preserves the power of the legacy workflow without reintroducing the same
fundamental mistakes.
