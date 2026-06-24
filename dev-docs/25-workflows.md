# Workflows

Tyde Workflows are Markdown-authored coordinator agents. The long-term model is
that users do **not** hand-author a UI form or JSON manifest. A user asks an
agent to "make a workflow that does X"; the agent writes a validated Markdown
file under a known workflow directory; the server discovers it; the Workflows
tab shows it; and the user can run it.

This document is the authoritative design for evolving the current workflow
prototype into that model. The core decisions below are final for this slice.

---

## 1. Goals

- Make agent-authored workflow files the primary authoring path.
- Keep the server as the source of truth for workflow discovery, validation,
  save semantics, run inputs, and errors.
- Keep workflows local-file-native: a workflow is a Markdown file with YAML
  frontmatter in either a project workflow directory or the global workflow
  directory.
- Make workflow catalog updates event-driven. Refresh may request a rescan, but
  must not be the mechanism that keeps the UI from going stale.
- Preserve the existing workflow run model: running a workflow spawns a
  coordinator agent with `AgentOrigin::Workflow` and workflow progress MCP tools.

## 2. Non-goals

- No remote push, PR, release, or tag behavior.
- No delete/rename workflow MCP tools in Phase 1 or Phase 2.
- No human-facing workflow editor in Phase 1. The CTA opens an editable agent
  prompt; the agent writes the file.
- No real-backend tests for this work. `backend.rs` real-AI tests stay opt-in
  and must not be run for this design unless a backend is changed with explicit
  approval.

---

## 3. Current state

The current implementation already has the core run shape. The changes in this
plan are mostly about authoring, validation, automatic catalog updates, inputs,
and UI/error polish.

### 3.1 Protocol model already exists

- `WorkflowId`, `WorkflowRunId`, and `WorkflowStepRunId` are typed protocol
  newtypes (`protocol/src/types.rs:269-292`).
- `AgentOrigin::Workflow` exists and is described as the origin for Tyde
  workflow coordinators and workflow-spawned children
  (`protocol/src/types.rs:423-439`).
- The protocol already has `TriggerWorkflow`, `CancelWorkflow`, and
  `WorkflowRefresh` input frames (`protocol/src/types.rs:563-566`) and
  `WorkflowNotify` / `WorkflowRunNotify` output frames
  (`protocol/src/types.rs:631-632`).
- Workflow summary, source, diagnostic, run, and step protocol types are already
  present (`protocol/src/types.rs:837-918`, `protocol/src/types.rs:920-988`).
- `TriggerWorkflowPayload` already accepts an `inputs: HashMap<String, Value>`,
  but the server does not validate it yet and the frontend currently sends `{}`
  (`protocol/src/types.rs:1001-1008`, `frontend/src/send.rs:284-299`).
- `HostBootstrapPayload` already includes workflow summaries, diagnostics, and
  runs, but not workflow catalog locations (`protocol/src/types.rs:1018-1042`).
- The protocol validator already parses workflow notify/run/trigger/cancel/
  refresh payloads (`protocol/src/validator.rs:275-279`,
  `protocol/src/validator.rs:345-353`).

### 3.2 Discovery and parsing already exist

- Discovery scans the global workflow directory, using
  `$TYDE_GLOBAL_WORKFLOWS_DIR` when set and otherwise `~/.tyde/workflows`
  (`server/src/workflows/registry.rs:87-98`,
  `server/src/workflows/registry.rs:187-198`).
- Discovery scans each project root's `.tyde/workflows` directory
  (`server/src/workflows/registry.rs:92-101`).
- Discovery only loads files with the `.md` extension
  (`server/src/workflows/registry.rs:122-126`).
- `parse_workflow_file` reads the file, splits YAML frontmatter from the body,
  deserializes frontmatter, checks non-empty `id` and `name`, parses triggers,
  and builds a `WorkflowDefinition` (`server/src/workflows/registry.rs:207-237`).
- Duplicate ids in the same catalog key are currently diagnosed as a warning and
  the later file is ignored (`server/src/workflows/registry.rs:133-155`).
- Project workflows currently shadow global workflows with the same id by
  omitting the global summary during `rebuild_summaries`; this is silent today
  (`server/src/workflows/registry.rs:165-184`). Re-reading that code matters:
  the override set is collected from **all** project ids before the single flat
  summary list is built, so one project's override hides the global summary
  everywhere, not only in that project.

### 3.3 Running a workflow already works

- `build_coordinator_prompt` turns the workflow body, run id, summary, declared
  child backends, and JSON inputs into the coordinator prompt
  (`server/src/workflows/runner.rs:6-29`).
- `trigger_workflow` resolves a workflow, creates a `WorkflowRunSnapshot`, emits
  `WorkflowRunNotify`, builds the coordinator prompt, injects the workflow MCP
  server, and spawns the coordinator as `AgentOrigin::Workflow`
  (`server/src/host.rs:5008-5156`).
- The coordinator gets the workflow progress MCP server when that MCP URL is
  available (`server/src/host.rs:5091-5111`).
