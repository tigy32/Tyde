# Backend Access Mode

`BackendAccessMode` is the protocol-level switch that tells a backend whether a
new session may mutate state. It is separate from `ToolPolicy`: tool policy is a
backend-specific allow-list when a backend can express one, while access mode is
the cross-backend contract that every backend must either honor or reject.

## Protocol flow

`protocol::BackendAccessMode` has two values:

- `Unrestricted` (default): the backend may use its normal tools and CLI
  permissions.
- `ReadOnly`: the backend must not edit/write files, run shell commands, or
  perform other state-changing work outside the agent's own message stream.

The value is carried on `SpawnAgentParams::New` from the frontend or
agent-control MCP bridge into `HostHandle::spawn_agent`. The host resolves the
normal `ResolvedSpawnConfig`, copies the requested `access_mode` into that
resolved config, and passes it to the backend through `BackendSpawnConfig`.
Built-in spawns that already construct a `ResolvedSpawnConfig` directly, such as
the AI reviewer, set the field explicitly.

Resumes do not accept a new access mode. A resumed session uses the behavior
encoded by the backend/session being resumed.

## Fail-closed rule

A backend must never silently downgrade `ReadOnly` to `Unrestricted`. If Tyde
cannot configure a backend so that read-only mode is honored, the spawn or turn
fails with a backend-specific error explaining the unsupported read-only case.

For MCP tools, Tyde does not maintain a global read-vs-write catalog. The AI
reviewer allow-list in `server/src/review/reviewer.rs` defines the MCP calls
that are read-only for the current reviewer use case. Other MCP tools are
considered mutating unless the serving MCP endpoint rejects them safely.

## Backend implementations

### Claude

Claude receives both layers:

- `BackendAccessMode::ReadOnly` maps to `--permission-mode plan`.
- The existing reviewer `ToolPolicy::AllowList` is still translated to
  Claude's `--allowedTools` flags.

Unrestricted Claude sessions keep `--permission-mode bypassPermissions` and the
existing `--dangerously-skip-permissions` behavior.

### Codex

Codex receives read-only mode in every place the app-server protocol exposes a
sandbox knob:

- The subprocess is started as `codex --sandbox read-only app-server ...`.
- `thread/start` uses sandbox `read-only`.
- Turn requests use sandbox policy `{ "type": "readOnly", ... }`.

Tyde also prepends a read-only system instruction via the combined spawn
instructions so the model does not waste turns attempting edits or shell
commands.

### Tycode

Tycode is launched with a generated custom-agent spec in read-only mode. The spec
uses the combined read-only system instruction and restricts native tools to the
read-side Tycode tools:

- `set_tracked_files`
- `search_types`
- `get_type_docs`

Tycode still exposes configured MCP tools through its MCP module. For the AI
reviewer, the Tyde agent-control MCP endpoint rejects mutating calls from
read-only agents, so only the reviewer read/list/await/comment tools remain
usable.

### Kiro / ACP

Kiro uses ACP capability negotiation and server-side rejection:

- ACP `initialize` advertises filesystem reads, disables filesystem writes, and
  disables terminal access for read-only sessions.
- `AcpBridge` rejects mutating built-in requests such as `fs/write_text_file`
  and terminal create/kill/release when access mode is read-only.
- `session/request_permission` is answered as cancelled in read-only mode.

This makes read-only fail closed even if an ACP server asks the client to perform
an operation after capabilities were negotiated.

### Antigravity

Antigravity CLI does not expose a documented, enforceable read-only mode for
Tyde to rely on. `agy --help` exposes `--sandbox`, but that flag is not a Tyde
read-only contract and must not be treated as equivalent to
`BackendAccessMode::ReadOnly`.

Antigravity read-only spawns therefore fail closed before launching `agy` with a
clear backend error. Unrestricted Antigravity sessions pass
`--dangerously-skip-permissions` so headless print-mode turns do not block on
interactive approvals.

### Mock

The mock backend records `access_mode` in its test session record and includes it
in mock summaries. Tests can assert that read-only mode reached the backend.

## AI reviewer

The AI reviewer now sets:

- `access_mode: BackendAccessMode::ReadOnly`
- the existing reviewer `ToolPolicy::AllowList`

The server no longer rejects non-Claude reviewer backends. Any enabled backend
may be selected; if that backend cannot honor read-only mode for the requested
spawn, the backend reports a typed startup/turn error instead of running
unrestricted.
