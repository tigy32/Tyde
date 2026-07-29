# Beta prelaunch QA and bug report

## Executive launch recommendation

**Recommendation: NO-GO for the next beta until the remaining P0 QA gaps are
closed.**

The range now contains 81 commits and 45,622 insertions, including a 20k-line
context-compaction feature, persisted Kiro-to-ACP migrations, provider
background-lifecycle changes, and mobile transport changes. Useful live
coverage passed for five backend kinds and several desktop UI surfaces, but the
release gate remains incomplete:

- the canonical real-backend command is red: **18 passed, 2 failed**;
- Hermes is **UNVERIFIED** because both tests stopped at an unavailable default
  OpenRouter route, not because a Hermes product defect was proven;
- the canonical rendered dev-instance workflow remains unavailable in this
  conversation because its still-running parent uses product protocol 40 while
  current-main children use product protocol 41; the debug MCP and UI-debug
  protocol did not change, and the protocol-40 parent was intentionally left
  untouched;
- end-to-end context compaction, beta.45 Kiro-store migration, a real non-Kiro
  ACP agent, mobile MQTT longevity, and several other high-risk paths remain
  unverified;
- the four confirmed product defects BUG-01 through BUG-04 are fixed on main,
  and their four-commit workbench passed the complete canonical
  `./dev.sh check`.

Minimum launch exit criteria:

1. complete the canonical rendered backend matrix from a product-protocol
   compatible QA environment without mutating the running protocol-40 parent;
2. select/preflight an authorized Hermes route and rerun both failed cases;
3. complete end-to-end compaction and beta.45-to-ACP migration validation;
4. exercise mobile MQTT and provider background lifecycle on the required
   clients;
5. run the mandatory post-land canonical check on clean main before release.

## Exact baseline and comparison range

The defensible baseline is annotated release tag **`v0.8.19-beta.45`**:

```text
tag object:    17e4183f90881e5623c3a0b0a93b5ddfe85d526d
peeled commit: 2adb4c16426b78475ad90380291d0598dff1be78
current main:  de2c5936cb68647165c068624bbd49e58e2d3658
```

Exact exclusive-start/inclusive-end range:

```text
2adb4c16426b78475ad90380291d0598dff1be78..de2c5936cb68647165c068624bbd49e58e2d3658
# equivalent:
v0.8.19-beta.45^{}..main
```

Git evidence:

```sh
git rev-parse v0.8.19-beta.45
git rev-parse 'v0.8.19-beta.45^{}'
git merge-base 'v0.8.19-beta.45^{}' main
git rev-list --left-right --count 'v0.8.19-beta.45^{}...main'
git rev-list --count 'v0.8.19-beta.45^{}..main'
git describe --tags --always main
git diff --stat 'v0.8.19-beta.45^{}..main'
```

Observed:

```text
merge-base: 2adb4c16426b78475ad90380291d0598dff1be78
symmetric counts: 0 81
main description: v0.8.19-beta.45-81-gde2c593
range: 140 files, 45,622 insertions, 5,399 deletions
```

`origin/main` also resolved to the beta.45 commit during analysis. Tracked app
versions remained `0.8.19-beta.45`; no new release-preparation commit existed.

## Complete new-functionality summary

### Context compaction and transcript durability

Commit `960703f4e06963453e2b520ca1a538fafee1db47` and its repair sequence add
manual and supervisor-triggered context compaction across desktop/mobile and
agent/team surfaces. The UI includes capability-aware controls, confirmation,
queued/running/failure state, metrics, timeline markers, and team aggregation.
Internally, Tyde adds native capability dispatch, safe inline fallback,
replacement binding, continuation installation, canonical transcript storage,
history pagination, replay/bootstrap handling, and protocol validation.
Evidence: `server/src/backend/compaction.rs`, `server/src/agent/mod.rs`,
`server/src/host.rs`, `server/src/store/transcript.rs`, `protocol/src/types.rs`,
`frontend/src/components/chat_view.rs`, `frontend/src/components/teams_panel.rs`,
`mobile-frontend/src/dispatch.rs`.

### Generic ACP backend and Kiro migration

Commits `f3c06d7d...` through `1242cc29...` replace the closed Kiro backend kind
with generic ACP while retaining built-in profile `acp:kiro`. Users can define
ACP commands, arguments, and adapters; setup and session schemas are intended
to be per profile. The backend now honors ACP capabilities/authentication,
id-less response chunks, and protocol `session/list`. Settings, sessions,
teams, agent-view preferences, and workflow histories migrate legacy `kiro`
values without discarding resumability. Evidence: `server/src/backend/acp/*`,
`server/src/store/legacy_backend_kind.rs`, `server/src/store/session.rs`,
`server/src/store/agent_teams.rs`, `server/src/store/agents_view_preferences.rs`,
`server/src/workflows/store.rs`, `frontend/src/components/settings_panel.rs`.

