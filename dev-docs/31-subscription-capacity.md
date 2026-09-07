# Subscription Capacity (Advisory)

Tyde shows the quota each backend reports for the account it is signed in to, so
a human or an orchestrator can avoid starting work on a near-exhausted
subscription.

**This feature is advisory. Full stop.** It never selects, reroutes, switches,
downgrades, or falls back between backends, accounts, or models. It never makes
a paid model call. It never guesses a number.

Collection is bounded and backend-native, and it does not require a
conversation. Every backend with a capacity source has an **out-of-band** one: a
short-lived provider process or connection that answers a read-only status call
and exits. The host polls those on its own schedule, so a backend nobody has
talked to still shows a real figure. No probe starts a model turn or spends
model tokens, every probe has a hard timeout, and a poll never overlaps another
for the same backend.

---

## 1. The two rules everything else follows from

**Vendor buckets are not comparable, so they are never merged.** A Codex
`primary` rolling window is not a Claude `five_hour` window. Credits are not a
percentage at all. There is no averaged score, no summed total, no single
"capacity" number anywhere in the system — `CapacityBucketId` is vendor-tagged
specifically to make cross-vendor bucket identity *unrepresentable*.

**Capacity is not token usage.** Tyde's own `TaskTokenUsage*` rollups measure
*this task*. Subscription capacity measures *your account*. They are not
summable, and token usage is **never** an input to capacity — not as a source,
not as a fallback, not as an estimate, nowhere. The token-usage popup renders
both, in separate labelled regions, precisely so the layout cannot invite the
arithmetic.

---

## 2. Where the data comes from

| Backend | Source | Out-of-band probe | Coverage |
|---|---|---|---|
| **Claude** | `get_usage` control request, with `rate_limit_event` as an early fallback | short-lived CLI in stream-json control mode | `AllVendorBuckets` |
| **Codex** | `account/read` + `account/rateLimits/read` | short-lived app-server | `AllVendorBuckets` |
| **Kiro** | `kiro-cli-chat chat --agent-engine v1 --no-interactive /usage` | the command itself | `RepresentativeBucketOnly` |
| **Antigravity** | `agy -p /usage` | the command itself | `AllVendorBuckets` |
| **Grok** | `_x.ai/billing` ACP extension | ephemeral admin ACP connection | `RepresentativeBucketOnly` |
| Hermes, Opencode, custom ACP agents, Tycode | — | — | `Unsupported { BackendHasNoCapacitySource }` |

Every probe relies on the provider's own existing login; Tyde does not open
credential files or copy credentials onto the wire. When a live session happens
to be running, its already-open channel is still used — the poll is the floor,
not the only path.

`_x.ai/billing` takes no `sessionId`, unlike `_x.ai/session/usage`: it is a
connection-scoped account read, which is what makes an ephemeral connection
sufficient. Grok's ephemeral connection is an admin session, and admin sessions
are excluded from `session/list`, so the probe never appears as a conversation.

**Which backends can be polled is a declared capability**, not a hardcoded list.
`BackendCapability::OutOfBandCapacity` (which requires `CapacityTelemetry`) is
what the poller, the seeded initial state, and
`BackendCapacitySnapshot::refreshable` all read. A backend that gains or loses an
out-of-band source changes one declaration and everything follows.

### Coverage is load-bearing, not a footnote

Coverage depends on the *source*, not only on the vendor, and Claude is the
reason it exists. Its passive `rate_limit_event` reports only the single limit
that is currently binding — the other limits still exist, and their utilization
is *unknown, not zero*, so that path is `RepresentativeBucketOnly`. Its
`get_usage` control read returns the complete `limits` array and is
`AllVendorBuckets`. Since the host polls `get_usage`, the complete report is now
the normal case and the representative one is the fallback.

Without `CapacityCoverage` on the wire a one-bucket report and a complete one
would render identically, and a user looking at a healthy row would have no way
to know a *different* limit sits at 98%. Grok reports one subscription period
and stays `RepresentativeBucketOnly`.

So **every UI surface renders coverage as text** — Settings and the
popup. Capacity has no MCP or agent-control exposure in this phase.
It is never a tooltip and never hover-only.

### Unit scales differ between vendors

Claude's `utilization` is a **fraction 0..1**. Codex's `usedPercent` is already
**0..100**, and Kiro prints a percent beside its credit totals. The backend
adapters convert these to a single 0..=100 scale exactly
once, at the boundary. This is a lossless unit conversion, not a semantic
normalization — and getting it wrong ships a 100×-off bar, so it is pinned by
tests on both the server and the UI side.

