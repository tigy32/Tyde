mod fixture;

use fixture::{Fixture, next_frame_matching_on};
use protocol::{
    BackendCapacityPayload, BackendCapacitySnapshot, BackendCapacityState, BackendKind,
    CapacityBucket, CapacityBucketId, CapacityCoverage, CapacityFreshness, CapacityMeasure,
    CapacityPlanLabel, CapacityReport, CapacityReset, CapacityScope, CapacitySource,
    CapacityWindow, ClaudeLimitType, Envelope, FrameKind, ValueProvenance,
};

const AGED_BY_MS: u64 = 30 * 60 * 1000;
const HOUR_MS: u64 = 60 * 60 * 1000;

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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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

    for used in 1..=8 {
        let updates = vec![
            claude_bucket(ClaudeLimitType::FiveHour, "session limit", used),
            claude_bucket(ClaudeLimitType::SevenDay, "weekly limit", used + 10),
            model_bucket("Fable", used + 20),
        ];
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(updates.len()));
        let mut tasks = Vec::new();
        for bucket in updates.clone() {
            let host = fixture.host_for_test();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                host.record_backend_capacity_for_test(
                    BackendKind::Claude,
                    BackendCapacityState::Known {
                        report: CapacityReport {
                            source: CapacitySource::ClaudeControlUsage,
                            observed_at_ms: None,
                            plan: None,
                            buckets: vec![bucket],
                            coverage: CapacityCoverage::AllVendorBuckets,
                        },
                    },
                )
                .await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut client = fixture.connect().await;
        let env = next_frame_matching_on(&mut client, "concurrent capacity replay", |env| {
            claude_snapshot(env).is_some()
        })
        .await;
        let snapshot = claude_snapshot(&env).unwrap();
        assert_eq!(
            held_report(&snapshot.state).unwrap().buckets,
            updates,
            "simultaneous partial readings must preserve every updated bar"
        );
    }
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

#[tokio::test]
async fn usage_limits_recover_from_corrections_without_reset_markers() {
    use server::backend::mock::{MockRequest, MockScript, MockTurn};
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "quota corrections",
            MockScript::one(MockTurn::text("ready")).with_unbounded_echo(),
        )
        .await;
    fixture.finish_turn(&agent).await;
    set_usage_setting(&mut fixture, "/usage_limits/enabled", true).await;
    let control = fixture.mock(&agent).await;
    for (cycle, reset) in [
        CapacityReset::At {
            at_ms: wall_ms() + 18_000_000,
        },
        CapacityReset::NotReported,
    ]
    .into_iter()
    .enumerate()
    {
        let mut bucket = claude_bucket(ClaudeLimitType::FiveHour, "session limit", 98);
        bucket.reset = reset;
        let mut report = CapacityReport {
            source: CapacitySource::ClaudeControlUsage,
            observed_at_ms: None,
            plan: None,
            buckets: vec![bucket, model_bucket("Fable", 88)],
            coverage: CapacityCoverage::AllVendorBuckets,
        };
        record(&fixture, report.clone()).await;
        usage_notice(&mut fixture, &agent, "Usage limit pause:").await;
        fixture
            .client
            .send_message(&agent.stream, "held for corrected reading".to_owned())
            .await
            .unwrap();
        fixture.expect_queued_messages(&agent, 1).await;
        // A missing magnitude is not evidence that the triggering quota recovered.
        report.buckets[0].measure = CapacityMeasure::ReportedWithoutMagnitude;
        record(&fixture, report.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            control
                .requests()
                .await
                .iter()
                .filter(|request| matches!(request,
            MockRequest::Input(message) if message.message == "held for corrected reading"))
                .count(),
            cycle
        );
        report.buckets[0].measure = used_percent(3);
        record(&fixture, report).await;
        usage_notice(&mut fixture, &agent, "Usage limit pause ended").await;
        fixture.finish_turn(&agent).await;
        assert_eq!(
            control
                .requests()
                .await
                .iter()
                .filter(|request| matches!(request,
            MockRequest::Input(message) if message.message == "held for corrected reading"))
                .count(),
            cycle + 1
        );
    }
    quota(&fixture, 95, wall_ms() + 18_000_000).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause:").await;
    fixture
        .client
        .send_message(&agent.stream, "held until threshold raised".to_owned())
        .await
        .unwrap();
    fixture.expect_queued_messages(&agent, 1).await;
    fixture
        .client
        .replace_setting("/usage_limits/stop_used_percent", 100u8, 90u8)
        .await
        .unwrap();
    usage_notice(&mut fixture, &agent, "Usage limit pause ended").await;
    fixture.finish_turn(&agent).await;
    assert_eq!(
        control
            .requests()
            .await
            .iter()
            .filter(|request| matches!(request,
        MockRequest::Input(message) if message.message == "held until threshold raised"))
            .count(),
        1
    );
    control.assert_clean().await;
}

