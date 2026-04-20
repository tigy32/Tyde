# Backend-Defined Session Settings

This document proposes the design for per-backend, per-session settings in
Tyde2. It builds on the architecture in `01-philosophy.md`, the agent protocol
in `03-agents.md`, and the existing `Backend` trait.

---

## 1. Problem

The old Tyde had a hardcoded `SessionSettingsData` struct with fields like
`model`, `effort`, `reasoning_effort`, `permission_mode`, etc. — a flat bag of
`Option<Option<String>>` fields that every backend partially understood. Adding
a new backend or a new setting meant touching the shared struct, the frontend
form, and every backend's settings handler.

Tyde2 currently has `SpawnCostHint` (low/med/high), which maps to
backend-specific defaults. This is useful for programmatic/orchestrator use
(agent-control MCP), but gives users no fine-grained control.

We need per-backend session settings (model selection, reasoning effort, etc.)
where each backend defines what it supports and the frontend renders controls
automatically — no frontend changes when a backend adds a setting.

---

## 2. Design Principles

1. **Each backend owns its settings definition.** The backend declares a typed
   schema. The frontend renders from it. No frontend code names backend-specific
   settings.

2. **Strong typing everywhere.** Field types are enums (`Select`, `Toggle`,
   `Integer`), not raw JSON Schema. Setting values are a typed enum
   (`SessionSettingValue`), not `serde_json::Value`. The compiler enforces
   structure at every layer. Only the key→value map is dynamic (because keys
   are backend-defined).

3. **Server owns settings state.** The frontend sends setting change requests.
   The server validates, applies, and emits the effective settings back. The
   frontend never computes effective settings.

4. **`cost_hint` and session settings coexist. Backends own the mapping.**
   `cost_hint` is a coarse programmatic hint (for orchestrators, agent-control
   MCP). The host passes both `cost_hint` and `session_settings` through to
   the backend without interpreting either. Each backend decides internally
   how to use `cost_hint` as a fallback for settings the user didn't
   explicitly set. Resolution order: explicit session setting >
   `cost_hint`-derived default > schema default — but the `cost_hint` →
   settings mapping lives inside each backend, not in the host.

5. **Settings are mutable, not part of the birth certificate.** `AgentStart` is
   immutable. Session settings are emitted as a separate event and can change
   mid-session.

---

## 3. Protocol Types

All types belong in `protocol/src/types.rs`.

### 3.1 Schema Types

```rust
/// Schema describing one backend's configurable session settings.
/// The frontend auto-generates UI controls from this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettingsSchema {
    pub backend_kind: BackendKind,
    pub fields: Vec<SessionSettingField>,
}

/// One configurable field in a backend's session settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettingField {
    /// Machine-readable key, e.g. "model", "reasoning_effort".
    /// Unique within a schema. This is the key used in values maps.
    pub key: String,
    /// Human-readable label for the UI, e.g. "Model", "Reasoning Effort".
    pub label: String,
    /// Optional description shown as tooltip or help text.
    pub description: Option<String>,
    /// The type and constraints of this field.
    pub field_type: SessionSettingFieldType,
}

/// The type of a session setting field. Determines how the frontend renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSettingFieldType {
    /// Dropdown / select from a fixed list of options.
    Select {
        options: Vec<SelectOption>,
        /// Default value (must match one of the option values), or None
        /// if the backend has no preference.
        default: Option<String>,
        /// If true, the user can leave this unset (rendered as "Auto" or
        /// "Default" in the UI). If false, a value must always be selected.
        nullable: bool,
    },
    /// Boolean toggle (on/off).
    Toggle {
        default: bool,
    },
    /// Bounded integer (e.g. temperature, max_tokens).
    Integer {
        min: i64,
        max: i64,
        step: i64,
        default: i64,
    },
}

/// One option in a Select field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    /// Machine-readable value sent in the settings map.
    pub value: String,
    /// Human-readable label shown in the UI.
    pub label: String,
}
```

### 3.2 Value and Values Types

```rust
/// A single session setting value. Typed enum — not serde_json::Value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSettingValue {
    String(String),
    Bool(bool),
    Integer(i64),
    Null,
}

/// Current session settings values for an agent.
/// Keys match `SessionSettingField.key` from the schema.
/// Absent keys and `Null` values both mean "use default."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSettingsValues(pub HashMap<String, SessionSettingValue>);
```