### Provider lifecycle, usage, and response handling

Claude subagent usage and Codex cumulative usage are reconciled more accurately.
Codex/Claude/Hermes background work is tracked until authoritative completion or
owner exit, including reconnect replay, late task adoption, child Codex
terminals, and Claude turns awakened by background completion. Hermes provider
iterations are split into separate Tyde messages with their own tools/usage.
Evidence: `server/src/agent/mod.rs`,
`server/src/backend/{claude,codex,hermes}.rs`,
`frontend/src/components/inflight_tray.rs`.

### Multi-pane composers and draft persistence

Every visible chat pane now owns its composer text, backend/profile/settings,
and tool actions. Drafts are keyed to exact chat/project/session/team ownership,
bounded by count/encoded size, debounced, and flushed at lifecycle boundaries.
Evidence: `frontend/src/state.rs`, `frontend/src/dispatch.rs`,
`frontend/src/components/{center_zone,chat_input,chat_view}.rs`,
`frontend/tauri-shell/src/lib.rs`.

### Desktop WebContent recovery

Tauri moves to 2.11.5. Apple WebContent termination triggers a bounded,
readiness-gated reload; repeated failures can produce a non-destructive native
restart-or-quit dialog while the host remains alive. Evidence:
`frontend/tauri-shell/src/lib.rs`, `frontend/src/app.rs`,
`frontend/src/bridge.rs`.

### Diff code intelligence

Eligible new-side live diff rows gain hover, go-to-definition, and references.
Old/stale/ineligible rows explain why they cannot answer. LSP subscriptions are
lazy and shared across file/diff tabs, with cleanup when the final owner closes.
Evidence: `frontend/src/components/{diff_view,file_view,code_intel_ui}.rs`,
`frontend/src/code_intel_dom.rs`.

### Hermes Settings

Hermes gains scalable profile selection, create/refresh/typed-delete flows,
Tyde-only provider disabling, probed model/fallback/toolset dropdowns, and
per-control text fallback. Profile creation copies only `config.yaml`; deletion
warns that it removes the selected entire `HERMES_HOME`. Evidence:
`frontend/src/components/hermes_settings.rs`,
`server/src/backend/{hermes,hermes_config}.rs`,
`protocol/src/hermes_config.rs`, `server/src/store/settings.rs`.

### Skill discovery

Tycode now receives projected, complete skill resources through native lazy
discovery instead of duplicated inline bodies. Hermes registers Tyde’s skill
store via profile `external_dirs`; projection/name normalization is shared with
Claude while retaining backend-specific contracts. Evidence:
`server/src/backend/skill_projection.rs`,
`server/src/backend/{tycode,claude_skills,hermes}.rs`.

### Host-scoped settings, errors, terminal, and supervisor safety

Settings visibly names the edited host, distinguishes device-only pages,
explains offline/read-only and chat-host mismatch, and offers connect/switch
actions. Home can create a terminal in host cwd without a project. Previously
silent action/send/command/startup failures now use a persistent dismissible
banner; first-run requires an installed enabled backend. Supervisor guidance no
longer treats pending work or a refusal as grounds to manufacture consent and
continue. Evidence: `frontend/src/components/settings_panel.rs`,
`frontend/src/components/{home_view,terminal_view,header}.rs`,
`frontend/src/{app,dispatch,send}.rs`, `server/src/backend/setup.rs`,
`server/src/agent/supervisor.rs`.

### Mobile MQTT and QA/build operations

Mobile transport now renews broker grants, uses unique authenticated control
nonces, maintains rendezvous during bounded candidate connections, sends an
immediate heartbeat, enforces peer liveness, and cleans abandoned/stale peers.
Operational changes also reduce wasm-test size by dropping DWARF, raise the
debug wasm shadow stack to 16 MiB, and add a mobile E2E OAuth harness. Evidence:
`mqtt-transport/src/*`, `mobile-frontend/src/app.rs`,
`server/src/connection.rs`, `.cargo/config.toml`, `tools/run-wasm-tests.sh`,
`mobile-frontend/e2e/live/*`.

## QA method and backend-by-backend results

### Method and scope

The authorized canonical live command was:

```sh
TYDE_RUN_REAL_AI_TESTS=1 cargo test -p tests --test backend real_ -- --ignored --nocapture
```

Result:

```text
running 20 tests
18 passed; 2 failed; 0 ignored; 20 filtered out; 37.80s
```

This command validates named automated backend scenarios. It does **not**
constitute the rendered manual dev-instance matrix or certify every changed
backend path.

Canonical `tyde_dev_instance_start` was attempted three times across the QA
passes. Every attempt timed out with:

```text
IncompatibleProtocol { client: 40, server: 41 }
```

This was product-protocol skew in the QA environment, not a change to the
`tyde-debug` MCP or UI-debug protocol. The running parent was compiled with
product protocol 40; the current-main child correctly required product protocol
41. The parent remained running and was not rebuilt, restarted, upgraded,
reconfigured, or otherwise mutated because this conversation depends on it.

A separate isolated current-main Tauri launch reached `frontend_ready`, loaded
the project, reported code intelligence Ready, and completed one minimal Codex
turn. That supplied targeted DOM evidence, but it did not provide canonical
store-isolation attestation, screenshots, a second client, or full rendered
backend certification.

### Backend results

| Backend | Automated result | Precise confirmed scope | Not certified by this result |
| --- | --- | --- | --- |
| **Tycode** | PASS | Two-turn cumulative usage; totals ended at input 20,319, output 65, reasoning 52. | Rendered lifecycle/tools, skills, background work, resume, compaction. |
| **ACP / built-in Kiro** | PASS | Generic-ACP Kiro turn, follow-up typing/streaming, duplicate-user-echo guard, interrupt/shared stream behavior, and explicit unavailable-usage contract. | Arbitrary ACP agent, auth, id-less third-party streaming, `session/list`, per-profile schema, migrations. |
| **Claude** | PASS | Cumulative usage, background-subagent parent resume, first-turn native child, resume, streaming, typing, interrupt. | Full background-command owner/replay matrix, rendered activity usage, compaction. |
| **Codex** | PASS | Cumulative/request usage, file-copy tool events, image input, naming, interrupt, resume, streaming, typing. | Root/child authoritative background-terminal tracking, reconnect replay, rendered usage transitions, compaction completion. |
| **Antigravity** | PASS | Runtime readiness plus grouped resume, streaming-delta, and typing cases. | Tools, usage, background work, automatic compaction, rendered lifecycle. |
| **Hermes** | **2 FAIL / UNVERIFIED** | The tests reached OpenRouter and retried. | No visible assistant result or Tyde MCP-bridge result was reached. Both stopped at HTTP 404: `No allowed providers are available for the selected model` using default `openrouter` / `anthropic/claude-haiku-4.5`. This is a provider/default-route or harness configuration block, not a proven Hermes product bug. |

The canonical command is therefore red even though its two failures do not
establish a Tyde Hermes defect.

## Targeted validations confirmed

- **Independent composers, in-session only:** two visible chats rendered two
  composers with distinct `beta draft` / `gamma draft` text through focus,
  tab hiding/restoration, and movement; only one global tool-output toggle
  rendered. No crossover/loss was observed in those exercised transitions.
  Reload persistence, bounds, owner recycling, and compaction retarget remain
  unverified.
- **Selected-host Settings:** Appearance showed device scope; General showed
  selected-host scope. Selecting offline `Offline QA` made host pages read-only,
  explained the state, offered Connect, identified the Local chat mismatch, and
  allowed switching back to Local.
- **Persistent failure surface:** the tested offline-host Connect failure
  produced a persistent `role="alert"` banner with a working dismiss control.
  The initial observation exposed implementation details; BUG-02 subsequently
  fixed the copy and added focused native/wasm coverage.
- **Generic ACP editor:** ACP was presented as supporting Kiro and other agents;
  command/arguments and Standard/Kiro adapters rendered; empty command was
  rejected; a named custom profile appeared separately in New Chat. The initial
  custom draft exposed BUG-01, which is now fixed. No real non-Kiro process was
  run.
- **Hermes safety UI:** real profiles/providers rendered; Add Profile stated
  that credentials/sessions/history are not copied; Delete required the exact
  profile name and named the target home. No create/delete/provider mutation
  was submitted against real state.
- **Home terminal:** with no project active, New Terminal created `/bin/zsh` in
  `/Users/mike/Tyggs/Tyde/frontend/tauri-shell`, independently matching the
  host process cwd.
- **Compaction affordance only:** after one Codex turn the UI showed an enabled
  Compact context control and `19.6K / 258.4K` occupancy, then entered native
  confirmation. Confirmation was not accepted; no compaction result was tested.
- **Code-intelligence readiness only:** the project reported Ready and ordinary
  files loaded. No diff-side behavior was exercised because main remained
  unmodified.

