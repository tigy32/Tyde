# Git Write Workflows Implementation Plan

## Context

Tyde2's git integration has viewing (diff, status) but lacks core write workflows: unstage, discard, commit, hunk-stage UI, and bulk actions. The server polling also crashes for non-git workspaces. These are daily-use features for a dev workspace.

All changes follow the architecture: protocol types first (single source of truth) → server owns behavior → router dispatches → frontend renders state from events.

---

## Phase 1: Foundation (no new protocol types, parallelizable)

### WI-1: Non-git workspace handling (server-only)

**Problem**: `spawn_project_subscription` (project_stream.rs:82-91) kills the entire polling loop when `build_git_status` fails, stopping file-list updates too.

**Fix**: In `build_git_status_with_runner`, catch per-root errors containing `"not a git repository"` and emit a synthetic clean `ProjectRootGitStatus` (branch: None, clean: true, files: empty). Propagate all other errors as before.

**Files**: `server/src/project_stream.rs` (build_git_status_with_runner ~line 324)

---

### WI-2: Hunk-stage button in diff view (frontend-only)

**Problem**: Backend `stage_hunk` is fully implemented, `hunk_id` exists on `ProjectGitDiffHunk`, but no UI button.

**Fix**: In `UnifiedHunk` and `SideBySideHunk` components, add a "+" stage button next to the hunk header. Only show when `scope == Unstaged`. Button sends `ProjectStageHunk` frame. Thread `root`, `scope`, and `relative_path` through from `DiffView` → `DiffFileView` → hunk components.

**Files**: `frontend/src/components/diff_view.rs`

---

## Phase 2: New mutating operations (new protocol types)

### WI-3: Unstage file

**Protocol** (`protocol/src/types.rs`):
- Add `ProjectUnstageFile` to `FrameKind` (input events section, after `ProjectStageHunk`)
- Add `ProjectUnstageFilePayload { path: ProjectPath }`

**Server** (`server/src/project_stream.rs`):
- Add `unstage_file(project, path)`: runs `git restore --staged -- <path>`. On error containing `"bad default revision"` or `"unknown revision"` (empty repo, no HEAD), retry with `git rm --cached -- <path>`. This is the same approach as the legacy implementation.

**Server** (`server/src/host.rs`):
- Add `unstage_project_file` method following `stage_project_file` pattern (line 2192). Call `unstage_file`, then `refresh_after_project_mutation`.

**Router** (`server/src/router.rs`):
- Add `ProjectUnstageFile` match arm after `ProjectStageHunk`, same validation pattern.

**Frontend** (`frontend/src/components/git_panel.rs`):
- Add `show_unstage_btn` prop to `GitFileSection`, pass `true` for staged section.
- Render "-" button per staged file. Click sends `ProjectUnstageFile`.

---

### WI-4: Discard changes

**Protocol** (`protocol/src/types.rs`):
- Add `ProjectDiscardFile` to `FrameKind`
- Add `ProjectDiscardFilePayload { path: ProjectPath }`

**Server** (`server/src/project_stream.rs`):
- Add `discard_file(project, path)`: runs `git checkout -- <path>` then `git clean -f -- <path>`. Succeed if at least one succeeds (checkout handles tracked files, clean handles untracked). Error only if both fail.

**Server** (`server/src/host.rs`):
- Add `discard_project_file` following stage pattern. Call `discard_file`, then `refresh_after_project_mutation`.

**Router** (`server/src/router.rs`):
- Add `ProjectDiscardFile` match arm.

**Frontend** (`frontend/src/components/git_panel.rs`):
- Add discard button (x icon) on unstaged/untracked files, next to the stage button.
- **Must show confirmation** via `window.confirm_with_message("Discard changes to \"<file>\"? This cannot be undone.")` before sending. Pattern from agents_panel.rs:430.

---

### WI-5: Commit UI

**Protocol** (`protocol/src/types.rs`):
- Add `ProjectGitCommit` to `FrameKind` (input)
- Add `ProjectGitCommitResult` to `FrameKind` (output)
- Add `ProjectGitCommitPayload { root: ProjectRootPath, message: String }`
- Add `ProjectGitCommitResultPayload { root: ProjectRootPath, commit_hash: String }`

**Server** (`server/src/project_stream.rs`):
- Add `commit(project, root, message)`: runs `git commit -m <message>`, then `git rev-parse HEAD` to get hash.

**Server** (`server/src/host.rs`):
- Add `commit_project` method. Calls `commit`, sends `ProjectGitCommitResult` event, then `refresh_after_project_mutation(path: None)`.

**Router** (`server/src/router.rs`):
- Add `ProjectGitCommit` match arm. Validate non-empty root and message.

**Frontend** (`frontend/src/components/git_panel.rs`):
- In `GitRootSection`, add commit area above file sections:
  - `<textarea>` for message, bound to `RwSignal<String>`
  - "Commit" button, disabled when textarea empty or no staged files
  - On click: send `ProjectGitCommit`, clear textarea

**Frontend** (`frontend/src/dispatch.rs`):
- Handle `ProjectGitCommitResult`: log the commit hash. State updates arrive automatically via git status polling.

---

## Phase 3: Bulk operations and polish

### WI-6: Bulk stage/unstage

**No new protocol types.** Frontend-only: iterate over section files and send individual stage/unstage frames.

- Add "Stage All" (++) button in Changes/Untracked section headers
- Add "Unstage All" (--) button in Staged Changes section header
- Reuse existing `stage_file` and `unstage_file` helpers

**Files**: `frontend/src/components/git_panel.rs`

---

### WI-7: Root-labeled git sections (minor polish)

The root sections already show root name + branch at line 118-121. The top-level header at line 34-48 should show ahead/behind indicators when single-root. Multi-root header is acceptable as-is since each section is individually labeled.

**Files**: `frontend/src/components/git_panel.rs`

---

## Dependency order

```
Phase 1 (parallel):  WI-1, WI-2
Phase 2 (sequential): WI-3 → WI-4 → WI-5  (each establishes pattern for next)
Phase 3 (after WI-3): WI-6, WI-7 (parallel)
```

## Protocol changes summary

New FrameKind variants: `ProjectUnstageFile`, `ProjectDiscardFile`, `ProjectGitCommit`, `ProjectGitCommitResult`
New payload structs: 4 corresponding payloads

## Files to modify

- `protocol/src/types.rs` — new frame kinds + payloads
- `server/src/project_stream.rs` — unstage_file, discard_file, commit, non-git fix
- `server/src/host.rs` — 3 new host methods
- `server/src/router.rs` — 3 new match arms
- `frontend/src/components/git_panel.rs` — unstage/discard/commit/bulk UI
- `frontend/src/components/diff_view.rs` — hunk stage button
- `frontend/src/dispatch.rs` — ProjectGitCommitResult handler

## Verification

1. Build: `cargo build` from repo root (compiles protocol → server → frontend)
2. Start dev instance via `tyde_dev_instance_start` MCP tool
3. Open a git repo project — verify staging, unstaging, discard, commit work
4. Open a non-git directory project — verify no crash, file list works
5. Open a multi-root project — verify per-root sections with labels
6. Test hunk staging from diff view
7. Test bulk stage/unstage all
8. Test commit with empty staged area (should error visibly)
9. Test unstage in empty repo (first commit scenario)