**Why `SessionSettingValue` instead of `serde_json::Value`?** Strong typing
always. `serde_json::Value` permits arrays, objects, and floats — none of which
are valid setting values. The typed enum constrains the value space at compile
time and makes `match` exhaustive. The map is still keyed by `String` because
keys are backend-defined, but the value side is fully typed.

**Validation invariant:** The server validates every value against the field's
`SessionSettingFieldType` before accepting it:
- `Select` fields accept only `String(v)` where `v` matches an option value
  (or `Null` if nullable).
- `Toggle` fields accept only `Bool(b)` (or `Null` to reset to default).
- `Integer` fields accept only `Integer(n)` where `min <= n <= max`
  (or `Null` to reset to default).

### 3.3 Payload Types

```rust
/// Server → Client on host stream.
/// Carries session settings schemas for all enabled backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSchemasPayload {
    pub schemas: Vec<SessionSettingsSchema>,
}

/// Client → Server on agent stream.
/// Request to change session settings for a running agent.
/// Partial update: only keys present are changed. Absent keys are untouched.
/// A key set to SessionSettingValue::Null resets that setting to its default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionSettingsPayload {
    pub values: SessionSettingsValues,
}

/// Server → Client on agent stream.
/// The full effective session settings for this agent.
/// Always a complete snapshot — never a delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettingsPayload {
    pub values: SessionSettingsValues,
}
```

### 3.4 FrameKind Additions

```rust
pub enum FrameKind {
    // ... existing variants ...

    // Input events (client → server)
    SetSessionSettings,     // on agent stream

    // Output events (server → client)
    SessionSchemas,         // on host stream
    SessionSettings,        // on agent stream
}
```

### 3.5 Changes to Existing Types

**`SpawnAgentParams::New`** — add optional initial session settings:

```rust
pub enum SpawnAgentParams {
    New {
        workspace_roots: Vec<String>,
        prompt: String,
        images: Option<Vec<ImageData>>,
        backend_kind: BackendKind,
        cost_hint: Option<SpawnCostHint>,
        session_settings: Option<SessionSettingsValues>,  // NEW
    },
    Resume {
        session_id: SessionId,
        prompt: Option<String>,
    },
}
```

**`BackendSpawnConfig`** — add session settings alongside existing cost_hint:

```rust
pub struct BackendSpawnConfig {
    pub cost_hint: Option<SpawnCostHint>,
    pub startup_mcp_servers: Vec<StartupMcpServer>,
    pub session_settings: SessionSettingsValues,  // NEW
}
```

The host passes both `cost_hint` and `session_settings` through without
interpreting either. Each backend owns the mapping from `cost_hint` to
setting defaults — the host never needs to know that "Claude Low = haiku."

**`AgentInput`** — add variant for mid-session setting changes:

```rust
pub enum AgentInput {
    SendMessage(SendMessagePayload),
    UpdateSessionSettings(SetSessionSettingsPayload),  // NEW
}
```

---

## 4. Backend Trait Changes

```rust
pub trait Backend: Send + 'static {
    /// Return the session settings schema for this backend kind.
    /// Static method — does not require a live session.
    /// The schema is constant for a given backend implementation.
    fn session_settings_schema() -> SessionSettingsSchema
    where
        Self: Sized;

    // ... existing methods unchanged ...
}
```

Each backend implements this to declare its supported settings. Examples:

**Claude:**
- `model`: Select — haiku, sonnet, opus (default: sonnet, nullable: true)
- `effort`: Select — low, medium, high, max (nullable: true)

**Codex:**
- `reasoning_effort`: Select — low, medium, high, xhigh (nullable: true)

**Gemini:**
- `model`: Select — gemini-2.5-pro, gemini-2.5-flash, gemini-2.5-flash-lite, etc. (default: auto-gemini-2.5, nullable: true)

**Mock:**
- Empty fields (no configurable settings).

---

## 5. Event Flow

### 5.1 Schema Discovery (connection startup)

After handshake, the server emits `SessionSchemas` on the host stream. This
happens as part of the initial state burst (alongside `HostSettings`,
`BackendSetup`, etc.).

