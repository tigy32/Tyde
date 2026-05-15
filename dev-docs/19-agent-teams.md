# Agent Teams

Spec for **Agent Teams**: persistent, server-owned teams of agents
organized as one manager and N direct reports. The user opens a chat
with the manager (just like any agent chat) and assigns work
conversationally. The manager delegates to its reports using MCP
tools. Each member binds a `CustomAgent` (its role, tools, prompts)
to a `SessionId` (its rolling memory). There is no kanban board, no
autonomous wake loop, and no Tyde-owned memory layer.

Audience: implementation agents and future maintainers.

This doc represents consensus between Claude and Codex design
proposals (see `dev-docs/proposals/agent-teams-claude.md`,
`dev-docs/proposals/agent-teams-codex.md`, and the two cross-review
files), simplified per Mike's direction to drop the board entirely
and lean on existing primitives.

It builds on:

- `01-philosophy.md` — non-negotiable architecture rules.
- `03-agents.md` — agent lifecycle and event model.
- `05-session-resume.md` — `SessionId`, `SpawnAgent::Resume`, the
  session store. **The v1 memory mechanism for team members.**
- `06-projects.md` — host-owned domain pattern.
- `11-agent-control-mcp.md` — embedded loopback MCP surface. **The
  v1 manager-to-report delegation surface.**
- `15-sub-agents.md` — `AgentOrigin`, parent/child plumbing.
- `16-queued-messages.md` — actor-owned per-agent queue.
- `17-custom-agents.md` — `CustomAgent` (the agent definition each
  team member is an instance of).

The whole spec reduces to:

> A **team** is a host-scoped record of `(name, manager_member_id,
> members[])`. A **team member** binds a `CustomAgent` (role / tools
> / prompts / backend) to a `SessionId` (rolling memory) and declares
> one or more `project_ids`. The server derives `workspace_roots`
> from those projects at spawn time. The user
> interacts with the team by opening the manager's chat — a normal
> agent chat, resumed via `SpawnAgent::Resume`. The manager delegates
> by calling `tyde_team_message_member`, which resolves the target
> member to a live `AgentId` (resuming or first-spawning as needed)
> and queues the message. The manager then uses the existing
> `tyde_read_agent` / `tyde_await_agents` to follow up. No board,
> no autonomous wake.

Teams span projects. The manager can have a frontend specialist on
project Foo, a backend specialist on projects Bar and Baz, and route
work across them.

---

## 1. Goals & Non-Goals

### Goals

- **Durable team membership.** Members survive backend session
  restarts, server restarts, and live-agent replacement.
- **Flat org chart**: one manager + N direct reports. Host-scoped.
- **Per-member projects.** A team can span codebases. Each member
  declares one or more `project_ids`; `workspace_roots` is derived
  as the union of those projects' roots at spawn time.
- **Members are `CustomAgent` instances.** Creating a member picks
  which `CustomAgent` it embodies — that defines its backend, system
  prompt, steering, skills, MCP servers. Two members can share the
  same `CustomAgent`; their `SessionId`s diverge from first spawn.
