# Git Diff Viewer Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/git.ts`
- `~/Tyde/src/diff_panel.ts`
- `~/Tyde/src/diff_settings.ts`
- `~/Tyde/tests/e2e/git.test.ts`

### Rewrite reference points
- `frontend/src/components/git_panel.rs`
- `frontend/src/components/diff_view.rs`
- `server/src/project_stream.rs`
- `server/src/router.rs`
- `protocol/src/types.rs`

### Legacy coverage
- Git panel supported repo discovery, repo selection, branch display, staged/changed/untracked groupings, bulk stage/unstage, discard, and commit flow.
- Diff panel supported multiple diff/file tabs, syntax-aware diff rendering, diff context controls, full-context expansion, and large-view virtualization.
- Diff interactions included line selection and feedback affordances.

### Rewrite coverage
- Git panel shows staged/unstaged/untracked groupings from the project snapshot and can refresh or stage a full file.
- Clicking a file requests `ProjectReadDiff` and opens a simple diff renderer.
- The server and protocol already support `ProjectStageHunk`, but the frontend does not surface it.

### Confirmed gaps vs legacy
- No repo discovery or repo selector UI.
- No inline multi-repo handling comparable to the legacy panel.
- No ahead/behind branch indicators in the UI even though the server tracks them.
- No unstage action.
- No discard action.
- No commit UI.
- No bulk stage / unstage controls.
- No hunk-stage UI even though the backend path exists.
- No diff tabs; only one global diff slot exists.
- No diff display settings like context size or view mode.
- No full-context expansion.
- No syntax highlighting in the diff view.
- No virtualization for large diffs.
- No selection actions, copy helpers, or feedback boxes.
- No side-by-side or richer file-preview modes from the legacy panel.
- No integrated diff/file switching inside the editor surface.

### Suggested next slices
- Expose `ProjectStageHunk` before adding advanced diff polish; the protocol/server path is already present.
- Add unstage/discard before commit UI, because the current workflow is effectively one-way.
- Unify diff state with future file-tab state instead of keeping a separate single-diff slot.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Some git parity gaps are not frontend-only. The rewrite protocol/server currently expose `ProjectStageFile` and `ProjectStageHunk`, but there are no primitives for unstage, discard, or commit. Those legacy features require protocol/backend expansion, not just UI work.
- `ProjectRootGitStatus` includes `ahead` and `behind`, but `frontend/src/components/git_panel.rs` only renders the branch name and drops ahead/behind state entirely.
- `ProjectGitDiffHunk` includes `hunk_id`, but `frontend/src/components/diff_view.rs` discards that identity while rendering. That makes hunk-level actions impossible even though the backend supports stage-hunk.
- The rewrite diff surface is effectively read-only. There is no way to act on a selected hunk, navigate back to file view in a richer way, or preserve diff-specific interaction state.
- Legacy diff behavior included generated before/after diffs and richer file-diff switching inside the same panel. The rewrite only renders the server's git diff payload into one global slot.

### Architectural note
- The rewrite already has enough backend surface to support a meaningful staged/unstaged diff workflow, but the current UI throws away some of that structure before it reaches the user.

## Pass 3 - GPT-5 Codex - 2026-04-13

### Interaction-level gaps
- Legacy diff viewing supports persisted diff-view settings including `unified` and `side-by-side` modes. The rewrite only renders one simple unified-style view.
- Legacy diff viewing supports find-in-diff and find-in-file flows directly from the diff/file panel. The rewrite has no diff search.
- Legacy diff viewing supports go-to-line and line reveal behavior through the shared panel APIs. The rewrite diff view has no navigation controls.
- Legacy diff viewing also covers before/after diffs produced outside normal git status flows, which is important for tool output and feedback flows. The rewrite diff surface is limited to server-provided git diff payloads.

## Pass 5 - GPT-5 Codex - 2026-04-13

### Additional state-handling gaps
- Legacy git behavior gracefully handles non-git workspaces and shows `Not a git repository` as a clean empty state without surfacing an error notification.
- The rewrite project subscription currently treats git availability as mandatory. `server/src/project_stream.rs` calls `build_git_status(&project).unwrap_or_else(|err| panic!(...))`, while `build_git_status` fails whenever `git status` exits non-zero. A non-git project therefore lacks the legacy graceful fallback path.
- Even if the backend were softened, `frontend/src/components/git_panel.rs` only distinguishes populated git status from `None` and renders a generic `No git status` empty state. It does not model the legacy repo-absent state separately.

## Pass 7 - GPT-5 Codex - 2026-04-13

### Additional multi-root git gaps
- The rewrite git header always derives branch display from `roots.first()` in `frontend/src/components/git_panel.rs`. For projects with multiple git roots, the top-level branch indicator is therefore ambiguous or outright wrong for the other roots being shown below.
- The rewrite renders one `GitRootSection` per root but never labels the section with its root path. In multi-root projects, users can end up with several unlabeled `Staged Changes` / `Changes` / `Untracked` groups with no repo/root context, which is materially weaker than the legacy repo-selection flow.
