# Overlapping generation accounting

Messages-mode accounting reconciles each running turn snapshot once per
`TypingStatusChanged` generation. A response ID locates its generation, so
metadata revisions replace that generation's snapshot rather than adding
another copy. Older response metadata cannot supersede a newer response's
snapshot. Request-only observations remain individually keyed until a turn
snapshot covers them. Messages-mode task ownership uses these own-generation
measurements, not provider session-cumulative metadata; reported cumulative
message metadata is preserved. ModelRequests-mode native cumulative accounting
is unchanged. The UI continues to render server-emitted usage.

Availability and optional-field coverage use the same generation source as
the counters. A complete running snapshot supersedes covered unavailable or
partially reported responses, including stale metadata. Newer, uncovered
request-only and unavailable responses remain visible until a snapshot covers
them; a complete turn cannot certify another generation's missing history.

## Regression provenance

| Change | Introduction date | First containing release tag |
| --- | --- | --- |
| Shared response-keyed accumulation, `8ce845601d7b1fcaf64308feb467103deef87638` | 2026-07-27 | `v0.8.19-beta.46` (2026-07-29) |
| AGY running-generation emission, `14b34c0cee40a673e7916cb139f29bfa67725b31` | 2026-08-25 | `v0.8.19-beta.76` (2026-08-26) |

The exact trigger is a Messages-mode generation with distinct response IDs
carrying overlapping known running-turn totals and no known provider
cumulative total. The shared accumulator adds those snapshots as independent
responses. AGY's per-request counters and running-generation emission are
correct; their interaction exposed the shared bug. Both paths first appear
together in `v0.8.19-beta.76`; the first containing stable tag is `v0.9.0`
(2026-08-29).
These are local Git tag/ancestry boundaries, not proof of an installed version
or a provider billing error. The immediately preceding tags
`v0.8.19-beta.45` and `v0.8.19-beta.75` exclude the respective commits.

Independent review of the unreleased fix candidate `2949d487` found a second
regression: numeric generation replacement retained response-keyed status.
An unavailable earlier response followed by a complete turn left stale
`BackendDidNotReport`; missing earlier optional fields likewise polluted
coverage. It has no affected release: the protocol flow now exercises
unavailable-to-covered recovery, optional-field upgrades, stale revisions,
and genuinely uncovered request-only/unavailable responses before final
coverage. This belongs to the same feature commit, not a released regression.

The generation tracker introduced in unreleased candidate `2949d487` on
2026-10-28 also left a restored working edge open when the host was killed
after a measured response but before idle. A new turn could then replace the interrupted generation's
measurement. Resume-start normalization now durably appends a typed
`MessageAccountingBoundary` before reducing live backend events, then feeds
its idle edge through Messages accounting. Accounting replay consumes this
record, but public history and lifecycle do not: queued resume work never
publishes a misleading idle. A failed boundary write stops resume through the
existing accounting storage error path. Historical response mappings remain
intact. The existing protocol flow uses the shared hard-kill fixture after a
gated measured response, verifies both prompt paths, and closes/reopens each
resulting journal to verify durable task/backend/model totals. No released
version contains this candidate.

Configured review of unreleased `d2b3fbb5` found that its in-memory-only
normalization lost interrupted usage again on queued-resume journal replay.
The diagnostic `7ed45688` established that case 0 instead stalled waiting for
an exact bare mock reply. The mock's actual rendering path prepends startup
MCP/customization metadata to every ordinary echo; equality to bare text can
never match that valid output. The flow now matches the exact unique echoed
prompt suffix, retaining its accounting assertions and bounded content-free
event checkpoints. No claim about delivery versus absence in the diagnostic
run follows from its watchdog. The new queued-reopen assertion distinguishes
the durable boundary fix from the prior in-memory-only candidate.

Configured review of `0c313149` found that this boundary write also treated
discarded, uncommitted journal-tail bytes as a fatal error, even after the
journal repaired the tail and durably wrote the boundary. Only a failed write
or an unreadable committed record prevents preserving the boundary. Successful
appends use the existing transcript-persistence reporting path: repaired tails
remain visible as incomplete accounting coverage, including after another
reopen, without making the resumed agent unusable. The protocol flow adds a
torn tail to an inactive fixture journal after hard kill, then verifies queued
resume, retained measured totals, partial coverage and durable replay. Fatal
resume errors fail that wait directly; they cannot disappear into a reply wait.

The same review found a Hermes contract mismatch: clarify and approval waits
emit idle/working edges inside one native turn, but message turn snapshots
still included requests from before the wait. Adding both generations then
counted the earlier requests twice. The Backend contract now explicitly scopes
Messages-mode `turn` snapshots to the emitted typing generation. Hermes keeps
its native turn sum for `ModelRequestTokenUsage` unchanged and separately
accumulates message-generation usage, resetting only when its own working
authority transitions from idle to working. This applies to submitted answers,
external resolutions and expiry as well as ordinary new prompts. Shared server
accounting never interprets question/tool events to invent a boundary.

`real_question_usage_accounting` drives the real Backend trait through launch,
a native question, its actual offered answer and settled output. It collects
events to completion without drain sleeps or local deadlines, folds late usage
onto the response's original generation, and compares each generation's final
turn measurement with its distinct request sum. Only prompt text differs by
provider. Its explicit Messages tracking-mode and question/request/turn-usage
capability gates admit Claude, Antigravity and Hermes; native ModelRequests
ownership (including Codex) is outside this contract. No real-provider result is
claimed until this exact case runs against every eligible backend, including a
Hermes control without the adapter correction.

### Validation collateral: deterministic composer checks

