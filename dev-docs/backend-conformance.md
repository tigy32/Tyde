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

## Workspace relocation readiness

Workspace relocation distinguishes `Busy`, `Rejected`, and `OutcomeUnknown`.
The first two guarantee no native workspace mutation. Scheduled project entry
retains its reservation on `Busy` and retries after backend events, without
attempting restoration. Only an uncertain mutation or a failed persistence
update requires restoration; failed restoration still terminates the affected
conversation rather than continuing with inconsistent roots. Fatal internal
shutdown has no initiating client close request, so its terminalization does
not require a client reply channel.

Codex can publish idle while native background-completion wakes remain pending.
Its relocation guard and native cwd update are serialized with wake admission.
Claude's pending CLI-wake latch permits later output routing, but is not an
outstanding operation: an inline task notification can leave it armed after
all real work finishes. Actual turns, background tasks and compaction still
block relocation. A native `set_cwd` init-only sequence is metadata, not a
model turn; unsolicited turns start on real output or failure, preventing a
stale wake latch from inventing an empty assistant response during relocation.
Hermes can emit a null final text on a cancelled native
continuation; it is absent text, not malformed prose. Real failed or empty
successful responses retain their existing error reporting.

`real_workspace_move_after_background_completion` runs the same real command,
foreground overlap, busy rejection, completion-card and relocated-relative-write
oracles for every backend declaring `BackgroundTasks` and `SetWorkspaceRoots`:
Claude, Codex and Hermes. It also verifies native conversation identity and
earlier context survive the move. Filesystem timestamps bracket physical
background completion inside the real foreground command, independently of
whether the provider delivers its notification immediately or at a turn
boundary. The existing server workbench protocol flow
covers rejection without restoration, deferred entry with its reservation held,
successful restoration after an uncertain outcome, and fatal failed restoration.
Restoring the old unconditional-restoration behavior makes its rejection
assertion fail; restoring Codex's untyped busy classification makes the real
scenario's initial busy assertion fail.

The existing `real_workspace_relocation` flow additionally holds a real
foreground command with a FIFO and requires typed `Busy` with the native cwd
unchanged, then typed `Rejected` for invalid roots. Its setup and oracles are
identical for all seven `SetWorkspaceRoots` providers, including those without
background-task support. The completion-overlap flow also uses FIFO handshakes,
so correctness does not depend on model latency or host load.

The relocation flows use fresh destinations without provider-specific trust
setup. The previous `trust_fixture_workspace` helper seeded Claude's native
trust config and masked failure to enter a new project. With that helper
removed, `real_workspace_relocation::claude` reproduced the native
`needs_trust` rejection at `Backend::set_workspace_roots`. The Claude adapter
now supplies native trust consent pinned to the validated canonical root,
under Tyde's policy that its projects are trusted. Conformance must not seed
that consent on the adapter's behalf.

## Cancelled input

`real_cancelled_send_leaves_session_admissible` and
`real_cancelled_relocation_leaves_session_admissible` hand-poll
`send_with_outcome` and `set_workspace_roots` on the current-thread
conformance runtime. They drop the future at each of its suspensions, either
before any task it woke has run or after it was woken but before its result
was observed. No backend task runs between a poll and a `BeforeOwnerRuns` drop,
so each boundary is deterministic rather than timing-dependent. After every
attempt, a probe turn must be admitted, answer with its own marker and go
idle; a busy refusal must be followed by the running turn's idle. An error
message in the probe, a drained running turn or a completed send's turn
fails the case. A completed send must be `Accepted` and a
completed relocation `Ok`. Setup and oracles are identical for every eligible
backend: all backends for sends, and `SetWorkspaceRoots` providers for
relocation.

Neither case has a deadline of its own; a hang is caught by the
`conformance` profile's ten-minute per-test guard.

Both cases establish red coverage for Codex. The red runs used a
temporary, uncommitted nextest profile that terminated a test after 300
seconds, so that each run fit within one foreground command; it is not part
of the cases. With the admission that reserved the root turn before
enqueueing its message, the send case stopped progressing after `AfterWake`
at its first suspension: every probe was refused busy and no backend event
arrived before termination. With the relocation whose in-flight phase was
set before its guard was armed and whose completion was awaited, the
relocation case stopped at the `BeforeOwnerRuns` drop of that completion
with no further backend event. Those runs show no progress for the
remainder of the 300 seconds, not for the full ten-minute guard, which was
not rerun. The fixed adapter passes both under the canonical profile.

Runs on 2026-10-16, with the Claude and ACP relocation owners:

