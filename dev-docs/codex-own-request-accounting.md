# Codex own-request accounting

This change is separate from the Messages-mode generation-overlap repair.
Codex reports ModelRequests; the server persists NativeAbsolute checkpoints
and task rollups sum each agent's own absolute total. Those server contracts,
provider limit percentages, usage periods, and lifecycle authority are unchanged.

## Evidence and introducing history

The accepted private audit handoff is
`~/tyde-evidence/codex-usage-audit-20261028/IMPLEMENTER-HANDOFF.md`.
It covers deployed Tyde 0.9.6-beta.12 and Codex 0.162.1. The 384-entry
closed-task sample is not a corrected total for the complete task ancestry.
Identifiers, conversation content, and complete provider records are private.

* Ordinary fork: the saved child total includes a parent baseline, although
  the parent is counted separately. The audited example has four own requests:
  520,812 prompt tokens versus 23,129,096 saved; the 22,608,284 excess exactly
  matches inherited usage. Trigger: fork an already-used session and generate
  a child request.
* Compaction: one audited request contributes 253,695 prompt and 4,022 output
  tokens that the saved cumulative checkpoint omits. Its exact native request
  completion precedes compaction. The historical route is **not proven** by
  a live transport capture. Upstream `compact_remote_v2::collect_compaction_output`
  calls `record_observed_response_completed`, but not `update_token_usage_info`;
  ordinary compaction updates both. This explains a possible production route,
  not proof of which route the historical request used.

The cumulative-trust path was introduced by `d6698dd975c93d4164cda2e9bc6d588e0b0ef63e`
on July 10, 2026. Its parent lacks that path. Ancestry checks include it in
`v0.8.19-beta.24` and exclude it from `v0.8.19-beta.23`; the first containing
stable tag is `v0.9.0`. These are source/release boundaries, not installation
dates. The provider release/commit/date that first exposed the compaction
omission is **unknown**. The addition of native request recording does not
establish that omission's onset.

## Accounting source and invariants

`codex::usage::OwnRequestUsage` reads native `token_usage_record` entries from
exactly the path returned by `thread/read`. It accepts only records explicitly
owned by the observed thread, deduplicates native response identities, and
rejects conflicting repetitions. It never sums native turn/thread snapshots,
uses cumulative-difference heuristics, or substitutes another accounting source.
Required scalar/cache fields must be present. Missing optional reasoning stays
unknown, including in aggregate usage. Cache reads/writes are disjoint prompt
subsets; reasoning is an output subset. Invalid subsets and inconsistent native
totals fail explicitly, without clamping them into plausible measurements.

The inbound owner reconciles on native token-usage notifications and terminal
turn events, **before** manual-compaction interception. Terminal reconciliation
uses the provider's pre-terminal rollout durability barrier. Live reads can
leave a partial final line for the next event; they never invent an incomplete
request. Accounting is not a lifecycle signal: native typing events still own
working/idle state, and the response splitter retains its existing boundaries.
Native context-window occupancy remains a separate provider measurement.
Per-response footers retain that notification's native request usage rather
than borrowing the latest ledger row when the reader has read ahead.

Resume rebuilds the same own-thread ledger and emits its final checkpoint,
not intermediate historical totals. Repeated request IDs remain idempotent
across replay/restart. An interrupted fork can have inherited completed turns
but no completed own requests. Replay therefore validates successful own
completion records, not transcript nonemptiness: native session metadata and
thread-settings checkpoints explicitly identify the owner of subsequent native
completion records, including copied ancestor prefixes. A settings checkpoint
without a nonblank string owner fails explicitly; it cannot erase ownership and
silently bypass the required-completion checks. Older snapshots without an owner
therefore cannot establish this exact accounting contract. A successful own turn
without exact request records still fails explicitly, on replay as well as live.
An unused fork emits no invented zero-valued request or parent checkpoint.
Native subagents use the same ownership rule rather than subtracting an inferred
first-request baseline. Shared server checkpoint and rollup code is not changed.

Local reads consume only newly appended bytes, bounded by the file size at
read start. SSH-hosted Codex reads through that app-server's native `fs/readFile`
API, then consumes only the unread suffix. This API transfers the complete
file and has the provider's file-size limit; large remote ledgers therefore
remain an explicit performance/compatibility risk, not a silent alternative
read path. No polling, timers, transcript-based status, or async locks are added.

## Contract eligibility

`DurableOwnRequestUsage` promises identity-bearing own totals across forks and
resume/restart; `CumulativeUsageReported` promises only session-cumulative usage
and explicitly permits unavailable cumulative scope after attaching mid-session.
`CompactionRequestUsageReported` separately promises native compaction performs
metered requests and reports those requests. `CompactionReported` promises a
compaction lifecycle, not billing evidence. Dependencies are validated in the
capability declarations. Codex now advertises these two stronger contracts.

This distinction was established by real runs, not backend-name exclusions.
The initial overly broad draft eligibility exercised Claude/Muse fork and
Claude/Grok/Hermes/Muse compaction. Muse fork passed. Claude did not satisfy
own-only cumulative semantics; all four compaction runs supplied no request
observations. Those are recorded failures of the draft eligibility, not passes
or evidence of exact zero-cost compaction. The existing capability definitions
never promised durable own billing or metered compaction, so neither their
providers nor the assertions are changed to pretend otherwise. The same strong
assertions now apply to every adapter declaring the actual new contracts.

## Validation scope

