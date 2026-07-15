//! Subscription capacity — advisory rendering of the server-owned
//! `BackendCapacity` snapshot.
//!
//! Everything here is a pure projection of `state.backend_capacity`, which the
//! server replays on host-stream subscribe and re-emits on every change. The
//! frontend runs no freshness clock (staleness is `CapacityFreshness`, computed
//! server-side), keeps no cache, infers nothing, and offers no refresh action —
//! both phase-1 sources are passive, so there is nothing to refresh.
//!
//! Two rules keep this honest and are load-bearing:
//!
//! 1. **No merged percentage.** Vendor buckets are incomparable (a Codex
//!    `primary` window is not a Claude `five_hour` window; credits are not a
//!    percentage at all). Buckets are rendered individually and never averaged,
//!    summed, or collapsed into one figure. A progress bar is drawn only for a
//!    bucket the vendor reported an authoritative `used_percent` for — never for
//!    credits, never for a magnitude-less bucket, and never for a non-`Known`
//!    state, because an empty bar reads as "0% used".
//! 2. **Capacity is not task token usage.** Task tokens are *this task*;
//!    subscription capacity is *your account*. They are not summable, so the
//!    compact row lives in its own labelled region with its own heading and its
//!    accessible text never combines the two.

use leptos::prelude::*;
use protocol::{
    BackendCapacitySnapshot, BackendCapacityState, BackendKind, CapacityBucket, CapacityBucketId,
    CapacityBucketStatus, CapacityCoverage, CapacityErrorCode, CapacityFreshness, CapacityMeasure,
    CapacityReport, CapacityReset, CapacityScope, CapacitySource, CapacityUnavailableReason,
    CapacityUnsupportedReason, CapacityWindow, ClaudeLimitType, CodexLimitSlot,
    PercentValueProvenance,
};

use crate::components::agents_panel::backend_label;
use crate::state::AppState;

// ── Formatting ──────────────────────────────────────────────────────────────

/// Absolute reset/observation times are authoritative on the wire (unix ms,
/// UTC). We render UTC rather than local time so the string a user reads is the
/// same one the server and the vendor mean, with no timezone ambiguity.
fn format_absolute_utc(ms: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    if date.get_time().is_nan() {
        return "invalid timestamp".to_owned();
    }
    let iso = String::from(date.to_iso_string());
    // "2026-07-14T18:20:31.000Z" -> "2026-07-14 18:20 UTC"
    match (iso.get(0..10), iso.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time} UTC"),
        _ => iso,
    }
}

/// Server-computed age (`CapacityFreshness.age_ms`) in words. The frontend never
/// diffs a timestamp against its own clock to decide how old a report is —
/// that would make desktop and mobile disagree, and would drift from the
/// server's staleness verdict.
fn format_age(age_ms: u64) -> String {
    let minutes = age_ms / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        "just now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else {
        format!("{days}d ago")
    }
}

fn format_duration_ms(ms: u64) -> String {
    let minutes = ms / 60_000;
    let hours = minutes / 60;
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{hours}h")
    }
}

/// Rolling-window duration. Both vendors report rolling windows in minutes.
fn format_window_minutes(minutes: u32) -> String {
    const DAY: u32 = 60 * 24;
    if minutes >= DAY && minutes.is_multiple_of(DAY) {
        format!("{}d", minutes / DAY)
    } else if minutes >= 60 && minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

/// Relative reset time, presentation only, derived from the server's absolute
/// value. A reset already in the past (clock skew, or a window that just
/// rolled) is stated plainly — never shown as a negative countdown, never
/// clamped away, never hidden.
fn format_reset_relative(at_ms: u64, now_ms: u64) -> String {
    if at_ms <= now_ms {
        return "reset time has passed".to_owned();
    }
    let remaining_ms = at_ms - now_ms;
    let minutes = remaining_ms / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        "resets in under a minute".to_owned()
    } else if minutes < 60 {
        format!("resets in {minutes}m")
    } else if hours < 24 {
        format!("resets in {}h {}m", hours, minutes % 60)
    } else {
        format!("resets in {}d {}h", days, hours % 24)
    }
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

// ── Vendor semantics ────────────────────────────────────────────────────────

fn source_label(source: CapacitySource) -> &'static str {
    match source {
        CapacitySource::ClaudeRateLimitEvent => "Claude",
        CapacitySource::CodexAccountRateLimitsUpdated => "Codex",
    }
}

/// Mandatory on every surface — never a tooltip. Without it, Claude's
/// single-bucket report and Codex's complete report render identically, and a
/// user looking at a healthy Claude row has no way to know a *different* Claude
/// limit sits at 98%.
fn coverage_text(coverage: CapacityCoverage, kind: BackendKind) -> String {
    let vendor = backend_label(kind);
    match coverage {
        CapacityCoverage::AllVendorBuckets => format!("All limits reported by {vendor}."),
        CapacityCoverage::RepresentativeBucketOnly => format!(
            "{vendor} reports only the limit that is currently binding. \
             Other limits exist and are not reported here."
        ),
    }
}

fn coverage_short_text(coverage: CapacityCoverage) -> Option<&'static str> {
    match coverage {
        CapacityCoverage::AllVendorBuckets => None,
        CapacityCoverage::RepresentativeBucketOnly => {
            Some("only the binding limit is reported \u{2014} see Settings")
        }
    }
}

fn scope_text(scope: &CapacityScope) -> String {
    match scope {
        CapacityScope::Account => "account".to_owned(),
        CapacityScope::Workspace => "workspace".to_owned(),
        CapacityScope::Individual => "individual".to_owned(),
        CapacityScope::ModelFamily { name } => format!("model family: {name}"),
        CapacityScope::OrganizationSpend => "organization spend".to_owned(),
        // Never guessed. "Not reported" is a real, common, correct answer.
        CapacityScope::NotReported => "scope not reported".to_owned(),
    }
}

fn window_text(window: &CapacityWindow) -> String {
    match window {
        CapacityWindow::Rolling { duration_minutes } => {
            format!(
                "rolling {} window",
                format_window_minutes(*duration_minutes)
            )
        }
        CapacityWindow::NotReported => "window not reported".to_owned(),
    }
}

/// A rolling window's start is unknown, so a missing reset is never synthesized
/// from the window duration.
fn reset_texts(reset: &CapacityReset) -> Option<(String, String)> {
    match reset {
        CapacityReset::At { at_ms } => Some((
            format_absolute_utc(*at_ms),
            format_reset_relative(*at_ms, now_ms()),
        )),
        CapacityReset::NotReported => None,
    }
}

fn bucket_status_text(status: CapacityBucketStatus) -> &'static str {
    match status {
        CapacityBucketStatus::Allowed => "allowed",
        CapacityBucketStatus::AllowedWarning => "approaching limit",
        CapacityBucketStatus::Rejected => "limit reached",
    }
}

fn bucket_status_slug(status: CapacityBucketStatus) -> &'static str {
    match status {
        CapacityBucketStatus::Allowed => "allowed",
        CapacityBucketStatus::AllowedWarning => "warning",
        CapacityBucketStatus::Rejected => "rejected",
    }
}

/// The vendor's own bucket type, spelled exactly as the vendor spells it.
///
/// This is not decoration. The server derives `bucket.label` from the vendor's
/// own naming rule, and that rule is **lossy**: Claude's `seven_day` and
/// `seven_day_overage_included` are two different limits that both label as
/// "weekly limit". Rendering the vendor type alongside the label is the only
/// thing that keeps them distinguishable. Never derived from `Debug` — that
/// would print `sevendayoverageincluded` and quietly invent a name the vendor
/// does not use.
fn bucket_vendor_id_text(id: &CapacityBucketId) -> &'static str {
    match id {
        CapacityBucketId::Codex { slot } => match slot {
            CodexLimitSlot::Primary => "codex primary",
            CodexLimitSlot::Secondary => "codex secondary",
            CodexLimitSlot::Credits => "codex credits",
        },
        CapacityBucketId::Claude { limit } => match limit {
            ClaudeLimitType::FiveHour => "claude five_hour",
            ClaudeLimitType::SevenDay => "claude seven_day",
            ClaudeLimitType::SevenDayOpus => "claude seven_day_opus",
            ClaudeLimitType::SevenDaySonnet => "claude seven_day_sonnet",
            ClaudeLimitType::SevenDayOverageIncluded => "claude seven_day_overage_included",
            ClaudeLimitType::Overage => "claude overage",
        },
    }
}