| Scenario | Claude | Codex | Hermes | Kiro | Grok | OpenCode | Antigravity | Muse |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `real_cancelled_send_leaves_session_admissible` | Pass | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_cancelled_relocation_leaves_session_admissible` | Pass | Pass | Pass | Pass | Pass | Pass | Pass | — |

Muse first failed at spawn because its served MSP schema was unsupported,
as did the untouched `real_task_list::muse`. After the Muse 1.4.10 host
support landed, the send case passed on Muse with the current probe and its
error check.

Before the owners, the relocation case failed on Claude and timed out under
the ten-minute guard on Grok and OpenCode.
Claude dropped after writing its `set_cwd` control request but before its
response. The CLI answers `set_cwd`, then emits an empty `result` frame, then
answers the `apply_flag_settings` request that follows. The probe turn,
admitted while that exchange was abandoned, was closed by the empty `result`
and reported no assistant output. Grok and OpenCode dropped inside the ACP
bridge's `restart_local`, after the old runtime's readers were aborted but
before its replacement was installed: the next prompt was admitted and went
typing, and the provider emitted nothing more.

Both adapters now hand an admitted relocation to an owner task that finishes
the provider exchange, and the caller only awaits its outcome. Claude holds
user-turn admission until that task has finished both control requests, and
refuses another relocation as busy meanwhile. ACP holds the prompt lock in
the task, so prompts wait for the session reload and another relocation is
busy. A failure whose caller has stopped waiting is reported as a backend
error rather than only logged.

After each dropped relocation the case immediately drops a second one at the
same point, before any probe turn. With Claude's busy check removed, that
second relocation was admitted alongside the unfinished one and the probe
reported an error; with it, the extended case passes on Claude, Codex,
Hermes, Kiro, Grok, OpenCode and Antigravity.

`real_cancelled_relocation_then_compaction`, for backends that also report
compaction, compacts immediately after each dropped relocation, then runs a
probe turn. Claude used to admit that compaction during the unfinished
`set_cwd` exchange, and the relocation then failed with an error. Grok
deferred it as an active turn while the reload held the prompt lock, and no
event ever settled it until the ten-minute guard. Compaction now waits for the
relocation's owner task in both adapters; the case passes on Claude, Codex,
Hermes and Grok.

## Typed input origins

`real_mid_turn_steering` passes distinct `MessageOrigin::Tychat` identities in
its initial send and mid-turn steer and requires those exact identities on the
backend-emitted user messages. Ordinary input remains untagged. Claude, Codex
and Hermes passed the same real-provider scenario after failing its new origin
assertion before the emitter fix. The server does not attach origins by matching
echo order; the interleaved UI/Tychat case is covered by the host sim flow.

## Child user-question policy and superseding input

The server applies `ToolCategory::AskUser` exclusion to child agents, including
children resumed from durable parent-session ownership. Root agents retain
questions. `Backend::validate_tool_categories` rejects unsupported exclusions
before spawn, resume, or fork; adapters enforce them natively rather than
hiding interactive cards. Existing allow/deny tool policies still apply.

- Claude uses native `--disallowedTools`.
- Codex disables the blocking tool and removes both asynchronous aliases from
  a private copy of the native model catalogue, preserving other tools and
  model metadata.
- Hermes disables its `clarify` toolset and detaches the native callback so
  calls copied from older history cannot bypass the catalogue restriction.
- Antigravity installs a private native `PreToolUse` deny hook for
  `ask_question`. The tool may still appear in the model's catalogue, but
  execution is denied before a question can reach the user. Rejected attempts
  retain their ordinary failed-tool diagnostics, not answer controls.

The host-local adapters also run on remote Tyde hosts. The legacy direct-SSH
Codex/Hermes launch paths reject category exclusions rather than silently
launching unrestricted children.

`real_excludes_ask_user_tools` is mandatory for every backend declaring
`UserQuestionRequests`. Its resume/fork companions additionally require the
corresponding lifecycle capability. Each scenario proves that an unrestricted
root can ask, then that an excluded session cannot ask and can still read a
real file with ordinary tools. There is no optional exclusion capability that
would let a question-capable backend skip this contract.

Codex runs this same oracle with both its asynchronous Astra profile and a
blocking Luna Plan profile. `blocking_question_profile.py` configures the real
CLI and forwards all native responses unchanged; it requires an actual native
blocking request as positive-control evidence. No events are fabricated.

Ordinary human messages supersede unanswered questions; typed tool answers
remain answers. The server interrupts a blocking question before dispatching
the queued follow-up. Async adapters cancel only questions already pending
when the accepted human message arrived. Automated agent-control, supervisor,
and review messages do not dismiss questions on the user's behalf.
`MessageOrigin::AgentControl` is a wire addition (protocol 68).

`real_new_message_withdraws_async_question` requires exactly one cancelled
completion and a successful unrelated follow-up on every asynchronous-question
backend. It failed on Codex and Antigravity before the fix and passes on both.
Server protocol regressions cover blocking and async follow-ups, reconnect
state, automated-message queueing, and root-versus-child policy. Existing DOM
tests cover the live attention label and cancelled question answer controls.

Validation on 2026-09-28 (UTC): fresh/resume exclusion passed on Claude,
Codex, Hermes, and Antigravity; fork exclusion passed on Claude and Codex
(the other two do not declare fork). Codex passed both native question
profiles. The root question/answer/cancel/recovery scenario passed on all
four. Removing Antigravity's native hook made the exclusion oracle fail;
re-enabling Codex's blocking native tool likewise failed its Plan-profile
oracle. Removing central child policy made the server agent-control flow fail.

An isolated desktop instance also exercised the reported Codex UI flow with
`gpt-6-astra`, low reasoning: the rendered root question had active choices
and both chat/sidebar showed `Needs your answer`. An unrelated message sent
with Enter produced the new reply, a cancelled question with no answer
controls, and `Idle` without the attention label. The instance attested
`storesEphemeral: true` and was stopped afterward. This is targeted DOM QA,
not a screenshot, multi-client, or full-backend certification claim.

## Experimental Codex questions retain the native turn

Codex 0.162.1's experimental `request_user_input` sends `isBlocking: false`
in Default mode and `true` in Plan mode. The native handler awaits a user
response in both modes; the flag controls the native UI's auto-resolution
policy, not whether the RPC releases the provider turn. Mapping Default
requests to `UserQuestionMode::NonBlocking` left ordinary follow-ups queued
without the server's blocking-question cancellation path.

The adapter now emits `Blocking` for these pending native RPCs and closes the
question's response before publishing paused typing. The native RPC's item
identity also owns its raw declaration, preventing a duplicate generic tool
from remaining open when typing pauses. Genuine
`request_user_input_async` message questions remain `NonBlocking`; native
answers, explicit cancellation, and question-tool opt-in remain unchanged.
Content-free routing diagnostics retain pending counts and the provider flag.

`real_user_question` runs the same answer/cancel/unrelated-follow-up flow on
every question-capable backend, including the ordinary Codex question path.
Codex additionally runs that oracle with the
experimental native tool enabled in Default and Plan profiles, forwarding
provider responses unchanged and requiring native flag evidence. A
nonblocking question must finish its asking turn before the paused-typing and
response-closure assertions, and before the unrelated message is sent; a
blocking question is interrupted first, as in the server's ordinary
human-input path. The answer turn must contain no backend user message: the
agent actor persists the typed answer once, so a backend echo would render
the single submission twice (Codex blocking questions once did, live and on
replay). The flow then resumes the session and answers a fresh question
through the backend's resume path under the same no-echo assertion;
`real_plan_approval` applies it to the approval turn. Replay is server-owned:
resume seeds the card from the authoritative journal and drops provider
history, so the protocol simulation for late question answers counts the
answer exactly once in reconnect and resumed bootstraps. The complete flow
must emit no protocol violations. The
follow-up must succeed and the old question must have
exactly one cancelled completion, never an implicit answer. Its completion
belongs to the earlier question request, not a tool newly started by the
follow-up. Existing protocol queue regressions cover that server dispatch.

Unfixed Default-profile coverage failed with a real native false flag and an
unclosed question response in 14.62 seconds. Fixed native Default and Plan
flows passed in 47.40 and 46.79 seconds, using GPT-6 Luna and GPT-5.6 Luna at
low reasoning. The unchanged Codex async superseding-input case passed in
20.86 seconds on GPT-6 Astra.

The extended question flow also passed on Claude, Hermes, and Antigravity.
Some subsequent native runs failed to issue the required request: the model
attempted to invoke the built-in control tool inside `exec`, where it is not
exported, and one claimed it was unavailable. The Codex-only prompt now
requires direct invocation and states that the experimental tool remains
enabled after cancellation. The oracle still requires real native requests
and never accepts an ordinary text question.

The Default native profile uses GPT-6 Astra after GPT-6 Luna failed to issue
the required native request. Plan retains GPT-5.6 Luna. Async questions
may arrive while their provider is still finishing the asking turn: a real
Antigravity run remained active after the fixture's initial ten-second drain.
The common flow now awaits the canonical idle event for nonblocking questions
before asserting paused typing and a closed response; neither assertion was
loosened.

Stricter UI QA exposed a second projection defect: the native question's raw
declaration survived as an uncompleted foreground tool. The strengthened
real-provider flow reproduced three `idle_with_foreground_tool` violations
and failed without explicit native identity binding in 51.98 seconds. This
regression uses unmodified provider output, not a synthetic event fixture.
With identity binding, the strengthened native Default flow passed on GPT-6
Astra in 53.92 seconds and Plan passed on GPT-5.6 Luna in 49.61 seconds, each
with three real native requests and no protocol violations.

Targeted rendered-UI QA used an ephemeral desktop instance and GPT-6 Luna,
with experimental settings confined to temporary project configuration. A
real false-flag request showed answer controls and `Needs your answer`.
Sending unrelated text with Enter cancelled the question, rendered the new
reply, retired answer controls, cleared the queue, and restored Send. A
subsequent pending question survived reload and accepted a question-card
answer; another accepted explicit Cancel. The instance and its stores were
removed. Screenshots and a second client are unavailable in this debug
launcher; this is targeted DOM QA, not full-backend certification.

After identity binding, a fresh ephemeral instance on GPT-6 Astra confirmed
the real Default-mode false flag and exactly one matching raw declaration.
Enter on unrelated chat input cancelled the answer controls, rendered the
new assistant reply, cleared the question attention and queue, and restored
Send with no error cards. A new draft enabled Send normally. The instance,
its stores, and the temporary experimental configuration were removed.

## Antigravity questions are nonblocking

Headless `agy` (checked on 1.2.11) cannot hold `ask_question` for a human:
print mode answers every question itself with "User Skipped" and keeps
going, its stream-json stdin accepts only `user` events, and the terminal is
used only for login. Antigravity therefore declares
`AsyncUserQuestionRequests` and reports its questions as `NonBlocking`. The
card stays answerable after the turn goes idle. The answer is relayed as the
next turn; an answer given mid-turn is queued until the running turn's
`result`, with no idle state emitted between the two turns.

`real_async_user_question` and `real_user_question` pin Antigravity to
`gemini-3.8-flash-low`. Both passed on Antigravity on 2026-09-25. Before the
fix, `real_async_user_question` failed on the `Blocking` mapping. With only
the `NonBlocking` mapping in place, it failed because an answer given
mid-turn produced no turn.

## Async answer and previous completion overlap

`real_async_answer_completion_overlap` requires asynchronous user questions
and native mid-turn steering. It exercises an answer whose steering is
declined because the provider already finished the asking turn, while Tyde
has not yet consumed that turn's completion. Codex is currently the only
backend declaring both required capabilities. Async-only question backends
retain the existing `real_async_user_question` flow and oracle.

The transparent real-CLI fixture holds the genuine asking-turn completion
for five seconds, preserving FIFO notification ordering while RPC replies
continue independently. Its content-free proof requires a native steering
decline and an accepted answer-turn start before releasing the completion;
it fabricates no responses, notifications, or command results. Missing
trigger evidence fails the case. Private diagnostics and transport proof
remain in the reported `tyde-real-completion-race-*` directory.

The unfixed Codex adapter failed the controlled case on 2026-10-05: the
answer turn emitted live work while the client's typing state was inactive.
The case checks distinct start/completion edges, rejects duplicate typing
events, verifies the answered question and real foreground command, and
retains a follow-up returned as Busy until the answer's completion permits
delivery. The server-level queue reproduction uses the same real provider
interleaving, through the real framed host protocol.

The Codex adapter now reasserts activity on every valid provider turn start,
including user-requested starts. Existing emitter deduplication still
suppresses redundant true calls. Duplicate and terminated provider starts,
question semantics, native steering, server queue policy, and UI rendering
remain unchanged.

Validation on 2026-10-05: the controlled unfixed case failed in 12.26
seconds; the fixed case passed in 21.70 seconds with the same proven
interleaving. The real-server reproduction queued the follow-up at 16.35
seconds, drained it at 24.54 seconds, and completed its reply at 27.75
seconds. The existing async-question flow passed on Codex in 118.08
seconds and Antigravity in 108.77 seconds. An initial Antigravity run
failed the existing independent-work assertion with no pending command;
an unchanged-code control passed in 111.48 seconds before the explicit
fixed-build recheck passed. Neither its oracle nor Antigravity production
code was changed.

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

Every successful `Backend::resume` stream contains exactly one internal
`BackendEvent::ResumeReplayComplete(Ok(()))` after historical replay. History
never emits `TypingStatusChanged(true)`. Adapters should place the marker
before live events. If a provider starts earlier, its explicit typing edge
begins an ordered live suffix; no more history may follow that edge. The
actor defers the suffix, including any completion, until the marker, then
reduces it before resume normalization. It never infers activity from text,
tools, or history. Persisted task restoration precedes live reduction.

The marker's `Err` form, a missing marker at the 30-second deadline, or a
closed stream fails startup rather than admitting partial replay. Failure
warnings include deferred event counts. Binding preparation and conformance
collection consume through the marker, leaving subsequent live events unread.

The adapter audit establishes the current production ordering:

- Codex's inbound gate orders its provider `turn/started` after the marker,
  even when that notification arrives on the transport before the resume RPC
  reply. Its existing self-start handling emits the live typing edge.
- Claude loads history and emits the marker before process initialization.
  On CLI 2.1.281, the mid-tool hard-stop regression observes no standalone
  bootstrap result before the next prompt. Its first `system/init` follows
  that prompt, and its terminal result belongs to the live response. Do not
  wait for an assumed bootstrap result before writing this prompt.
- ACP holds its inbound gate through replay flush, idle normalization, marker
  emission, and the switch to live handling. Kiro, Grok, and OpenCode share it.
- Hermes emits replay and the marker before buffered gateway events.
  Antigravity emits them before starting its live event-forwarding task.

## Restored live turn state and desktop projection

The user-visible fix is desktop `turn_active` parity plus the post-dispatch
attach flush, not a new Codex adapter fix. The actor's pre-boundary live-event
handling is hardening. No provider adapter is changed.

Desktop applies the server's typed state from `HostBootstrap`, `NewAgent`,
`AgentBootstrap`, and `AgentTurnStateNotify`, without loading chat to discover
activity. An assigned session establishes completed startup, matching mobile.
Fatal rows cannot be revived by a turn notification.

Parked attaches are flushed after queued and initial follow-up dispatch so
that the first bootstrap carries the resulting authoritative registry state.
A Busy initial follow-up marks the provider turn active, remains queued, and
does not disarm the ongoing turn's completion. Its end dispatches the queued
follow-up; the final completion settles idle. Dispatch failures flush terminal
state to parked clients.

Server simulations cover completed and ongoing pre-boundary turns, Busy
follow-ups on both sides of the marker, eager/lazy bootstraps, unattached
notifications, and final idle. The real-DOM sidebar flow exercises typed host
frames without an `AgentBootstrap`. Real conformance checks resumed streaming
and tool events occur while typing is active, with idle at completion; the
controlled Codex native-goal case covers provider-initiated turns.

Shutdown, orphaned CLIs, and in-flight turn persistence remain unchanged.

## Kiro resume startup command identity

On 2026-09-23, the unchanged
`real_conversation_on_resumed_session::kiro` failed before the fix in 6.93
seconds (7.01 seconds wall, exit 101). Sanitized notification tracing confirmed
that the first rejection was `_kiro.dev/commands/available`, normalized as
`available_commands_update`, carrying the temporary `session/new` identity
rather than the requested `session/load` identity. The router admitted the
current startup identity during replay, and Kiro normalization replaced its
subcommand table before the strict replay validator recorded the mismatch.
Requested-session history and a separate requested-session command advertisement
arrived afterward, but the retained first rejection failed resume at the barrier.

The router now discards only Kiro's command advertisement when replay is active,
the notification explicitly identifies the current startup session, and a
present, different requested replay identity exists. This happens before child
routing and side-effectful normalization; neither the subcommand table nor the
catalog sees that obsolete advertisement. Requested-session advertisements,
missing-identity failures, standard updates, history-bearing vendor updates,
and the existing Grok/OpenCode identity exceptions are unchanged. The
`53db493f` inbound gate remains held through replay flush and the in-band
`ResumeReplayComplete` boundary. No frontend behavior is inferred or changed.

The same unchanged case passed once with the fix in 11.17 seconds (11.21
seconds wall, exit 0), including requested history, a follow-up reading an
out-of-band rewritten file through a real tool, and the existing duplicate
history checks. Tracing observed the startup advertisement discarded before
normalization and the requested-session advertisement accepted, publishing 19
commands. Both runs used Kiro's default/auto model selection. Only Kiro was
rerun for this fix; the common case and its assertions remain unchanged for all
seven eligible providers. This is targeted regression evidence, not renewed
certification of other backends or every Kiro feature.

Content-free diagnostics are retained under the `tyde_acp_resume` tracing target;
they report known methods/update kinds, phases, identity comparisons, routing,
first rejection, and command counts, never session identities or payloads.
Local red and green evidence is retained under
`target/kiro-resume-diagnostics/` (`sanitized.log` and
`green/sanitized.log`, with separate invocation and timing files).

Scoped desktop QA also passed on Kiro CLI 2.21.2 with the rendered model
`auto`, using an initially empty disposable store and disposable workspace.
After native close confirmation, clicking the saved History conversation
replayed the original user/assistant history once and settled at Idle before
any prompt. The resumed slash menu contained the same 19 commands. One
follow-up recalled pre-resume context and returned the exact bytes of a file
rewritten outside the conversation, with one successful rendered read-tool
card and a Thinking-to-Idle transition. The read-tool renderer displays file
metadata, not file contents; the exact bytes were verified in the assistant
reply. There remained one saved conversation and no agent error cards.

Four bounded cold-build/startup readiness attempts timed out before the fifth
instance became ready; no QA prompts were sent during those attempts. The
scoped run used rendered UI controls, including the real native close dialog
confirmed through AT-SPI. Screenshots and second-client automation were
unavailable, so this does not claim those checks or broad backend certification.
The instance was stopped, its disposable stores removed, and its process gone.
Content-free UI evidence is in `target/kiro-resume-diagnostics/ui/`.

### Project-move validation collateral

The first diagnostic build also failed the existing server sim
`move_agent_preserves_conversation_and_persists_all_project_roots`: after the
return move, final bootstrap retained the destination's two roots instead of
the source's one root, while the immediately preceding source-project assertion
passed. Intermediate move acknowledgements and persisted-root assertions had
passed. An instrumented check passed without a production persistence change;
that pass alone does not resolve the race.

The test created fresh hosts against the same live store after each move,
without stopping the original host. `connect_fresh_host_with_bootstrap` starts
restoration of open agents; restored actor startup calls
`persist_agent_session` and `upsert_backend_session`, which write the captured
roots. These are additional asynchronous writers, not read-only persistence
observers. Returning bootstrap does not wait for their restoration to finish,
and dropping the client does not stop the host's agents. The test was racing
those stale startup writes against subsequent moves, not simulating a restart.

The scenario now connects another client to the one owning host for intermediate
bootstrap observations (session summaries read persisted SQLite rows), then uses
`restart_host` to shut down the owning agents before the final fresh host.
All existing busy/missing-project, acknowledgement, observer, session identity,
workspace-root, project, follow-up, and final persistence assertions are retained
unchanged. Only invalid overlapping-host setup is corrected; production
persistence behavior is untouched. Sanitized root-count/comparison diagnostics
remain in the sim and under the `tyde_session_roots` tracing target.

The committed check also exposed a separate wasm fixture timing error in
`one_chat_mount_preserves_transcript_and_composer_draft`: at the 852x130
resize, diagnostics showed viewport/document height 130 but shell height 516
and send-button bottom 500, the preceding frame's geometry. The test waited
100 milliseconds even though `attachShell` applies geometry in the iframe's
`requestAnimationFrame`. It now awaits that window's render cycles instead of
a timer. All existing reachability, bounds, DOM identity, draft, selection,
and focus assertions and diagnostic output remain unchanged; no production
mobile behavior changed.

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

## Plan approval presentation (2026-09-25)

Plan completions now carry `ToolExecutionResult::ExitPlanMode` with an enum
verdict and optional feedback, plan, and plan path. The desktop card projects
that server result: awaiting approval while pending, then approved or rejected
with feedback retained. Successful rejection is not a failed tool invocation.
The server omits transcript messages for tool responses with no text or images;
nonempty responses and ordinary messages keep their existing path.

The server-protocol regression failed on the former empty user message, with
483 other native tests passing. Both browser regressions failed against the
old presentation (`Running…` instead of `Awaiting approval`, and `Done` instead
of `Approved`), with 807 other desktop wasm tests passing. The fixed committed
workbench passed `./dev.sh check`. Scoped `real_plan_approval` passed on Claude
Haiku 4.5 (20.19 s) and Grok 4.6/low (63.30 s); no other paid conformance scenarios ran.

An isolated dev instance verified pending, rejected-with-feedback, and approved
cards on Claude Haiku 4.5 and Grok 4.5/low. Both retained the server decision,
used distinct approval/rejection colors, removed finished controls, and added
no user bubble for either decision. Grok 4.5 was the lowest-price available
catalog option (lower cached-input pricing than 4.6/4.7). The desktop ran on
Linux x86_64/WebKit at 1280×784. Screenshots and second-client automation are
not supported by this debug driver. The instance and its agents were stopped.

The reported Grok feedback loss without a plan file is upstream, not a Tyde
transport defect. It also reproduced in a top-level Grok 4.5 agent: the finished
card retained exact feedback, but the model asked generically for direction
instead of echoing it. Tyde sends the documented `cancelled` outcome and
`feedback`. Grok's `PlanApprovalOutcome::Cancelled` branch only uses feedback
when `plan_content.is_some()`; otherwise it supplies generic planning text to
the model. This branch is not specific to Tyde children or teams. No provider
workaround or second implementation commit was added.

Sources: [Grok response schema](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-tools/src/implementations/grok_build/exit_plan_mode/types.rs),
[Grok cancellation branch](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs#L2054-L2061),
and [model pricing](https://docs.x.ai/developers/pricing).

Post-rebase validation also exposed a startup race in the existing
`agent_control_http_await_returns_while_exit_plan_mode_is_pending` sim. Its
failing trace contained a replayed pending request but no live typing edge;
bootstrap correctly does not replay transient typing events. As in the adjacent
plan-approval sim, startup is now gated and the plan requested only after the
client subscribes. All pause/await/continuation assertions remain unchanged.

## Hermes clarify lifecycle

`real_user_question` requires live typing-off while the unanswered question
remains pending, exactly one cancelled completion when that question is
abandoned, and an ordinary follow-up immediately after cancellation (without
an artificial settlement delay). The same scenario and assertions apply to
every backend declaring `UserQuestionRequests`.

The unfixed Hermes `qwen-local` run failed the live typing assertion with
`Some(true)` instead of `Some(false)`. Hermes now emits typing-off at the
blocking `clarify.request` and typing-on after accepting the answer. The
concurrent reload fix in `302039e7` also closes the provider response before
the blocking request, retaining the native turn and attaching late usage to
the closed message. That implementation superseded the initial typing-only
fix here; it is retained rather than duplicated.

Cancellation exposed two native boundaries. `session.interrupt` acknowledges
before the clarify worker has settled history, so an immediate follow-up can
be treated as a redirect into the cancelled turn. The bridge now waits for
that worker only when interrupting a pending clarify. Ordinary interrupts
keep their existing behavior. Once settled, Hermes's synthesized assistant
interruption notice lacks a native Responses message sidecar. Its serializer
emitted that notice as an untyped assistant input item, which the local
llama.cpp Responses endpoint rejected with HTTP 400, `Cannot determine type
of 'item'`. Content-free instrumentation showed no untyped assistant items in
requests 1–6, then one in the failing post-cancellation request 7. The bridge
uses Hermes's own typed message serializer for these assistant notices,
retaining their text and all preceding conversation and tool results. Native
typed response items, reasoning items, and Chat Completions are untouched.

Select the host's existing Hermes profile with
`TYDE_HERMES_TEST_PROFILE=qwen-local`. That profile currently uses Chat
Completions, so Responses coverage additionally uses a disposable Hermes home
with a named custom provider pointing at the same local model and explicitly
selecting `codex_responses`. Set `HERMES_HOME` to that disposable home and
`TYDE_HERMES_TEST_PROFILE=default`; copy credentials directly into its private
configuration without printing them. The local Responses run reproduced the
HTTP 400 after the settlement fix and before the serializer fix. Both
transports exercise the real model, not a stub or canned gateway.

## Immediate follow-up after uncooperative termination

`real_immediate_message_after_uncooperative_stop` runs the same setup and
oracle for every `ResumeSession` backend. An isolated test process starts a
real agent, waits until a foreground tool writes its PID, verifies the tool's
ancestry back to the test process, and SIGKILLs the owned backend process
group. It resumes the saved session and immediately sends a tool-free reply
request. Exactly one non-empty StreamStart/StreamEnd pair, the requested
reply, the universal contract, and no delayed duplicate are required. No
provider output is fabricated and no process is selected by name or path.

Baseline at `302039e7` (2026-09-25), with only the new test and content-free
Claude diagnostics: Claude passed twice (11.87s and 9.92s); Codex 11.65s,
Kiro 8.93s, Grok 12.52s, Hermes 20.80s, and Antigravity 20.17s passed.
OpenCode failed in 8.58s: ACP `session/load` replay ended with the real
interrupted tool still unresolved, and Tyde rejected the entire resume.

The Claude trace establishes that resume initialization finished before the
follow-up was written; the subsequent init and result were the live reply,
not a separately identified bootstrap generation. The reported Claude reply
loss was **not reproduced**. Its quarantine behavior is deliberately unchanged;
a terminal-result wait without positive bootstrap identification would hang
the observed quiescent path. The new diagnostics report frame kinds and
presence only, not session identifiers or response content.

ACP now retires unfinished historical tool cards at the authoritative replay
boundary using Cancelled and an explicit unknown-outcome notice. It never
claims a side effect did not happen or invents a successful result. Retained
replay errors still reject startup; live tool handling is unchanged. This
replaces the Grok-only historical-result exception with the same boundary
handling for Kiro, Grok, and OpenCode. The unchanged real scenario then passed
all seven backends (111.95s combined): Claude Haiku 12.10s, Codex
GPT-5.6-Luna 21.56s, Hermes DeepSeek-V4-Flash-0731 24.16s, Grok-4.6 low
13.26s, Kiro auto 10.63s, OpenCode big-pickle 12.43s, and Antigravity
Gemini-3.8-Flash high 17.58s. The case now pins Antigravity's cheaper
Gemini-3.8-Flash low profile rather than its installed default. This is
targeted resume coverage, not full backend certification.

## Restart process containment

`real_interrupt_shutdown_kills_process_group` uses identical setup and oracle
for all seven backends: hold a real foreground tool, locate its owned backend
process group through PID ancestry, join a separate sleep probe to that group,
then interrupt and shut down. The whole operation must finish within 25s and
leave no running group members. Zombies are not running processes. No provider
output is fabricated; the probe tests ownership, not model behavior.

The guard passed all seven on the unmodified shutdown implementation (58.90s).
That establishes the existing explicit-shutdown contract, not a reproduction
of host exit forgetting to call shutdown. With restart containment changes,
six passed in the first run; Codex failed before initial readiness with HTTP
401, not at the containment oracle, and bounded retries confirmed the
credential failure. Once those credentials recovered, the Codex guard passed
(8.31s), so all seven pass with the change.

Protocol sims separately reproduced the missing durable InFlight marker and
unsafe managed-process termination. Real stdio subprocesses reproduced abrupt
SIGTERM/SIGINT exit. Those tests now pass, along with shared, cancellation-safe
25s shutdown, preserved restore intent and ordered queued messages. Both DOM
suites render all typed recovery phases, including a notice-only conversation.
Linux backend leaders use parent-death SIGKILL as best-effort containment;
descendants which escape the group and macOS abrupt host death remain gaps.

## Restart continuation origin

`real_restart_continuation_is_not_a_user_message` resumes a cleanly closed
session through every backend that declares resume and delivers a prompt with
the server-reserved host-restart origin through the production `Backend`
trait. The oracle is identical everywhere: the turn must show no user message,
must end exactly one response, and that response must be the requested marker,
so the continuation reached the model as a prompt without being echoed as
user text. Before the origin was threaded through the providers, every backend
echoed it as a user bubble. It passed all seven backends (14.63s combined); a
Codex-only rerun with captured output confirmed the scenario executed there
(10.18s).

The scenario covers the provider boundary only. Restoration ordering, queue
placement, adoption of a backend's own resumed turn, and the manual-resume
path are sims in `server/tests/session_resume.rs`. Providers that replay
history from their own transcripts may still show the continuation as a user
message on a later resume; that replay is not covered here.

## Shared swarm images

`real_swarm_shared_images` uses the real host and authenticated board MCP
router, not synthetic provider events. It uploads a real PNG before the first
member wake, publishes its metadata on Briefing, requires a real board read
and `tyde_swarm_read_image` completion carrying the exact metadata and pixels,
and requires the member to publish the pixel-derived color answer through
`tyde_swarm_update_thread` after reading the human thread summary, using its
exact sequence and advancing it once. Setup and assertions are identical for every eligible
provider; only the prompt wording differs to require the same actual reads.
The case requires image input, enforced read-only access and delegation
exclusion; Claude and Codex are the currently eligible providers.

Claude's real run reproduced a malformed public MCP completion: its native
transcript uses Anthropic `image.source` blocks, while MCP uses `data` and
`mimeType`. The Claude adapter now projects the actual native bytes onto typed
MCP image content, preserving other native content. Its native source-path
text annotation is not metadata; the oracle requires exactly one typed image
metadata block and one exact image block rather than rejecting that annotation.
Malformed or unsupported native image sources fail explicitly.

Validation on 2026-10-04: the shared-pixel case passed Claude Haiku 4.5
(12.82s) and Codex gpt-5.6-luna at low effort (10.22s). Server protocol simulations cover
PNG, JPEG, GIF and WebP, bounded validation, upload and publication idempotency,
image-only roots, replies, both boards, unpublished and foreign-swarm access
refusal, corrupt media, committed-but-uncertain durability and host restart.
The real-DOM flow covers picker, clipboard and multi-file drop, individual
removal, readiness, upload/read/publication failures and retry, independent
board drafts, Agents-tab preservation, decoded posted pixels and the viewer.
Ordinary chat-image behavior and swarm notification routing are unchanged.

The live Linux desktop flow additionally verified multi-image picker, paste and
drop, image-only publication, replies, per-image removal, decoded pixels and
the full-size viewer through the real host. Five published references matched
the host-owned files and digests. The composer measured 132.69px empty, used
the available pane width and did not overflow a 420px pane. This check caught
the client sending opaque history IDs where the image store requires UUIDs;
image uploads now mint cryptographically random UUIDs, and the mounted-DOM
flow checks the actual outbound identity shape before host acknowledgement.

## Mobile swarm request threads

Mobile now requests a typed, server-paged root-only board view. Roots sort by
original creation time, not later reply activity; cursor coordinates are
bound to that view and its snapshot. Opening a root reads its full thread.
The inbox never marks unseen replies as read. Root composition is on demand,
with separate root, board and reply drafts retained through navigation.
Shared pixels are read from the owning host only after opening the thread.

The default spawn and notification guidance now asks peers to keep concise,
human-oriented progress, results and decision questions in the original
Briefing request thread, and peer planning in Coordination. It does not
classify or move old posts, impose task status, or rewrite custom guidance.
The existing real image case now uses the default shared guidance and checks
the concise pixel-derived answer in the original human request thread. It
passed both eligible providers: Claude Haiku 4.5 (13.65s) and Codex
gpt-5.6-luna at low effort (23.37s). Setup and the oracle remain identical.

Protocol simulations cover root pagination, snapshot isolation, later
replies, and rejection of cursors reused across read views. The mounted
mobile flow covers sixty-one root rows, reachable reply history, older
linked threads, retries, canonical publication acknowledgement, independent
drafts, host isolation and actual decoded shared images. Its old flat-board
assertions now require replies to remain reachable through their thread,
not rendered in the inbox or falsely marked read there. Lazy images are
scrolled into view and awaited through the browser's decode operation.

The live direct-hosted PWA used an isolated native host and a fresh Chromium
mobile viewport. Three actual human roots and one reply were confirmed in
the host store, then read over the real paired connection. The inbox kept
creation order, hid reply traffic and its closed composer, opened the exact
reply and pixels, and preserved independent reply and new-request drafts.
At 390px it had no horizontal overflow; single-line request rows measured
67.75px. The native instance, browser and ephemeral stores were stopped.
This is scoped UI/host validation, not a complete backend certification.


## Stateful swarm interaction tools

`real_swarm_board_coordination` drives the production host and native MCP
interfaces under enforced read-only project access. A peer atomically creates
Coordination under the exact human request; another peer lists the directory,
reads the current summary and attributed deltas, and publishes a conditional
reply. Native tool results must preserve authenticated authors, causal rounds,
parent ownership, and the summary/delta sequence. All five new tools need actual
correlated completions, not private prose or inferred results.

The busy-delivery, sequential audit, follow-up cardinality, restart, and
resume oracles remain intact. Four distinct page limits (50, 60, 70, 80) make
the requested audit reads unambiguously separate work; see "Steered swarm
wakes" for how the busy phase now proves steering. Body segments and summary
changes both use the `kind` discriminator.
Invalid model arguments are still rejected rather than translated or guessed.

The deterministic server regression races two authenticated creators, rejects a
stale human-thread update without mutation, verifies catch-up and idempotent
stale retries, and exercises UTF-8 limits, pinned pagination and persistence.
It was run against the unchanged server and failed at the missing native tool;
with the implementation, it passes within the canonical server suite.

Scoped stateful-thread validation on 2026-10-04 used Claude Code Haiku
(Claude Haiku 4.5 through the existing OpenRouter credential after the direct
account reached its weekly quota) and Codex `gpt-5.6-luna` with low reasoning.
Both providers passed `real_swarm_board_coordination` and
`real_swarm_shared_images`, including the same assertions and capability gates.
The final coordination runs took 138s and 104s respectively; image runs took
19s and 18s respectively. These are feature-path results, not full backend certification.

Live desktop QA used a branch-built debug MCP launcher because the resident
launcher used protocol 69 and could not handshake with protocol 72. Its typed
UI-debug Evaluate transport drove the actual rendered app with attested
ephemeral project/session stores and one read-only Codex `gpt-5.6-luna` peer.
The human composer created sequence 1; the peer replaced its summary and
published one reply in that exact human thread at sequence 2. The current
state changed reactively. An explicit Coordination child linked back to that
human request; a second creation displayed a conflict, froze the uncertain
payload, and committed no losing delta. Edit unlocked the proposal. Durable
state contained exactly two threads and three deltas, and the instance was
stopped. Desktop/mobile DOM suites additionally cover conditional composers,
reactive state, compact layouts, and identical frozen retries. Unsupported
screenshot/second-client capabilities were not used or claimed.

## Human request threads

Swarms no longer open with a brief. Members stay Proposed until a human
request reaches them. Every human root is a Briefing request; without member
mentions it wakes all active members, and with mentions only those members.
Agents reply in that request with `update_thread`, or open
Coordination children beneath it; they never create Briefing roots. The real
case keeps its setup, oracle and per-provider prompt-only differences: the
opening request is now an ordinary human thread, and two peers append
SWARM_READY to it concurrently.

That concurrency exposed a Codex normalization defect. Codex reports an MCP
`isError` result as a failed `mcpToolCall` whose `result` omits the flag, so
its completion surfaced as an opaque `Other` result with `isError: false`
while Claude surfaced the canonical MCP error. The Codex adapter now restores
the flag from the failed status, so both providers deliver the same canonical
error and board code. The harness decodes that code from the canonical
`CallToolResult` for every provider and tolerates only a conflict on a
conditional write, which commits nothing and is retried by the member.
Without the fix, a genuine Codex conflict failed the case as unclassified.

Validation on 2026-10-05 used Claude Haiku 4.5 and Codex `gpt-5.6-luna` at
low effort. `real_swarm_board_coordination` passed on Claude (114s) and on
Codex (139s); each run hit and recovered from a real update conflict.
`real_swarm_shared_images` passed on Claude (22s) and Codex (16s). One
earlier Codex coordination attempt exceeded the five-minute limit during the
busy audit while still making progress; the next run passed. These are
feature-path results, not full backend certification.

Live desktop QA on 2026-10-05 drove the branch build through the branch's own
`tyde-dev-driver debug` launcher, because the resident launcher speaks
protocol 69 and this host speaks 73. With ephemeral stores and two read-only
Claude Haiku members, creation took only a name, scope and members; members
showed as ready to chat until the first request. That request committed
instantly as "Naming…", woke both members, and gained a generated title,
description and summary; both members replied in that thread. A human reply
rendered "posting…", then committed with a rewritten summary. A second
request led Peer 1 to open a Coordination child: the request showed
"Coordination · 1", and the flat Coordination tab showed it as "Re: <request
title>" with no root composer and no sequence label. The agreed answer came
back in the request thread. The instance was stopped. Unsupported screenshot
and second-client capabilities were not used or claimed.

## Steered swarm wakes

Every swarm wake, from a human post or an agent mention, now reaches a busy
member as steering into its running turn. When a backend cannot steer, Tyde
interrupts the turn and keeps the notification pending until the member is
idle; a wake never waits in the member's private queue. Swarm instructions are
standing builtin steering, and a wake is one line per post naming the author,
thread, title and post ID. The agent wake budget is optional and off by
default.

The busy phase of `real_swarm_board_coordination` posts BUSY_FOLLOWUP once
the initiator's audit-thread read is in flight. It requires the follow-up
intent Accepted while the member is still Thinking, with the member moved to
the follow-up round. It also requires AUDIT_DONE attributed to that round
and at least one audit read after the acceptance. Together these prove the
wake joined the running turn. The audit oracle requires all four passes in
order, each reading the board and then the thread with the pass's limit. All
audit reads must complete sequentially. A pass may repeat: one real Codex run
restarted the audit from limit 50 once the follow-up was steered in, making
nine audit reads. Per-tool busy traces print with indexed round and thread
IDs.

Scoped validation on 2026-10-05 used Claude Code Haiku with low effort and
Codex `gpt-5.6-luna` with low reasoning. Both received the follow-up as
steering between the limit-50 and limit-60 passes. Both passed
`real_swarm_board_coordination`, Codex in 260s and Claude in 133s. Both
passed `real_swarm_shared_images`, in 25s and 21s. One earlier Claude run
read the audit thread once and ended its turn without auditing, which
exceeded the busy deadline; the rerun passed unchanged. Another earlier
Claude run answered the follow-up with a summary read instead of the
required thread read. The shared guidance now names the
`tyde_swarm_read_thread` call explicitly. These are feature-path results,
not full backend certification.

## Swarm cases removed from conformance

`real_swarm_board_coordination` and `real_swarm_shared_images` started the
full production host and drove members through its swarm layer. That broke
the rule that conformance runs directly through the `Backend` trait: server
code sat between the oracle and the provider, so a server-side change could
make a provider bug pass. Both cases and `conformance2/swarm.rs` are deleted.

Swarm orchestration (requests, threads, mentions, steered wakes, budgets,
pause, restart and resume, images, naming, replacement) remains covered by
the protocol sims in `server/tests/swarms.rs`. The provider contracts those
cases had caught are now trait-level:

- `real_mcp_image_result` (`StartupMcpServers`, `McpImageResults`,
  `McpErrorResults`): an MCP server returns successful text and a PNG. The
  model must identify the colors from the pixels. A canonical `CallToolResult` retains
  the original text and image blocks; typed `NativeContent` instead retains
  native presentation text and the original pixels and media type without
  claiming canonical block boundaries. The complete original multiline text,
  including its annotation-shaped literal, must survive unchanged once. The
  second real call returns an image error on every eligible backend. Exactly
  two server calls, with only the second carrying `fail: true`, must produce
  one matching MCP image request and completion that stays failed, preserves
  the actual native reason and retains the complete unchanged server text once.
  Only the additional original error-pixel and media-type assertions depend on
  `McpErrorImageResults`; status, text and call counts never do. Universal
  contracts still cover every request and completion, including any native
  execution container.
- `real_mcp_error_result` (`StartupMcpServers`, `McpErrorResults`): the MCP
  server returns `isError: true`. The completion must be a failure carrying
  its retained result: either canonical MCP output with `isError: true`, or
  typed native output alongside the actual native failure reason. This covers
  the Codex restoration of the flag that its failed `mcpToolCall` items omit
  and Muse's native failure projection. `real_mcp_tool_call` keeps its success
  phases and stays gated only on `StartupMcpServers`.
- `real_mcp_image_text_fidelity` additionally requires
  `McpOriginalContentFidelity`. Its unchanged oracle requires canonical MCP
  output containing the complete original server text block once beside the
  original typed image, even when authentic text resembles a native image
  annotation. Native presentation is not an equivalent result. This flow
  does not certify arbitrary interleaving, `structuredContent`, or `_meta`.

### Capabilities

`McpImageResults` guarantees successful image results, not failed-image
retention. It is declared by Claude, Codex, Muse and OpenCode. Grok moves the
image into a synthetic user message and Hermes into a cache file path, so
neither stream carries the pixels. Kiro reports the canonical block, but its
model receives the image as base64 text: asked for the colors, it decoded
the PNG with a shell command, and once answered `red:green:blue` from the
"pixel data".

`McpOriginalContentFidelity` is declared by Claude, Codex and OpenCode, based
on their prior passing runs of the unchanged strict text-fidelity flow.
Muse does not declare it: its known strict-fidelity RED is an unsupported
guarantee, not a new passing result. Muse remains eligible for the basic
successful-image, text-error and lifecycle cases independently of this split.

`McpErrorResults` is declared by Claude, Codex, Grok, Kiro, Muse and OpenCode.
Antigravity and Hermes reach MCP servers through Tyde's bridge, which
reports every call as a success so the provider keeps the embedded result,
and both then flatten it to plain text.

`McpErrorImageResults` separately promises original image retention on an MCP
failure and requires both `McpImageResults` and `McpErrorResults`. Only Codex
declares it, based on actual retained failed-result pixels, text and error
status. It gates only additional pixel assertions within the shared image
success/error flow, not admission to that flow or any error-status/text checks.

### Provider fixes found by these cases

- Grok: ACP titles every MCP call `use_tool`, and startup-MCP detection
  matched that raw title instead of the normalized tool name, so no Grok MCP
  result was canonicalized. Detection now uses the normalized name, and the
  Grok adapter maps `OkayOutput` / `Error` onto a canonical result.
- OpenCode: MCP image blocks are moved out of the tool output into data-URL
  file attachments on the same part. The adapter now restores them as image
  blocks.
- OpenCode's default model declares no image input, so the image cases
  (`real_image_input`, `real_mcp_image_result`) select
  `opencode/mimo-v2.6-flash-free`. That model answered `magenta:aqua`, so the
  color answers canonicalize the CSS synonyms fuchsia/magenta and aqua/cyan.

### Results (2026-10-05)

- `real_mcp_tool_call`: pass on all seven backends.
- `real_mcp_error_result`: pass on Claude, Codex, Grok, Kiro and OpenCode.
- `real_mcp_image_result`: pass on Claude, Codex and OpenCode.
- `real_image_input`: pass on Claude, Codex, Kiro and OpenCode.

Negative controls: `real_mcp_image_result` fails on Claude without the
image projection (`missing field data`), and `real_mcp_error_result` fails on
Codex without the `isError` restoration. Grok and OpenCode failed both cases
before their fixes.

### Native failed-image audit (2026-10-14)

The strengthened successful-image phase identified the actual colors and
preserved the complete server-owned multiline literal and exact PNG on Claude,
Codex, Muse and OpenCode. The combined case then failed in its error phase;
none of those complete combined runs is recorded as passing.

Retained native results establish why failed-image retention is separate:
Claude returned error text only (178 bytes, no image blocks), OpenCode error
text only (178 bytes, no attachments), and Muse one committed text result
(191 bytes containing the unchanged 178-byte server block once, with a 13-byte
native prefix) without an image event for that failed call. These three missing
native payloads remain explicit unsupported guarantees, not adapter fixes or passes.
The native error-text oracle requires the entire server block, including every
newline, to occur exactly once without removing or rewriting that prefix. A
whole-line requirement incorrectly rejected the prefixed first line despite
the complete block being intact. The successful-image whole-line collision
oracle is unchanged.

Codex retained `isError: true`, the entire original text block once and one
`image/png` whose decoded 3,564 bytes and SHA-256 exactly match the fixture.
Its native code-mode trace contains one outer `exec` call/output and the inner
MCP image call. The old assertion counted all requests and rejected this valid
container rather than finding a missing payload. The shared error oracle
now requires exactly one request with the successful call's exact native tool
name, and exactly one completion with that request's call identity. It does not
ignore the outer card's declaration, arguments, completion or stream contracts.
The consolidated shared success/error case passed through the production
Backend on all four eligible backends, then passed again after restoring
canonical error-text block equality: Muse 27.11 seconds, Claude 15.40, Codex
10.18 and OpenCode 160.68. Failed-image pixels alone remain capability-gated;
all four still exercise genuine image-bearing error status and original text.
The transient dedicated-case
split excluded three backends' error-status/text/call-count checks and was
replaced rather than accepted as coverage.

## Muse integration workbench

Muse runs as a native `muse serve` MSP client, not as an HTTP imitation of
Wonder. The public `meta-models/muse-code-sdk` repository supplies the MIT
TypeScript client and protocol declarations; it does not contain the native
host. The integration is validated against Muse 1.4.6-R6000.1,
1.4.7-R6101.1 and 1.4.10-R6427.1 on Linux. Each accepted host is pinned by
its served `readSchema` fingerprint; any other fingerprint fails startup.
The harness launches the `muse` launcher from PATH, which auto-updates, so
real-provider runs set `MUSE_NO_AUTO_UPDATE=1` to keep the host fixed.
The 1.4.7 stable additions concern computer setup, sandbox availability,
partial-reply marking and name origin; existing consumed fields are unchanged.
The audited 1.4.7 to 1.4.10 SDK schema difference is additive: the open
`ItemKind` gains `monitorFeed`, a completed, turnless view item emitted
per watched line when the `monitor` tool runs with `show_lines`. The
adapter decodes it as a typed kind with no chat surface, like `hookRun`
and `userShell`; the line reaches the user through the agent's woken run.
Before that variant existed, the first feed item failed the session with
an unknown-variant payload error. Two other observed host contracts changed. `read_skill` is still in the default catalog but
1.4.10 `run.toolset` rejects it as a selector, while verified 1.4.6 and
1.4.7 artifacts still accept it at `session/start`. Selection is therefore
typed per audited host inside the backend: the served schema fingerprint
names the host, and only 1.4.10 marks `read_skill` `DefaultOnly` alongside
`write_todos` and `snooze_reminder`. Policy admission runs before the host
is known, so it refuses only selectors no audited host accepts; a named
toolset first identifies the host and reapplies the same admission with
that host's selection, so a host-only refusal returns the identical
guard, then derives the toolset and fails the spawn explicitly if the
launched host differs.
`real_native_tool_policies` caught the 1.4.10 rejection as a startup exit
on the derived deny-list toolset. A watched `monitor` command
completion wakes the agent in a new native run that can settle a provider
response with no text, reasoning or tool call. The adapter opens a client
stream only once a live response has something to publish, and neither
live nor replayed history publishes such a response, so the wake renders
no empty assistant message before or after resume.
`real_background_event_wakes_the_agent` (BackgroundTasks and
AgentInitiatedTurns) covers the wake, then resumes the session and checks
the replayed history. On 1.4.10 it failed first with one empty assistant
message and, once Muse's monitor used `show_lines`, with the `monitorFeed`
payload error. After the fixes it passes on Muse 1.4.10/Spark 1.3 (65.2s,
four feed items), Claude Haiku (50.7s), Codex (52.9s) and Hermes (56.4s). With the
1.4.10 fingerprint and these fixes, conversation, task list, MCP tool call,
native subagent, resumed session, background task, native tool policies
and hidden inference (Spark 1.3) pass on Muse 1.4.10.
The launch chooser bundles the official Muse SVG from
`https://muse.ai/images/favicon/app-squiggle.svg`, preserving its path and blue
gradient for both default and custom profiles without a runtime asset request.
Contributor mode is explicitly off. Model choices come from the account's
native catalogue: this subscription grants Wonder 1.0 and Spark 1.2/1.3,
with no separate Wonder Full catalogue entry. No unlisted model is advertised
or substituted.

