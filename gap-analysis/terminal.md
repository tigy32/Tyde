# Terminal Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/terminal.ts`

### Rewrite reference points
- `frontend/src/components/terminal_view.rs`
- `frontend/src/term_bridge.rs`
- `frontend/vendor/xterm-bridge.js`
- `frontend/src/dispatch.rs`
- `server/src/router.rs`
- `protocol/src/types.rs`

### Implemented since earlier passes
- Terminal rendering is now xterm-based (not plain `<pre>`).
- Keystroke-driven terminal interaction is restored.
- Resize/fit behavior is wired through terminal bridge and `TerminalResize`.
- Terminal close UI is implemented.
- Multiple terminal tabs are supported in the bottom dock.
- Exit signal metadata is now preserved/displayed in frontend state.

### Remaining gaps vs legacy
- Terminal title-change propagation is still missing.
- Terminal create flow is still first-root-only for multi-root projects (no root/cwd picker UI).
- `TerminalStartPayload.root` is still not preserved in frontend terminal state.
- Terminal errors are still mostly log-level/internal rather than strongly surfaced as user-facing terminal status cards.
- Terminal is still dock-only; no richer workbench tab placement parity with legacy workspace patterns.