- The spawned coordinator includes workflow metadata and an initial alias of
  `Workflow: <name>` (`server/src/host.rs:5116-5149`).
- Workflow runs are persisted in `WorkflowRunStore`; running runs are marked
  failed on host restart (`server/src/workflows/store.rs:23-40`), and every
  upsert saves the store (`server/src/workflows/store.rs:53-60`).
- Bootstrap snapshots currently include catalog summaries, diagnostics, and
  stored workflow runs (`server/src/host.rs:632-705`).

### 3.4 Workflow progress MCP already exists

- The workflow-specific MCP surface exposes `tyde_workflow_report_step` and
  `tyde_workflow_finish` (`server/src/workflows/mcp.rs:80-120`).
- The workflow MCP host derives the calling workflow run from the injected
  agent id and tells agents not to invent ids (`server/src/workflows/mcp.rs:123-134`).
- The workflow MCP server is loopback-only and mounted under `/mcp`
  (`server/src/workflows/mcp.rs:137-193`).

### 3.5 Agent-control MCP has the guard pattern we need

- Agent-control MCP tools are ordinary MCP tools on the agent-control surface
  (`server/src/agent_control_mcp.rs:343-500`).
- Mutating agent-control tools reject read-only callers through
  `reject_mutating_tool_for_read_only_caller`, which checks the caller's
  `BackendAccessMode` and emits a clear tool error
  (`server/src/agent_control_mcp.rs:467-475`,
  `server/src/agent_control_mcp.rs:623-629`,
  `server/src/agent_control_mcp.rs:748-760`).
- Phase 1 workflow save must use the same guard. Workflow targets is read-only
  and must not use the mutating guard.

### 3.6 Project file watching has the reusable pattern

- `project_stream.rs` already uses the `notify` crate
  (`server/src/project_stream.rs:8`).
- It creates a `RecommendedWatcher`, watches roots recursively, and forwards
  events into an async channel (`server/src/project_stream.rs:380-398`).
- It debounces filesystem changes before refreshing subscribers
  (`server/src/project_stream.rs:28`, `server/src/project_stream.rs:577-646`).
- It updates watched roots when project roots change
  (`server/src/project_stream.rs:789-802`).

### 3.7 Current frontend is a thin projection, but incomplete

- The Workflows panel renders server-emitted summaries, diagnostics, and runs;
  it has a manual Refresh button today (`frontend/src/components/workflows_panel.rs:321-345`).
- Empty states are minimal today: no host, no workflows, no runs
  (`frontend/src/components/workflows_panel.rs:348-395`).
- Run and cancel failures are currently logged instead of surfaced inline
  (`frontend/src/components/workflows_panel.rs:450-455`,
  `frontend/src/components/workflows_panel.rs:531-534`).
- `WorkflowNotify` replaces the host's workflow summaries and diagnostics in UI
  state (`frontend/src/dispatch.rs:1922-1930`), while bootstrap seeds summaries,
  diagnostics, and runs (`frontend/src/dispatch.rs:4101-4113`).
- Several classes referenced by the panel have no CSS definitions yet, including
  `.empty-state`, `.panel-header`, `.panel-title`, `.panel-subtitle`,
  `.primary-button`, `.run-card`, `.workflow-run-tree`,
  `.workflow-step-title`, and `.workflow-cancel-button`
  (`frontend/src/components/workflows_panel.rs:340-395`,
  `frontend/src/components/workflows_panel.rs:478-568`; compare
  `frontend/styles.css:9794-9966`).

---

## 4. Decisions and rationale

### 4.1 A workflow is a Markdown file

A workflow is a Markdown file with YAML frontmatter in one of these directories:

- Project scope: `<project-root>/.tyde/workflows/*.md`
- Global scope: `$TYDE_GLOBAL_WORKFLOWS_DIR/*.md` when the env var is set and
  non-empty, otherwise `~/.tyde/workflows/*.md`

Required frontmatter:

```yaml
---
id: build-and-test
name: Build and Test
coordinator:
  backend: codex
  access_mode: read_only
---
```

Optional frontmatter:

```yaml
description: Compile, lint, and summarize failures
triggers: [global]
inputs: []
declared_backends: [codex]
tags: [quality, ci]
```

The Markdown body is the coordinator prompt body. Running the workflow spawns a
coordinator agent with `AgentOrigin::Workflow`; the final prompt is the server's
coordinator wrapper plus the body. The coordinator gets the existing
`tyde_workflow_report_step` and `tyde_workflow_finish` MCP tools.

**Rationale:** Markdown is reviewable, diffable, and agent-editable. The server
owns validation and run semantics; the UI only renders the discovered catalog and
run state.

### 4.2 Agents are the primary authors

The primary authoring path is:

1. User asks an agent to create a workflow.
2. The agent calls `tyde_workflow_targets` to discover exact save targets.
3. The agent calls `tyde_workflow_save` with validated Markdown.
4. The server writes the file, reloads the catalog, and emits `WorkflowNotify`.
5. The Workflows tab shows the workflow without manual refresh.

**Rationale:** Users should not need to know the workflow file format to get
value. Agents can author the file, respond to validation diagnostics, and
iterate.

