# Backend Dev-Instance Manual QA

Use this workflow to certify a real agent backend through the rendered Tyde
desktop application. This is an end-to-end product audit, not a smoke test and
not a substitute for deterministic backend tests or `./dev.sh check`.

Real backend turns can spend money. Obtain explicit approval before starting
them. A certification run intentionally favors coverage over cost, but every
prompt and command must still be bounded.

## Certification contract

A run certifies one exact combination of Tyde commit, backend CLI version,
provider, model, access mode, reasoning level, host OS, and frontend build. A
different combination is a different run.

Do not infer one visible state from another. Seeing correct final output does
not prove that the agent was shown as active while it ran. Seeing a completed
tool card does not prove that it was shown as running first. Every temporal
claim below must be observed while that state is held and again after its
transition.

Use bounded sleeps or other harmless barriers to make transient states last
long enough to inspect. If a required state passes too quickly to observe, the
case is **not tested**; rerun it with a longer bounded barrier. Never mark a
case passed from the final state alone.

For every applicable case, inspect all rendered surfaces that expose it:

- the agent card in the sidebar and Agents view;
- the active/finished smart-view membership and status icon, label, and style;
- the open chat, streaming/reasoning area, input, stop control, and header;
- the tool, task, sub-agent, workflow, or in-flight card;
- the in-flight tray;
- a second connected client when the case calls for one.

An unsupported capability may be marked `N/A` only after recording why the
backend or selected mode cannot expose it. An unattempted applicable case is a
failure, not `N/A`.

## 1. Prepare and record the run

1. Confirm the backend CLI is installed and starts successfully outside Tyde.
2. Configure provider credentials and model in the same environment from
   which the Tyde host will launch.
3. Confirm the backend is enabled in **Settings → Backends** and Tyde reports
   the expected installed version.
4. Use a disposable workspace. Do not point destructive tool tests at a real
   project.
5. Record:
   - Tyde commit and whether the tree is clean;
   - backend and CLI version;
   - provider and model;
   - access mode, reasoning level, and backend-native settings;
   - host OS and architecture;
   - frontend URL, viewport, and browser version;
   - run start time and tester.
6. Build a capability matrix before testing. Include text, reasoning, file
   read, file write, foreground command, background command, cancellation,
   permission approval, user question, task list, image input/output,
   backend-native sub-agents, Tyde-managed agents, resume, fork, and
   compaction. Mark each `required` or `unsupported` with a reason.
7. Prepare unique marker strings for every prompt, command, file, and child.
   Reusing a marker can hide duplicated or misrouted events.

## 2. Start a clean Tyde dev instance

1. Call `tyde_dev_instance_start` with the repository root as `project_dir`.
2. Keep the returned `instance_id`; every later debug call must use it.
3. Open the returned `frontend_url` and wait for the home screen to finish
   loading.
4. Check the initial console and rendered UI for startup errors.
5. Confirm no prior agents, chats, tool cards, or in-flight rows from another
   run are present in the disposable store.
6. If code changes during the test, stop the instance and start a new one.
   Dev instances intentionally do not hot-reload.

Use `tyde_debug_evaluate` for DOM inspection and ordinary browser input for
clicks, typing, scrolling, and screenshots. Assertions must be based on the
rendered UI. Protocol state and logs may explain a failure, but they must not
replace the user-visible check.

Keep the console log from the entire run. A credential, secret, or unredacted
private tool payload in logs, screenshots, DOM, or error text is an immediate
failure.

## 3. Enforce the agent lifecycle oracle

The lifecycle checks in this section apply continuously to every later
section. Record a timestamped status ledger for each root agent and child.

| Held condition | Required visible agent state |
| --- | --- |
| Created but backend startup not acknowledged | Initializing, never completed |
| Turn accepted but no output yet | Thinking/active |
| Reasoning or assistant text streaming | Thinking/active |
| Assistant phase ended because it requested a tool | Thinking/active |
| Foreground tool executing | Thinking/active |
| Between tool completion and the next assistant phase | Thinking/active |
| Native child still running after its parent phase ends | Child Thinking/active |
| Waiting for user question or permission | Explicit waiting/attention state, not completed |
| Backend reports authoritative turn completion | Idle/completed |
| User cancels | Cancelling, then one terminal cancelled/idle state |
| Backend fails | Failed/error with a visible explanation |
| Agent closes | Terminated/removed consistently on every client |

The exact label may differ by surface, but meaning must not conflict. A check
icon, completed styling, finished-only placement, enabled Compact action, or
absence from the Active view all count as a completed claim. None may appear
while authoritative backend work is still running.