```
Client                              Server
  │                                   │
  │─── Hello ────────────────────────→│
  │←── Welcome ──────────────────────│
  │                                   │
  │←── HostSettings ─────────────────│  (host stream)
  │←── SessionSchemas ───────────────│  (host stream)
  │    { schemas: [                   │
  │      { backend_kind: "claude",    │
  │        fields: [                  │
  │          { key: "model",          │
  │            label: "Model",        │
  │            field_type: {          │
  │              kind: "select",      │
  │              options: [...],      │
  │              nullable: true }},   │
  │          { key: "effort", ... }   │
  │      ]},                          │
  │      { backend_kind: "codex",     │
  │        fields: [...] },           │
  │      ...                          │
  │    ]}                             │
```

If `enabled_backends` changes (via `SetSetting`), the server re-emits
`SessionSchemas` with only the schemas for currently enabled backends.

### 5.2 Spawning with Session Settings

The user selects a backend and configures settings in the chat input area.
On spawn, the frontend includes the chosen settings:

```
Client                              Server
  │                                   │
  │─── SpawnAgent ───────────────────→│  (host stream)
  │    { params: { kind: "new",       │
  │        backend_kind: "claude",    │
  │        prompt: "...",             │
  │        cost_hint: null,           │
  │        session_settings: {        │
  │          "model": "opus",         │
  │          "effort": "high"         │
  │        }                          │
  │    }}                             │
  │                                   │
  │←── AgentStart ───────────────────│  (agent stream, seq 0)
  │←── SessionSettings ─────────────│  (agent stream, seq 1)
  │    { values: {                    │
  │        "model": { "string": "opus" },
  │        "effort": { "string": "high" }
  │    }}                             │
  │←── ChatEvent(TypingStatus) ──────│  (seq 2)
  │←── ChatEvent(StreamStart) ───────│  (seq 3)
  │    ...                            │
```

The server (host layer) passes both through to the backend without
interpreting either:
1. Package `cost_hint` and `session_settings` into `BackendSpawnConfig`.
2. Call `Backend::spawn()`.
3. The backend resolves internally: explicit session setting >
   `cost_hint`-derived default > schema default.
4. The backend reports the resolved values back to the agent actor.
5. The agent actor emits the resolved values as `SessionSettings` on the
   agent stream.

### 5.3 Changing Settings Mid-Session

```
Client                              Server
  │                                   │
  │─── SetSessionSettings ──────────→│  (agent stream)
  │    { values: { "model": "sonnet" }│
  │                                   │
  │←── SessionSettings ─────────────│  (agent stream)
  │    { values: {                    │
  │        "model": { "string": "sonnet" },
  │        "effort": { "string": "high" }
  │    }}                             │
```

The server:
1. Validates the new values against the backend's schema.
2. Merges with current settings (partial update).
3. Forwards to the agent actor as `AgentInput::UpdateSessionSettings`.
4. The agent actor forwards to the backend (via existing `SessionCommand::UpdateSettings` path).
5. Emits the full effective settings back as `SessionSettings`.

If validation fails (unknown key, invalid value for a Select field), the server
emits `AgentError { fatal: false, code: internal, message: "..." }` — the
stream stays alive, the invalid change is rejected.

### 5.4 Replay

When a new frontend connects and replays an agent's event log, the
`SessionSettings` events are part of the log. The frontend sees the initial
settings and any subsequent changes in order.

---

## 6. Server Implementation

### 6.1 Schema Collection

The host actor collects schemas from all enabled backends at startup and
whenever enabled backends change:

```rust
fn collect_session_schemas(enabled: &[BackendKind]) -> Vec<SessionSettingsSchema> {
    enabled.iter().map(|kind| match kind {
        BackendKind::Claude => ClaudeBackend::session_settings_schema(),
        BackendKind::Codex => CodexBackend::session_settings_schema(),
        BackendKind::Gemini => GeminiBackend::session_settings_schema(),
        // ...
    }).collect()
}
```

### 6.2 Settings Resolution (spawn path)

The host does **not** interpret `cost_hint`. It passes both `cost_hint` and
`session_settings` through to the backend in `BackendSpawnConfig`. The
resolution happens inside each backend.

In the host/agent spawn path, before calling `Backend::spawn()`:

