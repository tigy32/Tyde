# Terminal Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/terminal.ts`

### Rewrite reference points
- `frontend/src/components/terminal_view.rs`
- `server/src/router.rs`
- `protocol/src/types.rs`

### Legacy coverage
- Xterm-based terminal sessions with proper terminal emulation, scrollback, resize/fit behavior, title updates, close handling, and native terminal input stream behavior.
- Multiple terminal sessions could be created and focused as first-class workbench tabs/views.

### Rewrite coverage
- Multiple terminal records can exist and be selected in a bottom dock.
- Terminal output is displayed as plain text in a `<pre>`.
- The user types into a standard input box that sends newline-terminated data.

### Confirmed gaps vs legacy
- No terminal emulation layer comparable to xterm.
- No ANSI-aware rendering beyond whatever plain text happens to look like.
- No resize/fit handling in the frontend.
- No close-terminal UI even though `TerminalClose` exists in protocol.
- No visible resize behavior even though `TerminalResize` exists in protocol.
- No terminal title-change handling.
- No scrollback/search/copy affordances comparable to a real terminal widget.
- No direct keystroke-driven shell interaction parity; the current input model is closer to sending lines than interacting with a PTY.
- No terminal view embedding into richer tabbed workbench state.

### Suggested next slices
- Replace the plain-text terminal body with a real terminal widget before layering more terminal commands on top.
- Surface close/resize paths already present in protocol once the frontend terminal model is upgraded.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Terminal interaction in the rewrite is line-oriented by design: a text input field sends strings to the PTY. That is fundamentally different from the legacy per-keystroke terminal session model.
- Terminal errors are not surfaced meaningfully in the UI. `frontend/src/dispatch.rs` logs `TerminalError`, but the terminal surface does not expose an error state comparable to the legacy runtime behavior.
- Terminal output is accumulated into one growing `String` buffer in app state. The legacy xterm-based path handled rendering/scrollback as terminal state rather than one plain-text buffer.

### Architectural note
- Terminal parity is partly a rendering problem, but mostly an interaction-model problem. A real terminal widget is the prerequisite for most remaining parity items.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Rewrite backend tests cover terminal close, resize, cwd targeting, and post-exit behavior, but the frontend terminal surface still does not expose close controls or a real resize-aware terminal widget.
- Legacy terminal service propagates terminal title changes into the UI; the rewrite terminal tabs do not participate in title updates.
- Legacy terminal integration is strong enough to support docked terminal views as part of the workspace shell. The rewrite bottom dock terminal remains a much thinner utility surface.

## Pass 7 - GPT-5 Codex - 2026-04-13

### Additional terminal state gaps
- The rewrite terminal-create flow is not root-aware for multi-root projects. `frontend/src/components/terminal_view.rs` always chooses `proj.roots.first()` and sends `relative_cwd: None`, so new terminals cannot be targeted to any root except the first one.
- The protocol/backend carry more terminal start metadata than the rewrite UI preserves. `TerminalStartPayload` includes `root`, but `frontend/src/state.rs::TerminalInfo` does not store it and `frontend/src/dispatch.rs` drops it on receipt.
- Terminal exit detail is also reduced in the rewrite UI. `TerminalExitPayload` includes both `exit_code` and `signal`, but the frontend only stores/displays `exit_code`, so signal-based termination context is lost.
