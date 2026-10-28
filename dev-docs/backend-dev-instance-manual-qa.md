# Backend Dev-Instance Manual QA

Use this workflow to certify a real agent backend through the rendered Tyde
desktop application. This is an end-to-end product audit, not a smoke test and
not a substitute for deterministic backend tests or `./dev.sh check`.

Real backend turns can spend money. Necessary testing has standing
authorization under `AGENTS.md`; no per-run permission is required.
Certification favors feature coverage, but it must obtain that coverage with
the cheapest suitable models, bounded prompts, and bounded commands.

## Frontend startup identity

The canonical dev-instance launcher builds once with `trunk build` into a fresh,
private `frontend-dist`, then serves that snapshot on a producer-owned ephemeral
loopback listener. Restart the disposable instance to pick up source changes.
Ordinary development `trunk serve` is unchanged.

Readiness requires ordered, typed build/serving receipts on the owned child's
stdout pipe, followed by one HTTP index request whose private serving identity
and SHA-256 match the receipt. Compiler output is captured from stderr, never
interpreted as readiness. A build failure, missing index, foreign responder or
index mismatch fails startup and joins owned-process/output cleanup. There are
no build deadlines, readiness sleeps or HTTP retries.

The ready result includes `frontendBuild.protocol` and
`frontendBuild.indexSha256`. Record them with the exact source commit and the
canonical `frontendUrl`; do not export the private HTTP identity header. Keep
cold compilation/setup and warm launch durations separate from UI-action
measurements. A build or readiness failure leaves UI actions unmeasured.

## Certification contract

A run certifies one exact combination of Tyde commit, backend CLI version,
provider, access mode, reasoning level, host OS, and frontend build. Record the
model used for every case. A run may use more than one model when the cheapest
default model does not expose every backend capability.

For full certification, run every applicable case independently for each
backend being certified. For a scoped release audit, assign the case IDs below
before spending money and report exactly that scope, not full certification.
Evidence from one backend cannot satisfy a case for another; shared setup
evidence is the only exception. This document is the canonical manual matrix,
not a request to run the paid conformance suite speculatively.

Do not infer one visible state from another. Seeing correct final output does
not prove that the agent was shown as active while it ran. Seeing a completed
tool card does not prove that it was shown as running first. Every temporal
claim below must be observed while that state is held and again after its
transition.

Hold transient work on an explicit observable barrier; see **Event barriers**
below. Never synchronize with sleeps, arbitrary delays, host-speed thresholds,
or retries until green. If a required interval cannot be captured, report
`BLOCKED` (missing observation/control) or `NOT TESTED` (not exercised), not a
pass inferred from the final state.

For every applicable case, inspect all rendered surfaces that expose it:

- the agent card in the sidebar and Agents view;
- the selected Smart View/filter membership and authoritative status icon,
  label, and style (idle is not a durable “finished” lifecycle);
- the open chat, streaming/reasoning area, input, stop control, and header;
- the tool, task, sub-agent, workflow, or in-flight card;
- the in-flight tray;
- a second connected client when the case calls for one.

An unsupported capability may be marked `N/A` only after recording why the
backend cannot expose it through any available model in the selected mode. A
limitation of the chosen cheap model is a reason to select the next-cheapest
capable model, not a reason to mark the backend feature `N/A`. An unattempted
applicable case remains `NOT TESTED` and blocks certification.

### Missing functionality is a failure

Certification is closed-world: every applicable matrix row and every required
rendered surface must have an explicit result. Missing UI, missing events,
missing progress, missing terminal state, or a feature that works on one
backend but not another is `FAIL`; it is never silently omitted, inferred from
the final answer, or downgraded to an observation.

For example, if Codex starts a background command but the in-flight tray never
shows its running row, the background-command case fails even when the command
eventually returns correct output. Claude passing the same case does not cover
Codex. The tester must record and report the Codex failure.

Every run report must include all `FAIL`, `BLOCKED`, `NOT TESTED`, and justified
`N/A` results. A QA agent must not return an overall pass, a shortened list of
only successful cases, or a generic “looks good” verdict while any required row
is missing. Discovering one failure does not authorize skipping the remaining
matrix; continue when safe so the report exposes the full release risk.

### Cost-controlled model selection

The purpose of this workflow is to certify Tyde's backend integration, not to
benchmark model intelligence. Use deterministic prompts that explicitly ask
for the feature under test rather than relying on a powerful model to infer the
test plan.

1. Choose the cheapest reliable model exposed by each backend. For this audit,
   select the discovered **Codex Luna** model at low reasoning and
   **Claude Haiku** at low effort. The original audit requested `gpt-6-luna`
   and Haiku 5.5; these names do not guarantee the current installed catalog.
   These are separate backend lanes, not interchangeable providers. Confirm
   discovery and actual model events; Haiku 4.5 does not certify a requested
   5.5 run. For other backends use their discovered catalog and record the cost
   basis, not an assumed universal cheap model.
2. Run every feature the default model supports on that model, including all
   lifecycle, replay, cancellation, concurrency, and multi-client cases. Do not
   reserve cheap models for a reduced smoke pass.
3. If the default model lacks a required capability such as image handling,
   extended reasoning, native task tracking, or native sub-agents, switch only
   that capability and its adjacent regression cases to the cheapest model on
   the same backend that supports it.
4. Do not select an expensive or production-default model merely because it is
   more capable conversationally. Use it only when no cheaper model can emit
   the protocol feature being certified.
5. Record the model, reason for selection, request/usage totals, and estimated
   cost for every case. The evidence index must make model switches explicit.
6. A backend is not certified until every feature exposed by at least one of
   its available models has been exercised. Passing all features of one cheap
   but capability-limited model is insufficient.
7. Certification applies to the backend feature paths and the models actually
   recorded. It does not claim that untested models produce equivalent answer
   quality.

## Release-audit handoff and stable case IDs

### Audit baseline and highest-risk gaps

Current source audit: `d8ff01f2` (2026-10-19), after stable publication;
the original matrix audited `7485d940`. This is a coverage inventory, **not**
a claim that these manual cases passed on either commit. The landed Vibe/
Dinosaur sidebar, Inline/team reviews, mobile double-tap fix and opt-in
self-improvement are included below. Still-parked usage/image features are
excluded. Record the exact tested host **and** frontend commit; publication
of stable does not certify newer main UI. This docs refresh runs no provider
conversations or manual product QA.

`be3c29ca` already fixes macOS cleanup; `d2ab1572` already requires desktop
startup smoke for every release target. Keep that packaged-target gate;
Linux DOM QA is not macOS/Windows startup evidence. New macOS archive/recovery
coverage (`f8f032c6`, `880a4250`, `33506519`) remains owned by release tooling,
not another manual download/build/promotion lane in this guide.

The old guide covered broad happy paths but had no stable assignment/evidence
keys, prescribed sleep-based observation, treated every provider too uniformly,
and omitted explicit review reuse/startup-close, host-role, pinning, stale-diff,
and mobile regression oracles. It also incorrectly excluded existing operator
chats as controllers and assumed same-store restart and visual drivers existed.
The additions below reuse sections 1–16; they do not create a parallel suite.
The separately owned pinned-Tychat lifecycle investigation is QA-14b only here,
not an invitation to duplicate its source fix.

**Original economic tester split (reassign explicitly for a new run):** Luna
carries `QA-00`–`QA-06`, `QA-09`, `QA-13` and `QA-14a`; Haiku 5.5 carries native children/workflows and `QA-07`–`QA-12`,
with Claude parity on shared flows. Each owns a separate ephemeral instance.
`QA-11` requires an actual Muse session/model, not Haiku evidence; `QA-14b`
and `QA-15` require their restart/platform prerequisites. Assign cross-backend
parity explicitly before claiming full certification. `QA-16` is mandatory
across **every** exercised session/backend path, not a one-time presentation smoke. Previewing review layouts and checking sidebar
presentation can reuse existing sessions without a fresh model turn. Workers
do not control siblings. Run one bounded flow per row, combining adjacent checks
and reusing its output for replay and all detail levels. No speculative paid
conformance, broad model sweep, or repeated full certification for this audit.

### Existing coverage to reuse, not rerun speculatively

| Cases | Existing boundary coverage inspected | Remaining manual observation |
| --- | --- | --- |
| QA-01–04 | `tests/tests/conformance2.rs`: `real_tool_type_mappings`, `real_watched_command_shows_every_interaction`, `real_background_task_outlives_its_turn`, `real_mcp_tool_call`, `real_mcp_error_result`, `real_mcp_image_lifecycle`, `real_native_tool_policies`, `real_enforced_read_only` | Real-provider contracts do not show rendered active/tray/detail states. MCP probe generators (`mcp_probe_script`, `mcp_image_probe_script`) live **inside** this suite, not as a standalone UI driver; install any adapted probe in the dev instance **before** spawning its agent. Replace timed fixture holds with event gates for manual observation. A fake MCP tool is acceptable; a fake provider is not backend certification. |
| QA-05–06 | Conformance `real_user_question/history`, `real_structured_user_question`, `real_async_user_question`, `real_async_answer_completion_overlap`, `real_session_settings`, `real_session_speed`; `server/tests/settings.rs::running_agent_settings_carry_the_schema_the_agent_validates`; question-card Wasm flow | Typed answer/no-echo, replay and model gates already have oracles. Still inspect actual double activation, custom/multi answers, live level switching and mobile running settings. |
| QA-07–09 | Conformance `real_native_wait_excludes_completed_children`, `real_native_child_hierarchy`, `real_native_workflow`, `real_nested_subagent_ownership`, `real_agent_await_survives_a_resumed_session`, `real_slow_native_compaction`; `server/tests/agents.rs` close/late-subscriber flows | Native and managed ownership, phase gaps, completion wake-ups and terminal bootstrap must also agree on rendered surfaces. CLI-native children may reject direct input/resume/cancel; don't require controls their protocol does not advertise. |
| QA-10 | `server/tests/review.rs::review_modes_share_persisted_aspects_and_exact_source_assignments`, `review_settings_migration_preserves_customization`, `advisory_review_reuses_context_and_keeps_compact_audit`, `advisory_review_reattaches_and_exposes_failures_and_configuration_changes`, `project_reviews_stay_responsive_during_reviewer_startup`, `mixed_source_comments_keep_identity_and_file_revision`; conformance `real_enforced_review_feedback` | Review sim owns mode/plan, deterministic startup-admission/Stop and nonregular-file oracles; `real_review_context_forks` covers actual Codex/Claude native forks, not rendered UI. A real UI run without those controls cannot claim to reproduce the exact interleaving. Read [review semantics](18-review.md), not old request/await aliases. |
| QA-12/14a sidebar | `frontend/src/components/workbench.rs::sidebar_modes_keep_one_agents_mount_and_independent_layouts`; `agents_panel.rs::server_project_entry_moves_the_visible_chat_into_its_workbench`; [sidebar contract](28-sidebar-grouping.md) | Real rendered mode/layout, pin filtering and workbench-tab ownership still need scoped observation; no new paid turns just for presentation. |
| QA-06b feedback | `server/tests/self_improvement.rs::busy_human_inputs_suggest_apply_automatic_edit_and_conflicting_undo`, `disabled_unsupported_replay_and_restart_do_not_schedule_or_apply`; mounted settings flow | Disabled/no-call, project scope, proposal/apply/undo and original-input identity need distinct evidence; Analyzer availability comes from canonical server state; read-only is best-effort no-action guidance, not containment. |
| QA-11–16 | Conformance `real_hidden_inference_tool_free`; existing `agents_panel`, `diff_view`, desktop/mobile `tool_card` Wasm flows; mounted mobile AppSurface; [mobile platform evidence](mobile-web-pwa.md) | Pinning/diff/status and three levels already have DOM coverage, not full backend/platform certification. Recorded mobile long-background/PWA keyboard-resume remains **unconfirmed**; focus-zoom evidence does not close it. No image-viewer pinch feature landed. |
| QA-00/platform startup | `tools/smoke_desktop_release.py`, `tools/smoke_windows_release.ps1`, [mandatory release startup smoke](33-pretag-release-build.md) | Packaged-target startup is a separate release gate, not proven by this manual audit or by changing docs. |