## QA environment finding and resolved product bugs

### QA-01 — Product-protocol 40/41 QA environment skew

- **Status:** Open QA-environment limitation; **not** a product bug and **not**
  a debug MCP/UI-debug protocol defect.
- **Severity:** High for release-validation coverage because it blocks the
  canonical rendered dev-instance matrix in this conversation.
- **Reproduction:** From the still-running beta.45 parent, call
  `tyde_dev_instance_start(project_dir="/Users/mike/Tyggs/Tyde")` against a
  current-main child.
- **Actual:** The parent sends product protocol 40 and the child requires
  product protocol 41, producing
  `IncompatibleProtocol { client: 40, server: 41 }`. Readiness times out and the
  attempted instance is removed.
- **Root-cause evidence:** Commit
  `960703f4e06963453e2b520ca1a538fafee1db47` changed
  `protocol/src/types.rs::PROTOCOL_VERSION` from 40 to 41 alongside real product
  wire changes. The compared `devtools-protocol`, UI-debug server/listener,
  `server/src/debug_mcp.rs`, dev driver, tool schemas, and debug documentation
  are unchanged Git blobs. The unchanged parent readiness code simply links the
  older product client.
- **Parent safety:** The running product-protocol-40 parent was intentionally
  left untouched. It was not rebuilt, restarted, upgraded, reconfigured, or
  mutated because this conversation depends on it.
- **Release impact:** Canonical store-isolated rendered certification,
  screenshots, and applicable second-client checks remain unverified. Complete
  them later from a product-protocol-compatible QA environment; do not describe
  this as a debug MCP or UI-debug protocol repair.

### BUG-01 — Generic ACP surfaces falsely identify Kiro

- **Original severity:** Medium.
- **Status:** **FIXED on main** by
  `d6fa8921b89b2e047666a275327cef5e9eb6b07c`
  (`fix: Use ACP identity in generic surfaces`).
- **Fix:** Session settings use `Session Settings (ACP)`. Shared schema-probe
  stages and malformed/empty model errors use ACP-neutral wording without
  echoing arbitrary response JSON. Adapter-specific Kiro startup errors remain
  truthful, while missing-session-id errors use the actual adapter display
  name.
- **Coverage:** Focused server and wasm tests cover custom/active ACP identity,
  malformed and empty model lists, raw-data non-disclosure, Stock identity, and
  adapter-specific Kiro identity.

### BUG-02 — Host failure copy exposes UUID and `JsValue(...)`

- **Original severity:** Medium for UX/support.
- **Status:** **FIXED on main** by
  `12eb8aab1048d502883349af37c482124821aefc`
  (`fix: Clean up host failure messages`).
- **Fix:** Host preparation/connection failures capture the configured label by
  exact id before awaiting. Tauri rejections accept only safe string messages
  and otherwise use stable generic copy without formatting, serializing, or
  logging arbitrary JavaScript values. Existing lifecycle and connection
  status contracts remain unchanged.
- **Coverage:** Native and wasm tests cover named/neutral copy, delayed rename,
  exact stored statuses, plain/Error/unknown rejection values, secret-bearing
  objects, throwing getters, and shared global/error-state cleanup.

### BUG-03 — ACP setup collapses real probe failures into command-not-found

- **Original severity:** Medium.
- **Status:** **FIXED on main** by
  `0cd92868bbf809cee908e26e00d17e470ea14269`
  (`fix: Preserve ACP setup diagnostics`).
- **Fix:** ACP setup now selects candidates by adapter, trims configured
  commands consistently with launch behavior, distinguishes missing paths from
  metadata/start/nonzero/timeout failures, preserves stable multi-profile
  status/code precedence, and emits bounded labeled aggregate diagnostics.
  Blank Kiro uses discovery; blank Stock fails clearly without spawning.
- **Coverage:** Deterministic tests cover the command-shape/classification
  matrix, adapter selection, mixed aggregation, UTF-8-safe bounds/truncation,
  and host-derived Kiro/Stock identities.

### BUG-04 — Config MCP status ignores configured ACP agents

- **Original severity:** Medium for operational/configuration tooling.
- **Status:** **FIXED on main** by
  `de2c5936cb68647165c068624bbd49e58e2d3658`
  (`fix: Use configured ACP status in MCP`).
- **Fix:** Config MCP reads its host settings and derives the same configured
  ACP setup agents used by the host/UI path before collecting status. Settings
  read failures return an MCP error result, and the existing response
  projection remains unchanged.