For every transition:

1. Capture the status immediately before starting the action.
2. Start the action and hold it for at least five seconds.
3. While held, capture the agent card, chat controls, relevant live card, and
   Active view membership. Confirm all surfaces agree that work is active or
   explicitly waiting.
4. If the turn has multiple assistant/tool phases, capture the gap after each
   `StreamEnd`/tool request and while the tool is running. A phase boundary is
   not a turn boundary.
5. Allow the authoritative completion event to arrive. Capture the terminal
   state and confirm it changes exactly once.
6. Wait another five seconds. Confirm no late event returns the card to active,
   changes success to failure, duplicates completion, or alters final output.
7. Repeat the active-to-completed transition on a subsequent turn. Startup-only
   correctness does not certify reused-session behavior.

Use a ledger like this for every agent:

```text
time | action/barrier | sidebar | Agents view | chat/input | live card | result
```

Any interval in which an agent is visibly completed while its backend work is
still running fails the backend immediately, even if the final answer is
correct.

## 4. Run baseline text and reasoning turns

1. Create a new chat with the backend and explicitly select the intended model
   and settings.
2. Immediately send a unique short prompt. This exercises input arriving close
   to agent bootstrap.
3. Verify the complete lifecycle oracle: Initializing if observable, then
   Thinking, then completed only after the response finishes.
4. Verify in the rendered chat:
   - the user message appears once;
   - the assistant response appears once and finishes normally;
   - the response shows the actual backend and model;
   - streaming text is ordered and never rewrites already finalized text;
   - no warning, identity error, duplicate, or empty assistant placeholder
     appears;
   - Stop is available only while cancellation is meaningful;
   - input enablement matches whether the backend accepts or queues another
     message.
5. Send a second unique prompt. Confirm it uses the same session, produces one
   new response, does not alter the first response, and repeats the correct
   active-to-completed lifecycle.
6. Run a reasoning-heavy prompt with a bounded final answer. Confirm reasoning
   appears only in the intended treatment, remains associated with the correct
   message, and does not let the agent appear completed before finalization.
7. Send an empty/whitespace submission and verify it is rejected locally
   without creating a user message, assistant placeholder, or backend turn.

## 5. Verify all token-usage surfaces

Perform these checks after the first completed turn, after the second, after a
tool-heavy turn, and after sub-agent work. Use screenshots and
`tyde_debug_evaluate` to capture visible text. A present-but-empty element is a
failure.

### Per-message usage

1. Inspect the footer of every completed assistant message.
2. Confirm it contains positive request usage such as `↑N` and `↓N`, not
   zeroes, blanks, `usage unavailable`, or a value copied from another message.
3. When the provider reports them, confirm cached-input and reasoning values
   appear in labelled forms.
4. Open the usage tooltip and confirm **Request**, **Turn**, and **Cumulative**
   scopes are labelled and plausible. Do not treat an unavailable scope as
   zero.
5. Confirm each message has its own request usage while cumulative usage is no
   smaller after later ordinary turns.
6. Confirm tool-only and reasoning-only phases attribute usage to the correct
   completed message without creating an empty row.

### Context Usage bar

1. Confirm **Context Usage** appears for the active conversation after usage
   metadata arrives.
2. Confirm it has a non-empty coloured fill rather than an empty track.
3. Open **View context usage** and verify counts and percentages are populated,
   finite, non-negative, and within the reported context-window limit.
4. Confirm the used-context value is positive and the percentage approximately
   agrees with `used tokens / context-window tokens`.
5. After each later turn, confirm the view refreshes and still refers to the
   active conversation. It need not always increase because a backend may
   compact or report a different authoritative snapshot.
6. Switch between two chats and confirm neither chat displays the other's
   context snapshot.

### Task total in Session Settings

1. Expand **Session Settings (<backend>)**.
2. Confirm the task-token control shows positive input and output totals.
3. Open **Task usage** and confirm the root agent has the expected backend and
   model. Every child must have its own row and the header must report the
   correct agent count.
4. Confirm totals are no smaller after later completed work.
5. Confirm totals are not double-counted. Compare with authoritative
   cumulative scopes instead of summing cumulative values from every message.
6. Confirm cached input and reasoning remain labelled components, not extra
   turns.
7. While a child is still active, confirm partial usage does not falsely mark
   the child completed; after completion, confirm the final total refreshes.

These surfaces answer different questions and need not show the same number:

- a message footer shows that request's usage;
- Context Usage shows the current context-window snapshot;
- Session Settings shows task-wide cumulative usage, including sub-agents.

They must nevertheless be internally consistent, correctly attributed, and
refreshed after completed work and navigation.

