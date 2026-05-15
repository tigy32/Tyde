## Summary

- The team state plumbing is mostly correct: the frontend has host-scoped
  `Team`, `TeamMember`, and `TeamMemberBinding` signals, and dispatcher
  handlers apply both `Upsert` and full-record `Delete` notifications.
- The main UI implementation does **not** satisfy §9.2: clicking a team opens
  a new `Team` tab with an embedded `ChatView`, rather than opening the
  manager's normal chat/session-resume view.
- Newly-created teams are effectively unusable from the frontend because the
  no-session manager path renders an explanatory dead-end instead of activating
  or opening a chat path where the user can send the first message.
- Several actions are tied to ambient `selected_host_id`, so already-open team
  views can operate on the wrong host after host selection changes.
- `cargo check --workspace` and `tools/run-wasm-tests.sh` pass, but the new
  wasm tests are too coupled to CSS/internal structure and one encodes the
  wrong Team-tab behavior.

## Findings

1. **blocker** — `frontend/src/components/teams_panel.rs:220-222`,
   `frontend/src/components/teams_panel.rs:893-909`,
   `frontend/src/components/team_view.rs:147-152`,
   `frontend/src/state.rs:742-748`,
   `frontend/src/components/chat_input.rs:486-518`

   Clicking a team creates `TabContent::Team` and embeds `ChatView` inside
   `TeamView`. That is not the same UI/path as a normal chat opened from the
   Sessions tab. It also breaks the visible chat controls: `active_agent` is a
   memo over only `TabContent::Chat`, so while the Team tab is active,
   `ChatInput` and other active-agent actions see `None` even when the manager
   `ChatView` has an `agent_ref`. Send/queue/interrupt/review controls can be
   disabled or target no agent while a manager chat is shown.

   **Recommended fix:** Remove the separate Team chat tab path. Team clicks
   should open/focus a normal `TabContent::Chat` for the manager `AgentId`,
   reusing the existing session-resume/new-agent flow. If a roster sidebar is
   needed, project it alongside that normal chat view from team signals rather
   than mounting a second chat wrapper.

2. **blocker** — `frontend/src/components/team_view.rs:154-173`,
   `frontend/src/components/teams_panel.rs:940-974`

   Members without a live binding and without a `session_id` have no usable
   first-activation path in the frontend. For the manager, `TeamView` renders
   "Activation requires ... and is not yet exposed in the UI." For reports,
   `open_member_chat` falls back to opening the team tab. A freshly-created
   team starts with a manager record but no session, so clicking it does not
   open a manager chat as §9.2 requires.

   **Recommended fix:** Add/reuse the intended team-member activation path so a
   user can open the manager's chat and send the first message. Do not spawn a
   plain user-origin agent as a workaround; the resulting chat must remain tied
   to the `TeamMember` server state and subsequent `TeamMemberNotify` /
   `TeamMemberBindingNotify` updates.

3. **blocker** — `frontend/src/components/team_view.rs:88-118`,
   `frontend/src/components/teams_panel.rs:940-968`,
   `frontend/src/components/sessions_panel.rs:214-250`

   Team manager/report resume logic is duplicated instead of reusing the
   Sessions resume path. The duplicated code sends `SpawnAgent(Resume)`
   directly and omits the existing behavior that switches active project
   context before the `NewAgent` echo arrives. This is exactly the path §9.2
   says should be shared.

   **Recommended fix:** Extract a shared resume helper from `sessions_panel.rs`
   (or move the existing logic to an action module) and call it from team
   manager/report opens. That helper should be the single owner of project
   switching and the typed `SpawnAgentPayload { params: Resume { .. } }` send.

4. **concern** — `frontend/src/components/team_view.rs:120-124`,
   `frontend/src/components/teams_panel.rs:912-923`,
   `frontend/src/components/teams_panel.rs:977-1020`

   Several team/member actions look up the host from
   `state.selected_host_id.get_untracked()` at click time. That is not safe for
   already-open team tabs: if the user changes the selected host, clicking a
   report in an existing team view looks in the newly selected host's member
   and binding maps instead of the tab's `host_id`.

   **Recommended fix:** Thread the rendered record's `host_id` through
   `open_team_tab`, `open_member_chat`, set-manager, and archive-member/team
   actions. Host-scoped IDs should never rely on ambient selection after the
   view has been opened.