#[tokio::test]
async fn usage_limits_do_not_treat_cached_observations_as_fresh() {
    use server::backend::mock::{MockRequest, MockScript, MockTurn};
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "cached quota",
            MockScript::one(MockTurn::text("ready")).with_unbounded_echo(),
        )
        .await;
    fixture.finish_turn(&agent).await;
    set_usage_setting(&mut fixture, "/usage_limits/enabled", true).await;
    let mut report = CapacityReport {
        source: CapacitySource::ClaudeControlUsage,
        observed_at_ms: Some(wall_ms().saturating_sub(180_000)),
        plan: None,
        buckets: vec![claude_bucket(
            ClaudeLimitType::FiveHour,
            "session limit",
            98,
        )],
        coverage: CapacityCoverage::AllVendorBuckets,
    };
    record(&fixture, report.clone()).await;
    fixture
        .client
        .send_message(&agent.stream, "not blocked by old quota".to_owned())
        .await
        .unwrap();
    fixture.finish_turn(&agent).await;
    report.observed_at_ms = Some(wall_ms());
    record(&fixture, report.clone()).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause:").await;
    fixture
        .client
        .send_message(&agent.stream, "await fresh correction".to_owned())
        .await
        .unwrap();
    fixture.expect_queued_messages(&agent, 1).await;
    let triggering_observation = report.observed_at_ms.unwrap();
    report.buckets[0].measure = used_percent(1);
    report.observed_at_ms = Some(triggering_observation.saturating_sub(1));
    record(&fixture, report.clone()).await;
    let mut replay_client = fixture.connect().await;
    let env = next_frame_matching_on(
        &mut replay_client,
        "latest quota observation replay",
        |env| claude_snapshot(env).is_some(),
    )
    .await;
    let snapshot = claude_snapshot(&env).unwrap();
    assert_eq!(
        bucket_percent(held_report(&snapshot.state).unwrap(), &report.buckets[0].id),
        Some(98),
        "a delayed older reading must not overwrite newer quota data or release held work"
    );
    report.observed_at_ms = Some(wall_ms().saturating_sub(180_000));
    record(&fixture, report.clone()).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let control = fixture.mock(&agent).await;
    assert!(
        !control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request,
        MockRequest::Input(message) if message.message == "await fresh correction"))
    );
    report.observed_at_ms = Some(wall_ms());
    record(&fixture, report).await;
    usage_notice(&mut fixture, &agent, "Usage limit pause ended").await;
    fixture.finish_turn(&agent).await;
    control.assert_clean().await;
}

fn wakeup_report(
    backend: BackendKind,
    bucket: CapacityBucketId,
    now: u64,
    reset: u64,
) -> (BackendKind, CapacityReport) {
    let source = match backend {
        BackendKind::Claude => CapacitySource::ClaudeControlUsage,
        BackendKind::Codex => CapacitySource::CodexAccountRateLimitsUpdated,
        BackendKind::Antigravity => CapacitySource::AntigravityUsageCommand,
        BackendKind::Grok => CapacitySource::GrokBilling,
        _ => panic!("unsupported wake-up fixture backend"),
    };
    (
        backend,
        CapacityReport {
            source,
            observed_at_ms: Some(now),
            plan: None,
            buckets: vec![CapacityBucket {
                id: bucket,
                label: "usage window".to_owned(),
                measure: used_percent(0),
                scope: CapacityScope::Account,
                window: CapacityWindow::Rolling {
                    duration_minutes: 5 * 60,
                },
                reset: CapacityReset::At { at_ms: reset },
                status: None,
            }],
            coverage: CapacityCoverage::AllVendorBuckets,
        },
    )
}

