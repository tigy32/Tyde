# Bootstrap Streams

Protocol version 5 retains the typed bootstrap frames introduced in protocol
version 4. `welcome` only accepts the handshake; it no longer carries bootstrap
data.

## Sequence semantics

- Host stream: `welcome` is server seq `0`; `host_bootstrap` is seq `1`.
- Every other server-published stream starts with its bootstrap at seq `0`.
- Live delta frames keep their existing kinds and follow the bootstrap.
- The server does not persist bootstrap blobs. Bootstraps are assembled from
  the same source-of-truth stores, registries, event logs, and snapshots used
  for live events.

## Bootstrap frames

### `host_bootstrap`

Sent once per host connection after `welcome`. It contains the host-wide state
that used to be replayed as many initial host frames:

- settings, mobile access state, backend setup, session schemas, sessions
- projects
- MCP servers, skills, steering, custom agents
- team preset catalog, drafts, teams, members, and member bindings
- live agent descriptors (`NewAgentPayload`) so clients can register agent
  streams before their `agent_bootstrap` frames arrive

`HostBootstrap` reuses the existing `SessionSummary` type for sessions.
Session schemas are treated as the subscriber's initial snapshot; later
`session_schemas` live frames are emitted only when the schema snapshot changes.
Mobile access subscribes with a bootstrap snapshot first, then activates the
live subscriber after the `host_bootstrap` frame has been queued. If mobile
state changes during that window, one live `mobile_access_state` follows the
bootstrap; unchanged initial state is not double-emitted.

### `project_bootstrap`

Sent as seq `0` on `/project/<project_id>`. It contains:

- the `Project`
- initial file list
- initial git status
- review summaries for that project

Implementation note: the previous initial review-summary replay was emitted on
project streams (`ProjectEventPayload::ReviewListChanged` from the host's
project subscription path), not on the host stream. The v4 bootstrap design
therefore kept review summaries in `ProjectBootstrap`, not `HostBootstrap`, and
that remains true in the current protocol.

### `agent_bootstrap`

Sent as seq `0` on each `/agent/<agent_id>/<instance_id>` stream. It is a
single frame containing ordered `AgentBootstrapEvent` entries built from the
agent event log plus active replay state:

- agent start
- agent error
- session settings
- queued messages
- chat events

After this frame, the stream continues with granular live agent events.

### `review_bootstrap`

Sent as seq `0` on `/review/<review_id>` after create/subscribe. It contains
the current `Review`. Later review mutations remain `review_event` deltas.

### `browse_bootstrap`

Sent as seq `0` on `/browse/<uuid>`. It contains the opened browse target and
the first directory listing or typed browse error.

### `terminal_bootstrap`

Sent as seq `0` on `/terminal/<terminal_id>`. It contains the terminal id and
`TerminalStartPayload`. PTY output starts only after this frame is queued, so
terminal output cannot race ahead of the bootstrap.

## Validation rules

`ProtocolValidator` enforces bootstrap-first ordering:

- host live frames are invalid before `host_bootstrap`
- project/review/browse/terminal live frames are invalid before their stream
  bootstrap
- agent streams must be registered by `host_bootstrap` or `new_agent`, then
  start with `agent_bootstrap`
- `host_bootstrap` registers listed agent streams
- `agent_bootstrap` validates its inner events in the same order as equivalent
  granular agent frames
