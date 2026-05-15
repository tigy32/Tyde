# Agent Teams — Backend Review

Reviewer: Claude (Opus 4.7)
Scope: `protocol/` and `server/` on `feat/agent-teams` (vs. `main`)
Excluded by request: `frontend/` (separate reviewer)
Branch commits in scope:

- 2d0de55 feat: Add agent team protocol
- 46ba76d feat: Add agent teams store
- f508949 feat: Wire agent team registry
- f492cb1 feat: Add team MCP tools
- e5ce00b feat: Reject referenced team deps
- b6b73ad feat: Harden team activation
- bfec0fe / 4ed5d54 / ed6c48f — frontend (not reviewed)

Diff stat (backend only):

```
protocol/src/lib.rs             |    6 +-
protocol/src/types.rs           |  181 ++++++
protocol/src/validator.rs       |  143 ++++-
server/src/agent/mod.rs         |    2 +
server/src/agent/registry.rs    |    8 +-
server/src/agent_control_mcp.rs |  213 ++++++-
server/src/backend/kiro.rs      |    4 +
server/src/host.rs              | 1308 +++++++++++++++++++++++++++++++++++++-
server/src/lib.rs               |    1 +
server/src/router.rs            |   97 ++-
server/src/store/agent_teams.rs |  859 +++++++++++++++++++++++++
server/src/store/mod.rs         |    1 +
server/src/team_registry.rs     |  909 +++++++++++++++++++++++++++
```

## Summary

The branch lands a deliberately narrow v1 of agent teams: typed protocol records
(`Team`, `TeamMember`, `TeamMemberBindingPayload`, plus seven new `FrameKind`
mutations and three notify kinds), a JSON-on-disk `AgentTeamsStore`, a single
`TeamRegistryActor` that owns bindings, host glue that spawns/resumes a team
member on demand via `message_team_member`, two MCP tools
(`tyde_team_describe`, `tyde_team_message_member`), and replay extensions on
the host stream. Code quality is high: the actor split is clean, the store
performs round-trip validation under `validate_store_file`, mutations are
serialised through the registry actor (no shared `Arc<Mutex<...>>` over the
durable record), and the test suite contains six new host-level integration
tests plus four store unit tests plus three validator tests covering all the
new code paths I would have written tests for myself. `cargo test
-p protocol -p server` is green (182 passed, 7 ignored, 0 failed), and `cargo
clippy -p protocol -p server` is clean.

The biggest gap is **scope, not correctness**. The proposal in
`agent-teams-claude.md` describes a board/cards/columns/memory-compaction
system; this branch ships only team membership + manager-to-report messaging.
Boards, cards, compaction, the `MemoryCompactor` internal agent, the
card-history audit trail, `CardActor`/`BoardColumn`/`CardHistoryEvent`, and
the wider MCP surface (`tyde_team_assign_card`, `tyde_team_compact_agent`,
`tyde_team_read_board`, etc.) are all absent. The store also chose JSON-file
persistence instead of SQLite (proposal §3.1), which is fine for the slice
landed but will not scale to the proposed board volume.

