# File Viewer Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/explorer.ts`
- `~/Tyde/src/diff_panel.ts`
- `~/Tyde/src/tabs.ts`
- `~/Tyde/src/workspace_view.ts`
- `~/Tyde/src/command_palette.ts`

### Rewrite reference points
- `frontend/src/components/file_explorer.rs`
- `frontend/src/components/file_view.rs`
- `frontend/src/components/find_bar.rs`
- `frontend/src/components/center_zone.rs`
- `frontend/src/actions.rs`
- `frontend/src/app.rs`
- `frontend/src/dispatch.rs`
- `server/src/project_stream.rs`

### Implemented since earlier passes
- Real center tabs now allow multiple file/diff/chat tabs.
- File viewer now has line numbers.
- Syntax highlighting is present for opened files.
- Find-in-file UI exists (shared find bar pattern with diff).
- Chat file links now open files in the viewer.
- Explorer now has lazy directory loading (`ProjectListDir`) rather than only a full recursive snapshot.

### Remaining gaps vs legacy
- Multi-root file handling is still incorrect in key paths:
  - `ProjectFileListPayload` roots are flattened client-side,
  - explorer/open flows remain mostly relative-path-first,
  - `open_file` still defaults to the first root.
- Command palette file-open flow is not root-aware and inherits the same multi-root ambiguity.
- No selected-row state in file explorer.
- No explorer context menu.
- No reveal-in-folder / reveal-in-Finder action.
- Hidden-file toggle is not persisted.
- No go-to-line control in file view.
- No line-target reveal when opening file references with `:line` suffixes.
- No large-file virtualization.
- No symlink-specific explorer affordance in project file tree.
- Command palette file indexing is still limited to loaded in-memory entries, not a fuller workspace index.
- Open file content can remain stale after background file modifications until reopened/refetched.
