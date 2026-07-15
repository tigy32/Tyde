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
automatically — no frontend changes when a backend adds a setting. Some
backends have static schemas; others, including Kiro and Hermes, publish
server-owned dynamic schema states because their valid model lists come from
the backend runtime.

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

4. **`cost_hint` and session settings coexist.** `cost_hint` is a coarse
   programmatic hint (for orchestrators, agent-control MCP). When host-level
   complexity tiers are enabled, the server resolves tier values through the
   backend schema before spawning. For dynamic schemas, non-empty tier/default
   settings are not applied while the schema is pending or unavailable; the
   spawn fails visibly instead of guessing valid values.

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
    /// Optional select options keyed by another setting's selected value.
    /// The field_type options apply while the controlling setting is unset.
    pub select_options_by_setting: Option<SelectOptionsBySetting>,
}

pub struct SelectOptionsBySetting {
    pub setting_key: String,
    pub values: Vec<SelectOptionsForValue>,
}

pub struct SelectOptionsForValue {
    pub setting_value: String,
    pub options: Vec<SelectOption>,
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
  (or `Null` if nullable). When `select_options_by_setting` is present, both
  validation and the generic frontend use the options advertised for the
  controlling setting's current value.
- `Toggle` fields accept only `Bool(b)` (or `Null` to reset to default).
- `Integer` fields accept only `Integer(n)` where `min <= n <= max`
  (or `Null` to reset to default).

### 3.3 Payload Types

```rust
/// Server → Client on host stream.
/// Carries session settings schema states for all enabled backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSchemasPayload {
    pub schemas: Vec<SessionSchemaEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionSchemaEntry {
    Ready { schema: SessionSettingsSchema },
    Pending { backend_kind: BackendKind },
    Unavailable { backend_kind: BackendKind, message: String },
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

When complexity tiers are disabled, the host drops `cost_hint`; backend
defaults apply. When tiers are enabled, the host validates and merges the
configured tier values against the backend schema before spawning, or leaves
the hint for a static backend built-in tier fallback when no user tier config
exists. Codex derives its built-in Low/High reasoning values from the ordered
efforts advertised by its default model metadata. Dynamic schema backends fail
visibly if non-empty tier/default values would apply while their schema is
unavailable.
Tier configuration writes are validated against the current schema and fail
with a typed command error instead of persisting unsupported values.

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
    /// Return the static/base session settings schema for this backend kind.
    /// Dynamic backends can have the host replace this with a probed
    /// `SessionSchemaEntry::Ready` schema.
    fn session_settings_schema() -> SessionSettingsSchema
    where
        Self: Sized;

    // ... existing methods unchanged ...
}
```

Each backend implements this to declare its supported settings. Examples:

**Claude:**
- `model`: Select — haiku, sonnet, opus (default: sonnet, nullable: true)
- `effort`: Select — low, medium, high, xhigh, max (nullable: true).
  `xhigh` and `max` are distinct Claude-native levels; Tyde preserves the
  selected level and does not invent aliases or normalize one level to another.

**Codex:**
- `model`: Dynamic Select from Codex `model/list` metadata.
- `reasoning_effort`: Dynamic Select from the selected model's ordered
  `supportedReasoningEfforts` metadata (nullable: true). Values such as `max`
  remain distinct from `xhigh`; Tyde does not invent or normalize effort
  levels that the selected model did not advertise.

**Antigravity:**
- `model`: Select — exact `agy models` labels such as
  `Gemini 3.5 Flash (Medium)` (default: `Gemini 3.5 Flash (Medium)`,
  nullable: false)

**Hermes:**
- `model`: Dynamic Select from Hermes `model.options` authenticated provider
  models. Labels include provider context and selected values carry the
  provider override back to Hermes.
- `reasoning_effort`: Select — Auto, none, minimal, low, medium, high, xhigh.
- `fast`: Toggle — requests Hermes fast service tier through `config.set fast`

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

The server (host layer) validates settings before spawning:
1. Validate explicit/stored settings against the server-owned schema.
2. If complexity tiers are enabled, merge configured tier settings through the
   same schema or preserve a backend built-in tier fallback hint.
   If tiers are disabled, drop `cost_hint` so backend defaults apply.
3. Package the validated `cost_hint` and `session_settings` into
   `BackendSpawnConfig`.
4. Call `Backend::spawn()`.
5. The backend resolves any remaining backend-owned defaults.
6. The backend reports the resolved values back to the agent actor.
7. The agent actor emits the resolved values as `SessionSettings` on the
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
4. The agent actor waits for the backend to acknowledge the update; Codex does
   not acknowledge until `thread/update` succeeds. This applies to fresh,
   resumed, and forked Codex sessions; each session updates the live values
   used by later `turn/start` calls only after that provider acknowledgement.
5. Only after acknowledgement does it persist and emit the full effective
   settings as `SessionSettings`. A provider rejection emits `AgentError` and
   leaves the prior settings unchanged.

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

The host actor collects schema entries from all enabled backends at startup and
whenever enabled backends change. Static backends return `Ready` immediately;
dynamic backends such as Kiro and Hermes may report `Pending` or `Unavailable`
while the server probes backend-native model/schema sources.

```rust
fn collect_session_schemas(enabled: &[BackendKind]) -> Vec<SessionSchemaEntry> {
    enabled.iter().map(|kind| match kind {
        BackendKind::Claude => SessionSchemaEntry::Ready {
            schema: ClaudeBackend::session_settings_schema(),
        },
        BackendKind::Codex => SessionSchemaEntry::Ready {
            schema: CodexBackend::session_settings_schema(),
        },
        BackendKind::Antigravity => SessionSchemaEntry::Ready {
            schema: AntigravityBackend::session_settings_schema(),
        },
        BackendKind::Tycode => SessionSchemaEntry::Ready {
            schema: TycodeBackend::session_settings_schema(),
        },
        BackendKind::Kiro => probe_kiro_schema_state(),
        BackendKind::Hermes => probe_hermes_schema_state(),
        // The test-only mock backend uses an empty static schema.
    }).collect()
}
```

### 6.2 Settings Resolution (spawn path)

The host validates all explicit, stored, and host-tier-derived session settings
against the server-owned schema before spawning. When complexity tiers are
disabled, `cost_hint` is dropped and backend defaults apply. When tiers are
enabled, the host resolves configured tier values first. Static backends may
apply a built-in fallback from the preserved hint; Codex resolves its fallback
from the current dynamic model schema instead of hardcoding model or effort
names.

In the host/agent spawn path, before calling `Backend::spawn()`:

```rust
let config = BackendSpawnConfig {
    cost_hint,
    startup_mcp_servers,
    session_settings: validated_session_settings,
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
`antigravity_cost_hint_defaults()` functions, extended to return
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
`antigravity_cost_hint_defaults()` functions evolve into
`cost_hint_defaults()` per backend, returning `SessionSettingsValues` instead
of ad-hoc tuples. The resolution logic (section 6.2) is shared — each backend
calls the same `resolve_settings()` helper with its own `cost_hint_defaults()`.

---

## 7. Frontend Implementation

### 7.1 State

```rust
// In AppState:
pub session_schemas: RwSignal<HashMap<BackendKind, SessionSchemaEntry>>,
pub agent_session_settings: RwSignal<HashMap<AgentId, SessionSettingsValues>>,
```

### 7.2 Dispatcher

Handle new frame kinds in `dispatch_envelope()`:

- `FrameKind::SessionSchemas` → parse `SessionSchemasPayload`, update
  `session_schemas` signal (keyed by `backend_kind`).
- `FrameKind::SessionSettings` → parse `SessionSettingsPayload`, update
  `agent_session_settings` signal (keyed by agent ID from stream path).

### 7.3 Chat Input Area

The chat input component reads the selected backend's `SessionSchemaEntry` from
`session_schemas`. `Ready` entries render controls dynamically; `Pending` and
`Unavailable` entries render server-owned status/error state rather than
frontend fallback controls:

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

It stays on both `SpawnAgentParams` and `BackendSpawnConfig`, but it only
affects spawning when host complexity tiers are enabled. If tiers are disabled,
the host drops the hint and backend defaults apply. If tiers are enabled, the
host merges configured tier values after validating them against the schema; if
no user tier config exists, the backend can use its built-in tier fallback. For
dynamic schemas, missing schema means non-empty tier/default values fail
visibly instead of being applied blindly.

### Why `Integer` in addition to `Select` and `Toggle`?

No current backend exposes integer settings, but settings like temperature,
max_tokens, and context window size are real parameters that backends may
expose. `Integer { min, max, step, default }` is a single enum variant — not
a speculative abstraction — and gives the frontend a concrete input widget
(number input or slider) without needing a protocol change later.

### Why schema entries instead of only static schemas?

Some backends have static schemas, but Kiro and Hermes need backend-native
model discovery. `SessionSchemaEntry` lets the server report `Ready`,
`Pending`, or `Unavailable` as typed state without frontend caches or guessed
fallback model lists.

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
- Add static backend schemas and host-owned dynamic schema refresh for backends
  that need native model discovery.
- Implement for each backend (Claude, Codex, Antigravity, Mock, Tycode, Kiro,
  Hermes).
- Add `session_settings: SessionSettingsValues` to `BackendSpawnConfig`
  (keep existing `cost_hint`).
- Evolve `codex_backend_defaults()`, `claude_backend_defaults()`,
  `antigravity_cost_hint_defaults()` into per-backend
  `cost_hint_defaults()` functions that return `SessionSettingsValues` instead
  of ad-hoc tuples.
- Add shared `resolve_settings()` helper (used by each backend in `spawn()`
  to merge cost_hint defaults + explicit session settings + schema defaults).
- Add schema state collection in host actor (`Ready`/`Pending`/`Unavailable`).
- Emit `SessionSchemas` after handshake and on `enabled_backends` change.
- Host validates explicit, stored, and complexity-tier settings against the
  available schema before spawning.
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
