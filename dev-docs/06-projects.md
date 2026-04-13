# Projects

This document specifies project modeling in Tyde2.

It builds on:

- `01-philosophy.md` for the architecture constraints
- `02-protocol.md` for framing and stream rules
- `03-agents.md` for agent lifecycle
- `05-session-resume.md` for persisted session metadata

---

## 1. Goals

We want a first-class server-owned concept of a **project**:

- A project is a named place an agent works in.
- A project has one stable identity and one explicit set of git roots.
- The server persists projects on disk.
- The server replays existing projects to new host subscribers.
- Live project changes are broadcast to every connected host stream.
- Spawning or resuming an agent may explicitly associate that live agent with a
  project.

This is not a frontend cache. Projects are part of the server's state model.

---

## 2. Data Model

Projects are protocol types, not ad hoc server-only structs.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub roots: Vec<String>,
}
```

Rules:

- `id` is server-generated UUID text.
- `name` is user-provided and must be non-empty.
- `roots` is the explicit list of git roots for the project.
- `roots` must contain at least one entry.
- `roots` must not contain empty or duplicate entries.

Projects are stored at `~/.tyde/projects.json`.

The store is server-owned and authoritative. If the file is invalid, loading
fails loudly. We do not silently recover with empty state.

---

## 3. Protocol Additions

### 3.1 Input events

All project mutation inputs are sent on the host stream.

```rust
pub enum FrameKind {
    ProjectCreate,
    ProjectRename,
    ProjectAddRoot,
    ProjectDelete,
    ProjectNotify,
}
```

Payloads:

```rust
pub struct ProjectCreatePayload {
    pub name: String,
    pub roots: Vec<String>,
}

pub struct ProjectRenamePayload {
    pub id: ProjectId,
    pub name: String,
}

pub struct ProjectAddRootPayload {
    pub id: ProjectId,
    pub root: String,
}

pub struct ProjectDeletePayload {
    pub id: ProjectId,
}
```

### 3.2 Output event

The server emits one host event shape for both replay and live updates:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectNotifyPayload {
    Upsert { project: Project },
    Delete { project: Project },
}
```

Why a tagged event instead of separate frame kinds:

- replay and live updates use the same event model
- create/rename/add-root all collapse into `Upsert`
- delete still carries the full deleted project payload, so the client never
  has to look elsewhere for the last known data

---

## 4. Replay Semantics

When a new host stream is registered:

1. The server emits `project_notify/upsert` for every persisted project.
2. Only after all projects are replayed does the server replay existing agents.

This ordering is required. Agents may carry `project_id`, so projects must
exist in the client's model before any agent references them.

This follows the philosophy document directly:

- ownership is explicit in protocol data
- initial state and live updates use the same event model
- no frontend inference or repair logic is needed

---

## 5. Agent and Session Association

`SpawnAgentPayload` gains an explicit optional `project_id`:

```rust
pub struct SpawnAgentPayload {
    pub name: String,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub params: SpawnAgentParams,
}
```

The project association is also carried on:

- `AgentStartPayload`
- `NewAgentPayload`
- `SessionSummary`
- persisted `SessionRecord`

Rules:

- New agent spawn may specify `project_id`.
- Resume may specify `project_id`.
- Resume without `project_id` inherits the stored session's `project_id`.
- If a `project_id` is provided, that project must already exist.
- The server never infers workspace roots from the project.
  `workspace_roots` remain explicit protocol data.

This keeps project ownership explicit while avoiding the forbidden fallback of
"guess the roots from the project."

---

## 6. Deletion Rule

Deleting a project that is still referenced by a stored session would create a
dangling `project_id`. That violates the philosophy rule that invalid states
should be unrepresentable.

So `project_delete` is rejected if any persisted session still references the
project.

The consequence is intentional:

- either the session must be migrated to another project first
- or the session must be removed before the project can be deleted

We do not compensate for dangling references later in the UI.

---

## 7. Storage

Projects are stored in:

`~/.tyde/projects.json`

Shape:

```json
{
  "records": {
    "<project-id>": {
      "id": "<project-id>",
      "name": "Tyde",
      "roots": ["/path/to/repo-a", "/path/to/repo-b"]
    }
  }
}
```

Writes are atomic:

- serialize full store
- write temp file
- `fsync`
- rename into place

This matches the existing session store pattern.

---

## 8. Non-Goals

- No frontend-side project registry.
- No special local-vs-remote project behavior in the client.
- No automatic workspace-root derivation from project roots.
- No silent delete of dangling session references.
