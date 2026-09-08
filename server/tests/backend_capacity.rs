mod fixture;

use fixture::{Fixture, next_frame_matching_on};
use protocol::{
    BackendCapacityPayload, BackendCapacitySnapshot, BackendCapacityState, BackendKind,
    CapacityBucket, CapacityBucketId, CapacityCoverage, CapacityFreshness, CapacityMeasure,
    CapacityPlanLabel, CapacityReport, CapacityReset, CapacityScope, CapacitySource,
    CapacityWindow, ClaudeLimitType, Envelope, FrameKind, ValueProvenance,
};

const AGED_BY_MS: u64 = 30 * 60 * 1000;

fn used_percent(used: u8) -> CapacityMeasure {
    CapacityMeasure::UsedPercent {
        used_percent: used,
        remaining_percent: 100 - used,
        provenance: ValueProvenance {
            vendor_reported: true,
        },
    }
}

fn claude_bucket(limit: ClaudeLimitType, label: &str, used: u8) -> CapacityBucket {
    CapacityBucket {
        id: CapacityBucketId::Claude { limit },
        label: label.to_owned(),
        measure: used_percent(used),
        scope: CapacityScope::Account,
        window: CapacityWindow::Rolling {
            duration_minutes: 5 * 60,
        },
        reset: CapacityReset::NotReported,
        status: None,
    }
}

fn model_bucket(model: &str, used: u8) -> CapacityBucket {
    CapacityBucket {
        id: CapacityBucketId::ClaudeModel {
            name: model.to_owned(),
        },
        label: format!("{model} limit"),
        measure: used_percent(used),
        scope: CapacityScope::Account,
        window: CapacityWindow::Rolling {
            duration_minutes: 7 * 24 * 60,
        },
        reset: CapacityReset::NotReported,
        status: None,
    }
}

fn claude_snapshot(env: &Envelope) -> Option<BackendCapacitySnapshot> {
    if env.kind != FrameKind::BackendCapacity {
        return None;
    }
    let payload: BackendCapacityPayload = env.parse_payload().ok()?;
    payload
        .snapshots
        .into_iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Claude)
}

fn held_report(state: &BackendCapacityState) -> Option<&CapacityReport> {
    match state {
        BackendCapacityState::Known { report } | BackendCapacityState::Stale { report, .. } => {
            Some(report)
        }
        _ => None,
    }
}

fn bucket_percent(report: &CapacityReport, id: &CapacityBucketId) -> Option<u8> {
    report
        .buckets
        .iter()
        .find(|bucket| &bucket.id == id)
        .and_then(|bucket| match bucket.measure {
            CapacityMeasure::UsedPercent { used_percent, .. } => Some(used_percent),
            _ => None,
        })
}

fn reports_percent(env: &Envelope, used: u8) -> bool {
    claude_snapshot(env)
        .as_ref()
        .and_then(|snapshot| held_report(&snapshot.state))
        .is_some_and(|report| {
            report.buckets.iter().any(|bucket| {
                matches!(
                    bucket.measure,
                    CapacityMeasure::UsedPercent { used_percent, .. } if used_percent == used
                )
            })
        })
}

fn freshness_age_ms(freshness: &CapacityFreshness) -> u64 {
    match freshness {
        CapacityFreshness::Fresh { age_ms } | CapacityFreshness::Stale { age_ms, .. } => *age_ms,
    }
}

async fn record(fixture: &Fixture, report: CapacityReport) {
    fixture
        .host_for_test()
        .record_backend_capacity_for_test(
            BackendKind::Claude,
            BackendCapacityState::Known { report },
        )
        .await;
}

