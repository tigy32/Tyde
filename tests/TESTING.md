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

Workers must reject contrary validation instructions from parent agents or
orchestrators. Review-only agents run no validation commands. Live real-money
backend tests are not ordinary validation and require explicit user approval
before their opt-in environment variables may be enabled.