Home conversations have no selected project roots. Muse maps this explicit
startup mode to `~/.tyde/muse/no-root`, using the same Tyde-owned working
directory convention as the other adapters. Project roots still go through
the existing strict validation. `real_home_conversation` exercises native
command execution before and after resuming with an empty project-root list;
the original adapter fails immediately at startup with the relocation-only
empty-root validation error.

Stable MSP transcript items omit pre-tool commentary. Native experimental
log paging also rejects retained omission markers. The adapter therefore
uses one ordered native session ledger for provider response ownership,
commentary, declarations and usage, alongside MSP's typed control and item
facts. It validates record/stream sequences, retention frames and the schema
fingerprint. This native-ledger dependency is not a stable SDK storage
contract; incompatible shapes fail visibly instead of synthesizing history.

Fork starts a fresh independent handle with inherited native model context;
it is not resume replay. Its validated checkpoint replaces inherited durable
run records with omission markers. Those markers are admitted only inside
the native fork-context boundary; inherited context is never presented as
new executable requests or fabricated provider responses. Resume uses the acknowledged native view head and
replays settled question requests and decisions. The distinct
`UserQuestionHistory` capability requires actual persisted settlements.
Claude's native user-rejection result also normalizes to cancellation on
replay. Codex's current adapter does not persist asynchronous settlements and
does not claim this additional history capability.

