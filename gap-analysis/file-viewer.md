# File Viewer Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/explorer.ts`
- `~/Tyde/src/diff_panel.ts`
- `~/Tyde/src/tabs.ts`
- `~/Tyde/src/workspace_view.ts`
- `~/Tyde/src/command_palette.ts`

### Rewrite reference points
- `frontend/src/components/file_explorer.rs`
- `frontend/src/components/file_view.rs`
- `frontend/src/components/center_zone.rs`
- `frontend/src/actions.rs`
- `frontend/src/components/command_palette.rs`

### Legacy coverage
- Rich file explorer with lazy directory loading, selected-row state, search, hidden-file persistence, context menu, and reveal-in-folder support.
- Files opened into reusable file tabs rather than a single global editor slot.
- File viewer and diff viewer shared one richer panel with syntax-aware rendering, line targeting, and large-file virtualization.
- Chat-linked file references could open a file directly and jump to a line.
- Command palette indexed workspace files instead of only already-loaded tree entries.

### Rewrite coverage
- File explorer is built from the project snapshot the server pushes and supports a basic filter box plus a client-side hidden-file toggle.
- Clicking a file issues `ProjectReadFile` and stores one `open_file` in global state.
- File view is a simple `<pre><code>` rendering with a binary placeholder.
- Command palette can open files that already exist in the loaded tree snapshot.

### Confirmed gaps vs legacy
- No file tabs. The rewrite can only hold one open file in global state.
- No workbench-style file navigation. Opening another file replaces the current file view.
- No selected-file state in the explorer.
- No explorer context menu.
- No reveal-in-folder / reveal-in-Finder action.
- No lazy per-directory loading behavior; the rewrite consumes a prebuilt full tree snapshot instead.
- No persistence for explorer hidden-file preference.
- No line numbers in the file viewer.
- No syntax highlighting.
- No large-file virtualization.
- No jump-to-line support from chat-linked file references.
- No shared file/diff panel behavior like the legacy `DiffPanel` supported.
- No richer command-palette file indexing across the workspace; results are limited to whatever is already present in the in-memory tree.

### Suggested next slices
- Introduce tabbed file state before polishing rendering; otherwise most file-view improvements still feel regressed.
- Add line targeting from chat output and command palette next, because that unlocks cross-surface workflows.
- Reuse the same rendering path for plain files and diff-adjacent file previews so parity work compounds.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Multi-root projects are functionally broken in the rewrite file surface. `ProjectFileListPayload` preserves roots, but `frontend/src/dispatch.rs` flattens all root entries into one `Vec<ProjectFileEntry>` keyed only by `ProjectId`.
- After flattening, the explorer tree only retains `relative_path`, so duplicate relative paths across roots collide conceptually and root grouping disappears.
- File open requests in `frontend/src/actions.rs` always use `project.roots.first()` as the root. For any project with more than one root, file opens can target the wrong root.
- The command palette inherits the same problem because it also opens files by relative path only.
- The rewrite server now computes a full recursive file snapshot of every project root on refresh via `collect_entries`, whereas the legacy explorer listed directories lazily. This is not just a UX difference; it is a scalability regression for large workspaces.
- `ProjectFileKind::Symlink` exists in protocol, but the rewrite explorer renders every non-directory node as a regular file entry. There is no symlink-specific treatment or affordance.
- The center area cannot keep a file and diff open independently. `CenterZone` picks `FileView` if `open_file` is set, otherwise `DiffView`; this makes the editor surface mutually exclusive instead of tabbed.

### Architectural note
- File-viewer parity is blocked by state shape as much as rendering. The rewrite needs root-aware file identity and tab-aware editor state before line-targeting or syntax-highlighting work will compose cleanly.

## Pass 3 - GPT-5 Codex - 2026-04-13

### Interaction-level gaps
- Legacy file viewing includes find-in-file support from the shared diff/file panel. The rewrite file viewer has no find UI or keyboard-driven search path.
- Legacy file viewing includes go-to-line support. The rewrite has no line-jump affordance.
- Legacy file viewing can reveal and scroll to a linked line when opened from chat or other surfaces. The rewrite has no equivalent reveal flow.
- Legacy file viewing lives in the same richer panel as diffs, so file tabs can reuse search, navigation, and virtualization behavior. The rewrite file view is a separate minimal `<pre>` surface with none of those controls.

## Pass 6 - GPT-5 Codex - 2026-04-13

### Additional refresh-behavior gaps
- Legacy test coverage verifies that an already-open file tab refreshes when an agent/tool modifies that file in the background.
- In the rewrite, open file contents only update when a new `ProjectFileContents` frame arrives. `frontend/src/dispatch.rs` does not reconcile the current `open_file` against project change notifications or file-list refreshes, so an open file view can remain stale until the user manually reopens it.