These are inspected source oracles, **not new execution results**. The retained
[backend conformance record](backend-conformance.md) also contains scoped
cross-backend failures (for example MCP replay and nested ownership); don't
turn historical evidence into a current pass or assume a backend-specific fix
closed another provider's gap. Report the actual tested commit and case.

### Available drivers and hard limits

| Existing surface | What it actually observes | Certification limit |
| --- | --- | --- |
| `server/src/debug_mcp.rs`: `tyde_dev_instance_start/wait/list/stop`, `tyde_debug_evaluate/events/snapshot/capabilities` | Isolated desktop instance; Promise-aware rendered DOM evaluation; cursor-based **process output**; process/readiness snapshot | The audited built-in driver advertised `screenshot: false`, `secondClient: false`; query current capabilities for the actual run. No `tyde_debug_query_screenshot` or product-event subscription tool. Process output/snapshot is not a chat protocol trace or a console collector. |
| `frontend/src/devtools.rs`, `devtools-protocol/src/lib.rs` | Typed UI evaluation transport; evaluation awaits Promises | DOM is evidence, not a screenshot. Use an attached browser/native driver for pixels, console/page errors, input and independently controlled clients; record its identity and prove it targets the child instance. |
| `playwright.mobile.config.mjs`, `mobile-frontend/e2e/` | Real mounted mobile DOM with deterministic fixture state; WebKit composer flow; viewport probe | No real host/backend. Useful coverage inventory, not real-provider or native-phone certification. Run repository validation only through `./dev.sh check`, not these configs directly. |
| `playwright.mobile.live.config.mjs`, `mobile-frontend/e2e/live/` | Deployed mobile login, pairing, connection and reload with a persistent sensitive browser profile | Does not send prompts or certify this workbench. Never reuse its ordinary paired host/profile for this audit. A separately verified disposable profile paired only to the ephemeral host is required. |
| Native desktop/phone automation | Native picker, clipboard, drag, keyboard, gestures, painted pixels, app resume | Only when actually available on the named OS/device/build. Desktop emulation and synthetic DOM input cannot satisfy native cases. |

Call `tyde_debug_capabilities` first. Required screenshots or a second client
without a verified external driver are **BLOCKED**, never fabricated. Continue
safe DOM-only subcases and label their narrower evidence; do not report their
parent row or the backend as certified. Likewise, the current start API accepts
no existing-store option and stop deletes its stores. Stop/start is a **fresh**
instance, not a same-store host restart. The restart subcase is `BLOCKED` unless
a separate verified isolated harness retains precisely those disposable stores.
Do not add a driver or a second giant suite just to hide these limits.

### Reload-driver gap: share the blocker, not an inferred pass

Historical external Haiku-lane checkpoint (original 2026-10-18 audit, **not**
a current-main pass): actual `claude-haiku-5-5`, low effort;
lifecycle, files and foreground/background held/terminal observations passed
at Summary, Compact and Full. That is scoped tester evidence, not full-backend
certification or an execution result of this documentation work item.
On **two distinct clean instances**, `location.reload` lost evaluation: the
first renderer exited; the second renderer stayed alive but evaluation hung.
The tester owns deduplicating and filing that driver issue. No third repeat or
new paid turn is required to reconfirm it. These observations establish a QA
control/observation gap, not successful protocol/app reload and not a diagnosed
product root cause.

Record one shared blocker (for example `DRIVER-RELOAD-01`) and link it from each
affected QA-09/reload and QA-16 level/phase record. Keep their results BLOCKED;
do not demand another clean setup for every row or erase earlier successful
held/terminal evidence. Continue safe in-app close/reopen through History and
record exactly whether it reopened a view or explicitly resumed a session.
That can satisfy the corresponding reopen subcase, **not** page reload,
reconnect or same-store host restart. Preserve the separate outcome for each
phase and all three levels. Do not control/restart another tester's instance
or delete the shared implementation workbench while testers still use it.

### Startup transport gap: reconcile one attempt, never duplicate it

Historical preflight evidence from the original audit, not a fresh launch in
this docs refresh: no instances existed; one
`tyde_dev_instance_start` for this workbench exceeded the MCP client's
`tools/call` wait at **300s**, returning no instance ID. A list during pending
startup was empty. Later, list recovered the **same single attempt** as running
with ephemeral-store attestation and the session-store path beneath storeDir;
the coordinator assigned it to the Luna tester. No second start was made.
This proves that this client timeout did **not** cancel startup. It does not
by itself certify rendered UI, backend readiness for turns, screenshots, or a
second client, and does not diagnose the cause of the slow startup.

The canonical server launcher now registers an actor-owned attempt **before**
launching work. `tyde_dev_instance_start` returns its starting handle promptly;
it does not certify readiness. `tyde_dev_instance_list` includes pending and
failed attempts, with exact `instanceId`, `projectId`, `projectDir`, start time
and typed `status`. Pending state carries a startup phase, not inferred endpoint
or store attestations. Output/events and snapshots work before readiness.

Call `tyde_dev_instance_wait` with that exact `instance_id` to await the same
attempt's completion. This is an event wait with no startup deadline. A lost
MCP response or disconnected waiter does not cancel actor-owned work. Recover
a lost start handle from the exact root/owner in the inventory and reattach;
never call start again to discover the result. Duplicate source starts fail
explicitly. Do not infer cancellation or absence from a client timeout.

Before creating an attempt or starting a child, the launcher requires the typed
`devtools-protocol/dev-instance-startup.json` contract in the exact source
checkout. Missing/unsupported contracts fail explicitly; merging the launcher
fix into the resident alone does not make older consumer checkouts launchable.
The declaration and matching notification producer must travel together.

The child sends typed, identity-bound host-listening and UI-ready notifications
to the owning launcher. The launcher performs the real host handshake and typed
UI Ping/Pong once notified, while observing child-exit events. UI startup
notification ownership is consumed once; later webview boots keep their normal
listener readiness without reconnecting to the retired startup endpoint. An incompatible
host protocol, malformed notification or failed UI response becomes an explicit
terminal failure, not a discarded probe and infinite retry. Failed state retains
its phase, typed failure kind, diagnostics and any cleanup error; wait returns
that same JSON state with MCP `isError: true`. Ready state alone exposes canonical
store/containment and endpoint attestations. No total build deadline is added.

Terminal results and bounded output remain observable for the owner's lifetime,
including cancelled startup when a workbench is removed. Explicit instance stop
joins process/resource cleanup and removes the attempt; failed cleanup remains
an error, not an empty success. Do not stop/control an instance after the
coordinator assigns it to a tester. Transfer cleanup responsibility explicitly.

These guarantees apply to the updated **server-hosted** launcher and matching
child source, not the separate legacy stdio driver. They survive MCP client
reconnection, not replacement of the owning server process. A workbench edit
cannot update an already-running launcher, adopt its unregistered child, or
retroactively provide its missing completion handle. Updating resident controls
requires explicit approval at a safe owner-coordinated checkpoint; do not restart,
relaunch, signal or migrate an existing pending attempt to apply this fix.
`screenshot: false` and `secondClient: false` remain unchanged; startup recovery
is not full rendered product QA.

### Capability and tool-family audit

Use the installed CLI discovery, server-advertised session settings, and actual
production adapter catalogs, not a provider-name checklist. Capability source:
`agent-adapter/src/lib.rs` (`BackendCapability`) and each
`server/src/backend/*` implementation's `capabilities()`. Tool source:
`protocol/src/types.rs` (`NativeToolCatalog`, `NativeToolSelection`,
`NativeToolKind`, `ToolRequestType`), Claude's init-frame catalog and
`claude_native_tool_kind`, Codex's model-supported tools/catalog projection and
native tool mapping, and `server/src/backend/muse/tools.rs`. Catalogs may be
adapter-only observations, not exposed UI controls: do not invent a debug API.
Use source evidence plus recorded runtime discovery; missing runtime evidence
is a gap, not an empty toolset.

Record every available tool under these families and map it to a case. Reuse
one representative operation per family unless a distinct native result shape
needs its own check. Any residual/unknown catalog entry needs an explicit case,
safe deferral (`NOT TESTED`), or evidenced unsupported result; do not silently
claim it was tested by another tool.