Kiro's typed `CreditUsage` measure also preserves the exact `used` and `limit`
values (`25.13 of 50`, for example). Its reset is date-only with no timezone, so
Tyde leaves `CapacityReset` as `NotReported` instead of inventing an instant.

### Provenance is per value, not per measure

`CapacityMeasure::UsedPercent` carries two numbers and they do **not** have the
same origin:

- **`used_percent` is always the vendor's own magnitude.** Claude reports it as a
  0..1 fraction and the adapter multiplies by 100. That is a *unit conversion*,
  not a derivation. Captioning the vendor's own percentage as "derived" is a lie
  about where the number came from, and the UI must never do it.
- **`remaining_percent` is Tyde's complement** (`100 - used`) unless the vendor
  supplies it directly. It is the **only** derived value anywhere in the model.

`ValueProvenance.vendor_reported` is the wire-compatible provenance flag for
`used_percent`. The protocol's
`used_percent_provenance()` and `remaining_percent_provenance()` identify the
two values independently: vendor-reported used and derived-complement remaining.

**UIs must call those two helpers and must not read `vendor_reported`
themselves.** The flag answers exactly one question — where the *used* figure
came from — and there are two symmetric ways to get this wrong, both of which
have already been shipped once and caught in review:

- Captioning the **used** figure as derived (attributing the vendor's own number
  to Tyde).
