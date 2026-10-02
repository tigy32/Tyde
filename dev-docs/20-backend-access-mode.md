# Backend Access Mode

`BackendAccessMode` is the protocol-level access contract for a session. It is
separate from `ToolPolicy`, the backend-specific tool allowance. The existing
`ReadOnly` mode remains advisory; `EnforcedReadOnly` is a distinct native
restriction used by read-only swarms. The two must never be substituted.

Advisory read-only mode is guidance only. Tyde advises the agent not to mutate source or
external state, but it does not reduce sandbox permissions, remove tools, or
reject MCP operations. A read-only agent has the same effective capabilities as
an unrestricted agent and is instructed not to use them for mutation.

## Protocol flow

`protocol::BackendAccessMode` has three values:

- `Unrestricted` (default): the backend may use its normal tools and CLI
  permissions.
- `ReadOnly`: the backend should treat the workspace as read-only. It may
  inspect code and state, including reading files, listing directories, and
  running shell commands needed for investigation and validation. It must not
  intentionally create, edit, or delete source files, use write/edit/apply-patch
  tools for source mutation, run destructive git commands, or modify external
  state.
- `EnforcedReadOnly`: require backend-native filesystem/tool restrictions.
  The host rejects a backend that does not declare this capability. Adding an
  advisory or hiding a UI action is not enforcement.

The value is carried on `SpawnAgentParams::New` from the frontend or
agent-control MCP bridge into `HostHandle::spawn_agent`. The host resolves the
normal `ResolvedSpawnConfig`, copies the requested `access_mode` into that
resolved config, and passes it to the backend through `BackendSpawnConfig`.
Built-in spawns that already construct a `ResolvedSpawnConfig` directly, such as
the AI reviewer, set the field explicitly.

Ordinary resume controls do not accept a new access mode. The server reapplies
the resolved policy at the native lifecycle boundary. Swarm recovery and
reviewed legacy conversion retain membership ownership and reapply the swarm's
workspace policy before admitting work; unrestricted native history must not
silently override that policy.

## Shared read-only advisory

`render_combined_spawn_instructions` prepends a shared read-only advisory for
backends that consume combined spawn instructions. The advisory is intentionally
not "no shell": read-only inspection can require command-line investigation. It
therefore permits reading files, listing directories, and read-only shell
commands such as `git status`, `git log`, `git diff`, `grep`/`rg`, `cat`, `ls`,
and `find`, while forbidding file creation, edits, deletes, state-changing
commands, and write/edit/apply-patch tools.

For `ReadOnly`, sandbox, permission, and tool choices are identical to
unrestricted mode. The advisory is its only access-mode difference.

## Enforcement model

`ReadOnly` is entirely advisory:

- The shared advisory tells the model what is permitted and what is forbidden.
- Backend-native permissions and sandboxes are the same as unrestricted mode.
- Access mode does not add a tool allow-list or remove tools.
- Tyde MCP endpoints do not reject operations based on access mode.

Independent authorization, ownership, and `ToolPolicy` checks still apply; they
are not read-only enforcement.

### Enforced read-only swarms

Read-only swarm admission requires both native enforced-read-only support and
native agent-delegation exclusion. Claude and Codex implement those contracts;
other backends fail admission rather than receiving a weaker substitute.

- Claude restricts the native tool catalog, withholds shell and file-mutation
  tools, disables ambient hooks/settings, and rejects other tool permissions.
- Codex applies its native read-only sandbox and approval policy at process,
  start, fork, resume, and turn boundaries. Code-execution kernels and native
  delegation remain disabled; supported native read tools are exposed directly
  rather than through a disabled wrapper. Native web search remains available;
  filesystem read-only access is not a promise of network isolation.
- Swarm members receive the four authenticated board tools, not the user's
  other configured MCP servers. Server-side ownership and method admission
  enforce the same boundary even for calls omitted from discovery.
- Writable swarm work requires explicit consent and an existing Git workbench.
  It is a separate policy, not a relaxation of an active read-only session.

#### Native command visibility

A denied command is still an actual tool attempt. Codex can reject an
`exec_command` before emitting a typed execution-start event. Fresh sessions
recover the actual call and terminal result from native raw-response events,
preserving the native call ID and exactly one tool card.

Installed Codex runtimes without raw-response subscriptions for protected
Fork/Resume use the owning process's native execution trace as the sole model
narrative source. This is native protocol evidence, not transcript inference.
Bounded, no-follow reads validate source identities, sequence continuity,
payload references, and the exact native turn-end fence. Original output-item
order anchors actual runtime receipts; typed execution remains their owner.
Missing or corrupt observation fails visibly without inventing an outcome or
losing the actual terminal receipt.

Private narrative can arrive at native inference boundaries on that transport.
Shared board posts remain live server events and are never copied from private
final messages. An unavailable trace transport is an explicit lifecycle error;
a remote Tyde host running its CLI locally uses the same server-owned path.

## Advisory backend implementations

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
