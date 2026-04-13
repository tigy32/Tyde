# Tyde2 Design Document

This is the single source of context for all agents working on Tyde2. Read this
first. Follow it exactly.

---

## Architecture Philosophy

These rules are non-negotiable. If a design or change violates any of them, it
is wrong.

1. **There must be one source of truth.**
   The protocol lives in Rust in `protocol/src/types.rs`, and everything else is
   generated from that. No handwritten duplicate command maps, payload shapes,
   parsers, or serializers. If a field mismatch can happen at runtime, the
   architecture is wrong.

2. **The server owns behavior; the UI only renders state.**
   `tyde-server` owns the real model of the world and emits state changes as
   events. The frontend reacts to those events. It does not reconstruct backend
   semantics, interpret raw subprocess output, or maintain its own model of
   what's happening.

3. **Local and remote are the same abstraction.**
   The frontend does not know or care about SSH, transport details, or
   remote-specific hacks. "Remote" is just another host connection. If the UI
   needs special-case logic for remote vs local, the boundary is in the wrong
   place.

4. **The Tauri layer must be as dumb as possible.**
   It proxies typed protocol messages. It does not invent new semantics,
   reinterpret state, or manually parse ad hoc payloads. If Tauri has protocol
   awareness beyond "deserialize, dispatch, serialize," the layering is wrong.

5. **State flows through events, not hidden caches.**
   Initial state and live updates use the same event model. Subscriptions come
   first, then the server replays current state as events. Caches and mirrors
   are a smell unless they are strictly derived and unavoidable.

6. **Do not cache by default.**
   Read from the source of truth directly unless profiling or tests prove a
   cache is necessary. Default caches create coherence bugs, race conditions,
   and hidden invalidation rules. Add a cache only after the uncached path is
   measured and shown to be too slow.

7. **Ownership must be explicit in the protocol.**
   If a session, project, or event belongs to a host, that ownership is encoded
   directly in the protocol — not inferred indirectly or patched in later.
   Routing should be obvious from typed data.

8. **Everything must use protocol types end-to-end.**
   Host actors, registries, and connection handlers pass protocol enums/payloads
   directly. Do not translate protocol frames into parallel app-level structs
   that duplicate shape and semantics. If we need additional variants, add them
   to the protocol as enums and use those directly. The only allowed local
   fields are runtime-only transport details (channels/handles) that cannot be
   serialized on the wire.

### Bug-Fix Philosophy

1. A real bug fix starts with root cause.
2. The best fix usually removes code.
3. Fixes should improve the architecture, not just the outcome.
4. Invalid states should be unrepresentable.
5. Workarounds are usually design debt.

### Enforcement Gates

These are concrete checks. If any of these are violated, the change is rejected:

- No handwritten protocol types in the frontend — all protocol types come from
  codegen.
- No protocol mirror types in server internals — do not introduce app-level
  frame/command/payload structs that duplicate protocol semantics. Extend
  protocol enums/types instead.
- No business/domain state in the frontend or Tauri bridge — the server owns it.
- No local-vs-remote branching outside `tyde-server` — the frontend and Tauri
  bridge must be transport-agnostic.
- If required UI data is missing, add protocol events/types in the server — do
  not add frontend caches or workarounds.

The shortest version is:

You want a server-centric, event-driven, typed architecture with one canonical
protocol definition, thin transport layers, explicit ownership, minimal caches,
and bug fixes that remove root causes instead of papering over them.

---

## Strong Typing, Always

- **Always use enums over strings.** There is literally never a reason to avoid strong typing. If a field has a known set of values, it's an enum.
- **Use typed wrappers for semantic values.** Versions are semver (`Version` struct), not bare strings. Stream paths are `StreamPath`, not `String`. IDs are typed newtypes, not raw strings.
- **Lean on the compiler.** If the compiler can catch it, the compiler should catch it. Prefer `match` over `if let` so the compiler forces you to handle new variants.

## No Fallbacks, No Inference