### 4.3 The server validates, saves, watches, and emits

The UI must never guess workflow directories or reconstruct catalog state. The
server sends catalog locations in bootstrap and notify payloads. The server also
watches workflow directories and emits catalog updates on file changes.
Within the server, the host is the single owner of the workflow catalog: save,
manual refresh, project-root changes, and filesystem watcher signals all flow
through one serialized reload-and-notify path.

**Rationale:** This follows the event-driven Tyde model: server events flow to
frontend state and reactive views. Refresh is a rescan request, not a stale-state
crutch.

### 4.4 Save semantics are intentionally strict

`tyde_workflow_save` validates content before writing. It writes exactly one
basename-only `.md` file under a server-approved workflow target. `create` never
overwrites and never creates a same-scope duplicate id. `replace` must identify
the exact file and current id being replaced.

**Rationale:** Agents are powerful, so the server must prevent accidental path
escapes, accidental overwrites, and ambiguous multi-root saves.

---

## 5. Phase 1: Agent-authored workflow files

Phase 1 ships the authoring loop, shared validation, automatic catalog updates,
locations in protocol, and a teaching empty state.

### 5.1 Protocol additions

Add these protocol types in `protocol/src/types.rs` and use them directly from
server, client, validator, generated frontend bindings, and MCP tool schemas.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowCatalogLocation {
    pub scope: WorkflowSourceScope,
    pub directory: String,
    pub exists: bool,
}
```

Add locations to catalog payloads:

```rust
pub struct WorkflowNotifyPayload {
    pub summaries: Vec<WorkflowSummary>,
    pub diagnostics: Vec<WorkflowDiagnostic>,
    #[serde(default)]
    pub locations: Vec<WorkflowCatalogLocation>,
}

pub struct HostBootstrapPayload {
    // existing fields...
    #[serde(default)]
    pub workflow_summaries: Vec<WorkflowSummary>,
    #[serde(default)]
    pub workflow_diagnostics: Vec<WorkflowDiagnostic>,
    #[serde(default)]
    pub workflow_runs: Vec<WorkflowRunSnapshot>,
    #[serde(default)]
    pub workflow_locations: Vec<WorkflowCatalogLocation>,
}
```

Add MCP request/response types for agent-control tools:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowSaveTarget {
    Global,
    Project {
        project_id: ProjectId,
        root: ProjectRootPath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WorkflowSaveMode {
    Create,
    Replace {
        existing_path: String,
        existing_id: WorkflowId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowTargetDirectory {
    pub target: WorkflowSaveTarget,
    pub location: WorkflowCatalogLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowTargetsResponse {
    pub targets: Vec<WorkflowTargetDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSaveRequest {
    pub target: WorkflowSaveTarget,
    pub mode: WorkflowSaveMode,
    pub filename: String,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSaveResponse {
    pub summary: WorkflowSummary,
    pub source: WorkflowSource,
    pub path: String,
    pub created: bool,
    pub diagnostics: Vec<WorkflowDiagnostic>,
}
```

Notes:

- `WorkflowSourceScope` is reused for locations so global/project ownership is
  typed the same way in source paths and writable target directories.
- `WorkflowSaveTarget::Project` requires both `project_id` and exact `root`; the
  server verifies the root belongs to that project. This avoids guessing in
  multi-root projects.
- `WorkflowSaveMode::Replace` carries the current path and id the agent believes
  it is replacing. The server verifies both before writing.
- `WorkflowSaveRequest` is an MCP tool payload, not a client frame in Phase 1.
  Keeping it in protocol still preserves one source of truth for schemas and
  tests.

Update `protocol/src/validator.rs` so `WorkflowNotifyPayload` and
`HostBootstrapPayload` validate the new location fields. Update `client` parse
paths so bootstrap and notify events accept locations.

### 5.2 Agent-control MCP tools

Add two tools to `server/src/agent_control_mcp.rs`.

#### `tyde_workflow_targets`

Read-only. Returns valid target directories for the caller's context.

Behavior:

- Always includes the global target directory.
- If the MCP call has an injected caller agent id and that agent has a
  `project_id`, include one project target for each root of that project.
- If there is no caller agent id, include global and all configured project
  targets. This matches the existing agent-control convention where missing
  caller id is allowed for non-agent clients.
- If the caller has no project id, do not infer a project from workspace roots;
  return only global and explain in diagnostics or a `notes` field that project
  targets require project context.
- Each target includes the exact `WorkflowSaveTarget` the agent should pass back
  to `tyde_workflow_save`, plus the corresponding `WorkflowCatalogLocation`.
- Targets are advisory. A permissive or stale `tyde_workflow_targets` result is
  safe because `tyde_workflow_save` independently re-validates the target,
  project id, root, filename, content, and mode before writing.

#### `tyde_workflow_save`

Mutating. Rejects read-only callers by reusing
`reject_mutating_tool_for_read_only_caller`.

Input: `WorkflowSaveRequest`.

Validation and save behavior:

1. Validate the caller access mode first.
2. Resolve the target directory server-side.
   - `global` resolves to the same global directory discovery uses.
   - `project` resolves only when `project_id` exists and `root` exactly equals
     one of that project's roots.
3. Validate `filename`:
   - basename only;
   - ends with `.md`;
   - no path separators;
   - no `..` component;
   - non-empty after trimming.
4. Parse and validate `markdown` with the shared validator described below.
5. Compute the final path as `target_directory / filename`; never accept a path
   from the filename.
6. Enforce mode:
   - `create`: fail if the path already exists or any workflow with the same id
     exists in the same scope.
   - `replace`: fail unless the computed final path exists, equals
     `existing_path`, the currently parsed workflow at that path has
     `existing_id`, and the replacement Markdown has the same id.
   - `replace` cannot change a workflow id. An id rename is a different workflow
     and requires creating a new file; delete/rename tools are deferred for this
     slice.
7. Write atomically where practical: create parent dir, write a temp file in the
   same directory, flush, then rename.
8. Invoke the host's serialized `reload_workflows_and_notify` path after the
   write. Save does not rebuild the catalog or emit `WorkflowNotify` on its own,
   and it does not wait for the filesystem watcher to notice the write.
9. Return `WorkflowSaveResponse` with the resulting summary, source, path,
   created flag, and any non-fatal diagnostics such as project-shadowing-global
   warnings.

Failure behavior:

- Return an MCP tool error with a structured message. Validation failures are
  not partial successes and must not write a file.
- Same-scope duplicate id is a hard save error.
- Project shadowing a global id is a warning diagnostic, not a hard block.

### 5.3 Shared pure workflow validator

Refactor the current file-bound parser (`parse_workflow_file`,
`server/src/workflows/registry.rs:207-237`) into a pure parser used by both
catalog discovery and save:

```rust
pub(crate) fn parse_workflow_content(
    markdown: &str,
    source: WorkflowSource,
) -> Result<WorkflowDefinition, WorkflowParseError>
```

`parse_workflow_file(path, source)` becomes a thin read-file wrapper around
`parse_workflow_content`.

Validation rules:

- File starts with valid YAML frontmatter delimited by `---`.
- `id` is non-empty after trim and matches
  `^[a-z0-9][a-z0-9_-]{0,63}$`.
- `name` is non-empty after trim.
- Markdown body is non-empty after trim.
- `coordinator.backend` deserializes to `BackendKind`.
- `coordinator.access_mode` deserializes to `BackendAccessMode`; omit means the
  protocol default.
- `triggers` must be known trigger surfaces; unknown trigger kinds fail visibly.
- Input ids are non-empty after trim and unique within the workflow.
- Phase 1 accepts only these input kind strings when present:
  `text`, `multiline_text`, `boolean`, `number`, `select`, `file_path`.
  Unknown input kinds fail visibly.
- Defaults type-check against the input kind:
  - omitted kind or `text`, `multiline_text`, `file_path`, `select`: JSON string;
  - `boolean`: JSON bool;
  - `number`: JSON number.
- Empty `description` is normalized away, matching current behavior.
- Tags are preserved but trimmed for display; empty tags are dropped.

Back-compat and migration:

- These validation rules are intentionally stricter than the current parser.
  Existing on-disk workflows with uppercase ids, ids outside the slug regex,
  unknown `input_type` strings, duplicate input ids, or mismatched defaults
  must not disappear silently.
- Discovery treats newly-strict legacy failures as visible diagnostics and skips
  the invalid workflow until the file is fixed. Prefer
  `WorkflowDiagnosticSeverity::Warning` for these migration failures so users
  understand that an old workflow needs updating; malformed YAML, unreadable
  files, and structurally incomplete workflows remain errors.
- Save has no grandfathering: `tyde_workflow_save` rejects invalid Markdown as a
  hard tool error before writing anything.
- Blast radius: a skipped legacy workflow is absent from the runnable catalog
  and cannot be triggered until fixed, but the Workflows tab shows the file path
  and diagnostic instead of silently dropping it.

Cross-file catalog rules live above the pure parser:

- Same-scope duplicate id: discovery emits an error diagnostic and ignores the
  later file; save is a hard error.
- Project workflow id shadowing a global workflow id is a Phase 1 behavioral
  rework, not just a warning. The current `rebuild_summaries` computes one
  global `project_override_ids` set across all projects, which hides the global
  summary everywhere. Replace that with per-context resolution:
  - the catalog stores all valid global and project definitions;
  - resolving with `project_id=P` prefers P's definition, then the global
    definition;
  - resolving without a project id returns the global definition;
  - `WorkflowNotifyPayload.summaries` includes both the global summary and the
    project summary; the Workflows panel's active-context projection hides the
    global summary only when the active project has a same-id project workflow;
  - other projects, and host/global context, still see and can run the global
    workflow.
- Discovery and save emit a warning diagnostic when a project workflow shadows a
  global id. The shadowing project workflow remains runnable in that project;
  the global workflow remains available outside that project.

### 5.4 Catalog locations

Add a helper that computes locations from the same inputs discovery uses:

```rust
pub(crate) fn workflow_catalog_locations(projects: &[Project])
    -> Vec<WorkflowCatalogLocation>
```

Rules:

- Include one global location.
- Include one project location per project root.
- `directory` is the exact target directory path.
- `exists` is `Path::is_dir()` at the time the snapshot is built.
- `exists` is a TOCTOU hint for teaching and display, not an authorization or
  write precondition. Save re-checks the target and creates missing directories
  as needed.
- Missing directories are not errors. They are useful in the empty state and for
  agents deciding where to save.

Every `WorkflowNotifyPayload` and `HostBootstrapPayload` must include these
locations. The frontend must not construct `~/.tyde/workflows` or
`.tyde/workflows` paths by string convention.

### 5.5 Server-side filesystem watcher

Add a workflow filesystem watcher that reuses the `notify` pattern from
`project_stream.rs`, but do **not** let the watcher own catalog state. The host
is the sole workflow catalog owner. The watcher only observes filesystem changes
and sends `Rescan` signals into the host-owned serialized reload path.

Watched paths:

- global workflow directory;
- each configured project root's `.tyde/workflows` directory.

Rules:

- Watch workflow directories, not entire project roots.
- Use recursive watching for the workflow directory subtree so future nested
  organization can be rejected or supported consistently by the discovery
  filter.
- Filter events to `*.md` paths before triggering a rescan.
- Debounce create/modify/remove/rename bursts.
- On debounce expiry, send a `Rescan` signal to the host catalog owner. The
  watcher must not rebuild the catalog, mutate host catalog state, or emit
  `WorkflowNotify`.
- On watcher errors, send a rescan/error signal to the host catalog owner so the
  next `WorkflowNotify` carries an error diagnostic. Do not leave the UI
  silently stale.
- Project create, project delete, add-root, and delete-root must update watcher
  targets and reload the catalog. Rename/reorder do not change roots, but a
  cheap reload is acceptable if it keeps the code simpler.
- Manual `WorkflowRefresh` remains as an explicit rescan request. It calls the
  same reload path as the watcher and save path.

Implementation shape:

- Introduce a small workflow-watch actor that owns `RecommendedWatcher` and a
  command channel for watcher target updates.
- The watcher actor sends only filesystem notifications such as
  `WorkflowCatalogSignal::Rescan { reason }` to the host. It has no `Save` or
  `Targets` command and never calls discovery directly.
- `tyde_workflow_save`, manual `WorkflowRefresh`, watcher rescans, and project
  root changes all enter the same host-owned serialized
  `reload_workflows_and_notify` path.
- Every `WorkflowNotify` emission flows through that one serialized owner. This
  avoids stale-then-fresh races where an immediate save path and a debounced
  watcher path rebuild from different snapshots and emit out of order.
- If the watcher later observes a write that `tyde_workflow_save` already
  reloaded, it enqueues another `Rescan`. The same host owner serializes it; a
  duplicate notify is acceptable, but it must be produced by the same reload path
  and must never regress to an older catalog snapshot.

### 5.6 Host integration

Update host startup and host state around one catalog owner:

- At host startup, build `WorkflowCatalog` and `workflow_locations` from the
  current project store.
- Start the workflow watcher after project store load, but keep the watcher
  stateless with respect to catalog contents.
- Replace ad hoc refresh logic with one serialized
  `reload_workflows_and_notify(reason)` helper/actor turn. It:
  1. reads the current project store;
  2. discovers workflows;
  3. applies same-scope duplicate and per-project shadowing diagnostics;
  4. recomputes locations;
  5. swaps the host-owned catalog and locations together;
  6. emits exactly one `WorkflowNotifyPayload` for that completed reload.
- Manual refresh, watcher rescans, save success, and project-root changes all
  call or enqueue this same helper. No other path may emit `WorkflowNotify`.
- `trigger_workflow` resolves against the host-owned catalog after any awaited
  save/refresh reload has completed. Do not add a second catalog cache in the
  watcher or MCP layer.
- Add host methods used by MCP:
  - `workflow_targets_for_agent(caller_agent_id: Option<&AgentId>)`
  - `workflow_save_from_agent(caller_agent_id: Option<&AgentId>, request)`
- `workflow_save_from_agent` writes the file, then awaits/enqueues
  `reload_workflows_and_notify("workflow_save")` through the same serialized
  owner before returning the response summary. The response summary is read from
  the post-reload host catalog, not from a separate save-local parse, so the MCP
  response and `WorkflowNotify` describe the same catalog snapshot.

### 5.7 Frontend empty-state teaching

Phase 1 frontend work is intentionally narrow.

- Store `workflow_locations` per host in `AppState` from bootstrap and
  `WorkflowNotify`.
- Rework the catalog list derivation for scoped shadowing. `WorkflowNotify` can
  contain a global and project summary with the same workflow id, so the panel
  must first derive the active-context effective summaries and then key rows by a
  stable source-aware key such as `(workflow_id, source.path)` or
  `(workflow_id, source.scope)`, not by `workflow_id` alone.