The elapsed-time assertion in `composer-webkit.spec.mjs` was introduced by
`552bdcfb591e5b0c7722b4197f457e4b8cea589c` on 2026-09-26, first contained
in `v0.9.4-beta.12` (2026-09-27). Its trigger is a measured resize cycle
reaching 300 ms, including a deliberately injected 250 ms wait, regardless of
correct rendering. The retained failing run has all 12 resize rows passing
height-change, current/original hit-target, native-value and stable-node
assertions; all four composition/send cases and the menu hit assertions also
pass. Only one elapsed value, exactly 300, rejects that correct product state.

Main `70d31a7f` retired this duplicate WebKit harness after moving its
contracts into the mounted-DOM app flow. Keep that canonical path instead of
resurrecting its obsolete Queue/Send expectations. Its six grow/shrink shapes
run before and during contact: 12 deterministic resize cases retain height,
hit-target, native-value, stable-node and typed-send assertions, synchronized
to render completion. No production UI behavior, elapsed threshold, delay or
retry is introduced.

## Historical repair: offline evidence and copies only

The initially authorized audit scope is the three Antigravity ledgers A, B,
and C from the 2026-10-28 handoff: 924 own requests, nine generations, 5,523
journal records. This does **not** authorize installing repaired data,
restarting the primary instance, deployment, or expanding the audit scope.
Root must approve any production operation separately.

`tools/repair-overlapping-usage.py` reads a hash-pinned offline database and
one to three hash-pinned ledgers. Prepare a consistent, quiescent database
snapshot with no adjacent WAL; retain the original database and journals.
Never point a write-capable SQLite client at production to prepare evidence.
The manifest is private: it contains session IDs and evidence paths. Do not
commit it or print it. The tool emits only aliases, hashes, counts and token
counters. It refuses missing requests, ambiguous ownership, inherited history,
conflicting revisions, duplicates, unfinished generations, unexpected saved
snapshots, and mismatched hashes or audited sums.

Manifest schema (`null` means unreported, not measured zero):

```json
{
  "version": 1,
  "database": {"path": "/offline/sessions.sqlite3", "sha256": "..."},
  "ledgers": [{
    "alias": "C", "path": "/offline/C.jsonl", "sha256": "...",
    "session_id": "PRIVATE", "requests": 2, "generations": 1,
    "expected": {
      "input_tokens": 24045, "output_tokens": 8123,
      "total_tokens": 32168, "cached_prompt_tokens": 16128,
      "cache_creation_input_tokens": null, "reasoning_tokens": 5411,
      "prompt_tokens": 40173
    },
    "saved": {
      "total_tokens": 49152, "prompt_tokens": 56300,
      "input_tokens": 40172, "output_tokens": 8980,
      "cached_prompt_tokens": 16128, "reasoning_tokens": 5972
    }
  }],
  "expected_aggregate": {}
}
```

The example contains C's audited counters but placeholder paths, hashes,
identity and aggregate: supply every saved archive counter exactly as stored
(including `partial_fields` if present) and all aggregate counters. For A and B the audited request counts are 249
and 673, and prompt sums are 37,944,212 and 89,069,771. Do not manufacture any
missing field from a ratio or another agent's totals.

The full three-ledger aggregate must equal:

| Counter | Unique own-request sum |
| --- | ---: |
| Uncached input | 4,006,305 |
| Cached prompt | 123,047,851 |
| Prompt | 127,054,156 |
| Output | 433,248 |
| Reasoning (output subset) | 236,681 |
| Native total (excludes cache in these ledgers) | 4,439,553 |
| Cache creation | unreported |

Dry run, then optionally generate new repaired copies:

```sh
python3 tools/repair-overlapping-usage.py --manifest /offline/private-manifest.json
python3 tools/repair-overlapping-usage.py --manifest /offline/private-manifest.json \
  --output /offline/new-repaired-copy
```

The second command creates a **new** private directory, retaining originals.
It changes only already-known derived cumulative message counters and the
selected task archive usage amounts. Request, turn, native checkpoints,
provider identities, ownership, navigation, model/backend attribution,
observation state, timing, and all other database rows remain untouched.
`proof.json` records per-ledger hashes, counts, before/after values, and the
aggregate independently checked against the manifest. Copies are not installed
and no server is contacted. Inspect and retain the proof before requesting
Root's explicit approval for a separately planned offline installation.

This cannot certify provider billing, original wire frames, native Codex
display totals, upstream reasoning provenance, or missing earlier history.
The aggregate is a sum of unique own-request journal records. The separate
custodian audit matched all 924 native input/cache/output counters; the sum
does not independently establish upstream reasoning or provider billing.

## Executed audit receipt

The implementation dry run independently recomputed the scoped immutable
copies: A 249 requests / 3 generations / 1,458 records; B 673 / 5 / 4,033;
C 2 / 1 / 32. All 924 unique own-request sums matched the handoff aggregate
above. The copy-only repair retained original hashes and every journal field
other than already-known derived cumulative usage. C's repaired native total
is 32,168, prompt is 40,173 and reasoning subset is 5,411. Provider request,
turn and native checkpoint values were not rewritten. Production remains
unchanged; the repaired copies are evidence, not installed data.

The protocol regression observes the canonical projections: reconnects use the
new subscriber-owned stream; bootstrap folds metadata into response messages;
closed roots are absent from advertised task rollups. Retention is checked by
reopening the saved session and again after host restart, not by expecting an
unadvertised closed root or raw metadata frames in bootstrap.

The shared-server flow uses the resumable Claude-labeled Messages-mode mock
with AGY-shaped running snapshots. Antigravity sessions are explicitly
non-resumable; the test does not override that lifecycle contract or claim
real-provider coverage. The same protocol flow verifies its emitted backend
and model attribution along with durable accounting.