AskUser-only category exclusion and exact question-only deny lists negotiate
`initialize.capabilities.userInputDialogs=false` while retaining the native
default toolset. An empty deny list is equivalent to unrestricted policy;
without a category exclusion, dialog support stays enabled. Each spawn,
resume and fork opens a private native host with its own negotiation.
Other allow/deny policies use exact native toolset selectors and enumerate
the configured MCP servers to retain their non-denied tools without an
unsafe wildcard. The default catalog includes `write_todos`,
`snooze_reminder` and, on 1.4.10 only, `read_skill`, but the host rejects
those names as selectors.
Omitting them loses unrelated controls. `NativeTool.selection` therefore
separates `DefaultOnly`, `NamedSelectable`, `NamedWithoutMcp` and `Unverified`
from the tool's effect classification. Missing selection metadata decodes as `Unverified`,
not presumed selectability. Muse's shared audited description marks the two
controls `DefaultOnly`, the six audited `subagent_*` tools `NamedWithoutMcp`,
and the remaining ordinary native registry `NamedSelectable`.
An actual named native catalog also attests the selectors it used; a tool
merely visible in the default catalog does not gain that evidence. Claude's
catalog does not yet make a selection-admission claim.

`NamedWithoutMcp` is the current adapter's verified named-selection admission
limit, not a claim that the native tools are universally incompatible with MCP.
Exactly owned native ledgers show a default catalog of 33 tools followed by a
named configuration that denied only the two default-only controls but returned
25 tools instead of 31: all six subagent tools were additionally absent. The
native trigger is unknown; no alias or undocumented configuration knob is
assumed. The adapter now rejects an effective named set containing both an MCP
tool and any `NamedWithoutMcp` tool before connecting to the native host. It
checks the effective selected tools, not merely whether an MCP server is
configured, and never silently removes an unrequested tool. Default and question-only toolsets
still retain the MCP tools and all six subagent tools together.

