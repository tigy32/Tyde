# Testing Guide for Tyde Server

## Testing Philosophy

All tests in this crate are **end-to-end tests** that interact with the server
only through its public protocol boundary: `InvokeRequest` dispatch and
`ServerFrame` / chat-event output. This design provides several critical
benefits:

### Why End-to-End Tests?

1. **Covariant with Implementation**: Tests are written against behavior, not
   implementation. You can completely refactor internal code without breaking
   tests, as long as the external behavior remains the same.

2. **No Tight Coupling**: Traditional unit tests that reach into internal
   functions, mock private dependencies, or test implementation details become
   obsolete when you refactor. Our tests remain valid regardless of how you
   restructure the internals.

## Test Architecture

### The Mock Backend

Instead of spawning real backend subprocesses (Tycode, Claude, Codex, etc.),
tests use a `MockSession` that implements the `ManagedSession` and
`ManagedCommandHandle` traits. The mock backend:

- Accepts `SessionCommand` messages (SendMessage, CancelConversation, etc.)
- Emits controlled JSON events through an `mpsc::UnboundedReceiver<Value>`
- Supports configurable behaviors via `MockBehavior`

### The Fixture Pattern

All tests use the `Fixture` struct from `tests/fixture.rs`, which provides:

```rust
pub struct Fixture {
    pub server: Arc<ServerState<MockSession, String>>,
    pub chat_events: Arc<Mutex<ChatEventBuffer>>,
    workspace_dir: TempDir,
}
```

### Key Testing Utilities

#### `Fixture::invoke(command, params)` - Send a Protocol Command

The `invoke()` method is the primary way to interact with the server in tests:

```rust
let result = fixture.invoke("list_agents", json!({})).await;
```

What `invoke()` does:
1. Parses the command + params into an `InvokeRequest`
2. Dispatches it through the server's `InvokeHandler`
3. Returns the `Result<Value, String>` response

#### `Fixture::create_conversation()` - Set Up a Conversation

Convenience method that creates a conversation with the mock backend:

```rust
let conversation_id = fixture.create_conversation().await;
```

#### Mock Behaviors

The `MockBehavior` enum controls what the mock backend does when it receives
a `SendMessage` command:

```rust
// Echo back successfully (emits TypingStatusChanged, StreamStart, StreamEnd)
fixture.set_mock_behavior(MockBehavior::Echo);

// Emit a custom sequence of JSON events
fixture.set_mock_behavior(MockBehavior::Events(vec![
    json!({"kind": "TypingStatusChanged", "data": true}),
    json!({"kind": "StreamStart", "data": {}}),
    json!({"kind": "StreamEnd", "data": {"content": "Hello!"}}),
    json!({"kind": "TypingStatusChanged", "data": false}),
]));

// Simulate a backend crash (drop the channel)
fixture.set_mock_behavior(MockBehavior::Crash);
```

## How to Write a Test

### Basic Test Structure

```rust
mod fixture;

#[tokio::test]
async fn test_feature_name() {
    let fixture = fixture::Fixture::new();

    // Create a conversation
    let conversation_id = fixture.create_conversation().await;

    // Send a message
    let result = fixture
        .invoke(
            "send_message",
            json!({
                "conversation_id": conversation_id,
                "message": "Hello"
            }),
        )
        .await
        .unwrap();

    // Assert on the result or collected events
    assert!(result.is_null() || result.is_object());
}
```

### Testing Agent Lifecycle

```rust
#[tokio::test]
async fn test_agent_spawn_and_list() {
    let fixture = fixture::Fixture::new();

    let result = fixture
        .invoke("list_agents", json!({}))
        .await
        .unwrap();

    let agents: Vec<Value> = serde_json::from_value(result).unwrap();
    assert!(agents.is_empty());
}
```

### Testing with Mock Events

```rust
#[tokio::test]
async fn test_conversation_receives_events() {
    let fixture = fixture::Fixture::new();
    fixture.set_mock_behavior(MockBehavior::Echo);

    let conversation_id = fixture.create_conversation().await;

    fixture
        .invoke(
            "send_message",
            json!({
                "conversation_id": conversation_id,
                "message": "Hello"
            }),
        )
        .await
        .unwrap();

    // Give the mock backend a moment to emit events
    tokio::time::sleep(Duration::from_millis(50)).await;

    let events = fixture.drain_chat_events(&conversation_id);
    assert!(!events.is_empty());
}
```

## What NOT to Test

- Don't test internal functions directly
- Don't mock internal dependencies
- Don't test implementation details (data structures, internal state)
- Don't test how something is implemented

**Do** test observable behavior through the protocol boundary.

## Benefits of This Approach

1. **Fearless Refactoring**: Change internal implementation without updating tests
2. **Meaningful Failures**: When tests fail, it means protocol-facing behavior broke
3. **Documentation**: Tests show how the server actually behaves
4. **Regression Prevention**: Behavior remains consistent across refactorings
5. **Faster Development**: Spend time building features, not maintaining brittle tests

## Running Tests

```bash
# Run all tyde-server tests
cargo test -p tyde-server

# Run specific test file
cargo test -p tyde-server --test basic

# Run specific test
cargo test -p tyde-server test_feature_name

# Run with output
cargo test -p tyde-server -- --nocapture
```
