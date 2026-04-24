# Git Diff Viewer Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/git.ts`
- `~/Tyde/src/diff_panel.ts`
- `~/Tyde/src/diff_settings.ts`
- `~/Tyde/tests/e2e/git.test.ts`

### Rewrite reference points
- `frontend/src/components/git_panel.rs`
- `frontend/src/components/diff_view.rs`
- `frontend/src/state.rs`
- `frontend/src/components/settings_panel.rs`
- `server/src/project_stream.rs`
- `server/src/router.rs`
- `protocol/src/types.rs`

### Implemented since earlier passes
- Diff view now supports persisted layout modes (`Unified` / `Side by Side`).
- Diff context controls exist (`Hunks` / `Full File`).
- Find-in-diff is implemented.
- Diff/file views now share center-tab workflows instead of a single global diff slot.

### Remaining gaps vs legacy
- No unstage action.
- No discard action.
- No commit UI.
- No bulk stage/unstage actions.
- No hunk-stage UI despite backend support for `ProjectStageHunk`.
- No ahead/behind branch indicators in git header.
- Multi-root git presentation is still weak:
  - header branch derives from first root,
  - root sections are not explicitly labeled by root path.
- Non-git workspaces still do not have a clean, explicit legacy-style "Not a git repository" state (server-side subscription still treats git-status failure as fatal for project polling).
- Diff tabs are still coarse-grained (content keyed by root+scope, not independently per diff target/path).
- No syntax highlighting in diff lines.
- No large-diff virtualization.
- No before/after (non-git-status) diff mode for tool-output/feedback workflows.
- No richer line-selection/copy/feedback actions comparable to legacy diff interactions.