async fn enable_wakeup_backends(fixture: &mut Fixture) {
    fixture
        .client
        .replace_setting(
            "/enabled_backends",
            vec![
                BackendKind::Claude,
                BackendKind::Codex,
                BackendKind::Antigravity,
                BackendKind::Grok,
            ],
            fixture.bootstrap.settings.enabled_backends.clone(),
        )
        .await
        .unwrap();
    fixture
        .next_frame_matching("wake-up backends enabled", |env| {
            env.kind == FrameKind::HostSettings
        })
        .await;
}

async fn assert_hidden_hi(launch: &server::UsageWakeupLaunchForTest) {
    use server::backend::mock::MockRequest;
    let requests = launch.control.requests().await;
    assert_eq!(
        requests.len(),
        1,
        "maintenance must never send a follow-up: {requests:?}"
    );
    assert!(
        matches!(&requests[0], MockRequest::Launch { message } if message == "hi"),
        "maintenance prompt must be exactly hi: {requests:?}"
    );
    assert_eq!(
        launch.config.execution_mode,
        server::backend::BackendExecutionMode::InferenceOnly
    );
    assert_eq!(
        launch.config.resolved_spawn_config.tool_policy,
        protocol::ToolPolicy::AllowList { tools: vec![] }
    );
    assert!(launch.config.startup_mcp_servers.is_empty());
    assert!(launch.config.cost_hint.is_none());
    launch.control.assert_clean().await;
}