```rust
// Host layer — no cost_hint interpretation, just pass-through
let config = BackendSpawnConfig {
    cost_hint: spawn_params.cost_hint,
    startup_mcp_servers,
    session_settings: spawn_params.session_settings
        .unwrap_or_default(),
};
let (backend, event_stream) = Backend::spawn(workspace_roots, config, initial_input).await?;
```

Each backend resolves settings internally in its `spawn()` implementation
using a common pattern:

```rust
// Inside each backend's spawn():
fn resolve_settings(
    config: &BackendSpawnConfig,
    schema: &SessionSettingsSchema,
) -> SessionSettingsValues {
    let mut resolved = SessionSettingsValues::default();

    // 1. Apply cost_hint defaults as the base layer (backend-specific)
    if let Some(hint) = config.cost_hint {
        let hint_defaults = cost_hint_defaults(hint); // private per-backend fn
        resolved.0.extend(hint_defaults.0);
    }

    // 2. Override with explicit session settings (user wins)
    for (key, value) in &config.session_settings.0 {
        match value {
            SessionSettingValue::Null => { resolved.0.remove(key); }
            other => { resolved.0.insert(key.clone(), other.clone()); }
        }
    }

    // 3. Fill remaining missing keys from schema defaults
    for field in &schema.fields {
        if !resolved.0.contains_key(&field.key) {
            if let Some(default) = schema_field_default(&field.field_type) {
                resolved.0.insert(field.key.clone(), default);
            }
        }
    }

    resolved
}
```

Each backend keeps its own `cost_hint_defaults()` — a private function that
maps `SpawnCostHint` to that backend's setting values. This is an evolution
of the existing `codex_backend_defaults()` / `claude_backend_defaults()` /
`gemini_backend_model()` functions, extended to return
`SessionSettingsValues` instead of ad-hoc tuples.

### 6.3 Agent Actor

The agent actor stores the current `SessionSettingsValues`. On spawn, it
receives the resolved settings from the backend and emits them as the
initial `SessionSettings` event.

On `AgentInput::UpdateSessionSettings` (mid-session change):
1. Validate new values against the backend's schema.
2. Merge with current settings (partial update).
3. Forward to backend via `Backend::send()` (using existing
   `SessionCommand::UpdateSettings` path internally).
4. Store the updated values.
5. Emit full `SessionSettings` snapshot to all subscribers.

### 6.4 Backend Consumption

Each backend resolves settings internally in `spawn()`, then reads the
resolved values. Example for Claude:

```rust
// In ClaudeBackend::spawn():
fn cost_hint_defaults(hint: SpawnCostHint) -> SessionSettingsValues {
    use SessionSettingValue::String as S;
    let pairs: &[(&str, &str)] = match hint {
        SpawnCostHint::Low    => &[("model", "haiku"), ("effort", "low")],
        SpawnCostHint::Medium => &[("model", "sonnet")],
        SpawnCostHint::High   => &[("model", "opus"), ("effort", "high")],
    };
    SessionSettingsValues(
        pairs.iter().map(|(k, v)| (k.to_string(), S(v.to_string()))).collect()
    )
}

// In spawn():
let resolved = resolve_settings(&config, &Self::session_settings_schema());
let model = match resolved.0.get("model") {
    Some(SessionSettingValue::String(s)) => Some(s.clone()),
    _ => None,
};
let effort = match resolved.0.get("effort") {
    Some(SessionSettingValue::String(s)) => Some(s.clone()),
    _ => None,
};
// ... use model/effort in UpdateSettings command
```

The existing `claude_backend_defaults()` / `codex_backend_defaults()` /
`gemini_backend_model()` functions evolve into `cost_hint_defaults()` per
backend, returning `SessionSettingsValues` instead of ad-hoc tuples. The
resolution logic (section 6.2) is shared — each backend calls the same
`resolve_settings()` helper with its own `cost_hint_defaults()`.

---

## 7. Frontend Implementation

### 7.1 State

```rust
// In AppState:
pub session_schemas: RwSignal<HashMap<BackendKind, SessionSettingsSchema>>,
pub agent_session_settings: RwSignal<HashMap<AgentId, SessionSettingsValues>>,
```

### 7.2 Dispatcher

