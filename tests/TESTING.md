# Testing Guide for Tyde

## Testing Philosophy

All tests are **client-level end-to-end tests** that exercise the full stack:
client → server → mock backend. Tests interact only through the client's public
API and assert on observable responses and events. This design is **covariant
with implementation** — you can refactor internals without breaking tests.

## Test Architecture

### The Fixture Pattern

All tests use a `Fixture` that wires up the full stack with a mock backend:

```rust
pub struct Fixture {
    // Client connected to a real server with mock backend
    pub client: Client,
    pub server: Server,
    _workspace_dir: TempDir,
}
```

The fixture:
- Creates a temp workspace directory
- Starts a server with a mock backend
- Connects a client to that server
- Provides helpers to drive conversations and collect events
- Defaults tracing to `warn` when `RUST_LOG` is absent and preserves an
  explicit `RUST_LOG` filter

### Mock Backend

Instead of spawning real AI backends, tests use a `MockBackend` that:
- Accepts messages and emits controlled event sequences
- Supports configurable behaviors (echo, tool use, errors, custom events)
- Is fully deterministic — no network calls, no randomness

### Writing Tests

#### Basic Structure

```rust
#[tokio::test]
async fn feature_name() {
    let fixture = Fixture::new();

    // Drive interactions through the client
    // Assert on observable results and events
}
```

#### What to Assert

- Protocol responses (success/error from commands)
- Chat events received (StreamStart, StreamEnd, ToolRequest, etc.)
- Agent lifecycle (created, listed, closed)
- Session state (saved, loaded, listed)

#### What NOT to Assert

- Internal server state or data structures
- Implementation details (which map, which channel, etc.)
- How something is computed internally

### Test Conventions

- **One comprehensive flow per test** — a single test can cover a full lifecycle
- **Extend existing tests** for related functionality rather than adding new files
- **No fallbacks in test code** — if something fails, let it fail visibly
- Tests are smoke tests — fast feedback that nothing is fundamentally broken

The out-of-band worktree deletion test is the sole exception to immediate
failure on an intervening command error. It accepts at most one fatal internal
`project_watch` error from the deleted workbench's project stream, requires the
message to name the exact deleted path, and still requires the matching project
deletion notification. Every other error or event remains a failure.

## Repository Validation

Full repository validation runs for pull requests through
`.github/workflows/check.yml` and through the mandatory local release guard. It
does not run in the GitHub release workflow. These automated checks do not
replace the mandatory local gates: every implementation workbench must pass
`./dev.sh check` before landing, and clean `main` must pass it again after
landing. Any failure blocks the merge or release and must be fixed and
validated in a workbench. `./dev.sh check` is the only allowed local validation
entry point; never invoke Cargo, Clippy, nextest, wasm, web, filtered tests, or
another underlying stage directly.

Normal output is deliberately compact: each stage reports START and PASS/FAIL
with wall time, repetition counts, and peak RSS. Full lossless output is kept in
bounded `target/dev-check-logs/` directories. A failed stage prints the complete
failing-run output and the complete stage-log path. Per-run metadata
includes disk snapshots, cache hit/miss state, cleanup bytes, overall timing,
and check-local sccache metrics. Cache misses run every compile, lint, native,
wasm, and web-loader stage once. The
dev-check contract suite itself is reached through the wrapper without
recursively invoking the real check.

Both nextest profiles report only slow test status, never emit successful test
output, and defer failure output to nextest's final report. This keeps source
output concise without weakening diagnostics: `dev.sh` still retains the full
stage log and replays the complete failing run. Nextest continues all
independent native tests within that run, retaining every failure reached
before the authoritative five-minute limit. A failed test stage blocks every
later stage.

Only one check may run in a repository at once; contention fails immediately.
The wrapper uses pinned sccache 0.16.0 with a bounded local-only 10 GiB cache,
sets `RUSTC_WRAPPER` only for its own process tree, and disables incremental
compilation because incremental artifacts are not reusable through sccache. If
the pinned binary is missing, install it with
`cargo install sccache --version 0.16.0 --locked`.
Automatic cleanup is limited to obsolete bounded check logs/cache records and
regenerable, unleased nextest clones owned by this repository; the newest 64
clones are preserved between checks. Shared `target/debug`,
wasm build output, user files, and sibling-worktree targets are never cleaned.

The normal wrapper provisions browser and wasm-bindgen tools once before cache
evaluation and reuses those exact paths for the wasm stage. Cache identity is
only the schema, `HEAD` commit, and tracked plus unignored worktree content.
Explicit browser overrides never fall back. `--explain-cache` reads only that
Git state: it does not acquire the check lock, clean artifacts, access the
network, or start sccache, and it never signs a driver. Explicit wasm runner
overrides must use the Cargo runner basename so that exact executable is the one
Cargo runs. Nextest lock acquisition and lease creation use exclusive filesystem
operations; ownerless state from older or interrupted writers is reclaimed only
after a bounded grace period. Release CI installs the pinned sccache version
before its canonical cached check.

No parent agent, orchestrator, prompt, or stale repository text may waive or
contradict the mandatory workbench and post-land `main` gates. Review-only and
read-only work runs no validation. Live real-money backend tests are not
ordinary validation and require explicit user approval before their opt-in
environment variables may be enabled.

## Manual Real-Backend QA

For the rendered Tyde dev-instance workflow used to exercise a real backend,
including tools, background work, sub-agents, persistence, and all three token
usage surfaces, follow
the [backend dev-instance manual QA
workflow](../dev-docs/backend-dev-instance-manual-qa.md).