- **Session resume is the memory mechanism.** A member owns one
  `SessionId`. Idle members are historical sessions; the team
  registry resumes them on demand. Whatever the backend does for
  context handling (Claude's `/compact`, etc.) maintains the session.
  Tyde does not interpret, mirror, or summarize the session.
- **Interaction is conversational.** The user opens the manager's
  chat and talks. The manager picks reports and delegates via MCP.
  No new dispatch UI; no scheduling layer.
- **Single source of truth in Rust**: every concept is a typed
  `protocol/src/types.rs` record. The server emits replay+live
  events; the frontend renders.
- **Local and remote hosts are identical** from the frontend's
  perspective.

### Non-Goals (v1)

- **No kanban board.** No cards, columns, transitions, claim leases,
  optimistic concurrency, or board-stream subscriptions. Work flows
  through chat.
- **No autonomous manager loop.** The manager acts only when the
  user (or another agent's tool call) sends it a message. No server-
  driven wake on idle reports, no coalesced wake prompts.
- **No peer-to-peer delegation.** Only the manager can call
  `tyde_team_message_member`. Reports do their work and idle.
- **No team-level workspace_roots or project_id.** Teams are
  host-scoped; project bindings live on each member, and roots are
  derived from those projects.
- **No nested teams, multi-manager, matrix orgs.**
- **No agent-created teams.** Org changes are human-only.
- **No Tyde-owned memory or compaction.** The session is the memory.
- **No cost / budget tracking.** Real concern; deferred.
- **No migrating an existing live agent into a team.** Members are
  created with the team.
- **No forking a member's session** (e.g. "branch the manager's
  history into a new member").

---

## 2. Conceptual Model

```
┌─────────────┐
│    Team     │  host-scoped; no project, no workspace_roots
│ TeamId, name│
│ manager_id  │
└──────┬──────┘
       │ has N (≥1, including the manager)
       ▼
┌─────────────────────┐
│     TeamMember      │
│  TeamMemberId       │
│  role: Manager|Report
│  state: Active|Paused │
│  project_ids: Vec │← 1+; teams span projects
│  roots: derived   │
└────┬──┬─────────────┘
     │  │
     │  │ binds (required) ┌────────────────────────┐
     │  └─────────────────▶│   CustomAgent          │
     │                     │   (17-custom-agents)   │
     │                     │   backend, prompts,    │
     │                     │   steering, tools, MCP │
     │                     └────────────────────────┘
     │
     │ owns 0..1            ┌────────────────────────┐
     └─────────────────────▶│   SessionId            │
                            │   (05-session-resume)  │
                            │   rolling memory       │
                            └───────────┬────────────┘
                                        │ resumed into
                                        ▼
                            ┌────────────────────────┐
                            │  live AgentId          │
                            │  (runtime; may be None) │
                            └────────────────────────┘
                                        ▲
                              surfaced via
                              TeamMemberBindingNotify
```

### Definitions

- **Team** — a host-owned organization unit. Has `TeamId`, `name`,
  exactly one active manager (`manager_member_id`), and a list of
  members. Teams have *no* `project_id` and *no* `workspace_roots`
  at the team level.
- **TeamMember** — a durable persistent member identity
  (`TeamMemberId`). Binds a `CustomAgent` and optionally a
  `SessionId`. Has an org role (`Manager` or `Report`), a `state`
  (`Active`/`Paused`), and `project_ids` (one or more required).
  At spawn time the server derives `workspace_roots` as the
  deduped union of those projects' roots. Two members can share the
  same `CustomAgent`; their sessions are independent.
- **CustomAgent** — defined in `17-custom-agents.md`. Owns backend
  kind, system prompt, steering, skills, MCP server list.
- **SessionId** — defined in `05-session-resume.md`. The
  backend-native resumable session identifier. The member's
  *memory* is this session's history. Tyde stores nothing
  additional about the member's recall.
- **Manager** — the unique member with `TeamMemberRole::Manager`.
  Receives user messages and delegates to reports via the team MCP
  tools.
- **Report** — `TeamMemberRole::Report`. Receives delegated messages
  from the manager. Cannot delegate further in v1.
- **Live `AgentId` binding** — runtime-only mapping between
  `TeamMemberId` and the currently-running `AgentId`. Emitted as
  `TeamMemberBindingNotify`. Never persisted as durable identity.

The existing `Agent` model from `03-agents.md` is unchanged. A live
team-member agent is a normal `Agent` with
`AgentOrigin::TeamMember` and `team_id`/`team_member_id` populated
on its birth-certificate payload.

---

## 3. Persistence Model

### Choice: one JSON file at `~/.tyde/agent_teams.json`

Same on-disk pattern as every other host-owned domain in Tyde2
(`projects.json`, `custom_agents.json`, `steering.json`,
`mcp_servers.json`, `sessions.json`): a single JSON file under
`~/.tyde/`, loaded once at startup, atomically rewritten on each
mutation (write-temp, `fsync`, `rename`). Mirror the existing
pattern in `server/src/store/session.rs` and
`server/src/store/project.rs`.

### Store file shape

```rust
#[derive(Serialize, Deserialize)]
struct AgentTeamsStoreFile {
    version: u32,
    teams: HashMap<TeamId, Team>,
    members: HashMap<TeamMemberId, TeamMember>,
}
```

The map keys mirror the typed-ID newtypes. Each field stores the
protocol struct directly — no parallel persistence mirror types.
Live `AgentId` bindings are runtime state, never persisted.

`version` starts at 1; bump and add a migration step if shape
changes.

### Single-writer actor

A `TeamRegistry` actor owns the in-memory `AgentTeamsStoreFile` and
the file path. All mutations serialize through it via mpsc. After
every accepted mutation it writes the updated file (temp + atomic
rename) before emitting protocol events. No `Arc<Mutex<...>>`.

### Validation

The registry validates on load and on every mutation. No silent
repair; loud failure on invariant violation:

- Every member references an existing team.
- Every member's `custom_agent_id` references an existing
  `CustomAgent`.
- Every member has one or more `project_ids`, each referencing an
  existing `Project`.
- Each team has exactly one active manager (`role == Manager &&
  state == Active`).
- A `TeamMemberId` belongs to exactly one team.
- A `SessionId` is owned by at most one member.
- A team's `manager_member_id` resolves to a member of that team
  with `role == Manager`.

If the file fails to load (invalid JSON, invariant violation,
unknown enum variant), startup fails loudly. No "best-effort drop
the bad rows" recovery.

### Runtime live-agent binding

The live `AgentId` for a member is **runtime state**, never
written to disk. It is emitted as `TeamMemberBindingNotify`.
After a server restart, the registry emits `current_agent_id:
None` for every member until they're rebound.

---

## 4. Org Structure

V1 invariants — chosen to make invalid states unrepresentable:

- A team has exactly one active manager.
- A `TeamMemberId` belongs to exactly one team.
- A manager cannot also be a report in the same team.
- Teams are host-scoped. No team-level `project_id`. Members bind
  one or more projects individually.
- Teams do not nest.

**Manager replacement** is human-only via `TeamSetManager`. The
new manager must already be a `Report` of the team. The transition
is atomic in one transaction. No auto-promotion when a manager's
session can no longer be resumed; the team waits for human action.

The org graph is depth-1. Forward-compatible with nesting later
without breaking the protocol.

---

## 5. Member Lifecycle & Session Continuity

A team member's memory is its `SessionId`. The existing session
store (`05-session-resume.md` / `sessions.json`) is the source of
truth for the conversation history. Tyde maintains no parallel
record.

### First activation

A member is "activated" when someone (the user, or the manager via
`tyde_team_message_member`) first sends it a message. With
`session_id: None`:

1. The registry issues `SpawnAgent::New` with:
   - `custom_agent_id` = member's `CustomAgent`
   - `workspace_roots` = union of member project roots
   - `parent_agent_id: None`
   - `project_id` = first `member.project_ids` entry
   - `prompt` = the incoming message
   - `backend_kind` = derived from the `CustomAgent`
2. The backend produces a fresh `SessionId`. The registry records
   it on the member in one transaction, then emits
   `TeamMemberNotify::Upsert` (now with `session_id: Some(...)`)
   and `TeamMemberBindingNotify { current_agent_id: Some(...) }`.

### Subsequent activations (member not currently bound)

With `session_id: Some(s)` and `current_agent_id: None`:

1. Registry issues `SpawnAgent::Resume { session_id: s, prompt:
   Some(message) }`.
2. Backend resumes; a new live `AgentId` is bound.
3. Registry emits `TeamMemberBindingNotify`.

### Subsequent activations (member currently bound)

The message is delivered to the live agent via the existing queue
actor (`16-queued-messages.md`). The session continues; no resume
needed.

### When does a member become unbound?

Same triggers as any other agent — the live `AgentId` disappears
when:

- the agent closes normally after going idle (existing behavior;
  the session persists in the session store)
- the agent crashes
- the server restarts

Tyde does not deliberately park members. Whatever lifetime rules
apply to regular agents apply to team members. If you want a
report to stay warm, keep sending it messages.

### Compaction / memory growth

Not Tyde's problem. The backend's own context handling keeps the
session usable. If `Backend::resume()` fails because the session
grew too large or the backend lost it, the registry surfaces a
visible binding-failure event. The user can delete and recreate
the member (`SpawnAgent::New` against the same `CustomAgent`).
**Tyde never silently falls back to `SpawnAgent::New` on resume
failure** — that would lose memory without the user knowing.

### `last_active_at_ms` for UI

The registry records a non-persisted `last_active_at_ms` per
member, updated whenever the live agent emits a turn. UI affordance
only; lost on restart; re-derived from next activity.

---

## 6. Manager and Delegation

There is no autonomous manager loop. The manager is a normal live
agent; it acts only when it receives a message.

### How the user works with a team

1. User opens the team in the Teams panel.
2. That opens the **manager's** agent chat — exactly the same UI as
   any other agent chat, resumed from `manager.session_id`.
3. User types: *"have alice take a look at the auth bug and bob
   refactor the dashboard."*
4. The manager replies in chat, and along the way calls
   `tyde_team_message_member({ member_id: alice, message: ... })`
   and similarly for bob. The server resolves each `member_id` to
   a live `AgentId` (resuming or first-spawning) and queues the
   message.
5. The tool call returns the live `AgentId`s. The manager can
   follow up using existing `tyde_read_agent` /
   `tyde_await_agents` from `11-agent-control-mcp.md`.

The manager's `CustomAgent` system prompt should explain its role
and the available team tools. The roster (who exists, what they
specialize in, their `project_ids`) is delivered as part of the
manager's session context — see §6.2.

### 6.2 How the manager knows who its reports are

Two paths, both supported:

- **Spawn-time context.** The first message the manager ever sees
  (via `SpawnAgent::New.prompt`, when the manager is first
  activated) includes a server-authored roster block listing each
  report's `member_id`, `name`, `description`, and `project_ids`.
  The manager remembers it for the rest of the session.
- **MCP query.** `tyde_team_describe` is always available and
  returns the current roster with live bindings. The manager calls
  it when in doubt, especially after a long gap or if it suspects
  membership changed.

The roster block is *not* re-injected on every message — that
would burn tokens. The manager has tools to refresh on demand.

### 6.3 Delegation flow

Manager calls
`tyde_team_message_member({ member_id, message, images? })`:

1. Server validates: caller is the team's active manager;
   `member_id` is a `Report` of the same team in `Active` state.
2. Server resolves the member to a live `AgentId`:
   - If currently bound → reuse.
   - Else if `session_id: Some(s)` → `SpawnAgent::Resume { s,
     prompt: Some(message) }`. Record any state changes
     (e.g. binding) and emit notifies.
   - Else → `SpawnAgent::New { custom_agent_id, workspace_roots,
     project_id, prompt: message, ... }`, where `workspace_roots`
     is the union of all bound project roots and `project_id` is
     the first bound project. Record the new `SessionId` on the
     member and emit notifies.
3. If the member was already bound, the message is queued via the
   queue actor.
4. Tool returns `{ member_id, agent_id, queued: bool }` so the
   manager can call `tyde_read_agent` / `tyde_await_agents` next.

The manager and reports otherwise communicate through normal agent
streams. There is no special "team channel."

### 6.4 What stops a report from delegating?

Authorization in `tyde_team_message_member` checks
`caller_agent_id` against the team's active manager. A report
calling it gets a typed authorization error. Reports cannot grow
the team or hire helpers.

(Reports can still call `tyde_spawn_agent` from the existing
agent-control MCP to spawn transient helpers — same as any other
agent. Those helpers are not team members. See §13 open
questions.)

---

## 7. Protocol Changes

All in `protocol/src/types.rs`. Frontend types are generated.

### 7.1 Typed IDs

```rust
#[serde(transparent)]
pub struct TeamId(pub String);

#[serde(transparent)]
pub struct TeamMemberId(pub String);
```

(`SessionId`, `CustomAgentId`, `AgentId`, `ProjectId`, `BackendKind`
all reused from existing protocol.)

### 7.2 Enums

```rust
#[derive(..., Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberRole { Manager, Report }

pub enum TeamMemberState { Active, Paused }
```

### 7.3 `AgentOrigin` extension

```rust
pub enum AgentOrigin {
    User,
    AgentControl,
    BackendNative,
    TeamMember,        // NEW
}
```

Extend `AgentStartPayload` / `NewAgentPayload`:

```rust
pub team_id: Option<TeamId>,
pub team_member_id: Option<TeamMemberId>,
```

Validation: `AgentOrigin::TeamMember` requires both fields `Some`;
all other origins require both `None`. Frontend never infers from
parentage.

### 7.4 Records

```rust
pub struct Team {
    pub id: TeamId,
    pub name: String,
    pub manager_member_id: TeamMemberId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

pub struct TeamMember {
    pub id: TeamMemberId,
    pub team_id: TeamId,
    pub role: TeamMemberRole,
    pub state: TeamMemberState,
    pub name: String,
    pub description: String,                   // free-form; surfaced to manager
    pub custom_agent_id: CustomAgentId,        // required
    pub session_id: Option<SessionId>,         // None until first spawn
    pub project_ids: Vec<ProjectId>,           // one or more required
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

pub struct TeamMemberBindingPayload {
    pub member_id: TeamMemberId,
    pub current_agent_id: Option<AgentId>,
    pub status: AgentControlStatus,
    pub last_active_at_ms: Option<u64>,
}
```

### 7.5 Streams

Teams ride on the existing **`/host/<uuid>`** stream. There is no
per-team detail stream — the team and its members are small enough
to live in host replay, and a chat with the manager is just a
normal `/agent/<id>` stream on top of session resume.

### 7.6 Input frame kinds (host stream)

```rust
TeamCreate
TeamRename
TeamDelete               // hard delete; cascades to all members
TeamSetManager           // new manager must already be a Report of this team
TeamMemberCreate         // requires custom_agent_id and project_ids
TeamMemberUpdate         // mutable: name, description, project_ids
TeamMemberDelete         // hard delete; rejects active manager / only member / live-bound
```

No `TeamMember*Message`, no `TeamCard*` anything.

### 7.7 Output frame kinds (host stream)

```rust
TeamNotify              // Upsert { team } | Delete { team }
TeamMemberNotify        // Upsert { member } | Delete { member }
TeamMemberBindingNotify // payload defined above
```

`Notify` payloads use the tagged `Upsert | Delete` pattern from
existing host domains; delete carries the full prior record.

### 7.8 Replay ordering

On host attach:

1. `HostSettings`
2. existing host prelude
3. `ProjectNotify` *(members may reference projects)*
4. `McpServerNotify`
5. `SkillNotify`
6. `SteeringNotify`
7. `CustomAgentNotify` *(members reference CustomAgents)*
8. `SessionNotify` *(members reference sessions)*
9. **`TeamNotify`** — team summaries
10. **`TeamMemberNotify`** — for each member
11. existing live `NewAgent` events (some may have
    `AgentOrigin::TeamMember`)
12. **`TeamMemberBindingNotify`** — for each member (current
    bindings, mostly `None` immediately after restart)

### 7.9 Validation in `protocol/src/validator.rs`

- `TeamMemberCreate.custom_agent_id` references an existing
  `CustomAgent`.
- `TeamMemberCreate.project_ids` is non-empty, and every id
  references an existing `Project`.
- `TeamMemberCreate.session_id` must be absent — fresh members
  start with no session.
- A `SessionId` cannot be claimed by more than one member.
- `TeamCreate` accepts an inline manager `TeamMemberCreate` as
  part of the same payload (atomic team-with-manager creation) and
  validates both together.
- `TeamSetManager.new_manager_member_id` references an existing
  member with `role == Report` and `state == Active`.
- `TeamMemberDelete` rejected if the member is the team's active
  manager, the team's only member, or has a live binding. Use
  `TeamDelete` to remove an entire team; it cascades to all members.
- `AgentOrigin::TeamMember` requires both `team_id` and
  `team_member_id`; other origins require both `None`.

---

## 8. MCP Surface

Add team tools to the existing embedded agent-control MCP server
(`11-agent-control-mcp.md`). Each tool is a thin shim over a typed
protocol command handled by the `TeamRegistry`; the MCP layer is
not a parallel control plane.

Caller identity is derived from the loopback URL injection (existing
pattern). The server knows the calling `AgentId` and from it the
calling `TeamMemberId` (if any).

### Tools

| Tool                              | Visibility   | Behavior                                                                 |
|-----------------------------------|--------------|--------------------------------------------------------------------------|
| `tyde_team_describe`              | Any team member | Returns team metadata + roster with each member's `CustomAgent` summary, `project_ids`, current binding, last-active. |
| `tyde_team_message_member`        | Manager only | Sends a message to a teammate. Resolves member → live `AgentId`, resuming or first-spawning as needed. Returns `{ member_id, agent_id, queued: bool }`. |

Deliberately omitted in v1:

- `tyde_team_create` / `tyde_team_add_member` — org changes are
  human-only.
- `tyde_team_message_team` (broadcast) — out of scope.
- Any tool reading another member's chat output. Managers use the
  existing `tyde_read_agent` / `tyde_await_agents` against the
  `AgentId` returned by `tyde_team_message_member`.

If `HostSettings.tyde_agent_control_mcp_enabled` is `false`, team
delegation is unavailable. The user can still chat with the
manager directly; the manager just can't delegate.

---

## 9. Frontend Surface

Brief; Mike will iterate later.

### 9.1 Teams panel

Sibling to existing Projects/Sessions/Agents panels:

- List of teams: name and member count.
- "New team" wizard: name, create the manager (pick `CustomAgent`,
  one or more projects via a multi-project picker, name,
  description), then add reports the same way.
- "Add report" / "Edit member" affordances inside a team.

### 9.2 Opening a team

Clicking a team **opens the manager's agent chat** (same UI as any
session resume from the Sessions tab). The chat stream is the
manager's `/agent/<id>` stream; the chat history is the manager's
session history.

A sidebar in this view shows the team roster:

- Each report: name, role, `CustomAgent` label, selected projects
  from the multi-project picker, live status (from
  `TeamMemberBindingNotify`), last-active time.
- Click a report → open *that report's* chat in another tab.
  Identical agent-chat view, different `AgentId`. No new UI.

### 9.3 No new dispatch primitives

Everything renders from `TeamNotify`, `TeamMemberNotify`,
`TeamMemberBindingNotify` + the existing agent stream. No refresh
button.

---

## 10. Failure Modes

### Member's session can't be resumed

`Backend::resume(member.session_id)` fails (session deleted,
backend lost it, version mismatch). The registry:

- Emits a `TeamMemberBindingNotify` with status indicating the
  failure (reuse `AgentControlStatus` failure variants).
- Does **not** silently `SpawnAgent::New`. Losing memory is a
  user-visible event, not a recovery path.
- User deletes the member and creates a new one (deliberate fresh
  start), or escalates.

### Manager session can't be resumed

Same as above. The team becomes unusable for new delegation
because the user can't reach the manager. UI shows team as
degraded. Human runs `TeamSetManager` against a `Report` (which
is then promoted), then may delete the old member.

### Member deleted while live agent is running

`TeamMemberDelete` is rejected if the member has a live binding,
to keep things simple. The user must close the live chat first (or
the agent must idle out), then delete. The session record itself
is not deleted by member delete; the member is removed from the
roster and the team store.

### `CustomAgent` deleted while in use

`CustomAgentDelete` is rejected by `17-custom-agents.md` validation
while any team member references it. (This rule needs to be added
when teams ship.)

### Project deleted while a member references it

`ProjectDelete` is rejected by `06-projects.md` validation while
any team member's `project_ids` contains it.

### Server restart

`TeamRegistry` loads `agent_teams.json` into memory. All bindings
are `None`. The registry emits replay in §7.8 order. Members are
not auto-spawned; they come back to life when next messaged.

### MCP disabled

`tyde_team_message_member` returns an unavailable error. The user
can still chat with the manager directly. Without the team MCP,
the manager is just an agent with a description of its team in
its session history.

### Race on team mutation

The `TeamRegistry` actor serializes mutations. The only racy case
is two clients mutating the same team simultaneously (e.g. one
renames it, another adds a member). Mutations are independent
fields; last-write wins per field. No optimistic concurrency
needed because there is no shared mutable card state to race on.

---

## 11. Implementation Order (rough)

1. Protocol types and frame kinds (§7). Generated frontend types
   fall out automatically.
2. `TeamRegistry` actor owning `~/.tyde/agent_teams.json` per §3,
   single-writer pattern, mirroring `server/src/store/session.rs`.
3. Extend `CustomAgentDelete` and `ProjectDelete` validation to
   reject if any team member references them.
4. Member activation path: `SpawnAgent::New` on first wake,
   `SpawnAgent::Resume` on subsequent wakes. Record `session_id`
   atomically with the member upsert.
5. Host stream replay extension (`TeamNotify`, `TeamMemberNotify`,
   `TeamMemberBindingNotify`).
6. Agent-control MCP team tools (§8) with manager-only auth on
   `tyde_team_message_member`.
7. Roster injection into the manager's first `SpawnAgent::New`
   prompt.
8. Frontend teams panel + member roster sidebar.
9. Tests (§12).

---

## 12. Testing

Unit / integration:

- Member create persists; reload from disk round-trips.
- First message to a fresh member triggers `SpawnAgent::New` and
  records the resulting `session_id` atomically with the member
  upsert.
- Subsequent message to an unbound member triggers
  `SpawnAgent::Resume` against the recorded `session_id` (never
  `New`).
- `Backend::resume` failure surfaces as visible binding failure;
  no fallback to `New`.
- `tyde_team_message_member` from a `Report` is rejected.
- `tyde_team_message_member` from the manager to a member of
  another team is rejected.
- `TeamDelete` hard-removes the team and cascades to all members.
- `TeamMemberDelete` of the active manager is rejected;
  `TeamMemberDelete` of the only member is rejected;
  `TeamMemberDelete` of a live-bound member is rejected.
- `CustomAgentDelete` and `ProjectDelete` rejected while
  referenced.
- Replay ordering: `CustomAgent`/`Project`/`Session` events
  precede teams.
- Server restart: members load with `current_agent_id: None`;
  next message activates them correctly.

Frontend (wasm-bindgen-test, per `CLAUDE.md`):

- Teams panel renders one row per `TeamId`.
- Opening a team navigates to the manager's `/agent/<id>` stream.
- Member sidebar shows live binding state and updates on
  `TeamMemberBindingNotify`.
- No frontend caches; clearing signals re-renders identical DOM.

---

## 13. Open Questions for Mike

These are deliberate unresolved points. Each is reversible.

1. **Roster injection.** Spec says the team roster is injected into
   the manager's first-spawn prompt and refreshed via
   `tyde_team_describe` on demand. Alternative: a small system-prompt
   prepend on *every* manager turn that lists the roster. That
   guarantees freshness but burns tokens. Confirm OK to do
   inject-once + on-demand refresh.

2. **Reports spawning transient helpers.** Reports can still call
   the existing `tyde_spawn_agent` to spawn one-off helpers (per
   `11-agent-control-mcp.md`). Those helpers aren't team members
   and don't show up in the roster. Confirm OK.

3. **Manager project selection.** A manager has its own
   `project_ids` like any member. For a coordination-only manager,
   the user still picks at least one project. Confirm we don't need
   "manager has the union of all reports' projects" auto-magic.

4. **Concurrency / cost cap per team.** Not in v1. Without the
   board and without autonomous wake, the manager spawns reports
   only as the user drives the conversation, so runaway loops are
   less of a concern — but a `HostSettings.team_max_concurrent_members`
   cap would still be sensible later.

5. **Manager auto-promotion.** When a manager's session can no
   longer be resumed, the team is degraded until the user runs
   `TeamSetManager`. We could auto-promote the longest-tenured
   report. v1 says no — humans should make this call.

6. **Resume failure handling.** Spec says do **not** silently
   `SpawnAgent::New` when `Resume` fails. The user must delete
   and recreate. Confirm.

---

## 14. Glossary cross-reference

| Term                    | Where defined                             |
|-------------------------|-------------------------------------------|
| `AgentId`               | `03-agents.md`                            |
| `SessionId`, resume     | `05-session-resume.md`                    |
| `AgentOrigin`           | `15-sub-agents.md` + §7.3 here            |
| `CustomAgent`, `CustomAgentId` | `17-custom-agents.md`              |
| `ProjectId`             | `06-projects.md`                          |
| Queued message / actor  | `16-queued-messages.md`                   |
| Loopback MCP server     | `11-agent-control-mcp.md`                 |

---

## 15. Summary

A team is a host-scoped record `(Team, TeamMembers)`. Each member
binds a `CustomAgent` (role / tools / instructions) to a
`SessionId` (rolling memory), binds one or more projects, and is
reanimated on demand via `SpawnAgent::Resume`. Workspace roots are
derived as the union of the member's project roots at spawn time.
The user works with the team by opening the
manager's chat — a normal agent chat resumed from
`manager.session_id`. The manager delegates by calling
`tyde_team_message_member`, which resolves the target member to a
live `AgentId` and queues the message. There is no board, no
autonomous loop, no Tyde-owned memory layer, no per-team detail
stream. Persistence is one JSON file. The frontend is a pure
projection.

The biggest scope risk is putting any of this back in v1 because
"surely we need it for X." We don't. Ship the manager-chat
delegation flow first; reassess after real use.
