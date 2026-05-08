# Agent Teams

Spec for **Agent Teams**: persistent, server-owned teams of agents organized
by a manager/report relationship, fed by a kanban board, with long-lived
memory maintained by Tyde-managed compaction so agents "vaguely remember"
prior work over time.

Audience: implementation agents and future maintainers.

This doc represents consensus between Claude and Codex design proposals
(see `dev-docs/proposals/agent-teams-claude.md`,
`dev-docs/proposals/agent-teams-codex.md`, and the two cross-review files
in the same directory). It is the spec to implement from. Where the
two proposals disagreed, the choices made here are explained in §16.

It builds on:

- `01-philosophy.md` — non-negotiable architecture rules.
- `03-agents.md` — agent lifecycle and event model.
- `06-projects.md` — host-owned domain pattern (replayed events,
  on-disk store).
- `11-agent-control-mcp.md` — embedded loopback MCP surface.
- `15-sub-agents.md` — `AgentOrigin`, parent/child plumbing.
- `16-queued-messages.md` — actor-owned per-agent queue.
- `17-custom-agents.md` — `CustomAgent`, `Steering`, `Skill`, `McpServer`.

The whole spec can be reduced to one structural claim:

> A **team member** is a durable, server-owned identity (`TeamMemberId`)
> with persistent memory and an org-chart role. The current live
> `AgentId` is a runtime binding, not the identity. Members are reanimated
> across backend session restarts, server restarts, and compactions by
> spawning a fresh agent and injecting the saved memory. The kanban
> **board** is the work intake; the **manager** is woken by a server-owned
> coordinator when the board needs human-style judgment.

Everything else (columns, tools, frontend) is mechanism.

---

## 1. Goals & Non-Goals

### Goals

- **Durable team membership** that survives backend session restarts,
  server restarts, compaction, and live-agent replacement.
- **Flat org chart**: each team has exactly one manager and N direct
  reports.
- **Server-owned kanban board**: humans drop work into `Backlog`; the
  manager pulls and delegates; reports work; the manager reviews.
- **Long-lived per-member memory**: a rolling summary plus a bounded
  verbatim recent-turn tail, persisted on the server, replayed into a
  fresh backend session whenever Tyde rebinds.
- **Backend-agnostic compaction**: Tyde-managed memory is the canonical
  durable record. We do not rely on backend-native `/compact` in v1.
- **Single source of truth in Rust**: every concept (team, member, card,
  event, memory) is a typed `protocol/src/types.rs` record. The server
  emits replay+live events. The frontend renders.
- **Local and remote hosts are identical** from the frontend's
  perspective. Teams are a host-owned domain like projects.

### Non-Goals (v1)

- Nested teams. A team has members, not sub-teams.
- Multiple managers per team, or matrix delegation.
- Cross-team agent membership. A `TeamMemberId` belongs to exactly one
  team.
- Reports promoting peers, hiring helpers, or otherwise mutating the
  org. Org changes are human-only.
- Agent-created teams. Humans create teams.
- Free-form delegation. Managers delegate only to direct reports.
- Custom kanban columns. Columns are a fixed enum.
- Backend-native `/compact` integration. Tyde-managed memory only.
- Cost / budget tracking and concurrency caps. (Real concern; deferred —
  see §16 open questions.)
- Project-scoped team templates. Teams may carry an optional
  `project_id`, but there are no template/inheritance semantics.
- Migrating an existing live agent into a team. Members are created
  with the team; new members are spawned as members from day one.
- User-editable memory through the UI. Memory is read-only in v1; users
  can request a manual compaction.

---

## 2. Conceptual Model

```
┌─────────┐  has 1   ┌──────────────┐
│  Team   │─────────▶│  TeamBoard   │
│         │   has N  └──────┬───────┘
│         │──────────┐      │ has N
└────┬────┘  has 1   │      ▼
     │ manager       │  ┌────────────┐  has N  ┌──────────────┐
     │               │  │ TeamCard   │────────▶│TeamCardEvent │
     ▼               │  └────┬───────┘         │ (append-only)│
┌──────────────┐ has N│       │                └──────────────┘
│  TeamMember  │◀─────┘       │ assignee 0..1
│ (Manager or  │              ▼
│  Report)     │       ┌──────────────┐
│              │       │  TeamMember  │ (must be in same team)
│ TeamMemberId │       └──────────────┘
└──────┬───────┘
       │ has 1
       ▼
┌──────────────┐    1:1    ┌────────────────────┐
│TeamMemberId  │──────────▶│  TeamMemberMemory  │
│ (durable)    │           │ summary_markdown,  │
│              │           │ recent_turns_text, │
└──────┬───────┘           │ active_card_ids,   │
       │ may be bound to   │ generation         │
       ▼                   └────────────────────┘
┌──────────────────┐    typed binding event:
│ live AgentId     │    TeamMemberBindingNotify
│ (runtime only,   │    { member_id, current_agent_id, status }
│  may be absent)  │
└──────────────────┘
```

### Definitions

- **Team** — a host-owned organization unit. Has `TeamId`, `name`,
  optional `project_id`, `workspace_roots`, exactly one active manager,
  and exactly one `TeamBoard`.
- **TeamMember** — a durable persistent member identity
  (`TeamMemberId`). Has a role, backend kind, optional
  `custom_agent_id`, `state` (Active/Paused/Archived), and a
  `TeamMemberMemory`. *Does not* have a stable `AgentId`.
- **Manager** — `TeamMemberRole::Manager`. Exactly one per team. Wakes
  on board events; pulls work; delegates; reviews.
- **Report** — `TeamMemberRole::Report`. Receives delegated cards;
  cannot assign work to others.
