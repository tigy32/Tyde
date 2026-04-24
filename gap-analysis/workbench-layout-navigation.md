# Workbench, Layout, and Navigation Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/workspace_view.ts`
- `~/Tyde/src/layout.ts`
- `~/Tyde/src/tabs.ts`
- `~/Tyde/src/command_palette.ts`
- `~/Tyde/src/tiling/*`

### Rewrite reference points
- `frontend/src/components/workbench.rs`
- `frontend/src/components/center_zone.rs`
- `frontend/src/components/dock_zone.rs`
- `frontend/src/components/command_palette.rs`
- `frontend/src/state.rs`

### Implemented since earlier passes
- Center zone now has a real tab model (`Home`, `Chat`, `File`, `Diff`) instead of a single mode switch.
- Multi-chat and multi-file workflows are now supported.
- Tab operations now include rename, close, close-others, and close-to-right.
- Per-project center-state memory exists (switching projects restores project-local tab/selection context).
- Workbench dock panels are resizable.

### Remaining gaps vs legacy
- No tab drag-reorder in the center tab bar.
- No unread/streaming status indicators on tabs.
- Layout persistence is still limited (dock sizes/layout composition are not restored like legacy workspace layouts).
- No tiling engine or movable widget system.
- No explicit workbench/worktree entities (creation/switch/rename/remove) in navigation.
- Command palette still lacks persistent history/"Recent" sections.
- Command palette file search still depends on in-memory tree data rather than a fuller workspace indexing lifecycle.
- No legacy-style dock/undock chat lifecycle flows.
