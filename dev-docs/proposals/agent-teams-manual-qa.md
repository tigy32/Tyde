# Agent Teams — Manual QA Report

Reviewer: Codex (manual driver via `tyde-debug` MCP)
Branch: `feat/agent-teams`
Dev instance: `aee72adf867d4ef8956e93bd0c853f13` — started ready → stopped (`exited(signal: 9)`). No QA child processes left running.

## Scenarios

- **A — Pass:** Created `QA Team 2026` through the New Team wizard. Team card showed `2 members`, with `QA Manager` and `QA Report`, correct CustomAgent labels, roots, and idle status.
- **B — Fail:** Clicking the team did **not** open a manager chat tab. Tabs stayed on Home/Projects/Agents; no `.chat-textarea`; no roster sidebar.
- **C — Blocked:** No roster sidebar was present. Clicking the report row in the team card also opened no chat tab.
- **D — Pass:** Report archive used bridge dialog (`plugin:dialog|message`, title `Archive member`), with no custom modal. After OK, report disappeared and count dropped to `1 member`.
- **E — Fail/copy:** Deleting the manager CustomAgent was rejected and visible in the header, but the error said only `referenced by team member <uuid>`, not the team name.
- **F — Pass:** Team archive used bridge dialog (`Archive team`), no custom modal. Team remained visible with an `archived` badge.

## Blockers / concerns

- **Blocker:** `frontend/src/components/teams_panel.rs` `open_member_chat` no-ops for fresh members with no `current_agent_id` and no `session_id`, so opening teams/reports is impossible after creation.
- **Blocker:** Roster sidebar from spec §9.2 appears absent, so project/live-status/last-active roster UX is unavailable.
- **Concern:** `server/src/host.rs` CustomAgent delete rejection is technically visible, but not user-friendly/team-referencing enough.
- **Nit:** Wizard is two screens, not the requested separate name → manager → report flow.

## Re-run on 2026-05-13 after blocker fixes (78088f7..fc797bc)

Reviewer: Codex (manual driver via branch-local `tyde-dev-driver debug` MCP)
Branch: `feat/agent-teams`
HEAD: `fc797bc feat(teams): Render roster sidebar alongside manager chats`
Dev instance: `d67e9dc5fef0450aa23211881f4d2b5d` — started ready via `tyde_dev_instance_start` (`frontendUrl` `http://127.0.0.1:54699`, host `127.0.0.1:54700`, UI debug `127.0.0.1:54701`) → stopped (`exited(signal: 9 (SIGKILL))`). Temporary clean stores were used and the user's original stores were restored afterward.

Note: the resident `mcp__tyde_debug` server in the parent app could not start this branch's child instance because its host-client protocol was newer (`client: 3`, branch child `server: 2`). I used the branch-built `tyde-dev-driver debug` MCP and drove the instance with its `tyde_debug_evaluate` tool.

### Scenarios

- **A — Pass:** Created `QA Team 20260513` through the New Team wizard with `QA Manager 20260513` and `QA Report 20260513`, both using `QA Teams Agent 20260513`. The Teams card showed `2 members`; both rows showed the CustomAgent label, workspace roots, and `idle` status.
- **B — Pass:** Clicking the team opened an active `QA Manager 20260513` chat tab. The chat input was visible and accepted typed text. The chat was a draft/no-agent tab: welcome text was shown, no chat header/backend badge was present, and the Agents panel still said `No agents yet`.
- **C — Fail:** With the fresh manager draft tab active, `.team-roster-sidebar` was absent. The roster did not appear until the manager was activated by a first message in scenario D.
- **D — Pass:** Sending `hello` from the manager draft tab activated the manager. The tab upgraded to a live `QA Manager 20260513` / `Codex` chat, displayed the sent user message, showed backend activity, then received `QA manager ack`. The roster sidebar remained visible after activation and listed the report with role, `idle`, CustomAgent label, and root.
- **E — Pass:** Clicking `QA Report 20260513` in the manager roster opened an active report chat tab. It showed the draft chat welcome/input and no report backend spawned; the Agents panel still listed only the manager agent.
- **F — Partial:** Clicking the report row's `Archive` button triggered the native-dialog path (no custom DOM modal appeared), but the native OK button could not be driven in this environment (`osascript` lacked accessibility permission). To complete state verification, I sent the same archive command over a branch-compatible protocol client. The report then disappeared from the team card and manager roster, and the card count dropped to `1 members`.
- **G — Pass:** Deleting the manager's CustomAgent from Settings showed the custom delete confirmation, then was rejected. The header showed `last error: custom_agent_delete failed ... cannot delete custom agent ... while referenced by team member ...`, and the CustomAgent row remained visible.
- **H — Partial:** Clicking the team `Archive` button triggered the native-dialog path with no custom DOM modal, but native OK automation was blocked as in F. After sending the archive command over the protocol client, the team remained visible with an `archived` badge.

