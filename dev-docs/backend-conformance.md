# Backend conformance at the trait boundary

The backend conformance suite is `tests/tests/conformance2.rs`. Its harness calls the production `Backend` trait directly. It runs real installed providers and checks normalized events, native history, settings, and filesystem results. Server protocol behavior belongs in the mock-server simulations.

The 31 scenarios below retain the original scenario names. Native-provider configuration and filesystem regressions also retain their original provider-specific oracles. Capability exclusions come from backend declarations; they are not passing backend runs.

## Migration coverage

Passing real runs recorded on 2026-09-09; `real_slash_commands` was recorded on 2026-09-14 (Claude, Codex, Antigravity) and 2026-09-15 (Kiro, Grok, OpenCode, Hermes); `real_mid_turn_steering` was recorded on 2026-09-20. A dash means the backend does not declare the case’s required capabilities or native skill delivery. The existing Codex configuration case applies only to Codex.

| Scenario | Claude | Codex | Kiro | Antigravity | Grok | OpenCode | Hermes |
|---|---|---|---|---|---|---|---|
| `real_codex_global_settings` | — | Pass | — | — | — | — | — |
| `real_conversation` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_skills` | Pass | Pass | — | Pass | — | — | — |
| `real_image_input` | Pass | Pass | Pass | — | Pass | Pass | Pass |
| `real_session_settings` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_session_speed` | Pass | Pass | — | — | — | — | — |
| `real_tool_type_mappings` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_interruption` | Pass | Pass | — | Pass | Pass | — | Pass |
| `real_mid_turn_steering` | Pass | Pass | — | — | — | — | Pass |
| `real_interrupt_after_background_response` | Pass | Pass | — | — | — | — | Pass |
| `real_conversation_on_resumed_session` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_resumed_session_groups_parallel_tool_calls` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_steering_compaction_and_resume` | Pass | Pass | — | — | Pass | — | Pass |
| `real_user_question` | Pass | — | — | Pass | — | — | Pass |
| `real_watched_command_shows_every_interaction` | — | Pass | — | — | — | — | — |
| `real_background_task_outlives_its_turn` | Pass | Pass | — | — | — | — | Pass |
| `real_background_task_cancel` | Pass | Pass | — | — | — | — | Pass |
| `real_conversation_in_native_subagent` | Pass | Pass | — | Pass | Pass | Pass | Pass |
| `real_native_wait_excludes_completed_children` | — | Pass | — | — | — | — | — |
| `real_nested_subagent_ownership` | Pass | Pass | — | Pass | Pass | Pass | Pass |
| `real_native_workflow` | Pass | — | — | — | — | — | — |
| `real_usage_accounting` | Pass | Pass | — | Pass | Pass | Pass | Pass |
| `real_subscription_capacity` | Pass | Pass | Pass | Pass | Pass | — | — |
| `real_capacity_without_a_conversation` | Pass | Pass | Pass | Pass | Pass | — | — |
| `real_task_list` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_mcp_tool_call` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_mcp_slow_server_is_not_reported_unavailable` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_tyde_agent_spawn` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_agent_await_survives_a_resumed_session` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_native_goal_lifecycle` | — | Pass | — | — | — | — | — |
| `real_slash_commands` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |

Additional real regressions cover Claude’s native session location, Codex’s abandoned and legacy dynamic await tools, Claude plan approval, and Codex discovery lifecycle. The lifecycle test starts the real CLI and uses a transparent proxy to delay or disconnect transport; it supplies no provider responses.

Server-only guarantees remain in `server/tests/session_resume.rs`: history paging and bootstrap barriers, server session-list metadata, and idle/busy compaction admission with a single correlated timeline entry. Catalog refresh and error propagation remain in `tests/tests/bootstrap.rs`, driven by typed `MockBackend::discover` results.

Normal validation is `./dev.sh check`; it builds the test binary and MCP bridge without running paid cases. Real cases are ignored and additionally require `TYDE_RUN_REAL_AI_TESTS=1`; `TYDE_REAL_BACKENDS` selects providers. Follow the authorization rules in `AGENTS.md` before running them.