## 6. Exercise tools, files, and background work

Use unique markers and verify every operation while active, after completion,
and after replay.

1. **Foreground command:** ask the agent to run a harmless command containing a
   unique marker and a bounded sleep of at least five seconds before output.
   While sleeping, verify the tool card and agent are running. Afterward,
   verify the same card retains command, output, exit status, and terminal
   state.
2. **Multi-phase tool turn:** require two sequential tool calls with a bounded
   sleep between them, followed by a one-line answer. At every assistant/tool
   boundary, confirm the agent remains Thinking. It must not flash completed
   after the assistant phase that declares a tool.
3. **File read:** create a disposable file with a unique marker, ask the agent
   to read it, and confirm the typed read card, path, and returned content are
   correct and not leaked into an unrelated chat.
4. **File write:** in a mode that allows writes, ask for one bounded edit.
   Verify permission behavior, typed modify card, before/after preview, actual
   file contents, and replay. Repeat in read-only mode and confirm no write
   occurs.
5. **Background command:** start a bounded background process, continue the
   turn, then check or wait for it. Confirm the original card and in-flight row
   remain running until the process ends and transition once without acquiring
   a new identity. If the agent turn itself has authoritatively ended, the
   agent may be idle while the separate background row remains running; the two
   surfaces must clearly communicate that distinction.
6. **Non-zero exit:** run a harmless command that exits non-zero. Confirm the
   typed result includes its exit code and failure treatment without turning
   into an unrelated top-level protocol error.
7. **Large and split output:** emit bounded output large enough to arrive in
   several chunks. Confirm ordering, no truncation below the documented limit,
   clear truncation at the limit, and one terminal result.
8. **Invalid tool input:** induce one safe malformed call when practical.
   Confirm it remains a typed tool request whose completion contains the
   validation error.
9. **Overlapping work:** while a background row is running, start another
   supported bounded action. Confirm each identity, status, output, and
   completion remains independent.

Recheck usage after tool-only and reasoning-heavy turns. A turn without
assistant text must remain visible through typed reasoning or tools, but a
truly content-free completion must not create an empty chat message.

## 7. Exercise permissions, questions, cancellation, and failure

1. Trigger a harmless permission request. Confirm the agent leaves Thinking
   only for a clear waiting/attention state, the requested action and risk are
   readable, and completion styling does not appear.
2. Approve once. Confirm the control disables immediately, the agent returns to
   Thinking, the action runs once, and the final state completes once.
3. Trigger another request and deny it. Confirm no action occurs, denial is
   represented once, and the agent either continues or terminates according to
   the backend's authoritative result.
4. Trigger a native user question when supported. Confirm every option and
   free-form path works, double submission is impossible, and the answer is
   delivered once.
5. Cancel during pre-output thinking, text streaming, a foreground tool, and a
   native child run. Each must show cancelling then one terminal cancelled
   state, stop backend work, and reject late contradictory success.
6. Force one safe backend error and one tool error. Confirm each is attributed
   to the correct agent and turn, preserves prior history, and leaves controls
   recoverable.
7. After every denial, cancellation, and failure, send a normal follow-up.
   Confirm the same session remains usable unless the backend explicitly made
   it non-resumable.

## 8. Exercise native task tracking

For backends with a native task or plan list, including Claude Code and Codex:

1. Request a bounded three-step task and explicitly require native tracking.
2. Confirm all descriptions and initial statuses render as typed task state.
3. Hold each step in progress long enough to verify the active task and agent
   are both active. Prior steps must become completed without duplication.
4. Confirm the agent does not appear completed between task steps.
5. Confirm the final state has exactly three completed steps and none pending
   or in progress after the turn ends.
6. Revise the plan during a second turn. Confirm the existing view updates
   authoritatively instead of appending a stale competing list.
7. Cancel during the second step. Confirm no remaining task incorrectly shows
   completed, and agent/task cancellation states agree.
8. Leave and reopen the conversation. Confirm descriptions, order, and states
   replay identically.
9. Confirm task updates create no empty assistant rows and do not suppress
   tools or usage metadata.

Backends without native task tracking must omit the component; Tyde must not
invent a synthetic task list.

## 9. Exercise backend-native sub-agents

Test foreground and background native children separately. Give every child a
unique name, prompt marker, command marker, and final-answer marker.

### Spawn and lifecycle

1. Spawn a named child whose first action is a bounded sleep before producing
   output. The parent must immediately show a typed spawn card with child name
   and prompt.
2. Before child output starts, confirm its agent card is Initializing or
   Thinking, never completed.