- **Memory** — server-owned rolling memory record per `TeamMemberId`.
  Independent of any current live `AgentId`. Updated by compaction;
  injected into future spawns.
- **Live AgentId binding** — runtime-only mapping between
  `TeamMemberId` and the currently-running `AgentId`. Surfaced via
  `TeamMemberBindingNotify`. Never persisted as durable identity. After
  any restart, all bindings are `None` until the coordinator rebinds
  them.
- **TeamBoard** — one kanban board per team.
- **TeamCardColumnKind** — fixed enum: `Backlog`, `Triage`,
  `Assigned`, `InProgress`, `Blocked`, `Review`, `Done`, `Canceled`.
- **TeamCard** — durable work item. Has `column`, optional
  `manager_member_id` and `report_member_id`, `version: u64` (for
  optimistic concurrency), title, body, position.
- **TeamCardEvent** — append-only typed activity log entry. Carried on
  a separate notify frame from card snapshots.

The existing `Agent` from `03-agents.md` is unchanged. A live team-member
agent is a normal `Agent` with `AgentOrigin::TeamMember` and
`team_id`/`team_member_id` populated on its birth-certificate payload.

---

## 3. Persistence Model

### Choice: SQLite at `~/.tyde/teams.db`

This is a **new persistence pattern** for Tyde2. Existing host-owned
domains (`projects.json`, `custom_agents.json`, `steering.json`) are JSON
blobs on disk. Teams are different in two ways that justify the cost of
a new dependency:

1. **Cards mutate frequently** (drag, claim, assign, status, comment).
   A populated team's board does many writes per minute when active.
   Full-file JSON rewrites scale poorly with hundreds of cards and
   thousands of activity events.
2. **Activity history is append-only and large.** A
   `team_card_events` table with an index is a natural fit. Storing it
   inside the same JSON file as the live state forces a rewrite of
   every event for every event.

We use **rusqlite** in WAL mode. The protocol shape is unchanged
behind the persistence choice — if the dependency proves problematic
later, the store can be swapped without churning the wire.

### Single-writer actor

A `TeamStoreActor` owns the `rusqlite::Connection`. All mutations
serialize through it via mpsc. No `Arc<Mutex<Connection>>`. Reads go
through the actor too in v1; if reads become a bottleneck we'll add a
benchmarked read path then.

The store actor deserializes SQL rows into protocol enums/structs.
**SQL strings are not load-bearing.** The store fails loudly on
unrecognized values rather than fallback-defaulting.

### Schemas

```sql
CREATE TABLE teams (
    team_id           TEXT PRIMARY KEY,         -- TeamId
    name              TEXT NOT NULL,
    project_id        TEXT,                     -- ProjectId, optional
    workspace_roots   TEXT NOT NULL,            -- JSON array<String>
    manager_member_id TEXT NOT NULL,            -- TeamMemberId
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    archived_at_ms    INTEGER                   -- soft delete; NULL while live
);

CREATE TABLE team_members (
    member_id         TEXT PRIMARY KEY,         -- TeamMemberId
    team_id           TEXT NOT NULL REFERENCES teams(team_id),
    role              TEXT NOT NULL,            -- TeamMemberRole, snake_case
    state             TEXT NOT NULL,            -- TeamMemberState
    name              TEXT NOT NULL,
    description       TEXT NOT NULL,            -- free-form prose for manager prompts
    backend_kind      TEXT NOT NULL,            -- BackendKind
    custom_agent_id   TEXT,                     -- CustomAgentId, optional
    project_id        TEXT,
    workspace_roots   TEXT NOT NULL,            -- JSON array
    last_session_id   TEXT,                     -- audit only; not used for resume
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
);
CREATE UNIQUE INDEX team_one_active_manager
    ON team_members(team_id) WHERE role = 'manager' AND state = 'active';

CREATE TABLE team_boards (
    board_id          TEXT PRIMARY KEY,
    team_id           TEXT NOT NULL REFERENCES teams(team_id),
    name              TEXT NOT NULL,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    archived_at_ms    INTEGER
);

CREATE TABLE team_cards (
    card_id           TEXT PRIMARY KEY,
    board_id          TEXT NOT NULL REFERENCES team_boards(board_id),
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    column_kind       TEXT NOT NULL,            -- TeamCardColumnKind
    position          REAL NOT NULL,            -- fractional rank within column
    manager_member_id TEXT,                     -- TeamMemberId, optional
    report_member_id  TEXT,                     -- TeamMemberId, optional
    version           INTEGER NOT NULL,         -- bumped on every mutation
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
);
CREATE INDEX cards_by_board_column ON team_cards(board_id, column_kind, position);

CREATE TABLE team_card_events (
    event_id          TEXT PRIMARY KEY,         -- TeamCardEventId
    card_id           TEXT NOT NULL REFERENCES team_cards(card_id),
    actor_json        TEXT NOT NULL,            -- TeamCardActor (JSON)
    event_json        TEXT NOT NULL,            -- TeamCardEventKind (JSON)
    created_at_ms     INTEGER NOT NULL
);
CREATE INDEX events_by_card ON team_card_events(card_id, created_at_ms);

CREATE TABLE team_member_memory (
    member_id         TEXT PRIMARY KEY REFERENCES team_members(member_id),
    generation        INTEGER NOT NULL,
    summary_markdown  TEXT NOT NULL,
    recent_turns_text TEXT NOT NULL,            -- bounded; ~4 KB target
    active_card_ids   TEXT NOT NULL,            -- JSON array<TeamCardId>
    source_compaction_id TEXT,                  -- TeamCompactionId, optional
    updated_at_ms     INTEGER NOT NULL
);

CREATE TABLE team_compactions (
    compaction_id     TEXT PRIMARY KEY,
    member_id         TEXT NOT NULL REFERENCES team_members(member_id),
    trigger           TEXT NOT NULL,            -- TeamCompactionTrigger
    status            TEXT NOT NULL,            -- TeamCompactionStatus
    started_at_ms     INTEGER NOT NULL,
    completed_at_ms   INTEGER,
    previous_generation INTEGER NOT NULL,
    next_generation   INTEGER,
    error             TEXT
);
```