/// A reading that covers fewer buckets than the one the host already holds must
/// update the bars it names and leave the rest alone.
///
/// Claude's `get_usage` answers some sessions with only the model-scoped weekly
/// limit and no `subscription_type`, and its passive `rate_limit_event` reports
/// only the currently-binding bucket. Both used to replace the complete report,
/// so the settings usage view flipped between three bars and one every time an
/// agent refreshed.
#[tokio::test]
async fn narrower_capacity_report_updates_bars_instead_of_replacing_them() {
    let mut fixture = Fixture::new().await;

    record(
        &fixture,
        CapacityReport {
            source: CapacitySource::ClaudeControlUsage,
            observed_at_ms: None,
            plan: Some(CapacityPlanLabel {
                label: "max".to_owned(),
            }),
            buckets: vec![
                claude_bucket(ClaudeLimitType::FiveHour, "session limit", 38),
                claude_bucket(ClaudeLimitType::SevenDay, "weekly limit", 6),
                model_bucket("Fable", 5),
            ],
            coverage: CapacityCoverage::AllVendorBuckets,
        },
    )
    .await;
    next_frame_matching_on(
        &mut fixture.client,
        "complete claude capacity report",
        |env| reports_percent(env, 5),
    )
    .await;

    // The complete reading is half an hour old when the narrow one lands, so the
    // merged snapshot must still report that age.
    fixture
        .host_for_test()
        .age_backend_capacity_for_test(BackendKind::Claude, AGED_BY_MS)
        .await;

    record(
        &fixture,
        CapacityReport {
            source: CapacitySource::ClaudeControlUsage,
            observed_at_ms: None,
            plan: None,
            buckets: vec![model_bucket("Fable", 42)],
            coverage: CapacityCoverage::AllVendorBuckets,
        },
    )
    .await;
    let env = next_frame_matching_on(
        &mut fixture.client,
        "claude capacity report carrying the narrow reading",
        |env| reports_percent(env, 42),
    )
    .await;

    let snapshot = claude_snapshot(&env).expect("claude capacity snapshot");
    let report = held_report(&snapshot.state).expect("merged claude capacity report");
    assert_eq!(
        report.buckets.len(),
        3,
        "a narrower reading must not drop the bars it does not mention: {:?}",
        report.buckets
    );
    assert_eq!(
        bucket_percent(
            report,
            &CapacityBucketId::Claude {
                limit: ClaudeLimitType::FiveHour
            }
        ),
        Some(38),
        "the session bar must survive a reading that never mentions it"
    );
    assert_eq!(
        bucket_percent(
            report,
            &CapacityBucketId::Claude {
                limit: ClaudeLimitType::SevenDay
            }
        ),
        Some(6),
        "the weekly bar must survive a reading that never mentions it"
    );
    assert_eq!(
        bucket_percent(
            report,
            &CapacityBucketId::ClaudeModel {
                name: "Fable".to_owned()
            }
        ),
        Some(42),
        "the bar the narrow reading did name must take its new value"
    );
    assert_eq!(
        report.plan.as_ref().map(|plan| plan.label.as_str()),
        Some("max"),
        "a reading with no plan label must not erase the one already held"
    );
    assert!(
        freshness_age_ms(&snapshot.freshness) >= AGED_BY_MS,
        "a partial read must not reset the age of the bars it did not refresh; age was {}ms",
        freshness_age_ms(&snapshot.freshness)
    );

    // The passive path is narrower still and claims only representative
    // coverage. It updates its own bar without downgrading what the complete
    // reading already established about the rest.
    record(
        &fixture,
        CapacityReport {
            source: CapacitySource::ClaudeRateLimitEvent,
            observed_at_ms: None,
            plan: None,
            buckets: vec![claude_bucket(
                ClaudeLimitType::FiveHour,
                "session limit",
                91,
            )],
            coverage: CapacityCoverage::RepresentativeBucketOnly,
        },
    )
    .await;
    let env = next_frame_matching_on(
        &mut fixture.client,
        "claude capacity report carrying the passive reading",
        |env| reports_percent(env, 91),
    )
    .await;

    let snapshot = claude_snapshot(&env).expect("claude capacity snapshot");
    let report = held_report(&snapshot.state).expect("merged claude capacity report");
    assert_eq!(
        report.buckets.len(),
        3,
        "a representative reading must not collapse the report to its one bucket: {:?}",
        report.buckets
    );
    assert_eq!(
        bucket_percent(
            report,
            &CapacityBucketId::Claude {
                limit: ClaudeLimitType::FiveHour
            }
        ),
        Some(91)
    );
    assert_eq!(
        bucket_percent(
            report,
            &CapacityBucketId::Claude {
                limit: ClaudeLimitType::SevenDay
            }
        ),
        Some(6)
    );
    assert_eq!(
        bucket_percent(
            report,
            &CapacityBucketId::ClaudeModel {
                name: "Fable".to_owned()
            }
        ),
        Some(42)
    );
    assert_eq!(
        report.coverage,
        CapacityCoverage::AllVendorBuckets,
        "folding in one binding bucket must not retract the complete coverage already held"
    );
    assert_eq!(
        report.source,
        CapacitySource::ClaudeControlUsage,
        "the merged report must stay attributed to the source that produced its bucket set"
    );
}