/// The bucket's display name. The vendor label is server-derived and is the
/// authority; if it is ever absent we fall back to the vendor's own bucket type
/// rather than inventing a name for it.
fn bucket_label_text(bucket: &CapacityBucket) -> String {
    if bucket.label.trim().is_empty() {
        bucket_vendor_id_text(&bucket.id).to_owned()
    } else {
        bucket.label.clone()
    }
}

// ── State semantics ─────────────────────────────────────────────────────────

/// A single short phrase naming the state, used as the row headline and in the
/// compact row. `Unsupported`, `Unavailable`, and `Stale` must never read as
/// "has capacity" — silence and empty bars both read as "fine".
fn state_headline(state: &BackendCapacityState, kind: BackendKind) -> String {
    let vendor = backend_label(kind);
    match state {
        BackendCapacityState::Known { report } => {
            format!("Reported by {}", source_label(report.source))
        }
        BackendCapacityState::Stale { .. } => "Stale \u{2014} last known report".to_owned(),
        BackendCapacityState::Unavailable { reason } => match reason {
            CapacityUnavailableReason::AwaitingFirstReport => "No report yet".to_owned(),
            CapacityUnavailableReason::MalformedReport => "Report could not be read".to_owned(),
            CapacityUnavailableReason::SourceUnreachable => "Source unreachable".to_owned(),
            CapacityUnavailableReason::SourceTimedOut => "Source timed out".to_owned(),
        },
        BackendCapacityState::Unsupported { .. } => {
            format!("{vendor} reports no capacity")
        }
        BackendCapacityState::AuthError { .. } => "Not authorized".to_owned(),
        BackendCapacityState::RateLimited { .. } => "Status source rate-limited".to_owned(),
    }
}

/// What the state means for the user's decision — the same idiom as the
/// existing `task_usage_unavailable_text`. Every non-`Known` state explains
/// itself; none of them is allowed to look like healthy capacity.
fn state_explanation(state: &BackendCapacityState, kind: BackendKind) -> Option<String> {
    let vendor = backend_label(kind);
    match state {
        BackendCapacityState::Known { .. } => None,
        BackendCapacityState::Stale { .. } => Some(format!(
            "{vendor} reports capacity passively, so this figure ages while the account is idle. \
             Run a turn to refresh it."
        )),
        BackendCapacityState::Unavailable { reason } => Some(match reason {
            CapacityUnavailableReason::AwaitingFirstReport => format!(
                "No report from {vendor} yet. It reports capacity passively, only after a turn \
                 completes. This is not zero usage \u{2014} nothing has been reported."
            ),
            // Covers both a vendor payload that failed validation and a Codex
            // notification that arrived without the complete snapshot: the
            // adapter discards it whole rather than publishing a partial one.
            CapacityUnavailableReason::MalformedReport => format!(
                "The last {vendor} report failed validation, so no figure is shown rather than a \
                 guessed one. The values are not partially trusted."
            ),
            CapacityUnavailableReason::SourceUnreachable => {
                format!("The {vendor} status source could not be reached.")
            }
            CapacityUnavailableReason::SourceTimedOut => {
                format!("The {vendor} status source did not respond in time.")
            }
        }),
        BackendCapacityState::Unsupported { reason } => Some(match reason {
            CapacityUnsupportedReason::BackendHasNoCapacitySource => {
                format!("{vendor} exposes no capacity source, so no quota can be shown.")
            }
            CapacityUnsupportedReason::BackendVersionTooOld => format!(
                "The installed {vendor} version does not report capacity. Update it to see quota."
            ),
            CapacityUnsupportedReason::AccountTypeNotReported => format!(
                "This {vendor} account does not report subscription quota (for example, API-key \
                 auth rather than a subscription)."
            ),
        }),
        BackendCapacityState::AuthError { detail } => Some(format!(
            "{} ({})",
            detail.summary,
            error_code_text(detail.code)
        )),
        BackendCapacityState::RateLimited {
            detail,
            retry_at_ms,
        } => {
            let mut text = format!("{} ({})", detail.summary, error_code_text(detail.code));
            if let Some(retry_at_ms) = retry_at_ms {
                text.push_str(&format!(
                    ". Retry after {} ({})",
                    format_absolute_utc(*retry_at_ms),
                    format_reset_relative(*retry_at_ms, now_ms()),
                ));
            }
            Some(text)
        }
    }
}

fn error_code_text(code: CapacityErrorCode) -> &'static str {
    match code {
        CapacityErrorCode::NotAuthenticated => "not authenticated",
        CapacityErrorCode::SourceRejected => "source rejected the request",
        CapacityErrorCode::RateLimited => "rate limited",
        CapacityErrorCode::MalformedResponse => "malformed response",
    }
}

fn state_slug(state: &BackendCapacityState) -> &'static str {
    match state {
        BackendCapacityState::Known { .. } => "known",
        BackendCapacityState::Stale { .. } => "stale",
        BackendCapacityState::Unavailable { .. } => "unavailable",
        BackendCapacityState::Unsupported { .. } => "unsupported",
        BackendCapacityState::AuthError { .. } => "auth-error",
        BackendCapacityState::RateLimited { .. } => "rate-limited",
    }
}

/// The report carried by `Known` and `Stale`. Every other state has none — and
/// must render as text, never as an empty bar.
fn state_report(state: &BackendCapacityState) -> Option<&CapacityReport> {
    match state {
        BackendCapacityState::Known { report } => Some(report),
        BackendCapacityState::Stale { report, .. } => Some(report),
        BackendCapacityState::Unavailable { .. }
        | BackendCapacityState::Unsupported { .. }
        | BackendCapacityState::AuthError { .. }
        | BackendCapacityState::RateLimited { .. } => None,
    }
}

/// Server-computed freshness, rendered verbatim. The client never re-derives it.
fn freshness_text(freshness: &CapacityFreshness) -> String {
    match freshness {
        CapacityFreshness::Fresh { age_ms } => format!("reported {}", format_age(*age_ms)),
        CapacityFreshness::Stale {
            age_ms,
            threshold_ms,
        } => format!(
            "reported {} \u{b7} past the {} freshness threshold",
            format_age(*age_ms),
            format_duration_ms(*threshold_ms),
        ),
    }
}

/// The bucket that owns the compact row's single progress bar: the most
/// constrained window the vendor reported a magnitude for. If a daily window is
/// fine but a weekly one is nearly exhausted, the weekly one is what will stop
/// your work, so it is the one that gets the bar. Where the vendor reports only
/// its own binding limit (Claude), that is the single bucket and this picks it
/// by construction. Credits and magnitude-less buckets are never eligible —
/// they are not percentages.
///
/// Returns the bucket with its used and remaining figures, so no caller has to
/// re-match the measure and invent a value for the arms that cannot occur.
///
/// Ties resolve to the later bucket in the vendor's own ordering. For Codex that
/// is `secondary` (the longer window), which is the right answer: two windows at
/// the same utilization are not equally constraining, and the longer one takes
/// longer to recover.
fn authoritative_bucket(report: &CapacityReport) -> Option<(&CapacityBucket, u8, u8)> {
    report
        .buckets
        .iter()
        .filter_map(|bucket| match &bucket.measure {
            CapacityMeasure::UsedPercent {
                used_percent,
                remaining_percent,
                ..
            } => Some((bucket, *used_percent, *remaining_percent)),
            CapacityMeasure::Credits { .. } | CapacityMeasure::ReportedWithoutMagnitude => None,
        })
        .max_by_key(|(_, used_percent, _)| *used_percent)
}

