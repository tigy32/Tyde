# Subscription Capacity (Advisory)

Tyde shows the quota each backend reports for the account it is signed in to, so
a human or an orchestrator can avoid starting work on a near-exhausted
subscription.

**This feature is advisory. Full stop.** It never selects, reroutes, switches,
downgrades, or falls back between backends, accounts, or models. It never makes
a paid model call. It never guesses a number.

Phase 1 is **passive-only**: both sources report as a side effect of turns the
user already ran. There is no polling, no background collection, and no refresh
action — there is nothing to refresh.

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

| Backend | Phase-1 source | Cost | Coverage |
|---|---|---|---|
| **Claude** | `rate_limit_event` on the existing stream-json pipe | zero | `RepresentativeBucketOnly` |
| **Codex** | `account/rateLimits/updated` on the existing app-server connection | zero | `AllVendorBuckets` |
| Antigravity, Hermes, Kiro, Tycode | — | — | `Unsupported { BackendHasNoCapacitySource }` |

Both sources are frames Tyde's subprocess pipes **already carry** and previously
dropped. Consuming them costs no new process, no network call, no credential
access, and no billing.

### Coverage is load-bearing, not a footnote

Codex reports every bucket it tracks. **Claude reports only the single limit
that is currently binding** — its other limits still exist, and their
utilization is *unknown, not zero*. Without `CapacityCoverage` on the wire, a
one-bucket Claude report and a complete Codex report would render identically,
and a user looking at a healthy Claude row would have no way to know a
*different* Claude limit sits at 98%.

So **every Phase 1 UI surface renders coverage as text** — Settings and the
popup. Capacity has no MCP or agent-control exposure in this phase.
It is never a tooltip and never hover-only.

### Unit scales differ between vendors

Claude's `utilization` is a **fraction 0..1**. Codex's `usedPercent` is already
**0..100**. The backend adapters convert both to a single 0..=100 scale exactly
once, at the boundary. This is a lossless unit conversion, not a semantic
normalization — and getting it wrong ships a 100×-off bar, so it is pinned by
tests on both the server and the UI side.

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
`used_percent`, and both Phase-1 adapters set it to `true`. The protocol's
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
must not invent it: Claude's adapter emits **no scope and no window, ever**
(both `NotReported`), **no plan label**, and **always** a vendor status. Codex
emits rolling windows on its two percentage buckets, **never** a status, and a
credits bucket with no window or reset. Codex's window buckets are scoped
`Individual` when the vendor sets `individualLimit`, and `Account` otherwise; the
credits bucket takes its scope from `rateLimitReachedType`, which reports a
workspace or organization condition when there is one and `NotReported`
otherwise.

Both adapters set `provenance.vendor_reported: true`. That is the **only**
`UsedPercent` shape either one produces, and it is the only shape a UI fixture
may use.

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
- **Claude's plan label** — lives in `~/.claude/.credentials.json`, a
  secret-bearing file Tyde has never opened (and on some installs the data is in
  the Keychain instead, so a file read would be silently machine-dependent).
  Claude's plan is therefore reported as **absent**, not guessed.
- **Codex `account/rateLimits/read`** — an *active* read whose auth, error, and
  billing behavior is unverified. It is **gated**: not implemented, not called,
  not polled, until a bounded one-shot verification is explicitly approved.

---

## 3. The state model

Six states, no more (`BackendCapacityState` in `protocol/src/types.rs`):

| State | Meaning |
|---|---|
| `Known` | Supported data retrieved and understood. |
| `Stale` | Last known report, past its freshness threshold. **The report is carried** — a stale number with an explicit stale marker beats no number, provided the UI says so. |
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

**`Stale` is the normal steady state, not an error.** With passive-only sources,
an idle account's data simply ages. That is designed for and labelled, not
hidden.

None of `Unavailable`, `Unsupported`, `Stale`, `AuthError`, or `RateLimited` may
ever render as "has capacity". A hidden row reads as "fine", and an empty
progress bar reads as "0% used" — both are the exact lie this feature exists to
prevent.

**`AuthError` and `RateLimited` are not reachable in Phase 1.** They exist in the
protocol and the UI renders them, but no passive code path produces them; they
arrive with the gated Codex read. Nothing may fabricate a fixture for them.

### Freshness is the server's verdict, and only the server's

`CapacityFreshness` is `Fresh { age_ms }` or `Stale { age_ms, threshold_ms }`,
with a **60-minute** threshold for both backends. Clients render it **verbatim**
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

There is a subtlety worth stating plainly: both passive sources arrive on a
**per-agent pipe** (Claude's `rate_limit_event` carries a `session_id`; Codex's
notification arrives on that agent's app-server connection), but both describe
**account-wide** state. So the host actor stores the snapshot at (host, backend),
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
run no freshness timer, infer nothing, and (in phase 1) offer no refresh button.

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

## 7. Orchestrator exposure — none in Phase 1

Capacity has **no MCP or agent-control surface at all** in Phase 1. It is not
returned by `tyde_list_launch_options`, not exposed by any new tool, and not
readable by an agent. The only consumers are the desktop and mobile UIs, from the
`BackendCapacity` host-stream event.

`tyde_spawn_agent` behavior is **unchanged**: no capacity input, no automatic
backend or model selection, no fallback, no downgrade, no reroute.

If capacity is later surfaced to orchestrators, it must return the same typed
states as the UI — including `coverage` — and must be scoped to the caller's own
host. `Unavailable`, `Unsupported`, and `Stale` may never be returned as
"available capacity". None of that exists yet, and nothing should be written as
if it does.

---

## 8. Explicitly not built

Stated plainly so none of it is mistaken for a gap to be quietly filled later.

- **Any background polling, for either backend.** Phase 1 is passive-only.
- **Any MCP / agent-control / orchestrator exposure.** See §7.
- **A refresh action.** Nothing to refresh; the UI says so rather than showing a
  dead button.
- **`AuthError` and `RateLimited` state in practice.** The protocol carries them
  and the UI renders them, but no passive code path emits them. No fixture may
  pretend otherwise.
- **Codex `account/rateLimits/read`** — gated, pending an explicitly approved
  one-shot verification of its auth/error/billing behavior.
- **Claude multi-bucket capacity.** Claude reports one binding bucket;
  everything else is `RepresentativeBucketOnly`. `/api/oauth/usage` is out of
  scope — it is undocumented, would require Tyde to hold the user's OAuth token,
  and its `refreshOAuth: true` means a "status read" can *rotate stored
  credentials*.
- **Claude capacity before the first turn** — no data exists.
  `AwaitingFirstReport`, not 0%.
- **Claude plan/limit label** — would require reading a secret-bearing file.
- **Codex capacity with no Codex agent running** — no app-server connection, no
  notification.
- **Dollar amounts for Claude** — never reported by these sources.
- **Cross-vendor comparison, or any merged percentage.**
- **Org/workspace/seat disambiguation beyond what the vendor states** —
  `CapacityScope::NotReported` is a real, common, correct answer.
- **Any inference from token usage, local usage caches, plan names, or
  historical consumption.**
- **Automatic routing, fallback, model downgrade, or backend switching on
  capacity.** Not in this feature, at any layer, ever.
