# Backend Access Mode

`BackendAccessMode` is the protocol-level switch that tells a backend whether a
new session should be allowed to mutate state. It is separate from
`ToolPolicy`: tool policy is a backend-specific allow-list when a backend can
express one, while access mode is the cross-backend read/write intent.

Read-only mode is best-effort. Tyde advises the agent not to mutate state and
uses backend-native or OS-level controls where they exist, but Tyde does not
maintain a global catalog of every tool or MCP method that can write. Backends
that can hard-block writes should do so; backends with coarser permission models
use the safest available mode and the shared advisory.

## Protocol flow

`protocol::BackendAccessMode` has two values:

- `Unrestricted` (default): the backend may use its normal tools and CLI
  permissions.
- `ReadOnly`: the backend should treat the workspace as read-only. It may
  inspect code and state, including reading files, listing directories, and
  running read-only shell commands where the backend exposes shell access. It
  must not create, edit, or delete files, use write/edit/apply-patch tools, or
  run commands that modify files, processes, or external state.

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
not "no shell": for Codex, Tycode, and Antigravity, read-only inspection can be
the only practical way to investigate a repository. It therefore permits
reading files, listing directories, and read-only shell commands such as
`git status`, `git log`, `git diff`, `grep`/`rg`, `cat`, `ls`, and `find`,
while forbidding file creation, edits, deletes, state-changing commands, and
write/edit/apply-patch tools.

Claude does not use this helper for read-only behavior; it uses its native
permission mode.

## Enforcement model

Read-only mode is advise-and-enforce-where-available, not a universal
fail-closed contract:

- The shared advisory tells the model what is permitted and what is forbidden.
- Backend-native controls and OS sandboxes are kept where available.
- Tool allow-lists are still used when a backend supports them.
- MCP tools are not globally classified by Tyde. For the AI reviewer, the Tyde
  agent-control MCP endpoint rejects mutating calls from read-only agents. Other
  MCP endpoints must reject unsafe mutation themselves or be omitted from a
  read-only configuration.

## Backend implementations

### Claude

Claude receives both native layers:

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

Tyde also keeps the forced approval policy and prepends the shared read-only
advisory. The advisory allows read-only shell inspection because Codex commonly
uses shell commands such as `cat`, `rg`, `find`, and `git diff` to read code;
the `--sandbox read-only` layer remains the OS-level write-blocking safety net.

### Tycode

Tycode is launched with a generated custom-agent spec in read-only mode. The
spec uses the shared read-only advisory and restricts native tools to the
read-side Tycode tools:

- `set_tracked_files`
- `search_types`
- `get_type_docs`

Tycode still exposes configured MCP tools through its MCP module. For the AI
reviewer, the Tyde agent-control MCP endpoint rejects mutating calls from
read-only agents, so only the reviewer read/list/await/comment tools remain
usable. Tyde does not add a Tycode shell tool in read-only mode.

### Kiro / ACP

Kiro uses ACP capability negotiation and server-side rejection. This path is
stricter than the shared advisory because ACP terminal access is not currently
split into read-only and mutating commands:

- ACP `initialize` advertises filesystem reads, disables filesystem writes, and
  disables terminal access for read-only sessions.
- `AcpBridge` rejects mutating built-in requests such as `fs/write_text_file`
  and terminal create/kill/release when access mode is read-only.
- `session/request_permission` is answered as cancelled in read-only mode.

Enabling read-only terminal commands for ACP would require a separate, safe
capability design.

### Antigravity

Antigravity read-only spawns receive the shared advisory in the prompt and
launch `agy` with `--sandbox`. They do not pass
`--dangerously-skip-permissions`; a known `agy` bug can bypass the sandbox if
those flags are combined.

Unrestricted Antigravity sessions pass `--dangerously-skip-permissions` so
headless print-mode turns do not block on interactive approvals.

### Mock

The mock backend records `access_mode` in its test session record and includes it
in mock summaries. Tests can assert that read-only mode reached the backend.

## AI reviewer

The AI reviewer sets:

- `access_mode: BackendAccessMode::ReadOnly`
- the existing reviewer `ToolPolicy::AllowList`

The server does not reject non-Claude reviewer backends. Any enabled backend may
be selected; the backend then applies its available read-only controls and
advisory behavior.
