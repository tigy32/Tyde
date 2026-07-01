# Backend Access Mode

`BackendAccessMode` is the protocol-level switch that tells a backend whether a
new session should be treated as allowed to mutate state. It is separate from
`ToolPolicy`: tool policy is a backend-specific allow-list when a backend can
express one, while access mode is the cross-backend read/write intent.

Read-only mode is best-effort guidance. Tyde advises the agent not to mutate
source or external state, but it does **not** enforce an OS-level read-only
workspace. The backend is allowed to use writable workspace sandboxes or normal
tool permissions so build/test tools can write outputs such as `target/`. The
tradeoff is explicit: a read-only agent can technically write the workspace; it
is instructed not to create, edit, or delete source files.

## Protocol flow

`protocol::BackendAccessMode` has two values:

- `Unrestricted` (default): the backend may use its normal tools and CLI
  permissions.
- `ReadOnly`: the backend should treat the workspace as read-only. It may
  inspect code and state, including reading files, listing directories, and
  running shell commands needed for investigation and validation. It must not
  intentionally create, edit, or delete source files, use write/edit/apply-patch
  tools for source mutation, run destructive git commands, or modify external
  state.

The value is carried on `SpawnAgentParams::New` from the frontend or
agent-control MCP bridge into `HostHandle::spawn_agent`. The host resolves the
normal `ResolvedSpawnConfig`, copies the requested `access_mode` into that
resolved config, and passes it to the backend through `BackendSpawnConfig`.
Built-in spawns that already construct a `ResolvedSpawnConfig` directly, such as
the AI reviewer, set the field explicitly.

Resumes do not accept a new access mode. A resumed session uses the behavior
encoded by the backend/session being resumed.

## Shared read-only advisory

`render_combined_spawn_instructions` prepends a shared read-only advisory for
backends that consume combined spawn instructions. The advisory is intentionally
not "no shell": read-only inspection can require command-line investigation. It
therefore permits reading files, listing directories, and read-only shell
commands such as `git status`, `git log`, `git diff`, `grep`/`rg`, `cat`, `ls`,
and `find`, while forbidding file creation, edits, deletes, state-changing
commands, and write/edit/apply-patch tools.

Backend sandbox/permission choices are deliberately looser than that advisory so
build/test commands can run. The advisory is the model-facing control; the
sandbox is not a hard source-write blocker.

## Enforcement model

Read-only mode is advisory plus targeted Tyde-side rejection, not a universal
fail-closed contract:

- The shared advisory tells the model what is permitted and what is forbidden.
- Backend-native modes are chosen to allow workspace build/test writes rather
  than to hard-block every write.
- Tool allow-lists may still be used when a backend needs one to expose the
  right read/build tools.
- MCP tools are not globally classified by Tyde. For the AI reviewer, the Tyde
  agent-control MCP endpoint rejects mutating calls from read-only agents. Other
  MCP endpoints must reject unsafe mutation themselves or be omitted from a
  read-only configuration.

The important tradeoff is that read-only sessions can technically write inside
the workspace. Tyde relies on the advisory and the agent-control MCP rejection,
not an OS write-block, to keep the mode best-effort.

## Backend implementations

### Claude

Claude read-only uses a permissive non-plan mode plus the shared advisory:

- `BackendAccessMode::ReadOnly` maps to `--permission-mode acceptEdits` so Bash
  and build/test commands are not blocked by Claude plan mode.
- Tyde appends the shared read-only advisory to Claude's system prompt.
- The existing reviewer `ToolPolicy::AllowList` is still translated to Claude's
  `--allowedTools` flags.

Claude `plan` mode is not used for Tyde read-only because plan mode restricts
Bash to read-only exploration commands and blocks commands such as `cargo check`
that need to write build outputs. Unrestricted Claude sessions keep
`--permission-mode bypassPermissions` and the existing
`--dangerously-skip-permissions` behavior.

### Codex

Codex receives writable workspace mode everywhere the app-server protocol
exposes a sandbox knob:

- The subprocess is started as `codex --sandbox workspace-write app-server ...`.
- `thread/start` uses sandbox `workspace-write`.
- Turn requests use sandbox policy `{ "type": "workspaceWrite", ... }`.

Tyde also keeps the forced approval policy and prepends the shared read-only
advisory. This intentionally replaces Codex's hard `read-only` sandbox with a
workspace-write sandbox so commands such as `cargo check`, `cargo test`, and
`cargo clippy` can populate `target/`. It does not relax to Codex
`danger-full-access`; unrestricted mode remains the only path that uses that
sandbox.

### Tycode

Tycode is launched with a generated custom-agent spec in read-only mode. The
spec uses the shared read-only advisory and exposes read/build-oriented native
tools:

- `set_tracked_files`
- `search_types`
- `get_type_docs`
- `run_build_test`

`run_build_test` lets read-only agents run validation commands that write build
outputs. The generated read-only agent still omits direct source mutation tools
such as `write_file`, `modify_file`, and `delete_file`; Tyde relies on the
advisory for source-mutation behavior and on agent-control MCP rejection for
mutating Tyde MCP calls.

Tycode still exposes configured MCP tools through its MCP module. For the AI
reviewer, the Tyde agent-control MCP endpoint rejects mutating calls from
read-only agents, so only the reviewer read/list/await/comment tools remain
usable.

### Kiro / ACP

Kiro read-only uses ACP advisory behavior rather than ACP hard blocking:

- ACP `initialize` advertises filesystem reads, filesystem writes, and terminal
  access even for read-only sessions.
- `AcpBridge` no longer rejects filesystem write or terminal built-in requests
  solely because access mode is read-only.
- `session/request_permission` follows the normal permission selection path in
  read-only mode.

This lets ACP-backed agents run build/test commands and tools that need
workspace writes. The tradeoff is the same as the rest of read-only mode: ACP can
technically write the workspace, and Tyde relies on the advisory and MCP
mutating-tool rejection rather than a hard ACP write block.

### Antigravity

Antigravity has no known workspace-write middle mode: `agy --sandbox` is the hard
terminal-restricted mode, while non-interactive tool use requires skipping
permissions. Read-only Antigravity therefore receives the shared advisory in the
prompt and launches `agy` with `--dangerously-skip-permissions`, without
`--sandbox`, so build/test commands can run.

Unrestricted Antigravity sessions also pass `--dangerously-skip-permissions` so
headless print-mode turns do not block on interactive approvals. The difference
for read-only is the advisory, not an Antigravity sandbox.

### Hermes

Hermes read-only uses the shared advisory seeded into `session.create` as a
system history message. Tyde does not claim a hard Hermes sandbox or MCP/tool
policy mapping for read-only mode yet. If a custom agent requires startup MCP
servers, custom MCP servers, or a non-default tool policy, the Hermes backend
fails visibly instead of pretending those policies were applied.

### Mock

The mock backend records `access_mode` in its test session record and includes it
in mock summaries. Tests can assert that read-only mode reached the backend.

## AI reviewer

The AI reviewer sets:

- `access_mode: BackendAccessMode::ReadOnly`
- the existing reviewer `ToolPolicy::AllowList`

The server does not reject non-Claude reviewer backends. Any enabled backend may
be selected; the backend then applies its available best-effort read-only
advisory behavior. The Tyde agent-control MCP endpoint still rejects mutating
calls from read-only agents.