#[tokio::test]
async fn usage_wakeups_are_hourly_hidden_and_durable_even_after_failure() {
    use server::backend::mock::{MockScript, MockTurn};
    let mut fixture = Fixture::new().await;
    enable_wakeup_backends(&mut fixture).await;
    let now = wall_ms();
    let claude = CapacityBucketId::Claude {
        limit: ClaudeLimitType::FiveHour,
    };
    let codex = CapacityBucketId::Codex {
        slot: protocol::CodexLimitSlot::Primary,
    };
    let reset = now - HOUR_MS;
    let host = fixture.host_for_test();
    host.run_usage_wakeup_tick_for_test(
        now,
        vec![wakeup_report(
            BackendKind::Claude,
            claude.clone(),
            now,
            reset,
        )],
    )
    .await;
    assert!(
        host.usage_wakeup_launches_for_test().await.is_empty(),
        "default settings must spend nothing"
    );
    set_usage_setting(&mut fixture, "/usage_limits/auto_start_short_windows", true).await;
    record(
        &fixture,
        wakeup_report(BackendKind::Claude, claude.clone(), now, reset).1,
    )
    .await;
    fixture
        .client
        .backend_capacity_refresh(protocol::BackendCapacityRefreshPayload {
            backend: BackendKind::Claude,
        })
        .await
        .unwrap();
    fixture
        .next_frame_matching("manual refresh answered without wake-up", |env| {
            claude_snapshot(env).is_some_and(|snapshot| {
                matches!(snapshot.state, BackendCapacityState::Unsupported { .. })
            })
        })
        .await;
    assert!(
        host.usage_wakeup_launches_for_test().await.is_empty(),
        "manual/passive refresh must never spend quota"
    );
    host.run_usage_wakeup_tick_for_test(
        now,
        vec![wakeup_report(
            BackendKind::Claude,
            claude.clone(),
            now,
            reset,
        )],
    )
    .await;
    let launches = host.usage_wakeup_launches_for_test().await;
    assert_eq!(launches.len(), 1);
    assert_hidden_hi(&launches[0]).await;
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.agents.is_empty(),
        "maintenance must not appear as an agent"
    );
    assert!(
        bootstrap.sessions.is_empty(),
        "maintenance must not appear as a saved chat"
    );

    let too_soon = now + HOUR_MS - 1;
    host.run_usage_wakeup_tick_for_test(
        too_soon,
        vec![wakeup_report(
            BackendKind::Codex,
            codex.clone(),
            too_soon,
            reset,
        )],
    )
    .await;
    assert_eq!(
        host.usage_wakeup_launches_for_test().await.len(),
        1,
        "hourly cap is host-wide, not per provider"
    );
    for hour in [1, 2, 10] {
        let tick = now + hour * HOUR_MS;
        host.run_usage_wakeup_tick_for_test(
            tick,
            vec![wakeup_report(
                BackendKind::Claude,
                claude.clone(),
                tick,
                reset,
            )],
        )
        .await;
    }
    assert_eq!(
        host.usage_wakeup_launches_for_test().await.len(),
        1,
        "unchanged zero usage must not cause endless hourly hi messages"
    );

    fixture.restart_host().await;
    assert!(
        fixture
            .bootstrap
            .settings
            .usage_limits
            .auto_start_short_windows
    );
    assert!(
        !fixture
            .bootstrap
            .settings
            .usage_limits
            .auto_start_weekly_windows
    );
    let restarted = fixture.host_for_test();
    let tick = now + 11 * HOUR_MS;
    restarted
        .run_usage_wakeup_tick_for_test(
            tick,
            vec![wakeup_report(
                BackendKind::Claude,
                claude.clone(),
                tick,
                reset,
            )],
        )
        .await;
    assert!(
        restarted.usage_wakeup_launches_for_test().await.is_empty(),
        "restart must retain reset deduplication"
    );

    restarted
        .set_usage_wakeup_script_for_test(MockScript::one(MockTurn::error_card(
            "provider unavailable",
        )))
        .await;
    let tick = now + 12 * HOUR_MS;
    let new_reset = tick - 1;
    restarted
        .run_usage_wakeup_tick_for_test(
            tick,
            vec![wakeup_report(
                BackendKind::Claude,
                claude.clone(),
                tick,
                new_reset,
            )],
        )
        .await;
    let launches = restarted.usage_wakeup_launches_for_test().await;
    assert_eq!(launches.len(), 1);
    assert_hidden_hi(&launches[0]).await;
    let next_tick = tick + HOUR_MS - 1;
    // A second host holding the same store must read the persisted guard, not
    // its own obsolete in-memory last-attempt time.
    host.run_usage_wakeup_tick_for_test(
        next_tick,
        vec![wakeup_report(BackendKind::Codex, codex, next_tick, reset)],
    )
    .await;
    assert_eq!(host.usage_wakeup_launches_for_test().await.len(), 1);
    let tick = now + 20 * HOUR_MS;
    restarted
        .run_usage_wakeup_tick_for_test(
            tick,
            vec![wakeup_report(BackendKind::Claude, claude, tick, new_reset)],
        )
        .await;
    assert_eq!(
        restarted.usage_wakeup_launches_for_test().await.len(),
        1,
        "a failed attempt must never be retried for that reset"
    );

    std::fs::write(fixture.store_dir().join("usage_wakeups.json"), "corrupt").unwrap();
    let tick = now + 21 * HOUR_MS;
    restarted
        .run_usage_wakeup_tick_for_test(
            tick,
            vec![wakeup_report(
                BackendKind::Claude,
                CapacityBucketId::Claude {
                    limit: ClaudeLimitType::FiveHour,
                },
                tick,
                tick - 1,
            )],
        )
        .await;
    assert_eq!(
        restarted.usage_wakeup_launches_for_test().await.len(),
        1,
        "unreadable spending guard must fail closed"
    );
}