## Resumed native-goal running state

`real_resumed_native_goal_reports_running` requires `NativeGoals` and
`ResumeSession`; Codex is currently the only eligible backend. It executes an
unfinished native goal, closes and resumes its session, sends an ordinary
follow-up, and observes consecutive working turns before releasing the goal's
filesystem prerequisite. Every live response and tool request must arrive
while the client's last typing state is active.

The transparent CLI fixture forwards genuine provider traffic unchanged,
holding the `thread/resume` reply until the provider's real `turn/started`
notification has been forwarded. It retains proof of that ordering and fails
if the provider never produces the trigger. It fabricates no provider events,
responses, or command outcomes. Private run logs and content-free transport
proof are retained in the reported `tyde-real-resume-race-*` directory.

On 2026-09-22, two uncontrolled baseline runs passed because they did not hit
the startup interleaving. The controlled unfixed run failed in 29.15 seconds:
after one completed working turn and an accepted follow-up, a real
`StreamStart` arrived while the client was idle. The transport proof confirmed
the native start was forwarded before the resume reply. The integrated case
reproduced the same failure in 32.74 seconds.

The fixed Codex adapter passed the unchanged controlled case in 70.29 seconds
on 2026-09-22, including the same proven start-before-reply interleaving, two
working turns after an accepted follow-up, and completion of the real goal.
Resume initialization and history replay now finish before inbound events can
mutate the resumed live state. Ordinary turn, streaming, and tool mapping are
unchanged outside that resume boundary.

## Ordered resume replay boundary

Every successful `Backend::resume` stream now contains exactly one internal
`BackendEvent::ResumeReplayComplete(Ok(()))`, after history and before live
events. Codex places it before releasing its inbound guard; ACP holds its
inbound gate through replay flush, marker emission, and the switch to live
handling. Claude emits it after explicit history loading, before starting the
CLI; process readiness does not wait for live turns to finish. Hermes,
Antigravity, and the mock emit it before forwarding or accepting live work.
Grok and OpenCode share the ACP producer. Asynchronous resume startup errors
use the marker's `Err` form. No client protocol or frontend inference is
involved.

The actor settles replay inline, preserves its 30-second deadline and fatal
close handling, and never drains a temporarily empty queue to infer the
boundary. Its idle publication cannot clear an active registry turn or its
stream. Binding preparation and the conformance harness likewise consume
only through the marker, leaving subsequent live events unread.

## Generated-image response ownership

`real_generated_image_preserves_tool_ownership` runs on backends declaring
`GenericGenerateImage`. It asks for native image generation alongside a real
shell command, then decodes the generated image and checks command output, typed image
completion, and exactly one durable response owner and terminal outcome for
every tool request. Codex and Antigravity currently declare that capability.

The Codex profile for this case uses `gpt-6-astra` with low reasoning effort.
Luna runs missed the triggering interleaving or failed the requested file/poll
orchestration. Astra reproduced the original bug on 2026-09-19: image completion
at 16:50:36.561 UTC was followed by empty reasoning at 16:50:36.769. The adapter
then redeclared the command under a different message owner, with unchanged
name, arguments, and offset. The durable-owner assertion failed with zero
owners for that command. This establishes the adapter ownership regression
independently of command execution success.

The final prompt keeps the marker command running for 60 seconds and copies
its native image in a separate tool call. The earlier 15-second command could
finish before generation, missing the overlap. With the same final test on
both builds, the unfixed adapter failed at 21:29:49 UTC with another owner
conflict and zero durable owners. The fixed adapter passed after consuming
empty reasoning at 21:27:43 with one pending image and one pending tool; it
retained that response's owner in the persisted message, including its image
and command declaration. The same final scenario also passed on Antigravity.
Codex resume/grouped-tool and native-child scenarios passed, and disposable
live UI QA retained the image and completed cards through reload and fork.

The initial PNG-signature assertion rejected Antigravity's real 1024×1024 JPEG
(`blue_square_1789837123865.jpg`), although native generation and its typed
completion succeeded. `GenericGenerateImage` does not specify PNG encoding.
The oracle now fully decodes the provider's image instead of accepting only a
PNG header; the unique-owner and completion assertions are unchanged.