Handle new frame kinds in `dispatch_envelope()`:

- `FrameKind::SessionSchemas` → parse `SessionSchemasPayload`, update
  `session_schemas` signal (keyed by `backend_kind`).
- `FrameKind::SessionSettings` → parse `SessionSettingsPayload`, update
  `agent_session_settings` signal (keyed by agent ID from stream path).

### 7.3 Chat Input Area

The chat input component reads the selected backend's schema from
`session_schemas` and renders controls dynamically:

- For `Select` fields: render a dropdown. If `nullable`, include an "Auto"
  option. Pre-select `default` if no value is set.
- For `Toggle` fields: render a toggle switch. Pre-set to `default`.
- For `Integer` fields: render a number input or slider with `min`/`max`/`step`
  constraints. Pre-set to `default`.

When the user changes a setting before spawning, the values are collected into
`SessionSettingsValues` and included in `SpawnAgentPayload.params.session_settings`.

When the user changes a setting on an active agent, the frontend sends
`SetSessionSettings` on the agent stream.

### 7.4 Schema-Driven Rendering

The frontend component is generic — it takes a `SessionSettingsSchema` and
`SessionSettingsValues` and renders:

```
[Model ▾ Opus   ] [Effort ▾ High  ]
```

No component knows about "model" or "effort" specifically. It iterates
`schema.fields` and renders each field by its `field_type`. This is the key
decoupling: adding a new setting to a backend requires zero frontend changes.

---

## 8. What About Resume?

When resuming a session (`SpawnAgentParams::Resume`), settings are restored
from the session store and validated against the current schema:

1. Client sends `SpawnAgent` with `Resume` params (no session_settings field).
2. The host layer loads the stored `SessionSettingsValues` for that session
   from the session store (persisted at session creation / last settings
   change).
3. The host validates stored values against the backend's current schema.
   Invalid values (e.g., a model that was removed) are dropped and replaced
   with schema defaults. This handles schema evolution gracefully.
4. The host passes the validated settings in `BackendSpawnConfig` to
   `Backend::resume()`.
5. The agent actor emits `SessionSettings` on the agent stream with the
   effective values.
6. The frontend renders the resumed agent's settings.

The user can then change settings on the resumed agent via
`SetSessionSettings`, just like on a new agent.

**Session store persistence:** The host layer persists
`SessionSettingsValues` alongside other session metadata (session_id,
backend_kind, workspace_roots). Settings are written on session creation
and on every `SetSessionSettings` update.

---

## 9. Decisions and Rationale

### Why a custom schema type instead of JSON Schema?

JSON Schema is untyped (`Value`), requires a generic renderer, and is far more
expressive than we need. Our schema type has three field kinds (`Select`,
`Toggle`, `Integer`) that cover all current and near-future backends. The types
are Rust enums — the compiler catches structural errors. If we need `Text` or
other field kinds later, we add a variant to `SessionSettingFieldType`.

### Why `SessionSettings` as a separate event instead of in `AgentStart`?

`AgentStart` is an immutable birth certificate (emitted once, never updated).
Session settings are mutable — they can change mid-session. Putting them in
`AgentStart` would violate the birth certificate contract. A separate event
follows the existing pattern: immutable identity in `AgentStart`, mutable state
in typed events.

### Why `SessionSettingValue` enum instead of `serde_json::Value`?

`serde_json::Value` permits arrays, objects, and floats — none of which are
valid setting values. The typed `SessionSettingValue` enum (`String`, `Bool`,
`Integer`, `Null`) constrains the value space at compile time. `match` is
exhaustive. The compiler prevents storing a JSON array as a setting value.

### Why `HashMap<String, SessionSettingValue>` instead of typed per-backend structs?

Typed per-backend structs would require the protocol crate (and by extension
the frontend codegen) to know every backend's setting fields at compile time.
That's exactly the coupling this feature eliminates. The schema provides type
information at runtime; the server validates values against it.

### Why server-side resolution instead of frontend-side?

The philosophy: "server owns behavior, UI only renders state." The frontend
sends raw user selections. The server resolves defaults, validates, and reports
effective values. The frontend never computes "if cost_hint is High and backend
is Claude, then model = opus."

### Why partial updates for `SetSessionSettings` but full snapshots for `SessionSettings`?

