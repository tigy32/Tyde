# Backend Access Mode

`BackendAccessMode` is the protocol-level switch that adds read/write guidance
to a new session. It is separate from `ToolPolicy`: tool policy is a
backend-specific allow-list when a backend can express one, while access mode
changes instructions only.

Read-only mode is guidance only. Tyde advises the agent not to mutate source or
external state, but it does not reduce sandbox permissions, remove tools, or
reject MCP operations. A read-only agent has the same effective capabilities as
an unrestricted agent and is instructed not to use them for mutation.

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

Backend sandbox, permission, and tool choices are identical to unrestricted
mode. The advisory is the only behavior access mode changes.

## Enforcement model

Read-only mode is entirely advisory:

- The shared advisory tells the model what is permitted and what is forbidden.
- Backend-native permissions and sandboxes are the same as unrestricted mode.
- Access mode does not add a tool allow-list or remove tools.
- Tyde MCP endpoints do not reject operations based on access mode.

Independent authorization, ownership, and `ToolPolicy` checks still apply; they
are not read-only enforcement.

## Backend implementations

### Claude

Claude read-only uses unrestricted permissions plus the shared advisory:

- `BackendAccessMode::ReadOnly` maps to `--permission-mode bypassPermissions`,
  exactly like unrestricted mode.
- Tyde appends the shared read-only advisory to Claude's system prompt.
- The existing reviewer `ToolPolicy::AllowList` is still translated to Claude's
  `--allowedTools` flags.

Claude `plan` mode is not used for Tyde read-only because access mode must not
change actual capabilities.

### Codex

Codex receives unrestricted mode everywhere the app-server protocol exposes a
sandbox knob:

- The subprocess is started as `codex --sandbox danger-full-access app-server ...`.
- `thread/start` and turn requests use `dangerFullAccess`.

Tyde keeps the forced approval policy and prepends the shared read-only
advisory. The advisory is the only difference from unrestricted mode.

### Tycode

Tycode receives the shared advisory through its projected steering file. It
keeps the same root-agent selection, native tools, and configured MCP tools as
an unrestricted session.

### ACP

ACP read-only uses ACP advisory behavior rather than ACP hard blocking:

- ACP `initialize` advertises filesystem reads, filesystem writes, and terminal
  access even for read-only sessions.
- `AcpBridge` no longer rejects filesystem write or terminal built-in requests
  solely because access mode is read-only.
- `session/request_permission` follows the normal permission selection path in
  read-only mode.

The shared advisory is the only access-mode difference.

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
system history message. Startup and custom MCP servers are loaded normally. A
non-default custom tool policy remains unsupported and fails visibly instead of
pretending the policy was applied.

### Mock

The mock backend records `access_mode` in its test session record and includes it
in mock summaries. Tests can assert that read-only mode reached the backend.

## AI reviewer

The AI reviewer sets:

- `access_mode: BackendAccessMode::ReadOnly`
- the existing reviewer `ToolPolicy::AllowList`

The server does not reject non-Claude reviewer backends. Any enabled backend may
be selected; the backend adds the read-only advisory. The reviewer's separate
`ToolPolicy::AllowList` remains independently enforced where supported.