For the Codex contentless-reasoning regression, retain provider notification
traces and confirm the image completion, command declaration, and empty
reasoning completion interleaved before the provider-response boundary. A
successful run without that trigger does not establish regression coverage.
The unfixed baseline must fail the ownership assertion or report the protocol
violation before the fixed run can certify the fix. Live UI/reconnect QA also
checks retained image rendering; tool-generation capability alone does not
promise that every backend exposes image attachments in assistant messages.
Paid runs require separate approval under `AGENTS.md`.

## Exhausted-account regression

`real_exhausted_account_stays_open` requires an authenticated real account
whose balance or usage quota is already exhausted for the selected model.
Set `TYDE_REAL_ACCOUNT_EXHAUSTED=1` in addition to the normal paid-run opt-in.
Missing exhaustion is a failure; this scenario never spends down an account
or injects a provider error. It submits two small prompts in the same session,
requires a capacity rejection and idle state for each, and checks that the
backend event stream remains open after each rejection. The scenario is
registered for every supported backend with identical setup and assertions.
Real runs, including a baseline run without the fix, still require approval.

## Workspace relocation

Real runs on 2026-09-11 used the same filesystem and retained-history
assertions for every eligible backend:

| Scenario | Claude | Codex | Hermes | Kiro | Grok | OpenCode | Antigravity |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `real_workspace_relocation` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_multiple_workspace_relocation` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |

Models were Claude Haiku 4.5 through OpenRouter, Codex `gpt-5.6-luna`, Hermes
`openai/gpt-4.1-mini` through OpenRouter, Kiro's `auto` selection, Grok
`grok-4.6`, OpenCode `opencode/mimo-v2.5-free`, and Antigravity
`Gemini 3.7 Flash (Medium)`.
Claude used isolated configuration trusting only disposable fixture roots.
Hermes's default DeepSeek model repeatedly continued reasoning after correct
file operations and timed out; the passing run used the existing
`TYDE_HERMES_TEST_MODEL` override without changing assertions.

The first Codex runs failed because loaded-thread `thread/resume` ignored
cwd/root overrides. The first Hermes run failed because its native session
remained busy during cleanup after Tyde's idle event. These real failures
establish red coverage for the corresponding fixes.

The remaining providers first failed this same scenario: Grok could not find
its directory-scoped transcript; aliasing it alone then left `pwd` in the
original directory. OpenCode retained the original cwd after both ACP reload
and process restart. Antigravity retained old workspace URIs in its native
trajectory despite different launch roots; `--new-project` was ignored when
resuming. These failures establish red coverage for the native metadata
relocation paths documented in `06-projects.md`.

After the fixes, Grok, OpenCode, and Antigravity passed the unchanged shared
single-root scenario, including retained conversation context, exact file
locations, rejected inputs, and the return move. Antigravity additionally
passed the two-root scenario. Kiro passed the single-root scenario again to
cover the shared ACP transport changes. That initial implementation declared
relocation capability on all seven,
with multiple-root capability on Codex and Antigravity.

The expanded multi-root lifecycle now checks startup, reads and writes in
all roots, exact root inventories, invalid additional roots, process resume
with unchanged visible user messages, removing extra roots while retaining
the primary cwd, and returning to the original workspace. It runs against
every backend declaring workspace relocation. Expected root counts, root
paths, and source-file contents are never supplied by the test's user prompts. Removing extra roots is followed by
an immediate process resume before sending the next prompt, so old root
context cannot survive unnoticed in conversation history.

Baseline live runs rejected multiple roots on Hermes, Kiro, Grok, and
OpenCode. The expanded startup check also found that Codex omitted runtime
roots from thread creation, despite already supporting multi-root relocation.
Claude's first baseline run was blocked by expired local OAuth credentials;
a subsequent run through the documented OpenRouter gateway reproduced the
one-root rejection with the real Claude CLI and Claude Haiku 4.5.

The stronger lifecycle then exposed stale Claude root context after removing
an extra directory and resuming, OpenCode exports truncated at exactly
65,536 bytes when piped, and Antigravity empty-cwd terminals alternating
between project roots. These are covered by the unchanged filesystem and
root-inventory assertions. All seven passed both expanded scenarios after
these fixes; Antigravity also passed `real_skills` to cover its retained native
skill projection. Grok relocation remains local Unix only.

Claude's gateway reports the same pinned Haiku 4.5 model as
`anthropic/claude-haiku-4.5`; the test accepts that exact alias alongside the
native CLI aliases. Grok's generated script repeatedly wrote literal
backslash-n text, so its prompt explicitly requests LF bytes. The common
filesystem assertions are unchanged. A Codex marker-copy failure and an
OpenCode turn timeout passed on unchanged retries; these failures are not
counted as successful runs.

Final passing evidence is retained in `/tmp/tyde-all-roots-live-furaasrr`
(Hermes and Kiro, both scenarios; Codex single-root),
`/tmp/tyde-all-roots-live-kkcnycoy` (Codex multi-root, OpenCode single-root,
Antigravity both scenarios and skills), `/tmp/tyde-all-roots-live-66po20s1`
(OpenCode multi-root), and `/tmp/tyde-all-roots-live-6gkhyp5u` (Claude and
Grok, both scenarios). These are targeted real-provider runs, not a run of
the complete conformance suite.

## Live context usage during tool loops

`real_usage_accounting` checks that context occupancy reaches the event stream
between sequential file reads, through either request telemetry or message
metadata. Counting distinct occupancy values only after the turn finishes does
not establish that the live context bar can update.

The OpenCode baseline on 2026-09-11 failed this assertion: the chain's four
request observations all arrived at chat-event position 27, with none between
tool requests at positions 7 and 13. OpenCode now publishes request usage and
advances cumulative usage when each request is recorded. Turn completion
updates the final message metadata without publishing or counting requests a
second time. The request diagnostic includes its identity and occupancy.

The fixed OpenCode run (`opencode/mimo-v2.5-free`) passed: context observations
arrived at chat-event positions 10, 16, 22, and 29, interleaved with tool
requests at positions 7, 13, and 19. The existing token-accounting assertions
also passed.

The extended scenario passed on Claude (Haiku), Codex (`gpt-5.6-luna`),
Antigravity, Grok (`grok-4.6`), and Hermes (`openai/gpt-4.1-mini` through
OpenRouter). Claude's first attempt failed at the opening handshake because
its OAuth session had expired; the approved retry passed after reauthentication.

## Running commands across response retries

`real_running_command_survives_response_retry` exercises the real CLI through
an isolated loopback HTTP proxy. The proxy forwards genuine upstream traffic
unchanged, then closes one response stream after a real tool-call item and
before its response-completed event. It supplies no fabricated provider events.
The case requires a real retryable `responseStreamDisconnected` notification,
a recovery request before the command's completion write, exactly one command
execution, exactly one successful typed completion, and a working follow-up
turn. A healthy run without the injected disconnect cannot pass.

The case requires `YieldsRunningCommands`; Codex is currently the only eligible
backend. Its transport fixture uses the authenticated Codex subscription
endpoint and temporarily isolates `CODEX_HOME`, preserving the installed login
without changing the user's configuration. Python 3 is required. Fixture
metadata and private provider diagnostics remain in the printed temporary
directory; credentials copied into the isolated home are removed afterward.
Do not share the raw diagnostic log without redacting it.

On 2026-09-22, the unchanged adapter failed this exact case: the command ran
once and finished its filesystem write, but its tool card retained the
synthetic failure instead of the genuine successful result. The fixed adapter
passed with one real disconnect, one retry before command completion, one
successful completion, and zero conflicting outcomes. This reproduction caught
the premature terminalization; unlike the reported production incident, it did
not emit a conflicting-duplicate warning. The fix distinguishes retryable
response failure from abandoned execution, retaining pending tool ownership
without concealing the failed response or changing terminal teardown.
Codex's existing `real_background_task_outlives_its_turn` and
`real_background_task_cancel` also passed against the fixed adapter, covering
normal late completion and explicit cancellation beside the retry path.