- **NEVER implement fallback functionality.** If something fails, let it fail visibly — log it, show a notification, or let it propagate. Never silently swallow errors (`catch { return {}; }`, `if (!x) return;`, `unwrap_or_default()`).
- **NEVER infer or guess parameters that should be known.** No heuristic lookups, no "find the most likely match", no auto-fill from context. If a value should be available, plumb it explicitly through the call chain.
- **One call path, least branching possible, always works or errors.** If you find yourself writing "try A, fallback to B", stop — fix A instead.

## Fail Fast, Fail Loud

- **Sequence numbers on the wire.** Every message gets a monotonic sequence number. Assert ordering. If a message arrives out of order, that's a bug — crash and show what's wrong immediately.
- **No compensation code for bugs.** Do not write code that attempts to recover from situations that shouldn't happen. If a sequence number gap occurs, panic with context — don't silently skip or reorder.
- **Prefer crashing with diagnostics over silent degradation.** A crash with a clear error message is infinitely better than a complex web of bugs compensating for other bugs.
- **Assertions everywhere.** `debug_assert!` in development, hard errors in protocol validation. If an invariant can be checked, check it.

## Events In, Events Out

- **The protocol is not request/response.** It is bidirectional event streams. The client sends events to the server (e.g. "send message to agent"). The server sends events to the client (e.g. "stream delta", "agent status changed"). There is no pairing of request→response.
- **The UI subscribes to output events and renders based on what it receives.** It does not "call a function and wait for a result." It fires an event and reacts to whatever events come back on the relevant stream.
- **No request IDs, no response correlation.** Streams are the correlation mechanism. If the client sends a "send message" event on a stream, subsequent events on that stream are the result — but they're events, not responses.

## Actors Over Locks

- **Prefer single-task actors over `Arc<Mutex<T>>`.** An actor runs on one tokio task, owns its state, and receives messages via channels. No locking needed because there's no concurrent access.
- **`Arc<Mutex<T>>` is a code smell.** If you find yourself reaching for it, ask whether the thing holding state should be an actor instead. The answer is almost always yes.
- **Actors communicate via typed channels** (`mpsc`, `oneshot`). The actor loop receives messages, processes them sequentially, and sends responses. No shared mutable state, no lock contention, no deadlocks.
- This won't always be possible — some things genuinely need shared state. But the default should be actors, and shared state should be the exception that requires justification.

## Keep It Simple

- Only make changes that are directly requested or clearly necessary.
- Don't add features, refactor code, or make "improvements" beyond what was asked.
- Don't add error handling for scenarios that can't happen.
- Don't create helpers or abstractions for one-time operations.
- Three similar lines of code is better than a premature abstraction.

---

## Design Decisions Log

Decisions made during protocol design that establish precedent:

### Envelope `kind` field: Enum, not string
Both Claude and Codex agents proposed the protocol envelope. Claude suggested `kind: String` for "extensibility", Codex suggested `kind: FrameKind` enum. **Decision: enum.** Strong typing always wins. Unknown message kinds should be a compile-time error, not a runtime surprise.

### Versions: Strongly typed semver
Both agents proposed `tyde_version: String`. **Decision: use a proper `Version` struct with semver semantics.** `protocol_version` is `u32` (simple integer bump). `tyde_version` is a typed semver value, not a bare string.

### Sequence numbers: Yes
Both agents proposed omitting sequence numbers ("transport guarantees ordering"). **Decision: include them.** Sequence numbers enable strong assertions — if messages arrive out of order or with gaps, that's a bug we want to catch immediately, not silently accept. Fail fast > trust blindly.

### Post-handshake message kinds: Deferred
Claude proposed application-level kinds (`invoke`, `result`, `event`). Codex proposed generic kinds (`data`, `end`, `error`). **Decision: deferred.** We'll design these when we get to the features that need them.

### Parse strategy: Deferred
Codex proposed dual types (`Envelope` for routing + `WireMessage`/`TypedFrame` for typed handling). Claude proposed `Envelope` + parse-on-demand. **Decision: deferred.** We'll see what feels right during implementation.
