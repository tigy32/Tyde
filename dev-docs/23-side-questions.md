# Side Questions / Backend Forks

Side questions are first-class Tyde agents created from an existing backend
session without mutating that source session. They are for "BTW" questions that
need the parent's context but should not add turns to the parent's transcript.

## Protocol

`AgentOrigin::SideQuestion` identifies an interactive side-question agent. It is
not a backend-native relay and it accepts normal `SendMessage` input.

Clients request a fork with `SpawnAgentParams::Fork`:

```rust
SpawnAgentParams::Fork {
    from_session_id: SessionId,
    prompt: String,
    images: Option<Vec<ImageData>>,
    access_mode: Option<BackendAccessMode>,
}
```

The outer `SpawnAgentPayload.parent_agent_id` is required. Side questions are
owned by a parent Tyde agent even though their backend session is a fork of
`from_session_id`.

Clients learn the correct `from_session_id` from the parent agent's optional
`session_id` on `AgentStartPayload` or `NewAgentPayload`. A freshly emitted
`NewAgent` may omit `session_id` until backend startup finishes, but the
subsequent `AgentStart` includes it once the live backend session is known.
Host bootstrap `NewAgent` snapshots include `session_id` for already-started
agents.

`prompt` is required in the protocol shape. The router applies the same
image-only allowance as new spawns: a blank prompt is accepted only when images
are attached.

The host resolves `from_session_id` from the session store and inherits the
parent session's backend kind, workspace roots, project, custom agent, and stored
session settings. The new agent's `AgentStart` / `NewAgent` payload keeps the
required parent agent link. The persisted child `SessionRecord.parent_id` is
always `from_session_id`.

Forks default to `BackendAccessMode::ReadOnly`. A caller must set
`access_mode: Some(...)` when it intentionally wants a different backend access
mode for the side question.

## True-fork semantics

A true fork means:

- the child receives a fresh backend-native `SessionId`;
- the source session is not resumed, appended to, copied on disk, or otherwise
  mutated by Tyde;
- the child starts as a normal interactive agent, not as a backend-native relay;
- unsupported backends fail with `AgentErrorCode::Unsupported` and no child
  `SessionRecord` is persisted.

Tyde must not implement a fake fork by resuming the parent, snapshotting files,
or copying backend session files. If the backend cannot create a native fork,
Tyde reports unsupported behavior instead.

## Backend matrix

- **Mock**: supported. The mock backend clones its in-memory session record under
  a new UUID and runs the child with the requested prompt. Tests use this for
  deterministic assertions that history was copied and the parent was not
  mutated.
- **Claude**: supported through Claude Code's native
  `--resume <parent-session-id> --fork-session` path. The child backend state is
  not pre-seeded with the parent id; Tyde waits for Claude's emitted child
  session id and stores that as the child `SessionId`, avoiding the existing
  session-id rotation guard.
- **Kiro**: unsupported for now. ACP has an unstable `session/fork` method with
  `sessionId`, `cwd`, and `mcpServers`, but Tyde does not currently capture an
  advertised `session.fork` capability from Kiro's initialize response. Do not
  copy Kiro session files as a fallback.
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
- **Gemini**: unsupported for now. Tyde does not have a native Gemini fork or ACP
  transport path wired up.

## Testing requirements

Native tests must cover at least:

1. Mock end-to-end fork creates an `AgentOrigin::SideQuestion` child.
2. The child has a distinct `SessionId` and persisted `parent_id` lineage.
3. Mock history is cloned into the child, child follow-up input works, and the
   parent history remains unchanged.
4. Unsupported backend fork emits a typed unsupported error and leaves the
   parent session record untouched.

Real-AI backend tests are only needed when changing that backend's fork behavior.
For Claude, target only the Claude-specific tests needed to verify the
`--fork-session` command path and child session-id capture; do not run the full
expensive `backend.rs` suite unless AGENTS.md requires it for the backend change.
For Codex, target deterministic fake app-server tests that verify the
`thread/fork` JSON-RPC request is called with the correct parent thread id, the
returned child thread id is stored, the initial turn targets the child thread,
and JSON-RPC method-not-found errors surface as `AgentErrorCode::Unsupported`
without touching the parent session.
