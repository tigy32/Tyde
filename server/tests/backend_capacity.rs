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

async fn set_usage_setting(fixture: &mut Fixture, path: &str, value: bool) {
    fixture
        .client
        .replace_setting(path, value, !value)
        .await
        .unwrap();
    fixture
        .next_frame_matching("usage settings applied", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await;
}

async fn quota(fixture: &Fixture, used: u8, reset: u64) {
    let mut bucket = claude_bucket(ClaudeLimitType::FiveHour, "Five hour", used);
    bucket.reset = CapacityReset::At { at_ms: reset };
    record(
        fixture,
        CapacityReport {
            source: CapacitySource::ClaudeControlUsage,
            observed_at_ms: None,
            plan: None,
            buckets: vec![bucket],
            coverage: CapacityCoverage::AllVendorBuckets,
        },
    )
    .await;
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn usage_notice(fixture: &mut Fixture, agent: &fixture::TestAgent, text: &str) {
    fixture.next_chat_event_matching(agent, text, |event| {
        matches!(event, protocol::ChatEvent::MessageAdded(message) if message.content.contains(text))
    }).await;
}

#[tokio::test]
async fn usage_limits_hold_compact_and_release_queued_work() {
    use server::backend::mock::{MockScript, MockTurn};
    let mut fixture = Fixture::new().await;
    assert!(!fixture.bootstrap.settings.usage_limits.enabled);
    assert!(!fixture.bootstrap.settings.usage_limits.compact_enabled);
    let agent = fixture
        .spawn_scripted(
            "usage compact",
            MockScript::one(
                MockTurn::text("large working context").with_context_usage(250_000, 300_000),
            )
            .with_unbounded_echo(),
        )
        .await;
    fixture.finish_turn(&agent).await;
    let reset = wall_ms().saturating_sub(1);
    quota(&fixture, 95, reset).await;
    // Disabled means an exceeded quota must not interfere with user work.
    fixture
        .client
        .send_message(&agent.stream, "allowed while disabled".to_owned())
        .await
        .unwrap();
    fixture.finish_turn(&agent).await;
    fixture
        .mock(&agent)
        .await
        .enqueue(MockTurn::text("large context again").with_context_usage(250_000, 300_000))
        .await;
    fixture
        .client
        .send_message(&agent.stream, "prepare context".to_owned())
        .await
        .unwrap();
    fixture.finish_turn(&agent).await;
    set_usage_setting(&mut fixture, "/usage_limits/compact_enabled", true).await;
    set_usage_setting(&mut fixture, "/usage_limits/enabled", true).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause:").await;
    fixture
        .client
        .send_message(&agent.stream, "held until reset".to_owned())
        .await
        .unwrap();
    let mut saw_queue = false;
    let mut saw_compaction = false;
    fixture
        .next_frame_matching("usage compaction and held queue", |env| {
            if env.kind == FrameKind::QueuedMessages && env.stream == agent.stream {
                let payload: protocol::QueuedMessagesPayload = env.parse_payload().unwrap();
                saw_queue |= payload
                    .messages
                    .iter()
                    .any(|message| message.message == "held until reset");
            }
            if env.kind == FrameKind::ContextCompactionNotify {
                let payload: protocol::ContextCompactionNotifyPayload =
                    env.parse_payload().unwrap();
                saw_compaction |= payload.trigger
                    == protocol::CompactionTrigger::UsageLimitRequested
                    && payload.status == protocol::ContextCompactionStatus::Completed;
            }
            saw_queue && saw_compaction
        })
        .await;
    fixture
        .client
        .control_goal(&agent.stream, protocol::GoalControl::Resume)
        .await
        .unwrap();
    fixture
        .next_frame_matching("native goal resume respects usage pause", |env| {
            env.stream == agent.stream
                && env.kind == FrameKind::AgentError
                && env
                    .parse_payload::<protocol::AgentErrorPayload>()
                    .is_ok_and(|payload| payload.message.starts_with("Usage limit pause blocks"))
        })
        .await;
    // Neither an expired timestamp nor missing readings authorizes resumption.
    fixture
        .host_for_test()
        .record_backend_capacity_for_test(
            BackendKind::Claude,
            BackendCapacityState::Unavailable {
                reason: protocol::CapacityUnavailableReason::AwaitingFirstReport,
            },
        )
        .await;
    let control = fixture.mock(&agent).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!control.requests().await.iter().any(|request| matches!(request,
        server::backend::mock::MockRequest::Input(message) if message.message == "held until reset")));
    quota(&fixture, 2, wall_ms() + 18_000_000).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause ended").await;
    fixture.finish_turn(&agent).await;
    assert_eq!(control.requests().await.iter().filter(|request| matches!(request,
        server::backend::mock::MockRequest::Input(message) if message.message == "held until reset")).count(), 1);
    let failed = fixture
        .spawn_scripted(
            "usage compaction failure",
            MockScript::one(MockTurn::text("large context").with_context_usage(250_000, 300_000))
                .with_compaction_failure(server::backend::mock::MockCompactionFailure::Rejected)
                .with_unbounded_echo(),
        )
        .await;
    fixture.finish_turn(&failed).await;
    quota(&fixture, 95, wall_ms().saturating_sub(1)).await;
    usage_notice(&mut fixture, &failed, "Usage limit compaction failed").await;
    fixture
        .client
        .send_message(&failed.stream, "held after failed compaction".to_owned())
        .await
        .unwrap();
    fixture.expect_queued_messages(&failed, 1).await;
    let failed_control = fixture.mock(&failed).await;
    quota(&fixture, 1, wall_ms() + 18_000_000).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!failed_control.requests().await.iter().any(|request| matches!(request,
        server::backend::mock::MockRequest::Input(message) if message.message == "held after failed compaction")));
    set_usage_setting(&mut fixture, "/usage_limits/enabled", false).await;
    usage_notice(&mut fixture, &failed, "Usage limit pause ended").await;
    fixture.finish_turn(&failed).await;
    assert_eq!(failed_control.requests().await.iter().filter(|request| matches!(request,
        server::backend::mock::MockRequest::Input(message) if message.message == "held after failed compaction")).count(), 1);
}