### Validation

The store actor validates on load and on every mutation. No silent
repair; loud failure on invariant violation:

- Every member references an existing team.
- Each team has exactly one active manager (enforced by partial unique
  index above).
- Reports' `manager_member_id` resolves to that team's active manager
  (denormalized for read clarity; checked on write).
- A `TeamMemberId` belongs to exactly one team.
- Cards reference a board in their team. Card `manager_member_id` and
  `report_member_id`, when set, reference members of that team.
- Card column/assignment invariants: `Assigned` requires a
  `report_member_id`; `Triage` requires a `manager_member_id`.
- Memory and compaction records reference existing members.

### Runtime live-agent binding

The live `AgentId` for a member is **runtime state**, never written to
SQLite as durable identity. It is emitted as `TeamMemberBindingNotify`.
After a server restart, the coordinator emits `current_agent_id: None`
for every member until it rebinds them.

`team_members.last_session_id` is **audit only** — kept so a user can
trace which session an old member last bound to. Team-member continuity
is memory injection, not session resume (§5).

---

## 4. Org Structure

V1 invariants — chosen to make invalid states unrepresentable:

- A team has exactly one active manager.
- A report's manager is its team's manager.
- A `TeamMemberId` belongs to exactly one team.
- A manager cannot also be a report in the same team.
- Teams do not nest. No `parent_team_id`.
- Cards cannot cross teams.
- Archiving a member with non-terminal assigned cards is rejected. The
  user must reassign or close those cards first.

**Manager replacement** is human-only via `TeamSetManager`. The new
manager must already be a `Report` of the team. The transition is
atomic in one transaction (old manager → report; chosen report →
manager). No auto-promotion when a manager dies; the team blocks new
delegation until a human replaces the manager.

The restriction is deliberate. Once you allow many-to-many
manager/report or nested teams, you have a delegation graph, and
graphs have cycles, and the kanban mental model breaks. The protocol
shape (`TeamMemberRole` is an enum, `Team` has a single
`manager_member_id`) is forward-compatible with adding nesting later
as a clean extension.

---

## 5. Memory & Compaction

### What "compaction" means in v1

Compaction is **updating server-owned memory**, full stop. We do not
invoke any backend's native `/compact`. The reason is observability:
native `/compact` rewrites the live session's context but produces no
artifact Tyde can persist, so it cannot be the durable cross-restart
record. Tyde-managed memory is.

When a member needs to act and has no live `AgentId`, Tyde:

1. Reads the member's `TeamMemberMemory` and `CustomAgent`/`Steering`
   resolution.
2. Spawns a fresh backend session of the member's `BackendKind`.
3. Injects the initial prompt:
   `role_card + summary_markdown + recent_turns_text + active_card_ids
   summary + (incoming work prompt if any)`.
4. Emits `TeamMemberBindingNotify` with the new `current_agent_id`.

If the member already has a live binding, work is queued onto its agent
via the existing queue actor (`16-queued-messages.md`).

### Memory schema

```rust
pub struct TeamMemberMemory {
    pub member_id: TeamMemberId,
    pub generation: u64,
    pub summary_markdown: String,        // human-readable rolling summary
    pub recent_turns_text: String,       // bounded verbatim tail, ~4 KB
    pub active_card_ids: Vec<TeamCardId>,
    pub source_compaction_id: Option<TeamCompactionId>,
    pub updated_at_ms: u64,
}
```

We deliberately keep the schema small:

- `summary_markdown` is prose. Users will eventually want to inspect
  this; markdown is friendlier than structured JSON.
- `recent_turns_text` is the verbatim tail. Verbatim matters for
  pronoun resolution and "as I said earlier" continuity. Bounded so
  injection doesn't blow the next session's context.
- `active_card_ids` is the only structured field — it is unambiguous
  (derivable from card state) and lets the spawn prompt cite the
  member's open work without parsing markdown.

We considered structured `open_commitments` (Codex's original
proposal) and rejected it for v1. It is a fuzzy-output ask — what
counts as a "commitment" vs. a "decision" vs. a regular note? — and
LLMs handle that category poorly. If we need structured commitments,
add them later.

### Triggers

The `TeamCoordinator` checks compaction eligibility only when a
member's `TypingStatusChanged(false)` fires (i.e. the member is idle).

```rust
pub enum TeamCompactionTrigger {
    TokenThreshold,      // ContextBreakdown.input_tokens > 0.6 * context_window
    CardBoundary,        // assigned card moved to Done | Blocked | Canceled
    Manual,              // human or self requested
    RestartRecovery,     // server restart found stale memory generation
}
```

No wall-clock-idle trigger. Compacting an idle agent that hasn't
moved is wasted spend; the threshold trigger fires anyway when work
resumes.

At most one compaction in flight per member. Additional triggers
coalesce into one pending reason.

### Compactor

A **`MemoryCompactor`** is an internal one-shot agent — not a team
member. It is not on the board, has no memory of its own, and is
terminated as soon as it produces output.

- Spawned via the existing `Backend` trait. Default
  `BackendKind::Tycode` with `CostHint::Low`. Configurable per host
  setting.
- Receives a deterministic prompt template containing:
  - the previous `summary_markdown`
  - all turns since the last compaction (read from the member's session
    history; Tyde already has it)
  - a manifest of card outcomes since last compaction (titles, final
    states, key transitions)
  - the role card