/// Provenance is **per value**, and the protocol says which is which.
///
/// The UI never inspects `ValueProvenance.vendor_reported` itself. It asks
/// `CapacityMeasure::used_percent_provenance()` and
/// `CapacityMeasure::remaining_percent_provenance()`, because those two answer
/// different questions and the raw flag only answers the first:
///
/// * `used_percent` is the vendor's magnitude, and the flag describes *it*.
///   Claude reports a 0..1 fraction that the adapter multiplies by 100 — a unit
///   conversion, not a derivation — so both adapters report `VendorReported`.
/// * `remaining_percent` is **always** `DerivedComplement`. Tyde computes
///   `100 - used`; the vendor never supplies it. Reading `vendor_reported` as if
///   it said something about the complement would attribute Tyde's arithmetic to
///   the vendor, which is the mirror image of the older bug that captioned the
///   vendor's own percentage as derived.
fn provenance_text(provenance: PercentValueProvenance) -> &'static str {
    match provenance {
        PercentValueProvenance::VendorReported => "vendor reported",
        PercentValueProvenance::DerivedFromVendorTotals => "derived from vendor totals",
        PercentValueProvenance::DerivedComplement => "derived (100 \u{2212} used)",
    }
}

/// The full accessible sentence for a percentage bucket. The bar itself is
/// decorative; this text is the source of truth. It carries the vendor's own
/// bucket type (labels are lossy — see `bucket_vendor_id_text`), both figures
/// with their own provenance, and the absolute reset time rather than only a
/// relative one.
fn bucket_aria_label(bucket: &CapacityBucket, used_percent: u8, remaining_percent: u8) -> String {
    let mut parts = vec![
        bucket_label_text(bucket),
        bucket_vendor_id_text(&bucket.id).to_owned(),
    ];
    parts.push(match bucket.measure.used_percent_provenance() {
        Some(provenance) => format!(
            "{used_percent} percent used, {}",
            provenance_text(provenance)
        ),
        None => format!("{used_percent} percent used"),
    });
    parts.push(match bucket.measure.remaining_percent_provenance() {
        Some(provenance) => format!(
            "{remaining_percent} percent remaining, {}",
            provenance_text(provenance)
        ),
        None => format!("{remaining_percent} percent remaining"),
    });
    match &bucket.reset {
        CapacityReset::At { at_ms } => {
            parts.push(format!("resets {}", format_absolute_utc(*at_ms)));
        }
        CapacityReset::NotReported => parts.push("reset time not reported".to_owned()),
    }
    if let Some(status) = bucket.status {
        parts.push(bucket_status_text(status).to_owned());
    }
    parts.join(", ")
}

// ── Bucket rendering ────────────────────────────────────────────────────────

/// A percentage bar. Drawn only for a vendor-reported `used_percent` on a
/// `Known`/`Stale` report — never for credits, a magnitude-less bucket, or a
/// state with no report at all.
fn percent_bar(bucket: &CapacityBucket, used_percent: u8, remaining_percent: u8) -> AnyView {
    let aria = bucket_aria_label(bucket, used_percent, remaining_percent);
    let width = used_percent.min(100);
    view! {
        <div class="capacity-bar" role="img" aria-label=aria>
            <div
                class="capacity-bar-fill"
                style=format!("width: {width}%")
            ></div>
        </div>
    }
    .into_any()
}

/// Used/remaining figures render only when the vendor reported a magnitude.
/// Credits are text (a balance is not a percentage), and a bucket the vendor
/// acknowledged without a number says exactly that.
///
/// Each figure carries its **own** provenance. The used percentage is the
/// vendor's; the remaining percentage is Tyde's complement unless the vendor
/// gave it. A single caption spanning both would tar the vendor's own number as
/// derived.
fn measure_view(bucket: &CapacityBucket, with_bar: bool) -> AnyView {
    match &bucket.measure {
        CapacityMeasure::UsedPercent {
            used_percent,
            remaining_percent,
            ..
        } => {
            let used = *used_percent;
            let remaining = *remaining_percent;
            // Two protocol helpers, two different questions. The used figure's
            // provenance is the vendor's claim; the remaining figure's is always
            // `DerivedComplement`. Neither is inferred from the raw flag here.
            let used_provenance = bucket
                .measure
                .used_percent_provenance()
                .map(provenance_text);
            let remaining_provenance = bucket
                .measure
                .remaining_percent_provenance()
                .map(provenance_text);
            view! {
                <div class="capacity-measure">
                    {with_bar.then(|| percent_bar(bucket, used, remaining))}
                    <span class="capacity-figures">
                        <span class="capacity-figure capacity-figure-used">
                            <span class="capacity-used">{format!("{used}% used")}</span>
                            {used_provenance.map(|text| view! {
                                <span class="capacity-provenance capacity-provenance-vendor">
                                    {text}
                                </span>
                            })}
                        </span>
                        <span class="capacity-figure capacity-figure-remaining">
                            <span class="capacity-remaining">
                                {format!("{remaining}% remaining")}
                            </span>
                            {remaining_provenance.map(|text| view! {
                                <span class="capacity-provenance capacity-provenance-remaining">
                                    {text}
                                </span>
                            })}
                        </span>
                    </span>
                </div>
            }
            .into_any()
        }
        CapacityMeasure::Credits {
            has_credits,
            unlimited,
            balance,
        } => {
            let text = if *unlimited {
                "unlimited credits".to_owned()
            } else {
                match balance {
                    Some(balance) => format!("credit balance: {balance}"),
                    None if *has_credits => "credits available, balance not reported".to_owned(),
                    None => "no credits".to_owned(),
                }
            };
            view! {
                <div class="capacity-measure capacity-measure-credits">
                    <span class="capacity-credits">{text}</span>
                </div>
            }
            .into_any()
        }
        CapacityMeasure::ReportedWithoutMagnitude => view! {
            <div class="capacity-measure capacity-measure-magnitudeless">
                <span class="capacity-no-magnitude">
                    "limit reported without an amount"
                </span>
            </div>
        }
        .into_any(),
    }
}

/// One row per vendor bucket, with unit-appropriate rendering. The vendor's own
/// bucket *type* is rendered next to the label because the label alone is
/// ambiguous: Claude's `seven_day` and `seven_day_overage_included` both label
/// as "weekly limit", and a Codex `primary` must never read as a Claude
/// `five_hour`.
fn bucket_row(bucket: &CapacityBucket) -> AnyView {
    let has_percent = matches!(bucket.measure, CapacityMeasure::UsedPercent { .. });
    let reset = match reset_texts(&bucket.reset) {
        Some((absolute, relative)) => format!("resets {absolute} \u{b7} {relative}"),
        None => "reset not reported".to_owned(),
    };
    let scope = scope_text(&bucket.scope);
    let window = window_text(&bucket.window);
    let vendor_id = bucket_vendor_id_text(&bucket.id);
    let label = bucket_label_text(bucket);
    let status = bucket.status;
    view! {
        <div class="capacity-bucket" data-capacity-bucket=vendor_id>
            <div class="capacity-bucket-head">
                <span class="capacity-bucket-label">{label}</span>
                {status.map(|status| view! {
                    <span
                        class="capacity-bucket-status"
                        data-capacity-status=bucket_status_slug(status)
                    >
                        {bucket_status_text(status)}
                    </span>
                })}
            </div>
            {measure_view(bucket, has_percent)}
            <div class="capacity-bucket-meta">
                <span class="capacity-meta-item">{window}</span>
                <span class="capacity-meta-item">{scope}</span>
                <span class="capacity-meta-item">{reset}</span>
                <span class="capacity-meta-item capacity-meta-vendor">{vendor_id}</span>
            </div>
        </div>
    }
    .into_any()
}

// ── Settings: the full authoritative view ───────────────────────────────────