5. **concern** — `frontend/src/components/teams_panel.rs:533-606`

   The New Team flow only captures team name plus the manager spec and closes
   after `TeamCreate`. The spec calls for a wizard: name → manager creation →
   optional reports. Reports can be added later, but there is no optional
   report step in the creation flow.

   **Recommended fix:** Add the optional reports step. Keep the implementation
   server-driven: send `TeamCreate`, wait for the server echo that provides the
   real `TeamId`, then send typed `TeamMemberCreate` commands for any reports
   the user chose to add.

6. **concern** — `frontend/src/components/teams_panel.rs:257-272`,
   `frontend/src/components/teams_panel.rs:843-865`

   Team archive uses a local `ConfirmDialog` component. The implementation
   avoids forbidden `window.confirm`, but the prompt specifically requires
   destructive archive operations to use `crate::bridge::confirm_dialog`.
   Member archive and set-manager already use the bridge dialog; team archive
   should be consistent.

   **Recommended fix:** Replace the local team-archive modal with the same
   async `crate::bridge::confirm_dialog` pattern used by `archive_member`.

7. **concern** — `frontend/src/components/teams_panel.rs:306-341`,
   `frontend/src/components/team_view.rs:51-66`,
   `frontend/src/components/team_view.rs:212-230`

   Archived members remain in member counts and rosters. The backend protocol
   soft-archives a member by emitting `TeamMemberNotify::Upsert` with
   `state = Archived`; the spec's failure-mode text says the member is removed
   from the roster. The current filters include every member with the matching
   `team_id`, so archived reports/managers stay visible and counted.

   **Recommended fix:** Exclude `TeamMemberState::Archived` from roster and
   member-count derivations unless there is a deliberate separate archived
   history surface.

8. **concern** — `frontend/src/components/teams_panel.rs:1114-1298`,
   `frontend/src/components/team_view.rs:456-554`

   The added wasm tests pass, but several assert CSS classes, DOM identity, or
   internal state rather than user-perceivable behavior: `.team-card`,
   `.team-member-status`, `.team-roster-card-status`, `is_same_node()`, and
   direct inspection of `state.center_zone`. The `opening_team_creates_team_tab`
   test also locks in the spec violation that clicking a team creates a
   `TabContent::Team`.

   **Recommended fix:** Rewrite these tests around visible text, accessible
   affordances, rendered counts, and the user-visible chat-opening behavior.
   The team-open test should assert that the manager's normal chat opens or
   resumes, not that a private tab enum variant exists.

9. **nit** — `frontend/src/components/teams_panel.rs:893-901`

   `open_team_tab` snapshots the team name into the tab label. If a later
   `TeamNotify::Upsert` changes the name, the team card updates but the tab
   label does not. This is another consequence of storing a separate Team tab
   rather than rendering purely from the server-backed team record.

   **Recommended fix:** If the Team tab survives, derive its displayed label
   reactively from `state.teams`; preferably remove the Team tab and use the
   normal chat tab path.

10. **nit** — `frontend/src/components/team_view.rs:557-577`

   `format_relative_ms` reads the client clock during render. It does not set
   up a timer, but unrelated rerenders can change "last active" text even when
   the server has emitted no team/binding change. That is a small wrinkle under
   the pure-projection rule.

   **Recommended fix:** Render a stable server timestamp, or introduce an
   explicit clock signal if relative time is intended as independent
   presentation state.

## What's right

- `frontend/src/state.rs` adds the expected host-scoped `teams`,
  `team_members`, and `team_member_bindings` signals and clears them on host
  disconnect.
- `frontend/src/dispatch.rs` routes `TeamNotify`, `TeamMemberNotify`, and
  `TeamMemberBindingNotify`; all three handle both `Upsert` and full prior
  record `Delete` payloads.
- The core team/member lists use stable IDs as `<For>` keys, and row
  components re-look-up records through `Memo`s instead of rendering the
  snapshotted `<For>` item fields.
- Team/member mutations go through typed protocol sends (`TeamCreate`,
  `TeamArchive`, `TeamSetManager`, `TeamMemberCreate`, `TeamMemberUpdate`,
  `TeamMemberArchive`).
- I found no `window.confirm`/`alert`/`prompt` or
  `web_sys::Window::confirm_with_message` usage in the new team frontend code,
  and no implementation `unwrap`/`expect` on team state lookups.

## Build/test results

- Ran `cargo check --workspace` at HEAD: passed.
- Ran `tools/run-wasm-tests.sh` at HEAD: passed, 51 tests passed.
- I read the new `wasm_tests` modules in `teams_panel.rs` and `team_view.rs`;
  they pass, but the assertions need the fixes noted above to match the
  project's user-perceivable/inviolable test philosophy.
