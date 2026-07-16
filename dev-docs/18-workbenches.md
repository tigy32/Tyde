# Workbenches

This document specifies workbench modeling in Tyde2.

It builds on:

- `01-philosophy.md` for the architecture constraints
- `02-protocol.md` for framing and stream rules
- `06-projects.md` for project identity, replay, and deletion rules
- `07-project-stream.md` for the per-project file/git event model
- `12-remote-hosts.md` for the host-as-connection invariant

---

## 1. Goals

A **workbench** is a git worktree, owned by Tyde, that lets a user run agents on
a separate branch of a project in parallel with the parent project. It appears
in the rail nested under its parent and behaves like a full project everywhere
else: it has its own file browser, git stream, agents, sessions, and terminals.

The non-negotiable claims of this design:

- A workbench **is a `Project`**. The discriminator is encoded as a typed enum
  variant on `Project.source`. There is no separate `Workbench` type, no
  parallel store, no parallel event channel.
- The server is the only thing that runs `git worktree add`/`remove`. The
  frontend never reasons about worktrees, paths, or branches — it sends typed
  user-intent events and renders typed state events.
- Workbench upserts and deletes flow through the existing `ProjectNotify`
  event. There is no `WorkbenchNotify`.
- Workbenches live on the host that owns their parent project. There is no
  cross-host workbench. Routing falls out of "connect to host A → its
  `ProjectNotify::Upsert` events include workbench projects."
- Invalid states are unrepresentable: a workbench cannot exist without its
  parent, a parent cannot be deleted while it has live workbenches, and a
  workbench cannot be deleted while sessions, agents, terminals, or
  project-scoped steering reference it.

This is one new variant on `Project.source`, two new frame kinds, and a small
set of preconditions on existing project handlers.

---

## 2. Data Model

The single canonical source remains `protocol/src/types.rs`.