Allow lists reject default-only or unverified native selectors. Non-question
deny lists must explicitly deny every default-only control; they cannot
silently drop one. Per-session MCP references additionally resolve against
the actual configured MCP inventory before the Muse process starts, rather
than trusting an arbitrary `mcp__` prefix. Default/question-only negotiation
does not acquire an extra discovery query. `real_native_tool_policies` stays
active: accepted policy catalogs retain exact equality, while full-catalog
allow lists and the original question-plus-MCP-only deny list must explicitly
fail pre-start admission. The final full-policy flow passed on Muse in
263.10 seconds with a 33-tool baseline after its MCP/question/read turns,
the exact nine-tool native-control selection, the configured MCP/read subset,
and exact denial catalogs. Conditional session controls remain part of those
native-only named selections; they are not relabeled default-only or omitted
from the equality oracle.

`real_native_local_work_waits_for_completion` now directly rejects the original
mixed named configuration through the production Backend before startup.
Its positive path explicitly denies both typed default-only controls and all
typed `NamedWithoutMcp` tools. The exact baseline-minus-eight name/kind equality,
all-`NamedSelectable` assertion, nine-second discovery proof, MCP/file liveness
and 35-second native-RPC proof remain required. This flow passed in 88.85
seconds. Restoring permissive mixed admission produced a real 8.43-second RED
at the unchanged pre-start rejection assertion. Restoring the 30-second RPC
cutoff produced a separate RED after an actual native response entered the
35-second delivery barrier. The unsupported mixed named combination is
quarantined, not advertised as repaired. Restoring unconditional dialog
support also failed the question-exclusion flow: the actual restricted
catalog retained the unwanted `request_user_input` tool.

