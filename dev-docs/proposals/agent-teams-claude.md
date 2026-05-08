# Agent Teams (proposal)

Status: proposal, not implemented. Author: Claude. Reviewer: Codex (next).

This document proposes **persistent, server-owned teams of agents** organised
by a manager/report relationship, fed by a kanban board, with long-lived
memory maintained by Tyde-managed compaction. It builds on:

- `01-philosophy.md` — non-negotiable architecture rules.
- `03-agents.md` — agent lifecycle and event model.
- `06-projects.md` — project ownership pattern (replayed host events,
  JSON-on-disk store).
- `11-agent-control-mcp.md` — embedded loopback MCP surface that agents use to
  drive Tyde.
- `15-sub-agents.md` — `AgentOrigin`, relay actors, and the existing
  parent/child plumbing.
- `16-queued-messages.md` — the actor-owned per-agent queue that we will reuse
  to deliver work prompts to busy reports.
- `17-custom-agents.md` — `CustomAgent`, `Steering`, `Skill`, `McpServer`,
  which define what an agent *is*.

The whole proposal can be reduced to one structural claim:

> A *Team* is a server-owned group of *persistent* agents glued together by a
> shared *Board*. Persistent here means an `AgentId` whose memory survives the
> backend session — Tyde owns the memory record and re-spawns the underlying
> backend session whenever it has to (compaction, restart, model change). The
> existing single-shot agent in `03-agents.md` is the *transient* case; teams
> are the persistent case.

Everything else (board columns, manager loops, MCP tools) is mechanism.

---

## 1. Goals & Non-Goals

### Goals

- Agents that exist *between* tasks. An `AgentId` is a stable, addressable
  member of an org chart, not a per-task subprocess.
- Org-chart structure: every team has exactly one manager and N reports.
  Cards are assigned by the manager, not by the human.
- A kanban board is the work intake. Humans drop cards in `Backlog`; the rest
  of the lifecycle is server-driven.
- Long-lived memory per persistent agent: a *rolling summary* + a *recent
  tail*, kept in sync via Tyde-managed compaction. The agent "vaguely
  remembers" prior cards.
- Backend-agnostic. Compaction has to work whether the underlying backend has
  a native `/compact` or not.
- One source of truth: every concept (team, member, card, column,
  transition, memory record) is a typed `protocol/src/types.rs` record. The
  server emits replay+live events. The frontend renders.

### Non-Goals (v1)

- Nested teams. A team has members, not sub-teams. (See open questions.)
- Reports with multiple managers, or managers who report to other managers
  *inside the team*. The org graph is a 1-deep tree per team.
- Cross-team agent membership. An agent belongs to at most one team in v1.
- Free-form delegation graphs. A manager only delegates to its direct reports
  — never to peers' reports.
- Card persistence after team disband. Disband deletes the board.
- Cost accounting / budget per team. (See open questions.)
- Project-scoped teams. v1 teams are host-scoped, like custom agents.
- Migrating an existing live agent into a team. Team membership is fixed at
  team-creation time; new members are spawned into the team.
- Backend "/compact" command pass-through. Tyde owns compaction; we never call
  the backend's own command.

---

## 2. Conceptual Model

```
┌─────────┐   has 1   ┌─────────┐   has N   ┌──────────────┐
│  Team   │──────────▶│ Manager │──────────▶│   Report     │
│         │           │ (Agent) │           │   (Agent)    │
│         │   has N   │         │           │              │
│         │──────────▶│         │           │              │
└────┬────┘           └─────────┘           └──────────────┘
     │ has 1
     ▼
┌─────────┐  has N   ┌──────────┐  has N   ┌──────────────┐
│  Board  │─────────▶│   Card   │─────────▶│   Comment    │
└─────────┘          │          │          └──────────────┘
                     │ in 1     │
                     ▼          │
              ┌──────────────┐  │
              │ BoardColumn  │  │
              └──────────────┘  │
                                │ assignee 0..1
                                ▼
                        ┌──────────────┐
                        │ Report Agent │
                        └──────────────┘

┌──────────────┐    1:1    ┌────────────────┐
│ Persistent   │──────────▶│  AgentMemory   │
│ Agent        │           │ (rolling sum +  │
│ (AgentId)    │           │  recent tail)  │
└──────────────┘           └────────────────┘
        │ may currently own
        ▼
   ┌────────────┐
   │ Backend    │  rotates on compaction; identity is the AgentId, not this
   │ Session    │
   └────────────┘
```

Key definitions:

- **Team** — a host-owned record (`TeamId`, name, members, manager,
  workspace_roots inherited at spawn time, `project_id?`). Owns one
  `Board`.
- **Member** — an `AgentId` plus a `TeamRole` (`Manager` or `Report`). A
  team has exactly one `Manager` member at all times.
- **Persistent Agent** — an `AgentId` that is a member of a team. It has a
  `AgentMemory` record. Its current backend session is a runtime detail; it
  may not even have one running right now.
- **Card** — a unit of work on the board. Has a `CardState`, `assignee:
  Option<AgentId>`, comments, and an audit trail of state transitions.
- **Board** — the team's kanban board. One per team. Owns its cards.
- **AgentMemory** — `{ rolling_summary: String, recent_turns:
  Vec<MemoryTurn>, last_compacted_at_ms, generation: u64 }`. Persisted.