| Production family | Representative paths (only when exposed) | Cases |
| --- | --- | --- |
| Read/search/intelligence | Read/Glob/Grep, read_file/search, `ReadFiles`, `SearchTypes`, `GetTypeDocs`; memory/skill/MCP resource reads | `QA-02`, `QA-03`; distinguish typed cards from legitimate `Other` |
| File mutation | Write/Edit/apply_patch, write_file/edit_file, notebook/memory edits, `ModifyFile` | `QA-02`, `QA-04`, `QA-13`; disposable targets only |
| Command execution | Bash/exec_command/bash, stdin/terminal continuation, monitor; `RunCommand` | `QA-02`, `QA-08`; foreground vs background identity |
| Controlled questions/delegation | AskUserQuestion/request_user_input (blocking or async); native Agent/Task/spawn_agent/subagent_spawn/workflow | `QA-05`, `QA-07`, `QA-08`; not Tyde MCP ownership |
| Session/task controls | TodoWrite/TaskCreate/TaskUpdate/update_plan/write_todos; native goals, work_list/work_stop, workflow progress, cron/reminder controls | `QA-07`; inspect actual capability/selection, delete any created schedules/goals |
| Web/media/opaque tools | WebSearch/WebFetch/web_search/web_fetch, view/generate image, MCP text/image/error/progress, `Other` | `QA-02`, `QA-03`, `QA-15`; no invented image or rich-content support |
| Tyde-managed MCP | Spawn/send/await/read/close, workbench entry, review and host configuration controls | `QA-08`, `QA-10`, `QA-12`; child credential/project scope and role guidance |

At this baseline Claude declares foreground/background children and workflow
progress; Codex declares background children, native wait progress, goals and
async questions but **not** `ForegroundSubagents` or `WorkflowProgress`. Muse
declares hierarchy, goals, structured questions and opaque progress, but not
those Claude-specific child/workflow capabilities. Do not force identical
scenarios across these declarations. Background cancellation also differs:
Claude/Codex declare `InterruptPreservesBackgroundTasks`; Muse declares
`InterruptCancelsOwnedBackgroundCommands`. Test each declared scope, including
isolation from another turn, rather than requiring universal kill-or-survive.

The conformance model overrides document Luna's blocking-question path versus
Astra's async path; verify the installed catalog before selecting an upgrade.
`real_async_answer_completion_overlap` already uses Luna, which alone does not
prove all async question variants are available there. Use the cheapest
**confirmed capable** model only for the missing subcase; the source's Astra
or Sol overrides are evidence of a reproduction choice, not a price ranking.
No model/credential/driver availability means `BLOCKED`, not backend `N/A`.

### Matrix (steps, oracles, capability gates and cleanup)

`C` = Codex Luna 6 low; `H` = actual Claude Haiku 5.5 low; `M` = cheapest
confirmed model in the other backend's discovery. A pair `C/H` means separate
results, not interchangeable evidence. `+cap` means the next-cheapest confirmed
capable model **only** when needed, with the reason and actual model recorded.
`C`/`H` name the original requested lanes; use current discovery and record the
actual model/version rather than silently substituting a different identity.
The lifecycle, usage and per-level `QA-16` oracles apply throughout. Keep all
failures; one failing subcase fails its row, while blocked/unattempted subcases
prevent a complete pass.

| ID / source risk | Bounded action and observable UI/protocol oracle | Prerequisite / cheapest model | Cleanup |
| --- | --- | --- | --- |
| **QA-00 Isolation/observation** (§1–3) | Attest stores before any spawn; blank instance, correct project/footer; capture actual commit/CLI/model/settings, driver capability output and evidence channels. Controller chat is not a test subject. | Ephemeral stores/workspace/browser origin; no paid turn | Stop/list, verify store deletion and process exit; include failure paths |
| **QA-01 Lifecycle/usage** (`21a938a4`, §4–5) | Bootstrap prompt then second bounded turn; stream/reasoning/tool phase never means idle. Compare card, chat, Active view, tray and request/context/task usage; no empty/duplicate rows or fabricated totals. | C/H; reasoning `+cap`, usage scopes as declared | Reuse for later rows, then close |
| **QA-02 Native tools/terminal** (§6) | Read marker, edit one file, search, two sequential command phases, split stdout/stderr, nonzero exit; hold foreground then native background execution. Foreground has no tray row; background launch is not exit 0; original card keeps exact command, final output/exit and identity after reopen. Include web/intelligence/opaque families actually cataloged. | C/H; actual tool families, BackgroundTasks and yield/move-to-background capabilities separately | Release gates, cancel remaining tasks, inspect actual file/process state |
| **QA-03 MCP success/error/progress** (`real_mcp_*`) | An ephemeral local MCP server returns marked text, an `isError` failure, and held work/progress when supported; image/text and failed-image subcases gated separately. Error must stay failed with explanation; held call remains active, progress belongs to the same card, one terminal result and replay. Unavailable server is explicit, not an empty success. | StartupMcpServers; McpErrorResults / McpImageResults / McpOriginalContentFidelity / McpErrorImageResults / OpaqueToolProgress independently; C/H/M `+cap` | Remove MCP config inside instance; stop server, close listener; no operator-host registration |
| **QA-04 Permissions/access** (§7, `real_enforced_*`, `real_native_tool_policies`) | Approve one harmless write once, deny another; read-only attempt leaves file unchanged. Test exposed exact allow/deny and AskUser/delegation exclusions, including resumed/forked session. Unsupported enforcement rejects before work; no silent unrestricted launch or false “permission denied” on cancellation. | C/H/M; approval mode and native policy/enforced-access gates; advisory ReadOnly is not EnforcedReadOnly | Compare file; restore only dev settings; close constrained sessions |
| **QA-05 Questions** (`a5434a4e`, `d0c4806c`, `82b0b2a6`, §7) | **a** options/custom text; **b** multi-question + multi-select and schema/note bounds if exposed; **c** double activation/duplicate answer; **d** pending/answered replay/reopen; **e** cancel/close/new-message withdrawal and stale answer; **f** async answer overlaps other completion. Exactly one delivered answer, no backend user echo, no completed waiting card or revived stale prompt; control stays usable. Inspect both question card and human-message count. | C/H for supported blocking forms; UserQuestionHistory, StructuredUserQuestionAnswers, AsyncUserQuestionRequests separately, `+cap` per actual model. Protocol duplicate/stale submission only via verified child endpoint; absent driver is BLOCKED | Answer or cancel all pending questions; close temporary variants |
| **QA-06 Settings / opt-in feedback** (`7635ac09`, §14, `real_session_settings/speed`) | Draft backend/profile changes schema immediately; model, effort, fast/speed and access changes retain chosen values during bootstrap, while busy, at next-turn boundary and after rejected update/reload. Old message metadata unchanged; Stop and settings remain reachable together on mobile. New host defaults do not mutate existing sessions (06a). For 06b reuse one human-input flow to inspect disabled vs explicitly enabled project-scoped Suggest/Apply/Undo; original input remains once, no analysis from tool activity/replay. | C/H for session settings; advertised SessionSpeed only. 06b uses the existing text utility on any of the eight supported backends; explicit cheapest model, normal auth/profile routing, extra cost recorded | Restore dev defaults/profile; disable learning, undo/remove only disposable learned rules and close analyzer sessions |
| **QA-07 Native children/tasks/workflows** (§8–9, `real_native_*`) | **a** task/plan replace/clear/cancel; **b** foreground/background native children as declared, two distinguishable children, child-owned tool/usage, async completion/wake turn once; **c** workflow/goal controls as exposed. Capture registration-to-first-stream gap, task steps and terminal replay. Native wait excludes already completed children; no raw control payload, duplicated child answer or ownership inferred from active tab. | C/H/M; TaskUpdates/Replacement/Clear, child mode/hierarchy/wait, WorkflowProgress, NativeGoals independently; `+cap` | Cancel native work through supported controls; clear tasks/goals/schedules, verify children exited |
| **QA-08 Tyde child ownership/control** (`ceb93ea5`, §10–11, `real_nested_subagent_ownership`) | Spawn two Tyde children in dev project, await all, release one gate, read ready output then remaining child. Steer/follow up according to backend; mix a native child where supported. Cancel/close parent while awaiting, repeat control safely; no false not-running error, no new child admitted during close, descendants/processes gone and no resurrection. Delegated input retains origin in replay. | AgentControlTools; C/H, native mixture `+cap`; only child-local injected MCP credentials, never controller controls | Close descendants and root; verify no tray/process or second-client bootstrap survivor |
| **QA-09 Queue/reconnect/resume/compaction** (§11–13, `d7fdfc1f`, `54fb9946`) | Held work then FIFO messages/cancel-and-send; disconnect/reconnect during active work; resume completed/cancelled/failed history with no implicit new turn, two context-dependent follow-ups and one tool. Fork boundary isolated. Supported compaction has one coherent operation/terminal outcome and usable next turn. Reopen completed native + awaited + detached children: Idle, empty tray, no prompt reconstructed as activity. Same-store host restart is a separate subcase. | C/H; ResumeSession/ForkSession/compaction availability; second client and same-store restart harness required for respective subcases | Settle queues, close sessions; never call fresh stop/start a persistence pass |
| **QA-10 Review modes/reuse/FIFO/startup-close** (`ecc1042c`, `307e1dbb`, `73a1d49f`, [review](18-review.md)) | **a** two default advisory calls reuse unchanged reviewer context; same request_id reattaches, new ID requests work, no old finding/output misattribution; list/read/edit/resolve bounded findings with reasons. **b** ordered inputs/follow-up registered before output; failed provider stays failed, completed round preserved on Stop. **c** close during observable startup admission removes durable reservation, cannot resurrect. **d** POSIX FIFO/nonregular-file review target rejects without blocking other project commands; stale anchors reject submission. **e** Preview Inline, Single reviewer and Review team/layouts; run only assigned bounded review paths with the mode-specific oracles below. | Read-only-capable reviewer; configure cheap C/H sources only inside instance, record effective defaults/plan. Native forks inherit the author's actual model. Exact held startup boundary requires fixture/driver support: otherwise c is BLOCKED, not inferred from idle close. FIFO d needs POSIX | Close requesters/reviewer contexts; remove FIFO and dev diff; no release commands |
| **QA-11 Muse secure helpers/unsupported** (`e4bc7124`, `321c3bff`) | Launch with/without selected project; naming, activity summary, supervisor/usage helper stays hidden and text-only with verified empty named tools, no observers/MCP/tool calls. Supported Muse host succeeds; unsafe/unknown toolsets fail explicitly. Reject EnforcedReadOnly, delegation-category exclusion, empty agent allow-list, unsupported exact selectors and MCP+NamedWithoutMcp combinations; no unrestricted substitute. | Muse discovered host/schema fingerprint (audited 1.4.6/7/10), M (source conformance Spark 1.3 low); helper fresh-session contract distinct from agent mode | Disable helpers in dev settings, close agents/helpers and any native sessions created |
| **QA-12 Host spawning/roles/workbench** (`cf1b5c29`, `0f1527ee`, `9aa4679f`) | Ordinary Agent owns children only; global fields absent/rejected; assigned Orchestrator/Root gets documented controls, no inherited role. Explicit global spawn is parentless/no inherited config, closes through authorized host UI (not child close). Agent-type/profile conflict/unknown IDs error. Workbench enter moves same conversation/project/review target only at turn boundary; parent-grouped sidebar heading is not the working project. 12b switches Vibe/Dinosaur and moves sidebar tabs without restarting sessions; project editor preserves root restrictions. Controller's project unchanged; no role-change steering message. | Isolated host role setup by authorized coordinator; C/H/M, actual injected tool catalogs; roles are guidance, not claimed OS security | Close global agents explicitly; remove only dev workbench/project/config after leaving it |
| **QA-13 Diff/tab freshness** (`ac37bd22`, [project stream](07-project-stream.md)) | Expand omitted context, edit same file behind open diff, then navigate/reopen tabs/project/review. New server snapshot replaces stale lines/stage controls without refresh click, no wrong-file staging, scroll anchor sane; stale review anchor visible. Stage/unstage only disposable hunk; file tab shows current content, not another tab's snapshot. | Disposable git repo; no model spend except optional reuse of QA-02 edit | Restore dev repo and close tabs; never stage implementation workbench |
| **QA-14a Pinned chats** (`32225019`) | Pin/unpin live and historical chat; one global Pinned section before hosts in Vibe and Dinosaur, configured host badge, correct opening host/project, no duplicate in normal tree. Pins bypass automatic inactive/subagent/other-project hiding but not text search. Independently pinned child floats once and returns on unpin; replay/reopen retains target. Same IDs on another host do not alias; a remote pin may legitimately appear in this global section. | C/H reused history; second ephemeral host/client for cross-host checks, otherwise that subcase BLOCKED | Unpin/close only dev chats, stop second instance |
| **QA-14b Pinned Tychat lifecycle** (user-reported; separate fix owner) | Pin the actual host-owned integration agent, record typed owner/state/pin target; cross intended same-store restart/approved isolated upgrade and compare closure/restoration inventory vs filter-only hiding. Preserve singleton/session ownership and pin binding; no transcript-based status or silent recreation. Apply QA-16 to any exercised conversation path. | Disposable Tychat pairing/config, C/H as configured, matching-version driver and verified same-store restart/upgrade harness. No upgrade/release action is authorized by this guide; missing harness is BLOCKED | Preserve failure evidence before mutation; coordinator owns disposable pairing/instance cleanup; never reset/unpair operator integration |
| **QA-15 Platform/media** (`c23308bd`, `7635ac09`, `cec4fd8a`, `07d14961`) | **a** real mobile keyboard open/type/send, app-background/resume, settings while streaming; **b** mobile repeated taps do not trigger accidental page double-tap zoom (`c8c35f0a`), small editing controls don't cause Safari focus zoom, intentional browser/pinch zoom remains allowed; **c** Linux native pinch cannot magnify desktop while scroll works; **d** native picker/paste/drop image and media replay/fork on capable backend; reject unsupported input before spend. Compare painted screenshots with DOM/viewport, don't substitute emulated phone or DOM DataTransfer. | Named OS/device/browser + deployed bundle/host commit; C/H `+cap` for image input; generated output needs GenericGenerateImage. Only landed features count; parked usage/image benches are excluded | Clear dev attachments/browser pairing/profile, stop instances and native input helpers |
| **QA-16 All three tool-output levels** (cross-cutting; below) | For every tested session/backend path switch **Summary → Compact → Full** while active/waiting, after completion, after replay and after reopen. Separate per-level observations of native tool/child, MCP, question, workflow and media actually exercised; no blanket pass. | Same paid turns/output as owning row; real rendered level controls on each frontend, no extra model sweep | Restore dev client's preference; never change operator browser storage |