The real-provider scenarios `real_fork_own_request_usage` and
`real_compaction_own_request_usage` use identical setup, collection, and
assertions for every capability-eligible backend. They exercise ordinary fork,
multiple child requests, two independent resume/restarts, paid compaction,
follow-up, replay, and exact unique-request sums for every token field.
The compaction fixture now retains, rather than discards, request observations;
the resume fixture also retains its accounting replay. Neither uses a scripted
provider or a synthetic accounting event.

`real_interrupted_fork_own_request_usage` adds a distinct interruption/restart
flow without altering either original scenario or its assertions. The existing
real-inference transport gate holds the child's first request before forwarding
it upstream. After native working and cancellation events settle, two fresh
resumes must preserve inherited history while reporting no own requests. The
first actual child completion must then satisfy the same all-field own-usage
oracle. Parent and follow-up requests use the real provider. No sleeps, latency
thresholds, synthetic replies, or races against model completion establish the
precondition. Its additional `Interrupt` requirement is necessary for this
scenario; neither stronger accounting capability is weakened.

The first real interrupted-fork control held/released one request with no
transport/upstream errors, zero own requests, one cancellation and native idle.
It failed before resume because the in-progress cancellation helper requires
an assistant response (`starts > 0`), but no upstream response had been allowed.
That helper and its existing callers remain unchanged. This pre-response case
instead requires zero start/delta/reasoning/end events and exactly one working,
cancellation and idle in that order. This strengthens its actual precondition;
it does not fabricate an empty assistant response or relax the resume/usage
assertions. The two original accounting controls already reproduced their
intended failures; neither scenario nor its common accounting oracle changed.

The later Native run passed all backend assertions in this flow, but the outer
proxy correctly failed its integrity check. A single observation-only run then
reproduced `BrokenPipeError` while copying a non-inference HTTP 200 response,
after the held request and before child exit. The recorded route was only
classified as non-inference; its exact historical endpoint is unproven. The
fixture had routed auxiliary traffic through an inference-only gate and treated
its disconnected copy as an inference failure.

The fixture now uses Codex's explicit `model_catalog_url` for the real catalog,
separately from its loopback inference `base_url`; native catalog discovery is
not replaced or synthesized. Codex 0.162.1's provider config and models endpoint
implement these independent routes. Unexpected non-inference gate requests fail
explicitly. Existing transport/upstream integrity assertions remain unchanged;
content-free exception/phase diagnostics are retained. Proxy workers are joined
before receipt publication so no active worker can change counters after they
are asserted. Native backend lifecycle and inference forwarding are unchanged.

On committed predecessor `27d9e3a3`, Codex 0.162.1 with `gpt-5.6-luna` passed
both new scenarios, existing `real_usage_accounting` (including a multi-request
turn and context updates), and `real_conversation_in_native_subagent`. The
conformance launcher required `TYDE_TEST_PROCESS_EXECUTABLE` to point at the
`./dev.sh check`-built `target/debug/tyde-server`; the first attempt without it
failed setup and was not coverage.

The live compaction fixture has four unique own requests. Its native
`compacted.compaction_response_id` explicitly identifies one own request with
27,860 prompt (27,648 cached), 150 output and zero reasoning tokens. The final
native token-count snapshot omits exactly that request in every token field;
the repaired adapter's conformance totals include it exactly once. This
establishes the actual remote-v2 route for the new fixture, **not** historical
C003. Private bounded, read-only native-source inspection retained only counts,
field totals and route/identity-match presence.

The committed-workbench gate passed formatting, all-target lint, native/server
builds and three cross-platform process-launch checks. Its native stage ran
578 tests: 575 passed; three failed because the inherited runtime denies the
required Bubblewrap namespaces. Later wasm/web stages were blocked. Native
execution took 150.05 seconds; cold compilation/setup dominated the earlier
stages (roughly 19 minutes). A warmed complete gate is not measured.

Local configured review could not start its native author fork because of the
same namespace restriction. A separately assigned proper Native executor then
completed genuine configured review of `133b9b16..1d6acc8b`. It found one P2:
the unpublished resume guard confused nonempty inherited history with missing
own accounting and rejected a fork interrupted before its first completion.
The final capability/five-failure audit found no additional issues and did not
relabel exclusions as passes. At that checkpoint, the P2 correction and its new real scenario had not yet
received executable qualification or updated-source review. Unrelated
creation findings are untouched. The separate Claude fork mismatch remains an
open candidate with original evidence/provenance; its failing phase and cause
are not established. No isolation policy was weakened or namespace test skipped.

Final qualification requires fail-before/pass-after evidence against the
broken/fixed adapter for every eligible backend. The live route evidence above
does not replace the separate fail-before executable proof. Auth/readiness or
namespace failures are failures, not skips. `./dev.sh check` remains mandatory
on the committed workbench. No paid scenario or failed review is certification.

## Historical repair proposal: read-only/copy-only

Do not modify installed ledgers or saved task accounting in place. A separate,
explicitly approved repair should first take immutable copies of the app
ledger, native ownership metadata, and native request sources at a recorded
cutoff. Pin hashes and sizes; exclude archived snapshots superseded by restored
live sessions. Resolve explicit thread ownership, deduplicate response identity,
and recompute all own token fields from exact requests. Derive task rollups
from those corrected own values, never from a guessed full-task multiplier.

Dry-run output should contain only aliases, counts, field-level before/after
totals, source completeness, and ambiguous/missing-source failures. Any output
ledger must be a newly created copy with an independently checked manifest;
installation and rollback require a separate owner decision. The accepted
sample neither certifies all 419 ancestry entries nor authorizes live repair.