Partial updates (client → server) are ergonomic: the user changed one field,
send only that. Full snapshots (server → client) are safe: the frontend always
knows the complete effective state, never has to merge or track incremental
changes. This is the same pattern as `HostSettings`.

### What happens to `cost_hint`?

It stays on both `SpawnAgentParams` and `BackendSpawnConfig`. The host passes
it through without interpretation. Each backend owns the mapping from
`cost_hint` to its own setting defaults — Claude knows that Low means haiku,
Codex knows that Low means low reasoning effort, etc. This keeps
backend-specific knowledge inside the backend and avoids the host needing a
centralized mapping table that couples it to every backend's model tiers.

The existing `codex_backend_defaults()` / `claude_backend_defaults()` /
`gemini_backend_model()` functions evolve into `cost_hint_defaults()` methods
that return `SessionSettingsValues` instead of ad-hoc tuples.

### Why `Integer` in addition to `Select` and `Toggle`?

No current backend exposes integer settings, but settings like temperature,
max_tokens, and context window size are real parameters that backends may
expose. `Integer { min, max, step, default }` is a single enum variant — not
a speculative abstraction — and gives the frontend a concrete input widget
(number input or slider) without needing a protocol change later.

### Why sync `session_settings_schema()` instead of async?

Schemas are static per backend kind — they describe what settings exist, not
their current values. No I/O, no subprocess queries, no network calls. A
synchronous function is the natural signature and avoids unnecessary async
machinery.

---

## 10. Implementation Scope

### Protocol crate
- Add `SessionSettingValue`, `SessionSettingsValues`, `SessionSettingsSchema`,
  `SessionSettingField`, `SessionSettingFieldType`, `SelectOption` to
  `types.rs`.
- Add payload types: `SessionSchemasPayload`, `SetSessionSettingsPayload`,
  `SessionSettingsPayload`.
- Add `FrameKind` variants: `SessionSchemas`, `SetSessionSettings`,
  `SessionSettings`.
- Add `session_settings: Option<SessionSettingsValues>` to
  `SpawnAgentParams::New`.
- Add `UpdateSessionSettings` variant to `AgentInput`.

### Server crate
- Add `session_settings_schema()` to `Backend` trait (sync, static).
- Implement for each backend (Claude, Codex, Gemini, Mock, Tycode, Kiro).
- Add `session_settings: SessionSettingsValues` to `BackendSpawnConfig`
  (keep existing `cost_hint`).
- Evolve `codex_backend_defaults()`, `claude_backend_defaults()`,
  `gemini_backend_model()` into per-backend `cost_hint_defaults()` functions
  that return `SessionSettingsValues` instead of ad-hoc tuples.
- Add shared `resolve_settings()` helper (used by each backend in `spawn()`
  to merge cost_hint defaults + explicit session settings + schema defaults).
- Add schema collection in host actor.
- Emit `SessionSchemas` after handshake and on `enabled_backends` change.
- Host passes `cost_hint` and `session_settings` through to backend without
  interpreting either.
- Add `SessionSettingsValues` tracking in agent actor.
- Handle `SetSessionSettings` input → validate → forward → emit.
- Persist session settings in session store on creation and update.
- On resume: load stored settings, validate against current schema, emit.

### Frontend crate
- Add `session_schemas` and `agent_session_settings` signals to `AppState`.
- Handle `SessionSchemas` and `SessionSettings` in dispatcher.
- Add schema-driven settings controls component (generic, renders from
  schema): dropdowns for `Select`, toggles for `Toggle`, number inputs
  for `Integer`.
- Integrate into chat input area (pre-spawn settings selection).
- Integrate into active agent UI (mid-session settings changes).

### Tests crate
- `SessionSettingValue` and `SessionSettingsSchema` serde round-trip tests.
- Spawn with session settings → verify `SessionSettings` event received.
- `SetSessionSettings` on active agent → verify updated `SessionSettings`.
- Settings validation: reject unknown keys, invalid select values, out-of-
  range integers.
- `cost_hint` + session settings interaction: explicit settings override.
- Resume with stored settings: verify loaded, validated, and emitted.
- Schema evolution: resume with settings that reference a removed option →
  verify graceful fallback to schema default.
