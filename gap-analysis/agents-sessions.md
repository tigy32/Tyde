# Agents and Sessions Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/agents.ts`
- `~/Tyde/src/sessions.ts`
- `~/Tyde/src/workspace_view.ts`

### Rewrite reference points
- `frontend/src/components/agents_panel.rs`
- `frontend/src/components/sessions_panel.rs`
- `frontend/src/state.rs`
- `protocol/src/types.rs`

### Implemented since earlier passes
- Agents panel now supports close/remove and rename actions.
- Agents panel can reopen/focus existing chat tabs without duplicating tabs.
- Sessions panel now supports delete in addition to refresh/filter/resume.
- Session list includes parent/child filtering, alias display, and per-host connectivity gating for resume/delete.
- Agent/session protocol is no longer limited to message send-only (`Interrupt`, `CloseAgent`, queued-message controls, and session-settings update paths exist).

### Remaining gaps vs legacy
- No interrupt/terminate controls from the **agents panel** itself (interrupt exists in chat input, not in the agent cards).
- No richer agent summary/preview text comparable to legacy cards.
- No session rename UI.
- No session alias editing flow.
- No dedicated external/non-Tyde session surface or filter.
- No explicit host grouping/host badges in session rows (host awareness exists in state, but not strongly in presentation).
- No list virtualization for large session sets.
- No explicit "new session" action from the sessions panel.
- Session lifecycle presentation is still thinner than legacy (no day-group separators, resuming/loading/error row states).