fn plan_text(report: &CapacityReport) -> String {
    match &report.plan {
        Some(plan) => format!("plan: {}", plan.label),
        // Not blank, not guessed. Claude's plan lives in a secret-bearing
        // credentials file Tyde does not open, so it is reported as absent.
        None => "plan not reported by this source".to_owned(),
    }
}

fn snapshot_card(snapshot: &BackendCapacitySnapshot) -> AnyView {
    let kind = snapshot.backend_kind;
    let state = &snapshot.state;
    let headline = state_headline(state, kind);
    let explanation = state_explanation(state, kind);
    let report = state_report(state);
    let freshness = freshness_text(&snapshot.freshness);
    let retrieved = format_absolute_utc(snapshot.retrieved_at_ms);
    let slug = state_slug(state);

    view! {
        <div
            class="capacity-card"
            data-capacity-backend=format!("{kind:?}").to_lowercase()
            data-capacity-state=slug
        >
            <div class="capacity-card-head">
                <span class="capacity-backend">{backend_label(kind)}</span>
                <span class="capacity-state" data-capacity-state=slug>{headline}</span>
            </div>

            <div class="capacity-card-meta">
                <span class="capacity-meta-item">{freshness}</span>
                <span class="capacity-meta-item">{format!("received {retrieved}")}</span>
                {report.map(|report| view! {
                    <span class="capacity-meta-item">
                        {format!("source: {}", source_label(report.source))}
                    </span>
                })}
                {report.map(|report| view! {
                    <span class="capacity-meta-item">{plan_text(report)}</span>
                })}
                {report.and_then(|report| report.observed_at_ms).map(|observed_at_ms| view! {
                    <span class="capacity-meta-item">
                        {format!("vendor observed {}", format_absolute_utc(observed_at_ms))}
                    </span>
                })}
            </div>

            {explanation.map(|text| view! {
                <p class="capacity-explanation">{text}</p>
            })}

            {report.map(|report| {
                let coverage = coverage_text(report.coverage, kind);
                let buckets = report.buckets.clone();
                view! {
                    <>
                        <p class="capacity-coverage">{coverage}</p>
                        <div class="capacity-buckets">
                            {buckets.iter().map(bucket_row).collect_view()}
                        </div>
                    </>
                }
            })}
        </div>
    }
    .into_any()
}

/// The full authoritative Settings view: every backend the selected host
/// reports, including the ones with no capacity source at all. `Unsupported` is
/// a rendered row, not a hidden one — a missing row reads as "fine".
#[component]
pub fn SubscriptionCapacitySection() -> impl IntoView {
    let state = expect_context::<AppState>();

    let snapshots = Memo::new(move |_| -> Vec<BackendCapacitySnapshot> {
        let Some(host_id) = state.selected_host_id.get() else {
            return Vec::new();
        };
        let mut snapshots: Vec<BackendCapacitySnapshot> = state
            .backend_capacity
            .get()
            .get(&host_id)
            .map(|by_kind| by_kind.values().cloned().collect())
            .unwrap_or_default();
        // Stable order so a fresh snapshot for one backend never reshuffles the
        // list under the reader's cursor.
        snapshots.sort_by_key(|snapshot| format!("{:?}", snapshot.backend_kind));
        snapshots
    });

    view! {
        <div class="settings-field settings-capacity">
            <h3 class="settings-section-title">"Subscription capacity"</h3>
            <p class="settings-description">
                "Quota reported by each backend for the account it is signed in to on the selected \
                 host. This is advisory only \u{2014} Tyde never reroutes, downgrades, or switches \
                 backends based on it. Both sources report passively, as a side effect of turns \
                 you already ran, so there is nothing to refresh and no figure is ever inferred \
                 from Tyde's own token usage."
            </p>
            {move || {
                let snapshots = snapshots.get();
                if snapshots.is_empty() {
                    return view! {
                        <p class="settings-description capacity-empty">
                            "No backend on the selected host has reported capacity state yet."
                        </p>
                    }
                    .into_any();
                }
                view! {
                    <div class="capacity-cards">
                        {snapshots.iter().map(snapshot_card).collect_view()}
                    </div>
                }
                .into_any()
            }}
        </div>
    }
}

// ── Token-usage popup: the compact row ──────────────────────────────────────

