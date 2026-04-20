# Generic Customized Agents

This document specifies host-owned custom agent definitions and the related
customization domains they reference in Tyde2. It builds on
`01-philosophy.md`, `03-agents.md`, `05-session-resume.md`, `06-projects.md`,
`09-host-settings.md`, and `11-agent-control-mcp.md`.

---

## 1. Overview

A **custom agent** is a reusable host-owned agent definition that packages
backend-agnostic agent instructions, referenced skills, referenced MCP servers,
and tool policy into one typed record selected at spawn
time. Steering is a separate domain: it is not part of a custom agent
selection, it is universal guidance owned by the server and applied to every
spawn for that host, plus any project-scoped steering for the selected
`project_id`. This intentionally replaces the old Tyde mix of JSON files,
directory globs, and frontend-owned interpretation with explicit protocol types,
server stores, and replayed host events.

---

## 2. Data Model

All persisted customization state is added in `protocol/src/types.rs` and used
end-to-end. Do not add parallel server-only mirror structs for these domains.

### 2.1 IDs and scopes

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CustomAgentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SteeringId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpServerId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringScope {
    Host,
    Project(ProjectId),
}
```

`SkillId` is the canonical skill slug and the on-disk directory name under
`~/.tyde/skills/`.

### 2.2 Custom agents

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomAgent {
    pub id: CustomAgentId,
    pub name: String,
    pub description: String,
    pub instructions: Option<String>,
    pub skill_ids: Vec<SkillId>,
    pub mcp_server_ids: Vec<McpServerId>,
    pub tool_policy: ToolPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPolicy {
    Unrestricted,
    AllowList { tools: Vec<String> },
    DenyList { tools: Vec<String> },
}
```

Rules:

- custom agents are host-owned only in v1; there is no `scope` field
- every custom agent is available for every backend
- `skill_ids` and `mcp_server_ids` must resolve exactly at upsert time

### 2.3 Steering

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steering {
    pub id: SteeringId,
    pub name: String,
    pub scope: SteeringScope,
    pub content: String,
}
```

This is server-owned state, not a filesystem glob. Project-scoped steering
stores the owning `ProjectId` directly in the protocol type.

### 2.4 Skills

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub description: Option<String>,
}
```

`Skill` only carries metadata. The body lives on disk at
`~/.tyde/skills/{skill_id}/SKILL.md`, alongside
`~/.tyde/skills/{skill_id}/metadata.json`.

### 2.5 MCP servers

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: McpServerId,
    pub name: String,
    pub transport: McpTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Http {
        url: String,
        headers: HashMap<String, String>,
        bearer_token_env_var: Option<String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}
```

`name` is the backend-visible MCP server name. The host rejects blank names,
duplicate names, and reserved names (`tyde-debug`, `tyde-agent-control`) at
write time.

### 2.6 Spawn metadata

```rust
pub struct SpawnAgentPayload {
    pub name: Option<String>,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub params: SpawnAgentParams,
}

pub struct AgentStartPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub created_at_ms: u64,
}

pub struct NewAgentPayload {
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
}
```

`custom_agent_id` is explicit so the UI can render the selected custom agent
without inferring from session history or backend state.

---

## 3. Persistence

Persist these host-owned domains beside the existing settings, project, and
session stores:

- `~/.tyde/custom_agents.json`
- `~/.tyde/mcp_servers.json`
- `~/.tyde/steering.json`
- `~/.tyde/skills/{name}/SKILL.md`
- `~/.tyde/skills/{name}/metadata.json`

The JSON-backed stores follow the existing pattern:

- full-file load
- full-file validation
- atomic replace on write
- loud failure on invalid persisted state

`steering.json` stores the `scope` field directly, including
`SteeringScope::Project(ProjectId)`. Skills are slightly different: the skill
store is a thin filesystem-backed index over `metadata.json` plus presence of
`SKILL.md`; the markdown body itself is read from disk only when the backend
materializer needs it.

---

## 4. Protocol Events

All customization mutations are host-owned and travel on `/host/<uuid>`.

### 4.1 Input events

Add `FrameKind` variants:

```rust
CustomAgentUpsert
CustomAgentDelete
SteeringUpsert
SteeringDelete
SkillRefresh
McpServerUpsert
McpServerDelete
```

Payload shape is concrete per domain, for example:

```rust
pub struct CustomAgentUpsertPayload {
    pub custom_agent: CustomAgent,
}

pub struct CustomAgentDeletePayload {
    pub id: CustomAgentId,
}

