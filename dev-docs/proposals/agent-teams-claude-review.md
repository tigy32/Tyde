# Agent Teams — Review and Unified Proposal

Author: Claude. Reviews `agent-teams-codex.md` against `agent-teams-claude.md`
and proposes the unified design we should ship.

---

## Summary of Codex's design (in your words)

- **The durable identity is `TeamMemberId`, not `AgentId`.** A member has a
  stable ID, a memory record, a `last_session_id`, and a backend/custom-agent
  config. The current live `AgentId` is a *runtime binding* emitted as a
  separate `TeamMemberBindingNotify` event, never persisted as durable
  identity (`agent-teams-codex.md` §2, §5, §9).
- **Single JSON store at `~/.tyde/agent_teams.json`** following the existing
  `projects.json`/`custom_agents.json` pattern, with an explicit "swap to
  SQLite later if measured" escape hatch (§3).
- **Compaction is a typed per-backend mode**:
  `NativeCompact | RestartFromMemory | MemoryOnly`. Memory updates are
  produced by the member itself via a typed MCP tool call
  `tyde_submit_team_memory_update`. Failed compactions surface visibly and
  the previous generation stays current — no partial replacement (§6).
- **Eight fixed columns** including `Claimed` (manager accepted
  responsibility) and `Assigned` (manager picked a report). Manager
  `Claimed → Assigned` runs under a server-owned **claim lease** that
  expires and bounces the card back if the manager dies mid-decision (§7,
  §12).
- **Two events per card mutation**: a `TeamCardNotify::Upsert` snapshot plus
  an append-only `TeamCardActivityNotify` event. Card mutations include
  `expected_version: u64` for optimistic concurrency (§9, §12).
- **`AgentOrigin::TeamMember` is a new variant** so the frontend never has
  to infer that a live agent is a team member from `parent_agent_id` or
  similar (§5, §9).

---

## Where Codex is right and I was wrong

I'm going to be direct rather than diplomatic. Codex's design is stronger
than mine on six points; I'd adopt them as-is.

### 1. Persistent identity is `TeamMemberId`, not `AgentId`

`agent-teams-codex.md` §5: *"A live `AgentId` is a runtime binding. The
server may create a new live agent for the same member when ... the server
restarted ... compaction used a restart-from-memory strategy."*

My proposal had `AgentId` be the persistent thing and conflated it with the
"live process". That collapses two genuinely different concepts. When a
backend session is restarted from memory, the new live agent has a new
`SessionId`; if you keep `AgentId` stable across that restart, you have an
`AgentId` that violates the existing rule that "an agent stream instance is
tied to its connection" (`03-agents.md` §2). If you let `AgentId` rotate,
you contradict your own claim that it's the durable identity.

Codex's split is the right factoring: **the team member is durable, the
agent is the current binding**. This aligns with how Tyde already models
sessions (`SessionId` is durable, `AgentId` is the live instance per
`05-session-resume.md`).

I was wrong. Adopt `TeamMemberId`.

### 2. `AgentOrigin::TeamMember` as a new origin variant

`agent-teams-codex.md` §5: *"I would add a new `AgentOrigin::TeamMember`
rather than overloading `AgentOrigin::AgentControl`."*

I didn't address how a live team-member agent surfaces its team-ness. The
existing `15-sub-agents.md` rule is "the frontend must never derive origin
from parentage" — which means we *must* have an explicit origin tag, not
infer "this `AgentId` is a team member because it has a `team_member_id`."

Codex is right. Add the variant.

### 3. Optimistic concurrency via `expected_version: u64` on card mutations

`agent-teams-codex.md` §9: *"Card mutation input payloads should include
`expected_version: u64`. If the client/manager acts on stale state, the
server emits `CommandError` with `CommandErrorCode::Conflict` rather than
silently applying a stale move."*