Native `serve` does not load the managed hooks in the audited hosts: `hook/list` is empty,
no hook events fire, and real native tools execute despite hook denial.
The adapter does not rely on those hooks. Customization uses model-visible
MSP input parts; durable `displayText` keeps the original user prompt in
transcripts. Explicit `turn/steer` has no `displayText`, so it uses the
session's already-retained instructions rather than injecting visible text.
Muse now rejects `EnforcedReadOnly` before native startup and does not declare
that capability. Its prior broad named-toolset path cannot retain default-only
controls, and the filesystem-write contract does not authorize silently
removing unrelated session controls or explicitly allowed tools. A
default-catalog-preserving, flags-only replacement has not been verified
across the remaining mutation pathways. Advisory `ReadOnly` is unchanged.
The full native-policy flow also checks this unsupported mode's direct
Backend admission rejection; it is not represented as successful protection.
Muse emits `FileInfo.bytes: None` for both live and replayed reads because
the native result retains no read-time file size. It performs no post-read
filesystem lookup and never substitutes numbered presentation-text length.
The read contents and successful result remain intact if the file changes or
disappears before adapter completion. Claude history likewise omits sizes
that its native transcript does not retain. The mounted UI distinguishes an
unavailable size from a genuinely empty file. The real live-read regression
holds genuine successful native completion, removes the original file, and
forwards unchanged frames: fixed Muse and Claude pass, while restoring only
Muse's old post-read stat produces a valid RED. Multiple native Muse selections
use `selectedLabels`, not free text; the explicit legacy answer encoding still
rejects ambiguous comma-containing option labels instead of guessing selections.

Native question replies now carry typed question identities, selected label arrays,
free text and a separate optional note. The desktop and mobile widgets render
native min/max selection bounds and the MSP 500-character answer/note limits;
structured text controls preserve line breaks. Muse's MSP has no advertised
per-question flag to forbid free text, so the adapter does not invent one.
Legacy textual replies remain an explicit compatibility input, not the source
for structured selections. Native settlements own the typed completion and
replay; completed mobile controls are read-only. A real regression failed before
this projection and passes with comma-containing labels, multiline notes and
multiline free text through resume. Existing question lifecycle/history cases
still pass on Muse. This additional capability is advertised only by adapters
that actually provide structured answers; existing question gates are unchanged.

Native goals require an explicit native idle wake on resume. Goal preflight
reads the validated durable control ledger, not the cached unloaded-session
view: that view can omit a committed pause and otherwise wake a paused goal.
Active-goal wake is pipelined with resume so real turn starts can precede the
resume acknowledgement while historical replay still precedes live events.
The real paused-goal lifecycle failed before this correction and passed after
it on Muse; the same unchanged case also passed on Codex. Muse's controlled
active-goal resume race still passes. No goal assertion was relaxed.

MCP completion joins the stable tool terminal with the native committed result
and model-visible content. A new provider response or native turn terminal
seals that content; the earlier text-only task completion cannot discard late
image pixels. Generic completions use typed `NativeContent`: unmodified
native presentation text plus independently retained image pixels and media
types in native image order. Generated annotations are neither stripped nor
replaced, and the adapter does not synthesize canonical MCP block metadata.
A native failed terminal stays `Failed`, preserving its original reason and
`result: Some(NativeContent)`; success does not acquire an invented ambiguity
or image-loss failure. Known Tyde agent-control results retain their existing
typed normalization path. The strengthened shared success/error flow passes
with the complete literal, original successful pixels and native failure
status/text. The failed call has no native image payload, so Muse does not
claim `McpErrorImageResults`. Text-error and replay coverage remain separate
from that unsupported guarantee.
Direct image input additionally passed on Wonder 1.0 with the audited 1.4.7
host, not only the cheaper Spark profile.

