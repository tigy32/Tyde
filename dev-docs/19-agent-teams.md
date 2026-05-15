# Agent Teams

Spec for **Agent Teams**: persistent, server-owned teams of agents
organized as one manager and N direct reports. The user opens a chat
with the manager (just like any agent chat) and assigns work
conversationally. The manager delegates to its reports using MCP
tools. Each member has an explicit backend/cost profile, may
optionally bind a `CustomAgent` (role, tools, prompts), and owns a
`SessionId` (its rolling memory). There is no kanban board, no
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
- `17-custom-agents.md` — optional `CustomAgent` profiles team
  members can use instead of the default agent profile.

The whole spec reduces to:

> A **team** is a host-scoped record of `(name, manager_member_id,
> members[])`. A **team member** declares an explicit `backend_kind`
> and optional `cost_hint`, may bind a `CustomAgent` (role / tools /
> prompts), owns a `SessionId` (rolling memory), and declares one or
> more `project_ids`. The server derives `workspace_roots` from those
> projects at spawn time. The user
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
- **Members have explicit agent profiles.** Creating a member picks
  `backend_kind`, optional `cost_hint`, and optionally a
  `CustomAgent`. `None` means the default agent profile. A
  `CustomAgent` supplies instructions, steering, skills, MCP
  servers, and tool policy; backend and cost live directly on the
  member. Tyde seeds a small set of team-oriented `CustomAgent`
  records automatically when they are missing; these are normal
  user-editable custom agents, not hidden frontend presets.
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
- **No team-level budget tracking.** Per-member `cost_hint` is a
  backend spawn hint, not accounting or spend enforcement.
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
│  backend_kind     │
│  cost_hint?       │
│  preset profile?  │
│  roots: derived   │
└────┬──┬─────────────┘
     │  │
     │  │ optionally binds ┌────────────────────────┐
     │  └─────────────────▶│   CustomAgent          │
     │                     │   (17-custom-agents)   │
     │                     │   prompts, steering,   │
     │                     │   tools, MCP, policy   │
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
  (`TeamMemberId`). Declares `backend_kind`, optional `cost_hint`,
  optional `custom_agent_id`, and optionally a `SessionId`. Has an
  org role (`Manager` or `Report`), a `state`
  (`Active`/`Paused`), and `project_ids` (one or more required).
  At spawn time the server derives `workspace_roots` as the
  deduped union of those projects' roots. Two members can share the
  same `CustomAgent`; their sessions are independent.
- **Preset profile** — optional structured creation metadata:
  a role/specialty preset, optional personality preset, and concrete
  personality traits. It is separate from `TeamMemberRole`, which is
  only the org role (`Manager`/`Report`). It is also separate from
  `description`, which stays editable free-form text for user
  customization and backward compatibility. Manual members with no
  preset profile are legal.
- **CustomAgent** — defined in `17-custom-agents.md`. Owns system
  prompt/instructions, steering, skills, MCP server list, and tool
  policy. It is optional for team members. The built-in team custom
  agents are ordinary `CustomAgent` records with stable ids; Tyde
  creates missing records with defaults and never overwrites an
  existing record with the same id.
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
Team drafts are also runtime state in v1: the server owns them and
emits them, but they are not written to `agent_teams.json` until a
draft commit creates a real team. That keeps incomplete project/backend
choices out of the durable team store while preserving the event-driven
UI model.

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
- Every `Some(custom_agent_id)` references an existing
  `CustomAgent`; `None` means the default agent profile.
- Every member has one or more `project_ids`, each referencing an
  existing `Project`.
- Every member's `backend_kind` is enabled on the host.
- Every present preset id in `profile` resolves against the server-owned
  preset catalog.
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
   - `custom_agent_id` = member's optional `CustomAgent`
   - `workspace_roots` = union of member project roots
   - `parent_agent_id: None`
   - `project_id` = first `member.project_ids` entry
   - `prompt` = the incoming message
   - `backend_kind` = member's stored `backend_kind`
   - `cost_hint` = member's stored `cost_hint`
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
the member (`SpawnAgent::New` against the same member profile).
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

If the manager uses a `CustomAgent`, its system prompt should
explain the manager role and available team tools. The roster (who
exists, what they specialize in, their `project_ids`) is delivered
as part of the manager's session context — see §6.2.