My proposal said "the actor serialises mutations" and called that
sufficient. It isn't. Actor serialization gives you *order*, not *correct
preconditions*. If client A reads `state=Triage` and submits `move →
Assigned`, while client B simultaneously moves `Triage → Cancelled`, both
reach the actor in some order; the second one operates on stale
state but my design would just apply it. That's exactly the kind of
silent-success race the philosophy doc rejects ("invalid states should be
unrepresentable", "no fallbacks").

I was wrong. Adopt `expected_version`.

### 4. Split snapshots and activity events

`agent-teams-codex.md` §7, §9: `TeamCardNotify::Upsert` carries the
current card; `TeamCardActivityNotify` carries one append-only event per
mutation.

My proposal embedded `history: Vec<CardHistoryEvent>` inside the `Card`
record, so every card snapshot grew with the activity log. That's wasteful
for replay (cards with long history bloat every notify), and it conflates
"the card right now" with "what happened to the card". Codex's split makes
the activity log a separate stream — cards stay small, activity stays
append-only and queryable.

This is the same shape Tyde2 already uses for `MessageAdded` chat events
versus the `ChatMessage` snapshot — events log activity, snapshots show
state. I was inconsistent with the rest of the codebase.

Adopt the split.

### 5. Typed `TeamCompactionMode` per backend

`agent-teams-codex.md` §6: *"Add an explicit backend capability instead of
trying one strategy and silently falling back to another."*

My proposal said "Tyde-managed compaction for everyone; ignore native
/compact." That collapses three genuinely different behaviours into one,
which violates "strong typing always." Codex's three-mode enum
(`NativeCompact | RestartFromMemory | MemoryOnly`) is correct: it forces
us to declare per-backend what we can actually do, and `MemoryOnly` is the
honest answer for backends where Tyde can save memory but cannot reduce
the live context — at which point the server visibly blocks new
delegation rather than pretending compaction worked.

I was hand-waving. Adopt the typed mode.

### 6. Self-compaction via typed MCP tool

`agent-teams-codex.md` §6: *"Do not scrape arbitrary assistant prose and
pretend it is memory. The server should ask the member to compact and
require a typed MCP call such as `tyde_submit_team_memory_update`."*

My proposal had a separate `MemoryCompactor` internal agent type that
spawns a fresh backend, prompts it to summarise, and parses its tool-use
response. That's more code, an extra subprocess per compaction, and it
asks the *wrong* agent to do the summarising — the compactor doesn't have
the conversation context, only what we hand it. The agent itself is
better positioned to summarise its own history.

The hard-context-pressure case (the agent is too full to think, can't
compact itself) is real but rarer than I made it out to be. Codex's
trigger policy ("idle or safe task boundary, except for hard
context-pressure stop") handles this: hit hard pressure → terminate the
session, restart from previous memory, lose the unsummarised tail. That's
the same blast radius as my external compactor would have when it failed
to produce useful output.

Adopt self-compaction.

### Smaller things Codex got right

- **Manager claim lease**: server-owned timeout for `Claimed` cards. I
  hand-waved at "manager crashes mid-delegation"; Codex specifies a typed
  `ManagerClaimExpired` event. Adopt.
- **`Claimed` rather than `Triage`** for the column name. More accurate —
  "claimed" describes ownership change, "triage" implies a decision was
  already made. Adopt.
- **Replay ordering**: Codex's full ordering (settings → projects → MCP/
  skills/steering/customs → teams → members → boards → cards → activity →
  memory → live agents) is more carefully thought through than my
  one-line addition. Adopt.

---

## Where I stand by my design

Two places.

### 1. Compaction triggers should not include wall-clock idle

Codex §6 lists "Wall-clock idle: at most once every 24 hours while idle"
as a trigger. I had only `token_threshold | card_boundary | manual`.

A 24h idle compaction with no new turns since the last one is a no-op.
Codex's own conditional ("only if new turns/card activity happened since
the previous compaction") collapses this into "compact when there's
something to compact and the agent has been idle long enough" — which is
just the token-threshold trigger fired late. The wall-clock variant adds
no new state, just spend.

I'd drop it. Token threshold + card boundary + manual is sufficient.

### 2. Capability tags (`TeamCapabilityId`) are premature

Codex §9 introduces a `TeamCapabilityId` newtype and a
`capability_ids: Vec<TeamCapabilityId>` field on `TeamMember`. Codex's own
§13 admits *"V1 can use typed capability tags and descriptions, but
skill-based matching may need a stronger model later."*

If skill-based matching needs a stronger model, capability tags are
solving a problem we don't have yet. Right now the manager LLM reads each
report's `description` field (free-form prose, e.g. "frontend-focused, has
worked on the agents panel") and decides. Adding a typed capability ID
makes the schema heavier without giving the manager LLM anything it
couldn't infer from the description.

Philosophy rule: *"Three similar lines of code is better than a premature
abstraction."* Drop the capability_ids field. If we need real
capability-based routing later, we add it then.

---

## Genuine remaining disagreements

These I can't unilaterally decide. Mike chooses.

### D1. JSON vs SQLite for the team store

**Option A (Codex):** one `~/.tyde/agent_teams.json`, full-file load and
atomic replace. Consistent with `projects.json`, `custom_agents.json`,
etc.

**Option B (me):** `~/.tyde/teams.db` (SQLite, WAL), like the existing
`session.db`.

**Tradeoff:**

- JSON is consistent with existing host-owned domains and avoids a second
  persistence stack. Codex argues correctly that it's behind the
  protocol, so we can swap later without churning the wire.
- SQLite is a better fit for the data shape. Cards mutate frequently and
  have append-only activity logs. JSON forces a full-file rewrite on
  every mutation; with hundreds of cards and thousands of activity
  events, that's measurable. SQLite gives us atomic two-thing writes
  (card update + activity append) inside one txn for free, plus indexed
  queries ("all cards assigned to member X").

**My recommendation:** start with SQLite. The JSON store rewrites the
*entire* store on every event, and team boards are exactly the workload
that pattern fails on. It's not premature — it's matching the existing
`session.db` precedent for write-heavy event-logged domains. JSON is the
right call for `projects.json` because projects are tiny and rarely
mutate; cards are not.

But Codex's "swap later behind the protocol" argument is also valid. I'd
take SQLite if I'm calling it.

### D2. Memory schema: structured fields vs prose summary

**Option A (Codex):** `summary_markdown: String` plus typed
`open_commitments: Vec<TeamMemoryCommitment>`,
`recent_turns: Vec<TeamRecentTurn>`,
`active_card_ids: Vec<TeamCardId>`.

**Option B (me):** `rolling_summary: String` plus
`recent_turns: Vec<MemoryTurn>` (verbatim text).

**Tradeoff:**

- Codex's structured fields let the server reason about open commitments
  ("the agent said it'd revisit X") and active cards directly. The
  manager's wake prompt can render commitments without parsing markdown.
- My pure-prose approach is simpler and avoids defining what counts as a
  "commitment" vs a "decision" vs a regular note. Forcing the agent to
  emit structured commitments is the kind of typed-output requirement
  that LLMs do badly when the categories are fuzzy.

**My recommendation:** Codex's, but trimmed. Keep
`summary_markdown` (prose) and `active_card_ids: Vec<TeamCardId>`
(unambiguous, derivable from card state). Drop `open_commitments` for v1
— it's a structured-output ask without a clear definition. Drop
`recent_turns` as a separate field; replace with a simple bounded
`recent_turns_text: String` (concatenated transcript tail) since
verbatim recent context is what matters, not structured per-turn data.

### D3. Should reports be able to spawn transient sub-agents?

I raised this in my open questions; Codex didn't address it. The
`tyde_spawn_agent` MCP tool today lets *any* agent spawn another. If a
report on a card spawns helpers, those helpers are not team members,
have no memory, and don't appear on the board.

**Option A:** allow it (status quo). Helpers are invisible to the team;
the report uses them like a function call.

**Option B:** disallow it for team members. Force the report to ask the
manager to delegate.

**Option C:** allow it but record helpers as nested cards under the
parent.

**My recommendation:** A. The report needing to ask the manager to spawn a
helper is friction with no payoff — it's just an LLM-to-LLM round trip
to do what the report could do directly. Helpers are ephemeral; making
them first-class on the board is over-modelling.

But this is a design call about how much the kanban must reflect *all*
work. Mike calls.

### D4. Manager auto-promotion when the manager dies

I said no, manual via `TeamSetManager`. Codex didn't directly address.
This still needs a Mike call; both designs leave the team in a stuck
state until human intervention. Worth confirming that's the desired
behaviour vs. promoting the longest-tenured report automatically.

### D5. "Continually compact" interpretation

I asked Mike directly. Codex specified triggers (token, task boundary,
wall-clock-idle, manual) without flagging this as a question. The
substantive question is: does Mike want compaction to fire on *every*
card boundary regardless of context size, or only when context is
actually under pressure?

**My recommendation:** card boundary always (it's a natural narrative
seam), even if context is low. Cheap. But Mike should confirm this
matches "continually" or whether he wants something even more frequent.

---

## Proposed unified design

This is what I'd actually advocate for. It would become
`dev-docs/<n>-agent-teams.md`. Stays opinionated; assumes Mike calls
D1=SQLite, D2=trimmed-structured, D3=allow, D4=manual, D5=token+boundary
+manual.

### 1. Goals & non-goals

**Goals.** Persistent team agents whose memory survives backend session
restarts; one manager + N reports per team; kanban as the work intake;
Tyde-managed compaction with explicit per-backend mode; replay+live
events end-to-end.

**Non-goals (v1).** Nested teams; multi-team membership; matrix
management; agent-created teams; cross-team delegation; project-scoped
team templates; cost/budget tracking; reports promoting helpers to team
members.

### 2. Conceptual model

- **`TeamMember` is the durable identity** (Codex §2, §5). It has a
  `TeamMemberId`, role, backend kind, custom_agent_id, optional
  `last_session_id`, and a `TeamMemberMemory`. It does not have a stable
  `AgentId`.
- **A live `AgentId`** is the current binding to a member, surfaced as
  `TeamMemberBindingNotify { member_id, current_agent_id, status }` on
  the host stream.
- **`Team`** has `id`, `name`, optional `project_id`, exactly one active
  `manager_member_id`, `workspace_roots`. One `TeamBoard` per team.
- **`TeamCard`** has a typed `TeamCardColumnKind`, optional
  `manager_member_id` and `report_member_id`, and `version: u64`.
- **`TeamCardEvent`** is an append-only typed activity log entry. Carried
  on a separate `TeamCardActivityNotify` frame.
- **`TeamMemberMemory`** is keyed by `TeamMemberId`, has a `generation`,
  a `summary_markdown`, an `active_card_ids: Vec<TeamCardId>`, and a
  bounded `recent_turns_text`.

### 3. Persistence model

**SQLite at `~/.tyde/teams.db`**, WAL mode, owned by a `TeamStoreActor`.
Same single-writer pattern as `session.db`. Schemas:

```sql
CREATE TABLE teams (
    team_id           TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    project_id        TEXT,
    workspace_roots   TEXT NOT NULL,        -- JSON array<String>
    manager_member_id TEXT NOT NULL,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    archived_at_ms    INTEGER
);

CREATE TABLE team_members (
    member_id         TEXT PRIMARY KEY,
    team_id           TEXT NOT NULL REFERENCES teams(team_id),
    role              TEXT NOT NULL,        -- 'manager' | 'report'
    state             TEXT NOT NULL,        -- 'active' | 'paused' | 'archived'
    name              TEXT NOT NULL,
    description       TEXT NOT NULL,
    backend_kind      TEXT NOT NULL,
    custom_agent_id   TEXT,
    project_id        TEXT,
    workspace_roots   TEXT NOT NULL,
    last_session_id   TEXT,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
);
CREATE UNIQUE INDEX team_one_active_manager
    ON team_members(team_id) WHERE role = 'manager' AND state = 'active';

CREATE TABLE team_cards (
    card_id           TEXT PRIMARY KEY,
    board_id          TEXT NOT NULL,
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    column_kind       TEXT NOT NULL,
    position          REAL NOT NULL,
    manager_member_id TEXT,
    report_member_id  TEXT,
    version           INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
);
CREATE INDEX cards_by_team_state ON team_cards(board_id, column_kind, position);

CREATE TABLE team_card_events (
    event_id          TEXT PRIMARY KEY,
    card_id           TEXT NOT NULL REFERENCES team_cards(card_id),
    actor             TEXT NOT NULL,         -- JSON-encoded TeamCardActor
    created_at_ms     INTEGER NOT NULL,
    event             TEXT NOT NULL          -- JSON-encoded TeamCardEventKind
);
CREATE INDEX events_by_card ON team_card_events(card_id, created_at_ms);

CREATE TABLE team_member_memory (
    member_id         TEXT PRIMARY KEY,
    generation        INTEGER NOT NULL,
    summary_markdown  TEXT NOT NULL,
    recent_turns_text TEXT NOT NULL,
    active_card_ids   TEXT NOT NULL,         -- JSON array<TeamCardId>
    updated_at_ms     INTEGER NOT NULL
);

CREATE TABLE team_compactions (
    compaction_id     TEXT PRIMARY KEY,
    member_id         TEXT NOT NULL REFERENCES team_member_memory(member_id),
    trigger           TEXT NOT NULL,
    mode              TEXT NOT NULL,
    status            TEXT NOT NULL,
    started_at_ms     INTEGER NOT NULL,
    completed_at_ms   INTEGER,
    previous_generation INTEGER NOT NULL,
    next_generation   INTEGER,
    error             TEXT
);
```

No silent recovery on load. Validation invariants from Codex §3 apply.

### 4. Org structure

- One active manager per team. `team_members.role='manager'` with
  `state='active'` is unique per team.
- Reports report to the team's manager.
- Members belong to one team only.
- Teams do not nest.
- `TeamSetManager` swaps roles atomically (new manager must already be a
  report).
- Manager death does not auto-promote. Human intervenes via
  `TeamSetManager`. (D4.)

### 5. Memory & compaction

`TeamMemberMemory` schema (trimmed):

```rust
pub struct TeamMemberMemory {
    pub member_id: TeamMemberId,
    pub generation: u64,
    pub summary_markdown: String,
    pub recent_turns_text: String,    // bounded to ~4k chars
    pub active_card_ids: Vec<TeamCardId>,
    pub updated_at_ms: u64,
}
```

**Triggers**: `TokenThreshold` (input_tokens > 0.6 × context_window),
`TaskBoundary` (card moves to Done/Cancelled/Blocked), `Manual`. No
wall-clock idle trigger.

**Production**: the live member calls
`tyde_submit_team_memory_update(summary_markdown, recent_turns_text,
active_card_ids)` via the agent-control MCP. Server validates:

- caller's `member_id` matches the request URL's injected member context
- text fields non-empty within bounds
- `active_card_ids` reference cards in the team

On success: bump `generation`, replace memory row, emit
`TeamMemoryNotify::Updated`. On failure (timeout, validation): emit
`TeamCompactionNotify { status: Failed }`, keep prior generation.

**Per-backend mode** (`TeamCompactionMode`):

| Backend | Mode (v1)             | Notes |
|---------|-----------------------|-------|
| Claude  | `NativeCompact` if/when wrapper proves it; else `RestartFromMemory` | Tyde memory is still the canonical durable copy. |
| Codex   | `RestartFromMemory`   | |
| Tycode  | `RestartFromMemory`   | |
| Kiro    | `MemoryOnly`          | Block delegation under hard pressure. |
| Gemini  | `RestartFromMemory`   | |

`MemoryOnly` is honest: Tyde stores memory, but cannot reduce live
context. Hard pressure → server blocks delegation to that member with a
typed visible error.

**Across restarts**: the member has no live `AgentId`; on next wake,
spawn a new live agent of `BackendKind` and `custom_agent_id`, inject
the role context (CustomAgent + Steering) plus
`summary_markdown + recent_turns_text + active_card_ids`. Emit
`TeamMemberBindingNotify` with the new `current_agent_id`.

### 6. Task lifecycle

**Columns** (Codex's): `Backlog | Claimed | Assigned | InProgress |
Blocked | Review | Done | Canceled`.

**Allowed transitions** (matrix from Codex §7, plus my §6.2):

```
Backlog    → Claimed     (manager)
Backlog    → Canceled    (human)
Claimed    → Assigned    (manager picks report)
Claimed    → Backlog     (manager releases / claim expired)
Claimed    → Canceled    (manager / human)
Assigned   → InProgress  (assigned report)
Assigned   → Canceled    (human / manager)
InProgress → Review      (assigned report)
InProgress → Blocked     (assigned report)
Blocked    → InProgress  (assigned report, after unblock)
Blocked    → Assigned    (manager re-prompts)
Blocked    → Canceled    (manager / human)
Review     → Done        (manager / human)
Review     → Assigned    (manager rejects, sends back)
```

Anything else: server emits `CommandError` with `Conflict` or
`Forbidden`. Reports cannot mark cards `Done`.

**Mutations carry `expected_version`**. Stale → `CommandError::Conflict`.
Server emits one `TeamCardNotify::Upsert` snapshot plus one
`TeamCardActivityNotify` event per accepted mutation.

**Manager claim lease**: when the manager moves `Backlog → Claimed`, a
server-owned lease starts (default 5 min, configurable). If the manager
hasn't moved the card to `Assigned` or `Canceled` by then, the
coordinator emits `ManagerClaimExpired` and bounces the card back to
`Backlog`.

### 7. Manager loop

**`TeamCoordinator` actor per team**, subscribed to its team's events.
Wakes the manager via `SendMessage` (using existing queue mechanics from
`16-queued-messages.md`) on:

1. New `Backlog` card.
2. Report moved card to `Review`.
3. Report moved card to `Blocked`.
4. Report agent failed/closed while assigned.
5. Manager claim lease expiring soon.
6. Compaction completed for a report (changed open work).
7. Server restart with non-terminal cards.

**Coalescing**: at most one outstanding wake message at a time. Multiple
events while the manager is busy collapse into one "the board changed —
N new backlog, M reviews waiting" prompt.

**Wake prompt content**: team/member IDs, manager role instructions,
current cards needing action (typed snapshot), report roster (id, name,
description, current load, live status, last failure summary), explicit
instruction to use the team MCP tools. No raw card history unless asked.

**Assignment**: manager calls `tyde_assign_team_card_to_report(card_id,
report_member_id, expected_version)`. Server validates membership,
report state, version. On success: state → `Assigned`, append
`ReportAssigned`, send a `SendMessage` to the *report* with the card
body (queued if busy).

### 8. Protocol changes

All in `protocol/src/types.rs`.

**Typed IDs**: `TeamId, TeamMemberId, TeamBoardId, TeamCardId,
TeamCardEventId, TeamCompactionId`.

**Enums**: `TeamMemberRole {Manager, Report}`,
`TeamMemberState {Active, Paused, Archived}`, `TeamCardColumnKind`
(eight variants), `TeamCompactionMode {NativeCompact, RestartFromMemory,
MemoryOnly}`, `TeamCompactionTrigger {TokenThreshold, TaskBoundary,
Manual, RestartRecovery}`, `TeamCompactionStatus {Started, Completed,
Failed}`, `TeamCardActor {Human, Member{member_id}, Server}`.

**`AgentOrigin` gains `TeamMember` variant.** Validation:
`origin == TeamMember` requires both `team_id` and `team_member_id` on
the agent's birth-certificate payloads; non-team agents must have both
`None`.

**Records**: `Team`, `TeamMember`, `TeamBoard`, `TeamCard`,
`TeamCardEvent`, `TeamCardEventKind` (tagged enum: `Created | Moved |
ManagerClaimed | ManagerClaimExpired | ReportAssigned | NoteAdded |
Blocked | ReviewRequested | Completed | Canceled`),
`TeamMemberMemory` (per §5), `TeamCompactionRecord`.

**Input frame kinds (host stream)**: `TeamCreate, TeamRename, TeamDelete,
TeamSetManager, TeamMemberCreate, TeamMemberUpdate, TeamMemberArchive,
TeamCardCreate, TeamCardUpdate, TeamCardMove, TeamCardClaim,
TeamCardAssignReport, TeamCardAddNote, TeamCardDelete,
TeamMemberCompactNow`. Every card mutation payload includes
`expected_version: u64`.

**Output frame kinds**: `TeamNotify, TeamMemberNotify, TeamBoardNotify,
TeamCardNotify, TeamCardActivityNotify, TeamMemoryNotify,
TeamCompactionNotify, TeamMemberBindingNotify`.

Each `Notify` payload uses tagged `Upsert | Delete` where applicable.
`TeamCardNotify::Upsert` carries a full `TeamCard`;
`TeamCardActivityNotify` carries one `TeamCardEvent`.

**Streams**: team state lives on `/host/<uuid>` (replayed +
live). No new per-team stream — the host stream already does this for
projects, custom agents, etc., and per-team streams add multiplexing
without buying us isolation.

**Replay order on host attach**: settings → projects →
mcp_servers/skills/steering/custom_agents → teams → team_members →
team_boards → team_cards (snapshots) → team_card_events (full history) →
team_member_memory → team_compactions → existing live agents →
team_member_binding (with `current_agent_id: None` for any member not
currently bound).

### 9. MCP surface

Added to existing agent-control MCP (`11-agent-control-mcp.md`). Each
tool maps 1:1 to a typed protocol event handled by `TeamCoordinator`;
the MCP layer is a thin shim, not a parallel control plane.

| Tool                                    | Maps to                  | Caller          |
|-----------------------------------------|--------------------------|-----------------|
| `tyde_list_team_members`                | (read)                   | team member     |
| `tyde_list_team_cards`                  | (read)                   | team member     |
| `tyde_read_team_card`                   | (read)                   | team member     |
| `tyde_claim_team_card`                  | `TeamCardClaim`          | manager only    |
| `tyde_assign_team_card_to_report`       | `TeamCardAssignReport`   | manager only    |
| `tyde_update_team_card_status`          | `TeamCardMove`           | per §6 matrix   |
| `tyde_add_team_card_note`               | `TeamCardAddNote`        | team member     |
| `tyde_submit_team_memory_update`        | (memory write)           | self only       |

Caller identity is derived from the MCP URL injection (the existing
loopback pattern). Members cannot impersonate other members.

If `tyde_agent_control_mcp_enabled` is `false`, all team loops pause
visibly; humans can still operate the board manually.

### 10. Frontend surface

- **Teams panel**: list teams, member roster, manager indicator, current
  live agent binding (from `TeamMemberBindingNotify`), team status.
- **Board view**: fixed columns; cards keyed by `CardId`; drag/drop
  emits `TeamCardMove` with `expected_version`.
- **Card detail**: title, body, current assignments, activity history
  (from `TeamCardActivityNotify` stream), open agent chat link.
- **Member card**: memory generation, last compaction time,
  `summary_markdown` preview, list of `active_card_ids`, manual compact
  button.
- **No refresh button.** All updates flow from server events.

### 11. Failure modes

- **Manager dies in `Claimed`**: lease expires → card returns to
  `Backlog`, `ManagerClaimExpired` event, surfaced in UI.
- **Report dies while `Assigned`/`InProgress`**: coordinator emits
  server-authored `Blocked { reason: "assignee unavailable" }` after a
  timeout, wakes manager.
- **Compaction fails**: previous generation stays current,
  `TeamCompactionNotify { status: Failed }` emitted, manager woken if
  hard context pressure remains.
- **Race on a card**: optimistic concurrency rejects the second writer
  with `CommandError::Conflict`. UI re-renders from the fresh
  `TeamCardNotify::Upsert`.
- **Missing manager**: team blocks new claims/assignments. Human
  must `TeamSetManager`. No auto-promotion in v1.
- **Member archived while assigned**: hard archive rejected if member
  appears on a non-terminal card; user must reassign first.
- **MCP disabled**: managers/reports can't act; humans operate manually.
- **Cycle attempts**: schema prevents (one manager, one team per member,
  no nesting).

### 12. Things still open for Mike

1. **Storage**: SQLite (recommended) vs JSON (Codex). [D1]
2. **Memory schema fields**: trimmed structured (recommended) vs prose-only
   (mine) vs full structured (Codex). [D2]
3. **Reports spawning helpers**: allow (recommended) vs require manager
   delegation. [D3]
4. **Manager death**: manual replacement (recommended) vs auto-promote. [D4]
5. **Compaction frequency**: token+boundary+manual (recommended) vs adding
   wall-clock idle. [D5]
6. **Manager `Done` authority**: manager can mark `Done` directly
   (recommended) vs requires human. (Codex §13 raised this.)
7. **Card history replay volume**: replay all (recommended for v1) vs
   pagination behind an explicit history-stream request. (Codex §13
   raised this; volume isn't a v1 problem.)

---

## Bottom line

Codex's design is more careful than mine on identity (`TeamMemberId` vs
`AgentId`), concurrency (`expected_version`), event shape (split snapshot
+ activity), and per-backend honesty (`TeamCompactionMode` enum). Adopt
those wholesale.

I'd push back on Codex on storage (SQLite is the right call for the data
shape) and on capability tags (premature). Wall-clock-idle compaction is
also dead weight.

The unified design above is what I'd ship. It needs Mike to settle five
remaining design calls.
