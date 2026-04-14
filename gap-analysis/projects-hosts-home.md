# Projects, Hosts, and Home Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/home_view.ts`
- `~/Tyde/src/projects.ts`
- `~/Tyde/src/project_state.ts`
- `~/Tyde/src/settings.ts`

### Rewrite reference points
- `frontend/src/app.rs`
- `frontend/src/components/home_view.rs`
- `frontend/src/components/project_rail.rs`
- `protocol/src/types.rs`

### Legacy coverage
- Home view was aware of onboarding, hosts, remote status, bridge chat availability, and agents across workspaces.
- Project rail grouped projects by host, showed remote-host sections, and exposed actions like add remote project, remove project, manage roots, and create/remove workbench.
- Host list and remote Tyde server state were first-class concepts throughout the app.

### Rewrite coverage
- The frontend connects to a single hardcoded host ID during app startup.
- Home view shows a hero, simple action buttons, projects, and active agents.
- Project rail can open home, switch projects, and create a project by typing a local path.

### Confirmed gaps vs legacy
- No multi-host model in the frontend.
- No host selector anywhere in the main app shell.
- No remote host sections in the project rail.
- No remote project open flow.
- No remote Tyde server readiness/status in home or project navigation.
- No onboarding wizard or dependency bootstrap flow.
- No bridge-chat readiness gating.
- No project rename UI.
- No project delete UI.
- No manage-roots UI.
- No workbench creation/removal from the project rail.
- The protocol exposes `ProjectRename`, `ProjectAddRoot`, and `ProjectDelete`, but the rewrite frontend does not use them.
- The rewrite home view is local-project focused and does not cover cross-host or remote-operability concerns that the legacy app already handled.

### Suggested next slices
- Add host awareness before expanding project actions; otherwise remote parity stays structurally impossible.
- Surface existing project mutation frames (`rename`, `add_root`, `delete`) once host/project identity is no longer hardcoded to a single local path.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Multi-host parity is blocked by the rewrite data model, not just the shell UI. `Project` and `SessionSummary` in the rewrite protocol do not carry host identity, while the legacy app organizes navigation and session behavior around host-scoped state.
- The rewrite app startup path hardcodes a single host connection in `frontend/src/app.rs`, which means host selection is absent before the UI even renders.
- Add-project flow in the rewrite is limited to typing a single local filesystem path into the rail popover. Legacy behavior covered remote projects, grouped remote hosts, and richer project/workbench actions.
- Project metadata is much thinner in the rewrite. The legacy home/rail surfaces show host grouping and project activity state; rewrite `Project` only carries `id`, `name`, and `roots`.
- The rewrite home surface has no equivalent of legacy onboarding/dependency readiness and therefore cannot communicate whether the environment is actually ready for a given backend or remote flow.

### Architectural note
- Host-aware parity likely requires host identity to become part of the rewrite app's main entity model, not just a sidebar concern.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Legacy E2E coverage verifies bridge-chat creation being enabled/disabled from home based on MCP control availability. The rewrite home view has no equivalent gated bridge-chat action.
- Legacy E2E coverage verifies project agent counts on home cards staying in sync with created chats. The rewrite home view shows only a simplified active-agent summary.
- Legacy E2E coverage verifies host headers in the sidebar/rail, including `Local` grouping. The rewrite rail has no host-grouped navigation.
- Legacy E2E coverage verifies remote connection dialogs, reconnecting states, suppression of stale remote projects before reconnect, and persistence of remote workspace views across navigation. The rewrite has no comparable remote-host project lifecycle.
- Legacy E2E coverage verifies an onboarding/setup wizard on first launch. The rewrite has no onboarding flow.

## Pass 5 - GPT-5 Codex - 2026-04-13

### Additional home-surface gaps
- Legacy home has a dedicated Projects/Agents tab structure with focused empty states and separate fetch/refresh behavior per section. The rewrite home view renders projects and agents as one static page and has no equivalent home-level tab model.
- Legacy home actions include both `Open Remote` and the `Bridge` entry point from the main hero/action area. The rewrite `frontend/src/components/home_view.rs` only exposes `New Chat` and `Open Workspace`.
- Legacy home behavior is explicitly tied to the workspace handoff flow: opening a workspace transitions into a workspace-local welcome state rather than leaving the user in a single global shell. The rewrite home surface has no comparable handoff concept.

## Pass 6 - GPT-5 Codex - 2026-04-13

### Additional agent-surface gaps on home
- Legacy home explicitly hides internal title-helper/runtime utility agents from user-facing agent surfaces. The rewrite home view renders all agents from `state.agents` without any comparable filtering rule.