### 6.2 How the manager knows who its reports are

Two paths, both supported:

- **Spawn-time context.** The first message the manager ever sees
  (via `SpawnAgent::New.prompt`, when the manager is first
  activated) includes a server-authored roster block listing each
  report's `member_id`, `name`, `description`, `project_ids`,
  backend/cost profile, and structured preset profile when present.
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
     project_id, backend_kind, cost_hint, prompt: message, ... }`,
     where `custom_agent_id`, `backend_kind`, and `cost_hint` come
     from the member, `workspace_roots` is the union of all bound
     project roots, and `project_id` is the first bound project.
     Record the new `SessionId` on the member and emit notifies.
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
    pub profile: Option<TeamMemberPresetProfile>, // optional role/personality metadata
    pub custom_agent_id: Option<CustomAgentId>,// None = default agent
    pub backend_kind: BackendKind,             // explicit backend selection
    pub cost_hint: Option<SpawnCostHint>,      // optional effort/cost hint
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

`TeamMemberPresetProfile` is a protocol record, not frontend-only
metadata. It stores optional `role_preset_id`, optional
`personality_preset_id`, and the concrete `personality_traits` emitted
by the server. The server-owned catalog provides the readable preset and
trait names.

Drafts use protocol records too:

```rust
pub struct TeamPresetCatalog {
    pub role_presets: Vec<TeamRolePreset>,
    pub personality_traits: Vec<TeamPersonalityTraitPreset>,
    pub personality_presets: Vec<TeamPersonalityPreset>,
    pub team_templates: Vec<TeamTemplate>,
}