### Blockers / concerns

- **Blocker:** The roster sidebar still does not render for a fresh manager draft tab, so scenario C and the spec expectation that opening a team shows the roster before first activation are not satisfied. It does render once the manager has a live binding.
- **Concern:** Native bridge confirms are hard to complete through `tyde_debug_evaluate` alone. In this run, macOS accessibility restrictions blocked scripted OK clicks, so F/H needed a protocol fallback after confirming no custom modal was used.
- **Concern:** CustomAgent delete rejection is visible, but the copy is still UUID-oriented (`referenced by team member <uuid>`) rather than naming the team/member for users.
- **Harness note:** The parent app's installed debug MCP protocol mismatch can make future branch QA look like a startup timeout unless the branch-local debug MCP is used.

## Extensive QA run on 2026-05-13 (post-polish: 9a7751d..83c0858)

Reviewer: Codex (manual driver via branch-local `tyde-dev-driver debug` MCP)
Branch: `feat/agent-teams`
HEAD: `83c0858 style: cargo fmt cleanup from wizard refactor`
Dev instances observed before the run was cut off: smoke/probe/create attempts used
fresh temp stores under `/tmp/tyde-agent-teams-*`; successful verification pass used
instance `df179f5aebd14e028484226eef87b82b` and was stopped cleanly. A later
follow-up instance `b65c1b3e02674fd2b824e7f1dda3dbed` timed out while sending the
manager's first message; per the follow-up note the dev instance is no longer
running. The test harness used a fake local `tycode-subprocess` under the temp
`HOME` so message activation was deterministic and did not hit real AI APIs.

Build note: `cargo build -p tyde -p tyde-dev-driver` cannot run on this branch
because there is no package named `tyde`; I built `cargo build -p tauri-shell -p
tyde-dev-driver`, which completed successfully.

### Scenario results

1. **3-step wizard — Pass.** Verified the wizard rendered three discrete screens:
   `New team — name`, `New team — manager`, and `New team — reports (optional)`.
   Empty name stayed on step 1 with `Team name is required.`; incomplete manager
   stayed on step 2 with `Member name is required.`; step 3 allowed adding a
   report and finishing.
2. **Roster on draft tab — Pass.** After creating `QA Core Team`, clicking the
   team opened a `QA Manager` draft chat and immediately rendered
   `.team-roster-sidebar` with `QA CORE TEAM`, `QA Report`, `Report`, `idle`,
   `QA Fake Agent`, `Project B`, and `/tmp/tyde-proj-b` before any message.
3. **First message activates manager — Partial.** I opened the draft manager tab
   and attempted to send `hello`. The verification call timed out waiting for the
   fake backend response, so manager activation was not conclusively verified.
4. **Open report from roster — Not reached.** The run was cut off before report
   roster click/spawn verification completed.
5. **Multiple teams — Not reached.**
6. **Cross-project teams — Partial.** The single verified team had manager roots
   `/tmp/tyde-proj-a` / `Project A` and report roots `/tmp/tyde-proj-b` /
   `Project B`. Opening the manager switched visible project context to project
   A (`tyde-proj-a` in the file/git area), and the roster showed the report's
   `Project B`, but report-click project switching was not reached.
