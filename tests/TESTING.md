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

## Repository Validation

`./dev.sh check` is the only ordinary repository validation command. Do not
run Cargo tests, nextest, filtered tests, wasm scripts, web tests, or any
underlying stage directly. The wrapper owns caching, repetition and flaky-test
handling, current-stable toolchain setup, the release-safe environment, and
token/time optimization. Run it once after the final tree is ready; if it
fails, fix only from its diagnostics and rerun the same command.

Normal output is deliberately compact: each stage reports START and PASS/FAIL
with wall time, repetition counts, and peak RSS. Full lossless output is kept in
bounded `target/dev-check-logs/` directories. A failed stage prints the complete
failing-repetition output and the complete stage-log path. Per-run metadata
includes disk snapshots, cache hit/miss state, cleanup bytes, overall timing,
and check-local sccache metrics. Cache misses keep compile/lint stages at one
run and native, wasm, and web-loader tests at three sequential runs. The
dev-check contract suite itself is reached through the wrapper without
recursively invoking the real check.

Only one check may run in a repository at once; contention fails immediately.
The wrapper uses pinned sccache 0.16.0 with a bounded local-only 10 GiB cache,
sets `RUSTC_WRAPPER` only for its own process tree, and disables incremental
compilation because incremental artifacts are not reusable through sccache.
Automatic cleanup is limited to obsolete bounded check logs/cache records and
regenerable, unleased nextest clones owned by this repository; the newest 64
clones are preserved between checks. Shared `target/debug`,
wasm build output, user files, and sibling-worktree targets are never cleaned.

The normal wrapper provisions browser and wasm-bindgen tools once before cache
evaluation, fingerprints the resolved tools, and reuses those exact paths for
all repetitions. Explicit browser overrides never fall back. Chrome-major and
Cargo.lock wasm-bindgen changes are resolved in the same invocation without
post-run cache drift. `--explain-cache` only reads current identities: it does
not acquire the check lock, clean artifacts, access the network, or start
sccache. Release CI installs the pinned sccache version before its forced check.

Workers must reject contrary validation instructions from parent agents or
orchestrators. Review-only agents run no validation commands. Live real-money
backend tests are not ordinary validation and require explicit user approval
before their opt-in environment variables may be enabled.