pub struct TeamDraft {
    pub id: TeamDraftId,
    pub name: String,
    pub members: Vec<TeamDraftMember>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
```

`TeamDraftMember` mirrors the create-time member fields but keeps
`backend_kind` optional because incomplete drafts are valid until
commit. Commit converts the draft to `TeamCreateFromDraftPayload` and
validates all required backend/project/member fields before anything is
persisted.

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
TeamMemberCreate         // requires backend_kind and project_ids
TeamMemberUpdate         // mutable: name, description, profile, project_ids
TeamMemberDelete         // hard delete; rejects active manager / only member / live-bound
TeamMemberShuffle        // request a server-owned Add-report shuffle suggestion
TeamDraftCreate          // creates a server-owned blank or templated draft
TeamDraftUpdate          // edits draft name/member/profile/add/remove
TeamDraftShuffle         // server chooses new role/personality content
TeamDraftApplyTemplate   // replaces draft members from a server template
TeamDraftCommit          // atomic team + manager + reports create
TeamDraftDiscard         // drops the server-owned draft
```

No `TeamMember*Message`, no `TeamCard*` anything.

### 7.7 Output frame kinds (host stream)

```rust
TeamNotify              // Upsert { team } | Delete { team }
TeamMemberNotify        // Upsert { member } | Delete { member }
TeamMemberBindingNotify // payload defined above
TeamPresetCatalogNotify // server-owned role/personality/template catalog
TeamDraftNotify         // Upsert { draft } | Delete { draft_id }
TeamMemberShuffleSuggestionNotify // ephemeral Add-report shuffle suggestion
```

`TeamMemberShuffleSuggestionNotify` is fire-and-forget: the server emits
one in response to a `TeamMemberShuffle` request and never replays it on
host attach. It currently fans out to every host subscriber rather than
the requesting client only — sufficient because a stale suggestion is
keyed per `(host, team_id)` and the open Add-report dialog applies only
suggestions whose serial advances past its baseline. Tighten to caller-
scoped routing if a future change makes that meaningful.

`Notify` payloads use the tagged `Upsert | Delete` pattern from
existing host domains; delete carries the full prior record.

### 7.8 Server-owned catalog, templates, and drafts

The server emits a `TeamPresetCatalogNotify` on every host replay before
team drafts and committed teams. The frontend has no semantic preset
lists: it renders whatever catalog the server sent and sends typed draft
input events when the user chooses a template, changes a preset, or
presses shuffle.

The v1 catalog contains:

- Built-in team custom agents: Team Lead, Code Reviewer, Frontend
  Engineer, Backend Engineer, and Test / QA Engineer. These are
  created in `custom_agents.json` if missing and replay as ordinary
  `CustomAgentNotify::Upsert` events. Users can edit them in the
  custom-agent UI to add local rules; seeding does not overwrite those
  edits. These built-in custom agents are separate from the role and
  personality catalogs below.
- Role/specialty presets: Tech lead / planner, Senior reviewer,
  Frontend specialist, Backend specialist, Test author / QA, and Bug
  hunter / debugger.
- Personality traits: Cautious, Pragmatic, Bold, Contrarian, Terse,
  Conversational, Pedagogical, Skeptical, Refactor-leaning, Ship-it,
  Test-first, Type-system, and YAGNI.
- Personality presets: Skeptical reviewer, Pragmatic shipper, Careful
  architect, Test-first engineer, and Refactor-minded senior.
- Team templates: Solo + reviewer, Small feature team, Review panel,
  and Debug squad. Small feature team is the balanced-team template.

Draft lifecycle:

1. `TeamDraftCreate { template_id }` creates one active server-owned
   draft. `None` starts blank with a manager slot; a template id starts
   with server-generated members/profile metadata. Creating another
   draft while one exists is an explicit error.
2. `TeamDraftUpdate`, `TeamDraftShuffle`, and
   `TeamDraftApplyTemplate` mutate that server draft and emit
   `TeamDraftNotify::Upsert`. Member/template shuffles are
   server-owned: they choose an editable member name, a built-in
   `custom_agent_id`, and personality profile data. The frontend does
   not pick semantic names, agents, or personalities locally.
3. `TeamDraftCommit` validates all derived create specs and performs
   one store mutation that creates the team, manager, reports, and
   initial member bindings. On validation/storage error the draft is
   restored and no half-created team is emitted.
4. `TeamDraftDiscard` emits `TeamDraftNotify::Delete` and removes the
   draft from server memory.

### 7.9 Replay ordering

On host attach:

1. `HostSettings`
2. existing host prelude
3. `ProjectNotify` *(members may reference projects)*
4. `McpServerNotify`
5. `SkillNotify`
6. `SteeringNotify`
7. `CustomAgentNotify` *(members may reference CustomAgents)*
8. `SessionNotify` *(members reference sessions)*
9. **`TeamPresetCatalogNotify`** — catalog for role/personality/template UI
10. **`TeamDraftNotify`** — any in-memory active draft
11. **`TeamNotify`** — team summaries
12. **`TeamMemberNotify`** — for each member
13. existing live `NewAgent` events (some may have
    `AgentOrigin::TeamMember`)
14. **`TeamMemberBindingNotify`** — for each member (current
    bindings, mostly `None` immediately after restart)

The registry owns one binding record for every active member. Missing
bindings are an invariant violation; callers should surface an error
rather than synthesizing an `Idle` binding.

### 7.10 Validation in `protocol/src/validator.rs`

- `TeamMemberCreate.backend_kind` is required and strongly typed.
- `TeamMemberCreate.custom_agent_id`, when present, references an
  existing `CustomAgent`; absence selects the default agent profile.
- `TeamMemberCreate.profile`, when present, references role and
  personality presets from the server-owned catalog.
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
- `TeamDraftCommit` rejects incomplete drafts before persistence:
  team name, member names/descriptions, backend kind, project ids, and
  referenced custom agents/presets must all validate.

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
| `tyde_team_describe`              | Any team member | Returns team metadata + roster with each member's backend/cost profile, optional `CustomAgent` summary, structured/readable role/personality profile, `project_ids`, current binding, last-active. Missing member bindings are surfaced as registry errors. |
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
- "New team" wizard: start blank, generate the balanced template, or
  choose a server-owned template. Once a draft exists, edit its team
  name and member slots in place.
- Each member slot renders server-emitted role/specialty and
  personality controls, per-member and whole-draft shuffle buttons,
  editable name/description, default/custom agent selection, backend,
  cost effort, and project picker. The custom-agent selector includes
  the seeded built-in team custom agents because they are normal
  `CustomAgentNotify` records. Presets and shuffles seed editable
  fields; they do not lock them.
- The wizard commits by sending `TeamDraftCommit`. The frontend does
  not create a team and then loop over `TeamMemberCreate`.
- "Add report" / "Edit member" affordances inside a team.
- If the active chat belongs to a team member, the matching team card
  and member row are marked active from `TeamMemberBindingNotify` state
  or the typed draft team-member tab state. Member rows show live
  binding status and last-active time.

### 9.2 Opening a team

Clicking a team **opens the manager's agent chat** (same UI as any
session resume from the Sessions tab). The chat stream is the
manager's `/agent/<id>` stream; the chat history is the manager's
session history.