pub struct SteeringUpsertPayload {
    pub steering: Steering,
}

pub struct SteeringDeletePayload {
    pub id: SteeringId,
}

pub struct SkillRefreshPayload {}

pub struct McpServerUpsertPayload {
    pub mcp_server: McpServerConfig,
}

pub struct McpServerDeletePayload {
    pub id: McpServerId,
}
```

Validation happens in the host actor before persistence:

- missing referenced `SkillId` / `McpServerId` rejects the custom-agent upsert
- `SteeringScope::Project(project_id)` rejects if the project does not exist
- blank content, blank names, reserved MCP names, and duplicate MCP names reject
  immediately

There are no best-effort repairs.

### 4.2 Output events

Add `FrameKind` variants:

```rust
CustomAgentNotify
SteeringNotify
SkillNotify
McpServerNotify
```

Each domain gets one replay/live event enum with the same shape:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomAgentNotifyPayload {
    Upsert { custom_agent: CustomAgent },
    Delete { id: CustomAgentId },
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SteeringNotifyPayload {
    Upsert { steering: Steering },
    Delete { id: SteeringId },
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillNotifyPayload {
    Upsert { skill: Skill },
    Delete { id: SkillId },
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpServerNotifyPayload {
    Upsert { mcp_server: McpServerConfig },
    Delete { id: McpServerId },
}
```

`SkillRefresh` is the only input that does not mutate from a payload body. It
causes the server to rescan `~/.tyde/skills/` and fan out the canonical skill
set as `SkillNotify` upserts/deletes. This is an explicit event path for
out-of-band disk edits, not a frontend cache refresh.

One important limit: if a skill is deleted out-of-band after the last
`SkillRefresh`, the server only discovers that missing `SKILL.md` / metadata
when the next spawn tries to materialize that exact `SkillId`. Spawn must fail
loudly at that point; there is no hidden background watcher.

### 4.3 Replay ordering

When a host stream registers, the new replay order is:

1. `HostSettings`
2. existing host prelude (`SessionSchemas`, `BackendSetup`)
3. `ProjectNotify`
4. `McpServerNotify`
5. `SkillNotify`
6. `SteeringNotify`
7. `CustomAgentNotify`
8. existing agents (`NewAgent`, then their agent streams)

The relative order from `ProjectNotify` through `CustomAgentNotify` is required.
`SteeringScope::Project(ProjectId)` depends on projects, `CustomAgent`
references skills and MCP servers by ID, and agents carry `custom_agent_id`.

---

## 5. Spawn-Time Resolution

Resolve customization once, on the server, before a backend session starts. A
good home is `server/src/agent/customization.rs`, called from `host.rs` before
backend spawn. The important rule is that resolution is server-owned and typed,
not reconstructed in the frontend.

At spawn:

1. Start with the built-in startup MCP set from the existing
   `startup_mcp_servers_for_settings()` logic in `host.rs`. That keeps
   `tyde-debug` and `tyde-agent-control` server-owned.
2. If `custom_agent_id` is set, resolve that exact `CustomAgentId`. Missing ID
   is a spawn error. Then union the custom agent's referenced MCP servers into a
   name-keyed map. Reserved names remain reserved; any collision with a reserved
   name is a spawn error.
3. Collect steering in two exact buckets:
   `SteeringScope::Host` plus `SteeringScope::Project(project_id)` when the
   spawn has a `project_id`. Sort by `title` and join with one blank line between
   entries.
4. Collect the custom agent's referenced skills. Missing `SkillId`,
   missing `metadata.json`, or missing `SKILL.md` is a spawn error. Materialize
   the resolved skills per backend.

Important separation:

- custom-agent `instructions` are the selected agent identity
- steering is universal host/project guidance applied to every spawn

Some backends ultimately merge those into one prompt surface, but the resolver
keeps them separate so selection, replay, and resume stay explicit.

---

## 6. Per-Backend Translation

The backend adapter translates the resolved customization into that backend's
native startup surface.

| Backend | Instructions | Steering | Skills | MCP |
|---|---|---|---|---|
| Claude | `.claude/agents/{id}.md` + `--agent {id}` | `--append-system-prompt` | `.claude/skills/{name}/SKILL.md` via `--add-dir` | temp JSON via `--mcp-config` |
| Codex | prepend to `model_instructions_file` | same file | `.agents/skills/tyde-{name}/` symlink | `-c mcp_servers.{name}.command=...` / `url=...` |
| Kiro | ACP `systemPrompt` | ACP `systemPrompt` append | `.kiro/skills/tyde-{name}/` | ACP `mcpServers` |
| Gemini | prepended to prompt | same prompt surface | `.gemini/skills/tyde-{name}/` | `.gemini/settings.json` |
| Tycode | one temp workspace root containing `.tycode/tyde_steering.md`, appended to `workspace_roots` | same root | same temp workspace root containing `.tycode/skills/` | `--mcp-servers <json>` |