- Relabel the panel subtitle to explain the authoring model, for example:
  `Ask an agent to author Markdown workflows; Tyde discovers and runs them.`
- When no workflows are visible for the active context, render an empty state
  that explains:
  - workflows are Markdown files with YAML frontmatter;
  - agents should write them;
  - available global/project directories, using server-sent locations;
  - manual Refresh is only a rescan request.
- Add CTA: `Ask an agent to create a workflow`.
- Add a small frontend helper, for example
  `open_new_chat_with_prefill(host_id, project_id, prompt)`, that opens the
  normal new-chat draft and sets `chat_input` in one state transition. Today the
  pieces exist separately (`open_tab()` and `chat_input.set()`); the CTA needs a
  single helper so the active draft and prefill cannot get out of sync.
- The CTA uses the same backend choice as an ordinary new chat in that host and
  project context: no agent is spawned until the user sends, and backend
  selection remains the existing default/draft-backend path. The CTA must not
  invent a special workflow-authoring backend.
- The CTA prompt is built from server-sent `workflow_locations`, not hardcoded
  paths. When there is an active project with project locations, list those
  first as the preferred target and list the global location as optional. Without
  an active project, present the global target first.
- The CTA opens a new chat tab and pre-fills the editable composer with a prompt
  like:

  ```text
  Create a Tyde workflow that ...

  Preferred workflow target:
  - Project: /repo/.tyde/workflows

  Optional global target:
  - Global: /Users/me/.tyde/workflows

  Use tyde_workflow_targets first, then save it with tyde_workflow_save.
  ```

- The prompt must remain editable. Do not auto-send.
- The CTA should prefer project context when there is an active project; global
  remains available in the prompt as an option.
- Phase 1 must ship enough CSS for this empty state to look intentional. Define
  minimal `.empty-state`, `.empty-state.small`, `.panel-header`, `.panel-title`,
  `.panel-subtitle`, and `.primary-button` styles in Phase 1. Detailed catalog
  and run-card polish remains Phase 2, but the Phase 1 CTA must not depend on
  undefined classes.

### 5.8 Phase 1 testing

Follow `tests/TESTING.md`: client-level end-to-end tests through client →
server → mock backend, asserting observable protocol events and MCP tool
results. Extend `tests/tests/workflows.rs`; do not add real-AI backend tests.

E2E tests:

1. **Agent save flow with serialized notify**
   - Create a fixture and project.
   - Spawn or use a mock agent with agent-control MCP access.
   - Call `tyde_workflow_targets` and assert global plus project-root targets.
   - Call `tyde_workflow_save` with `mode=create`.
   - Assert the returned summary/source/path/created flag.
   - Assert a `WorkflowNotify` arrives without `WorkflowRefresh` and includes
     the saved workflow.
   - If the watcher also observes the write, any subsequent `WorkflowNotify`
     must still include the saved workflow; no stale empty/old catalog notify may
     appear after the save notify.

2. **Read-only save rejection**
   - Spawn a read-only agent.
   - Call `tyde_workflow_save`.
   - Assert MCP error contains the read-only mutating-tool rejection.
   - Assert no file was written and no catalog update contains the workflow.

3. **Collision and replace semantics**
   - `create` fails when the filename already exists.
   - `create` fails when the same-scope id exists at a different filename.
   - project save with an id matching a global workflow succeeds with a warning
     diagnostic.
   - `replace` fails when `existing_path` does not match the computed path.
   - `replace` fails when `existing_id` does not match the current file id.
   - `replace` fails when the replacement Markdown changes the workflow id.
   - `replace` succeeds when path and id match.

4. **Scoped project shadowing**
   - Create a global workflow and a project workflow with the same id in project
     A.
   - Assert project A resolves/runs the project workflow and receives a warning
     diagnostic.
   - Assert project B and host/global context still see and resolve the global
     workflow. This guards the Phase 1 behavioral rework from the current
     all-project override set.

5. **Shared validator behavior**
   - Invalid YAML frontmatter fails.
   - Invalid slug id fails.
   - Empty body fails.
   - Unknown trigger fails.
   - Unknown input kind fails.
   - Duplicate input ids fail.
   - Default type mismatch fails.

6. **Legacy strictness diagnostics**
   - Put an existing-style workflow on disk with an uppercase id or unknown
     `input_type`.
   - Assert discovery emits a visible warning diagnostic with the source path and
     skips the workflow instead of silently dropping it.
   - Assert `tyde_workflow_save` rejects the same content as a hard tool error.

7. **Watcher auto-update**
   - Write a workflow file directly under `.tyde/workflows`.
   - Assert `WorkflowNotify` arrives without manual refresh.
   - Modify it and assert the summary changes.
   - Remove or rename it and assert the catalog updates.

8. **Project root watcher target updates**
   - Add a project root with a workflow directory and assert the host emits a
     `WorkflowNotify` containing the workflow after watcher targets are updated
     and the serialized reload runs.
   - Delete the root and assert its workflow disappears from the catalog.

9. **Bootstrap includes locations**
   - Connect a second client and assert `HostBootstrapPayload.workflow_locations`
     includes the global directory and each project workflow directory with
     accurate `exists` values.

