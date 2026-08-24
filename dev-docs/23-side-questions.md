# Side Questions / Backend Forks

Side questions are first-class Tyde agents created from an existing backend
session without mutating that source session. They are for "BTW" questions that
need the source chat's context but should not add turns to its transcript.

A fork is **stand-alone**. It is not owned by the agent it forked from and is
not distinguishable from a chat the user started themselves: it carries
`AgentOrigin::User`, no `parent_agent_id`, and no session `parent_id`. That is
what makes it a top-level chat — it is not nested in the sidebar, not hidden by
the hide-sub-agents filter, not counted in another agent's descendant usage,
not torn down when that agent closes, and not pushed a level deeper into the
sub-agent depth limit (which at the cap silently strips the agent-control
tools). Its session is a root session, so it appears in the ordinary session
list.

## Protocol

A fork is an ordinary interactive agent. Nothing in the protocol marks it as
having been forked once it is running; the only fork-specific surface is the
spawn request.

Clients request a fork with `SpawnAgentParams::Fork`:

```rust
SpawnAgentParams::Fork {
    from_session_id: SessionId,
    prompt: String,
    images: Option<Vec<ImageData>>,
    access_mode: Option<BackendAccessMode>,
}
```

The outer `SpawnAgentPayload.parent_agent_id` must be **absent**. A fork is
defined solely by `from_session_id`; naming an owning parent is rejected as
`InvalidInput` rather than ignored, because silently accepting it is how a fork
becomes a sub-agent that dies with its owner.

Nothing about the source agent has to be live. A fork needs a session id, not a
running agent, so a session whose agent has been closed is still forkable.

Clients learn the correct `from_session_id` from the source agent's optional
`session_id` on `AgentStartPayload` or `NewAgentPayload`. A freshly emitted
`NewAgent` may omit `session_id` until backend startup finishes, but the
subsequent `AgentStart` includes it once the live backend session is known.
Host bootstrap `NewAgent` snapshots include `session_id` for already-started
agents.

`prompt` is required in the protocol shape. The router applies the same
image-only allowance as new spawns: a blank prompt is accepted only when images
are attached.

The host resolves `from_session_id` from the session store and inherits the
source session's backend kind, workspace roots, project, custom agent, and
stored session settings. It inherits nothing else: the new agent's `AgentStart`
/ `NewAgent` payload carries no parent agent link, and the persisted
`SessionRecord.parent_id` is `None` so the forked session lists as a root
session.

Forks default to `BackendAccessMode::Unrestricted`, matching ordinary new
sessions. A caller may set `access_mode: Some(BackendAccessMode::ReadOnly)` when
it intentionally wants advisory read-only guidance for the side question.

## True-fork semantics

A true fork means:

- the child receives a fresh backend-native `SessionId`;
- the source session is not resumed, appended to, copied on disk, or otherwise
  mutated by Tyde;
- the child starts as a normal interactive agent, not as a backend-native relay,
  and outlives the agent it was forked from;
- unsupported backends fail with `AgentErrorCode::Unsupported` and no child
  `SessionRecord` is persisted.

Tyde must not implement a fake fork by resuming the parent, snapshotting files,
or copying backend session files. If the backend cannot create a native fork,
Tyde reports unsupported behavior instead.

## Backend matrix

- **Mock**: supported. The mock backend clones its in-memory session record under
  a new UUID and runs the child with the requested prompt. The store is
  process-global, so a session stays forkable after its agent closes. Tests use this for
  deterministic assertions that history was copied and the parent was not
  mutated.
- **Claude**: supported through Claude Code's native
  `--resume <parent-session-id> --fork-session` path. The child backend state is
  not pre-seeded with the parent id; Tyde waits for Claude's emitted child
  session id and stores that as the child `SessionId`, avoiding the existing
  session-id rotation guard.
- **ACP**: unsupported for now. ACP has an unstable `session/fork` method with
  `sessionId`, `cwd`, and `mcpServers`. Tyde now parses the `initialize`
  response into typed capabilities, but `session.fork` is not among the ones it
  captures, so there is nothing to gate a fork on. Do not copy an agent's
  session files as a fallback.
- **Tycode**: unsupported for now. Tycode source lives outside this repo and the
  currently consumed `tycode-subprocess` protocol exposes `UserInput`, image
  input, cancel, and resume but no `ForkSession` command in Tyde2's write scope.
- **Codex**: supported through the Codex app-server `thread/fork` JSON-RPC
  method. Verified against the current Codex CLI schema (`ThreadForkParams`,
  `ThreadForkResponse`, and `Thread.forkedFromId`). Tyde sends the parent
  `threadId` and stores the returned `result.thread.id` as the child
  `SessionId`. `runtimeWorkspaceRoots` is a valid `ThreadForkParams` field and
  Tyde sends it with the forked thread's roots. `persistExtendedHistory` is
  accepted but deprecated/ignored by current app-server builds; Tyde sends
  `false` to preserve limited-history persistence semantics. Older Codex CLI
  builds that do not expose `thread/fork` must fail gracefully as unsupported
  with an update-Codex message. Do not ship a rollout-file or session-file copy
  fallback.
- **Antigravity**: fork is unsupported for now. Resumable Antigravity sessions
  use the native `agy` conversation UUID as the Tyde `SessionId` and resume via
  exact `--conversation=<UUID>`; legacy Tyde-minted `antigravity-...` sessions
  remain non-resumable because they have no native conversation ID.

## Testing requirements

Native tests must cover at least:

1. Mock end-to-end fork creates a top-level `AgentOrigin::User` agent with no
   `parent_agent_id`.
2. The child has a distinct `SessionId` and no persisted `parent_id`, so it is
   a root session.
3. Mock history is cloned into the child, child follow-up input works, and the
   source history remains unchanged.
4. Closing the source agent leaves the fork running, and its session is still
   forkable afterwards.
5. A fork request naming a `parent_agent_id` is rejected as `InvalidInput`.
6. Unsupported backend fork emits a typed unsupported error and leaves the
   source session record untouched.

Real-AI backend tests are only needed when changing that backend's fork behavior.
For Claude, target only the Claude-specific tests needed to verify the
`--fork-session` command path and child session-id capture; do not run the full
expensive `backend.rs` suite unless AGENTS.md requires it for the backend change.
For Codex, target deterministic fake app-server tests that verify the
`thread/fork` JSON-RPC request is called with the correct parent thread id, the
returned child thread id is stored, the initial turn targets the child thread,
and JSON-RPC method-not-found errors surface as `AgentErrorCode::Unsupported`
without touching the parent session.