#[tokio::test]
async fn usage_limits_resume_only_the_interrupted_work() {
    use server::backend::mock::{MockRequest, MockScript, MockTurn};
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "usage interrupt",
            MockScript::one(MockTurn::held_text("working")).then(MockTurn::text("continued")),
        )
        .await;
    fixture.next_chat_event_matching(&agent, "working", |event| {
        matches!(event, protocol::ChatEvent::StreamEnd(data) if data.message.content.contains("working"))
    }).await;
    set_usage_setting(&mut fixture, "/usage_limits/enabled", true).await;
    quota(&fixture, 90, wall_ms().saturating_sub(1)).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause:").await;
    quota(&fixture, 1, wall_ms() + 18_000_000).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause ended").await;
    fixture.finish_turn(&agent).await;
    let control = fixture.mock(&agent).await;
    assert_eq!(
        control
            .requests()
            .await
            .iter()
            .filter(|request| matches!(request, MockRequest::Interrupt))
            .count(),
        1
    );
    assert_eq!(
        control
            .requests()
            .await
            .iter()
            .filter(|request| matches!(request, MockRequest::Input(_)))
            .count(),
        1
    );
    control.enqueue(MockTurn::held_text("second task")).await;
    fixture
        .client
        .send_message(&agent.stream, "new work".to_owned())
        .await
        .unwrap();
    fixture.next_chat_event_matching(&agent, "second task", |event| {
        matches!(event, protocol::ChatEvent::StreamEnd(data) if data.message.content.contains("second task"))
    }).await;
    quota(&fixture, 95, wall_ms().saturating_sub(1)).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause:").await;
    fixture.client.interrupt(&agent.stream).await.unwrap();
    fixture
        .next_frame_matching("manual cancellation of usage continuation", |env| {
            env.kind == FrameKind::AgentActivityStats
                && env
                    .parse_payload::<protocol::AgentActivityStatsPayload>()
                    .is_ok_and(|payload| {
                        payload.agent_id == agent.new_agent.agent_id
                            && payload
                                .stats
                                .usage_limit_pause
                                .is_some_and(|pause| !pause.resume_interrupted_turn)
                    })
        })
        .await;
    quota(&fixture, 0, wall_ms() + 18_000_000).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause ended").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        control
            .requests()
            .await
            .iter()
            .filter(|request| matches!(request, MockRequest::Input(_)))
            .count(),
        2
    );
    control.assert_clean().await;
}