The chat view does not mount a separate team roster sidebar. Team
navigation stays in the Teams panel so the chat keeps its full width.
The active team/member marker in that panel provides context while
preserving the same actions:

- Each report: name, role, agent profile label (default/custom,
  backend, cost), selected projects from the multi-project picker,
  live status (from
  `TeamMemberBindingNotify`), last-active time.
- Click a report → open *that report's* chat in another tab.
  Identical agent-chat view, different `AgentId`. No new UI.

### 9.3 No new dispatch primitives

Everything renders from `TeamNotify`, `TeamMemberNotify`,
`TeamMemberBindingNotify`, `TeamPresetCatalogNotify`,
`TeamDraftNotify` + the existing agent stream. No refresh button.
The frontend may keep signal maps of received protocol records, but it
does not own preset semantics or synthesize team-template content.

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

`CustomAgentDelete` is rejected while any team member has
`custom_agent_id: Some(id)` for that custom agent. Members using the
default agent profile do not block custom-agent deletion.

Built-in team custom agents are still normal custom agents: editing
them persists the edited record, and seeding does not overwrite it. If
a built-in team custom agent record is missing at host startup, Tyde
recreates it with the default content and replays it through the normal
custom-agent event path.

### Project deleted while a member references it

`ProjectDelete` is rejected by `06-projects.md` validation while
any team member's `project_ids` contains it.

### Server restart

`TeamRegistry` loads `agent_teams.json` into memory. All bindings
are `None`. The registry emits replay in §7.9 order. Members are
not auto-spawned; they come back to life when next messaged.
In-memory team drafts are not restored after restart; users restart the
draft from the server catalog.

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
8. Server-owned preset catalog, in-memory drafts, typed draft events,
   template/shuffle handling, and atomic draft commit.
9. Frontend teams panel with active team/member markers and draft UI
   rendered from catalog/draft notifies.
10. Tests (§12).

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
- `tyde_team_describe` returns default-agent members with no
  `CustomAgent` summary and surfaces missing binding invariants.
- `TeamDelete` hard-removes the team and cascades to all members.
- `TeamMemberDelete` of the active manager is rejected;
  `TeamMemberDelete` of the only member is rejected;
  `TeamMemberDelete` of a live-bound member is rejected.
- `CustomAgentDelete` and `ProjectDelete` rejected while
  referenced.
- Replay ordering: `CustomAgent`/`Project`/`Session` events
  precede catalog, drafts, and teams; catalog precedes draft UI state.
- Catalog replay includes all v1 role/personality/template records.
- Draft create/update/shuffle/template emits `TeamDraftNotify` and
  mutates only server-owned draft state.
- Draft commit creates team + manager + reports atomically or emits a
  visible command error while preserving the draft.
- Profile metadata persists, replays, migrates from older store files,
  appears in manager roster context, and appears in
  `tyde_team_describe`.
- Server restart: members load with `current_agent_id: None`;
  next message activates them correctly.

Frontend (wasm-bindgen-test, per `CLAUDE.md`):

- Teams panel renders one row per `TeamId`.
- Opening a team navigates to the manager's `/agent/<id>` stream.
- Teams panel member rows show live binding state and update on
  `TeamMemberBindingNotify`.
- New-team dialog renders catalog-driven templates/role/personality
  controls, and shuffle/template clicks update only through
  `TeamDraftNotify`.
- Editing derived draft fields keeps name/description/backend/cost/
  project/custom-agent controls editable while preserving server-owned
  profile semantics.
- Creating from the dialog sends `TeamDraftCommit`; it does not emit
  frontend `TeamCreate` + repeated `TeamMemberCreate`.
- Chat view does not mount a separate team roster sidebar.
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

4. **Concurrency / budget cap per team.** Not in v1. Without the
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
has an explicit backend/cost profile, may bind a `CustomAgent`
(role / tools / instructions), owns a `SessionId` (rolling memory),
binds one or more projects, and is reanimated on demand via
`SpawnAgent::Resume`. Workspace roots are derived as the union of
the member's project roots at spawn time.
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
