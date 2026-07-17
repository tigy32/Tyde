//! Subscription capacity on mobile web.
//!
//! Mobile consumes the *same* server-owned `BackendCapacity` snapshot as
//! desktop and renders the same meanings: the same six states, the same
//! mandatory coverage caveat, the same absolute timestamps, the same
//! vendor-native buckets. There is no mobile-only capacity model, no
//! mobile-only freshness maths, and no dropped error state — a state hidden on
//! a small screen reads as "fine", which is exactly the failure this feature
//! exists to prevent.
//!
//! The layout is stacked rather than a wide row (label above bar above meta) so
//! nothing clips at a phone width. That is the only difference from desktop.

use leptos::prelude::*;
use protocol::{
    BackendCapacitySnapshot, BackendCapacityState, BackendKind, CapacityBucket, CapacityBucketId,
    CapacityBucketStatus, CapacityCoverage, CapacityErrorCode, CapacityFreshness, CapacityMeasure,
    CapacityReport, CapacityReset, CapacityScope, CapacitySource, CapacityUnavailableReason,
    CapacityUnsupportedReason, CapacityWindow, ClaudeLimitType, CodexLimitSlot,
    PercentValueProvenance,
};

use crate::state::AppState;

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
        BackendKind::Kiro => "Kiro",
        BackendKind::Tycode => "Tycode",
    }
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// UTC, so the string a user reads is the one the server and the vendor mean.
fn format_absolute_utc(ms: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    if date.get_time().is_nan() {
        return "invalid timestamp".to_owned();
    }
    let iso = String::from(date.to_iso_string());
    match (iso.get(0..10), iso.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time} UTC"),
        _ => iso,
    }
}

/// Server-computed age. Mobile never diffs a timestamp against its own clock to
/// decide how old a report is — that is what would make mobile and desktop
/// disagree about the same snapshot.
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
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h", minutes / 60)
    }
}

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

/// A reset in the past is stated, never counted down negatively and never
/// hidden.
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

fn source_label(source: CapacitySource) -> &'static str {
    match source {
        CapacitySource::ClaudeRateLimitEvent => "Claude",
        CapacitySource::ClaudeControlUsage => "Claude",
        CapacitySource::CodexAccountRateLimitsUpdated => "Codex",
    }
}

/// Mandatory text on mobile too — a caveat that only exists on desktop is a
/// dropped error state.
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