- Produces output via a single typed tool call:
  `submit_compaction({ summary_markdown, recent_turns_text,
  active_card_ids })`. The shape is enforced by the tool schema.
- On success: the store actor commits a new memory row, bumps
  `generation`, writes a `team_compactions` row, emits
  `TeamMemoryNotify::Updated` and `TeamCompactionNotify::Completed`.
- On failure (tool not called, schema violation, timeout): the prior
  generation stays current; emits
  `TeamCompactionNotify::Failed { error }`. Loud, no partial update.

Why external compactor and not self-compaction (the member calling
`tyde_submit_team_memory_update`)? Two reasons:

1. **Deterministic prompt**: Tyde controls the input. The member's
   own context could be anywhere — mid-card, mid-thought, partial
   tool output. The compactor sees a clean slate and a structured
   input.
2. **Cost predictability**: a small, fast model on a bounded prompt.
   The member could be on an expensive model with a large live
   context.

Self-compaction is reconsidered in §16 if the external compactor's
output quality proves poor.

### Generation drift

When the coordinator goes to bind a fresh `AgentId` to a member, it
checks the member's memory generation against the live session's
start-time generation (recorded when the binding was made). If they
diverge by more than 1, the live session is too stale; the coordinator
gracefully terminates it and respawns with the latest memory before
delivering the new work. (We never restart mid-turn.)

### Memory across server restarts

`teams.db` is on disk. After a server restart, all memory records are
intact. No member has a live `AgentId`. The coordinator, on first wake
need (a card in `Backlog`, a non-terminal `Assigned` card, etc.),
spawns a fresh backend session per §5 and emits a new
`TeamMemberBindingNotify`.

This is also why team members do **not** use the
`05-session-resume.md` machinery. Session resume is for transient,
single-task agents. Team-member continuity is memory injection.
`team_members.last_session_id` is kept for audit only.

---

## 6. Task / Card Lifecycle

### Columns

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamCardColumnKind {
    Backlog,
    Triage,
    Assigned,
    InProgress,
    Blocked,
    Review,
    Done,
    Canceled,
}
```

Six live + two terminal columns. The split between `Backlog → Triage →
Assigned` exists because "manager has accepted responsibility" and
"manager has chosen an assignee" are different states — useful for
failure handling (claim leases, §6.3) and useful for the user to see
which cards the manager is sitting on.

### Allowed transitions

```text
Backlog    → Triage      (manager only)
Backlog    → Canceled    (human only)
Triage     → Assigned    (manager only)
Triage     → Backlog     (manager: release; coordinator: claim lease expired)
Triage     → Canceled    (manager or human)
Assigned   → InProgress  (assigned report only)
Assigned   → Canceled    (manager or human)
InProgress → Review      (assigned report only)
InProgress → Blocked     (assigned report only; coordinator on report failure)
Blocked    → InProgress  (assigned report only)
Blocked    → Assigned    (manager re-prompts)
Blocked    → Canceled    (manager or human)
Review     → Done        (manager or human)
Review     → Assigned    (manager rejects, sends back)
Any non-terminal → Canceled (human override always allowed)
```

Anything not in this table is rejected by the server with a
`CommandError` (`InvalidInput`, `Forbidden`, or `Conflict` — see §11).

**Reports cannot mark cards `Done`.** This keeps the manager role
meaningful.

### Manager claim lease

When the manager moves a card `Backlog → Triage`, a server-owned
**claim lease** starts (default 5 minutes; configurable in
`HostSettings`). If the manager has not moved the card to `Assigned`
or `Canceled` by lease expiry:

- The coordinator moves the card back to `Backlog`.
- It appends a `ManagerClaimExpired { manager_member_id }` activity
  event.
- The manager is woken with a coalesced "your claim expired on N
  cards" prompt the next time it idles.

This handles "manager crashes mid-triage" without leaving cards
invisibly stuck.

### Optimistic concurrency

Every card mutation payload carries `expected_version: u64`. The store
actor compares against the current row's `version`. On mismatch, the
mutation is rejected with `CommandErrorCode::Conflict` and the server
re-emits a fresh `TeamCardNotify::Upsert` so all clients reconcile.

This catches the human-vs-manager race ("user drags a card while the
manager is assigning it") cleanly and without silent fallbacks.
Actor serialization gives ordering, not preconditions; we need both.

### Events emitted

For every accepted card mutation, the server emits **two** frames on
the team's stream:

1. `TeamCardNotify::Upsert { card }` — the new full snapshot.
2. `TeamCardActivityNotify { event }` — one append-only typed event.

This keeps card snapshots small (no embedded history) and the activity
log queryable as a separate stream.

For deletes / archives, `TeamCardNotify::Delete { card }` carries the
**full** prior record (not just the ID), matching the project precedent
in `06-projects.md`.

---

## 7. Manager Loop

### The manager does not poll

A `TeamCoordinator` actor (one per live team) subscribes to its team's
events and the typing-status streams of its members. It wakes the
manager only when something needs human-style judgment. Wake delivery
goes through the existing queued-message mechanism
(`16-queued-messages.md`) — the work is the card; the queued
`SendMessage` is just a notification.

### Wake triggers

1. A new card lands in `Backlog`.
2. A card has been stuck in `Triage` past its claim lease.
3. A report moved a card to `Review`.
4. A report moved a card to `Blocked`.
5. A report's live agent failed or closed while assigned.
6. Server restart with non-terminal cards (replay-driven wake).
7. Compaction completed and the member's `active_card_ids` changed.
8. Human explicitly requests manager attention on a card.

### Coalescing

At most one outstanding wake message exists for a manager at any time.
If the manager is busy and five new triggers fire, they coalesce into
one prompt summarising what changed: "since you last looked, 3 new
backlog cards, 1 review waiting, 2 reports blocked." The coalesced
prompt is delivered once the manager idles.

### Wake prompt content

Server-authored. Bounded. Deterministic. Contains:

- Team and member IDs.
- Manager role instructions (from `CustomAgent`/`Steering`).
- Cards needing manager action: typed snapshots with title, body
  excerpt, current column, assignee.
- Roster of reports: `member_id`, name, free-form `description`,
  current open card count, last live status (idle/thinking/failed),
  last-failure summary if any.
- Memory hint (`summary_markdown` excerpt + `active_card_ids`).
- Explicit instruction to use the team MCP tools (§9).

Raw card history is not included in the wake prompt. The manager pulls
it explicitly via `tyde_team_read_card`.

### Assignment flow

1. Manager calls
   `tyde_team_assign_card({ card_id, report_member_id, expected_version })`.
2. Server validates: caller is the team's manager; report is a `Report`
   of the same team in `Active` state; card version matches; card's
   current column allows transitioning to `Assigned`.
3. Server moves card → `Assigned`, bumps `version`, appends a
   `ReportAssigned { manager_member_id, report_member_id }` activity
   event.
4. Server delivers a `SendMessage` to the **report** with the card
   body. If the report has no live binding, the coordinator first
   spawns a fresh agent per §5; then queues the prompt.
5. Subscribers receive `TeamCardNotify::Upsert` and
   `TeamCardActivityNotify`.

The manager does not message the report directly. The team is the
mediator. This is what makes the kanban the source of truth: a queued
`SendMessage` to a non-running report can disappear (queue is runtime-
only, per `16-queued-messages.md`); a persisted card assignment
survives and is redelivered on the next spawn.

---

## 8. Protocol Changes

All in `protocol/src/types.rs`. Frontend types are generated from these
definitions per the codegen rule.

### 8.1 Typed IDs

```rust
#[derive(..., Serialize, Deserialize)] #[serde(transparent)]
pub struct TeamId(pub String);
pub struct TeamMemberId(pub String);
pub struct TeamBoardId(pub String);
pub struct TeamCardId(pub String);
pub struct TeamCardEventId(pub String);
pub struct TeamCompactionId(pub String);
```

### 8.2 Enums

```rust
#[derive(..., Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberRole { Manager, Report }