3. Open the child. Confirm its initial prompt, backend, model, parent relation,
   and active state are visible.
4. Keep the child running across at least two assistant/tool phases. Inspect it
   during its first command, after the phase-ending `StreamEnd`, between tools,
   and during its final reasoning. It must remain Thinking throughout.
5. Confirm the child changes to completed only after the backend's
   authoritative child completion. The parent tool card and child agent card
   must agree.
6. Wait five seconds after completion and confirm no late event reopens the
   child or changes its result.

### Child-owned work

1. Make the child run a foreground command, start a bounded background command,
   use a native task list if supported, and finish with one unique line.
2. Confirm every tool, background transition, task update, reasoning segment,
   and final answer renders in the child's chat, not the parent's.
3. Confirm the foreground card includes command, output, exit status, and one
   terminal state.
4. Confirm the background card retains identity from running to terminal.
5. Confirm the final message appears once, is not empty, and shows the child's
   backend and model. The parent shows only typed spawn/await progress, not a
   duplicate of the child's answer.
6. Leave and reopen the child. Confirm its status, cards, tasks, final message,
   and ordering replay identically.

### Background child outliving its parent turn

1. Spawn a native child in the background and make it sleep long enough for the
   parent turn to finish first.
2. After the parent completes, confirm the child remains Thinking, remains in
   the Active view, and retains running styling. This is required even when no
   child assistant stream is currently open.
3. Navigate away and back while the child runs. Confirm it never flashes or
   settles as completed before its authoritative notification.
4. Open the child from every available **Open agent** action and confirm each
   targets the same child.
5. After the child completes, confirm parent progress, child status, final
   answer, and in-flight surfaces transition exactly once.

### Native child usage

1. While the child runs, open the parent's **Task usage** popover. Confirm the
   child is a distinct row with the expected backend and model.
2. After completion, confirm positive child input/output totals come from its
   own turns rather than zeroes or a copy of the parent.
3. Confirm the task total includes the child exactly once and the header reports
   the correct agent count.

## 10. Exercise Tyde-managed agents and mixed ownership

1. Ask the root agent to spawn two named Tyde-managed agents with different
   bounded durations, then await them.
2. Confirm both children are active concurrently, each **Open agent** target is
   correct, and the first completion does not complete the second.
3. Confirm `await` reports ready and still-thinking agents accurately at an
   intermediate barrier, not just at the end.
4. Send a follow-up to a still-running child where supported and verify it
   queues or executes according to the contract without changing identity.
5. Mix a backend-native child and Tyde-managed child in one parent turn.
   Confirm neither is flattened into a generic command, neither stream is
   dropped, and their status transitions remain independent.
6. Confirm the parent finishes once, late child completion remains attached to
   the correct request, and no foreign/duplicate identity error appears.
7. Reopen **Task usage** and confirm root, native child, and Tyde-managed
   children have separate correctly attributed rows without double-counting.

## 11. Exercise ordering, queueing, and interruption races

1. Send two unique messages rapidly. Verify FIFO processing, no loss, no
   duplicate user rows, and correct active state across the handoff.
2. Submit a message at the instant a turn completes. Repeat until the boundary
   is exercised; confirm it is neither rejected nor attached to the prior turn.
3. Cancel and immediately send a new message. Confirm late events from the
   cancelled request cannot complete or corrupt the new request.
4. Navigate between parent and child repeatedly while both stream. Confirm no
   event is routed to the active tab merely because it is active.
5. Start two children whose output interleaves. Confirm text, reasoning, tools,
   tasks, usage, and terminal state remain bound to the correct child.
6. Trigger rename immediately after chat creation and during a turn. Confirm a
   user name wins over generated naming and remains stable on all clients.
7. Repeat the highest-risk lifecycle case twice in the same session and once in
   a fresh session. A one-time pass is insufficient for an ordering-sensitive
   case.

## 12. Exercise persistence, resume, fork, and compaction

1. Leave and reopen every root and child conversation. Verify messages,
   reasoning, tools, tasks, images, model, usage, and final states replay in the
   same order without transient completed/active misclassification.
2. Reload the frontend while an agent is active. Confirm bootstrap reconstructs
   active status and open work instead of showing completion until a new event
   arrives.
3. Reload again after completion. Confirm no active state is resurrected.
4. Exercise **Resume** on completed, cancelled, and failed sessions where
   supported. Confirm history loads without blocking unrelated commands and
   the first new turn follows the lifecycle oracle.
5. Exercise **Fork** from a known point. Confirm prior history is present once,
   later source history is absent, and source/fork usage and statuses do not
   leak into one another.