For QA-05, start with one valid native option question from the actual installed
tool schema; Codex file IO may use its command/apply-patch path, not named
Read/Write tools. Do not assume free-text-only, multi-select or arbitrary
multi-question shapes are supported by every model. A model that never invokes
a valid question is not proof of a renderer bug. A typed pending-question event
without its rendered form is FAIL. If work ends Idle/tool-Done without a pending
question, stop that observation wait and record the invocation/shape gap; do not
wait for an impossible form, retry to green or claim its answer path passed.

### QA-10e: current rendered review modes, not old Lite/Heavy labels

Use the normal diff/Comments surface and its **Review** dialog, not a retired
standalone review workbench. In the disposable host, select **Authoring agent**
when Inline/forks need it. Settings → Review and the dialog edit the same host
configuration. Current mode labels are **Inline**, **Single reviewer**,
**Review team**; team layouts are **Per aspect**, **Per reviewer**,
**Every combination** (`protocol::ReviewMode` / `ReviewTeamLayout`). Old
Lite/Heavy and light/deep are migration inputs, not current UI choices.

1. Inspect the rendered **Team preview**, aspect/source/model assignments,
   agent counts and unsupported reason before Start review. The no-spend
   `tyde_preview_review` exposes the same typed plan when the installed child
   host advertises it; source changes do not upgrade a running host's tools.
   Use a tiny disposable diff and existing cheap author session. Preview all
   layouts; do not launch every source×aspect combination just for coverage.
2. **Inline:** all selected aspect guidance goes to the selected author's own
   context; no reviewers, reviewer execution or panel findings should appear.
   An empty findings list is not reviewer completion certification. Any actual
   author response is a normal session path requiring QA-16 observations.
3. **Single reviewer:** one real native author-context fork inherits backend/
   model/settings, with no separate model picker. Require ForkSession plus
   enforced read-only review support; unsupported fork/read-only is an explicit
   unavailable plan/error, never a fresh-provider substitute. Check distinct
   ownership and no private reviewer content leaking into author/siblings.
4. **Review team:** sources are Fork current agent, an explicit backend/model,
   or a preserved host-default selection. Per aspect needs every enabled
   aspect mapped to its source; Per reviewer gives each source all aspects;
   Every combination schedules each source/aspect pair. Compare exact preview
   counts and actual assignments for the one preassigned execution. Forked
   sources inherit the author's cheap model; explicit sources use their own
   advertised cheap settings. Reuse/failure/Stop oracles in QA-10a–d still apply.
5. Edit an aspect's guidance without changing its stable identity; inspect the
   shared configuration and subsequent preview. A stale expected plan must
   reject launch rather than silently select a new team. A verified driver is
   needed to hold that exact race; otherwise mark just that subcase BLOCKED.
   Findings retain execution attribution across reuse; completion is advisory,
   not approval. Restore only disposable settings/aspects and close reviewers.

### QA-12b / QA-14a: desktop modes and workbench ownership

Source: [sidebar contract](28-sidebar-grouping.md), `header.rs`, `dock_zone.rs`,
`project_rail.rs`, and mounted `workbench.rs` / `agents_panel.rs` flows. Reuse
existing sessions; these presentation checks need no new provider prompt.

- Operate the top **Interface mode** select: **Vibe** has Agents/History;
  **Dinosaur** includes Files/Git/Search/Refs and advanced tools. Center chat
  tabs, composer, transcript, status and subscriptions stay unchanged. Each
  mode remembers its own sidebar order/side/visibility/width on that client.
- Move a sidebar tab via its menu and supported keyboard controls
  (Ctrl+Shift+Left/Right for sides, Ctrl+Shift+Up/Down for order); drag only with
  an actual input driver. Empty sides retain a drop target; Escape/abandoned
  drag changes nothing. One panel mount remains: no duplicate cards, lost
  pending question, restarted tool or hidden Stop. Cross QA-16 on the same held
  work rather than starting another session per layout. A missing reload/input
  driver blocks that subcase, not permission to fake gestures or persistence.
- Global **Pinned** precedes host groups in both modes. Inspect host badges,
  text search versus automatic-hide bypass, child counts/active versus inactive
  disclosure and unpin return without duplication. A missing configured-host
  label must show an actionable host error, not an opaque-ID substitute.
- Create/enter only a disposable workbench: entry changes the working project,
  roots, review target and restored tabs while agent headings deliberately stay
  grouped under the parent project. Open that grouped agent and verify its
  actual workbench tabs/project, not a heading-inferred ownership. Reopen a
  sibling and confirm its own tabs. `create_only` must not move the controller.
- Open **Edit project** from the heading pencil or project-rail menu; rename and
  root changes follow server events. Last-root and workbench/parent restrictions
  remain enforced. Removing a standalone project root keeps its directory and
  files. Use only fixture roots; never edit the implementation workbench.
- If persistence itself is in scope, inspect both modes after remount/reopen;
  reload remains driver-gated. Invalid/unknown layout state must show an error,
  not a guessed reset. **Settings → Appearance → Reset sidebar layout** resets
  only the current mode. Storage-fixture/migration subcases require a verified
  disposable profile; do not inject state into the operator browser.

### QA-06b: landed opt-in feedback integration

**Settings → Self-improvement** is separate from ordinary Feedback reporting.
With **Learn from feedback** off, require the **Off** status plus available
scope/usage/event evidence at the observed input/settled boundary; UI text alone
cannot certify absence of provider requests. No assistant/tool/replayed input
should schedule analysis. Missing observation channels are BLOCKED.

Only when this subcase is assigned, explicitly pick the **Project** scope (not
**All projects (host-wide)**), select an analyzer backend and its
cheapest model. All eight supported backends use the normal text utility;
read-only is no-action guidance, not enforced containment. Reuse one accepted
human message while work is held and verify the author receives it exactly
once without interruption. In **Suggest**, proposal editing must not apply it;
**Apply** changes the selected scope, **Undo** is revision-checked. A later
human edit must conflict rather than be overwritten. Exercise **Automatic**
only when explicitly assigned, with a disposable rule; human-owned edits still
need approval. Inspect **Analyzer usage** and failure/unsupported state, not a
success inferred from a new rule. Disable afterward and remove/undo only test
rules; preserve the original author input and isolate host/project scopes.
Any exercised author/analyzer conversation path still needs QA-16; an opaque
helper unavailable to the UI is an evidence limitation, not a fabricated pass.

### QA-14b: distinguish disappearance from lifecycle failure

Reuse `server/tests/tychat.rs::singleton_settings_resume_reset_and_secret_boundary`
and `private_journal_creation_replacement_and_unpair_survive_restart` as existing
protocol oracles, not evidence that this user-reported regression is fixed.
Before the intended restart, capture the visible pin/group/filter state and
protocol-owned `AgentOrigin::Tychat`, `TychatStatePayload` pairing status and
agent reference, session/pin target aliases and its integration status. Do not
export pairing secrets, fingerprint values or raw session IDs. The host singleton
is not a manually spawned chat that happens to have Tychat text.