Notes:

- For Claude, keep instructions and steering distinct because Claude has a real
  first-class agent identity surface.
- For Codex, Kiro, Gemini, and Tycode, the adapter may concatenate custom-agent
  instructions plus steering in that order, but only after the resolver has
  kept them separate.
- Tycode uses exactly one synthesized extra workspace root per spawn. That root
  contains both the steering file and the materialized skills tree; do not
  append one root for steering and another for skills.
- Tycode should use the same extra-workspace-root pattern the old app already
  used for steering and skill injection. There is no new frontend logic here.

---

## 7. Tool Policy

Tool policy is declared on `CustomAgent.tool_policy`.

- Claude: translate directly to Claude's native allow/deny tool surface.
- Codex, Kiro, Gemini, Tycode: reject the spawn if the selected custom agent
  declares `AllowList` or `DenyList`.

This must fail fast. Do not silently drop tool restrictions on backends that do
not support them.

---

## 8. Settings UI

`frontend/src/components/settings_panel.rs` gets four new tabs:

- Custom Agents
- MCP Servers
- Steering
- Skills

Behavior:

- Custom Agents: list + editor for `CustomAgent`
- MCP Servers: list + transport editor for `McpServerConfig`
- Steering: list + markdown editor for `Steering`, including a scope badge
- Skills: list only; editing opens the on-disk `SKILL.md`, then the UI dispatches
  `SkillRefresh` to resync from the server

All frontend state comes from dedicated signals updated only by the notify
events above. No frontend-side registry, no cached mirrors, no direct file
parsing in the UI.

---

## 9. New-Chat Dropdown UX

Enabled backends still come from `HostSettings`. The new-chat split button
becomes a nested flyout:

- first level: enabled backends
- second level per backend:
  `Default agent` first, then all host-scoped custom agents

Selection sends `SpawnAgent` with `custom_agent_id` set for the selected custom
agent or `None` for `Default agent`. No extra server query is needed because
`state.custom_agents` is already populated reactively from replay and live
notify events.

---

## 10. Scope (v1)

v1 scope is intentionally narrow:

- Custom agents: host-only
- MCP servers: host-only
- Skills: host-only
- Steering: host and project scope

Project-scoped steering is included now because project-aware universal guidance
is the main reason to split steering into its own domain.

---

## 11. Session Resume

Persist `custom_agent_id` on `SessionRecord`:

```rust
pub struct SessionRecord {
    // existing fields...
    pub custom_agent_id: Option<CustomAgentId>,
}
```

Resume rules:

- new sessions inherit `custom_agent_id` from the spawn payload
- resumed sessions re-resolve the current custom-agent definition from the
  stored `SessionRecord.custom_agent_id`
- if `SpawnAgentPayload.custom_agent_id` is present on a resume, it must match the
  stored value exactly; mismatches are rejected
- if the referenced custom agent no longer exists, the server emits a non-fatal
  `AgentError` on the agent stream and resumes the session without that custom
  agent's instructions, skills, MCP servers, or tool policy

That deletion rule is explicit and visible. There is no heuristic "best match"
lookup and no silent degradation.

---

## 12. Implementation Split

### Backend / Codex

- protocol types in `protocol/src/types.rs`
- four store modules under `server/src/store/`
- host-actor mutation handling and replay fanout on `/host/<uuid>`
- spawn-time resolver in `server/src/agent/` or equivalent
- per-backend translation in `server/src/backend/*.rs`
- coverage for store validation, replay ordering, resolver failures, backend
  translation, and resume semantics

### Frontend / Claude

- new state slices in `frontend/src/state.rs`
- notify dispatch wiring in `frontend/src/dispatch.rs`
- four new settings tabs in `settings_panel.rs`
- nested new-chat flyout using already-replayed host state

---

## 13. Out Of Scope For v1

- project-scoped custom agents
- project-scoped MCP servers
- project-scoped skills
- per-backend session-setting overrides in the custom-agent editor
- remote-host skill materialization over SSH before the proper remote-host
  design lands

---

## 14. Open Questions

None.