Within the landed scope I found one **high**-severity correctness issue
(stale bindings after agent termination without `close_agent`), two **medium**
issues (TOCTOU on validation refs vs. concurrent project/custom-agent delete;
no recovery path when a member's stored `session_id` becomes non-resumable),
plus several smaller items.

## Findings

### HIGH — Stale `current_agent_id` bindings when an agent terminates outside `close_agent`

`team_registry::clear_binding_by_agent` is invoked from exactly one place:
`HostHandle::close_agent` (`server/src/host.rs:2317-2330`). Any agent that
terminates without going through `close_agent` (backend process exit, crash,
panicked actor, fatal startup failure that doesn't path back through close)
will leave its `TeamMemberBindingPayload.current_agent_id` pointed at a dead
`AgentId` indefinitely.

The downstream effect: `team_registry::plan_message_member` resolves the
member's activation by inspecting `binding.current_agent_id`
(`server/src/team_registry.rs:544-555`) and returns `Reuse { agent_id }` for
that dead id. `HostHandle::message_bound_team_member` then calls
`self.agent_handle(&agent_id)`, gets `None`, and returns
`team member X is bound to missing agent Y`
(`server/src/host.rs:1628-1633`). The error is descriptive but recovery is
manual — there is no path that demotes the binding back to "no current
agent" so the next `message_team_member` could re-spawn.

The host already runs `spawn_host_team_status_task`
(`server/src/host.rs:3794-3875`) which subscribes to agent status changes and
calls `record_agent_activity`. That method keeps `current_agent_id` set even
for `AgentControlStatus::Failed` / terminated statuses
(`server/src/team_registry.rs:632-650`), so it cannot recover the binding.

**Recommended fix:** in `spawn_host_team_status_task`, when the snapshotted
`status.terminated` (or equivalent `Failed`) is observed for an agent that
holds a team binding, also call `clear_binding_by_agent` so a follow-up
message can spawn a fresh session. Equivalently: make
`record_agent_activity` itself clear `current_agent_id` when the incoming
status is terminal. Either change is a few lines and would be testable by
extending `team_resume_failure_marks_binding_failed`.

### MEDIUM — TOCTOU between `agent_team_validation_refs` snapshot and registry mutation

`HostHandle::create_team` / `create_team_member` / `update_team_member`
(e.g. `server/src/host.rs:1802-1816`) snapshot the host's
`custom_agent_ids` + `project_ids` under the state lock
(`agent_team_validation_refs`, `server/src/host.rs:4384-4410`), drop the
lock, then send a `TeamRegistryCommand::CreateTeam { refs, ... }` to the
registry actor. The registry validates against this *snapshot* of refs and
saves. In parallel, another envelope can hit `delete_project` /
`delete_custom_agent`, both of which check team-member references via
`team_registry.snapshot()` (`server/src/host.rs:1315-1331`,
`server/src/host.rs:1416-1432`) — but that check happens BEFORE the in-flight
create_team_member command has been processed by the registry.

Interleaving:

1. T1: `create_team_member` snapshots refs (project P exists). Submits to
   registry.
2. T2: `delete_project(P)` snapshots team members (no reference yet).
3. T1: registry processes create_member, validates against stale refs,
   persists.
4. T2: deletes P.

Result: the team member references a deleted project. The store's
`validate_store_file` will reject this on the next save *if* it ever runs
with up-to-date refs, but in v1 there is no path that re-validates with
fresh refs — the in-memory copy stays inconsistent until restart, at which
point `AgentTeamsStore::load` will fail with
`team member references missing project P` and the server panics on boot.
The same race exists for `CustomAgentId`.

**Recommended fix:** funnel both sides through a single ordering authority.
Two reasonable shapes:

- Have the registry actor *itself* perform the deletion check: when the
  host processes `delete_project`, it asks `team_registry` to atomically
  check-and-reject, and only on success commits the delete. Or:
- Have the registry actor receive a `validation_refs` *callback* (an
  `Arc<dyn Fn()>`-style hook) that reads live refs at the moment of
  mutation, so the snapshot it validates against is taken inside the actor's
  serialised mutation step rather than at the caller's earlier point.

The second is closer to the existing structure. Either way the test
`team_references_block_custom_agent_and_project_delete` covers the easy
direction; the racy direction is not tested.

### MEDIUM — A member's stored `session_id` is sticky even when the session is gone

Once `set_member_session_id` writes a session id
(`server/src/store/agent_teams.rs:368-396`), nothing in the team surface
clears it. If the underlying `SessionStore` record is later deleted or marked
`resumable = false`, `ensure_team_resume_session` will fail
(`server/src/host.rs:1764-1778`), `record_binding_failure` will mark the
binding `Failed`, but the *next* `message_team_member` call will plan the
same `Resume { session_id }` activation again and fail the same way. There
is no operator-visible reset action.

The `team_resume_failure_marks_binding_failed` test asserts the binding goes
to `Failed` (good) and notes `report.session_id` is still
`Some(&bad_session_id)` (also asserted, not flagged as a bug). I read that
assertion as documenting the *current* behaviour, not endorsing it.

**Recommended fix:** on resume failure, either (a) clear
`TeamMember.session_id` so the next activation falls through to `New`, or
(b) add a `TeamMemberClearSession` payload/MCP-tool so an operator can do it
explicitly. (a) is cheaper and matches the proposal's "the memory record IS
the resume mechanism" stance (§5.6). The store API already supports the
write, only the helper is missing.

### MEDIUM — Mutating an archived team is allowed

`archive_team` sets `Team.archived_at_ms = Some(now)`
(`server/src/store/agent_teams.rs:173-189`) but none of the downstream
mutations — `rename_team`, `set_manager`, `create_member`, `update_member`,
`archive_member` — check `archived_at_ms`. A client can rename or grow an
archived team, or demote/promote its manager. The frontend may avoid this
in normal usage but the protocol/server contract should not depend on that.

**Recommended fix:** add an `assert_team_active(&team_id)` guard at the top
of the mutating store methods that rejects when `archived_at_ms.is_some()`.
Trivially testable.

### MEDIUM — `tyde_team_describe` / `tyde_team_message_member` are registered on all agents

The MCP tools are added unconditionally to `TydeAgentControlMcpServer`
(`server/src/agent_control_mcp.rs:357-414`). The authorisation check
("caller is a team member" / "caller is the active manager") runs only on
invocation, returning a runtime error. This is functionally safe, but every
agent now sees two extra tools in its tool list, with prompts that imply
team semantics ("Manager-only: send a message to an active report..."). For
a non-team agent the tools are noise that consumes context and invites the
LLM to try them.

The proposal §9.2 says these tools should be available to "any agent" /
"manager", not necessarily filtered at registration time. I do not think
this is a defect against spec — but worth a note. If filtering at
registration is wanted, it would need the MCP loopback URL → agent_id
lookup to be available at the time tool descriptors are generated, which is
a structural change.

### LOW — `TYDE_AGENT_TEAMS_STORE_PATH` env override is read every load but undocumented

`AgentTeamsStore::default_path` (`server/src/store/agent_teams.rs:52-62`)
honours `TYDE_AGENT_TEAMS_STORE_PATH`. The other host stores
(`SessionStore`, `ProjectStore`, etc.) do not seem to take comparable env
overrides — this is the first I noticed. No issue functionally, but if it's
meant for tests only, gating behind `cfg(test)` or documenting in
`dev-docs/` would prevent surprise. If it's meant for users, it should
appear in the host docs alongside `~/.tyde/agent_teams.json`.

### LOW — `team_registry_error` heuristic dispatch on substring matches

`server/src/host.rs:3471-3484` classifies registry errors into `NotFound` /
`Conflict` / `Internal` / `Invalid` based on substring matches like
`"missing"`, `"already"`, `"active manager"`, `"live-bound"`. This works
today because all error strings come from a small set of constructors, but
it's fragile: rewording an error message will silently re-classify it.

For comparison, `project_command_error` next to it does the same thing for
git errors, so this isn't a new pattern in the codebase. Still — the team
errors are *typed* internally up to the boundary; promoting to a typed
`TeamRegistryError` enum and mapping that to `AppError` would be a small
change with real durability benefit, and is much easier to do now (one
caller) than later.

### LOW — `spawn_unbound_team_member` busy-polls for session binding

`wait_for_agent_session_id_result` (`server/src/host.rs:1731-1762`) loops
with `tokio::time::sleep(Duration::from_millis(10))` for up to 30s waiting
for the agent's session id to land in `state.agent_sessions`. This works
but burns wakeups; with N concurrent first-time team activations it scales
poorly. The agent registry already has a `watch::channel`-shaped status
handle (`agent_status_handle`); a one-shot or `watch` on session-id
assignment would be drop-in. Not urgent — busy-poll latency is bounded —
but worth flagging.

### LOW — Concurrent first-time `message_team_member` calls can leak a spawned agent

Two concurrent `message_team_member` calls against the same currently
unbound member each see `TeamMemberActivation::New` from
`plan_message_member` (the plan is read-only and not serialised with the
subsequent spawn). Both will call `spawn_unbound_team_member`. Both spawns
proceed; the first to call `bind_member_agent` writes
`TeamMember.session_id` via `set_member_session_id`. The second's
`bind_member_agent` finds `member.session_id = Some(other_session)` and
returns
`team member X session_id A does not match agent session B`
(`server/src/team_registry.rs:577-580`). At that point the second spawned
agent is alive, attached to host streams, with no team binding — a leaked
fresh agent the user did not ask for, costing money for as long as it
lives.

The fix is to serialise the plan+spawn pair through the registry actor so
the "I'm about to spawn for this member" intent is recorded atomically and
a concurrent call sees it. Equivalently, add a `reserve` step on the
registry before the host spawns. Until then, document the constraint and
have callers not race themselves; in practice the manager LLM is the only
caller and is unlikely to fire two concurrent delegations for the same
report, but the MCP loopback does not enforce that.

### LOW — Manager roster prompt only injects on first spawn, only for managers

`prepend_manager_roster` is only called for the manager's first
`TeamMemberActivation::New` (`server/src/host.rs:1592-1599`). The roster
text is never re-injected on resume or on subsequent messages, even though
the roster can change between turns
(`create_member` / `archive_member` / `set_manager` all run). And reports
get no team context at all — a freshly spawned report agent's only
indication that it's on a team is the `team_id` / `team_member_id` on its
`AgentStartPayload` and the presence of the team MCP tools.

This is consistent with the proposal's "we just give it good inputs"
philosophy in §7.2, but worth highlighting because a report that doesn't
know which team it's in cannot meaningfully use `tyde_team_describe`
(the tool resolves the team from the *caller's* binding, so it works
without the LLM knowing the team_id — but it can't message peers because
that's manager-only anyway). Net: probably fine for v1, will need revisit
when reports get more responsibility.

### LOW — `prepend_manager_roster` uses raw multi-line string literals with embedded newlines

`server/src/host.rs:4038-4097`. The implementation uses literal newlines
inside `block.push_str(...)` calls. It works, but it's hard to read,
especially the trailing `"\n"` markers. A `writeln!` against a `String`
or a single `format!` template would be more legible and less error-prone
if the format ever changes. Cosmetic.

### LOW — `Team` / `TeamMember` / payload structs lack `schemars::JsonSchema`

Most protocol payloads derive `JsonSchema`; the new team types do not (see
diff in `protocol/src/types.rs`). This is fine because the MCP tool input
structs (`TeamMessageMemberToolInput`) do have it, and the output
serialisation is plain `serde_json`. But if these types ever go into a
tool *output* schema or a generated TypeScript binding pipeline, the
omission will bite. Trivial to add now.

### NIT — `STORE_VERSION = 1` with no migration path defined

`server/src/store/agent_teams.rs:14`. Acceptable for v1 since the only
acceptable version is 1, but no test asserts what happens when a v0/v2
file is loaded today (`validate_store_file` returns
`agent teams store version must be 1, got X`). Worth a tiny test pinning
the failure mode so a future schema change can't silently bypass it.

### NIT — `Display` for `TeamId` / `TeamMemberId` exposes the raw UUID without prefix

Other IDs in the protocol follow the same pattern, so this is consistent.
Just noting that the absence of a `team_` / `tm_` prefix means log lines
mix opaque UUIDs from multiple ID types, harder to scan. Out of scope to
change just for teams.

## What's right

These are intentionally non-trivial design choices that landed cleanly:

- **Actor-owned mutations.** `TeamRegistryActor` is the only writer for
  `AgentTeamsStore` (`server/src/team_registry.rs:128-465`). Reads go
  through it too, so there's no `Arc<Mutex<...>>` over the file. This is
  exactly the "actors over locks" pattern the codebase already uses for the
  agent registry and review registry.
- **Replay ordering.** `register_host_stream` emits HostSettings →
  CustomAgents → **Teams** → **TeamMembers** → existing NewAgents (which
  may carry `team_id`/`team_member_id`) → **TeamMemberBindings**
  (`server/src/host.rs:445-570`). Teams precede agents that reference them,
  per the proposal §8.7. Bindings come after the agents themselves, so the
  binding's referenced `current_agent_id` is already present by the time
  the frontend tries to render it.
- **Round-trip validation.** `AgentTeamsStore::load` runs
  `validate_store_file` on disk content
  (`server/src/store/agent_teams.rs:46-50`), and `validate_and_save` runs
  it again before each persist (`:398-401`), so the store cannot save a
  state that wouldn't reload. The validator enforces the one-active-manager
  invariant, manager-team consistency, session-uniqueness across members,
  and ref integrity for `CustomAgentId` / `ProjectId`.
- **AgentOrigin invariants are validator-enforced.** The protocol's
  `validate_agent_origin` (`protocol/src/validator.rs:407-426`) requires
  `TeamMember` origin to carry both `team_id` and `team_member_id`, and
  conversely rejects non-team origins with team ids set. Test coverage
  includes both directions
  (`accepts_team_member_origin_with_team_fields`,
  `rejects_team_member_origin_without_team_fields`,
  `rejects_non_team_origin_with_team_fields`).
- **`tyde_team_message_member` authorisation is server-side and typed.**
  The MCP tool returns a structured `TeamToolError { code: Authorization,
  message }` when the caller is not the active manager, which is easier
  for the caller LLM to react to than a free-form string. The
  `team_message_member_rejects_report_caller` test pins this.
- **Project / custom agent delete is blocked while referenced.**
  `delete_project` and `delete_custom_agent` now both consult
  `team_registry.snapshot()` and refuse to delete a record a team member
  references (`server/src/host.rs:1315-1331`, `:1416-1432`), with a
  `Conflict` `AppError`. The `team_references_block_custom_agent_and_project_delete`
  test exercises both. (Caveat: the racy direction is not covered; see
  MEDIUM finding above.)
- **`team_first_message_records_report_session_id` and
  `team_subsequent_unbound_message_resumes_session`** together prove the
  full lifecycle: first delegation spawns a fresh agent, captures the
  session id into the persisted member record, and a follow-up message
  after the agent is closed correctly takes the `Resume` path against the
  recorded session id, with the same `session_id` preserved across the
  re-spawn.
- **`archive_member` blocks live-bound members.**
  `server/src/team_registry.rs:752-774` plus
  `team_archive_rejects_live_bound_member` — refusal is a
  `Conflict`, not a silent ignore.

## Build / test results

```
$ cargo build -p protocol -p server
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.30s

$ cargo test -p protocol -p server
   ...
   test result: ok. 182 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out
   Doc-tests protocol: ok. 0 passed; 0 failed; ...
   Doc-tests server:   ok. 0 passed; 0 failed; ...

$ cargo clippy -p protocol -p server --no-deps
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.78s
   (no warnings)
```

New tests added on this branch (backend):

Protocol (`protocol/src/validator.rs`):
- `accepts_team_member_origin_with_team_fields`
- `rejects_team_member_origin_without_team_fields`
- `rejects_non_team_origin_with_team_fields`

Store (`server/src/store/agent_teams.rs`):
- `member_create_persists_and_load_round_trips`
- `archiving_active_manager_is_rejected`
- `set_member_session_id_rejects_duplicate_session_owner`
- `update_member_validates_project_references`

Host integration (`server/src/host.rs`):
- `team_first_message_records_report_session_id`
- `team_subsequent_unbound_message_resumes_session`
- `team_message_member_rejects_report_caller`
- `team_resume_failure_marks_binding_failed`
- `team_archive_rejects_live_bound_member`
- `team_references_block_custom_agent_and_project_delete`

Coverage gaps I'd add before declaring v1 stable:

- A test that asserts what happens when an agent attached to a binding
  terminates without `close_agent` — currently nothing covers this, and the
  HIGH finding above lives in that gap.
- A concurrency test for two simultaneous first-time `message_team_member`
  calls against the same member (LOW finding).
- A test for `archive_team` followed by `rename_team` /
  `create_member` to pin whether mutations on archived teams should be
  rejected (MEDIUM finding).

## Scope vs. proposal

For the record, items from `agent-teams-claude.md` that are NOT in this
branch (so a reader doesn't expect them):

- Boards, cards, columns, card history, `CardActor`, `BoardColumn`,
  `CardHistoryEvent`, `CardEdit`/`CardMove`/`CardAssign`/`CardCommentAdd`.
- `AgentMemory` record, `MemoryCompactor` agent, compaction triggers
  (token threshold, card boundary, explicit), `AgentMemoryNotify`.
- `TeamMemberCompactNow` / `tyde_team_compact_agent` etc.
- SQLite store at `~/.tyde/teams.db` (proposal §3.1) — current
  implementation is JSON on disk at `~/.tyde/agent_teams.json`. For the
  v1 surface (teams + members, manager messaging) JSON is fine; for boards
  it would not be.
- Per-team stream `/team/<team_id>/<instance_id>`. Current implementation
  fans out `TeamNotify` / `TeamMemberNotify` / `TeamMemberBindingNotify`
  on the *host* stream only. That's sufficient for the v1 surface and
  simpler — no per-team subscription state to manage — but the proposal's
  rationale (avoid spamming full board state to every host subscriber)
  will reappear when boards land.

These are scope choices, not defects. Flagged here only because the
proposal document is in-repo and a future reader comparing the two might
otherwise assume the branch is incomplete against a v1 contract that
was, in practice, narrowed.