The unchanged `real_mcp_image_text_fidelity` case has prior passing results on
Claude, Codex and OpenCode, which declare `McpOriginalContentFidelity`. Muse
does not declare that capability, and its retained strict RED remains an
explicit unsupported guarantee: a generated image annotation can collide with
genuine server text. Native audits with interleaved text and two images confirm that
neither MSP nor the retained ledger preserves original block boundaries,
`structuredContent` or `_meta`; outbound MCP requests carry no native call
identity for an independent exact result join. `NativeContent` honestly
preserves the available native text and images without claiming the missing
canonical shape or emitting an artificial normalization failure. Separating
this presentation from original-content fidelity does not repair or turn the
known Muse strict failure green. Full rich-result fidelity requires a native
retained-result surface and separate evidence for those additional guarantees.

The separate `real_mcp_image_lifecycle` case adds exact terminal replay and
model-context retention through resume and independent fork. Its lifecycle
requirements do not narrow the existing live-image case, which remains open
to OpenCode as well as Claude, Codex and Muse. Initial lifecycle runs passed
on Muse and Claude but caught a Codex MCP replay omission; it also reproduces
against unchanged mainline production code and is outside the Muse-only fix
scope. It remains outstanding, not successful cross-backend certification.

The original lifecycle seed already stated the image colors, so later color
recall could pass from text alone. The strengthened seed acknowledges only
storage and rejects color disclosures in text or reasoning. Fork runs before
any parent recall can contaminate the source with an answer. The strengthened
case passes on Muse and Claude; Codex still fails the exact MCP replay oracle.

A separate real user-image history flow preserves the original live-image
case and catches Muse dropping attachment bytes on resume. Stable MSP exposes
only attachment metadata; the native queued-input intake retains the actual
bytes. Projection joins intake to the user item by its exact command ID and
checks metadata against those bytes. Only the consumed queued-input schema is
pinned: unrelated native session-start intake uses a different schema version.

Native background stopping targets the exact tool item's task ID. Its typed
acknowledgement is admission-only; native terminal events remain authoritative.
A yielded bash result can report `background_running` while stable MSP leaves
`background` absent. The adapter uses that durable native fact plus the current
uncached `task/list` inventory, rather than closing the still-running command
with its response. Historical progress never grants a current cancel affordance.
Named cancellation rechecks the native inventory and validates the echoed stop
command and task. Real outliving-turn and named-cancel cases failed before this
projection and pass afterward. Task lists, usage accounting, subscription
capacity and plain-text resume also pass on the audited 1.4.7 host.

Survival across interruption of the still-active owning turn is unsupported:
native `turn/interrupt` kills that turn's work. A direct real-host audit also
admitted explicit `task/background`
promotion, then observed native cancellation and a missing command proof after
interrupt. Repeating that same native audit with normal-lane `turn/cancel`
also cancelled the promoted task and left the proof absent; switching stop
commands therefore cannot satisfy the background-survival contract. The
adapter does not synthesize survival or change the strict survival oracle.
The distinct, unchanged later-turn isolation check passed on Muse within
`real_interruption` (159.69 seconds): interrupting a later foreground turn
must not kill an earlier turn's background command.

`BackgroundTasks` promises only that commands can outlive their launching turn.
The separate `InterruptPreservesBackgroundTasks` promise keeps the existing
strict survival cases on Claude, Codex and Hermes. Muse instead declares
`InterruptCancelsOwnedBackgroundCommands`: a still-active owning turn shares
cancellation with its background commands, without claiming session-wide,
subagent or workflow cancellation. Capability validation requires an explicit,
non-conflicting scope when interruption and backgrounding coexist.

The new `real_interrupt_cancels_owned_background_command` scenario requires an
actual running process, native background progress, an interrupted streaming
answer, process death without the unreleased completion proof, and exactly one
cancelled card before closing the host. Same-session followup and exact replay
must also work. Its initial run failed replay in 13.99 seconds: the adapter
ignored the owning run's cancellation terminal and left its response open.
Consuming that authentic terminal fixed the flow (20.16 seconds), including
live cancellation, followup, exact replay and resumed followup. The strict
owning-turn survival cases separately passed on Claude, Codex and Hermes.

The native-policy flow also preserves a genuine `read_file({})` declaration,
its native missing-path failure, same-session recovery and exact replay.
Restoring fatal missing-field normalization produced a real 40.94-second RED
before recovery. This is valid JSON with missing required arguments, not
syntactically malformed JSON. Almost-JSON and non-object argument recovery
remain unverified: bounded real attempts and inspected native declarations
did not produce those inputs. No scripted provider or fabricated event is
counted as coverage for that path.

The new user-image history case passes on Muse. Cross-backend runs expose absent
image replay on Codex, Hermes, Kiro, Grok and OpenCode; Claude stopped earlier at
the live pixel-identification oracle. Antigravity is ineligible for this flow.
These results are retained, not counted as green cross-backend certification,
and other-provider fixes are outside the user's Muse-only scope.

Muse-native `subagent_spawn` now uses normalized spawn cards and the shared
`SubAgentEmitter` relay interface. Each returned handle can supply a scoped
emitter for its own descendants. Native accepted parent-run identity and the
attested provider call resolve the immediate owner; native `parentSessionId`
and `depth` are not used to reconstruct the tree, because Muse stores nested
control records in the outer session and reports flat physical provenance.

Each child follows its explicitly bound session/log and opaque MSP view cursor.
Muse rejects live view subscription for these unloaded native child sessions,
so file-append notifications drive native view paging without polling, directory
searches or guessed paths. The same production projection handles each child's
messages, reasoning, tools and per-request usage. Initial replay is bounded by
the acknowledged native snapshot; actual terminal metadata does not resurrect
finished work, while an unfinished response hands over to a live stream.
Child events reach their relay before descendant registration, matching the
host's parent-card publication barrier. Native terminal events own completion.
The snapshot handoff regression distinguishes completed attempt A from active
attempt B: its original run failed in 12.56 seconds; the fixed flow passed in
21.61 seconds without reopening A or losing B's live response.

The existing sibling-delegation scenario failed before normalization and passes
on Muse (17.81 seconds). A new `NativeSubagentHierarchy` scenario checks two
native generations, immediate-parent cards, separate conversations/accounting,
a yielded shell, exact replies and resume without new live historical children.
It also reproduces the rendered-app child `task/list` failure before its fix
(7.90 seconds), and passes afterward (38.78 seconds). An observed child has no
loaded root task-control plane: it preserves native execution-state facts but
does not query that inventory or offer an unsupported direct cancel button.
Native child conversations retain the shared read-only relay contract; parent
native collaboration tools remain the control surface.

The existing nested-ownership scenario's Muse prompt now explicitly names native
spawn/wait/read tools instead of allowing a mistaken Tyde MCP spawn with backend
`default`. Its assertions and other provider prompts are unchanged. Runs against
all six eligible backends pass Muse (19.14 seconds), Codex (13.86), Grok (43.06)
and OpenCode (26.67), but fail Claude at native-child discovery and Hermes at
child completion. These failures remain red; they are not Muse certification
passes or permission to weaken the common scenario. The hierarchy reply oracle
accepts exact assistant content from both replay `MessageAdded` and live
`StreamEnd`, rather than rejecting correctly rendered live replies.

Scoped rendered QA on Linux, Muse 1.4.7 and Spark 1.3 low observed all three
cards Thinking with the command proof absent, then all three Idle with the
proof present. Two 16-pixel indentation steps and both parent/descendant tool
links were rendered, each conversation retained its own final reply, child
input was read-only, and no error appeared after the fix. The initial long-shell
UI failure was retained and added to real conformance rather than counted as
passing. Both disposable instances were stopped, their stores removed, and
owned native processes verified absent. Later retained captures cover the
desktop question/read/native-output layouts, phone-width loader and native
output, and the corrected Muse profile flow. The latter also launched a real
Tyde-managed Muse child and verified both native model identities, lineage,
working-to-idle transitions and exact child response. These are scoped QA,
not a claim of full backend certification or physical-device testing.

Mid-turn steering is not advertised: the native host backgrounds the
conformance command after ten seconds even with a longer foreground yield.
Manual compaction now uses native `session/compact` admission and the durable
command target, candidate installation and command outcome. Busy turns and
background work defer dispatch. Installation identity correlates the terminal
and observation into one timeline entry; estimated native budget snapshots do
not become a trusted provider occupancy reset. Focus text is rejected because
MSP does not expose that parameter. The unchanged compaction/resume scenario
failed before this transport was implemented (17.44 seconds) and passes on
Muse 1.4.7 after the fix (64.37 seconds): idle dispatch, busy-turn deferral,
tool continuity, retained steering and native resume. Its first fixed run also
caught internal compaction events being decoded as chat events; the adapter
now routes the typed compaction envelope through the backend event boundary.
The slow-compaction regression additionally held genuine native terminal
delivery beyond the old 300-second cutoff. The restored expiry failed in
323.64 seconds; the fixed durable-outcome flow passed in 341.51 seconds.
Automatic compaction, native workflow presentation, full background-task
control, slash commands and native skill projection remain uncertified.
Tyde-managed agent tools remain a separate MCP capability. A passing ordinary
repository check is not a real-provider certification; retained scoped runs
must also pass before this workbench is ready.