### 2.1 New typed wrappers

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectRootPath(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitBranchName(pub String);
```

`ProjectRootPath` replaces bare `String` in all root-bearing project payloads.
`GitBranchName` is the wire-level type for branch names. Both are validated at
the protocol boundary (see §6.1).

### 2.2 `Project` and `ProjectSource`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    #[serde(default)]
    pub sort_order: u64,
    pub source: ProjectSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectSource {
    Standalone {
        roots: Vec<ProjectRootPath>,
    },
    GitWorkbench {
        parent_project_id: ProjectId,
        branch: GitBranchName,
        roots: Vec<WorkbenchRoot>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchRoot {
    pub parent_root: ProjectRootPath,
    pub worktree_root: ProjectRootPath,
}
```

This replaces the previous `Project { roots: Vec<String> }` shape. The enum
variant is the type-system question "is this a workbench"; the protocol cannot
represent "workbench without parent," "parent reference without branch," or
"standalone with workbench metadata."

### 2.3 Convenience accessor

The frontend, project stream, agents, terminals, and any other consumer that
needs root paths must use:

```rust
impl Project {
    pub fn root_paths(&self) -> Vec<ProjectRootPath> {
        match &self.source {
            ProjectSource::Standalone { roots } => roots.clone(),
            ProjectSource::GitWorkbench { roots, .. } => {
                roots.iter().map(|r| r.worktree_root.clone()).collect()
            }
        }
    }

    pub fn parent_project_id(&self) -> Option<&ProjectId> {
        match &self.source {
            ProjectSource::Standalone { .. } => None,
            ProjectSource::GitWorkbench { parent_project_id, .. } => {
                Some(parent_project_id)
            }
        }
    }

    pub fn is_workbench(&self) -> bool {
        matches!(self.source, ProjectSource::GitWorkbench { .. })
    }
}
```

No call site outside `protocol/src/types.rs` may match on `ProjectSource` to
extract root paths. They go through `root_paths()`. This keeps the source
distinction local to the type and prevents the implicit duplication that would
otherwise grow at every call site.

### 2.4 Rules

- Standalone projects always have at least one root.
- Workbench projects have one `WorkbenchRoot` per parent root (see §6.1).
- Workbench `WorkbenchRoot.parent_root` must equal an entry in the parent's
  `Standalone { roots }` at all times.
- `branch` is preserved verbatim as the user typed it. The path is computed
  separately (see §5.1).
- `Project.id` is server-generated (UUID).
- `Project.name` defaults to the branch text on workbench creation; users may
  rename it through `ProjectRename`.

---

## 3. Protocol Additions

### 3.1 New frame kinds

```rust
pub enum FrameKind {
    // ...existing...
    WorkbenchCreate,
    WorkbenchRemove,
}
```

There is **no** `WorkbenchNotify`. There is **no** `WorkbenchRename`. There is
**no** per-operation `/workbench/<uuid>` stream.

Workbench upserts and deletes flow through `ProjectNotify::{Upsert, Delete}`.
Renames reuse `ProjectRename` (metadata only — see §6.4). Operation success
is observable as the resulting `ProjectNotify` event; failures are
`CommandError` on the host stream.

### 3.2 Input payloads

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchCreatePayload {
    pub parent_project_id: ProjectId,
    pub branch: GitBranchName,
    /// Display name for the project record. Frontend defaults to `branch.0`.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchRemovePayload {
    pub id: ProjectId,
}
```

Both are sent on the host stream, matching the existing
`ProjectCreate` / `ProjectDelete` pattern.

### 3.3 Output events

`WorkbenchCreate` emits:

- `ProjectNotify::Upsert { project }` on success — fanned out to every host
  subscriber on this host.
- `CommandError { request_kind: WorkbenchCreate, code, message, fatal: false }`
  on failure.

`WorkbenchRemove` emits:

- `ProjectNotify::Delete { project }` on success — fanned out.
- `CommandError { request_kind: WorkbenchRemove, code, message, fatal: false }`
  on failure.

The requesting client correlates "my create" by `(parent_project_id, branch)` —
the next `ProjectNotify::Upsert` whose source is `GitWorkbench` and whose
`(parent_project_id, branch)` matches the request is the one to switch to. No
request IDs are needed; the matching happens against typed protocol data.

### 3.4 Existing payloads update to typed roots

```rust
pub struct ProjectCreatePayload {
    pub name: String,
    pub roots: Vec<ProjectRootPath>,
}

pub struct ProjectAddRootPayload {
    pub id: ProjectId,
    pub root: ProjectRootPath,
}

pub struct ProjectDeleteRootPayload {
    pub id: ProjectId,
    pub root: ProjectRootPath,
}
```

`ProjectCreate` always produces `ProjectSource::Standalone`.
`ProjectAddRoot` / `ProjectDeleteRoot` only operate on standalone projects (see
§6.5 and §6.6).

### 3.5 Scoped reorder

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectReorderScope {
    TopLevel,
    WorkbenchChildren { parent_project_id: ProjectId },
}

pub struct ProjectReorderPayload {
    pub scope: ProjectReorderScope,
    pub project_ids: Vec<ProjectId>,
}
```

Drag/drop within the top-level list reorders standalone projects. Drag/drop
within a parent's workbench list reorders that parent's children. Reordering
across scopes (e.g. moving a workbench out from under its parent) is not
representable.

---

## 4. Replay Semantics

On host stream registration:

1. Replay host bootstrap as today.
2. Replay every project with `ProjectNotify::Upsert`, ordered:
   - all `ProjectSource::Standalone` records first, ordered by `sort_order`;
   - then all `ProjectSource::GitWorkbench` records, grouped by their
     `parent_project_id` and ordered by their per-parent `sort_order`.
3. Replay agents (unchanged from `06-projects.md`).

This ordering guarantees the parent record is in the client's project map
before any workbench `Upsert` referencing it arrives. The same rule applies to
live updates: `WorkbenchCreate` only emits the new workbench's `Upsert` after
the parent record is already known to the subscriber, which is automatic
because the parent was emitted at subscription time.

---

## 5. Server-side ownership

A workbench is a project, so the existing project actor / store / fanout
absorbs it. There is no new actor. There is no new store.

### 5.1 Worktree path computation

The server is the only thing that computes worktree paths. The client never
sends a path.

For each parent root, the worktree path is the parent root's sibling directory
named:

```
<parent-basename>--<sanitized-branch>
```

Sanitization rules:

- Preserve `[A-Za-z0-9._-]` literally.
- Replace every other character with `-`.

Percent-encoding is deliberately not used: LLVM treats `%` in an output path
as a unique-name placeholder, so rust-lld cannot link inside a directory
whose name contains `%` (wasm builds inside the worktree fail with
"cannot open output file").

Examples:

```
parent root: /Users/mike/Tyde2
branch:      feature-login
path:        /Users/mike/Tyde2--feature-login

parent root: /Users/mike/Tyde2
branch:      feature/login
path:        /Users/mike/Tyde2--feature-login
```

The mapping is lossy: distinct branches (`feature/login` vs `feature-login`)
can map to the same path. The existing create preflight rejects a computed
path that already exists on disk or is registered to another project, so a
collision surfaces as `CommandErrorCode::Conflict` rather than corruption.

### 5.2 `ProjectStore`

The store gains:

```rust
impl ProjectStore {
    pub fn create_workbench(
        &mut self,
        parent_project_id: ProjectId,
        name: String,
        branch: GitBranchName,
        roots: Vec<WorkbenchRoot>,
    ) -> Result<Project, ProjectStoreError>;

    pub fn delete_workbench(
        &mut self,
        id: &ProjectId,
    ) -> Result<Project, ProjectStoreError>;

    pub fn list_children(&self, parent: &ProjectId) -> Vec<ProjectId>;
}
```

`create_workbench` and `delete_workbench` are the only mutation paths that
touch records with `ProjectSource::GitWorkbench`. They do not run git; the
project manager runs git first, then calls the store.

`list_children` is used by `ProjectDelete`, `ProjectAddRoot`, and
`ProjectDeleteRoot` to enforce the precondition checks in §6.

### 5.3 Git invocation

The server runs git directly with `tokio::process::Command`. No shell.

Creation uses, per parent root:

```
git -C <parent_root> rev-parse --show-toplevel
git -C <parent_root> check-ref-format --branch <branch>
git -C <parent_root> rev-parse --verify --quiet refs/heads/<branch>   # must fail (branch must not exist)
git -C <parent_root> rev-parse --verify <base_ref-or-HEAD>^{commit}
git -C <parent_root> worktree add -b <branch> <worktree_path> <resolved_sha>
```

Removal uses, per worktree root:

```
git -C <worktree_root> status --porcelain=v1 --untracked-files=all
git -C <parent_root> worktree remove <worktree_root>
```

No `--force` flag is used in v1.

### 5.4 Multi-root preflight + rollback

Creation against an N-root parent runs an explicit preflight before any
mutation:

1. The parent exists and is `ProjectSource::Standalone`.
2. Every parent root resolves to a git top-level (`rev-parse --show-toplevel`).
3. The branch passes `check-ref-format --branch` in every parent root.
4. The branch does not yet exist in any parent root.
5. Every computed worktree path is absent on disk.
6. No computed worktree path equals an existing `ProjectRootPath` of any other
   project record (standalone or workbench). This is a record-level check, not
   just a filesystem check.

If all preflight checks pass, the server runs `git worktree add -b <branch>
<path>` for each parent root in sequence.

If any single `git worktree add` fails:

- Stop. Do not proceed to remaining roots.
- Roll back every previously-created worktree from this same request by running
  `git worktree remove --force <path>`. (`--force` is allowed *here* because
  the worktree was created seconds ago by the server itself and is known to be
  empty.)
- Delete the branch created by each rolled-back add with
  `git branch -D <branch>`, so an identical retry is not blocked by residue.
- Do not persist any project record.
- Surface the original git failure as `CommandErrorCode::Internal`. Include
  any rollback-step failures verbatim in the message — never hide them.

If all `git worktree add` succeed, the server calls
`ProjectStore::create_workbench`. If the store call fails, run the same
rollback (`git worktree remove --force` and `git branch -D` for every created
worktree) and surface the storage error.

### 5.5 Concurrent create serialization

`WorkbenchCreate` against the same parent is serialized in the host handler
via a per-parent mutex. Two concurrent requests for branch `foo` against the
same parent never both pass the "branch does not exist" preflight; the loser
sees `CommandErrorCode::Conflict`.

(Concurrent create against *different* parents is fine — the mutex is keyed on
parent id.)

### 5.6 No new actor

A "workbench actor" would have to coordinate with the project actor that
already manages `/project/<id>` for the workbench's roots. Workbench filesystem
state and git status are exactly what the project actor already watches. The
only additional cross-actor work is "spawn the project actor on create, abort
it on remove," which the existing project lifecycle already does for
standalone projects.

---

## 6. Lifecycle & Deletion Rules

### 6.1 Branch validation

`GitBranchName` is validated at the protocol boundary. The server runs
`git check-ref-format --branch <branch>` against every parent root. If
that command exits non-zero, `WorkbenchCreate` fails with
`CommandErrorCode::InvalidInput`. This catches `..`, `x y`, leading `-`,
embedded refspec characters, etc., before any other work.

### 6.2 Workbench create

Server flow:

1. Look up the parent. Reject `NotFound` if absent.
2. Reject `InvalidInput` if parent is not `ProjectSource::Standalone`. (Closes
   nested workbenches at the protocol level.)
3. Reject `InvalidInput` if any parent root is not a git top-level. Workbenches
   are specifically a git feature. A standalone project does not require git;
   a workbench parent does. (Per claude's omission #2.)
4. Validate branch (§6.1).
5. Compute worktree paths (§5.1).
6. Multi-root preflight + rollback flow (§5.4).
7. Persist `ProjectSource::GitWorkbench` record. Assign per-parent
   `sort_order = max(existing children sort_order) + 1`.
8. Fan out `ProjectNotify::Upsert { project }`.
9. Spawn the project actor. Failure is logged as a warning because git, store,
   and the authoritative upsert have already succeeded.

Workbench creation is allowed while parent agents/terminals are running.
That is the point of the feature.

### 6.3 Parent delete

`ProjectDelete` is rejected with `Conflict` if any project record has
`ProjectSource::GitWorkbench { parent_project_id, .. }` matching the target.

```
cannot delete project <id> while referenced by workbench <workbench_id>
```

The check is metadata-only. It does not look at on-disk state. A parent whose
on-disk directory has been removed externally is still rejected for delete
while a workbench record exists, because the protocol's parent linkage is what
the rule is about.

The user must `WorkbenchRemove` every child first. There is no cascade.

### 6.4 Workbench rename

`ProjectRename` works on a workbench identically to a standalone project. It
changes only `Project.name`. The git branch and on-disk path do not change.

### 6.5 `ProjectAddRoot` rules

- On a workbench: rejected with `InvalidInput`. Workbench roots are managed
  only by `WorkbenchCreate` / `WorkbenchRemove`.
- On a parent that has at least one workbench child: rejected with `Conflict`.
  The workbench's `WorkbenchRoot.parent_root` linkage is a live reference into
  the parent's `Standalone { roots }`. Adding a root would silently introduce
  a parent root with no corresponding worktree, breaking the "workbench mirrors
  parent" invariant. The user removes every workbench first, then adds the
  root, then re-creates workbenches.

### 6.6 `ProjectDeleteRoot` rules

- On a workbench: rejected with `InvalidInput`.
- On a parent that has at least one workbench child: rejected with `Conflict`.
  Removing a parent root that any workbench's `WorkbenchRoot.parent_root`
  points at would create a dangling reference in the protocol type. Same
  resolution: remove the workbenches first.

### 6.7 Workbench delete (`WorkbenchRemove`)

`ProjectDelete` on a `ProjectSource::GitWorkbench` record is rejected with
`InvalidInput`. The user must use `WorkbenchRemove`. This ensures the
on-disk worktree is always cleaned up alongside the record.

`WorkbenchRemove` blockers — any of the following rejects with `Conflict`:

- An agent whose `AgentStartPayload.project_id == workbench.id` is currently
  live.
- A terminal launched in this project is currently live.
- A persisted session (`SessionRecord.project_id`) references this id.
- A project-scoped steering record (`Steering { scope: Project(id) }`)
  references this id.
- A persisted team member binding references this project.
- Any worktree root is dirty (`git status --porcelain=v1 --untracked-files=all`
  produces output). The error message includes the dirty root paths verbatim.
- The parent project record is missing — surfaces as `Internal` (this means
  the store is corrupt; see §8).
- An absent worktree path is pruned from git bookkeeping and does not block
  deletion of the authoritative record.

No automatic cleanup of any blocker. The user resolves each explicitly.

Successful flow:

1. Validate the target is a workbench.
2. Validate every blocker above.
3. Run `git worktree remove <worktree_root>` for every root (no `--force`).
4. Call `ProjectStore::delete_workbench`.
5. Abort the project actor and drop its stream.
6. Fan out `ProjectNotify::Delete { project }`.

`ProjectNotify::Delete` is only emitted after both git removal and store
deletion succeed. If git removal succeeds but store delete fails, surface
`Internal` and leave the record in place. Do not emit `Delete`. The record
points at missing roots, which is unfortunate but is less dangerous than
asserting deletion that didn't happen. A future explicit repair command can
handle this.

---

## 7. Frontend Projection

The frontend already maintains a `Vec<Project>` per host, populated from
`ProjectNotify`. Workbench rendering is a derivation, not a new source of
truth.

### 7.1 Rail rendering

```rust
let projects: Memo<Vec<Project>> = state.projects_for_host(host_id);

let top_level = move || {
    projects.get()
        .iter()
        .filter(|p| matches!(p.source, ProjectSource::Standalone { .. }))
        .cloned()
        .collect::<Vec<_>>()
};

let workbenches_for = move |parent: ProjectId| {
    projects.get()
        .iter()
        .filter(|p| p.parent_project_id() == Some(&parent))
        .cloned()
        .collect::<Vec<_>>()
};
```

The view is a `<For>` over `top_level` keyed by `Project.id`, with a nested
`<For>` inside each row over `workbenches_for(row.id)` also keyed by
`Project.id`. Both closures must live inside `move ||` blocks so adding /
removing workbenches reactively re-runs.

There is no parallel "workbench list" collection on the frontend. There is one
source of truth — server-emitted projects — and one derivation per render
site.

### 7.2 Context menus

- On a top-level (`Standalone`) project: **Rename**, **Manage roots**,
  **New Workbench**, **Delete Project**.
- On a workbench: **Rename**, **Remove Workbench**.

### 7.3 Selecting a workbench

Selecting a workbench in the rail behaves identically to selecting a
standalone project: the frontend opens its `/project/<id>` stream and shows
file browser, git, terminals, chats. Nothing in the selection logic branches
on `is_workbench()`.

### 7.4 Active-project fallback on delete

If the user is currently viewing a project and `ProjectNotify::Delete` arrives
for that project's id:

- If the deleted project is a workbench, fall back to its `parent_project_id`
  if that parent record is still present; otherwise fall back to home.
- If the deleted project is a standalone project, fall back to home.

The deleted payload carries the full project, so `parent_project_id` is
available without a second lookup.

### 7.5 What the UI does NOT do

- It does not run `git worktree add` or `git worktree remove`.
- It does not compute the `<root>--<branch>` path.
- It does not infer parent/child relationships from path conventions (e.g.
  parsing `--`).
- It does not store an "is this a workbench" boolean separate from the
  protocol record.
- It does not check `.git` directly.

---

## 8. Storage

`~/.tyde/projects.json` gains an explicit `version` field and a new record
shape:

```json
{
  "version": 2,
  "records": {
    "abc-123": {
      "id": "abc-123",
      "name": "Tyde2",
      "sort_order": 0,
      "source": {
        "kind": "standalone",
        "roots": ["/Users/mike/Tyde2"]
      }
    },
    "def-456": {
      "id": "def-456",
      "name": "feature-login",
      "sort_order": 0,
      "source": {
        "kind": "git_workbench",
        "parent_project_id": "abc-123",
        "branch": "feature-login",
        "roots": [
          {
            "parent_root": "/Users/mike/Tyde2",
            "worktree_root": "/Users/mike/Tyde2--feature-login"
          }
        ]
      }
    }
  }
}
```

### 8.1 Migration

On load, an unversioned store (the current v1 shape with top-level `roots`)
migrates to v2:

```text
v1: { id, name, roots, sort_order }
→
v2: { id, name, sort_order, source: { kind: "standalone", roots } }
```

The migration runs once on load and writes back v2 with the same atomic-rename
semantics as today. Migration **must not** infer workbenches from path patterns
or any other heuristic. If the user wants to import original Tyde workbenches,
that will be a separate explicit command (non-goal §10).

### 8.2 Validation on load

- Every id is non-empty.
- Every name is non-empty.
- Every standalone `roots` is non-empty and unique.
- Every workbench `roots` is non-empty.
- Every workbench `parent_project_id` resolves to an existing record.
- Every workbench parent record is `ProjectSource::Standalone`.
- For every workbench root, `parent_root` matches an entry in the parent's
  `Standalone { roots }`.
- Workbench `parent_root` and `worktree_root` are unique within the workbench.
- No two records share a `worktree_root` or `roots[i]`.
- `sort_order` values are sufficient for deterministic sort within scope.

Load validates leniently for legacy survivability: invalid records are
quarantined with warnings and the repaired store is persisted. Mutation paths
continue to enforce the strict invariants above.

### 8.3 Atomic writes

Unchanged from `06-projects.md`: serialize → temp file → fsync → rename.

---

## 9. Edge cases

### 9.1 Parent project deleted while workbench exists

Rejected (§6.3). Metadata-only check; on-disk state is irrelevant to the
rejection.

### 9.2 Workbench worktree directory deleted out-of-band

The project actor for the workbench begins emitting project errors on git and
file operations — existing failure model. `WorkbenchRemove` prunes stale git
bookkeeping and deletes the authoritative project record.

### 9.3 Workbench has uncommitted or untracked changes on remove

Rejected with `Conflict`. Error message includes the dirty root paths
verbatim. No `--force`, no auto-stash, no auto-discard.

### 9.4 Workbench branch already exists at create time

Rejected with `Conflict`. v1 only creates new branches; adopting an existing
branch is non-goal §10.

### 9.5 Worktree path already exists on disk

Rejected with `Conflict`. No auto-suffix, no idempotent reuse.

### 9.6 Worktree path already registered as another project's root

Rejected with `Conflict` at preflight (§5.4 step 6). This is a record-level
check, not just a filesystem check.

### 9.7 Parent has dirty changes at create time

Allowed. The UI path starts from `HEAD`; agent-control MCP may select an
explicit commit-ish. Both paths resolve a committed base and never use
working-tree state. The server does not copy, stash, or otherwise interpret
the parent's dirty content.

### 9.8 Parent root is not a git top-level

Rejected with `InvalidInput` at preflight (§5.4 step 2). Workbenches are a
git feature; a standalone project that points at a non-git directory cannot
be workbenched. (The user can still keep the project standalone.)

### 9.9 Parent is itself a workbench

Rejected with `InvalidInput` at preflight (§5.4 step 1, since
`parent.source` is not `Standalone`). No nested workbenches.

### 9.10 Parent removes a `parent_root` referenced by a workbench

Rejected (§6.6). The user must remove the workbench first.

### 9.11 Branch contains a slash

Allowed if git accepts it. Path suffix replaces the slash with a dash:

```
feature/foo  →  <parent>--feature-foo
```

### 9.12 Concurrent `WorkbenchCreate` against the same parent

Serialized via per-parent mutex (§5.5). One request wins, the other receives
`CommandErrorCode::Conflict` from the "branch does not exist" preflight.

### 9.13 Project stream startup fails after successful create

The workbench project record exists (git and store both succeeded). The stream
failure is surfaced as a project/command error. The record is not rolled back
because a subscriber failed.

### 9.14 Store delete fails after successful `git worktree remove`

`ProjectNotify::Delete` is **not** emitted. `Internal` is surfaced. The
record remains and now points at missing roots. A future repair command can
handle this. Pretending the delete succeeded would be worse.

### 9.15 Server crash mid-create

If the crash is between `git worktree add` and store persistence, the worktree
exists on disk with no record. Next server start loads cleanly with no
workbench record. The orphan is benign — the next `WorkbenchCreate` of the
same branch surfaces the path-collision error, making the orphan visible to
the user. We do not run startup reconciliation; that is exactly the kind of
inferred-state code the philosophy doc forbids.

### 9.16 Remote hosts

The frontend sends `WorkbenchCreate` on the host stream of the host that owns
the parent project. The remote server runs `git worktree add` against its own
filesystem. The resulting `ProjectNotify::Upsert` flows back on the same host
connection. No `ssh://` paths, no local-vs-remote branching in the frontend.

Workbenches **cannot** span hosts. A workbench whose worktree lives on a
different host than its parent is not a workbench; that is a clone, and is
non-goal §10.

---

## 10. Non-Goals

Not part of v1:

- Nested workbenches.
- Workbench creation from an existing branch.
- Force-remove of dirty worktrees (future explicit `force` flag).
- Auto-stash, auto-discard, or auto-commit of dirty changes.
- Deleting git branches when removing a workbench.
- Renaming git branches.
- Moving worktree directories on `ProjectRename`.
- Per-root branch names for multi-root projects.
- Auto-syncing parent root edits (`ProjectAddRoot` / `ProjectDeleteRoot`) into
  existing workbenches.
- Cascading parent deletion into workbench deletion.
- Closing agents / killing terminals / deleting sessions automatically.
- Cross-host workbenches (worktree on a different host than parent).
- Auto-discovery of pre-existing on-disk worktrees not created by Tyde.
- Importing original Tyde (`~/Tyde`) workbench records — would be a separate
  explicit import command.
- Repairing out-of-band-deleted worktrees (future explicit
  `WorkbenchForgetMissing` repair command).
- Agent-driven workbench removal.
- Showing parent/workbench git ahead/behind relationships in the UI.

---

## 11. Agent-control MCP

Authenticated control-surface callers can use `tyde_list_workbenches` and
`tyde_create_workbench`. Listing is read-only and is limited to the caller's
canonical standalone project plus that project's workbenches. Creation is
limited to that same standalone parent and is rejected for read-only callers.

Creation accepts an optional `base_ref`. When omitted, every parent root uses
its current `HEAD`. Before any worktree is added, the server resolves the base
to a full commit SHA and records whether every parent root is dirty. Only the
resolved SHA is passed to `git worktree add`; uncommitted and untracked parent
changes are disclosed in the result but are never copied. A base must resolve
in every root of a multi-root project or creation makes no changes.

The result carries the canonical project identity, branch, parent identity,
and one entry per root containing the parent root, worktree root, resolved base
commit, and dirty-parent disclosure. `tyde_spawn_agent` should receive the
returned `project_id`, not copied paths. With a project id, the server derives
authoritative roots; supplied roots must match them. Without a project id,
explicit non-empty roots remain required.

The UI creation path continues to use parent `HEAD`; exposing base selection in
the protocol and UI is deferred. Removal, reassignment, journaling, startup
reconciliation, and idempotency keys are not part of this MCP v1.