/// A compact capacity row for one backend, rendered inside the task token-usage
/// popup under its own heading.
///
/// It is deliberately a sibling *region* of the task rollup, not a line in it:
/// task tokens measure this task, subscription capacity measures your account,
/// and the two are not summable. The heading, the labelled region, and the
/// accessible text all keep them apart so the layout never invites the
/// arithmetic.
///
/// Exactly one bucket owns the bar (§`authoritative_bucket`); the rest collapse
/// to a `+N more` pointer at Settings. States with no report render as text —
/// an empty bar would read as "0% used", which is precisely the lie this
/// feature exists to avoid.
#[component]
pub fn CapacityCompactRow(binding: Memo<Option<(String, BackendKind)>>) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Look the snapshot up reactively from the (host, backend) identity rather
    // than snapshotting it into a prop: the popover stays mounted across server
    // updates, and a baked-in value would freeze on screen.
    let snapshot = Memo::new(move |_| -> Option<BackendCapacitySnapshot> {
        let (host_id, backend_kind) = binding.get()?;
        state
            .backend_capacity
            .get()
            .get(&host_id)
            .and_then(|by_kind| by_kind.get(&backend_kind))
            .cloned()
    });

    view! {
        {move || {
            let snapshot = snapshot.get()?;
            let kind = snapshot.backend_kind;
            let vendor = backend_label(kind);
            let state_ref = &snapshot.state;
            let slug = state_slug(state_ref);
            let freshness = freshness_text(&snapshot.freshness);
            let report = state_report(state_ref);

            let body = match report {
                Some(report) => {
                    let coverage_note = coverage_short_text(report.coverage);
                    // `Stale` carries its last known report: a stale number with
                    // an explicit stale marker beats no number, provided the UI
                    // says so — and it does, in the state line and the freshness
                    // line, both of which are text.
                    match authoritative_bucket(report) {
                        Some((bucket, used_percent, remaining)) => {
                            let other_count = report.buckets.len().saturating_sub(1);
                            let more = (other_count > 0)
                                .then(|| format!("+{other_count} more in Settings"));
                            let reset = reset_texts(&bucket.reset);
                            // Label + vendor type: "weekly limit" alone cannot
                            // tell seven_day from seven_day_overage_included.
                            let label = bucket_label_text(bucket);
                            let vendor_id = bucket_vendor_id_text(&bucket.id);
                            view! {
                                <>
                                    <div class="capacity-compact-bucket">
                                        <span class="capacity-compact-label">{label}</span>
                                        <span class="capacity-compact-vendor">{vendor_id}</span>
                                        {percent_bar(bucket, used_percent, remaining)}
                                        <span class="capacity-compact-figures">
                                            {format!("{used_percent}% used")}
                                        </span>
                                        {reset.map(|(absolute, relative)| view! {
                                            <span class="capacity-compact-reset" title=absolute>
                                                {relative}
                                            </span>
                                        })}
                                    </div>
                                    {more.map(|text| view! {
                                        <span class="capacity-compact-more">{text}</span>
                                    })}
                                    {coverage_note.map(|note| view! {
                                        <span class="capacity-compact-coverage">
                                            {format!("\u{26a0} {note}")}
                                        </span>
                                    })}
                                </>
                            }
                            .into_any()
                        }
                        // A report whose buckets carry no percentage at all —
                        // Claude acknowledging a limit without a utilization, or
                        // a credits-only Codex report. Text, never a bar: an
                        // empty bar would read as "0% used".
                        None => {
                            let rows: Vec<_> = report
                                .buckets
                                .iter()
                                .map(|bucket| {
                                    let label = bucket_label_text(bucket);
                                    let vendor_id = bucket_vendor_id_text(&bucket.id);
                                    view! {
                                        <div class="capacity-compact-textual-row">
                                            <span class="capacity-compact-label">{label}</span>
                                            <span class="capacity-compact-vendor">{vendor_id}</span>
                                            {measure_view(bucket, false)}
                                        </div>
                                    }
                                })
                                .collect();
                            view! {
                                <>
                                    <div class="capacity-compact-textual">{rows}</div>
                                    {coverage_note.map(|note| view! {
                                        <span class="capacity-compact-coverage">
                                            {format!("\u{26a0} {note}")}
                                        </span>
                                    })}
                                </>
                            }
                            .into_any()
                        }
                    }
                }
                None => {
                    let headline = state_headline(state_ref, kind);
                    let explanation = state_explanation(state_ref, kind);
                    view! {
                        <>
                            <span class="capacity-compact-state">{headline}</span>
                            {explanation.map(|text| view! {
                                <span class="capacity-compact-explanation">{text}</span>
                            })}
                        </>
                    }
                    .into_any()
                }
            };

            Some(view! {
                <div
                    class="capacity-compact"
                    data-capacity-state=slug
                    role="group"
                    aria-label=format!("Subscription capacity reported by {vendor}")
                >
                    <div class="capacity-compact-title">
                        {format!("Subscription \u{b7} reported by {vendor}")}
                    </div>
                    {body}
                    <div class="capacity-compact-freshness">{freshness}</div>
                </div>
            })
        }}
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppState;
    use leptos::mount::mount_to;
    // `ClaudeLimitType`/`CodexLimitSlot` come in through `use super::*`.
    use protocol::{Envelope, FrameKind, StreamPath, ValueProvenance};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 700px; height: 600px;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn query(container: &HtmlElement, selector: &str) -> Option<HtmlElement> {
        container
            .query_selector(selector)
            .unwrap()
            .map(|el| el.dyn_into().unwrap())
    }

    fn text_of(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    /// True when the text carries a negative duration — a `-` immediately
    /// followed by digits and one of the duration units this UI actually renders
    /// (`m`, `h`, `d`), e.g. `-5m`, `-12h`, `-3d`. That is what a negative
    /// countdown would look like on screen.
    ///
    /// Deliberately narrow, because the obvious check is wrong: the rendered
    /// reset cell contains an ISO date, so searching for a bare `-` (or `-1`)
    /// matches the date's own separators. In `2026-07-13` every hyphen is
    /// followed by digits and then another hyphen or a space — never by a
    /// duration unit — so this predicate cannot mistake a date for a minus sign.
    fn contains_negative_duration(text: &str) -> bool {
        text.split('-').skip(1).any(|after_minus| {
            let digits = after_minus.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && matches!(after_minus.chars().nth(digits), Some('m' | 'h' | 'd'))
        })
    }

    fn count(container: &HtmlElement, selector: &str) -> u32 {
        container.query_selector_all(selector).unwrap().length()
    }

    fn state_with_host(host_id: &str) -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|map| {
            map.insert(host_id.to_owned(), StreamPath(format!("/host/{host_id}")));
        });
        state.connection_statuses.update(|map| {
            map.insert(
                host_id.to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        crate::dispatch::prime_host_for_tests(&state, host_id);
        state
    }

    /// Capacity reaches the UI only through the server's `BackendCapacity`
    /// frame — the same path a real host uses. No test writes the signal
    /// directly, so these tests also cover the dispatch wiring.
    fn dispatch_capacity(
        state: &AppState,
        host_id: &str,
        seq: u64,
        snapshots: Vec<BackendCapacitySnapshot>,
    ) {
        let envelope = Envelope::from_payload(
            StreamPath(format!("/host/{host_id}")),
            FrameKind::BackendCapacity,
            seq,
            &protocol::BackendCapacityPayload { snapshots },
        )
        .expect("envelope serialize");
        crate::dispatch::dispatch_envelope(state, host_id, envelope);
    }

    // ── Fixtures ────────────────────────────────────────────────────────────
    //
    // Every fixture mirrors what the server's adapters can *actually* emit. A
    // fixture that invents a value the server never produces tests a UI that
    // will never exist, and it hides the shapes that do. From `claude.rs`
    // `map_passive_rate_limit_event`, `codex.rs`
    // `map_passive_rate_limits_updated`, and `host.rs`
    // `backend_capacity_snapshots`:
    //
    //   * `provenance.vendor_reported: true` is the ONLY `UsedPercent` shape
    //     either adapter produces. The used percentage is the vendor's own
    //     magnitude (Claude's 0..1 fraction is unit-converted, not derived), so
    //     `used_percent_provenance()` is `VendorReported`. The remaining
    //     percentage is `DerivedComplement` regardless — Tyde computes it, and
    //     the flag says nothing about it.
    //   * Claude: `scope` and `window` are ALWAYS `NotReported`; `status` is
    //     ALWAYS `Some`; `plan` is ALWAYS `None`. Labels are the vendor rule and
    //     are all distinct: "session limit" / "weekly limit" / "Fable 5 limit" /
    //     "Opus limit" / "Sonnet limit" / "overage limit".
    //   * Codex: window labels are `"{limitName} primary limit"` /
    //     `"{limitName} secondary limit"`; credits are labelled "credits".
    //     `status` is ALWAYS `None`. Window buckets are scoped `Individual` when
    //     `individualLimit` is true and `Account` otherwise; the credits bucket
    //     takes its scope from `rateLimitReachedType`. Only the window buckets
    //     carry a rolling window and a reset.
    //   * Freshness is recomputed on emit from `retrieved_at_ms`, so `Fresh`
    //     carries a real, nonzero `age_ms` for a late-joining client, and a
    //     `Known` report older than the 60-minute threshold is emitted as
    //     `Stale`.

    /// The server's freshness threshold, from `host.rs`.
    const FRESHNESS_THRESHOLD_MS: u64 = 60 * 60 * 1000;

    /// A snapshot the server just recorded.
    fn fresh() -> CapacityFreshness {
        CapacityFreshness::Fresh { age_ms: 0 }
    }

    fn future_reset() -> CapacityReset {
        CapacityReset::At {
            at_ms: now_ms() + 2 * 24 * 3_600_000 + 4 * 3_600_000,
        }
    }

    /// A Claude bucket exactly as `map_passive_rate_limit_event` builds one.
    fn claude_bucket(
        limit: ClaudeLimitType,
        label: &str,
        measure: CapacityMeasure,
        reset: CapacityReset,
    ) -> CapacityBucket {
        CapacityBucket {
            id: CapacityBucketId::Claude { limit },
            label: label.to_owned(),
            measure,
            // Claude reports neither, ever.
            scope: CapacityScope::NotReported,
            window: CapacityWindow::NotReported,
            reset,
            // Claude always carries a vendor status.
            status: Some(CapacityBucketStatus::AllowedWarning),
        }
    }

    /// The only `UsedPercent` shape either adapter emits: the vendor supplied
    /// the used magnitude, and Tyde computed the complement.
    fn used_percent(used: u8) -> CapacityMeasure {
        CapacityMeasure::UsedPercent {
            used_percent: used,
            remaining_percent: 100 - used,
            provenance: ValueProvenance {
                vendor_reported: true,
            },
        }
    }

    /// Claude: one representative bucket, `RepresentativeBucketOnly` coverage,
    /// no plan label (the source does not report one).
    fn claude_known() -> BackendCapacitySnapshot {
        claude_snapshot(claude_bucket(
            ClaudeLimitType::SevenDay,
            "weekly limit",
            used_percent(82),
            future_reset(),
        ))
    }

    fn claude_snapshot(bucket: CapacityBucket) -> BackendCapacitySnapshot {
        BackendCapacitySnapshot {
            backend_kind: BackendKind::Claude,
            state: BackendCapacityState::Known {
                report: CapacityReport {
                    source: CapacitySource::ClaudeRateLimitEvent,
                    observed_at_ms: None,
                    plan: None,
                    buckets: vec![bucket],
                    coverage: CapacityCoverage::RepresentativeBucketOnly,
                },
            },
            retrieved_at_ms: now_ms(),
            freshness: fresh(),
        }
    }

    /// A Codex window bucket as `map_passive_rate_limits_updated` builds one:
    /// the vendor's `limitName` prefixes the slot label, rolling window, no
    /// status, `Individual` scope (this fixture's `individualLimit` is true).
    fn codex_window_bucket(
        slot: CodexLimitSlot,
        label: &str,
        used: u8,
        minutes: u32,
    ) -> CapacityBucket {
        CapacityBucket {
            id: CapacityBucketId::Codex { slot },
            label: label.to_owned(),
            measure: used_percent(used),
            scope: CapacityScope::Individual,
            window: CapacityWindow::Rolling {
                duration_minutes: minutes,
            },
            reset: future_reset(),
            status: None,
        }
    }

    /// Codex credits, exactly as the adapter builds them: no window, no reset,
    /// no status, a scope taken from `rateLimitReachedType` (null here, so
    /// `NotReported`), and a balance that is an opaque sanitized vendor string.
    fn codex_credits_bucket() -> CapacityBucket {
        CapacityBucket {
            id: CapacityBucketId::Codex {
                slot: CodexLimitSlot::Credits,
            },
            label: "credits".to_owned(),
            measure: CapacityMeasure::Credits {
                has_credits: true,
                unlimited: false,
                balance: Some("12.5".to_owned()),
            },
            scope: CapacityScope::NotReported,
            window: CapacityWindow::NotReported,
            reset: CapacityReset::NotReported,
            status: None,
        }
    }

    /// Codex: all vendor buckets — two rolling windows plus credits, which are
    /// not a percentage. Labels carry the vendor's `limitName` prefix exactly as
    /// the adapter builds them (`"{limitName} primary limit"`).
    fn codex_known() -> BackendCapacitySnapshot {
        BackendCapacitySnapshot {
            backend_kind: BackendKind::Codex,
            state: BackendCapacityState::Known {
                report: CapacityReport {
                    source: CapacitySource::CodexAccountRateLimitsUpdated,
                    observed_at_ms: None,
                    plan: Some(protocol::CapacityPlanLabel {
                        label: "pro".to_owned(),
                    }),
                    buckets: vec![
                        codex_window_bucket(
                            CodexLimitSlot::Primary,
                            "subscription primary limit",
                            30,
                            300,
                        ),
                        codex_window_bucket(
                            CodexLimitSlot::Secondary,
                            "subscription secondary limit",
                            91,
                            10_080,
                        ),
                        codex_credits_bucket(),
                    ],
                    coverage: CapacityCoverage::AllVendorBuckets,
                },
            },
            retrieved_at_ms: now_ms(),
            freshness: fresh(),
        }
    }

    fn snapshot_with_state(
        kind: BackendKind,
        state: BackendCapacityState,
        freshness: CapacityFreshness,
    ) -> BackendCapacitySnapshot {
        BackendCapacitySnapshot {
            backend_kind: kind,
            state,
            retrieved_at_ms: now_ms(),
            freshness,
        }
    }

    fn mount_settings(container: &HtmlElement, state: AppState) -> impl Sized {
        mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <SubscriptionCapacitySection /> }
        })
    }

    /// Settings renders one row per vendor bucket, with each bucket's own unit,
    /// scope and rolling window — never a merged figure across buckets.
    #[wasm_bindgen_test]
    async fn settings_renders_every_vendor_bucket() {
        let container = make_container();
        let state = state_with_host("h-cap-buckets");
        dispatch_capacity(&state, "h-cap-buckets", 0, vec![codex_known()]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        assert_eq!(
            count(&container, ".capacity-bucket"),
            3,
            "one row per vendor bucket (primary + secondary + credits)"
        );

        let text = text_of(&container);
        // The server's label carries the vendor's own `limitName` prefix; the
        // frontend renders it verbatim and invents nothing.
        assert!(
            text.contains("subscription primary limit")
                && text.contains("subscription secondary limit")
                && text.contains("credits"),
            "each vendor bucket keeps the server's own label, got: {text}"
        );
        // Only Codex's two windows are rolling; its credits bucket has none.
        assert!(
            text.contains("rolling 5h window") && text.contains("rolling 7d window"),
            "each window bucket states its own rolling window, got: {text}"
        );
        assert!(
            text.contains("window not reported"),
            "the credits bucket reports no window and must say so, got: {text}"
        );
        assert!(
            text.contains("plan: pro"),
            "a vendor-reported plan label must be shown, got: {text}"
        );
    }

    /// Claude's labels are all distinct — `seven_day` is "weekly limit" and
    /// `seven_day_overage_included` is "Fable 5 limit". The frontend hardcodes
    /// neither: it renders whatever label the server sends, and renders the
    /// vendor's own bucket *type* beside it. The type is the durable identity —
    /// it stays correct if the vendor's naming ever changes again, and it is what
    /// keeps a Codex `primary` from reading as a Claude `five_hour`.
    #[wasm_bindgen_test]
    async fn vendor_bucket_types_and_labels_are_both_rendered_verbatim() {
        let container = make_container();
        let state = state_with_host("h-cap-ids");
        let plain = claude_snapshot(claude_bucket(
            ClaudeLimitType::SevenDay,
            "weekly limit",
            used_percent(40),
            future_reset(),
        ));
        dispatch_capacity(&state, "h-cap-ids", 0, vec![plain]);
        let _handle = mount_settings(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        let text = text_of(&container);
        assert!(
            text.contains("weekly limit") && text.contains("claude seven_day"),
            "the vendor bucket type must be rendered beside the server's label, got: {text}"
        );
        assert!(
            !text.contains("seven_day_overage_included") && !text.contains("Fable"),
            "the plain weekly bucket must not borrow the overage-inclusive identity, got: {text}"
        );

        // The overage-inclusive limit is a different bucket, with the server's
        // own distinct label and its own vendor type.
        let overage_included = claude_snapshot(claude_bucket(
            ClaudeLimitType::SevenDayOverageIncluded,
            "Fable 5 limit",
            used_percent(40),
            future_reset(),
        ));
        dispatch_capacity(&state, "h-cap-ids", 1, vec![overage_included]);
        for _ in 0..4 {
            next_tick().await;
        }
        let text = text_of(&container);
        assert!(
            text.contains("Fable 5 limit") && text.contains("claude seven_day_overage_included"),
            "the overage-inclusive bucket renders the server's label and its own type, got: {text}"
        );
        assert!(
            !text.contains("weekly limit"),
            "the superseded bucket's label must be gone, got: {text}"
        );
        // The identity is in the accessible name too, not only the visible chip.
        let bar = query(&container, ".capacity-bar").expect("bar renders");
        let aria = bar.get_attribute("aria-label").unwrap_or_default();
        assert!(
            aria.contains("Fable 5 limit") && aria.contains("claude seven_day_overage_included"),
            "the accessible name must carry both the label and the vendor type, got: {aria}"
        );
    }

    /// Provenance is per value, and the protocol decides which is which. The UI
    /// asks `used_percent_provenance()` and `remaining_percent_provenance()` —
    /// two different questions — rather than reading `vendor_reported` twice.
    ///
    /// Both adapters send `vendor_reported: true`, so the used figure is
    /// `VendorReported`. The remaining figure is `DerivedComplement` **always**:
    /// Tyde computes `100 - used`, and the flag says nothing about it.
    /// Reinterpreting the flag as the remaining figure's provenance would
    /// attribute Tyde's arithmetic to the vendor.
    #[wasm_bindgen_test]
    async fn used_and_remaining_carry_their_own_provenance() {
        let container = make_container();
        let state = state_with_host("h-cap-prov");
        dispatch_capacity(&state, "h-cap-prov", 0, vec![claude_known()]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let used = query(&container, ".capacity-figure-used").expect("used figure renders");
        let remaining =
            query(&container, ".capacity-figure-remaining").expect("remaining figure renders");
        let used_text = used.text_content().unwrap_or_default();
        let remaining_text = remaining.text_content().unwrap_or_default();

        assert!(
            used_text.contains("82% used") && used_text.contains("vendor reported"),
            "the used figure is the vendor's own number and must say so, got: {used_text}"
        );
        assert!(
            !used_text.contains("derived"),
            "the vendor's percentage must never be captioned as derived, got: {used_text}"
        );
        assert!(
            remaining_text.contains("18% remaining")
                && remaining_text.contains("derived (100 \u{2212} used)"),
            "the remaining figure is Tyde's complement and must say so, got: {remaining_text}"
        );
        assert!(
            !remaining_text.contains("vendor reported"),
            "the remaining complement must never be attributed to the vendor \u{2014} \
             `vendor_reported` describes the used value only, got: {remaining_text}"
        );

        // The same split is in the accessible name, not only the visible text.
        let bar = query(&container, ".capacity-bar").expect("bar renders");
        let aria = bar.get_attribute("aria-label").unwrap_or_default();
        assert!(
            aria.contains("82 percent used, vendor reported"),
            "accessible name must attribute the used figure to the vendor, got: {aria}"
        );
        assert!(
            aria.contains("18 percent remaining, derived (100 \u{2212} used)"),
            "accessible name must attribute the remaining figure to Tyde's complement, got: {aria}"
        );
    }

    /// The unit scale on the wire is 0..=100. A bucket the server reports at 82
    /// renders as "82% used" / "18% remaining" and an 82%-wide bar — never
    /// 0.82, never 8200. This is the 100x regression guard on the presentation
    /// side of the Claude fraction / Codex percent conversion.
    #[wasm_bindgen_test]
    async fn percent_scale_renders_as_whole_percent() {
        let container = make_container();
        let state = state_with_host("h-cap-scale");
        dispatch_capacity(&state, "h-cap-scale", 0, vec![claude_known()]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);
        assert!(
            text.contains("82% used"),
            "used percent must render on the 0-100 scale, got: {text}"
        );
        assert!(
            text.contains("18% remaining"),
            "remaining percent must be shown alongside used, got: {text}"
        );
        assert!(
            !text.contains("0.82") && !text.contains("8200"),
            "the 0..1 fraction and a 100x-inflated value must never reach the DOM, got: {text}"
        );

        // The bar is decorative; its geometry still tracks the reported value.
        let bar = query(&container, ".capacity-bar").expect("percentage bucket draws a bar");
        let fill = query(&container, ".capacity-bar-fill").expect("bar has a fill");
        let bar_width = bar.get_bounding_client_rect().width();
        let fill_width = fill.get_bounding_client_rect().width();
        assert!(bar_width > 0.0, "bar must have layout width");
        let ratio = fill_width / bar_width;
        assert!(
            (ratio - 0.82).abs() < 0.02,
            "fill must span 82% of the bar, got ratio {ratio}"
        );

        // The text — not the color or the width — is the source of truth.
        let aria = bar.get_attribute("aria-label").unwrap_or_default();
        assert!(
            aria.contains("82 percent used") && aria.contains("18 percent remaining"),
            "accessible name must carry used and remaining, got: {aria}"
        );
        assert!(
            aria.contains("UTC"),
            "accessible name must carry the absolute reset time, not only a relative one, got: {aria}"
        );
        assert_eq!(bar.get_attribute("role").as_deref(), Some("img"));
    }

    /// Coverage is mandatory text on every surface, and the two vendors say
    /// different things: Codex reports all its buckets, Claude reports only the
    /// limit that currently binds. Without this, a healthy-looking Claude row
    /// hides the fact that another Claude limit could be at 98%.
    #[wasm_bindgen_test]
    async fn coverage_is_stated_per_vendor() {
        let container = make_container();
        let state = state_with_host("h-cap-coverage");
        dispatch_capacity(
            &state,
            "h-cap-coverage",
            0,
            vec![claude_known(), codex_known()],
        );
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);
        assert!(
            text.contains("reports only the limit that is currently binding"),
            "RepresentativeBucketOnly must be spelled out, got: {text}"
        );
        assert!(
            text.contains("Other limits exist and are not reported here"),
            "the caveat must say the other limits are unknown, not fine, got: {text}"
        );
        assert!(
            text.contains("All limits reported by Codex"),
            "AllVendorBuckets must be spelled out too, got: {text}"
        );
    }

    /// Credits are not a percentage and must never be rendered as a bar — an
    /// 82%-style bar on a credit balance would be meaningless.
    #[wasm_bindgen_test]
    async fn credits_bucket_has_no_bar() {
        let container = make_container();
        let state = state_with_host("h-cap-credits");
        let mut snapshot = codex_known();
        if let BackendCapacityState::Known { report } = &mut snapshot.state {
            // Keep only the credits bucket, so any bar in the DOM must be its own.
            report
                .buckets
                .retain(|bucket| matches!(bucket.measure, CapacityMeasure::Credits { .. }));
        }
        dispatch_capacity(&state, "h-cap-credits", 0, vec![snapshot]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        assert_eq!(
            count(&container, ".capacity-bucket"),
            1,
            "the credits bucket still renders as a row"
        );
        assert!(
            query(&container, ".capacity-bar").is_none(),
            "a credits bucket must not draw a progress bar"
        );
        let text = text_of(&container);
        assert!(
            text.contains("credit balance: 12.5"),
            "credits render as text, got: {text}"
        );
        assert!(
            !text.contains('%'),
            "credits must not be presented on a percentage scale, got: {text}"
        );
        assert!(
            !text.contains("vendor reported") && !text.contains("derived"),
            "a credits balance has no used/remaining provenance to report, got: {text}"
        );
    }

    /// Every non-`Known` state renders as explanatory text and never as a bar.
    /// An empty bar reads as "0% used" — the exact lie this feature avoids —
    /// and a missing row reads as "fine".
    ///
    /// The cases are exactly the no-report states the Phase-1 server can emit:
    /// `AwaitingFirstReport` (both adapters, including Codex receiving an
    /// incomplete notification), `MalformedReport` (both adapters on a failed
    /// validation), and `Unsupported` (the four backends with no source). The
    /// protocol also carries `AuthError` and `RateLimited`, and the UI renders
    /// them, but no Phase-1 code path produces them — they arrive with the gated
    /// Codex read — so there is no honest fixture for them yet.
    #[wasm_bindgen_test]
    async fn non_known_states_render_text_and_never_a_bar() {
        let cases: Vec<(BackendCapacityState, &str)> = vec![
            (
                BackendCapacityState::Unavailable {
                    reason: CapacityUnavailableReason::AwaitingFirstReport,
                },
                "nothing has been reported",
            ),
            (
                BackendCapacityState::Unavailable {
                    reason: CapacityUnavailableReason::MalformedReport,
                },
                "failed validation",
            ),
            (
                BackendCapacityState::Unsupported {
                    reason: CapacityUnsupportedReason::BackendHasNoCapacitySource,
                },
                "exposes no capacity source",
            ),
        ];

        for (index, (capacity_state, expected)) in cases.into_iter().enumerate() {
            let container = make_container();
            let host_id = format!("h-cap-state-{index}");
            let state = state_with_host(&host_id);
            dispatch_capacity(
                &state,
                &host_id,
                0,
                vec![snapshot_with_state(
                    BackendKind::Claude,
                    capacity_state,
                    fresh(),
                )],
            );
            let _handle = mount_settings(&container, state);
            for _ in 0..4 {
                next_tick().await;
            }

            let text = text_of(&container);
            assert!(
                text.contains(expected),
                "state must explain itself; expected {expected:?}, got: {text}"
            );
            assert!(
                query(&container, ".capacity-bar").is_none(),
                "a state with no report must never draw a bar (case {index})"
            );
            assert!(
                !text.contains('%'),
                "a state with no report must show no percentage (case {index}), got: {text}"
            );
            assert!(
                query(&container, ".capacity-card").is_some(),
                "the backend must still render a visible row (case {index})"
            );
        }
    }

    /// Freshness comes from the server's `CapacityFreshness` and nothing else.
    ///
    /// `host.rs::backend_capacity_snapshots` recomputes `age_ms` from
    /// `retrieved_at_ms` **on every emit**, so all three of these are shapes a
    /// real client receives:
    ///
    /// * just-recorded `Fresh { age_ms: 0 }` → "reported just now";
    /// * a **late-joining** client subscribing well after the last report →
    ///   `Fresh { age_ms: <real age> }`, which must render that real age and not
    ///   "just now" (the bug this fix closed);
    /// * a report past the 60-minute threshold → `Stale`, which keeps the last
    ///   known figure and says so.
    ///
    /// The frontend runs no clock against `retrieved_at_ms` to second-guess any
    /// of it; if it did, desktop and mobile would disagree about the same
    /// snapshot.
    #[wasm_bindgen_test]
    async fn freshness_is_rendered_from_the_servers_verdict_only() {
        // Just recorded.
        let container = make_container();
        let state = state_with_host("h-cap-fresh");
        dispatch_capacity(&state, "h-cap-fresh", 0, vec![claude_known()]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let text = text_of(&container);
        assert!(
            text.contains("reported just now"),
            "a just-recorded Fresh snapshot renders as 'just now', got: {text}"
        );
        assert!(
            !text.contains("Stale") && !text.contains("freshness threshold"),
            "a Fresh snapshot must not be marked stale, got: {text}"
        );

        // Late joiner: the server recomputes the age on emit, so a client that
        // subscribes 50 minutes after the last report is told 50 minutes — not
        // "just now". Rendering a real age here is only possible because the
        // server stopped shipping a frozen `age_ms: 0`.
        let container = make_container();
        let state = state_with_host("h-cap-latejoin");
        let mut late = claude_known();
        late.freshness = CapacityFreshness::Fresh {
            age_ms: 50 * 60 * 1000,
        };
        dispatch_capacity(&state, "h-cap-latejoin", 0, vec![late]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let text = text_of(&container);
        assert!(
            text.contains("reported 50m ago"),
            "a late-joining client must see the server's real age, got: {text}"
        );
        assert!(
            !text.contains("just now"),
            "an aged Fresh snapshot must never render as 'just now', got: {text}"
        );
        assert!(
            !text.contains("Stale"),
            "under the threshold it is still Fresh, not Stale, got: {text}"
        );
        assert!(
            text.contains("82% used"),
            "the figure is still shown while Fresh, got: {text}"
        );

        // Stale: the report survives, explicitly marked, with the server's own
        // age and threshold. This is the normal steady state for a passive
        // source — an idle account's figure simply ages.
        let container = make_container();
        let state = state_with_host("h-cap-stale");
        let BackendCapacityState::Known { report } = claude_known().state else {
            unreachable!("fixture is Known");
        };
        dispatch_capacity(
            &state,
            "h-cap-stale",
            0,
            vec![snapshot_with_state(
                BackendKind::Claude,
                BackendCapacityState::Stale {
                    report,
                    stale_since_ms: now_ms(),
                },
                CapacityFreshness::Stale {
                    age_ms: 2 * FRESHNESS_THRESHOLD_MS,
                    threshold_ms: FRESHNESS_THRESHOLD_MS,
                },
            )],
        );
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);
        assert!(
            text.contains("82% used"),
            "a stale report still carries its last known figure, got: {text}"
        );
        assert!(
            text.contains("Stale"),
            "the stale state must be named in text, got: {text}"
        );
        assert!(
            text.contains("2h ago"),
            "freshness must render the server's age, got: {text}"
        );
        assert!(
            text.contains("past the 1h freshness threshold"),
            "the server's own threshold must be stated, got: {text}"
        );
    }

    /// A reset already in the past is preserved and stated plainly — never a
    /// negative countdown, never clamped, never hidden.
    ///
    /// The previous form of the negative-countdown assertion rejected any `-1`
    /// anywhere in the rendered text. That mistook the ISO date's own separators
    /// for a minus sign: the cell renders as
    /// `resets 2026-07-13 07:11 UTC \u{b7} reset time has passed`, and `07-13`
    /// contains `-1`. It therefore failed on the 10th–19th of every month while
    /// the behaviour it guards was correct throughout — the production formatter
    /// returns "reset time has passed" for a past instant and does its arithmetic
    /// in `u64`, so a negative countdown is unrepresentable. The contract that
    /// assertion was reaching for is asserted directly below, and more tightly.
    #[wasm_bindgen_test]
    async fn past_reset_is_stated_not_counted_down() {
        let container = make_container();
        let state = state_with_host("h-cap-past");
        let reset_at_ms = now_ms().saturating_sub(60_000);
        let mut snapshot = claude_known();
        if let BackendCapacityState::Known { report } = &mut snapshot.state {
            report.buckets[0].reset = CapacityReset::At { at_ms: reset_at_ms };
        }
        dispatch_capacity(&state, "h-cap-past", 0, vec![snapshot]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);

        // The absolute reset instant is still shown, in UTC, and it is the right
        // one — not merely "some string containing UTC". Derived from the UTC
        // getters rather than from the component's own ISO slicing, so this
        // cross-checks the rendered value instead of restating how it is built.
        let reset_at = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(reset_at_ms as f64));
        let expected_utc = format!(
            "{:04}-{:02}-{:02} {:02}:{:02} UTC",
            reset_at.get_utc_full_year(),
            reset_at.get_utc_month() + 1,
            reset_at.get_utc_date(),
            reset_at.get_utc_hours(),
            reset_at.get_utc_minutes(),
        );
        assert!(
            text.contains(&expected_utc),
            "the absolute reset time must still be shown; expected {expected_utc:?}, got: {text}"
        );

        assert!(
            text.contains("reset time has passed"),
            "a past reset must be stated, got: {text}"
        );

        // A past reset renders no countdown at all, so neither a negative one nor
        // a nonsensical positive one can reach the user.
        assert!(
            !text.contains("resets in"),
            "a past reset must not render a countdown of any sign, got: {text}"
        );
        assert!(
            !contains_negative_duration(&text),
            "a past reset must never render a negative duration, got: {text}"
        );
    }

    /// Reset "not reported" is a real answer and is never synthesized from the
    /// rolling-window duration (a rolling window's start is unknown).
    #[wasm_bindgen_test]
    async fn missing_reset_is_reported_as_missing() {
        let container = make_container();
        let state = state_with_host("h-cap-noreset");
        let mut snapshot = claude_known();
        if let BackendCapacityState::Known { report } = &mut snapshot.state {
            report.buckets[0].reset = CapacityReset::NotReported;
        }
        dispatch_capacity(&state, "h-cap-noreset", 0, vec![snapshot]);
        let _handle = mount_settings(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);
        assert!(
            text.contains("reset not reported"),
            "a missing reset must be stated, got: {text}"
        );
        assert!(
            !text.contains("resets in"),
            "a missing reset must not be invented from the window, got: {text}"
        );
    }

    /// Capacity is a pure projection of server events: a second frame replaces
    /// the rendered figures with no client-side refresh action and no cache.
    #[wasm_bindgen_test]
    async fn a_later_frame_updates_the_rendered_figures() {
        let container = make_container();
        let state = state_with_host("h-cap-live");
        dispatch_capacity(&state, "h-cap-live", 0, vec![claude_known()]);
        let _handle = mount_settings(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(text_of(&container).contains("82% used"));

        let mut updated = claude_known();
        if let BackendCapacityState::Known { report } = &mut updated.state {
            report.buckets[0].measure = used_percent(95);
        }
        dispatch_capacity(&state, "h-cap-live", 1, vec![updated]);
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);
        assert!(
            text.contains("95% used") && text.contains("5% remaining"),
            "a server update must re-render the figures, got: {text}"
        );
        assert!(
            !text.contains("82% used"),
            "the superseded figure must be gone, got: {text}"
        );
        // There is nothing to refresh: both phase-1 sources are passive.
        assert!(
            query(&container, "button").is_none(),
            "phase 1 must not offer a refresh button"
        );
    }
}