- Reinterpreting `vendor_reported` as the **remaining** figure's provenance
  (attributing Tyde's arithmetic to the vendor). `remaining_percent_provenance()`
  is `DerivedComplement` *always*; there is no input under which it is not.

### Vendor labels and bucket identity

The server derives `bucket.label` from the vendor's own naming rule:

| Claude `rateLimitType` | Server label |
|---|---|
| `five_hour` | "session limit" |
| `seven_day` | **"weekly limit"** |
| `seven_day_overage_included` | **"Fable 5 limit"** |
| `seven_day_opus` | "Opus limit" |
| `seven_day_sonnet` | "Sonnet limit" |
| `overage` | "overage limit" |

Codex labels are built the same way, from the vendor's own `limitName`:
`"{limitName} primary limit"` and `"{limitName} secondary limit"` (e.g.
`"subscription primary limit"`), plus `"credits"`.

Every label above is **distinct** — `seven_day` and `seven_day_overage_included`
no longer collide. Every surface nonetheless renders the vendor's own bucket type
alongside the label (`claude seven_day`, `claude seven_day_overage_included`,
`codex primary`, …), spelled exactly as the vendor spells it. The type is the
durable identity: it stays correct if a vendor's naming changes again, and it is
what keeps a Codex `primary` from reading as a Claude `five_hour`.

Three rules follow, all enforced in code:

- **Never invent a label.** The server's label is the authority; if it were ever
  absent, the UI falls back to the vendor bucket type, not to a made-up name. No
  model-family, plan-specific, or `limitName`-derived label is hardcoded in the
  frontend — including "weekly limit" and "Fable 5 limit", which the frontend
  only ever echoes.
- **Never derive the bucket type from `Debug`.** That prints
  `sevendayoverageincluded` — a name the vendor does not use.
- **Never treat a label as an identity.** Compare `CapacityBucketId`, never the
  display string.

### What Claude does and does not report

Worth stating because it is easy to assume otherwise, and because UI fixtures
must not invent it. Claude's **passive** path emits no scope, no window, no plan
label, and always a vendor status. Claude's **`get_usage`** path emits
`Account` scope, rolling windows, a plan label taken from the vendor's own
`subscription_type`, and no status. The two are different shapes from the same
backend, which is why fixtures must say which source they model. Codex
emits rolling windows on its two percentage buckets, **never** a status, and a
credits bucket with no window or reset. Codex's window buckets are scoped
`Individual` when the vendor sets `individualLimit`, and `Account` otherwise; the
credits bucket takes its scope from `rateLimitReachedType`, which reports a
workspace or organization condition when there is one and `NotReported`
otherwise.

The adapters mark a directly printed percentage as vendor-reported. If a source
reports only exact totals, the percentage may instead be derived from those
totals and is marked accordingly.

### Not sources — and why

- **`~/.claude/stats-cache.json`** — local *token* history. Inferring quota from
  token usage is the thing this feature must never do.
- **`~/.claude/policy-limits.json`** — enterprise policy, not quota, despite the
  name.
- **Codex `account/usage/read`** — experimental, returns token counts, not quota.
- **`anthropic-ratelimit-unified-*` headers** — land inside the Claude Code
  process; Tyde runs it as a subprocess and never sees them. (`rate_limit_event`
  is Claude's own forwarding of this data, which is exactly why only the
  representative bucket survives.)
- **`~/.claude/.credentials.json` for the plan label** — a secret-bearing file
  Tyde has never opened (and on some installs the data is in the Keychain
  instead, so a file read would be silently machine-dependent). Unnecessary
  anyway: `get_usage` reports `subscription_type` directly.
- **ACP `/usage`** — Kiro's ACP implementation reduces the response to a plan
  name and bucket count, discarding the numeric values. The V1 non-interactive
  CLI command is therefore the numeric source.

---

## 3. The state model

Six states, no more (`BackendCapacityState` in `protocol/src/types.rs`):

| State | Meaning |
|---|---|
| `Known` | Supported data retrieved and understood. |
| `Stale` | Last known report, past its freshness threshold **or kept alive through a failed refresh**. **The report is carried** — a stale number with an explicit stale marker beats no number, provided the UI says so. `last_error` says which of the two it is. |
| `Unavailable` | Supported source, no usable data right now. |
| `Unsupported` | This backend/version/account exposes no capacity source at all. |
| `AuthError` | Local credentials cannot authorize the status source. |
| `RateLimited` | The status source itself refused collection. |

"No data yet" is **not** a seventh state. It is
`Unavailable { AwaitingFirstReport }` — a typed *reason*, distinct from a
transport failure. It is **not zero usage and not "OK"**. It means neither
vendor has reported anything since this host started. A received incomplete
Codex notification becomes `Unavailable { MalformedReport }`, never a partial
report or a false awaiting state.

`Unavailable { MalformedReport }` is the other honest refusal: a report that
failed validation is discarded whole. Its values are not partially trusted and
no figure is shown.

**`Stale` is the normal steady state, not an error.** An idle account's data
simply ages. That is designed for and labelled, not hidden.

### A failed reading does not erase a good one

When a poll fails over a report the host already holds, the snapshot degrades to
`Stale` carrying that report plus `last_error`, rather than being replaced by
`Unavailable`. Replacing it renders as "no capacity data" while the host still
knows a real, recent figure — the worst available answer.

Two details are load-bearing:

- **The stored `retrieved_at_ms` stays at the original collection time.** A
  backend failing every retry must report its true age, not reset to "just now"
  on each failure. Freshness is recomputed from that original instant.
- **`MalformedReport` is deliberately *not* absorbed.** Unreachable and timed-out
  mean no answer arrived, so the last good number is still the best available.
  Malformed means an answer arrived and could not be interpreted — evidence the
  source has changed under us. Keeping an older figure alive on top of that hides
  a real breakage behind something that still looks like a reading. `Unsupported`
  is likewise never absorbed: it is a real retraction of the source.

`Unsupported { BackendNotInstalled }` is separate from
`BackendHasNoCapacitySource`: a backend that reports quota when installed, but is
not installed here, has a source — there is just no account to read. A host that
is configured not to probe real backends claims neither, and leaves the honest
`AwaitingFirstReport`.

None of `Unavailable`, `Unsupported`, `Stale`, `AuthError`, or `RateLimited` may
ever render as "has capacity". A hidden row reads as "fine", and an empty
progress bar reads as "0% used" — both are the exact lie this feature exists to
prevent.

### Freshness is the server's verdict, and only the server's

`CapacityFreshness` is `Fresh { age_ms }` or `Stale { age_ms, threshold_ms }`,
with a **60-minute** threshold for every backend. Clients render it **verbatim**
and never run a clock against `retrieved_at_ms` to second-guess it. If they did,
desktop and mobile would disagree about the same snapshot and both could drift
from the server's verdict.

The host recomputes `age_ms` from `retrieved_at_ms` on every emit and replay.
The stored timer still owns the one-hour state transition, but a late subscriber
receives its truthful current age rather than a frozen zero.

So a client can receive three real freshness shapes, and both UIs are tested
against all three: a just-recorded `Fresh { age_ms: 0 }` ("reported just now"); a
**late-joining** subscriber's `Fresh { age_ms }` with a real age, which must
render that age and never "just now"; and a report past the threshold, emitted as
`Stale`, which keeps the last known figure and marks it.

---

## 4. Ownership and scoping

Capacity is a property of **(host, backend)** — never global, never per-agent.

There is a subtlety worth stating plainly: reports are collected through a
**per-agent process**, but describe **account-wide** state. So the host actor
stores the snapshot at (host, backend),
accepts reports from *any* agent's connection for that backend, and never keys
it by agent. Closing an agent does not clear the snapshot.

Capacity fans out on the owning host's stream only. It never crosses a host
boundary, never lands on an agent/project/terminal stream, and never enters
session artifacts. A remote host runs its own Claude/Codex install signed in to
its own vendor account, so its capacity is that host's alone.

The initial typed replay follows the canonical host bootstrap ordering. It is
released after the first routed client request when one arrives during bootstrap,
or after a bounded idle grace for an otherwise idle client, so a required browse
or terminal bootstrap cannot be interleaved behind capacity.

### The polling schedule

One task per installed pollable backend, so a slow or wedged provider delays only
its own next reading:

- **First poll shortly after startup**, staggered per backend by up to 4 seconds.
  Deliberately small — showing capacity without starting a conversation is the
  point, so the first reading must not sit behind interval-scale jitter.
- **Then every 45 minutes**, plus up to 10% deterministic jitter. Under the
  60-minute freshness threshold on purpose: a healthy backend refreshes before
  its snapshot ages out, instead of flickering between fresh and stale.
- **On failure, retry from 5 minutes**, doubling to at most the base interval.
- **Never two polls at once for one backend.** A manual refresh arriving while a
  poll is in flight coalesces into it rather than starting a second provider
  process, so repeated clicking cannot stack up work.
- **`Unsupported` is an answer, not a failure.** An account that cannot report
  quota gets a full interval, not the failure backoff — otherwise the host would
  respawn a provider process every few minutes to be told the same thing.
- **The loop holds a weak host handle** and stops when the host is dropped.
  A strong one would keep every host that ever started polling alive, still
  spawning provider processes for hosts nobody is using.
- **Every probe bounds its own exchange and then tears down regardless.** The
  timeout wraps the read, never the cleanup: a timeout that dropped the whole
  future would orphan the provider process, and a poll on a timer would
  accumulate those.

A host configured not to probe real backends (`skip_real_backend_probe`) does not
poll at all and claims nothing about what is installed.

`BackendCapacityRefresh` is the client's on-demand request. It is refused for a
backend with no out-of-band source rather than silently ignored.

### Nothing is persisted

Snapshots are memory-only and recollected after restart. A rehydrated snapshot
from a previous server lifetime would render as `Known` while being arbitrarily
old — precisely the "silently treated as healthy capacity" failure. Quota moves;
a stale-but-confident number is worse than an honest absence.

The frontends follow the same rule: `backend_capacity` is cleared on host
disconnect, and the server replays the current snapshot on the next subscribe.

---

## 5. Privacy

- **No secrets are read.** Not `.credentials.json`, not the Keychain, not
  Codex's `auth.json`. Codex's `planType` arrives inside the rate-limits payload
  — a plan tier, not an identity.
- **No raw vendor payloads leave the server.** The typed snapshot is the only
  thing on the wire. This also guarantees Claude's `@internal` Slack telemetry
  fields (`overagePeriodMonthly`, `overagePeriodChannel`) are dropped at the
  adapter and can never escape.
- **No account identifiers** anywhere — no `account_id`, `accountUuid`,
  `organizationUuid`, or email — in payloads, logs, or artifacts.
- **`CapacityErrorDetail.summary` is curated.** Vendor error text is logged
  server-side and never echoed verbatim to a UI, so a token or header cannot ride
  out inside an error string.
- **Vendor payloads are untrusted input.** Out-of-range percentages and bad
  timestamps become `Unavailable { MalformedReport }` — never a silent clamp,
  never a default.

---

## 6. What the UI must do

The frontends are a pure projection of the server snapshot. They keep no cache,
run no freshness timer, and infer nothing. The refresh button asks the *server*
to collect; a frontend never reads a provider itself.

**The refresh button is rendered only where `BackendCapacitySnapshot.refreshable`
is true.** That flag is the server's answer to "can this host collect for this
backend", and it depends on what is installed here — which a frontend cannot
know. A UI must never derive the affordance from the backend kind, or it will
offer a dead button on a host where that CLI is absent.

**Desktop** — `frontend/src/components/backend_capacity.rs`:

- `SubscriptionCapacitySection` (Settings → Backends) is the full authoritative
  view: every backend the host reports, **including the unsupported ones**; the
  state and its explanation; the mandatory coverage line; the plan label when
  reported (and "plan not reported by this source" when not); and one row per
  vendor bucket carrying its label, the vendor's bucket type, its measure with
  per-value provenance, its scope, its window, its reset, and its vendor status
  — each of which may honestly be "not reported".
- `CapacityCompactRow` sits inside the task token-usage popup, in its own
  labelled region under a "Subscription · reported by <vendor>" heading.

**Mobile** — `mobile-frontend/src/components/backend_capacity.rs`: the same
states, the same coverage caveat, the same absolute timestamps, the same error
states, in a stacked layout. There is no mobile-only capacity model and no
mobile-only freshness maths.

### Rendering rules

- **A progress bar is drawn only for a vendor-reported `used_percent`** on a
  `Known` or `Stale` report. Never for credits (a balance is not a percentage),
  never for a bucket the vendor acknowledged without a magnitude, and never for a
  state with no report — an empty bar reads as "0% used".
- **Used and remaining render only when the vendor reported a magnitude**, and
  each carries its own provenance, taken from `used_percent_provenance()` and
  `remaining_percent_provenance()` (§2). The vendor's used figure is never
  captioned as derived, and the remaining complement is never attributed to the
  vendor.
- **Every bucket renders the vendor's own type** next to the server's label. The
  labels are distinct today; the type is the durable identity, and it is what
  keeps a Codex `primary` from reading as a Claude `five_hour`. The frontend
  hardcodes no label of its own.
- **In the compact row, exactly one bucket owns the bar** — the most constrained
  window. If a short window is fine but a weekly one is nearly exhausted, the
  weekly one is what will actually stop your work. Where the vendor names its own
  binding limit (Claude), that is the only bucket and this picks it by
  construction. Ties resolve to the later bucket in the vendor's ordering, which
  for Codex is the longer `secondary` window — two windows at equal utilization
  are not equally constraining, and the longer one takes longer to recover. The
  rest collapse to a `+N more` pointer at Settings. A report with **no**
  percentage bucket at all (Claude acknowledging a limit without a utilization,
  or a credits-only Codex report) renders as text with no bar.
- **Absolute time is authoritative**; relative durations ("resets in 2d 4h") are
  presentation only, derived from the server's absolute value, and always
  accompanied by the absolute time in the accessible text. A reset already in
  the past is stated as such — never a negative countdown, never clamped, never
  hidden. A missing reset is reported as missing and is **never** synthesized
  from the window duration (a rolling window's start is unknown).
- **Accessibility:** the bar is decorative (`role="img"`), and its `aria-label`
  carries the same sentence the text does — label, vendor bucket type, used % and
  its provenance, remaining % and its provenance, absolute reset, vendor status.
  Severity is never carried by colour alone. The coverage caveat is text, present
  in the accessible name.

---

## 7. Factual agent-control projection

The authenticated `tyde_list_launch_options` call projects a backend only when
Tyde holds an actual `CapacityReport` for it. The projection reuses the
canonical report, including factual buckets, coverage, source, and observation
time, and adds the host retrieval timestamp. Limits remain backend-scoped;
launch profiles map to that backend through the existing catalog rather than
duplicating quota per profile.

Known and retained stale reports use the same factual shape. The projection
does not expose or invent `Known`, `Stale`, `Unavailable`, `Unsupported`,
`AuthError`, or `RateLimited` classifications, health states, predictions, or
guesses. With no actual report, the backend is omitted. The agent bearer
credential is required so anonymous loopback callers cannot read account quota.

`tyde_spawn_agent` behavior is **unchanged**: no capacity input, no automatic
backend or model selection, no fallback, no downgrade, no reroute.

---

## 8. Explicitly not built

Stated plainly so none of it is mistaken for a gap to be quietly filled later.

- **Probes that cost tokens.** Every source above is a read-only status call. An
  opt-in "monitor limits" mode that spends quota to discover quota has no
  qualifying backend — every backend with a source already has a free one — and
  would contradict the rule that this feature never makes a paid model call.
- **A separate capacity MCP tool or routing policy.** See §7; list-options is a
  factual advisory projection only.
- **Claude capacity from `/api/oauth/usage`.** Out of scope — it is undocumented, would require Tyde to hold the user's OAuth token,
  and its `refreshOAuth: true` means a "status read" can *rotate stored
  credentials*.
- **Capacity before the first successful provider read** — represented as
  `AwaitingFirstReport`, never 0%.
- **Dollar amounts for Claude** — never reported by these sources.
- **Cross-vendor comparison, or any merged percentage.**
- **Org/workspace/seat disambiguation beyond what the vendor states** —
  `CapacityScope::NotReported` is a real, common, correct answer.
- **Any inference from token usage, local usage caches, plan names, or
  historical consumption.**
- **Automatic routing, fallback, model downgrade, or backend switching on
  capacity.** Not in this feature, at any layer, ever.