#[tokio::test]
async fn usage_wakeups_prioritize_weekly_and_target_each_provider_quota() {
    let mut fixture = Fixture::new().await;
    enable_wakeup_backends(&mut fixture).await;
    set_usage_setting(&mut fixture, "/usage_limits/auto_start_short_windows", true).await;
    set_usage_setting(
        &mut fixture,
        "/usage_limits/auto_start_weekly_windows",
        true,
    )
    .await;
    let host = fixture.host_for_test();
    let now = wall_ms();
    let reset = now - HOUR_MS;
    let claude = CapacityBucketId::Claude {
        limit: ClaudeLimitType::FiveHour,
    };
    let codex = CapacityBucketId::Codex {
        slot: protocol::CodexLimitSlot::Secondary,
    };
    let grok = CapacityBucketId::Grok {
        bucket: "weekly".to_owned(),
    };
    let agy_gemini = CapacityBucketId::Antigravity {
        bucket: "gemini-weekly".to_owned(),
    };
    let agy_third_party = CapacityBucketId::Antigravity {
        bucket: "3p-5h".to_owned(),
    };
    let reports = |tick| {
        let mut gemini = wakeup_report(BackendKind::Antigravity, agy_gemini.clone(), tick, reset);
        gemini.1.buckets[0].window = CapacityWindow::Rolling {
            duration_minutes: 7 * 24 * 60,
        };
        gemini.1.buckets.push(
            wakeup_report(
                BackendKind::Antigravity,
                agy_third_party.clone(),
                tick,
                reset,
            )
            .1
            .buckets
            .remove(0),
        );
        vec![
            wakeup_report(BackendKind::Claude, claude.clone(), tick, reset),
            wakeup_report(BackendKind::Grok, grok.clone(), tick, reset),
            gemini,
            wakeup_report(BackendKind::Codex, codex.clone(), tick, reset),
        ]
    };
    let mut concurrent = Vec::new();
    for _ in 0..8 {
        let host = host.clone();
        let reports = reports(now);
        concurrent.push(tokio::spawn(async move {
            host.run_usage_wakeup_tick_for_test(now, reports).await;
        }));
    }
    for task in concurrent {
        task.await.unwrap();
    }
    let launches = host.usage_wakeup_launches_for_test().await;
    assert_eq!(
        launches.len(),
        1,
        "concurrent ticks must claim only one launch"
    );
    assert_eq!(
        launches[0].backend_kind,
        BackendKind::Grok,
        "weekly must precede short even when short appears first"
    );
    for hour in 1..5 {
        let tick = now + hour * HOUR_MS;
        host.run_usage_wakeup_tick_for_test(tick, reports(tick))
            .await;
    }
    let launches = host.usage_wakeup_launches_for_test().await;
    assert_eq!(
        launches.len(),
        5,
        "each independent quota group gets one turn, across five separate hours"
    );
    assert_eq!(launches[1].backend_kind, BackendKind::Antigravity);
    assert!(
        matches!(launches[1].config.session_settings.as_ref().unwrap().0.get("model"), Some(protocol::SessionSettingValue::String(model)) if model.starts_with("Gemini "))
    );
    assert_eq!(launches[2].backend_kind, BackendKind::Claude);
    assert_eq!(launches[3].backend_kind, BackendKind::Antigravity);
    assert!(
        matches!(launches[3].config.session_settings.as_ref().unwrap().0.get("model"), Some(protocol::SessionSettingValue::String(model)) if model.starts_with("Claude Sonnet "))
    );
    assert_eq!(
        launches[4].backend_kind,
        BackendKind::Codex,
        "Codex secondary with five-hour duration belongs to short windows"
    );
    for launch in &launches {
        assert_hidden_hi(launch).await;
    }
    set_usage_setting(
        &mut fixture,
        "/usage_limits/auto_start_short_windows",
        false,
    )
    .await;
    set_usage_setting(
        &mut fixture,
        "/usage_limits/auto_start_weekly_windows",
        false,
    )
    .await;
    let tick = now + 8 * 24 * HOUR_MS;
    host.run_usage_wakeup_tick_for_test(
        tick,
        vec![wakeup_report(BackendKind::Grok, grok, tick, tick - 1)],
    )
    .await;
    assert_eq!(
        host.usage_wakeup_launches_for_test().await.len(),
        5,
        "disabling both toggles stops maintenance"
    );
}

