# Projects, Hosts, and Home Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/home_view.ts`
- `~/Tyde/src/projects.ts`
- `~/Tyde/src/project_state.ts`
- `~/Tyde/src/settings.ts`

### Rewrite reference points
- `frontend/src/app.rs`
- `frontend/src/bridge.rs`
- `frontend/src/components/home_view.rs`
- `frontend/src/components/project_rail.rs`
- `frontend/src/components/host_browser.rs`
- `frontend/src/components/settings_panel.rs`
- `frontend/src/state.rs`

### Implemented since earlier passes
- Multi-host model is now present in app state and UI.
- Host selection, add/remove host flows, and remote SSH host setup exist.
- Remote host sections are shown in the project rail.
- Remote/local project open flow exists via host browser.
- Remote lifecycle/readiness status is surfaced in settings.
- Project delete UI exists in the rail.
- Home now has Projects/Agents tabs and host-grouped project sections.
- Home project cards now show active-agent counts.

### Remaining gaps vs legacy
- No onboarding/setup wizard on first launch.
- No dedicated bridge-chat entrypoint/readiness gating on home.
- No project rename UI.
- No manage-roots UI (`ProjectAddRoot` is available in protocol but not surfaced in UI).
- No explicit workbench/worktree creation/removal flows from navigation.
- Home agent list still lacks legacy-style filtering for internal/runtime-helper agents.
- Remote reconnect lifecycle behavior is improved but still not at full legacy parity (e.g., richer reconnect dialogs/state transitions).