fn scope_text(scope: &CapacityScope) -> String {
    match scope {
        CapacityScope::Account => "account".to_owned(),
        CapacityScope::Workspace => "workspace".to_owned(),
        CapacityScope::Individual => "individual".to_owned(),
        CapacityScope::ModelFamily { name } => format!("model family: {name}"),
        CapacityScope::OrganizationSpend => "organization spend".to_owned(),
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

fn reset_text(reset: &CapacityReset) -> String {
    match reset {
        CapacityReset::At { at_ms } => format!(
            "resets {} \u{b7} {}",
            format_absolute_utc(*at_ms),
            format_reset_relative(*at_ms, now_ms()),
        ),
        // Never synthesized from the window duration: a rolling window's start
        // is unknown.
        CapacityReset::NotReported => "reset not reported".to_owned(),
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

/// The vendor's own bucket type, spelled as the vendor spells it.
///
/// The server's label rule is lossy — Claude's `seven_day` and
/// `seven_day_overage_included` both label as "weekly limit" — so the type is
/// what keeps two different limits distinguishable. Never derived from `Debug`,
/// which would print `sevendayoverageincluded` and invent a name the vendor does
/// not use.
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
        CapacityBucketId::ClaudeModel { .. } => "claude model",
    }
}

/// The server's label is the authority; if it is ever absent we fall back to the
/// vendor's own bucket type rather than inventing a name.
fn bucket_label_text(bucket: &CapacityBucket) -> String {
    if bucket.label.trim().is_empty() {
        bucket_vendor_id_text(&bucket.id).to_owned()
    } else {
        bucket.label.clone()
    }
}

/// Provenance is per value, and the protocol says which is which. Mobile never
/// inspects `ValueProvenance.vendor_reported` itself: it asks
/// `used_percent_provenance()` and `remaining_percent_provenance()`, because
/// those answer different questions and the raw flag only answers the first.
/// `used` is the vendor's magnitude; `remaining` is **always**
/// `DerivedComplement`, since Tyde computes `100 - used`.
fn provenance_text(provenance: PercentValueProvenance) -> &'static str {
    match provenance {
        PercentValueProvenance::VendorReported => "vendor reported",
        PercentValueProvenance::DerivedFromVendorTotals => "derived from vendor totals",
        PercentValueProvenance::DerivedComplement => "derived (100 \u{2212} used)",
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
        BackendCapacityState::Unsupported { .. } => format!("{vendor} reports no capacity"),
        BackendCapacityState::AuthError { .. } => "Not authorized".to_owned(),
        BackendCapacityState::RateLimited { .. } => "Status source rate-limited".to_owned(),
    }
}

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
            CapacityUnsupportedReason::ExternalProvider => format!(
                "This {vendor} session uses an external provider. Capacity is managed by that \
                 provider rather than {vendor}."
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

fn plan_text(report: &CapacityReport) -> String {
    match &report.plan {
        Some(plan) => format!("plan: {}", plan.label),
        None => "plan not reported by this source".to_owned(),
    }
}

/// The one bucket that owns a bar: the most constrained window the vendor gave
/// a magnitude for. Credits and magnitude-less buckets are never eligible.
///
/// Returns the used and remaining figures with it, so no caller has to re-match
/// the measure and invent a value for the arms that cannot occur.
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

/// Decorative. The text beside it is the source of truth.
fn percent_bar(bucket: &CapacityBucket, used_percent: u8, remaining_percent: u8) -> AnyView {
    let aria = bucket_aria_label(bucket, used_percent, remaining_percent);
    let width = used_percent.min(100);
    view! {
        <div class="capacity-bar" role="img" aria-label=aria>
            <div class="capacity-bar-fill" style=format!("width: {width}%")></div>
        </div>
    }
    .into_any()
}

fn measure_view(bucket: &CapacityBucket) -> AnyView {
    match &bucket.measure {
        CapacityMeasure::UsedPercent {
            used_percent,
            remaining_percent,
            ..
        } => {
            let used = *used_percent;
            let remaining = *remaining_percent;
            // Two protocol helpers, two different questions. Neither figure's
            // provenance is inferred from the raw flag here.
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
                    {percent_bar(bucket, used, remaining)}
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
        // Not a percentage — text, never a bar.
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
                <span class="capacity-no-magnitude">"limit reported without an amount"</span>
            </div>
        }
        .into_any(),
    }
}

fn bucket_row(bucket: &CapacityBucket) -> AnyView {
    let vendor_id = bucket_vendor_id_text(&bucket.id);
    let label = bucket_label_text(bucket);
    let status = bucket.status;
    let window = window_text(&bucket.window);
    let scope = scope_text(&bucket.scope);
    let reset = reset_text(&bucket.reset);
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
            {measure_view(bucket)}
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

fn snapshot_card(snapshot: &BackendCapacitySnapshot) -> AnyView {
    let kind = snapshot.backend_kind;
    let state = &snapshot.state;
    let slug = state_slug(state);
    let headline = state_headline(state, kind);
    let explanation = state_explanation(state, kind);
    let report = state_report(state);
    let freshness = freshness_text(&snapshot.freshness);
    let retrieved = format_absolute_utc(snapshot.retrieved_at_ms);

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

/// The compact summary for one backend — mobile's parity with the desktop
/// token-usage popup row. Exactly one bucket owns the bar; states with no
/// report render as text, because an empty bar reads as "0% used".
///
/// It is explicitly labelled as *subscription* capacity so it can never be read
/// as task token usage, which is a different measurement of a different thing.
fn compact_row(snapshot: &BackendCapacitySnapshot) -> AnyView {
    let kind = snapshot.backend_kind;
    let vendor = backend_label(kind);
    let state = &snapshot.state;
    let slug = state_slug(state);
    let freshness = freshness_text(&snapshot.freshness);

    let body = match state_report(state) {
        Some(report) => {
            let coverage_note =
                matches!(report.coverage, CapacityCoverage::RepresentativeBucketOnly)
                    .then(|| "\u{26a0} only the binding limit is reported".to_owned());
            match authoritative_bucket(report) {
                Some((bucket, used_percent, remaining)) => {
                    let other_count = report.buckets.len().saturating_sub(1);
                    let more = (other_count > 0).then(|| format!("+{other_count} more below"));
                    // Label + vendor type: "weekly limit" alone cannot tell
                    // seven_day from seven_day_overage_included.
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
                            </div>
                            {more.map(|text| view! {
                                <span class="capacity-compact-more">{text}</span>
                            })}
                            {coverage_note.map(|note| view! {
                                <span class="capacity-compact-coverage">{note}</span>
                            })}
                        </>
                    }
                    .into_any()
                }
                // Claude acknowledging a limit without a utilization, or a
                // credits-only Codex report. Text, never a bar: an empty bar
                // would read as "0% used".
                None => view! {
                    <>
                        <span class="capacity-compact-state">
                            "no percentage limit reported"
                        </span>
                        {coverage_note.map(|note| view! {
                            <span class="capacity-compact-coverage">{note}</span>
                        })}
                    </>
                }
                .into_any(),
            }
        }
        None => {
            let headline = state_headline(state, kind);
            view! { <span class="capacity-compact-state">{headline}</span> }.into_any()
        }
    };

    view! {
        <div
            class="capacity-compact"
            data-capacity-state=slug
            data-mobile-test="capacity-compact"
            role="group"
            aria-label=format!("Subscription capacity reported by {vendor}")
        >
            <div class="capacity-compact-title">
                {format!("Subscription \u{b7} reported by {vendor}")}
            </div>
            {body}
            <div class="capacity-compact-freshness">{freshness}</div>
        </div>
    }
    .into_any()
}

/// Mobile Settings: the same authoritative view as desktop, stacked. Every
/// backend the host reports appears, including the ones with no capacity source
/// — a hidden row reads as "fine".
#[component]
pub fn SubscriptionCapacitySection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let snapshots = Memo::new(move |_| -> Vec<BackendCapacitySnapshot> {
        let Some(host) = state.active_local_host_id.get() else {
            return Vec::new();
        };
        let mut snapshots: Vec<_> = state
            .backend_capacity_by_host
            .get()
            .get(&host)
            .map(|by_kind| by_kind.values().cloned().collect())
            .unwrap_or_default();
        snapshots.sort_by_key(|snapshot| format!("{:?}", snapshot.backend_kind));
        snapshots
    });

    view! {
        <div class="settings-section" data-mobile-test="settings-capacity">
            <h2 class="settings-section-title">"Subscription capacity"</h2>
            <p class="settings-description">
                "Quota reported by each backend for the account it is signed in to. Advisory only \
                 \u{2014} Tyde never reroutes or downgrades backends based on it, and never infers \
                 it from Tyde's own token usage. Both sources report passively, so there is \
                 nothing to refresh."
            </p>
            {move || {
                let snapshots = snapshots.get();
                if snapshots.is_empty() {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted">
                                "No backend has reported capacity state yet."
                            </span>
                        </div>
                    }
                    .into_any();
                }
                view! {
                    <div class="capacity-cards">
                        {snapshots.iter().map(|snapshot| view! {
                            <>
                                {compact_row(snapshot)}
                                {snapshot_card(snapshot)}
                            </>
                        }).collect_view()}
                    </div>
                }
                .into_any()
            }}
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId};
    use leptos::mount::mount_to;
    // `ClaudeLimitType`/`CodexLimitSlot` come in through `use super::*`.
    use protocol::{
        BackendCapacityPayload, CapacityPlanLabel, Envelope, FrameKind, StreamPath, ValueProvenance,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    /// A phone-width viewport: the mobile layout must not clip or overlap here.
    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 390px; height: 800px;",
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

    fn count(element: &HtmlElement, selector: &str) -> u32 {
        element.query_selector_all(selector).unwrap().length()
    }

    // ── Fixtures ────────────────────────────────────────────────────────────
    //
    // These mirror what the server's adapters actually emit — a fixture that
    // invents a value the server never produces tests a UI that will never
    // exist. From `claude.rs` `map_passive_rate_limit_event`, `codex.rs`
    // `map_passive_rate_limits_updated`, and `host.rs`
    // `backend_capacity_snapshots`:
    //
    //   * `provenance.vendor_reported: true` is the ONLY `UsedPercent` shape
    //     either adapter produces, so `used_percent_provenance()` is
    //     `VendorReported`. `remaining_percent_provenance()` is
    //     `DerivedComplement` regardless — Tyde computes it.
    //   * Claude: `scope` and `window` are ALWAYS `NotReported`; `status` is
    //     ALWAYS `Some`; `plan` is ALWAYS `None`. Labels are all distinct
    //     ("weekly limit" vs "Fable 5 limit", …).
    //   * Codex: window labels are `"{limitName} primary limit"` /
    //     `"{limitName} secondary limit"`; credits are labelled "credits".
    //     `status` is ALWAYS `None`; credits carry no window or reset.
    //   * Freshness is recomputed on emit, so a late-joining client receives a
    //     real nonzero `Fresh { age_ms }`.

    const FRESHNESS_THRESHOLD_MS: u64 = 60 * 60 * 1000;

    fn fresh() -> CapacityFreshness {
        CapacityFreshness::Fresh { age_ms: 0 }
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

    fn future_reset() -> CapacityReset {
        CapacityReset::At {
            at_ms: now_ms() + 2 * 24 * 3_600_000,
        }
    }

    /// Claude reports no scope and no window, and always carries a status.
    fn claude_bucket(limit: ClaudeLimitType, label: &str, used: u8) -> CapacityBucket {
        CapacityBucket {
            id: CapacityBucketId::Claude { limit },
            label: label.to_owned(),
            measure: used_percent(used),
            scope: CapacityScope::NotReported,
            window: CapacityWindow::NotReported,
            reset: future_reset(),
            status: Some(CapacityBucketStatus::AllowedWarning),
        }
    }

    /// Codex window buckets carry a rolling window and no status.
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

    fn claude_known() -> BackendCapacitySnapshot {
        BackendCapacitySnapshot {
            backend_kind: BackendKind::Claude,
            state: BackendCapacityState::Known {
                report: CapacityReport {
                    source: CapacitySource::ClaudeRateLimitEvent,
                    observed_at_ms: None,
                    plan: None,
                    buckets: vec![claude_bucket(ClaudeLimitType::SevenDay, "weekly limit", 82)],
                    coverage: CapacityCoverage::RepresentativeBucketOnly,
                },
            },
            retrieved_at_ms: now_ms(),
            freshness: fresh(),
        }
    }

    fn codex_known() -> BackendCapacitySnapshot {
        BackendCapacitySnapshot {
            backend_kind: BackendKind::Codex,
            state: BackendCapacityState::Known {
                report: CapacityReport {
                    source: CapacitySource::CodexAccountRateLimitsUpdated,
                    observed_at_ms: None,
                    plan: Some(CapacityPlanLabel {
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
                        },
                    ],
                    coverage: CapacityCoverage::AllVendorBuckets,
                },
            },
            retrieved_at_ms: now_ms(),
            freshness: fresh(),
        }
    }

    /// Capacity reaches mobile the same way it reaches desktop: through the
    /// server's `BackendCapacity` frame on the host stream. No test writes the
    /// signal directly, so these tests cover the dispatch wiring too.
    ///
    /// Each case uses its own `host_id`: the inbound validators are thread-local
    /// and shared across the wasm test binary, so a shared host would make one
    /// test's sequence numbers collide with another's.
    fn mount_with(
        container: &HtmlElement,
        host_id: &str,
        snapshots: Vec<BackendCapacitySnapshot>,
    ) -> impl Sized {
        let state = AppState::new();
        let host = LocalHostId(host_id.to_owned());
        // Primes the protocol validator with a synthetic Welcome + HostBootstrap
        // and rewinds the seq validator, so the first real frame is seq 0.
        crate::dispatch::prime_host_for_tests(&state, &host);
        state.active_local_host_id.set(Some(host.clone()));

        let envelope = Envelope::from_payload(
            StreamPath(format!("/host/{host_id}")),
            FrameKind::BackendCapacity,
            0,
            &BackendCapacityPayload { snapshots },
        )
        .expect("envelope serialize");
        crate::dispatch::dispatch_envelope(&state, &host, envelope);

        mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <SubscriptionCapacitySection /> }
        })
    }

    /// Mobile shows the same authoritative meaning as desktop: every vendor
    /// bucket, each with its own unit and window, plus the coverage statement.
    /// Nothing is dropped for lack of screen width.
    #[wasm_bindgen_test]
    async fn mobile_renders_every_bucket_and_coverage() {
        let container = make_container();
        let _handle = mount_with(
            &container,
            "h-cap-buckets",
            vec![claude_known(), codex_known()],
        );
        for _ in 0..4 {
            next_tick().await;
        }

        // Claude's 1 bucket + Codex's 3.
        assert_eq!(
            count(&container, ".capacity-bucket"),
            4,
            "mobile renders one row per vendor bucket, same as desktop"
        );

        let text = text_of(&container);
        assert!(
            text.contains("reports only the limit that is currently binding")
                && text.contains("All limits reported by Codex"),
            "both coverage statements must survive on mobile, got: {text}"
        );
        assert!(
            text.contains("82% used") && text.contains("18% remaining"),
            "used/remaining render on the 0-100 scale, got: {text}"
        );
        assert!(
            !text.contains("0.82") && !text.contains("8200"),
            "the 0..1 fraction must never reach the DOM, got: {text}"
        );
        // Provenance is per value, from the protocol helpers: the vendor's used
        // figure is `VendorReported` and is never captioned as derived; the
        // remaining figure is always `DerivedComplement` and is never attributed
        // to the vendor.
        let used = query(&container, ".capacity-figure-used").expect("used figure renders");
        let remaining =
            query(&container, ".capacity-figure-remaining").expect("remaining figure renders");
        let used_text = used.text_content().unwrap_or_default();
        let remaining_text = remaining.text_content().unwrap_or_default();
        assert!(
            used_text.contains("vendor reported") && !used_text.contains("derived"),
            "the vendor's used percentage must never read as derived, got: {used_text}"
        );
        assert!(
            remaining_text.contains("derived (100 \u{2212} used)")
                && !remaining_text.contains("vendor reported"),
            "the remaining complement is Tyde's and must never be attributed to the vendor, \
             got: {remaining_text}"
        );
        // The server's labels are distinct, and the vendor's own bucket type is
        // rendered beside each of them — it is the durable identity, and it is
        // what keeps a Codex `primary` from reading as a Claude `five_hour`.
        assert!(
            text.contains("weekly limit") && text.contains("subscription secondary limit"),
            "the server's own labels are rendered verbatim, got: {text}"
        );
        assert!(
            text.contains("claude seven_day") && text.contains("codex secondary"),
            "the vendor bucket type is rendered on mobile too, got: {text}"
        );
        assert!(
            text.contains("credit balance: 12.5"),
            "credits render as text on mobile too, got: {text}"
        );
        assert!(
            text.contains("plan: pro"),
            "a vendor-reported plan label is shown, got: {text}"
        );
        assert!(
            text.contains("UTC"),
            "absolute reset times are shown, not only relative ones, got: {text}"
        );
    }

    /// Every no-report state renders visibly, with an explanation and no bar. An
    /// `Unsupported` backend that simply vanished on mobile would read as
    /// healthy capacity.
    ///
    /// These are exactly the no-report states the Phase-1 server can emit. The
    /// protocol also carries `AuthError` and `RateLimited`, and the UI renders
    /// them, but no Phase-1 code path produces them — they arrive with the gated
    /// Codex read — so there is no honest fixture for them yet.
    #[wasm_bindgen_test]
    async fn mobile_renders_all_error_states_without_a_bar() {
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
            // Bound to a local rather than passed as `&format!(..)`: `mount_with`
            // returns `impl Sized`, which under Rust 2024's capture rules captures
            // its arguments' lifetimes, so the temporary would be dropped while
            // `_handle` still borrows it.
            let host_id = format!("h-cap-state-{index}");
            let _handle = mount_with(
                &container,
                &host_id,
                vec![BackendCapacitySnapshot {
                    backend_kind: BackendKind::Claude,
                    state: capacity_state,
                    retrieved_at_ms: now_ms(),
                    freshness: fresh(),
                }],
            );
            for _ in 0..4 {
                next_tick().await;
            }

            let text = text_of(&container);
            assert!(
                text.contains(expected),
                "mobile must explain the state; expected {expected:?}, got: {text}"
            );
            assert!(
                query(&container, ".capacity-bar").is_none(),
                "a state with no report must never draw a bar on mobile ({expected})"
            );
            assert!(
                !text.contains('%'),
                "a state with no report must show no percentage ({expected}), got: {text}"
            );
        }
    }

    /// Freshness on mobile comes from the server's verdict and nothing else — a
    /// mobile-only clock is exactly how mobile and desktop end up disagreeing
    /// about the same snapshot.
    ///
    /// The server recomputes `age_ms` on every emit, so mobile must render all
    /// three real shapes: a just-recorded `Fresh`, a **late-joining** client's
    /// `Fresh` with a real nonzero age, and a `Stale` report past the threshold.
    #[wasm_bindgen_test]
    async fn mobile_freshness_comes_from_the_server_verdict() {
        let container = make_container();
        let _handle = mount_with(&container, "h-cap-fresh", vec![claude_known()]);
        for _ in 0..4 {
            next_tick().await;
        }
        let text = text_of(&container);
        assert!(
            text.contains("reported just now"),
            "a just-recorded Fresh snapshot renders as 'just now', got: {text}"
        );
        assert!(
            !text.contains("freshness threshold"),
            "a Fresh snapshot must not be marked stale, got: {text}"
        );

        // Late joiner: the server's recomputed age, rendered verbatim — never
        // "just now" for an aged report.
        let container = make_container();
        let mut late = claude_known();
        late.freshness = CapacityFreshness::Fresh {
            age_ms: 50 * 60 * 1000,
        };
        let _handle = mount_with(&container, "h-cap-latejoin", vec![late]);
        for _ in 0..4 {
            next_tick().await;
        }
        let text = text_of(&container);
        assert!(
            text.contains("reported 50m ago") && !text.contains("just now"),
            "a late-joining client must see the server's real age, got: {text}"
        );
        assert!(
            !text.contains("Stale") && text.contains("82% used"),
            "under the threshold it is still Fresh and still shows the figure, got: {text}"
        );

        let container = make_container();
        let BackendCapacityState::Known { report } = claude_known().state else {
            unreachable!("fixture is Known");
        };
        let _handle = mount_with(
            &container,
            "h-cap-stale",
            vec![BackendCapacitySnapshot {
                backend_kind: BackendKind::Claude,
                state: BackendCapacityState::Stale {
                    report,
                    stale_since_ms: now_ms(),
                },
                retrieved_at_ms: now_ms(),
                freshness: CapacityFreshness::Stale {
                    age_ms: 2 * FRESHNESS_THRESHOLD_MS,
                    threshold_ms: FRESHNESS_THRESHOLD_MS,
                },
            }],
        );
        for _ in 0..4 {
            next_tick().await;
        }

        let text = text_of(&container);
        assert!(text.contains("82% used"), "stale keeps its figure: {text}");
        assert!(text.contains("Stale"), "stale is named: {text}");
        assert!(
            text.contains("2h ago") && text.contains("past the 1h freshness threshold"),
            "the server's freshness verdict is rendered verbatim: {text}"
        );
    }

    /// The compact row is mobile's parity with the desktop popup row: one bar
    /// for the most-constrained bucket, the rest collapsed to a count, and an
    /// explicit "Subscription" label so it can never be read as task tokens.
    /// The layout stacks at phone width without clipping.
    #[wasm_bindgen_test]
    async fn mobile_compact_row_is_labelled_and_stacks() {
        let container = make_container();
        let _handle = mount_with(&container, "h-cap-compact", vec![codex_known()]);
        for _ in 0..4 {
            next_tick().await;
        }

        let compact = query(&container, ".capacity-compact").expect("compact row renders");
        let compact_text = compact.text_content().unwrap_or_default();

        assert_eq!(compact.get_attribute("role").as_deref(), Some("group"));
        let label = compact.get_attribute("aria-label").unwrap_or_default();
        assert!(
            label.contains("Subscription capacity"),
            "the compact row must name itself as subscription capacity, got: {label}"
        );
        assert!(
            compact_text.contains("Subscription"),
            "the visible heading must say Subscription, got: {compact_text}"
        );

        // The most-constrained bucket wins the single bar: the 91% secondary
        // (weekly) window, not the 30% primary (5h) one. The bar's bucket is
        // named by both the server's label and the vendor's own type.
        assert!(
            compact_text.contains("91% used")
                && compact_text.contains("subscription secondary limit")
                && compact_text.contains("codex secondary"),
            "the most-constrained bucket owns the bar and names itself, got: {compact_text}"
        );
        assert!(
            !compact_text.contains("30% used") && !compact_text.contains("primary limit"),
            "the other buckets must not also claim the bar, got: {compact_text}"
        );
        assert_eq!(
            count(&compact, ".capacity-bar"),
            1,
            "exactly one bar in the compact row \u{2014} buckets are never merged"
        );
        assert!(
            compact_text.contains("+2 more"),
            "the remaining buckets collapse to a count, got: {compact_text}"
        );

        // Nothing clips or runs off the edge at a phone width.
        //
        // `get_bounding_client_rect` is unavailable in this crate — it returns a
        // `DomRect`, and mobile-frontend does not enable that web-sys feature — but
        // the assertion never needed a rect. `offset_*` (feature `HtmlElement`) and
        // `client_width` (feature `Element`) are already enabled and carry the same
        // layout facts. They are integers, so the edge checks below are exact and
        // drop the 1px float epsilon the rect form had to carry.
        //
        // Every element here is statically positioned, so all `offset_*` values are
        // measured against the same offset parent — the absolutely-positioned test
        // container — and are directly comparable.
        let viewport_width = container.client_width();
        assert!(
            viewport_width > 0,
            "the test viewport must have layout width"
        );

        let compact_right = compact.offset_left() + compact.offset_width();
        assert!(
            compact_right <= viewport_width,
            "the compact row must not overflow the {viewport_width}px viewport; \
             its right edge is at {compact_right}"
        );

        let bar = query(&container, ".capacity-bar").expect("bar renders");
        let bar_width = bar.offset_width();
        assert!(bar_width > 0, "the bar must have real layout width");
        let bar_right = bar.offset_left() + bar_width;
        assert!(
            bar_right <= viewport_width,
            "the bar must stay inside the {viewport_width}px viewport; \
             its right edge is at {bar_right}"
        );

        // The detail card stacks *below* the compact row, not beside it: its top
        // edge must clear the compact row's bottom edge. The rect form only required
        // `card.top >= compact.top`, which a side-by-side layout would satisfy too;
        // clearing the bottom edge is the guarantee it was reaching for.
        assert_eq!(
            count(&container, ".capacity-card"),
            1,
            "one card for the one backend"
        );
        let card = query(&container, ".capacity-card").expect("card renders");
        let card_top = card.offset_top();
        let compact_bottom = compact.offset_top() + compact.offset_height();
        assert!(
            card_top >= compact_bottom,
            "the detail card must stack under the compact row, not beside it; \
             card top {card_top} vs compact bottom {compact_bottom}"
        );
    }
}