Hidden inference helpers (supervisor verdicts, agent naming, usage wake-up)
run Muse in a dedicated host: an untrusted workspace, contributor mode
off, a text-only `run.system_prompt`, `run.toolset=[]` (a named mode with
no active tools), an empty `run.reminder_roster` (Muse's autonomous
observers), workflow triggers, web search, web fetch and artifacts
disabled, and no user-input dialogs. All of these are documented settings;
`muse config validate` reports the `run.*` keys as active and
user-overridable on 1.4.10. Only fresh sessions with an empty
allow-list and no MCP servers are admitted. The adapter fails closed if Muse
reports a non-empty or non-named toolset, commits a tool call, or emits any
item other than user, assistant or reasoning text. Agent-mode empty allow-lists
are still rejected. Passive user rules and memory context may still be
injected into the prompt, and helper sessions are durable native sessions.
`real_hidden_inference_tool_free` failed before this path existed with the
production startup error ("no verified empty named toolset") and passes on
Muse 1.4.7 in 5.09 seconds with an empty provider-confirmed catalog and a
text-only reply to a tool-provoking prompt. On 1.4.10 the catalog stays
empty and no reminder children run, but Muse's provider-default system
prompt (about 46k characters) still documents its namespaced native tools
(`muse.read_file`, `muse.bash`, `muse.edit_file` and others), and the Meta
provider only sends `tool_choice: auto`. Spark 1.3 therefore called those
absent tools in 6 of 7 hidden sessions; Muse rejected them as unknown and
the adapter failed the helper closed. The helper host now also passes
`run.system_prompt`, an active, user-overridable setting that replaces
the provider default (`base_instructions_source` becomes `settings`), with
a short text-only instruction. Hidden sessions then carry no `muse.*` tool
text, and the case passes 3 of 3 on the default Spark 1.3 with every
existing check retained. `config validate --plane defaults` and a direct
`session/start` with the override on verified 1.4.6 and 1.4.7 artifacts
show the same active, overridable `run.system_prompt`.
Unadvertised native capabilities and the explicitly recorded cross-backend
failures remain limitations, not passing conformance. Ordinary repository
validation, retained real-provider evidence and focused review are separate
requirements; none substitutes for another.

## Codex polling after native rejoin

`real_watched_command_shows_every_interaction` holds a real command on a FIFO,
requires three empty still-running polls, then yields a fourth polling code
cell and observes an empty native wait before explicitly releasing the command.
It checks fresh, resumed and forked sessions with the same prompt and oracle.
Native process identities must agree, every poll completes once, the yielded
cell retains its continuation identity and empty content, and the final wait
carries the command's real output. Successful cells deliberately print a
failure-looking line so stdout cannot override the native execution outcome.
Only Codex currently declares the case's yielding capabilities.

The same flow admits a nested command only after its original model slot has
been published, then lets a second nested command outlive the turn. FIFO
release follows actual tool admission and idle events, not elapsed time. Its
late terminal receipt must complete the original command card exactly once.

On Codex CLI 0.162.1, ordinary native resume delivered typed command and usage
events but no raw polling declarations. The unchanged adapter therefore showed
the fresh session's three empty polls and zero after resume, despite the native
provider record containing the actual calls and results. Native provider
history replay is text-oriented and is not Tyde's saved canonical transcript;
this fix does not reconstruct live calls from that history.

All rejoined Codex sessions now bind the existing process-owned execution
source. Original model slots, native code-cell replies and direct continuation
results feed the adapter's canonical response/tool pipeline. Runtime-to-model
slot correlation uses native identities, including commands admitted after a
code cell yields. Full native code-cell replies preserve `Result`/`Yielded`,
cell identity and content separately from execution success. Missing required
native results fail explicitly at a completed-turn fence. Typed command items
still own command output and exit/cancel outcomes; native assistant messages
use the same command-promotion path as their stdout equivalents. No shared
server/UI lifecycle or provider-specific rendering logic changes.

A rejoin without the required native observation transport fails explicitly
rather than silently dropping polling activity. This includes SSH-launched CLI
processes that cannot expose their process-owned trace. A Tyde server running
locally on a remote host is not that unsupported launch mode. Existing saved
messages that already omitted calls are not backfilled from provider history.

The regression failed against the original adapter after resume (zero poll
cards) and passed the complete extended flow on CLI 0.162.1 with Luna/low on
2026-10-23: nextest `3368a93a-3b13-47e0-8686-4f9023bb7c52`, 97.802 seconds.
No other backend declares the required yielding capabilities. The isolated
source build took 5m46s and is not a warmed validation-gate measurement.

## Claude reviewer native configuration

Enforced read-only Claude launches use native safe mode with user settings
only (`--safe-mode --setting-sources user`), not an empty `--setting-sources`
list or bare mode. Claude keeps the user's auth, provider and model
configuration (including `apiKeyHelper`, OAuth/keychain and settings `env`),
while suppressing ambient plugins, skills, agents, user hooks, LSP and other
executable customization. Safe mode still runs credential helpers, so the
reviewed workspace's project and local settings stay excluded: a helper there
is workspace-controlled code, possibly part of the diff under review. Auth
defined only in those files is not available to enforced reviewers; define
it at user scope. Native read-only tools, `dontAsk`, user hook suppression,
strict MCP isolation, delegation exclusions and per-process tool catalog
verification remain enforced. Ordinary launches are unchanged. Enforced
read-only sessions with explicitly selected Tyde skills are rejected before
materialization: safe mode cannot load them, and starting a degraded session
would misrepresent the selection.

Safe mode also suppresses explicit `--mcp-config`. Before admitting a turn,
the Backend registers the exact host-owned MCP set through Claude's native
`mcp_set_servers` control and checks its acknowledgement and authoritative
connected state. Rejection, partial registration or missing authorization
fails startup; there is no less-isolated retry. Fresh, forked and resumed
processes share this policy. Claude emits session-bearing frames before
initialization, so restricted fresh and forked spawns, like resume, finish
this verification before admitting the initial prompt or reporting ready.
Provider configuration is not copied or
reinterpreted by Tyde. The user's auth helpers are necessary credential
operations, not reviewer-selected tools.

Administrator-managed policy is outside the reviewer's customization boundary.
Claude documents that safe mode still applies managed settings, including
policy-configured hooks, status line and file-suggestion commands, and that
only a managed-level `disableAllHooks` can disable managed hooks. The previous
empty `--setting-sources` launch applied the same managed tier: owned native
2.1.296 controls ran a managed `SessionStart` command under both launch
policies. Those commands are organization code, not reviewer capabilities; the
reviewer's own catalog stays read-only. Tyde neither masks nor parses managed
policy. Bare mode skips such hooks but discards OAuth/keychain auth, so it is
not used.

`real_claude_review_configuration` is a Claude-native configuration regression,
like the existing Codex-native settings case, not a cross-provider settings
schema. It requires explicit `TYDE_TEST_CLAUDE_API_KEY`,
`TYDE_TEST_CLAUDE_BASE_URL` and `TYDE_TEST_CLAUDE_MODEL` inputs. An owned native
home supplies credentials only through `apiKeyHelper` and provider/model
routing through settings. The workspace's project settings define a competing
`apiKeyHelper`. An ordinary real Backend turn positively exercises the user
hook, ambient MCP and workspace helper. Fresh, forked and resumed enforced
reviewers must authenticate through the user's helper, report the
settings-routed provider model in native responses, complete dedicated
feedback calls, leave protected files unchanged, expose no
mutation/delegation/ambient tools and execute no user hook, MCP process or
workspace helper. The original settings must remain byte-identical.
The fresh reviewer also calls both advertised native control tools once.
Real Claude refusals are tool-specific: `AskUserQuestion` is denied because
the CLI is in don't-ask mode, and `ExitPlanMode` reports that it is not in
plan mode. Neither says `No such tool available`. The case requires these
specific refusals, not arbitrary failures; unavailable mutation/delegation
tools must still report the exact attempted tool as unavailable. Successful,
cancelled or unnormalizable forbidden calls remain failures. A real-provider
native control run demonstrated both refusals and that the former blanket
unavailable-tool assertion rejects them. This does not identify the tool in
the earlier fork failure, whose identity was not retained.
Both control refusals must survive native resume replay unchanged.
Assistant wording is not an isolation oracle: a real Haiku resumed turn
completed dedicated feedback, one read and one exact native unavailable-tool
denial without a forbidden success, but omitted at least one requested
`WRITE_BLOCKED`/`DELEGATION_BLOCKED` token. The former prose conjunction
rejected that turn. Isolation is instead required by the strict native
outcomes and catalog, protected file bytes, child observations and executable
customization markers; none of those assertions depends on model prose.
The real Backend refusal regression also exposed a normalization defect:
the adapter converted every native `ExitPlanMode` error into success and
discarded the error text. Native plan-exit errors now remain failures in
both live events and history. Plan-approval decisions still complete through
the existing control bridge; `EnterPlanMode` handling is unchanged.
The case installs no administrator-managed policy and does not claim managed
hooks are suppressed. No credentials are copied to the fixture or retained in
its diagnostics. The shared `real_enforced_review_feedback` and
`real_enforced_read_only` cases continue to cover transport failure and native
restriction contracts.
## Feedback analysis

Feedback analysis reuses the existing naming/summary utility path:
`agent_name_generation_spawn_config` and `Backend::spawn` in `InferenceOnly`
mode. All eight supported backends are eligible; the selected backend and
model retain their normal provider/auth routing. Protocol 90 replaces the
misleading analyzer capability name with `TextAnalysis`. Read-only means best-effort
no-action guidance, not a new containment guarantee. `EnforcedReadOnly` and
ordinary coding/review launches are unchanged.

The owner explicitly narrowed this feature from provider containment to text
analysis. The unlanded protected-launch engines, ambient-executable canaries
and compression/isolation assertions were archived and removed, not treated
as unrelated contracts to weaken. Existing title/inference/enforced-access
conformance remains unchanged. `real_feedback_analysis` covers typed replies,
semantic overlap, generic guidance and reuse through the existing utility
configuration, with identical scenarios/assertions for all eight backends.
Invalid JSON or provider failures are explicit failures; there is no parser,
provider or authentication fallback. Source eligibility is not live proof:
report the actual scenario/provider receipts separately.


## InferenceOnly helpers preserve selected settings

`real_session_settings` now opens an ordinary InferenceOnly helper with the
fixture's selected settings, checks two completed text turns against the
unchanged universal model pin, closes it, and runs the existing normal-agent
settings flow. It uses the existing supervision configuration, whose model,
cost hint and InferenceOnly configuration match the naming helper; it adds no
helper mode or title-generation behavior.

The real Codex negative control failed on the helper's model pin: selected
`gpt-5.6-luna` reported `gpt-6.1-sol`. Codex discarded configured settings at
startup and omitted model/effort overrides when starting InferenceOnly turns.
Both paths now propagate the existing resolved settings, retaining the helper's
existing approval policy. Normal-agent startup and all other adapters are
unchanged.

The entire extended case passed on all eight SessionSettings-capable backends:
Claude, Codex, Kiro, Grok, Hermes, Muse, Antigravity and OpenCode. These are real
Backend runs, not title-generation certification or rendered-UI coverage. Codex
used its configured native route with Luna/low; Claude and Hermes used the
approved small-test API credentials without copying or modifying native OAuth
or global configuration. The other backends retained their normal profiles.
Content-safe receipts and complete private diagnostics are retained under
`/home/tyggs/tyde-evidence/codex-helper-selected-model/`.