- **Compaction** — the Tyde-driven process that rewrites
  `(rolling_summary, recent_turns)` and bumps `generation`.

The existing `Agent` from `03-agents.md` is unchanged. A team member is just
an `Agent` whose `AgentId` happens to be referenced by a `Team` and whose
memory record is non-empty. The agent registry doesn't need a new type —
it gains a *flag*.

---

## 3. Persistence Model

All team state lives on the **server**, per philosophy rule 2.

### 3.1 Choice: SQLite, not JSON

Existing host-owned domains (`projects.json`, `custom_agents.json`,
`steering.json`, `mcp_servers.json`) are JSON-blob-on-disk: full-file load,
full-file write, atomic rename. That works because those domains are *small,
slow-changing lists of small records*.

Teams are different:

- A populated board can have hundreds of cards.
- Cards mutate frequently (state transitions, comments, drags).
- Cards have ordered history (audit trail) we want to query without rewriting
  the whole file.
- Memory turns have a tail-with-eviction shape — natural fit for an indexed
  table.
- We want transactional moves ("move card from `InProgress` to `Review`,
  append history row, set `last_moved_at_ms`") without rewriting JSON for
  every move.

So: **one SQLite database** at `~/.tyde/teams.db`. Same WAL pattern as the
existing `session.db`. Schemas:

```sql
CREATE TABLE teams (
    team_id           TEXT PRIMARY KEY,         -- TeamId (UUID)
    name              TEXT NOT NULL,
    project_id        TEXT,                     -- nullable; FK by convention
    workspace_roots   TEXT NOT NULL,            -- JSON array<String>
    created_at_ms     INTEGER NOT NULL,
    disbanded_at_ms   INTEGER                   -- soft delete; NULL while live
);

CREATE TABLE team_members (
    team_id           TEXT NOT NULL REFERENCES teams(team_id),
    agent_id          TEXT NOT NULL,            -- AgentId
    role              TEXT NOT NULL,            -- 'manager' | 'report'
    joined_at_ms      INTEGER NOT NULL,
    left_at_ms        INTEGER,                  -- soft delete
    PRIMARY KEY (team_id, agent_id)
);
CREATE UNIQUE INDEX team_one_active_manager
    ON team_members(team_id) WHERE role = 'manager' AND left_at_ms IS NULL;

CREATE TABLE cards (
    card_id           TEXT PRIMARY KEY,         -- CardId (UUID)
    team_id           TEXT NOT NULL REFERENCES teams(team_id),
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    state             TEXT NOT NULL,            -- BoardColumn enum, snake_case
    assignee_agent_id TEXT,
    created_by        TEXT NOT NULL,            -- 'human' | AgentId
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    rank              REAL NOT NULL             -- fractional rank within column
);
CREATE INDEX cards_by_team_state ON cards(team_id, state, rank);

CREATE TABLE card_history (
    history_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id           TEXT NOT NULL REFERENCES cards(card_id),
    at_ms             INTEGER NOT NULL,
    actor             TEXT NOT NULL,            -- 'human' | AgentId
    event             TEXT NOT NULL             -- JSON-encoded CardHistoryEvent
);

CREATE TABLE card_comments (
    comment_id        TEXT PRIMARY KEY,         -- CardCommentId
    card_id           TEXT NOT NULL REFERENCES cards(card_id),
    author            TEXT NOT NULL,            -- 'human' | AgentId
    body              TEXT NOT NULL,
    created_at_ms     INTEGER NOT NULL
);

CREATE TABLE agent_memory (
    agent_id          TEXT PRIMARY KEY,         -- AgentId
    generation        INTEGER NOT NULL,         -- bumped on each compaction
    rolling_summary   TEXT NOT NULL,
    last_compacted_at_ms INTEGER NOT NULL,
    last_input_tokens INTEGER NOT NULL          -- last observed context size
);

CREATE TABLE agent_memory_turns (
    turn_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id          TEXT NOT NULL REFERENCES agent_memory(agent_id),
    at_ms             INTEGER NOT NULL,
    role              TEXT NOT NULL,            -- 'user' | 'assistant'
    content           TEXT NOT NULL             -- compact text representation
);
CREATE INDEX agent_memory_turns_by_agent
    ON agent_memory_turns(agent_id, turn_id);
```

`card_history.event` is a JSON-encoded `CardHistoryEvent` enum (defined in
the protocol). Storing it as JSON in one column keeps the table append-only
without inventing a row schema per event variant. The protocol enum is the
typed interface; SQLite is just durable bytes.

### 3.2 Why one database, not per-team

The team store is a registry like `projects.json` and `custom_agents.json`,
just bigger. One database is simpler than per-team files and gets us
cross-team queries (e.g. "list all cards assigned to agent X") for free.

### 3.3 Replay model

`teams.db` is the source of truth. The host actor loads everything into
memory at boot, just like the projects store. Live mutations go through the
host actor (or a `TeamActor` it spawns) and are written through to SQLite
inside the same operation. There is no read-through cache: the in-memory
copy *is* the live state, and SQLite is its durable shadow. Consistent with
philosophy rule 6.

### 3.4 Memory storage rationale

Per-agent memory could live in `~/.tyde/agent_memory/<agent_id>/memory.json`
following the existing JSON pattern. We co-locate it in `teams.db` for two
reasons:

1. Memory updates and card moves often happen in the same logical
   transaction (compaction triggered by a card-completion turn). One
   database makes that atomic.
2. `agent_memory_turns` is naturally append/evict — exactly what a SQL index
   on `(agent_id, turn_id)` gives you.

If a non-team agent ever needs memory (e.g. a long-lived solo "personal
assistant" agent), the `agent_memory` table covers them too, keyed by
`AgentId` regardless of team membership. Out of scope for v1, but the schema
allows it.

---

## 4. Org Structure

**Pick the simplest thing.**

- A team has exactly one manager at all times.
- Members are either `Manager` or `Report`.
- A report has exactly one manager (the team's manager).
- A team has zero or more reports.
- An `AgentId` is a member of *at most one* team at a time. Cross-team
  membership is out of scope — it makes memory and authority muddy.
- Teams do not nest. A manager cannot have sub-managers in v1. (The MCP
  surface still allows a manager to spawn a transient sub-agent for a
  bounded subtask via existing `tyde_spawn_agent` — but that sub-agent is
  not a team member and gets no memory record.)
- Replacing the manager is a single typed event: `TeamSetManager { team_id,
  new_manager_agent_id }`. The new manager must already be a member of the
  team (`Report`); the demoted manager becomes a `Report`. Atomic in one
  transaction.

This is deliberately less expressive than what Mike floated. The reason:
once you allow many-to-many manager/report or nested teams, you have a
delegation graph, and graphs have cycles, and now you need cycle detection,
and the kanban-board mental model breaks. We can lift the restriction later
without breaking the protocol — `TeamRole` is an enum, and we'd add
variants.

---

## 5. Memory & Compaction Strategy

This is the hardest part of the proposal. I want to be honest about what's
real and what's hand-waving.

### 5.1 What "continually /compact" means here

A persistent agent has an immortal `AgentId` and a mortal backend session.
The `AgentMemory` record is the bridge:

```
┌──────────────────────── Persistent Agent (AgentId) ─────────────────────┐
│                                                                          │
│   role_card           ← from CustomAgent + Steering (immutable)          │
│   rolling_summary     ← rewritten by compactor                           │
│   recent_turns        ← tail, evicted oldest-first as new turns come in  │
│                                                                          │
│   ╭──── current backend session ────╮  (mortal, may be absent)           │
│   │  SessionId, ChatEvent log,      │                                    │
│   │  in-flight queued messages      │                                    │
│   ╰──────────────────────────────────╯                                   │
└──────────────────────────────────────────────────────────────────────────┘
```

When the agent receives a new task (manager assigns a card), Tyde:

1. If the agent has **no live backend session**, spawn one with an initial
   prompt that *injects the memory*: role_card + rolling_summary +
   recent_turns + the card prompt.
2. If the agent **has** a live session, append the card prompt as a normal
   `SendMessage` (queued via the existing queue actor).

So compaction is not "tell the backend to reduce its context". It is
"summarise the agent's history into Tyde's memory record, then optionally
re-spawn the backend session from scratch using the new summary." That's
the only honest cross-backend strategy.

### 5.2 When compaction triggers

Three triggers, all server-driven:

1. **Token threshold.** After every `TypingStatusChanged(false)` (the agent
   is idle), inspect the most recent `ContextBreakdown.input_tokens` /
   `context_window` from the agent's last `ChatMessage`. If
   `input_tokens > 0.6 * context_window`, schedule compaction.
2. **Card boundary.** When a card the agent owns moves to `Done` or
   `Blocked`, compaction is scheduled regardless of token count. Card
   completion is a natural narrative break and produces a nice "what did I
   accomplish on card X" summary.
3. **Explicit request.** A manager (or human) can `tyde_team_compact_agent`
   on demand. Useful before assigning a big new card.

`60%` is a starting number, not a calibrated one. Tunable via
`HostSettings`.

We do **not** compact on a wall-clock timer. There's no good reason to
compact an agent that hasn't done anything.

### 5.3 What is preserved vs. summarised

Compaction inputs:

- `role_card` — never touched. Always derived fresh from the
  `CustomAgent` + `Steering` of record at spawn time.
- `previous rolling_summary` — included in the compactor's prompt.
- `all turns since the last compaction` — the compactor sees these, even
  the ones already partially summarised, because the previous summary lives
  beside them.
- A manifest of card outcomes since the last compaction (titles, final
  states, key decisions extracted from `card_history`). This is the
  "external memory" anchor — even if the summary drifts, the cards are
  durable.

Compaction outputs:

- A new `rolling_summary` (target ~2000 tokens; configurable).
- A trimmed `recent_turns` tail of the last N turns (default N=10) verbatim,
  *not* paraphrased. Verbatim recent turns matter because pronoun resolution
  / "as I said earlier" depends on exact phrasing.

So the new memory after compaction is:
`role_card + rolling_summary + last_10_turns_verbatim`.

### 5.4 How compaction is implemented

A `MemoryCompactor` is a special internal agent type. It:

- Is spawned via the existing `Backend` trait — same as any agent.
- Uses `BackendKind::Tycode` with `cost_hint: Low` by default (cheap, fast,
  no team context bleed). Configurable per host setting.
- Receives a single deterministic prompt template that includes the inputs
  in 5.3 and an instruction to produce the outputs.
- Produces output as a typed JSON blob: `{ rolling_summary: String,
  notable_decisions: [String], confidence: f32 }`. The JSON shape is
  enforced via a tool-use schema; the compactor's only allowed tool call is
  `submit_compaction(...)`.
- Is terminated as soon as the tool call succeeds.

The compactor is not a team member, has no card, has no memory of its
own. It is a one-shot internal agent.

After the compactor returns, the host actor commits in one SQLite
transaction:

1. `UPDATE agent_memory SET generation = generation + 1, rolling_summary =
   ?, last_compacted_at_ms = ?, last_input_tokens = ? WHERE agent_id = ?`.
2. `DELETE FROM agent_memory_turns WHERE agent_id = ? AND turn_id NOT IN
   (...last N...)`.
3. Emit `AgentMemoryNotify::Updated` on the team's stream.

If the agent has a *live* backend session, the live session keeps running
with its existing context — we don't kill it just because we compacted. The
compaction takes effect the **next** time we have to spawn a fresh backend
session for that agent (which happens on server restart, or when a chosen
model has a smaller context window than the live one, or when memory
generation diverges from the session's start-time generation by more than
1).

This last condition matters: if the live session was spawned at memory
generation 3, and we've since compacted to generation 5, the live session's
context is increasingly out of sync with what the team thinks the agent
remembers. Forcing a re-spawn at generation+2 keeps drift bounded.

### 5.5 Cross-backend reality

| Backend | Native /compact?    | Strategy                             |
|---------|---------------------|--------------------------------------|
| Claude  | Yes (`/compact`)    | Tyde-managed compaction. Ignore native. |
| Codex   | No                  | Tyde-managed compaction. |
| Tycode  | Yes (Tyde controls) | Tyde-managed compaction. |
| Kiro    | No                  | Tyde-managed compaction. |
| Gemini  | No                  | Tyde-managed compaction. |

Why ignore Claude's native `/compact`? Because we need backend-agnostic
behaviour and we can't observe the result of native `/compact` to persist
it across backend restarts. Tyde-managed compaction is the only way to
have the memory record survive when the live backend session is gone.
Native `/compact` is a runtime optimisation Claude does for itself; ours
is the durable memory.

This is the **honest gap**: we have no native primitive to lean on. Every
backend gets the same Tyde-driven path. That's a feature, not a regression
— it's the only way to satisfy "memory across restarts" uniformly.

### 5.6 Memory across server restarts

`teams.db` is on disk. After a server restart, an agent's `AgentMemory`
record is intact. The agent has *no* live backend session. The next time
the agent is asked to act on a card, Tyde spawns a fresh backend session,
injecting `role_card + rolling_summary + recent_turns`. From the manager's
perspective the report just woke up.

This means there is no "resume" semantics for team agents in the
session-resume sense (`05-session-resume.md`). The session-resume system
keeps working for *transient* agents. Team agents do not use it; the
memory record *is* their resume mechanism. `BackendNative` sub-agent
sessions of a team agent stay non-resumable per `15-sub-agents.md`.

---

## 6. Task / Card Lifecycle

### 6.1 Card states

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardColumn {
    Backlog,        // human-created, not yet picked up
    Triage,         // manager has seen it, deciding who/what
    Assigned,       // assigned to a report; report has not started
    InProgress,     // report is actively working
    Review,         // report claims done; manager hasn't accepted
    Blocked,        // report is stuck; needs manager attention
    Done,           // manager accepted
    Cancelled,      // closed without completion
}
```

Six live columns plus two terminal columns. The split between `Backlog →
Triage → Assigned` exists because "manager has read this" and "manager has
chosen an assignee" are different states and the loop logic needs to
distinguish them.

### 6.2 Allowed transitions

```
Backlog    → Triage         (manager auto on wake)
Triage     → Assigned        (manager chooses assignee)
Triage     → Cancelled       (manager rejects card)
Assigned   → InProgress      (assignee picks it up)
InProgress → Review          (assignee submits)
InProgress → Blocked         (assignee blocked)
Review     → Done            (manager accepts)
Review     → InProgress      (manager rejects, sends back)
Blocked    → InProgress      (assignee or manager unblocks)
Blocked    → Cancelled       (manager kills it)
Backlog    → Cancelled       (human closes before triage)
```

Anything not in this table is rejected by the server with a non-fatal
`AgentError` on the actor's stream. Strong typing again — invalid moves
should be unrepresentable in the protocol payload as much as possible (we
encode the move as `(CardId, BoardColumn)` rather than a free-form
"transition" field).

### 6.3 Who can move

| Actor       | Allowed moves                                                  |
|-------------|----------------------------------------------------------------|
| Human (UI)  | Anything. Humans can override.                                 |
| Manager     | `Backlog→Triage`, `Triage→{Assigned,Cancelled}`, `Review→{Done,InProgress}`, `Blocked→{InProgress,Cancelled}`. |
| Report      | `Assigned→InProgress` (only own cards), `InProgress→{Review,Blocked}` (only own cards). |
| Other agent | Nothing. The server rejects it as a permission error.          |

The "human can override" rule isn't a fallback — it's an explicit privilege
of the `human` actor identity. Humans always own escape hatches.

### 6.4 Events

Server-emitted, on the team's stream `/team/<team_id>/<instance_id>`:

- `BoardNotify::CardUpsert(card)` — full card snapshot. Used for create,
  move, edit, comment-add. Single shape simplifies replay.
- `BoardNotify::CardDelete(card_id)` — only used when a card is hard-
  deleted. Cancelled cards stay; they aren't deleted.
- `TeamNotify::Upsert(team_summary)` — team metadata changed (name,
  members, manager).
- `TeamNotify::Disband(team_id)` — soft-deleted team.
- `AgentMemoryNotify::Updated(agent_id, generation, summary_preview)` —
  fired after each compaction. `summary_preview` is the first ~200 chars,
  the full summary is read on demand via a separate event (avoids spamming
  large payloads).

Why `CardUpsert` is a full-card payload, not a delta: same reason
`QueuedMessages` is a snapshot in `16-queued-messages.md`. Snapshots
collapse replay and live updates into the same code path.

---

## 7. Manager Loop

Managers are LLM agents, not state machines. They wake up when something
needs human-style judgement. The team actor delivers triggers as
`SendMessage` payloads on the manager's existing agent stream — reusing the
already-implemented queued-messages plumbing.

### 7.1 Wake triggers

The team actor watches its board and emits a `SendMessage` to the manager
when:

1. A new card lands in `Backlog`. Prompt template:
   > "Card #{N} '{title}' was added to backlog. Read it (use the
   > `tyde_team_read_card` tool), then either move it to `Triage` and
   > assign it to a report, or cancel it with a reason."

2. A card moves to `Review` (manager must verify):
   > "Card #{N} '{title}' was submitted for review by {report_name}. Read
   > the card and the agent's recent activity. Accept it (move to `Done`)
   > or send back to `InProgress` with feedback."

3. A card moves to `Blocked` (manager must unstick):
   > "Card #{N} is blocked: {report_message}. Decide: unblock with new
   > guidance, reassign, or cancel."

4. A report's typing status flips to idle and the report has no active
   card *and* the backlog is non-empty: opportunity to assign.
   This is throttled — at most one such poke per report per 60s — to avoid
   pestering the manager when reports are between cards by design.

5. Periodic check-in (default off, set per-team via `TeamSettings`).
   Mike's "managers periodically look at the board" — opt-in, not the
   default. Idle polling burns money.

The team actor itself is implemented as a tokio actor that subscribes to
the same `ChatEvent` stream the host already broadcasts. No polling.

### 7.2 How a manager picks a report

The manager prompt at triage time includes:

- Card title + body.
- A roster snippet for the team:
  ```
  Reports:
  - alice (id: ag_…): backend-focused. Currently: 1 card in InProgress, last
    completed: "Add SSH config field" 3h ago. Memory generation 4.
  - bob (id: ag_…): frontend-focused. Currently idle. Memory generation 2.
  ```
- Each report's *role_card* description (from their `CustomAgent`) plus a
  short workload line (cards in flight).
- The `tyde_team_assign_card` tool to call.

The manager's choice is the LLM's call. Tyde does not implement a matching
heuristic — that's the agent's job. We just give it good inputs.

### 7.3 Follow-up after assignment

When the manager calls `tyde_team_assign_card(card_id, report_agent_id)`:

1. Server moves the card `Triage → Assigned`.
2. Server emits a `SendMessage` to the **report**, prompting it to start
   (with the card's body and links to relevant context). Reuses the queue
   actor — if the report is mid-turn, the work prompt queues; otherwise it
   runs.
3. Server emits `BoardNotify::CardUpsert` for all team subscribers.

The manager doesn't directly message the report. The team actor mediates,
because the team actor is the one source of truth for "what work is the
report supposed to do right now."

### 7.4 Why not let managers do it via plain SendMessage?

Two reasons:

- We want typed, auditable work delivery. A `tyde_team_assign_card` tool
  call writes a `card_history` row with the manager as `actor`. A free-form
  `SendMessage` doesn't.
- If the report goes offline, the assignment must survive. A queued
  `SendMessage` to a non-running report disappears (queue is
  runtime-only, per `16-queued-messages.md`). The card stays in
  `Assigned` and is redelivered on the next spawn.

---

## 8. Protocol Changes

All in `protocol/src/types.rs`.

### 8.1 IDs

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CardId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CardCommentId(pub String);
```

### 8.2 Enums

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Manager,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardColumn {
    Backlog, Triage, Assigned, InProgress, Review, Blocked, Done, Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardActor {
    Human,
    Agent { agent_id: AgentId },
}
```

`CardActor` replaces stringly-typed `created_by` / `actor`. Strong typing,
no `if value == "human"` branches.

### 8.3 Records

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamMember {
    pub agent_id: AgentId,
    pub role: TeamRole,
    pub joined_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Team {
    pub id: TeamId,
    pub name: String,
    pub project_id: Option<ProjectId>,
    pub workspace_roots: Vec<String>,
    pub members: Vec<TeamMember>, // includes the manager
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardComment {
    pub id: CardCommentId,
    pub author: CardActor,
    pub body: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: CardId,
    pub team_id: TeamId,
    pub title: String,
    pub body: String,
    pub state: BoardColumn,
    pub assignee_agent_id: Option<AgentId>,
    pub created_by: CardActor,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub rank: f64,
    pub comments: Vec<CardComment>,
    pub history: Vec<CardHistoryEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardHistoryEvent {
    Created      { at_ms: u64, actor: CardActor },
    StateChanged { at_ms: u64, actor: CardActor, from: BoardColumn, to: BoardColumn },
    AssigneeChanged { at_ms: u64, actor: CardActor, from: Option<AgentId>, to: Option<AgentId> },
    CommentAdded { at_ms: u64, actor: CardActor, comment_id: CardCommentId },
    Edited       { at_ms: u64, actor: CardActor, fields: Vec<CardEditField> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardEditField { Title, Body }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMemorySummary {
    pub agent_id: AgentId,
    pub generation: u64,
    pub last_compacted_at_ms: u64,
    pub rolling_summary_preview: String, // first ~200 chars
}
```

### 8.4 Input frame kinds (host stream)

Team mutations ride on `/host/<uuid>` like project mutations.

```rust
TeamCreate
TeamDisband
TeamRename
TeamSetManager
TeamAddMember        // member must be a non-team agent
TeamRemoveMember     // cannot remove the manager; demote first
CardCreate           // human-created or agent-created; server checks actor
CardEdit
CardMove             // typed transition; server validates permission table
CardAssign           // sets assignee_agent_id
CardCommentAdd
CardDelete           // hard delete; rare; humans only
CompactAgentRequest  // explicit compact-now
```

### 8.5 Output frame kinds

```rust
TeamNotify
BoardNotify
AgentMemoryNotify
```

with payloads:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamNotifyPayload {
    Upsert  { team: Team },
    Disband { id: TeamId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoardNotifyPayload {
    CardUpsert { card: Card },
    CardDelete { id: CardId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMemoryNotifyPayload {
    Updated { summary: AgentMemorySummary },
}
```

### 8.6 Streams

Two new stream patterns:

- `/team/<team_id>/<instance_id>` — a per-subscriber team stream. Mirrors
  `/agent/...`. Replays existing team state (members, cards, memory
  summaries) then streams live changes.
- `/host/<uuid>` — gains `TeamNotify` upserts/deletes (the *list* of
  teams), so a frontend can render the team selector before subscribing to
  any team. Same shape as `ProjectNotify`.

`BoardNotify` and `AgentMemoryNotify` only flow on `/team/...`. The host
stream only carries the team list, not card-level detail. That mirrors
projects: `/host/...` sees `ProjectNotify`, `/project/...` sees file-level
changes.

### 8.7 Replay ordering

Extend the host replay sequence from `17-custom-agents.md` §4.3:

1. `HostSettings`
2. existing host prelude
3. `ProjectNotify`
4. `McpServerNotify`
5. `SkillNotify`
6. `SteeringNotify`
7. `CustomAgentNotify`
8. **`TeamNotify`** ← new, must precede agents because agents reference
   team membership.
9. `NewAgent` and per-agent stream attach
10. (per team subscription) on `/team/<team_id>/...`: `TeamNotify(Upsert)`
    at seq 0, then `BoardNotify` for every live card, then
    `AgentMemoryNotify` for each member's memory summary.

### 8.8 Validation rules

In `protocol/src/validator.rs`:

- `TeamCreate.members` must contain exactly one `Manager` and zero or
  more `Report`s.
- All `agent_id`s referenced by a team must already exist as agents.
- A given `agent_id` must not appear in more than one live team.
- `CardMove.to` must be reachable from `card.state` per §6.2.
- `CardMove` from a non-allowed actor for that transition is rejected.
- `TeamSetManager.new_manager_agent_id` must already be a `Report` of that
  team.

These are server-side checks, but the protocol shape itself eliminates the
worst classes (no stringly-typed states, transitions encoded as typed enum
moves, etc.).

---

## 9. MCP Surface

### 9.1 Decision: tools, mapped 1:1 to protocol events

Managers and reports drive the team via the existing **agent-control MCP**
(`11-agent-control-mcp.md`). The MCP server is already injected into every
spawned agent. Adding team tools there avoids inventing a second control
plane.

But each MCP tool is a thin shim over a typed protocol event. The server
handler is the same code path as the human's UI click. **The MCP surface
is not a parallel command model.** It's a callable wrapper. This satisfies
philosophy rule 1 (one source of truth) — both UI and MCP funnel into the
same `TeamActor` mutation path.

### 9.2 New tools on agent-control MCP

| Tool                          | Maps to                          | Caller       |
|-------------------------------|----------------------------------|--------------|
| `tyde_team_list`              | (read) team list                 | any agent    |
| `tyde_team_describe`          | (read) team + members + memory   | any agent    |
| `tyde_team_read_board`        | (read) cards by column           | team member  |
| `tyde_team_read_card`         | (read) full card                 | team member  |
| `tyde_team_move_card`         | `CardMove`                       | per §6.3     |
| `tyde_team_assign_card`       | `CardAssign`                     | manager      |
| `tyde_team_comment_card`      | `CardCommentAdd`                 | team member  |
| `tyde_team_compact_agent`     | `CompactAgentRequest`            | manager / self |

Notes:

- The MCP server identifies the **calling agent** from the loopback
  request URL (per `11-agent-control-mcp.md`). That's how it enforces
  per-actor permissions without a bearer token.
- There is intentionally no `tyde_team_create_team` or
  `tyde_team_add_member`. Team org changes are human-only in v1. We can
  lift this later, but giving managers the power to grow their own team is
  a scope explosion.
- There is no `tyde_team_send_to_report`. Reports receive work *only* via
  `CardAssign`. This is what makes the kanban the source of truth.

### 9.3 Why not protocol events emitted directly by agents?

Agents don't speak the host protocol. They speak MCP. The server already
draws that boundary in `11-agent-control-mcp.md`. Introducing a second
"agents can emit `/host/...` events directly" path would be a parallel
control plane. Reject.

---

## 10. Frontend Surface

Brief, per the spec.

### 10.1 Teams panel (sibling to Projects, Sessions, Agents)

- List of teams: name, member count, open card count, manager avatar.
- "New team" wizard: name, pick existing agents or "spawn new", choose
  manager.

### 10.2 Board view (per team)

- Columns: `Backlog | Triage | Assigned | InProgress | Review | Blocked`.
  `Done` and `Cancelled` collapsed in a footer drawer.
- Cards keyed by `CardId` (philosophy reactivity rule).
- Drag & drop between columns by humans → emits `CardMove`. Server
  validates, possibly rejects.
- Click a card → side panel with title, body, comments, history, current
  assignee.
- "Open agent stream" button on a card → opens that report's
  `/agent/...` stream like any other agent.

### 10.3 Member cards

In the team header / sidebar:

- Each member rendered as a card showing:
  - Name, role (manager / report).
  - First 200 chars of `rolling_summary_preview` from
    `AgentMemoryNotify`.
  - Memory generation, last compaction time.
  - Live `TypingStatusChanged` indicator.
- Click → expands to show full memory (lazy-loaded via a new
  `AgentMemoryRead` request, since rolling summaries can be 2k tokens).

### 10.4 No new dispatch primitives

Everything renders from `TeamNotify`, `BoardNotify`, `AgentMemoryNotify`
and the existing agent state. No frontend caches.

---

## 11. Failure Modes

### 11.1 Manager crashes mid-delegation

State at crash: card is in `Triage`, manager has decided to assign but
hasn't called the tool. Outcome: card stays in `Triage`. On manager
re-spawn, the team actor's wake trigger #1 (or rather a "stuck in triage"
variant) re-prompts the manager.

If the manager is fatally dead (e.g. backend gone), the team has no
manager. The team actor emits `AgentMemoryNotify` style health event,
human is alerted in the UI. v1 does not auto-promote a report. Human
intervenes via `TeamSetManager`. (See open question on auto-promotion.)

### 11.2 Report can't complete card

Report moves card to `Blocked` with a comment. Manager wake trigger #3
fires. Manager decides: unblock with guidance (move back to
`InProgress`), reassign to another report, or cancel.

If the report itself is fatally dead, the card stays `InProgress` with no
live assignee. The team actor detects "assignee has been dead for >5min
and card hasn't moved" and synthesises a `Blocked` move with
actor=`Human` (system) and comment "assignee died". This is an explicit
typed transition, not silent recovery.

### 11.3 Compaction loses important context

Compaction is destructive — that's the trade. We mitigate:

- Verbatim recent_turns means the immediate "what we were doing" tail is
  intact.
- Card history is durable. The manager can always read prior cards.
- We keep `agent_memory_turns` rows for one generation prior on disk
  (don't `DELETE` immediately; flag with `superseded_at_generation`). v1
  doesn't expose them, but they're recoverable for debugging.
- Compaction generation is included in `AgentMemoryNotify`. If the user
  notices the agent forgot something, the timestamp tells them when the
  loss happened.

We do **not** try to detect "important" context and preserve it
selectively. That's a fuzzy heuristic and would violate "no inference."

### 11.4 Cycles in delegation

By construction: managers only delegate to direct reports, reports cannot
delegate, no nested teams. The org graph is a depth-1 tree. No cycles
representable.

### 11.5 Multiple subscribers, racy moves

Same problem as queued messages. The team actor processes mutations
serially. Two `CardMove` calls for the same card from two clients hit
the actor in some order; the second one's precondition (current state)
may now be wrong; the actor rejects with a non-fatal `AgentError` and
emits a fresh `BoardNotify::CardUpsert` so all clients reconcile.

### 11.6 Disbanded team with live agents

Disband marks the team as soft-deleted (`disbanded_at_ms`). Member
agents remain alive. Their `AgentMemory` records are kept (humans may
re-team them later). Cards are not deleted but become read-only — the
board renders in archive mode. We do not auto-terminate agents, because
they may have ongoing transient sub-agents we don't want to kill.

### 11.7 Memory generation drift

Live backend session was spawned at gen 3, current memory is gen 5. We
re-spawn the session at next idle. There is a tiny window where the
session's worldview is older than the team's. This is acceptable — the
session can only act between turns, and we restart between turns. We
don't restart mid-turn.

### 11.8 SQLite write contention

One database, multiple actors writing. We use a single-writer pattern: a
`TeamStoreActor` owns the `rusqlite::Connection`. All mutations are
mpsc-serialised through it. This is the actors-over-locks rule from
philosophy. Reads can either go through the actor or via a read-only
connection if benchmarked necessary.

---

## 12. Things I'd push back on / questions for Mike

These are unresolved by design. I picked one answer for each above
because the doc is supposed to be implementable, but they're the spots
where I think Mike (or Codex) might disagree.

### 12.1 "Continually compact" vs. "compact at boundaries"

I picked threshold + card-boundary triggers. Mike's phrasing was
"continually" — if he means literally every N turns, regardless of
context size, that's wasteful. If he means "always eventually," the
threshold trigger covers it. Worth confirming.

### 12.2 Are sub-teams really out of scope?

Reports who become managers of their own team is the obvious next
feature. I declared it out of v1 to keep delegation a tree. But Mike's
"proper org structure" language hints at multi-level. **Push back:**
let v1 be flat and add nesting later as a clean protocol extension
(`TeamRole` gains `SubManager` or teams gain `parent_team_id`). Doing
it day one will balloon the schema and create cycle questions.

### 12.3 Should the manager be itself an LLM agent at all?

The manager is the most expensive part of a team — it wakes every time
something changes on the board. An honest alternative: the manager is a
*deterministic* state machine with one LLM call per triage decision. The
loop becomes: "card lands → state machine routes to LLM for assignment
choice only → state machine handles everything else." Cheaper, more
predictable, less personality.

I picked "manager is an LLM agent" because Mike said "team of agents
with a manager." Worth confirming whether the manager is genuinely an
agent (with memory, with personality, with `CustomAgent` identity) or
whether it's a thin LLM call dressed as one.

### 12.4 Auto-promotion when a manager dies

Should the team actor pick a report and promote it (`TeamSetManager`)
on manager death? I said no — humans intervene. If teams are meant to
be highly autonomous, auto-promotion might be desired. But picking the
"right" replacement is itself a judgement call that I'd rather defer
to a human in v1.

### 12.5 Should reports be allowed to spawn transient sub-agents?

Today, agent-control MCP lets any agent `tyde_spawn_agent`. A report
working on a card can fan out helpers. That's fine, but those helpers
won't have memory and won't be on the board. **Worth confirming:** is
the report's sub-agent fan-out something we want to track on the board
as nested cards, or do we just let it stay invisible?

### 12.6 Cost / concurrency limits per team

Not in v1. But teams of agents with autonomous loops are a money fire
hazard. A `HostSettings.team_max_concurrent_turns` cap, plus a per-team
spend cap, are obvious. We deferred budget tracking already in
`11-agent-control-mcp.md` future work — does Mike want it sooner here?

### 12.7 What happens to a card when its assignee leaves the team?

Right now: `TeamRemoveMember` of an assignee → cards stay assigned to
a non-member, which is invalid. We need an auto-rule. I'd pick: any
in-flight cards owned by the leaver move back to `Triage`, manager
gets woken up.

### 12.8 Inline editing of card body / title from the UI vs. agent

I propose `CardEdit` accepts edits from any team member or human. But
this collides with the "kanban is the source of truth" idea — should
agents be allowed to edit a card's body after the manager has already
read it? Maybe edits after `Triage` should be append-only comments.

### 12.9 The manager's "queue" of attention triggers

Right now the team actor delivers each trigger as a separate
`SendMessage` to the manager. With the queue actor in
`16-queued-messages.md`, those land in the manager's queue while it's
busy. Fine for correctness. But the manager could end up with 20 wake-
prompts queued during a long card. I'd want a coalescing layer: at
most one outstanding "look at the board" prompt at a time, and the
rest become a single "the board changed N times" summary. Worth
discussing.

### 12.10 Naming

I called the kanban entity "Card" rather than "Task" because
`ChatEvent::TaskUpdate` already exists and means in-conversation
task lists, not team work. Worth confirming the name `Card` is fine.

---

## 13. Implementation Order (rough)

1. Protocol types and frame kinds (`teams`, `cards`, `memory`).
2. `teams.db` store actor with the schema in §3.1.
3. `TeamActor` per-team, behind a `TeamRegistry`. Mirrors agent
   registry pattern.
4. Host stream replay extension (`TeamNotify`).
5. Per-team stream `/team/<id>/<instance>` with `BoardNotify` and
   `AgentMemoryNotify`.
6. Wire `MemoryCompactor` internal agent type. Implement the
   threshold trigger + card-boundary trigger.
7. Memory-injected backend respawn path (`spawn_with_memory`).
   Modifies `agent::spawn` to consult `agent_memory` and prepend the
   role_card + summary to the initial prompt.
8. Agent-control MCP team tools.
9. Frontend teams panel + board view + member memory cards.
10. Manager loop wake triggers, including coalescing.
11. Tests: card transition matrix, permission table, memory drift,
    compaction round-trip, replay ordering, multi-subscriber race.

---

## 14. Summary

A team is a server-owned record of `(Team, Members, Board, Cards,
AgentMemory)`. Persistent agents are agents whose memory survives
across backend sessions. The kanban board is the work intake. Managers
are LLM agents that the team actor wakes up when the board needs human-
style judgement. Compaction is Tyde-managed, backend-agnostic, and
triggered by token-threshold or card-boundary. Everything is replayed
host-style; the frontend is a pure projection.

The biggest design risk is compaction quality. The biggest scope risk
is letting teams nest in v1.