#[tokio::test]
async fn usage_wakeups_require_reset_evidence_and_coalesce_overlapping_windows() {
    let mut fixture = Fixture::new().await;
    // A fresh host bootstraps with no enabled providers. Enabling maintenance
    // must not implicitly enable them; the scenario needs an explicit opt-in.
    assert!(fixture.bootstrap.settings.enabled_backends.is_empty());
    enable_wakeup_backends(&mut fixture).await;
    set_usage_setting(
        &mut fixture,
        "/usage_limits/auto_start_weekly_windows",
        true,
    )
    .await;
    let host = fixture.host_for_test();
    let now = wall_ms();
    let sonnet = CapacityBucketId::Claude {
        limit: ClaudeLimitType::SevenDaySonnet,
    };
    let short = CapacityBucketId::Claude {
        limit: ClaudeLimitType::FiveHour,
    };
    for hour in 0..5 {
        let tick = now + hour * HOUR_MS;
        let mut report = wakeup_report(BackendKind::Claude, sonnet.clone(), tick, tick - 1);
        match hour {
            0 => report.1.buckets[0].reset = CapacityReset::NotReported,
            1 => report.1.observed_at_ms = Some(tick - 180_000),
            2 => report.1.buckets[0].measure = CapacityMeasure::ReportedWithoutMagnitude,
            3 => report.1.buckets[0].measure = used_percent(1),
            4 => {
                let mut exhausted =
                    wakeup_report(BackendKind::Claude, short.clone(), tick, tick - 1)
                        .1
                        .buckets
                        .remove(0);
                exhausted.measure = used_percent(100);
                report.1.buckets.push(exhausted);
            }
            _ => unreachable!(),
        }
        host.run_usage_wakeup_tick_for_test(tick, vec![report])
            .await;
        assert!(
            host.usage_wakeup_launches_for_test().await.is_empty(),
            "unsafe observation at hour {hour} must not spend quota"
        );
    }
    let tick = now + 5 * HOUR_MS;
    let reset = tick + HOUR_MS;
    host.run_usage_wakeup_tick_for_test(
        tick,
        vec![wakeup_report(
            BackendKind::Claude,
            sonnet.clone(),
            tick,
            reset,
        )],
    )
    .await;
    assert!(
        host.usage_wakeup_launches_for_test().await.is_empty(),
        "future timer with rounded zero is already running"
    );
    let tick = reset;
    let mut report = wakeup_report(BackendKind::Claude, sonnet.clone(), tick, reset);
    report.1.buckets[0].reset = CapacityReset::NotReported;
    report.1.buckets.push(
        wakeup_report(BackendKind::Claude, short.clone(), tick, reset)
            .1
            .buckets
            .remove(0),
    );
    host.run_usage_wakeup_tick_for_test(tick, vec![report.clone()])
        .await;
    let launches = host.usage_wakeup_launches_for_test().await;
    assert_eq!(
        launches.len(),
        1,
        "previously observed deadline establishes the reset even when it disappears"
    );
    assert_eq!(
        launches[0]
            .config
            .session_settings
            .as_ref()
            .unwrap()
            .0
            .get("model"),
        Some(&protocol::SessionSettingValue::String("sonnet".to_owned()))
    );
    assert_hidden_hi(&launches[0]).await;
    set_usage_setting(
        &mut fixture,
        "/usage_limits/auto_start_weekly_windows",
        false,
    )
    .await;
    set_usage_setting(&mut fixture, "/usage_limits/auto_start_short_windows", true).await;
    for hour in [7, 14] {
        let tick = now + hour * HOUR_MS;
        report.1.observed_at_ms = Some(tick);
        host.run_usage_wakeup_tick_for_test(tick, vec![report.clone()])
            .await;
        assert_eq!(
            host.usage_wakeup_launches_for_test().await.len(),
            1,
            "weekly hi also consumes the overlapping short reset even if short was disabled"
        );
    }
    set_usage_setting(
        &mut fixture,
        "/usage_limits/auto_start_weekly_windows",
        true,
    )
    .await;
    let tick = now + 15 * HOUR_MS;
    host.run_usage_wakeup_tick_for_test(
        tick,
        vec![wakeup_report(BackendKind::Claude, sonnet, tick, tick - 1)],
    )
    .await;
    assert_eq!(
        host.usage_wakeup_launches_for_test().await.len(),
        1,
        "timestamp drift cannot bypass the independent weekly cooldown"
    );
    let tick = now + 8 * 24 * HOUR_MS;
    host.run_usage_wakeup_tick_for_test(
        tick,
        vec![wakeup_report(
            BackendKind::Claude,
            CapacityBucketId::ClaudeModel {
                name: "Fable".to_owned(),
            },
            tick,
            tick - 1,
        )],
    )
    .await;
    let launches = host.usage_wakeup_launches_for_test().await;
    assert_eq!(launches.len(), 2);
    assert_eq!(
        launches[1]
            .config
            .session_settings
            .as_ref()
            .unwrap()
            .0
            .get("model"),
        Some(&protocol::SessionSettingValue::String("fable".to_owned())),
        "model-scoped quotas must use the alias advertised in the backend catalog"
    );
    assert_hidden_hi(&launches[1]).await;
}
