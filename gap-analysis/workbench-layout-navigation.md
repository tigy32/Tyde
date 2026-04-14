# Workbench, Layout, and Navigation Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

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

### Legacy coverage
- True workspace shell with tab manager for chats/files, unread state, streaming state, tab rename/reorder/close operations, and docking/tiling persistence.
- Workbench concept existed as a real navigation primitive, not just a component name.
- Command palette had richer command history and indexed file search over the workspace.

### Rewrite coverage
- Fixed three-way center selector: `Home`, `Chat`, `Editor`.
- Fixed dock layout: left is `Files/Git`, right is `Agents/Sessions`, bottom is `Terminal`.
- Command palette supports a static command list and file search over the in-memory file tree.

### Confirmed gaps vs legacy
- No real chat/file tab model in the center area.
- No unread or streaming indicators per tab.
- No drag-reorder, close-others, or tab rename flows.
- No layout persistence.
- No tiling engine.
- No movable widgets/workbench widgets.
- No workbench entities despite the `Workbench` component name.
- No simultaneous multi-chat and multi-file workflows; the center area is one global mode switch.
- Command palette is materially smaller in scope than the legacy version.
- File search in the command palette is limited to already-loaded tree entries, not an indexed workspace view.

### Suggested next slices
- Reintroduce a real center-tab model before building more feature depth into `Chat` or `Editor`; the single-view architecture itself is a parity blocker.
- Keep dock simplification if desired, but tab state and persistence need to return for the app to feel comparable to the legacy workspace shell.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- The rewrite's center-area limitation is encoded directly in app state. `CenterView` is only `Home | Chat | Editor`, and the corresponding state only stores one `open_file` and one `diff_content`.
- Because of that state shape, the rewrite cannot naturally support multiple simultaneous file editors, multiple diffs, or several independent chat/file contexts the way the legacy tab/workbench model does.
- The rewrite command palette is not just smaller; it is structurally shallower. The legacy palette had history/section behavior and independent file indexing, while the rewrite palette searches static commands plus the already-materialized project tree.
- Dock content is fixed at compile time in the rewrite (`Files/Git`, `Agents/Sessions`, `Terminal`). The legacy shell had a more widgetized workspace model with layout-level ownership.

### Architectural note
- This is the main parity bottleneck for the rewrite. As long as the center area remains a single global mode switch, every other surface has to compete for the same slot.

## Pass 3 - GPT-5 Codex - 2026-04-13

### Additional interaction-level gaps
- Legacy workspace navigation exposes file/diff panel actions like find and go-to-line through the active tab context. The rewrite has no comparable center-surface command routing because there is no active file/diff tab abstraction.
- Legacy command palette can participate in a richer file-opening workflow because it targets the shared tabbed diff/file panel. The rewrite palette can only push one global editor state at a time.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Legacy E2E coverage verifies full dock/undock chat journeys with title preservation, message preservation, and multi-tab support. The rewrite has no docking lifecycle at all.
- Legacy E2E coverage verifies workspace isolation between multiple open workspaces and restoration of per-workspace state when switching back. The rewrite does not have a comparable multi-workspace shell.
- Legacy E2E coverage verifies that closing the last tab keeps the active project selected instead of dropping back into an invalid state. The rewrite does not have a tab lifecycle where this behavior could exist.
- Legacy E2E coverage verifies recovery from malformed persisted workspace roots without producing a blank workspace. The rewrite currently has far less persisted workbench state, but also no equivalent resilience path for richer workspace restoration because that system is absent.
- Legacy E2E coverage verifies git workbench/worktree creation, rename, switch, and removal from navigation context menus. The rewrite has no workbench/worktree concept beyond the component name.

## Pass 5 - GPT-5 Codex - 2026-04-13

### Additional workspace-instance gaps
- Legacy opening a workspace or creating a workbench lands in a workspace-local welcome screen with no inherited tabs from the parent workspace. The rewrite has no per-workspace welcome state; it reuses the same global `Home | Chat | Editor` center surface for every project.
- Rewrite workspace state is still globally singleton-shaped (`active_project_id`, `active_agent_id`, `open_file`, `diff_content`). That means there is no isolated initial state per workspace/workbench comparable to the legacy workspace-view instances.

## Pass 6 - GPT-5 Codex - 2026-04-13

### Additional command-palette gaps
- Legacy command palette persists recent command history and renders sectioned command results (`Recent` vs all commands). The rewrite palette has a fixed static command list and no persistence/history model.
- Legacy command palette behaves like a broader workspace navigator with its own indexing lifecycle and result-state management. The rewrite palette is still a much thinner transient search overlay.
