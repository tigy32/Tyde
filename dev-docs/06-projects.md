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

Deleting a project detaches dependent metadata before removing the project
record:

- persisted sessions keep their history, but their `project_id` is cleared
- project-scoped steering is deleted
- team members drop the deleted project id from `project_ids`

If a team member loses its last project, it remains in the team with an empty
`project_ids` list and cannot spawn a fresh project-bound agent until a project
is assigned again.

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

## 9. Backend workspace relocation

`Backend::set_workspace_roots(&mut self, Vec<String>)` changes the execution
workspace of an idle conversation without sending a prompt or replacing its
session identity. It is a backend primitive, not a project-move protocol event:
the server and frontend do not yet expose it as a move action.

The first root is the default working directory. Unsupported root sets are
rejected rather than truncated. Callers must serialize relocation with other
input, settings, and compaction. The implementations reject ongoing turns,
tracked background work, and compaction. Initial support requires existing,
absolute local directories and rejects empty, duplicate, missing, file, and
remote roots; SSH sessions cannot be relocated through this operation.

| Backend | Implementation | Root sets |
| --- | --- | --- |
| Codex | `thread/settings/update` acknowledges cwd; subsequent `turn/start` requests carry the replacement runtime roots | One or more |
| Hermes | `session.cwd.set`; verifies returned directory | Exactly one |
| Claude | Native `set_cwd` control request; verifies directory and transcript relocation | Exactly one |
| Kiro | ACP `session/load` with the existing session ID and new directory; suppresses replayed history and restores model/mode | Exactly one |
| OpenCode | Explicit unsupported result; ACP reload acknowledges the new directory but native tools retain the original cwd | None |
| Grok | Explicit unsupported result; live `session/load` looks for its transcript under the destination directory scope | None |
| Antigravity | Explicit unsupported result; restarted conversations retain terminals with old or inconsistent default directories | None |

The supported paths passed real relocation conformance on 2026-09-11; see
`backend-conformance.md` for models and limitations. ACP providers must
advertise `session/load` and honor its directory argument for an existing
session. Claude
requires a persistent session and a destination trusted by the native CLI;
its trust-required response is returned as an error without asserting user
consent. Its native setter also refreshes destination project configuration
and relocates the transcript. Hermes waits up to ten seconds for native
post-turn cleanup when its cwd setter explicitly reports busy without mutation. Codex stages runtime roots
in the live adapter after the native cwd acknowledgement; those roots are
materialized on the next turn, including Tyde background wake turns. Callers
must persist the requested roots for later process-level resume.

`real_workspace_relocation` exercises the same history-preserving move, real
file reads/writes, invalid-request rejection, and return move on every backend
declaring `SetWorkspaceRoots`. `real_multiple_workspace_relocation` repeats
that flow with two destination roots for `SetMultipleWorkspaceRoots` backends.
The scenarios never give the model the destination paths or file contents.
They require checking runtime cwd before writing; all filesystem assertions
remain identical across providers. Claude can use a disposable config via
`TYDE_CONFORMANCE_CLAUDE_FIXTURE_CONFIG` and matching `CLAUDE_CONFIG_DIR`.
That directory must contain `.claude.json` and a `tyde-conformance-fixture`
marker. The shared setup trusts only its newly created fixture roots in that
isolated config; it never changes the user's normal project trust settings.

A later server operation must coordinate project assignment, persistent roots,
project-scoped steering, skills, MCP configuration, and client notifications.
This primitive leaves Tyde's project configuration unchanged. Persist roots
only after provider acknowledgement. A transport failure or mismatched reply
can leave the provider state uncertain; it is not proof of rollback, and must
be reconciled before continuing the conversation.