After the restart boundary, compare the same sources with the rendered sidebar,
Agents and History views. Report these observations separately:

1. **Closed/archived:** record an observed `AgentClosed` event and its
   event/action ordering; record any separately supplied authoritative archive
   evidence without conflating it with closure. `AgentClosedPayload` contains
   only the agent ID, not a reason, and current `SessionSummary` has no archive
   status/reason field. History membership alone cannot certify archival.
   Without an explicit acknowledged action, don't call it a deliberate user
   close. Missing archive evidence stays unknown/BLOCKED, never reconstructed
   from a transcript or invented archive-reason field.
2. **Restoration/startup failed:** the singleton's oracle is
   `TychatStatePayload.status = TychatBridgeStatus::Failed { reason }`, its
   agent reference and the rendered integration warning/readiness state.
   `start_tychat` routes `reconcile_tychat` errors through `TychatService::fail`;
   generic `restore_open_agents` explicitly excludes `AgentOrigin::Tychat`.
   Do not require an `agent_restoration_failures` entry or invent a retry
   disposition for this path. Generic bootstrap/restoration failure reasons
   and retry dispositions apply only to agents in that generic restore pass.
   An observed typed Tychat failure is FAIL, not unknown/BLOCKED or a filter
   problem; never manually resume the host-owned archive to hide it.
3. **Visibility only:** canonical live integration/agent and pin snapshots
   still name the owner, but text search, a collapsed pin tree, a disconnected
   host or a different filtered surface hides it. Clear search/expand through
   the UI; require the same target to reappear once, with truthful status.
   Global explicit pins bypass automatic inactive/subagent/other-project hiding;
   hiding a live pin through those toggles is FAIL, not expected filtering.
   A visibility issue is not proof that the agent terminated.

Where no classification is observable, report unknown/BLOCKED with the missing
channel, never infer a lifecycle from history. A restart intended to preserve
integration must retain the host-owned singleton, pairing/session ownership and
correct persisted pin binding, without duplicate agents. Unexpected closure,
restoration failure, ownership loss or broken visibility is FAIL with its
separate evidence. Do not repair the observation with Pair, ResetAgent, manual
Resume or a replacement chat. No upgrade command or production store mutation
is authorized by this case. Upgrade-only reproduction remains BLOCKED until the
coordinator supplies an approved isolated harness; that source fix belongs to
the separate Implementer.

### QA-16: exact controls and per-level evidence

The current labels are **Summary**, **Compact**, **Full**. Source:
`frontend/src/components/settings_panel.rs` (Tool output segmented buttons),
`frontend/src/components/chat_view.rs::ToolOutputModeToggle` (chat-header cycle,
with titles `Tool output: summary/compact/full (click to switch to ...)`), and
`mobile-frontend/src/components/settings_view.rs` (Tool output select).
This is a **client-global, locally persisted presentation preference**, not a
backend session setting. Use a disposable browser origin/profile; a second
client has its own preference. Switching it must not send a backend turn or
change server-owned work/status. Do not set internal signals/storage as a
substitute for operating the rendered controls.

For **each tested session/backend path**, cycle all levels with work held,
then repeat on terminal output and on replay/reopen. Include a success, an
error, question/permission actions, native child navigation, MCP progress and
images, and workflow actions wherever applicable to that path. Reuse existing
case outputs; do not manufacture unsupported tools. Capture each level with
cards collapsed and expanded where supported. Summary/Compact intentionally
collapse successful output; Compact's expanded preview is shorter and Full
shows all output. Required result/error/image content must remain available
through the supported disclosure or Full, never discarded by switching.
Specialized cards may render identically; record the observed invariance, not
an assumed reduction in detail. Failures must retain visible error treatment
and explanation; permissions/questions must remain answerable exactly once,
child Open actions must target the same owner, and running work must remain
truthfully running at **all** levels. Preserve images, status, exit result,
usage, actions, scrollability and correct ownership without duplicates or
restarting work. Reopen/reload also verifies persistence of the chosen level.
Missing controls/platform evidence are `BLOCKED`, not a reason to skip a level.