pub enum TeamMemberState { Active, Paused, Archived }

pub enum TeamCardColumnKind {
    Backlog, Triage, Assigned, InProgress, Blocked, Review, Done, Canceled,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamCardActor {
    Human,
    Member { member_id: TeamMemberId },
    Server,
}

pub enum TeamCompactionTrigger {
    TokenThreshold, CardBoundary, Manual, RestartRecovery,
}

pub enum TeamCompactionStatus { Started, Completed, Failed }
```

### 8.3 `AgentOrigin` extension

Add a new variant:

```rust
pub enum AgentOrigin {
    User,
    AgentControl,
    BackendNative,
    TeamMember,        // NEW
}
```

Extend `AgentStartPayload` and `NewAgentPayload`:

```rust
pub team_id: Option<TeamId>,
pub team_member_id: Option<TeamMemberId>,
```

Validation: `AgentOrigin::TeamMember` requires both fields to be
`Some`; all other origins require both to be `None`. This is a
protocol-level invariant — no inference from `parent_agent_id` etc.

### 8.4 Records

```rust
pub struct Team {
    pub id: TeamId,
    pub name: String,
    pub project_id: Option<ProjectId>,
    pub workspace_roots: Vec<String>,
    pub manager_member_id: TeamMemberId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub archived_at_ms: Option<u64>,
}

pub struct TeamMember {
    pub id: TeamMemberId,
    pub team_id: TeamId,
    pub role: TeamMemberRole,
    pub state: TeamMemberState,
    pub name: String,
    pub description: String,
    pub backend_kind: BackendKind,
    pub custom_agent_id: Option<CustomAgentId>,
    pub project_id: Option<ProjectId>,
    pub workspace_roots: Vec<String>,
    pub last_session_id: Option<SessionId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

pub struct TeamBoard {
    pub id: TeamBoardId,
    pub team_id: TeamId,
    pub name: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub archived_at_ms: Option<u64>,
}

pub struct TeamCard {
    pub id: TeamCardId,
    pub board_id: TeamBoardId,
    pub title: String,
    pub body: String,
    pub column: TeamCardColumnKind,
    pub position: f64,
    pub manager_member_id: Option<TeamMemberId>,
    pub report_member_id: Option<TeamMemberId>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamCardEventKind {
    Created,
    Moved          { from: TeamCardColumnKind, to: TeamCardColumnKind },
    ManagerClaimed { manager_member_id: TeamMemberId },
    ManagerClaimExpired { manager_member_id: TeamMemberId },
    ReportAssigned { manager_member_id: TeamMemberId, report_member_id: TeamMemberId },
    NoteAdded      { body: String },
    Blocked        { reason: String },
    ReviewRequested{ summary: String },
    Completed,
    Canceled       { reason: String },
}

pub struct TeamCardEvent {
    pub id: TeamCardEventId,
    pub card_id: TeamCardId,
    pub actor: TeamCardActor,
    pub event: TeamCardEventKind,
    pub created_at_ms: u64,
}

pub struct TeamMemberMemory {
    pub member_id: TeamMemberId,
    pub generation: u64,
    pub summary_markdown: String,
    pub recent_turns_text: String,
    pub active_card_ids: Vec<TeamCardId>,
    pub source_compaction_id: Option<TeamCompactionId>,
    pub updated_at_ms: u64,
}

pub struct TeamCompactionRecord {
    pub id: TeamCompactionId,
    pub member_id: TeamMemberId,
    pub trigger: TeamCompactionTrigger,
    pub status: TeamCompactionStatus,
    pub started_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub previous_generation: u64,
    pub next_generation: Option<u64>,
    pub error: Option<String>,
}

pub struct TeamMemberBindingPayload {
    pub member_id: TeamMemberId,
    pub current_agent_id: Option<AgentId>,
    pub status: AgentControlStatus,
}
```

### 8.5 Streams

Two stream patterns:

- **`/host/<uuid>`** — host stream gains team summaries + team CRUD.
  Carries the *list* of teams so a frontend can render the team
  selector. Mirrors `ProjectNotify`.
- **`/team/<team_id>/<instance_id>`** — per-subscriber team detail
  stream. Carries members, boards, card snapshots, card activity,
  memory, compactions, and bindings for one team. Subscribers attach
  via a typed `TeamSubscribe` input on the host stream and detach via
  `TeamClose`. Same pattern as project streams in `07-project-stream.md`.

This split keeps host replay light (a frontend that lists teams
doesn't need every card of every team) and follows the existing
`Project` precedent.

### 8.6 Input frame kinds (host stream)

```rust
TeamCreate
TeamRename
TeamArchive
TeamSetManager
TeamMemberCreate
TeamMemberUpdate
TeamMemberArchive
TeamSubscribe          // open a /team/<id>/<instance> stream
TeamClose              // close a /team/<id>/<instance> stream
TeamBoardRename
TeamCardCreate
TeamCardUpdate
TeamCardMove           // typed transition with expected_version
TeamCardClaim          // Backlog → Triage; manager only
TeamCardAssignReport   // Triage → Assigned; manager only
TeamCardAddNote
TeamCardArchive
TeamMemberCompactNow
```

Every card mutation payload includes
`expected_version: u64`.

### 8.7 Output frame kinds

On `/host/<uuid>` (after existing replay):

```rust
TeamNotify  // Upsert { team } | Delete { team }
```

On `/team/<team_id>/<instance_id>`:

```rust
TeamStart                  // seq 0; carries Team and TeamBoard
TeamMemberNotify           // Upsert { member } | Delete { member }
TeamMemberBindingNotify    // payload defined above
TeamCardNotify             // Upsert { card } | Delete { card }
TeamCardActivityNotify     // { event: TeamCardEvent }
TeamMemoryNotify           // Updated { memory: TeamMemberMemory }
TeamCompactionNotify       // { record: TeamCompactionRecord }
```

`Notify` payloads use the tagged `Upsert | Delete` pattern from
existing host domains. Delete payloads carry the **full** prior record
(matching `06-projects.md`), so subscribers can render archive views
without re-reading.

### 8.8 Replay ordering

On host attach:

1. `HostSettings`
2. existing host prelude
3. `ProjectNotify`
4. `McpServerNotify`
5. `SkillNotify`
6. `SteeringNotify`
7. `CustomAgentNotify`
8. **`TeamNotify`** — list of teams (summaries only)
9. existing live `NewAgent` events

On team attach (`TeamSubscribe`):

1. `TeamStart` (seq 0; carries team + board)
2. `TeamMemberNotify::Upsert` for each member
3. `TeamCardNotify::Upsert` for each non-archived card
4. `TeamCardActivityNotify` for each persisted card event, in order
5. `TeamMemoryNotify::Updated` for each member's current memory
6. `TeamCompactionNotify` for the most recent N compactions per member
   (default N=5) — older compactions paginated on demand
7. `TeamMemberBindingNotify` for each member (with
   `current_agent_id: None` for any not currently bound)

### 8.9 Validation in `protocol/src/validator.rs`

- `TeamCreate.manager_member_id` must reference a member created in the
  same payload (atomic team-with-manager creation), or an existing
  member already in `Active` state with no current team.
- `TeamCardClaim` rejected if caller is not the team's active manager.
- `TeamCardAssignReport.report_member_id` must be a `Report` of the
  same team in `Active` state.
- `TeamCardMove.to` must be reachable from the card's current column
  per the §6 transition table, with the actor permitted for that
  transition.
- Stale `expected_version` → `Conflict`.
- `AgentOrigin::TeamMember` requires both `team_id` and
  `team_member_id`; other origins require both to be `None`.

These shape the wire so the worst classes of invalid input are
unrepresentable.

---

## 9. MCP Surface

### Decision: tools, mapped 1:1 to typed protocol commands

Managers and reports drive the team via the existing **agent-control
MCP** (`11-agent-control-mcp.md`). The MCP server is already injected
into every spawned agent. New team tools are **thin shims** over the
same `TeamCoordinator` mutation path used by the human UI. There is no
parallel command model — the MCP layer is a callable wrapper.

Caller identity is derived from the loopback URL injection (existing
pattern). A member cannot impersonate another member by passing a
different ID. A report calling `tyde_team_assign_card` is rejected
because the server knows the caller's role.

### New tools

| Tool                         | Maps to                          | Caller                    |
|------------------------------|----------------------------------|---------------------------|
| `tyde_team_list`             | (read)                           | any team member           |
| `tyde_team_describe`         | (read: team + members + memory)  | any team member           |
| `tyde_team_read_board`       | (read: cards by column)          | team member               |
| `tyde_team_read_card`        | (read: full card + activity)     | team member               |
| `tyde_team_claim_card`       | `TeamCardClaim`                  | manager only              |
| `tyde_team_assign_card`      | `TeamCardAssignReport`           | manager only              |
| `tyde_team_move_card`        | `TeamCardMove`                   | per §6 transition matrix  |
| `tyde_team_add_card_note`    | `TeamCardAddNote`                | team member               |
| `tyde_team_compact_member`   | `TeamMemberCompactNow`           | manager (any) or self     |

Tools deliberately **omitted** in v1:

- `tyde_team_create` / `tyde_team_add_member` — org changes are
  human-only.
- `tyde_team_send_to_report` — reports receive work *only* via card
  assignment. This is what makes the kanban the source of truth.

If `HostSettings.tyde_agent_control_mcp_enabled` is `false`, team
loops pause visibly. Humans can still operate the board manually.

---

## 10. Frontend Surface

Brief — Mike will iterate on UI later. v1 just needs to render
protocol events; no client-side caches, no client-side board logic.

### 10.1 Teams panel

Sibling to the existing Projects/Sessions/Agents panels:

- List of teams: name, member count, open card count, manager indicator,
  archived badge.
- "New team" wizard: name, optional project, create manager
  (pick `BackendKind` + `CustomAgent`), create reports the same way.
  Org changes go through typed protocol commands.

### 10.2 Board view

- Fixed columns: `Backlog | Triage | Assigned | InProgress | Blocked
  | Review`. `Done` and `Canceled` collapsed into a footer drawer.
- Cards keyed by `TeamCardId` (philosophy reactivity rule).
- Drag/drop emits `TeamCardMove` with `expected_version`.
- Click → side panel: title, body, current assignments, activity log
  (from `TeamCardActivityNotify` stream), "open agent chat" button
  jumping to the assignee's `/agent/...` stream.

### 10.3 Member cards

In the team header / sidebar:

- Each member: name, role, backend/custom-agent labels, live status
  (from `TeamMemberBindingNotify` + agent typing status).
- Memory generation, last compaction time, `summary_markdown` preview,
  list of `active_card_ids`.
- Click → expands to full memory (lazy-loaded; rolling summaries can
  be ~2K tokens).
- "Compact now" button.

### 10.4 No new dispatch primitives

Everything renders from `TeamNotify`, `TeamMemberNotify`,
`TeamCardNotify`, `TeamCardActivityNotify`, `TeamMemoryNotify`,
`TeamCompactionNotify`, `TeamMemberBindingNotify`.

No refresh button; views update from server events only.

---

## 11. Failure Modes

### Manager crashes mid-triage

Card sits in `Triage`. Claim lease (default 5 min) expires → coordinator
moves the card back to `Backlog`, appends `ManagerClaimExpired` activity
event, and surfaces in UI. On manager respawn, the card is in `Backlog`
and will appear as a fresh wake trigger. No invisible stuck state.

### Manager dies and won't come back

Team blocks new claims/assignments. UI shows the team in a degraded
state. Human must `TeamSetManager` to a current report. No
auto-promotion.

### Report can't complete a card

Report moves card → `Blocked` with a reason. Manager wakes (trigger 4),
decides: send back to `InProgress` with guidance, reassign to a
different report (`Blocked → Assigned`), or cancel.

### Report dies while assigned

Coordinator detects (typing status `Failed` or session closed) and,
after a configurable grace period, moves the card →
`Blocked { reason: "assignee unavailable" }` with `actor: Server`.
Manager wakes. This is an explicit typed transition, not silent
recovery.

### Compaction fails

Tool not called, schema invalid, timeout: the prior memory generation
stays current. `TeamCompactionNotify::Failed { error }` is emitted. If
the failure was due to hard context pressure on the live binding, the
coordinator terminates the live agent and respawns from the
last-good memory before the next work delivery.

### Compaction loses important context

Compaction is destructive — that's the trade. Mitigations:

- `recent_turns_text` is verbatim, so the immediate "what we were
  doing" tail is intact.
- Card activity history is durable and queryable (separate stream).
- Compaction generation is included in `TeamMemoryNotify`. The user
  can see when memory shifted and inspect the prior generation via
  `team_compactions`.
- We do **not** try to detect "important" context heuristically; that
  would violate "no inference."

### Race on a card

Two writers (e.g. human drag + manager assign) hit the actor in some
order. The second's `expected_version` is stale → server emits
`CommandError::Conflict`, re-emits `TeamCardNotify::Upsert` so all
clients reconcile, and the second writer must retry on the new
version.

### Server restart with active cards

`TeamStoreActor` loads everything from `teams.db`. All bindings are
`None`. The coordinator emits replay events in the §8.8 order. On
encountering a non-terminal card, it wakes the relevant manager or
report (which triggers a fresh spawn per §5).

### MCP disabled mid-session

Team autonomous loops pause. UI shows team as paused. Humans can still
operate the board manually. The server does not inject a partial tool
surface.

### Cycles in delegation

Schema-prevented. One manager per team, reports cannot delegate, no
nested teams, no cross-team cards. The org graph is depth-1.

### Member archived while assigned

`TeamMemberArchive` is rejected if the member appears on any
non-terminal card. The user must reassign or close those cards first.

---

## 12. Implementation Order (rough)

1. Protocol types and frame kinds (§8). Generated frontend types fall
   out automatically.
2. `TeamStoreActor` with the schemas in §3, single-writer pattern.
3. `TeamCoordinator` per-team actor + `TeamRegistry`. Mirrors the
   existing agent registry pattern.
4. Host stream replay extension: `TeamNotify` summaries.
5. Per-team detail stream `/team/<id>/<instance>` plumbing.
6. Member spawn-with-memory path: a fresh agent of `BackendKind` with
   role + memory + active-card injection.
7. Card lifecycle: transitions, claim leases, optimistic concurrency,
   activity events.
8. Manager wake triggers + coalescing on top of the queue actor.
9. `MemoryCompactor` internal agent + token-threshold and card-boundary
   triggers.
10. Agent-control MCP team tools (§9).
11. Frontend teams panel + board view + member memory cards.
12. Tests (§13).

---

## 13. Testing

Unit / integration:

- Card transition matrix exhaustively (every `from × to × actor`).
- Optimistic concurrency: stale `expected_version` always rejected.
- Permission table: each tool × caller-role × card-state combination.
- Memory generation drift: spawn → compact → respawn → memory injected.
- Compactor failure paths: tool not called, schema violation, timeout.
- Coalesced wake messages: N triggers → 1 prompt with summary.
- Replay ordering: `TeamSubscribe` → exact frame order, all references
  resolvable.
- Multi-subscriber races: two clients both move the same card.
- Server restart: non-terminal cards resume their wake triggers.

Frontend (wasm-bindgen-test, per `CLAUDE.md`):

- Board renders one card per `TeamCardId`; updates on
  `TeamCardNotify::Upsert`.
- Activity log scrolls in `created_at_ms` order.
- Member card shows memory generation that updates on
  `TeamMemoryNotify::Updated`.
- No frontend caches; clearing the signals re-renders identical DOM.

---

## 14. Glossary cross-reference

| Term in this doc        | Where it's defined                       |
|-------------------------|------------------------------------------|
| `AgentId`               | `03-agents.md` (live agent identity)     |
| `SessionId`             | `05-session-resume.md`                   |
| `AgentOrigin`           | `15-sub-agents.md` + §8.3 here           |
| `CustomAgent` / `Steering` | `17-custom-agents.md`                  |
| `ProjectId`             | `06-projects.md`                         |
| Queued message / actor  | `16-queued-messages.md`                  |
| Loopback MCP server     | `11-agent-control-mcp.md`                |

---

## 15. Forward compatibility

The protocol shape preserves these extension points without breaking
changes:

- `TeamMemberRole` is an enum → can add `SubManager` etc. for nested
  orgs.
- `Team` has no `parent_team_id`; can be added without rewriting
  existing fields.
- `TeamCardColumnKind` is an enum → can add lanes; clients must handle
  unknown variants per the existing protocol convention.
- `TeamCompactionTrigger` can grow new variants (e.g. `WallClockIdle`)
  without invalidating prior records.
- A `TeamCompactionMode` enum (NativeCompact / RestartFromMemory /
  MemoryOnly) was considered for v1 and dropped — every backend is
  effectively `RestartFromMemory` today. Adding the enum later is a
  pure addition: existing records imply `RestartFromMemory`.

---

## 16. Open Questions for Mike

These are deliberate unresolved points; the doc above picked an answer
for each so an implementer is unblocked, but they are the spots where
Mike (or future use) might pick differently. Each is reversible.

1. **Storage = SQLite.** New dependency for Tyde2 (sessions are JSON
   today; there is no existing `session.db` to lean on). The data
   shape — append-heavy activity log, frequent card mutations — makes
   SQLite a real win, but it's a new pattern. Confirm OK to introduce
   `rusqlite` here. If not: fall back to JSON with full-file rewrites,
   accept the rewrite cost, swap later if it bites.

2. **Manager `Done` authority.** Spec lets the manager mark `Review →
   Done` directly; humans can override by reopening or canceling. The
   activity log makes it auditable. If you'd rather every `Done`
   require human acceptance, change the §6 transition table —
   protocol-only change.

3. **Reports spawning transient helpers.** The agent-control MCP's
   `tyde_spawn_agent` lets any agent spawn another. A working report
   may fan out helpers; spec leaves this allowed and invisible to the
   board. If you want helpers tracked as nested cards or disallowed
   for team members, that's a §9 decision — say so before we
   implement.

4. **Concurrency / cost cap per team.** Not in v1. Autonomous loops
   are a money-fire hazard. A `HostSettings.team_max_concurrent_turns`
   cap and per-team spend cap are obvious; deferred only because the
   coordinator-as-event-driven design throttles natural pacing
   already. Confirm OK to defer.

5. **Manager auto-promotion.** When a manager dies fatally, the team
   blocks until a human runs `TeamSetManager`. We could auto-promote
   the longest-tenured report. v1 says no — picking the right
   replacement is a judgment call; humans should make it. If you want
   auto-promotion, easy addition to the coordinator.

6. **Member memory user-editing.** Spec marks memory read-only in v1;
   users can request a manual compaction. A "edit memory" affordance
   (with the agent re-spawned to pick up the edit) is reasonable
   later — confirm OK to defer.

7. **Compactor: external one-shot vs self-compaction.** Spec uses an
   external `MemoryCompactor` (deterministic prompt, cheap model). The
   alternative is self-compaction via an MCP tool the member calls.
   External is more controllable; self-compaction has the member's
   live context for free. If the external compactor's output quality
   is poor in practice, switch to self-compaction — protocol-compatible.

---

## 17. Summary

A team is a server-owned record of `(Team, TeamBoard, TeamMembers,
TeamCards, TeamCardEvents, TeamMemberMemory, TeamCompactions)`.
`TeamMemberId` is durable; `AgentId` is a runtime binding emitted as
`TeamMemberBindingNotify`. The kanban board is the work intake;
managers are LLM agents woken by a `TeamCoordinator` only when
something needs human-style judgment. Compaction is Tyde-managed via
an internal one-shot `MemoryCompactor`, triggered by token threshold
or card boundary. Persistence is SQLite at `~/.tyde/teams.db`,
single-writer through a `TeamStoreActor`. The MCP surface is a thin
shim over typed protocol commands handled by the same coordinator the
UI uses. The frontend is a pure projection.

The biggest design risk is compaction quality. The biggest scope risk
is letting teams nest in v1.