- **Coverage:** The injected handler seam proves built-in Kiro and configured
  Stock reach collection, rejects a regression to `&[]`, covers settings-read
  failure, and locks the response shape.

### Fix validation and landing

The final four-commit workbench at
`0b897d18e31bc21c913189cdd4069813b6551e8e` passed the sole canonical command:

```text
./dev.sh check
RESULT PASS (cache miss, 395s, 13 stages)
logs: /Users/mike/Tyggs/Tyde--fix-pre-beta-qa-bugs/target/dev-check-logs/run-20260729T234506Z-16511
```

All formatting, compilation, Clippy, native nextest, wasm-browser, web-loader,
and dev-check contract stages passed. The four commits were then cherry-picked
onto main in BUG-01 through BUG-04 order without conflicts, producing the landed
hashes above. A post-land clean-main `./dev.sh check` was not run during report
coordination and remains a mandatory release gate.

## Unverified release-critical gaps

1. **Context compaction (P0):** no accepted real compaction, native/fallback
   completion, busy deferral, queued input, interrupt/failure, metrics, marker
   deduplication, transcript/session preservation, continuation, restart,
   mobile, child, or team partial-failure evidence.
2. **Beta.45 migration and non-Kiro ACP (P0):** no upgrade of all five legacy
   Kiro-bearing store categories and no real stock ACP auth/id-less stream/
   capabilities/schema/`session-list`/resume/delete path. Static
   `Backend::list_sessions()` remains explicitly Kiro-only at
   `server/src/backend/acp/backend.rs:4310-4314`, and backend-kind-only ACP tier
   schema still resolves through Kiro; current user-facing caller impact needs
   audit.
3. **Canonical rendered certification (P0):** unavailable from the current
   protocol-40 parent because current-main children use product protocol 41.
   Debug MCP/UI-debug itself is unchanged, the running parent was deliberately
   untouched, and the manual Tauri fallback remains partial rather than
   canonical store-isolated certification.
4. **Hermes (P0):** visible content and MCP bridge remain unverified until an
   allowed provider/model route is preflighted and both failed cases rerun.
   Profile create/delete/provider changes also require disposable Hermes state.
5. **Mobile MQTT (P0):** no second client for pairing, grant renewal, immediate
   heartbeat, nonce compatibility, candidate races, stale-peer teardown,
   sleep/wake, network loss, or permanent authorization failure.
6. **Provider background lifecycle (P0/P1):** Claude background-subagent parent
   resume passed, but true root/child background terminals across turns,
   foreground/background discrimination, reconnect replay, owner loss, late
   adoption, rendered tray state, and Hermes iteration behavior remain open.
7. **Apple WebContent and durable drafts (P1):** no process termination,
   repeated/hidden recovery, native dialog, host survival, reload-time draft
   restoration, encoded-size eviction, or cross-owner reload check.
8. **Diff code intelligence (P1):** no unstaged/staged/stale/old-side/renamed
   diff behavior or subscription cleanup was exercised.
9. **Skill discovery (P1):** no real Tycode/Hermes selected skill with bundled
   resources, normalized naming, omission behavior, resume, or `external_dirs`
   preservation was observed.
10. **Supervisor and remaining setup/error edges (P1):** no refusal/question
    non-interference, unintended-stop retry, first-run installed+enabled gate,
    production remote write routing, project-terminal cwd, or timeout/nonzero
    presentation beyond static review.

## Evidence and limitations

- Analysis target after landing: local `main` at
  `de2c5936cb68647165c068624bbd49e58e2d3658`.
- Evidence sources: Git tags/history/diffs, repository code/docs, canonical
  live-test output summaries, process/protocol logs, and live DOM inspection.
- The full raw 20-test output was not preserved in the artifacts; the recorded
  aggregate, installed versions, named tests, and decisive output support the
  per-backend summaries, but a reviewer cannot independently reconstruct every
  pass or prove the absence of every skip line. A future run should retain the
  full log or a 20-row PASS/FAIL/SKIP ledger.
- Screenshot and second-client capabilities were unavailable. The manual Tauri
  runtime did not return canonical `storesEphemeral` or Hermes-containment
  attestation.
- Targeted validation used one minimal Codex turn. It did not authorize or
  perform destructive Hermes mutations or project edits for diff creation.
- BUG-01 through BUG-04 were implemented in a dedicated workbench and the
  final workbench passed `./dev.sh check` across all 13 stages. The fixes were
  landed locally without remote mutation. Clean-main post-land validation was
  not run during this report-only coordination step and remains required before
  release.
- Passing automated scenarios are not described as full backend certification;
  rendered and change-specific scope is stated separately above.
