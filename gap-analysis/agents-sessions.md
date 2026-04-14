# Agents and Sessions Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/agents.ts`
- `~/Tyde/src/sessions.ts`
- `~/Tyde/src/workspace_view.ts`

### Rewrite reference points
- `frontend/src/components/agents_panel.rs`
- `frontend/src/components/sessions_panel.rs`
- `frontend/src/state.rs`

### Legacy coverage
- Agents panel supported summaries plus action hooks like interrupt, terminate, and remove.
- Sessions panel supported rename/delete flows, alias handling, external/non-Tyde sessions, host/workspace-aware identity, and virtualization for larger lists.
- Session handling was more explicitly tied to backend/session metadata rather than only a lightweight summary card list.

### Rewrite coverage
- Agents panel supports filtering and selecting an agent to focus chat.
- Sessions panel supports filtering, refreshing, and resuming a session.
- Parent/child grouping exists in both panels in simplified form.

### Confirmed gaps vs legacy
- No interrupt action from the agents panel.
- No terminate action from the agents panel.
- No remove/close agent action from the agents panel.
- No agent summary/preview text comparable to the legacy cards.
- No session rename UI.
- No session delete UI.
- No alias editing flow.
- No external backend session view.
- No host-aware session identity in the UI.
- No virtualization for larger session lists.
- No explicit new-session flow from the sessions panel.
- Current panels are browse/select surfaces; the legacy panels were more operational.

### Suggested next slices
- Restore agent actions first; that unlocks real runtime control and is higher leverage than card polish.
- Then restore session mutation flows (`rename`, `delete`, aliases), especially if resume becomes a primary workflow.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Agent-control parity is blocked by protocol shape. The rewrite `AgentInput` enum only contains `SendMessage`, so interrupt/cancel/terminate-style controls cannot be added from the panel without expanding the protocol.
- Session-management parity is also blocked by protocol scope. The rewrite protocol exposes session listing and resume, but not session rename, delete, alias mutation, or richer record management.
- Rewrite `SessionSummary` is thinner than the legacy session-record model and does not carry host identity or the richer metadata the legacy sessions panel uses for cross-host and external-session handling.
- The rewrite agents panel also lacks legacy-style operational context like summary text and action hooks, so it functions as a selector rather than a control surface.

### Architectural note
- This surface depends on whether the rewrite intends agents/sessions to be browse-only or operational. The legacy app clearly treated them as operational runtime controls.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Legacy E2E coverage verifies interrupt/remove controls from the agents views and terminate semantics that dispose runtime conversations. The rewrite agents surface has no equivalent controls.
- Legacy E2E coverage verifies that hidden runtime agent chats can be reopened from the agents widget without duplicating tabs or losing live updates. The rewrite has no comparable hidden-chat recovery flow.
- Legacy E2E coverage verifies merged backend sessions, resume into a fresh tab, and deletion scoped to the selected backend session record. The rewrite sessions panel only supports a simplified resume list.
- Legacy sessions behavior includes toggles for agent sessions and external/non-Tyde sessions, plus alias editing. The rewrite sessions panel is materially narrower and lacks all of those operational filters.

## Pass 6 - GPT-5 Codex - 2026-04-13

### Additional presentation and lifecycle gaps
- Legacy sessions are organized with richer lifecycle cues: loading/error states, day-group separators, active-session highlighting, and resuming-state feedback. The rewrite sessions panel is a flat list with refresh and filter controls but no comparable state presentation.
- Legacy agent/session surfaces filter or separate internal/runtime-helper records more deliberately. The rewrite panels currently render whatever agent/session summaries arrive, with no equivalent user-facing suppression or categorization beyond the basic child-session filter.