Use one record per level, phase and owning case/subcase (including the native
child's own conversation), not one record for the whole backend:

```text
case/subcase | backend/actual model | session alias | frontend/platform
phase: held / terminal / replay / reopen | level: Summary / Compact / Full
collapsed/expanded | status/actions | result/error/images | owner/count
PASS / FAIL / BLOCKED / NOT TESTED / justified N/A | evidence links | cleanup
```

### Event barriers (replace sleep-based QA)

Subscribe/capture before triggering work. Use actual DOM changes (a
MutationObserver predicate or driver locator/state wait), backend tool-ready
output, FIFO/stdin/socket release, explicit process completion, or typed
protocol completion. `tyde_debug_evaluate` awaits a Promise, so a DOM observer
can resolve when the named visible condition holds; disconnect it on success
or cancellation. Process-output cursors are useful diagnostics but are not a
protocol drain or an idle authority. Only `TypingStatusChanged(bool)` owns
working/idle; `StreamEnd`, a tool result, and a final-looking answer do not.

A concrete POSIX foreground/background hold uses **two** case-specific FIFOs
in the disposable workspace. The controller first creates `ready.fifo` and
`release.fifo` with `mkfifo`, then starts this foreground readiness wait in a
managed command session **before** triggering the product command:

```python
from pathlib import Path
with Path("ready.fifo").open("rb", buffering=0) as ready:
    if ready.read() != b"QA_READY_unique_case_marker":
        raise RuntimeError("QA readiness channel closed without the ready token")
print("QA execution reached the hold")
```

Keep that session attached and await its command-completion event (with
`write_stdin` for a running `exec_command` session); no detached reader, polling
loop or timeout-based delay. Ask the backend to run this through its **real
foreground command tool**, using the same absolute disposable paths:

```python
from pathlib import Path
with Path("ready.fifo").open("wb", buffering=0) as ready:
    ready.write(b"QA_READY_unique_case_marker")
with Path("release.fifo").open("rb", buffering=0) as gate:
    if gate.read(1) != b"R":
        raise RuntimeError("QA release channel closed without the release token")
print("QA_DONE_unique_case_marker", flush=True)
```

Readiness must use that independent channel: Claude foreground stdout reaches
`consume_user_tool_result` and the rendered RunCommand result only **after**
the command completes. Waiting for QA_READY in tool stdout before releasing
would deadlock the fixture; text in the displayed command source is not proof
of execution. Auto-backgrounding it to obtain output would test a different
mode and must not satisfy the foreground/no-tray oracle.

Once the controller's ready reader completes **and** the rendered card is
running, inspect all required surfaces and levels. Readiness is only a control
barrier, never a substitute for rendered running-state evidence. The controller
runs `printf R > release.fifo` only after those observations; then await process
completion and the authoritative terminal event. Every reference to a command
ready marker in this guide means this channel, not assumed streaming stdout.
Use separate FIFO pairs for each child/phase; release one child to inspect an
intermediate await result before releasing the other.
Use backend-native background launch/terminal yield, never shell-detached
jobs. On Windows use a verified equivalent blocking stdin/socket event; no
POSIX means the FIFO-specific subcase is N/A, not all cancellation coverage.

For cancel/close, **do not release** the gate: require acknowledged terminal
state and underlying process exit while natural completion is still
impossible. This preserves the control assertion without a host-speed latency
threshold. Never write to a FIFO after its reader was killed. Unlink both FIFOs
during cleanup, including after failures; cancel/reap any still-waiting controller
reader if the backend fails before readiness. A per-test hung watchdog may
diagnose a missing event; it is not a performance pass/fail or a synchronization delay.
For late-event checks, drain an observable ordered boundary then reopen/reload
and send the next bounded turn. If the driver cannot observe the required
boundary, report that subcase BLOCKED; don't replace it with a quiet interval.

A late subscriber to completed work may receive typed Idle in bootstrap
without historical `TypingStatusChanged(false)`. Use that authoritative snapshot
for initial state, then observe new live transitions; do not wait for replay of
an event the protocol does not promise. In `server/tests/agents.rs`,
`help_controls_top_level_agents_across_projects` (`b6afc70e`) retains authenticated
sender/origin and deliberately late-bootstrap oracles. Its attachment gate is
a test-harness correction, not a new product behavior or a current UI pass.

## 1. Prepare and record the run

1. Confirm the backend CLI is installed and starts successfully outside Tyde.
2. Configure provider credentials and the default cheap QA model in the same
   environment from which the Tyde host will launch.
3. Confirm the backend is enabled in **Settings → Backends** and Tyde reports
   the expected installed version.
4. Use a disposable workspace and a disposable Tyde dev instance. Do not point
   destructive tool tests at a real project or run certification against the
   operator's everyday Tyde host. The launcher's `project_dir` is the build
   source and initially seeded project, **not** a safe destructive-test target.
   After store attestation, create the fixture below under that instance's
   storeDir, then add/select that root through the **child instance's** project
   UI before spawning test agents. Keep the implementation workbench untouched.
   The controller may run these filesystem-only setup commands; this does not
   certify product behavior:

   ```sh
   : "${storeDir:?Set storeDir from the attested start result}"
   qa_root="$storeDir/qa-workspace"
   mkdir "$qa_root"
   git -C "$qa_root" init
   printf 'Disposable QA fixture\n' > "$qa_root/marker.txt"
   git -C "$qa_root" add marker.txt
   git -C "$qa_root" -c user.name='Tyde QA' -c user.email='qa@invalid' commit -m 'Seed disposable QA fixture'
   ```
5. Record:
   - Tyde commit and whether the tree is clean;
   - backend and CLI version;
   - provider, default cheap QA model, and any capability-specific models;
   - access mode, reasoning level, and backend-native settings;
   - host OS and architecture;
   - frontend URL, viewport, browser version and disposable profile/origin;
   - on mobile, deployed bundle identity as well as host commit (a workbench
     host does not prove the deployed client contains its UI changes);
   - run start time and tester;
   - assigned stable QA IDs/subcases and per-level QA-16 evidence slots.
6. Build a capability-and-model matrix before testing. Include text, reasoning,
   file read, file write, foreground command, background command,
   cancellation, permission approval, user question, task list, image
   input/output, backend-native sub-agents, Tyde-managed agents, resume, fork,
   and compaction. Assign the cheapest capable model to every required row.
   Mark a row `unsupported` only when no model available through that backend
   exposes it, and record the evidence.
7. Prepare unique marker strings for every prompt, command, file, and child.
   Reusing a marker can hide duplicated or misrouted events.
8. Prove that the QA environment uses disposable project, session, review, and
   frontend stores, not merely a disposable filesystem workspace.
   `tyde_dev_instance_start` redirects every mutable store, including
   `TYDE_SESSION_STORE_PATH`, into an instance-specific temporary directory;
   `tyde_dev_instance_stop` removes that directory. Require
   `storesEphemeral: true` in the start result, record `storeDir` and
   `sessionStorePath`, and confirm they contain no production records. Abort
   before creating an agent if this attestation is missing or either path
   points at the operator's normal stores. Running on the operator's normal
   host, polluting its session history, or silently reusing any normal store is
   an immediate setup `FAIL` and cannot certify a backend.
   For Hermes, Tyde-store isolation alone is insufficient. Pass the typed
   `hermes` option with a loopback stub and additionally require
   `hermesEnvironment.homeEphemeral: true`,
   `hermesEnvironment.hermesHomeEphemeral: true`,
   `hermesEnvironment.networkPolicy: "loopback_stub_only"`, and both resolved
   home paths beneath the canonical `storeDir`. Require
   `hermesEnvironment.home == hermesEnvironment.resolvedHome`,
   `hermesEnvironment.hermesHome ==
   hermesEnvironment.resolvedHermesHome`, an exported `hermesExecutable` or
   `hermesPython` invocation path that exists outside the disposable home, and
   its corresponding canonical `resolvedHermesExecutable` or
   `resolvedHermesPython` target. The two Python values may differ when a
   virtual-environment symlink must be retained. Record `hermesLauncherChain`,
   `hermesPythonLauncherChain`,
   `skippedLaunchers`, `skippedPythonLaunchers`, and
   `launcherEnvironmentPreserved`. Without that attestation, Hermes QA is
   `BLOCKED`; do not open Hermes settings or start a Hermes agent. If
   `launcherEnvironmentPreserved` is `false`, the contained run remains valid
   for Tyde/Hermes protocol QA but cannot certify production-wrapper behavior
   affected by toolset, tool-progress, `PYTHONPATH`, or network-guard
   environment.
9. Confirm the available driver can capture screenshots, rendered DOM,
   console/backend events, and a separately controlled second client. Treat a
   missing required observation channel as `BLOCKED`, not as permission to
   infer the result from final DOM or protocol state.

## 2. Start a clean Tyde dev instance

1. Call `tyde_dev_instance_start` with the repository root as `project_dir`.
   Save its starting `instanceId`, then call `tyde_dev_instance_wait` with that
   same `instance_id`. Require `status.kind: "ready"` before using endpoints or
   store attestations. Reattach to this same handle if the MCP wait response is
   lost; never start a duplicate.
   For no-paid Hermes QA, also pass a disposable profile and a stub that is
   already listening on numeric loopback:

   ```json
   {
     "project_dir": "/path/to/Tyde2",
     "hermes": {
       "profiles": ["qa"],
       "loopbackStub": {
         "baseUrl": "http://127.0.0.1:43123/v1",
         "model": "tyde-stub"
       }
     }
   }
   ```
   The `hermes` object without `loopbackStub` is rejected. Only numeric
   `127.0.0.1` is accepted; do not substitute `localhost`, IPv6, or a
   non-loopback HTTP URL.
2. Before any backend call, require `storesEphemeral: true`, confirm the
   returned `sessionStorePath` is beneath the returned `storeDir`, and record
   both paths. This is a hard preflight gate, not optional evidence collected
   after the run.
3. Keep the returned `instance_id`; every later debug call must use it.
4. Create and control test agents only through the returned child
   `frontend_url` (or verified controls bound to that child's endpoints).
   Existing operator chats may coordinate testers and call the dev launcher;
   this controller activity is not product certification and need not be moved.
   Never use those chats' injected agent/config tools to create test subjects
   on the operator host. All exercised agents, children, projects, settings,
   MCP registrations, reviews and stores must belong to the ephemeral instance.
   Record controller and test-subject identities separately.
5. Open the returned `frontend_url` and wait for the home screen to finish
   loading.
6. Check the initial console and rendered UI for startup errors.
7. Confirm no prior agents, chats, tool cards, or in-flight rows from another
   run are present in the disposable store.
8. Confirm the project rail, selected-project label, footer, and error banner
   all refer to the disposable project. A stale deleted workbench, missing
   project root, or global error inherited from another store fails setup.
9. If code changes during the test, stop the instance and start a new one.
   Dev instances intentionally do not hot-reload.

### Loopback-stub request contract

This Hermes-only containment path uses scripted provider responses. It can
exercise Tyde above the backend boundary but **cannot certify a real provider**
or satisfy real-backend conformance. Label it contained protocol/manual QA,
not a passing paid-backend lane.

The stub must provide OpenAI-compatible `GET /v1/models` and
`POST /v1/chat/completions` responses for `tyde-stub`, with JSON for
`stream: false` and valid SSE ending in `[DONE]` for `stream: true`. The first
chat completion is not the first visible QA turn: Tyde requests a short
automatic agent name before registering the agent. Return a non-empty short
assistant name and a valid usage object for that request, then advance to the
scripted turn responses. A stub that consumes the first scripted response as
the name will manufacture an agent-creation failure.

Keep supervisor disabled for baseline cases. Enable it only when the stub has
separate deterministic responses for each helper completion and the case is
explicitly testing supervisor behavior. Record every request sequence; an
unexpected provider or non-loopback request is a containment failure.

Use `tyde_debug_evaluate` for DOM inspection and a verified browser/native
input driver for clicks, typing, scrolling, console capture and screenshots.
A browser page does not certify native Tauri input merely by sharing its URL. Assertions must be based on the
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

### Lifecycle control is unconditional and independent of natural completion

Cancel and close are the user's authority over an agent, so they must work no
matter what the agent is doing. Two rules apply everywhere below:

- **Independent.** Cancel and close must reach acknowledged terminal state
  while a tool/await is still held and before its natural completion is
  possible. Require the underlying process to exit, not just a hidden row.
  A control that only takes effect after the tester releases work has failed.
  Do not judge responsiveness by host-load-dependent elapsed-time thresholds.
- **Truthful.** Neither control may describe a running agent as absent. Any
  `agent not running`, `agent not found`, or equivalent message about an agent
  still shown as active is a failure on its own, whatever happens afterwards —
  it sends the user hunting for a crash that did not occur, and it usually
  means a control is refusing precisely when it is needed.

An agent that cannot be cancelled or closed is unrecoverable through the UI,
so treat either failure as blocking rather than cosmetic.

The exact label may differ by surface, but meaning must not conflict. A check
icon, completed styling, finished-only placement, enabled Compact action, or
absence from an explicitly selected, unfiltered active-status view all count
as a completed claim. None may appear while authoritative backend work is
still running. Search, collapsed children, mode visibility and deliberate
filters may hide rows without claiming completion; inspect typed status and
reveal the row rather than inferring lifecycle from that visibility alone.
There is no current Hide finished control; closed agents are removed.

For every transition:

1. Capture the status immediately before starting the action.
2. Start the action and hold it on an explicit release barrier.
3. While held, capture the agent card, chat controls, relevant live card, and
   Active view membership. Confirm all surfaces agree that work is active or
   explicitly waiting.
4. If the turn has multiple assistant/tool phases, capture the gap after each
   `StreamEnd`/tool request and while the tool is running. A phase boundary is
   not a turn boundary.
5. Allow the authoritative completion event to arrive. Capture the terminal
   state and confirm it changes exactly once.
6. Cross an observable event/drain boundary, reopen, and send the next bounded
   turn. Confirm no late event revives prior work, changes its success to
   failure, duplicates completion, or alters finalized output.
7. Repeat the active-to-completed transition on a subsequent turn. Startup-only
   correctness does not certify reused-session behavior.
8. For every root and child spawn, sample once immediately after Tyde registers
   the agent but before its first backend stream event. Repeat with foreground
   and background children. The card must remain Initializing or Thinking; an
   idle/completed default during this registration-to-stream gap is a failure,
   even when a later event corrects it.

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

### Current context bar

1. Confirm **Current context** appears for the active conversation after usage
   metadata arrives.
2. If the backend reports category detail, confirm the coloured fill matches it.
   Otherwise confirm the UI says the breakdown is unavailable instead of
   inventing categories.
3. When tasks exist, click the compact task progress link (for example,
   **Task 1 of 5 in progress →**) and confirm the task list replaces the context
   view. Click the thin context bar below the task list and confirm it switches
   back.
4. Verify context counts and percentages are populated, finite, non-negative,
   and within the reported context-window limit.
5. Confirm the used-context value is positive and the percentage approximately
   agrees with `used tokens / context-window tokens`.
6. After each later turn, confirm the view refreshes and still refers to the
   active conversation. It need not always increase because a backend may
   compact or report a different authoritative snapshot.
7. Switch between two chats and confirm neither chat displays the other's
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
8. After child completion, reconcile the child's row with its rendered message
   usage and any authoritative completion usage. Positive usage must not remain
   permanently `partial` or `unreported` when the backend supplied the missing
   values, and a known-unsplittable total must be explained rather than shown
   as an unexplained zero or tiny output count.

These surfaces answer different questions and need not show the same number:

- a message footer shows that request's usage;
- Current context shows the current context-window snapshot;
- Session Settings shows task-wide cumulative usage, including sub-agents.

They must nevertheless be internally consistent, correctly attributed, and
refreshed after completed work and navigation.

## 6. Exercise tools, files, and background work

Use unique markers and verify every operation while active, after completion,
and after replay.

1. **Foreground command:** ask the agent to run a harmless marked command
   blocked on the explicit release barrier above. After its ready marker,
   verify the tool card and agent are running before releasing it. Afterward,
   verify the same card retains command, output, exit status, and terminal
   state. The in-flight tray must stay empty for the whole run: a command the
   agent is waiting on is not background work, and a tray row for it is `FAIL`.
2. **Multi-phase tool turn:** require two sequential tool calls, each with its
   own ready/release handshake, followed by a one-line answer. At every
   assistant/tool boundary, confirm the agent remains Thinking. It must not flash completed
   after the assistant phase that declares a tool.
3. **File read:** create a disposable file with a unique marker, ask the agent
   to read it, and confirm the typed read card, path, and returned content are
   correct and not leaked into an unrelated chat.
4. **File write:** in a mode that allows writes, ask for one bounded edit.
   Verify permission behavior, typed modify card, before/after preview, actual
   file contents, and replay. Repeat in read-only mode and confirm no write
   occurs.
5. **Background command:** start a bounded process that outlives the call that
   launched it — on Codex, give the exec tool a short timeout so it returns
   while the process is still alive — then continue the turn and check or wait
   for it. Confirm the original card and in-flight row
   show the exact command that was executed, not a model-written description or
   completion summary. They must remain running until the process ends and
   transition once without acquiring a new identity. If the agent turn itself
   has authoritatively ended, the agent may be idle while the separate
   background row remains running; the two surfaces must clearly communicate
   that distinction.
   The launch acknowledgement must not be rendered as process `Done` or
   `exit 0`. After the in-flight row disappears, the original card must retain
   the real final stdout, stderr, exit status, and terminal state through
   replay and restart.
   Absence of the running row on any applicable backend is `FAIL`, even if the
   command completes successfully or another backend renders it correctly.
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
   Every surface must describe cancellation as cancellation, not permission
   denial, rejected tool input, command failure, or successful completion.
   Backend control prompts or synthetic rejection instructions must never be
   exposed as user-facing stdout, stderr, or assistant text.
6. Cancel a command held on an explicit release barrier after its ready
   marker and running card are visible. Do not release it. Require terminal
   cancellation and actual process exit before natural completion is possible,
   then a usable follow-up. A cancel that needs the gate released is a failure,
   not a slow success. Check declared cancellation scope separately for work
   already backgrounded; some backends preserve those independent tasks.
7. Repeat the previous step but **close** the agent instead of cancelling it,
   while the long command is still running. Confirm the agent disappears from
   the Agents tab promptly, no further frames arrive on its stream, and it does
   not reappear on reload or in a new client's bootstrap.
8. Force one safe backend error and one tool error. Confirm each is attributed
   to the correct agent and turn, preserves prior history, and leaves controls
   recoverable.
9. After every denial, cancellation, and failure, send a normal follow-up.
   Confirm the same session remains usable unless the backend explicitly made
   it non-resumable.

## 8. Exercise native task tracking

For backends with a native task or plan list, including Claude Code and Codex:

1. Request a bounded three-step task and explicitly require native tracking.
2. Confirm all descriptions and initial statuses render as typed task state.
3. Hold each step on a release barrier; verify the active task and agent
   are both active before release. Prior steps must become completed without duplication.
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

Test foreground and background native children separately where those modes
are declared. Record unsupported mode evidence instead of assuming every
Subagents backend supports both. Give every child a
unique name, prompt marker, command marker, and final-answer marker.

### Spawn and lifecycle

1. Spawn a named child whose first tool emits ready then blocks on a
   child-specific release barrier. The parent must immediately show a typed
   spawn card with child name and prompt.
2. Before child output starts, confirm its agent card is Initializing or
   Thinking, never completed. Capture the exact interval after the spawn
   acknowledgement and before the first child stream event; checking
   only after streaming begins misses the highest-risk lifecycle gap. A
   command gate starts later and cannot prove that earlier interval; missing
   capture/admission control means this subcase is BLOCKED.
3. Open the child. Confirm its initial prompt, backend, model, parent relation,
   and active state are visible.
4. Keep the child running across at least two assistant/tool phases. Inspect it
   during its first command, after the phase-ending `StreamEnd`, between tools,
   and during its final reasoning. It must remain Thinking throughout.
5. Confirm the child changes to completed only after the backend's
   authoritative child completion. The parent tool card and child agent card
   must agree.
6. Cross an ordered observable boundary and reopen the child; confirm no
   late event reopens its work or changes its result.
7. Spawn two same-type children with different requested names or backend
   descriptions. Confirm those labels propagate to cards, open-agent actions,
   progress rows, and replay so the children remain distinguishable.
8. Expand and collapse every native spawn, await, and progress card. Confirm
   neither state exposes raw thread/agent/task identifiers, temporary output
   paths, serialized backend control payloads, synthetic prompts, or text that
   instructs the model not to reveal internal metadata.

### Child-owned work

1. Make the child run a foreground command, start a bounded background command,
   use a native task list if supported, and finish with one unique line.
2. Confirm every tool, background transition, task update, reasoning segment,
   and final answer renders in the child's chat, not the parent's.
3. Confirm the foreground card includes command, output, exit status, and one
   terminal state.
4. Confirm the background card retains identity from running to terminal, and
   that its running row appears in the *child's* in-flight tray and not the
   parent's. The child's foreground command must leave both trays empty.
5. Confirm the final message appears once, is not empty, and shows the child's
   backend and model. The parent shows only typed spawn/await progress, not a
   duplicate of the child's answer.
6. Leave and reopen the child. Confirm its status, cards, tasks, final message,
   and ordering replay identically.
7. Compare the parent's final progress summary with the child's rendered tool
   history. Child tool counts and completed-work summaries must match for zero,
   one, and multiple tool calls.

### Background child outliving its parent turn

1. Spawn a native child in the background, hold it on a release barrier, and
   let the parent turn finish before releasing that child.
2. After the parent completes, confirm the child remains Thinking, remains in
   the Active view, and retains running styling. This is required even when no
   child assistant stream is currently open.
3. Navigate away and back while the child runs. Confirm it never flashes or
   settles as completed before its authoritative notification.
4. Open the child from every available **Open agent** action and confirm each
   targets the same child.
5. After the child completes, confirm parent progress, child status, final
   answer, and in-flight surfaces transition exactly once.
6. Confirm **Tyde** does not synthesize a parent response out of the
   background-completion notification. Any parent notification Tyde emits must
   remain typed status/progress with its own clear identity, and Tyde must not
   render the child's final answer a second time on the parent.
7. Confirm the CLI's **own** wake turn appears in the parent exactly once. When
   a background task completes, the CLI wakes the model and runs a real turn
   with no user message behind it; that turn is legitimate parent content, gets
   its own usage, and must not be dropped. Note that the model's wake text
   often quotes the child's answer — that is the CLI's content, not a Tyde
   rendering duplicate, and is not a defect.

### Native child usage

1. While the child runs, open the parent's **Task usage** popover. Confirm the
   child is a distinct row with the expected backend and model.
2. After completion, confirm positive child input/output totals come from its
   own turns rather than zeroes or a copy of the parent.
3. Confirm the task total includes the child exactly once and the header reports
   the correct agent count.

## 10. Exercise Tyde-managed agents and mixed ownership

1. Ask the root agent to spawn two named Tyde-managed agents held on separate
   release barriers, then await both. Release them independently.
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

### Cancelling and closing a parent that is awaiting children

An orchestrator blocked on an await holds its turn open indefinitely. That is
the state in which cancel and close have historically failed, so exercise it
explicitly rather than inferring it from the idle-agent cases above.

8. With the parent still blocked on `await` and at least two children running,
   press **Cancel** on the parent. Confirm the parent reaches a terminal
   cancelled state while the child gates remain held. Confirm Cancel does **not**
   report `agent not running`, `agent not found`, or any other message implying
   the agent is dead — the parent is running, and saying otherwise is a
   failure even if the turn later settles.
9. Repeat with a fresh parent and children, but **close** the parent from the
   Agents tab while it is awaiting. Confirm the parent and every descendant
   disappear from the Agents tab before releasing any child gate, and that
   the in-flight tray stops listing them.
10. After that close, confirm the tree does not resurrect: reload the frontend
    and connect a second client, and verify neither shows the closed parent or
    any child. A row that survives reload means the registry was never updated.
11. While the parent is closing, confirm it cannot spawn more children. Any
    spawn attempted after the close must be refused with a message naming the
    closing state, and the Agents tab must not gain a new child mid-teardown.
12. Press **Cancel** and **Close** several times in a row on the awaiting
    parent. Confirm repeated presses are harmless and still converge on a
    closed agent; a second press that does nothing at all, or that reports the
    agent as not running, is a failure.
13. Verify the children are genuinely gone, not merely hidden — their backend
    processes must exit, and no orphan may keep streaming into a closed parent.

## 11. Exercise ordering, queueing, and interruption races

1. Send two unique messages rapidly. Verify FIFO processing, no loss, no
   duplicate user rows, and correct active state across the handoff.
2. Submit a message at the observed completion boundary (subscribe before
   releasing the prior work). Confirm it is neither rejected nor attached to
   the prior turn. If the boundary cannot be observed/controlled, mark this
   subcase BLOCKED; do not repeat until an accidental race passes.
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
   Also confirm the selected project and conversation restore consistently:
   project rail, editor, chat, footer, and agent panel must not disagree or
   require clicking an agent merely to repair bootstrap state.
   This is a mandatory terminal-bootstrap regression case: complete one
   backend-native child, one awaited Tyde-managed child, and two detached
   Tyde-managed children, then record the empty in-flight tray and Idle parent
   and child cards. Reload the frontend, reopen the parent, and confirm the
   tray remains empty and every card remains Idle even though replayed history
   contains the user prompts and each child's final-answer marker. Any
   completed transcript reconstructed as Thinking or any final-answer row
   listed as running is `FAIL`.
4. From the rendered **History** panel, locate completed, cancelled, and failed
   sessions and exercise **Resume** on each supported state. Record the source
   session identity, backend, model, project, terminal state, and last unique
   marker before clicking Resume. Do not send a prompt as part of the resume
   action. Confirm exactly one resumed conversation opens, its prior history
   appears exactly once and in order, and the parent agent card settles at Idle
   without a turn starting merely because it was resumed. A persistent
   Initializing or Thinking card before any prompt is a failure. Confirm
   unrelated commands remain responsive while history loads.
5. Use every resumed session rather than treating successful history rendering
   as proof of a working resume. Send a unique context-dependent prompt that
   requires information from before the resume. From acceptance until the real
   terminal event, assert the parent agent card continuously shows an active
   state such as Initializing or Thinking; the chat composer showing Thinking
   does not count, and any Idle/completed card while the backend is still
   running is a failure. Confirm exactly one correct terminal response. Then send a second unique
   prompt in the same resumed conversation and exercise one supported tool.
   Confirm both turns use the original backend/provider session, retain the
   selected model and settings, append to the same History entry, and do not
   duplicate old messages, usage, tools, or session rows.
6. Leave and reopen the newly used resumed conversation from **History**.
   Confirm the pre-resume history and both post-resume turns replay once, the
   final state stays idle, and the conversation remains usable for another
   bounded turn. Repeat after a frontend reload. A same-store host restart
   needs the separately verified harness described in the driver limits;
   mark that subcase BLOCKED when unavailable.
7. Exercise **Fork** from a known point. Confirm prior history is present once,
   later source history is absent, and source/fork usage and statuses do not
   leak into one another.
8. Exercise manual or automatic compaction when supported. Confirm the agent
   shows Compacting, never completed during compaction, history remains
   coherent, and the next turn succeeds with plausible context usage.
9. With a verified same-store restart harness, restart the isolated host and
   repeat replay checks. In-memory correctness alone is not enough. The default
   dev stop deletes stores, so it cannot satisfy this assertion; record BLOCKED
   rather than passing a fresh store as persistence evidence.

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

1. Before the first turn, use the New Chat agent/profile selector to switch
   between at least two available backends. After every selection, confirm the
   **Session Settings (<backend>)** heading and complete settings schema change
   immediately to the selected backend. Send a uniquely marked turn and
   confirm its agent card, message metadata, model, and actual response backend
   all match the final selection. A selector that changes visually while the
   old backend or schema launches is a failure.
2. Switch the draft agent/profile repeatedly and confirm unrelated review
   backend, target, and cost controls do not change. After a session starts,
   confirm its backend identity remains immutable while documented
   per-session settings remain editable.
3. Change every exposed per-session setting before the first turn and verify
   the response metadata reflects the selected value.
   Capture the bootstrap interval as well: a chosen model, effort, access mode,
   or profile must not temporarily render as `Auto` or another default while
   the backend initializes and then silently correct itself later.
4. Change settings between turns where allowed. Confirm the new value applies
   only at the documented boundary and old messages retain their metadata.
5. Attempt a setting change while busy. Confirm it is queued, rejected, or
   applied according to the UI contract without silently changing the active
   turn.
6. Switch model where supported and verify message metadata and usage identify
   the actual model used for each turn.
7. Attempt a model or setting update that the backend rejects. Confirm the
   visible control reverts to the last applied value, the rejected value is
   not persisted, and the next turn uses the displayed value. Reload and
   reopen the session to verify the same authoritative setting remains.
8. When a capability-specific model is needed, switch from the default cheap
   model, exercise the capability and its lifecycle/replay regressions, then
   switch back. Confirm history keeps the actual model on every old message and
   new turns use the newly selected model only.
9. Attach a small image when supported. Confirm preview, send, backend receipt,
   final rendering, replay, and fork behavior. Unsupported media must be
   rejected before backend spend.
10. Request image output when supported. Confirm the image and associated typed
   event both survive replay and neither replaces the other.
11. Change the host default backend. Confirm new chats use the new default while
   existing chats retain their original backend, model, settings, and session.
12. Run bounded turns concurrently on two different backends using their cheap
   QA models. Confirm status, output, tools, usage, capacity, settings, and
   errors remain isolated and every backend label identifies the actual
   producer.
13. Enable agent activity summaries and hold a normal turn on a release gate
   until a summary attempt is observed (where exposed). Confirm the summary
   reaches a non-error state, the hidden helper never appears as an agent or tool user, and backend logs contain no
   authentication or tool-policy failure. Repeat after reloading the client.
14. Enable the supervisor and run one bounded case that produces `done`, one
   that requires `awaiting_user`, and one safe interrupted case that requires
   `continue`. Confirm the hidden verdict call authenticates on this backend,
   no retry-exhaustion warning appears for helper failure, and the visible
   agent status remains authoritative while the verdict runs.
15. After a completed supervised turn, reload and reconnect while the terminal
    warning or final assistant card is visible. Confirm the agent remains idle,
    the composer sends normally rather than queueing, and no stale stream makes
    any surface return to Thinking.

### Native Ubuntu image input

Exercise native input, not JavaScript-created `DataTransfer` fixtures: those
fixtures expose files that WebKitGTK can withhold from real input.

1. Start a disposable dev instance, select its project, and open an empty chat
   on an image-capable backend. Verify **More send actions → Attach image** is
   available without draft text. Use the native file chooser to select a PNG;
   verify its preview and that selecting the same file again works.
2. Copy image pixels from a native application or screenshot utility, focus
   the composer, and paste with Ctrl+V. Verify a decoded preview, not merely a
   paste event. Also paste ordinary text and verify native text insertion is
   unchanged.
3. Drag a real PNG from a native file manager into the composer. Verify the
   hover affordance, one preview per file, and a cleared hover state after
   leaving or dropping. Drop outside the composer and verify no attachment.
   Check internal text drags still work.
4. Send the collected images in one bounded turn on the cheapest capable
   model. Verify backend receipt, the rendered image count, and replay after
   reloading. Check explicit rejection for unsupported backends and invalid
   files; do not treat a disappearing attachment as successful delivery.
5. Stop the instance and verify its disposable stores were removed.

For isolated Ubuntu/X11 QA, a GTK drag source advertising `text/uri-list` and
`Gtk.Clipboard.set_image` exercise actual platform input. Drive them with
native mouse/keyboard events on the dev instance's display. Do not use DOM
file injection as the native reproduction. On Wayland, clipboard ingestion
must use the webview's active GDK display, not an independently selected X11
or data-control clipboard.

Reproduction evidence for the desktop attachment fix (Ubuntu 26.04.1,
WebKitGTK 2.52.6): two clean pre-fix instances received trusted image-paste
events with no types/files, and native file drops advertised URI-list/HTML
but exposed no files or readable URI. Neither created a preview. The fixed
instance decoded clipboard, drop, and native-picker images at 32×32 pixels;
plain-text paste remained intact. One Claude Haiku turn received all three
images and identified their count and color; all three decoded images and the
answer survived reload without visible errors. This is scoped image-input QA,
not certification of every backend scenario.

## 15. Presentation, accessibility, and responsiveness audit

1. Repeat representative baseline, tool, task, and child states at narrow and
   wide supported viewports. Confirm no status, control, output, or usage value
   is clipped or hidden behind scrolling without an affordance.
2. Verify long names, paths, commands, errors, and model identifiers wrap or
   truncate with accessible full text and never obscure status.
3. Navigate essential controls by keyboard. Confirm visible focus, meaningful
   labels, correct disabled states, and no accidental double activation.
4. Confirm status is communicated by text/icon as well as color and that live
   updates are understandable without relying on animation alone.
   Inspect the accessibility tree or element attributes: each status icon must
   expose a meaningful accessible name such as Initializing, Thinking,
   Cancelled, Failed, or Completed rather than an unlabeled glyph and CSS class.
5. During split output and concurrent children, exercise input and navigation;
   record frozen controls, runaway scrolling, duplicate renders and layout
   corruption with observable action/state evidence. Host CPU/load or elapsed
   performance measurements are diagnostics, never a pass/fail threshold.
6. Apply QA-16's Summary/Compact/Full checks to every exercised path, including
   specialized cards; retain separate per-level evidence.
7. Inspect the console after every major section. New warnings, parse failures,
   identity violations, panics, unhandled promises, or repeated reconnects fail
   the section even if the UI recovers.

## 16. Evidence, rerun, and cleanup

Keep an evidence index by stable QA ID/subcase, backend and QA-16 level/phase,
not only successes or failures. Link the numbered walkthrough steps used.
The index must contain no blank or omitted rows:

- `PASS`, `FAIL`, `BLOCKED`, `NOT TESTED`, or justified `N/A`;
- exact disposable QA prompt and unique non-secret markers (never private
  conversation/codeword content);
- before, held-active, transition, and terminal screenshots;
- relevant rendered DOM text from `tyde_debug_evaluate`;
- both collapsed and expanded card evidence when details can contain different
  content, including an accessibility-tree or attribute capture for statuses;
- locally correlated agent/child/tool/task/session aliases and instance identity;
  do not print raw session IDs, credentials or private prompt/codeword content;
- console/log excerpt or explicit note that none appeared;
- backend, provider, model, settings, commit, and `instance_id`;
- observed timestamps and reproduction attempts.

Report failures as they are found and repeat all of them in the final report.
Never suppress a failure because the final response succeeded, the defect seems
backend-specific, a different backend passed, or implementation code appears
to explain the behavior.

For a failure, preserve the first attempt and its held/terminal evidence.
Reproduce once in a clean instance only when it answers a diagnostic question;
record that reason, never retry to green or erase the original failure. This
audit does not fix product bugs or alter release state. File the case ID,
actual settings and evidence for follow-up. After a separately authorized fix,
run its regression and affected adjacent cases; expand only when the change's
reach warrants the extra spend. A targeted pass is scoped evidence, not full
backend recertification.

Finally, call `tyde_dev_instance_stop` with the saved `instance_id`. Confirm it
no longer appears in `tyde_dev_instance_list` and no backend child process from
the run remains alive. Confirm the returned `storeDir` no longer exists; a
surviving session store is cleanup `FAIL`.

## Pass criteria

The backend passes only when every required matrix entry has linked evidence
and every temporal state was observed while held. In particular:

- every applicable function and rendered surface is present; missing
  functionality is a failure, including backend-specific omissions such as a
  background task that runs but never appears in the in-flight tray;
- no root or child is ever rendered completed while authoritative work is
  still running;
- agent cards, Active view, chat controls, live cards, in-flight tray, and
  second clients never contradict one another;
- normal, reasoning, tool, file, background, permission, question,
  cancellation, failure, task, native-child, Tyde-managed-child, mixed,
  concurrency, replay, resume, fork, compaction, reconnect, settings, and media
  paths applicable to the backend complete without missing, duplicated,
  misordered, or misattributed events;
- every feature exposed by any available model on the backend was exercised on
  the cheapest capable model, with every model switch and its cost recorded;
- the complete workflow passed independently for every backend being
  certified, without borrowing feature evidence from another backend;
- every running state reaches exactly one truthful terminal state and no late
  event contradicts it;
- usage is populated, refreshed, attributed to the correct root or child, and
  not double-counted;
- persistence and clean restart reproduce the same history and terminal state;
- no unexpected warning, protocol error, identity violation, panic, secret
  leak, or orphan backend process occurs;
- no required row remains failed, blocked or untested; after separately
  authorized fixes, a full certification claim requires all applicable rows,
  not just the targeted retest;
- every tested session/backend path has separate Summary, Compact and Full
  results while active/waiting, terminal, replayed and reopened.

Missing evidence, an unobserved transient state, or an unexplained `N/A` means
the backend is not certified. Any `FAIL`, `BLOCKED`, or `NOT TESTED` row also
means the backend is not certified and must be reported explicitly.