Wasm/component tests:

- Empty Workflows panel renders the teaching text and server-provided locations.
- CTA opens a new chat tab and pre-fills `chat_input` with an editable workflow
  creation prompt built from `workflow_locations`, without sending it.
- CTA uses the normal new-chat draft backend path and does not spawn an agent
  until the user sends.
- Phase 1 baseline CSS classes exist for the empty state and CTA.
- Existing workflows suppress the teaching empty state.
- Same-id global/project summaries render the active-context effective row with a
  source-aware key, without duplicate-key stale rendering.

Do not run `tests/tests/backend.rs` real-AI tests for this work.

---

## 6. Phase 2: Inputs, errors, and visual polish

Phase 2 completes the user-facing run path after agent-authored workflows exist.

### 6.1 Typed inputs end-to-end

Replace the stringly typed input field:

```rust
pub input_type: Option<String>
```

with typed controls:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInputControl {
    #[default]
    Text,
    MultilineText,
    Boolean,
    Number,
    Select,
    FilePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowInputOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowInputSpec {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub control: WorkflowInputControl,
    #[serde(default)]
    pub options: Vec<WorkflowInputOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}
```

`WorkflowInputControl::Text` is the default for omitted controls. `Select` uses
`options`; empty options is invalid for `Select`.

Server trigger validation before creating a run:

- Resolve workflow first.
- Reject unknown input keys with `CommandErrorCode::InvalidInput`.
- Apply defaults for missing keys.
- Reject missing required inputs after defaults.
- Type-check every effective input:
  - `Text`, `MultilineText`, `FilePath`: string;
  - `Boolean`: bool;
  - `Number`: JSON number;
  - `Select`: string matching an option value.
- `FilePath` is validated as a string only in Phase 2. It is not an existence
  check, sandbox grant, path canonicalization, or filesystem authorization.
- Store the effective input map in `WorkflowRunSnapshot.inputs`.
- Pass the effective input map to `build_coordinator_prompt`.
- Do not spawn a coordinator when validation fails.

Frontend run flow:

- If `summary.inputs` is empty, Run behaves as today.
- If `summary.inputs` is non-empty, Run opens a modal.
- The modal renders controls from `WorkflowInputControl`, pre-fills defaults,
  marks required fields, and validates locally for responsiveness.
- Submit sends the typed JSON map to the server.
- Command-palette workflow runs route through the same modal.
- `send::trigger_workflow` accepts an `inputs: HashMap<String, Value>` argument
  and stops hardcoding `{}`.

### 6.2 Error surfacing

Current workflow UI event handlers log run/refresh/cancel failures. Phase 2
surfaces typed errors inline.

Server:

- Keep using `AppError` → `CommandErrorPayload` for client command failures; the
  existing conversion already maps invalid input, not found, conflict, internal,
  and protocol violations to typed `CommandErrorCode` values
  (`server/src/error.rs:73-95`).
- `trigger_workflow` input validation must return `AppError::invalid` so the
  client receives `CommandErrorCode::InvalidInput`.
- `workflow_refresh`, `trigger_workflow`, and `cancel_workflow` failures must
  all propagate instead of being swallowed.
- `tyde_workflow_save` failures are returned as MCP tool errors with the same
  code/message discipline. If a future UI invokes save directly, it must use the
  same typed error shape.

Frontend:

- Add workflow-panel error state keyed by host and operation/request kind.
- In dispatch, when `CommandErrorPayload.request_kind` is one of
  `WorkflowRefresh`, `TriggerWorkflow`, or `CancelWorkflow`, write the message
  into workflow-panel error state.
- Render workflow command failures as a panel-level banner, not on a specific
  catalog card. `CommandErrorPayload` currently carries `stream`,
  `request_kind`, `operation`, `code`, `message`, and `fatal`, but no
  `workflow_id` (`protocol/src/types.rs:3780-3787`), so a trigger failure cannot
  be attributed to a specific card without adding new protocol fields. Do not
  infer card attribution from local UI state.
- Clear workflow-panel errors on the next successful `WorkflowNotify` or
  `WorkflowRunNotify` for that host, depending on the failed operation.
- Do not rely on `log::error!` as user-visible feedback.

### 6.3 Visual polish

Phase 2 builds on the minimal Phase 1 empty-state/button/header CSS. Do not
leave `.empty-state`, `.panel-header`, `.panel-title`, `.panel-subtitle`, or
`.primary-button` undefined until Phase 2; those baseline definitions shipped in
Phase 1. Phase 2 adds or refines the classes needed for the polished catalog and
run states:

- `.workflow-card`, `.catalog-card`, `.run-card`
- `.workflow-run-tree`, `.workflow-step-tree`, `.workflow-step-title`
- `.workflow-cancel-button`, `.workflow-run-button`
- `.workflow-tag-pill`
- `.workflow-trigger-line`
- `.workflow-input-chip`, especially `Needs N inputs`
- `.workflow-source-actions`
- active-run treatment for running cards

Catalog cards render:

- name, description, source, coordinator backend/access mode;
- tags as pills;
- trigger line;
- declared child backends;
- `Needs N inputs` chip when inputs are non-empty;
- source actions:
  - View source opens the workflow Markdown file in the existing project file
    view when project-scoped; global source can use host browse/open-source
    behavior if available, otherwise show the path.
  - Edit with agent opens a new chat with a pre-filled editable prompt asking an
    agent to update the exact workflow file. Do not auto-send.

Run cards render:

- clear active-running treatment;
- coordinator and child agent links;
- progress tree with readable step titles and statuses;
- cancel action only while running;
- run summary/error after completion.

### 6.4 Phase 2 testing

E2E tests in `tests/tests/workflows.rs`:

1. **Input validation rejects bad runs**
   - Missing required input yields `CommandErrorCode::InvalidInput`.
   - Unknown input key yields `InvalidInput`.
   - Wrong JSON type yields `InvalidInput`.
   - Invalid select value yields `InvalidInput`.
   - No `WorkflowRunNotify` with a new running run is emitted for rejected
     triggers.

2. **Defaults and effective inputs**
   - Trigger with omitted optional/defaulted values.
   - Assert the run snapshot contains defaults.
   - Assert the coordinator prompt received by the mock backend contains the
     effective JSON inputs.

3. **Inputs modal route**
   - Component test: workflow with inputs opens the modal.
   - Required validation blocks submit.
   - Submit calls `send::trigger_workflow` with the typed inputs.
   - Command-palette path opens the same modal.

4. **Workflow command errors render in the panel banner**
   - Cause a trigger validation error.
   - Assert the panel-level workflow error banner renders the error.
   - Assert the error is not attributed to a specific workflow card because the
     protocol does not carry `workflow_id`.
   - Emit/receive the next successful workflow notify/run notify.
   - Assert the banner clears.

5. **Visual state smoke tests**
   - Component tests assert tags, triggers, source actions, input chip, running
     card treatment, and cancel button visibility.

Again, do not run `tests/tests/backend.rs` real-AI tests for this work.

---

## 7. Implementation order

1. Add protocol types and validator/client support for locations and MCP
   request/response structs.
2. Refactor workflow parsing into `parse_workflow_content` and strengthen
   validation.
3. Rework catalog summaries/resolution so project shadowing is scoped to the
   same project id instead of using one all-project override set.
4. Add catalog location computation.
5. Add the host-owned serialized `reload_workflows_and_notify` path and route
   every `WorkflowNotify` emission through it.
6. Add `tyde_workflow_targets` and `tyde_workflow_save` on agent-control MCP,
   with read-only save rejection.
7. Wire save success, manual refresh, project-root changes, and watcher rescans
   into the serialized reload path.
8. Add workflow filesystem watcher and project-root target updates.
9. Update frontend state and dispatch for `workflow_locations`.
10. Ship the Phase 1 empty state, CTA helper, and minimal CSS.
11. Add Phase 1 E2E and wasm/component tests.
12. Phase 2: typed inputs protocol and server validation.
13. Phase 2: inputs modal and `send::trigger_workflow` input map.
14. Phase 2: panel-level error surfacing.
15. Phase 2: catalog/run card visual polish.
16. Add Phase 2 E2E and wasm/component tests.

---

## 8. Open risks and deferred work

### 8.1 Watcher overhead

Watching only workflow directories should keep overhead low, but hosts with many
projects and many roots can still produce many watcher registrations. If this is
measured as a problem, the fallback is not a UI Refresh crutch; it is a server
watch strategy change, such as one parent watcher per project root filtered to
`.tyde/workflows` paths.

### 8.2 Directory existence and create races

Workflow directories may not exist until an agent saves the first workflow. The
server must treat missing directories as normal locations with `exists=false`.
That flag is a TOCTOU hint only: a directory can appear or disappear after the
payload is emitted. Saves create the target directory after re-validating the
target. Watcher setup must tolerate missing dirs and start watching them after
creation or after the next project/refresh rescan.

### 8.3 Duplicate watcher notifications

A successful save enters the same serialized `reload_workflows_and_notify` path
as every other rescan and returns after that path produces the authoritative
notify. The filesystem watcher may observe the same write and enqueue another
rescan after debounce. This is allowed as long as the duplicate notify is
produced by the same host-owned reload path and never regresses to an older
catalog snapshot.

### 8.4 Delete and rename tools are deferred

Do not add `tyde_workflow_delete` or `tyde_workflow_rename` in Phase 1 or Phase
2. Agents can replace content safely, but `replace` cannot change a workflow's
id. If an agent needs a different id through the Phase 1 tools, it must create a
new workflow file; the old file remains because there is intentionally no
delete/rename tool yet. Dedicated delete/rename tools need their own
confirmation and collision semantics and should be designed later.

### 8.5 Source viewing for global workflows

Project-scoped source viewing can reuse project file views. Global workflows do
not necessarily belong to a project root. Phase 2 may initially show the global
source path and route edits through an agent prompt rather than adding a new
host-file viewer.