6. Exercise manual or automatic compaction when supported. Confirm the agent
   shows Compacting, never completed during compaction, history remains
   coherent, and the next turn succeeds with plausible context usage.
7. Restart the dev instance against the same disposable stores and repeat
   replay checks. In-memory correctness alone is not enough.

## 13. Exercise multiple clients and connectivity

1. Open the same host in a second client before spawning an agent. Confirm both
   clients receive the same agent once.
2. While a bounded root turn and child are active, compare all status surfaces
   on both clients. Neither may lag into a contradictory completed state.
3. Disconnect one client during streaming and reconnect it. Confirm the other
   remains correct and the reconnected client catches up without duplication.
4. Disconnect or stop the host during a bounded turn. Confirm the frontend
   communicates disconnection rather than success, preserves recoverable
   history, and does not leave permanent phantom activity after restart.
5. Close an agent from one client. Confirm termination/removal reaches the
   other exactly once and no stale Active-view entry remains.

## 14. Exercise settings and supported media

1. Change every exposed per-session setting before the first turn and verify
   the response metadata reflects the selected value.
2. Change settings between turns where allowed. Confirm the new value applies
   only at the documented boundary and old messages retain their metadata.
3. Attempt a setting change while busy. Confirm it is queued, rejected, or
   applied according to the UI contract without silently changing the active
   turn.
4. Switch model where supported and verify message metadata and usage identify
   the actual model used for each turn.
5. Attach a small image when supported. Confirm preview, send, backend receipt,
   final rendering, replay, and fork behavior. Unsupported media must be
   rejected before backend spend.
6. Request image output when supported. Confirm the image and associated typed
   event both survive replay and neither replaces the other.

## 15. Presentation, accessibility, and performance audit

1. Repeat representative baseline, tool, task, and child states at narrow and
   wide supported viewports. Confirm no status, control, output, or usage value
   is clipped or hidden behind scrolling without an affordance.
2. Verify long names, paths, commands, errors, and model identifiers wrap or
   truncate with accessible full text and never obscure status.
3. Navigate essential controls by keyboard. Confirm visible focus, meaningful
   labels, correct disabled states, and no accidental double activation.
4. Confirm status is communicated by text/icon as well as color and that live
   updates are understandable without relying on animation alone.
5. During split output and concurrent children, watch for frozen input,
   excessive layout shifts, runaway scrolling, duplicate renders, or sustained
   high CPU/memory. Record measurements when suspicious.
6. Inspect the console after every major section. New warnings, parse failures,
   identity violations, panics, unhandled promises, or repeated reconnects fail
   the section even if the UI recovers.

## 16. Evidence, rerun, and cleanup

Keep an evidence index for every numbered case, not only failures:

- `PASS`, `FAIL`, or justified `N/A`;
- exact prompt and unique markers;
- before, held-active, transition, and terminal screenshots;
- relevant rendered DOM text from `tyde_debug_evaluate`;
- agent, child, tool, task, session, and instance identities where visible;
- console/log excerpt or explicit note that none appeared;
- backend, provider, model, settings, commit, and `instance_id`;
- observed timestamps and reproduction attempts.

For every failure, restart a clean instance and reproduce it once without
changing code. Preserve both attempts. After a fix, run the original case,
adjacent lifecycle cases, and then the entire certification workflow from a
clean instance. A targeted retest alone does not recertify the backend.

Finally, call `tyde_dev_instance_stop` with the saved `instance_id`. Confirm it
no longer appears in `tyde_dev_instance_list` and no backend child process from
the run remains alive.

## Pass criteria

The backend passes only when every required matrix entry has linked evidence
and every temporal state was observed while held. In particular:

- no root or child is ever rendered completed while authoritative work is
  still running;
- agent cards, Active view, chat controls, live cards, in-flight tray, and
  second clients never contradict one another;
- normal, reasoning, tool, file, background, permission, question,
  cancellation, failure, task, native-child, Tyde-managed-child, mixed,
  concurrency, replay, resume, fork, compaction, reconnect, settings, and media
  paths applicable to the backend complete without missing, duplicated,
  misordered, or misattributed events;
- every running state reaches exactly one truthful terminal state and no late
  event contradicts it;
- usage is populated, refreshed, attributed to the correct root or child, and
  not double-counted;
- persistence and clean restart reproduce the same history and terminal state;
- no unexpected warning, protocol error, identity violation, panic, secret
  leak, or orphan backend process occurs;
- all failures from the run were fixed and the full workflow then passed from
  a clean instance.

Missing evidence, an unobserved transient state, or an unexplained `N/A` means
the backend is not certified.
