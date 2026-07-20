# Backend Dev-Instance Manual QA

Use this workflow to test a real agent backend through the rendered Tyde
desktop application. This is an end-to-end product check, not a substitute for
the deterministic backend tests or `./dev.sh check`.

Real backend turns can spend money. Obtain explicit approval before starting
them, use the cheapest suitable model, and keep prompts bounded.

## 1. Prepare the backend

1. Confirm the backend CLI is installed and starts successfully outside Tyde.
2. Configure the provider credentials and model in the same environment from
   which the Tyde host will launch.
3. Confirm the backend is enabled in **Settings → Backends** and that Tyde
   reports the expected installed version.
4. Use a disposable workspace. Do not point destructive tool tests at a real
   project.
5. Record the commit under test and the backend, CLI version, provider, model,
   access mode, and reasoning level.

## 2. Start a clean Tyde dev instance

1. Call `tyde_dev_instance_start` with the repository root as `project_dir`.
2. Keep the returned `instance_id`; every later debug call must use it.
3. Open the returned `frontend_url` and wait for the home screen to finish
   loading.
4. Check the initial console/rendered UI for startup errors.
5. If code changes during the test, stop this instance and start a new one.
   Dev instances intentionally do not hot-reload.

Use `tyde_debug_evaluate` for DOM inspection and ordinary browser input for
clicks, typing, scrolling, and screenshots. Assertions must be based on the
rendered UI. Protocol state may explain a failure, but it must not replace the
user-visible check.

## 3. Run a baseline turn

1. Create a new chat with the backend and explicitly select the intended model
   and settings.
2. Immediately send a short prompt that requires a short textual response.
   This also exercises input arriving close to agent bootstrap.
3. Verify all of the following in the rendered chat:
   - the user message appears once;
   - the assistant response appears once and finishes normally;
   - the response shows the actual backend and model;
   - no warning, stream-identity error, duplicate message, or empty assistant
     placeholder appears;
   - the input is enabled again when the turn ends.
4. Send a second short prompt. Confirm it uses the same session, produces one
   new response, and does not alter the first response.

## 4. Verify all token-usage surfaces

Perform these checks after the first completed turn and again after the second.
Use screenshots and `tyde_debug_evaluate` to capture the visible text. A
present-but-empty element is a failure.

### Per-message usage

1. Inspect the footer of each completed assistant message.
2. Confirm it contains positive request usage such as `↑N` and `↓N`, not
   zeroes, blanks, `usage unavailable`, or a value copied from another message.
3. When the provider reports them, confirm cached-input and reasoning values
   appear in their labelled forms.
4. Open the usage tooltip and confirm **Request**, **Turn**, and **Cumulative**
   scopes are labelled and plausible. Do not silently treat an unavailable
   scope as zero.
5. Confirm the second message has its own request usage while cumulative usage
   is no smaller than after the first turn.

### Context Usage bar

1. Confirm the **Context Usage** bar is visible for the active conversation
   after usage metadata arrives.
2. Confirm the bar has a non-empty coloured fill rather than an empty track.
3. Open **View context usage** and verify the displayed token counts and
   percentages are populated, finite, non-negative, and within the reported
   context-window limit.
4. Confirm the used-context value is positive and the percentage agrees
   approximately with `used tokens / context-window tokens`.
5. After the second turn, confirm the view refreshes and still refers to the
   active conversation. It need not increase for every backend because some
   providers compact or report a different authoritative context snapshot.

### Task total in Session Settings

1. Expand the bottom **Session Settings (<backend>)** row.
2. Confirm the task-token control shows positive input and output totals.
3. Click it and confirm the **Task usage** popover lists the root agent with the
   expected backend and model. If the task has sub-agents, each must have its
   own row and the header must report the correct agent count.
4. Confirm totals are no smaller after the second completed turn.
5. Confirm totals are not obviously double-counted. Compare them with the
   authoritative cumulative scopes instead of adding cumulative values from
   every message. Cached input and reasoning must remain labelled components,
   not extra turns.

The three surfaces answer different questions and therefore need not show the
same number:

- a message footer shows that request's usage;
- Context Usage shows the backend's current context-window snapshot;
- Session Settings shows task-wide cumulative usage, including sub-agents.

They should nevertheless be internally consistent, populated from backend
reports, and refreshed after completed work.

## 5. Exercise tools and background work

Run only capabilities the backend supports, and verify every operation both
while active and after completion.

1. **Foreground command:** ask it to run a harmless command such as `printf`.
   Verify a typed tool card appears before completion and retains its command,
   output, exit status, and terminal state.
2. **Background command:** ask it to start a bounded sleep or harmless
   background process, continue the turn, then check or wait for completion.
   Verify the original tool card remains visible and transitions to the final
   state without acquiring a new identity.
3. **Cancellation:** start bounded work, cancel it from the UI, and verify one
   terminal cancelled state with no late conflicting completion.
4. **Invalid tool input:** induce one safe malformed call when practical.
   Verify it remains a typed tool request whose completion contains the
   validation error; it must not become a top-level red protocol error.

Recheck all three usage surfaces after tool-only or reasoning-heavy turns. A
turn without assistant text must remain visible through its typed reasoning or
tool events, but a truly content-free completion must not create an empty chat
message.

## 6. Exercise sub-agents

Where supported, test both backend-native sub-agents and Tyde-managed agents.

1. Spawn a named native sub-agent with a short prompt and wait for it.
2. Verify the parent immediately shows a typed spawn card with the child name
   and prompt.
3. Open the child and verify its initial prompt, tool activity, final response,
   backend, and model are visible.
4. Ask the parent to spawn a Tyde-managed agent, then await it through the Tyde
   agent tools.
5. Verify spawn and await cards name every affected agent and that every
   **Open agent** action opens the correct child.
6. In one turn, mix a native sub-agent with a Tyde-managed agent. Verify neither
   is rendered as a generic command and neither tool stream disappears.
7. Confirm the parent finishes once, late child completion remains attached to
   the correct request, and no foreign/duplicate identity error appears.
8. Reopen **Task usage** and confirm the children appear as separate rows and
   the task total updates without double-counting the parent.

## 7. Exercise persistence and lifecycle

1. Send two messages rapidly and verify FIFO processing with no lost input.
2. Rename immediately after creating a chat and verify the user name wins over
   generated naming.
3. Open the same host from a second client and verify the agent appears without
   waiting for the first client's attachment.
4. Leave and reopen the conversation. Verify message, reasoning, tool, image,
   model, and usage metadata replay identically.
5. Exercise **Resume** and **Fork**. Confirm history opens without blocking
   unrelated commands and the first new turn works normally.
6. If the backend supports image output, request one small image. Verify it is
   rendered in chat, survives reopen/history replay, and does not replace the
   associated typed tool event.

## 8. Collect evidence and clean up

For every failure, save:

- backend, provider, model, settings, commit, and `instance_id`;
- exact prompt and reproduction steps;
- screenshot of the rendered failure;
- relevant DOM text from `tyde_debug_evaluate`;
- whether it reproduces after a clean instance restart;
- any matching warning or error, without credentials or secret tool payloads.

Finally, call `tyde_dev_instance_stop` with the saved `instance_id`. Confirm it
no longer appears in `tyde_dev_instance_list`.

## Pass criteria

The backend passes only when the normal, tool, background, cancellation,
sub-agent, replay, resume, and fork paths applicable to it complete without
missing or duplicated UI events. Per-message usage, Context Usage, and the
Session Settings task total must all be populated, refresh correctly, and
remain present after replay.