7. **Empty team — Not reached.**
8. **Set manager (promote a report) — Not reached.**
9. **Edit member — Not reached.**
10. **Archive a report — Not reached.**
11. **Try to archive the active manager — Not reached.**
12. **Try to archive a live-bound member — Not reached.**
13. **Reject CustomAgent delete with named blocker — Not reached.**
14. **Reject Project delete with named blocker — Not reached.**
15. **CustomAgent delete succeeds after archive — Not reached.**
16. **Cancel mid-wizard — Not reached.**
17. **Back button — Not reached.**
18. **Multiple reports — Not reached.**
19. **Restart dev instance, verify state survives — Not reached.**
20. **Manual JSON inspection — Partial.** Store writes were indirectly verified
   when the Teams panel showed `QA Core Team2 members` with manager/report rows.
   I did not complete invariant inspection of `agent_teams.json` before cutoff.
21. **Activation race — Not reached.**
22. **Resume failure — Not reached.**

### Evidence captured

Representative `tyde_debug_evaluate` sequence used during the successful pass:
open Teams, click `+ New team`, assert `New team — name`, click `Next` with an
empty name, fill `QA Core Team`, fill manager fields with `QA Manager`,
`QA Fake Agent`, `/tmp/tyde-proj-a`, `Project A`, add report `QA Report` with
`/tmp/tyde-proj-b`, `Project B`, then `Finish` and click the team title.
Observed team card text included:

- `QA Core Team2 members`
- `QA ManagerManageridle` / `QA Fake Agent/tmp/tyde-proj-a`
- `QA ReportReportidle` / `QA Fake Agent/tmp/tyde-proj-b`

Opening the manager produced visible text including:

- `QA Manager`
- `Send a message to start a conversation`
- roster: `QA CORE TEAM`, `QA Report`, `Report`, `idle`, `QA Fake Agent`,
  `Project B`, `/tmp/tyde-proj-b`

### Blockers

- None conclusively found in the scenarios completed before cutoff.

### Concerns

- The first-message activation verification timed out in the manual harness.
  Because the draft roster and wizard already worked, this may be harness/script
  fragility rather than an app bug, but I did not gather enough evidence to
  classify it.
- Custom agents/projects were slow to appear in the wizard selects immediately
  after opening the manager step; waiting for host replay populated them. This is
  probably expected startup replay latency, but it is user-perceivable if the
  wizard is opened immediately after connect.

### Recent-fix verification status

- `7e79e1b` 3-step wizard: **verified pass**.
- `9a7751d` roster sidebar on draft manager tabs: **verified pass**.
- `245d2a1` named CustomAgent/Project delete blockers: **not reached**.
- `9cc1c2c` typed activation errors: **not reached**.

### Recent-fix verification (test-level, post-QA-cutoff)

Since the UI-driven QA was cut off before reaching scenarios 13-22, the
remaining two recent fixes were verified by reading the updated test
assertions and function signatures rather than by driving the UI:

- `245d2a1` named CustomAgent/Project delete blockers — **verified via test**.
  `server/src/host.rs::team_references_block_custom_agent_and_project_delete`
  now asserts the error message contains `custom agent "Team Custom Agent"`,
  `team "Product Team"`, and `team member "Manager"`/`"Report"` (display names),
  and explicitly asserts the message does NOT contain the raw UUIDs of
  `manager.id` or `report.id`.
- `9cc1c2c` typed activation errors — **verified via signature**.
  `activate_team_member` returns `AppResult<TeamMemberMessageOutcome>`
  (with `const OPERATION: &str = "team_member_activate"`); the router
  callsite is plain `.await?;` with no `.map_err` translation hack.

### Follow-ups

- TOCTOU between team-validation refs and concurrent deletes is fixed by this `fix(teams):` commit; see `create_member_and_delete_custom_agent_serialize` and `create_member_and_delete_project_serialize`.
