use std::path::PathBuf;
use std::time::Duration;

use client::{AgentEndpoint, AgentEvent, HostEndpoint, HostEvent, ProjectEvent};
use protocol::{
    AgentBootstrapEvent, BackendCapacityState, BackendKind, ChatEvent, ProjectRootPath,
    ReviewSummaryScope, SendMessagePayload, SpawnAgentParams, SpawnAgentPayload,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

#[derive(Debug)]
enum AgentProbe {
    Started(String),
    Final(String),
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

// Provider normalization is exercised by real conformance. This client flow
// starts at the typed backend boundary and verifies replay and freshness.
fn codex_capacity_state(used_percent: u8) -> BackendCapacityState {
    use protocol::*;
    let rolling = |slot, label: &str, percent, minutes, reset| CapacityBucket {
        id: CapacityBucketId::Codex { slot },
        label: label.to_owned(),
        measure: CapacityMeasure::UsedPercent {
            used_percent: percent,
            remaining_percent: 100 - percent,
            provenance: ValueProvenance {
                vendor_reported: true,
            },
        },
        scope: CapacityScope::Individual,
        window: CapacityWindow::Rolling {
            duration_minutes: minutes,
        },
        reset: CapacityReset::At { at_ms: reset },
        status: None,
    };
    BackendCapacityState::Known {
        report: CapacityReport {
            source: CapacitySource::CodexAccountRateLimitsUpdated,
            observed_at_ms: None,
            plan: Some(CapacityPlanLabel {
                label: "pro".to_owned(),
            }),
            coverage: CapacityCoverage::AllVendorBuckets,
            buckets: vec![
                rolling(
                    CodexLimitSlot::Primary,
                    "5-hour limit",
                    used_percent,
                    300,
                    1_700_000_000_000,
                ),
                rolling(
                    CodexLimitSlot::Secondary,
                    "Weekly limit",
                    17,
                    10_080,
                    1_700_100_000_000,
                ),
                CapacityBucket {
                    id: CapacityBucketId::Codex {
                        slot: CodexLimitSlot::Credits,
                    },
                    label: "Credits".to_owned(),
                    measure: CapacityMeasure::Credits {
                        has_credits: true,
                        unlimited: false,
                        balance: Some("12.50".to_owned()),
                    },
                    scope: CapacityScope::Account,
                    window: CapacityWindow::NotReported,
                    reset: CapacityReset::NotReported,
                    status: None,
                },
            ],
        },
    }
}

fn claude_capacity_state() -> BackendCapacityState {
    use protocol::*;
    BackendCapacityState::Known {
        report: CapacityReport {
            source: CapacitySource::ClaudeRateLimitEvent,
            observed_at_ms: None,
            plan: None,
            coverage: CapacityCoverage::RepresentativeBucketOnly,
            buckets: vec![CapacityBucket {
                id: CapacityBucketId::Claude {
                    limit: ClaudeLimitType::SevenDayOverageIncluded,
                },
                label: "Fable 5 limit".to_owned(),
                measure: CapacityMeasure::UsedPercent {
                    used_percent: 82,
                    remaining_percent: 18,
                    provenance: ValueProvenance {
                        vendor_reported: true,
                    },
                },
                scope: CapacityScope::Account,
                window: CapacityWindow::Rolling {
                    duration_minutes: 10_080,
                },
                reset: CapacityReset::At {
                    at_ms: 1_700_000_000_000,
                },
                status: Some(CapacityBucketStatus::AllowedWarning),
            }],
        },
    }
}

#[tokio::test]
async fn runtime_surfaces_typed_server_voice_capabilities() {
    let directory = tempfile::tempdir().expect("create voice capability tempdir");
    let host = server::spawn_host_with_mock_backend(
        directory.path().join("sessions.json"),
        directory.path().join("projects.json"),
        directory.path().join("settings.json"),
    )
    .expect("initialize voice-capable host");
    let HostEndpoint {
        mut events,
        commands: _,
    } = connect_runtime(host).await;

    let capabilities = timeout(Duration::from_secs(5), async {
        loop {
            if let HostEvent::Voice(client::VoiceEvent::Capabilities(capabilities)) =
                events.recv().await.expect("host event stream")
            {
                return capabilities;
            }
        }
    })
    .await
    .expect("typed VoiceCapabilities event");
    assert_eq!(capabilities.protocol, protocol::VOICE_PROTOCOL_VERSION);
    assert!(
        capabilities
            .conversation_formats
            .iter()
            .all(|pair| pair.uplink.valid() && pair.downlink.valid())
    );
    assert!(
        capabilities
            .dictation_formats
            .iter()
            .all(protocol::VoiceAudioFormat::valid)
    );
}

#[tokio::test]
async fn passive_capacity_replays_deduplicates_and_stales_over_public_client() {
    init_tracing();
    let directory = tempfile::tempdir().expect("create capacity tempdir");
    let host = server::spawn_host_with_mock_backend(
        directory.path().join("sessions.json"),
        directory.path().join("projects.json"),
        directory.path().join("settings.json"),
    )
    .expect("initialize mock host");

    let HostEndpoint {
        mut events,
        commands,
    } = connect_runtime(host.clone()).await;
    let initial_bootstrap = match next_host_event(&mut events, "bootstrap").await {
        HostEvent::HostBootstrap(payload) => payload,
        _ => panic!("expected HostBootstrap"),
    };
    let initial = next_host_event(&mut events, "initial capacity replay").await;
    let HostEvent::BackendCapacity(initial) = initial else {
        panic!("expected BackendCapacity replay");
    };
    assert!(initial.snapshots.iter().any(|snapshot| matches!(
        (snapshot.backend_kind, &snapshot.state),
        (BackendKind::Codex, BackendCapacityState::Unavailable { .. })
    )));

    assert!(
        host.ingest_backend_capacity_for_test(BackendKind::Codex, codex_capacity_state(82),)
            .await
    );
    let updated = next_host_event(&mut events, "known passive capacity").await;
    let HostEvent::BackendCapacity(updated) = updated else {
        panic!("expected capacity update");
    };
    let codex = updated
        .snapshots
        .iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Codex)
        .expect("Codex snapshot");
    let BackendCapacityState::Known { report } = &codex.state else {
        panic!("Codex passive notification must produce a known report");
    };
    assert_eq!(
        report.coverage,
        protocol::CapacityCoverage::AllVendorBuckets
    );
    assert_eq!(report.buckets.len(), 3);
    assert!(matches!(
        &report.buckets[0].measure,
        protocol::CapacityMeasure::UsedPercent {
            used_percent: 82,
            remaining_percent: 18,
            provenance: protocol::ValueProvenance {
                vendor_reported: true
            },
        }
    ));
    assert!(initial_bootstrap.agents.is_empty());

    // The backend ingress is bound to this host's sender. A separate host
    // starts honestly awaiting its own first passive notification.
    let isolated_directory = tempfile::tempdir().expect("create isolated capacity tempdir");
    let isolated_host = server::spawn_host_with_mock_backend(
        isolated_directory.path().join("sessions.json"),
        isolated_directory.path().join("projects.json"),
        isolated_directory.path().join("settings.json"),
    )
    .expect("initialize isolated mock host");
    let HostEndpoint {
        events: mut isolated_events,
        ..
    } = connect_runtime(isolated_host).await;
    assert!(matches!(
        next_host_event(&mut isolated_events, "isolated bootstrap").await,
        HostEvent::HostBootstrap(_)
    ));
    assert!(
        matches!(next_host_event(&mut isolated_events, "isolated capacity replay").await,
        HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            matches!((snapshot.backend_kind, &snapshot.state),
                (BackendKind::Codex, BackendCapacityState::Unavailable {
                    reason: protocol::CapacityUnavailableReason::AwaitingFirstReport,
                })) ))
    );

    // A repeated identical report is an observation, not a duplicate state
    // change: it refreshes the stale deadline without churn on the public
    // event stream.
    host.age_backend_capacity_for_test(BackendKind::Codex, 59 * 60 * 1000)
        .await;

    // A second agent seeing the same account-wide push does not fan out a
    // duplicate event or create a second per-agent capacity snapshot.
    assert!(
        host.ingest_backend_capacity_for_test(BackendKind::Codex, codex_capacity_state(82),)
            .await
    );
    assert!(
        timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );
    let HostEndpoint {
        events: mut refreshed_events,
        ..
    } = connect_runtime(host.clone()).await;
    assert!(matches!(
        next_host_event(&mut refreshed_events, "refreshed bootstrap").await,
        HostEvent::HostBootstrap(_)
    ));
    assert!(
        matches!(next_host_event(&mut refreshed_events, "refreshed capacity replay").await,
        HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            snapshot.backend_kind == BackendKind::Codex
                && matches!(snapshot.freshness, protocol::CapacityFreshness::Fresh { age_ms } if age_ms < 60_000)))
    );

    // A later report from another agent connection replaces the account-wide
    // value; capacity is not keyed by agent or session.
    assert!(
        host.ingest_backend_capacity_for_test(BackendKind::Codex, codex_capacity_state(90),)
            .await
    );
    assert!(
        matches!(next_host_event(&mut events, "last writer capacity").await,
        HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            snapshot.backend_kind == BackendKind::Codex && matches!(&snapshot.state,
                BackendCapacityState::Known { report }
                    if matches!(&report.buckets[0].measure,
                        protocol::CapacityMeasure::UsedPercent { used_percent: 90, .. })) ))
    );

    host.age_backend_capacity_for_test(BackendKind::Codex, 120_000)
        .await;

    let HostEndpoint {
        events: mut late_events,
        ..
    } = connect_runtime(host.clone()).await;
    assert!(matches!(
        next_host_event(&mut late_events, "late bootstrap").await,
        HostEvent::HostBootstrap(_)
    ));
    let late = next_host_event(&mut late_events, "late capacity replay").await;
    assert!(
        matches!(late, HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            snapshot.backend_kind == BackendKind::Codex
            && matches!(&snapshot.state, BackendCapacityState::Known { report }
                if matches!(&report.buckets[0].measure,
                    protocol::CapacityMeasure::UsedPercent { used_percent: 90, .. }))
            && matches!(snapshot.freshness, protocol::CapacityFreshness::Fresh { age_ms } if (120_000..60 * 60 * 1000).contains(&age_ms))))
    );

    assert!(
        host.ingest_backend_capacity_for_test(BackendKind::Claude, claude_capacity_state(),)
            .await
    );
    assert!(
        matches!(next_host_event(&mut events, "Claude passive capacity").await,
        HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            snapshot.backend_kind == BackendKind::Claude
                && matches!(&snapshot.state, BackendCapacityState::Known { report }
                    if report.coverage == protocol::CapacityCoverage::RepresentativeBucketOnly
                        && report.buckets[0].label == "Fable 5 limit")))
    );

    host.mark_backend_capacity_stale_for_test(BackendKind::Codex)
        .await;
    assert!(
        matches!(next_host_event(&mut events, "stale capacity").await,
        HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            snapshot.backend_kind == BackendKind::Codex && matches!(&snapshot.state, BackendCapacityState::Stale { .. })))
    );

    // Capacity remains advisory: even a stale Codex report cannot change a
    // caller-selected backend or invent a launch profile.
    commands
        .spawn_agent(SpawnAgentPayload {
            name: Some("capacity-invariant".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root()],
                prompt: "capacity must not route this".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("capacity must not affect spawn");
    let advisory_spawn = next_new_agent_event(&mut events, "advisory spawn").await;
    assert!(
        advisory_spawn.info.backend_kind == BackendKind::Claude
            && advisory_spawn.info.launch_profile_id.is_none()
    );

    assert!(
        host.ingest_backend_capacity_for_test(
            BackendKind::Codex,
            BackendCapacityState::Unavailable {
                reason: protocol::CapacityUnavailableReason::MalformedReport
            },
        )
        .await
    );
    let malformed_capacity =
        next_backend_capacity_event(&mut events, "malformed Codex capacity").await;
    assert!(
        malformed_capacity.snapshots.iter().any(|snapshot| matches!(
            (snapshot.backend_kind, &snapshot.state),
            (
                BackendKind::Codex,
                BackendCapacityState::Unavailable {
                    reason: protocol::CapacityUnavailableReason::MalformedReport,
                }
            )
        )),
        "unexpected event before malformed Codex capacity: {malformed_capacity:?}"
    );

    // Capacity is deliberately memory-only: a restarted host replays the
    // honest awaiting-first-report state rather than a persisted quota value.
    let restarted = server::spawn_host_with_mock_backend(
        directory.path().join("sessions.json"),
        directory.path().join("projects.json"),
        directory.path().join("settings.json"),
    )
    .expect("restart mock host");
    let HostEndpoint {
        events: mut restarted_events,
        ..
    } = connect_runtime(restarted).await;
    assert!(matches!(
        next_host_event(&mut restarted_events, "restart bootstrap").await,
        HostEvent::HostBootstrap(_)
    ));
    assert!(
        matches!(next_host_event(&mut restarted_events, "restart capacity replay").await,
        HostEvent::BackendCapacity(payload) if payload.snapshots.iter().any(|snapshot|
            matches!((snapshot.backend_kind, &snapshot.state),
                (BackendKind::Codex, BackendCapacityState::Unavailable { .. })) ))
    );
}

/// A failed reading must not erase the numbers a good one produced.
///
/// Before polling existed, every failure replaced the report outright, which
/// renders as "no capacity data" — the worst answer available, because the host
/// still knows a real figure and simply stops showing it. A transient failure
/// now degrades to `Stale` carrying that report plus the error, and holds the
/// *original* collection time so a backend failing every retry reports its true
/// age instead of resetting to "just now" on each failure.
#[tokio::test]
async fn failed_capacity_reading_keeps_the_last_good_report() {
    init_tracing();
    let directory = tempfile::tempdir().expect("create capacity failure tempdir");
    let host = server::spawn_host_with_mock_backend(
        directory.path().join("sessions.json"),
        directory.path().join("projects.json"),
        directory.path().join("settings.json"),
    )
    .expect("initialize mock host");
    let HostEndpoint {
        mut events,
        commands: _,
    } = connect_runtime(host.clone()).await;
    assert!(matches!(
        next_host_event(&mut events, "bootstrap").await,
        HostEvent::HostBootstrap(_)
    ));
    assert!(matches!(
        next_host_event(&mut events, "initial capacity replay").await,
        HostEvent::BackendCapacity(_)
    ));

    assert!(
        host.ingest_backend_capacity_for_test(BackendKind::Codex, codex_capacity_state(64),)
            .await
    );
    let known = next_backend_capacity_event(&mut events, "known capacity").await;
    let known = codex_snapshot(&known);
    assert!(matches!(known.state, BackendCapacityState::Known { .. }));
    assert!(
        known.refreshable,
        "Codex has an out-of-band source, so the host must offer refresh"
    );
    let collected_at_ms = known.retrieved_at_ms;

    // Age the report so a preserved snapshot has a distinguishable, nonzero age
    // to report. A failure must not reset that age to zero.
    host.age_backend_capacity_for_test(BackendKind::Codex, 10 * 60 * 1000)
        .await;

    host.record_backend_capacity_for_test(
        BackendKind::Codex,
        BackendCapacityState::Unavailable {
            reason: protocol::CapacityUnavailableReason::SourceUnreachable,
        },
    )
    .await;
    let after_failure = next_backend_capacity_event(&mut events, "failed reading").await;
    let after_failure = codex_snapshot(&after_failure);
    let BackendCapacityState::Stale {
        report, last_error, ..
    } = &after_failure.state
    else {
        panic!(
            "a failed reading must keep the report, got {:?}",
            after_failure.state
        )
    };
    assert!(
        matches!(
            &report.buckets[0].measure,
            protocol::CapacityMeasure::UsedPercent {
                used_percent: 64,
                ..
            }
        ),
        "the preserved report must be the last good one: {:?}",
        report.buckets[0].measure
    );
    let detail = last_error
        .as_ref()
        .expect("a preserved report must say why refreshing failed");
    assert_eq!(detail.code, protocol::CapacityErrorCode::SourceUnreachable);
    assert!(
        matches!(
            after_failure.freshness,
            protocol::CapacityFreshness::Stale { age_ms, .. } if age_ms >= 10 * 60 * 1000
        ),
        "a failure must not reset the report's age: {:?}",
        after_failure.freshness
    );
    assert!(
        after_failure.retrieved_at_ms < collected_at_ms,
        "the preserved report keeps its original collection time"
    );

    // A malformed answer is not absorbed: the source produced something Tyde
    // cannot interpret, so no figure is shown rather than an older one dressed
    // up as a current reading.
    host.record_backend_capacity_for_test(
        BackendKind::Codex,
        BackendCapacityState::Unavailable {
            reason: protocol::CapacityUnavailableReason::MalformedReport,
        },
    )
    .await;
    let malformed = next_backend_capacity_event(&mut events, "malformed reading").await;
    assert!(
        matches!(
            codex_snapshot(&malformed).state,
            BackendCapacityState::Unavailable {
                reason: protocol::CapacityUnavailableReason::MalformedReport
            }
        ),
        "a malformed answer must replace the report, not hide behind it"
    );
}

/// A backend with no out-of-band source must not advertise a refresh action,
/// and asking for one anyway must be refused rather than silently ignored.
#[tokio::test]
async fn capacity_refresh_is_offered_only_where_it_can_work() {
    init_tracing();
    let directory = tempfile::tempdir().expect("create capacity refresh tempdir");
    let host = server::spawn_host_with_mock_backend(
        directory.path().join("sessions.json"),
        directory.path().join("projects.json"),
        directory.path().join("settings.json"),
    )
    .expect("initialize mock host");
    let HostEndpoint {
        mut events,
        commands,
    } = connect_runtime(host.clone()).await;
    assert!(matches!(
        next_host_event(&mut events, "bootstrap").await,
        HostEvent::HostBootstrap(_)
    ));
    let replay = match next_host_event(&mut events, "initial capacity replay").await {
        HostEvent::BackendCapacity(payload) => payload,
        _ => panic!("expected BackendCapacity replay"),
    };

    for snapshot in &replay.snapshots {
        let expected = matches!(
            snapshot.backend_kind,
            BackendKind::Claude
                | BackendKind::Codex
                | BackendKind::Kiro
                | BackendKind::Antigravity
                | BackendKind::Grok
        );
        assert_eq!(
            snapshot.refreshable, expected,
            "{:?} advertises the wrong refresh affordance",
            snapshot.backend_kind
        );
        // A backend that reports capacity must never seed as "no source": that
        // renders as a permanent "this backend cannot report quota" for a
        // backend that simply has not been read yet.
        if expected {
            assert!(
                !matches!(
                    snapshot.state,
                    BackendCapacityState::Unsupported {
                        reason: protocol::CapacityUnsupportedReason::BackendHasNoCapacitySource
                    }
                ),
                "{:?} reports capacity but seeded as having no source",
                snapshot.backend_kind
            );
        }
    }

    commands
        .backend_capacity_refresh(protocol::BackendCapacityRefreshPayload {
            backend: BackendKind::Hermes,
        })
        .await
        .expect("the request itself is well-formed and reaches the server");
    let error = timeout(Duration::from_secs(5), async {
        loop {
            if let HostEvent::CommandError(error) = events.recv().await.expect("host event stream")
            {
                return error;
            }
        }
    })
    .await
    .expect("refreshing a backend with no out-of-band source must be refused");
    assert!(
        error.message.contains("Hermes"),
        "the refusal must name the backend it refused: {}",
        error.message
    );
}

fn codex_snapshot(payload: &protocol::BackendCapacityPayload) -> protocol::BackendCapacitySnapshot {
    payload
        .snapshots
        .iter()
        .find(|snapshot| snapshot.backend_kind == BackendKind::Codex)
        .expect("Codex snapshot")
        .clone()
}

#[tokio::test]
async fn runtime_accepts_backend_config_schema_catalog() {
    init_tracing();

    let session_store_dir = tempfile::tempdir().expect("create session tempdir");
    let host = server::spawn_host_with_mock_backend(
        session_store_dir.path().join("sessions.json"),
        session_store_dir.path().join("projects.json"),
        session_store_dir.path().join("settings.json"),
    )
    .expect("initialize host with mock backend");

    let HostEndpoint {
        mut events,
        commands: _,
    } = connect_runtime(host).await;
    match next_host_event(&mut events, "initial host bootstrap").await {
        HostEvent::HostBootstrap(payload) => {
            // The runtime must accept a bootstrap whose deep-config schema
            // catalog is empty — the current state now that Hermes manages
            // its real config through backend-native settings instead.
            assert!(
                payload.backend_config_schemas.is_empty(),
                "unexpected deep-config schemas: {:?}",
                payload.backend_config_schemas
            );
        }
        _ => panic!("expected initial HostBootstrap"),
    }
}

#[tokio::test]
async fn split_endpoints_allow_event_loops_and_commands_to_run_independently() {
    init_tracing();

    let session_store_dir = tempfile::tempdir().expect("create session tempdir");
    let host = server::spawn_host_with_mock_backend(
        session_store_dir.path().join("sessions.json"),
        session_store_dir.path().join("projects.json"),
        session_store_dir.path().join("settings.json"),
    )
    .expect("initialize host with mock backend");

    let host_endpoint = connect_runtime(host).await;
    let HostEndpoint {
        mut events,
        commands,
    } = host_endpoint;

    match next_host_event(&mut events, "initial host bootstrap").await {
        HostEvent::HostBootstrap(payload) => {
            assert!(payload.sessions.is_empty());
            assert!(payload.projects.is_empty());
        }
        _ => panic!("expected initial HostBootstrap"),
    }

    let (session_list_tx, session_list_rx) = oneshot::channel();
    let (new_agent_tx, new_agent_rx) = oneshot::channel();
    let (session_count_tx, mut session_count_rx) = mpsc::channel(2);
    let (host_loop_stop_tx, mut host_loop_stop_rx) = oneshot::channel();
    let (host_loop_result_tx, host_loop_result_rx) = oneshot::channel();

    tokio::spawn(async move {
        let mut session_list_tx = Some(session_list_tx);
        let mut new_agent_tx = Some(new_agent_tx);
        let mut expected_session_id = None;
        let mut saw_second_count = false;

        loop {
            let event = tokio::select! {
                _ = &mut host_loop_stop_rx => break,
                event = events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    event
                }
            };
            match event {
                HostEvent::SessionList(payload) => {
                    if let Some(tx) = session_list_tx.take() {
                        let _ = tx.send(payload);
                    }
                }
                HostEvent::NewAgent(agent) => {
                    expected_session_id = agent.info.session_id.clone();
                    if let Some(tx) = new_agent_tx.take() {
                        let _ = tx.send(agent);
                    }
                }
                HostEvent::SessionSummaryCountUpdated(payload) => {
                    saw_second_count |= expected_session_id.as_ref() == Some(&payload.session_id)
                        && payload.assistant_turn_count >= 2;
                    let _ = session_count_tx.send(payload).await;
                }
                HostEvent::HostSettings(_)
                | HostEvent::SettingsWriteResult(_)
                | HostEvent::AgentActivitySummary(_)
                | HostEvent::TaskTokenUsage(_)
                | HostEvent::AgentsViewPreferencesNotify(_)
                | HostEvent::HostBootstrap(_)
                | HostEvent::BackendSetup(_)
                | HostEvent::BackendConfigSchemas(_)
                | HostEvent::BackendConfigSnapshots(_)
                | HostEvent::BackendCapacity(_)
                | HostEvent::AgentClosed(_)
                | HostEvent::ProjectNotify(_)
                | HostEvent::NewTerminal(_)
                | HostEvent::SessionSchemas(_)
                | HostEvent::LaunchProfileCatalogNotify(_)
                | HostEvent::CommandError(_)
                | HostEvent::CustomAgentNotify(_)
                | HostEvent::SteeringNotify(_)
                | HostEvent::SkillNotify(_)
                | HostEvent::McpServerNotify(_)
                | HostEvent::WorkflowNotify(_)
                | HostEvent::WorkflowRunNotify(_)
                | HostEvent::MobileAccessState(_)
                | HostEvent::MobilePairingOffer(_)
                | HostEvent::TeamNotify(_)
                | HostEvent::TeamMemberNotify(_)
                | HostEvent::TeamMemberBindingNotify(_)
                | HostEvent::TeamPresetCatalogNotify(_)
                | HostEvent::TeamDraftNotify(_)
                | HostEvent::TeamContextCompactionNotify(_)
                | HostEvent::TeamMemberShuffleSuggestionNotify(_)
                | HostEvent::Voice(_) => {}
            }
        }
        let _ = host_loop_result_tx.send(saw_second_count);
    });

    commands
        .list_sessions()
        .await
        .expect("list_sessions command should succeed");
    let sessions = timeout(Duration::from_secs(5), session_list_rx)
        .await
        .expect("timed out waiting for SessionList")
        .expect("host event loop dropped before SessionList");
    assert!(
        sessions.sessions.is_empty(),
        "mock host should start with no resumable sessions"
    );

    let prompt = "runtime split test";
    commands
        .spawn_agent(SpawnAgentPayload {
            name: Some("split-runtime".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent command should succeed");

    let agent = timeout(Duration::from_secs(5), new_agent_rx)
        .await
        .expect("timed out waiting for NewAgent")
        .expect("host event loop dropped before NewAgent");
    assert_eq!(agent.info.name, "split-runtime");
    let session_id = agent.info.session_id.clone().expect("agent session id");

    let AgentEndpoint {
        mut events,
        commands,
        ..
    } = agent;

    let (probe_tx, mut probe_rx) = mpsc::channel::<AgentProbe>(16);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::Bootstrap(payload) => {
                    for event in payload.events {
                        match event {
                            AgentBootstrapEvent::AgentStart(payload) => {
                                if probe_tx
                                    .send(AgentProbe::Started(payload.name))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            AgentBootstrapEvent::ChatEvent(payload) => {
                                if send_chat_probe(&probe_tx, payload).await.is_err() {
                                    return;
                                }
                            }
                            AgentBootstrapEvent::AgentError(_)
                            | AgentBootstrapEvent::SessionSettings(_)
                            | AgentBootstrapEvent::QueuedMessages(_)
                            | AgentBootstrapEvent::AgentActivityStats(_)
                            | AgentBootstrapEvent::ContextCompaction(_)
                            | AgentBootstrapEvent::ContextCompactionCapability(_)
                            | AgentBootstrapEvent::HasPriorHistory { .. } => {}
                        }
                    }
                }
                AgentEvent::Start(payload) => {
                    if probe_tx
                        .send(AgentProbe::Started(payload.name))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                AgentEvent::Chat(payload) => {
                    if send_chat_probe(&probe_tx, *payload).await.is_err() {
                        break;
                    }
                }
                AgentEvent::Renamed(_)
                | AgentEvent::SessionSettings(_)
                | AgentEvent::QueuedMessages(_)
                | AgentEvent::SessionHistory(_)
                | AgentEvent::ContextCompactionNotify(_)
                | AgentEvent::ContextCompactionCapability(_)
                | AgentEvent::ActivityStats(_) => {}
                AgentEvent::Error(err) => panic!("unexpected agent error: {}", err.message),
            }
        }
    });

    match next_agent_probe(&mut probe_rx, "AgentStart").await {
        AgentProbe::Started(name) => assert_eq!(name, "split-runtime"),
        other => panic!("expected AgentStart probe, got {other:?}"),
    }
    match next_agent_probe(&mut probe_rx, "initial stream end").await {
        AgentProbe::Final(content) => {
            assert!(
                content.ends_with(&format!("mock backend response to: {prompt}")),
                "expected mock response suffix, got: {content}"
            );
        }
        other => panic!("expected initial final response, got {other:?}"),
    }
    let first_count =
        next_session_count(&mut session_count_rx, &session_id, "initial session count").await;
    assert_eq!(first_count.assistant_turn_count, 1);
    assert!(first_count.updated_at_ms > 0);

    let follow_up = "follow-up after background event loop";
    commands
        .send_message(SendMessagePayload {
            message: follow_up.to_owned(),
            images: None,
            origin: None,
            tool_response: None,
        })
        .await
        .expect("follow-up send should succeed");

    match next_agent_probe(&mut probe_rx, "follow-up stream end").await {
        AgentProbe::Final(content) => {
            assert!(
                content.ends_with(&format!("mock backend response to: {follow_up}")),
                "expected mock response suffix, got: {content}"
            );
        }
        other => panic!("expected follow-up final response, got {other:?}"),
    }
    let second_count = next_session_count(
        &mut session_count_rx,
        &session_id,
        "follow-up session count",
    )
    .await;
    assert_eq!(second_count.assistant_turn_count, 2);
    assert!(second_count.updated_at_ms >= first_count.updated_at_ms);
    host_loop_stop_tx
        .send(())
        .expect("host event loop must remain active through count assertions");
    let saw_second_count = timeout(Duration::from_secs(5), host_loop_result_rx)
        .await
        .expect("timed out stopping host event loop")
        .expect("host event loop dropped its accumulated count result");
    assert!(
        saw_second_count,
        "host event loop must accumulate the matching session's second count"
    );
}

#[tokio::test]
async fn runtime_preserves_project_bootstrap_until_project_endpoint_is_opened() {
    init_tracing();

    let session_store_dir = tempfile::tempdir().expect("create session tempdir");
    let project_root = tempfile::tempdir().expect("create project root");
    let project_path = session_store_dir.path().join("projects.json");
    let project = server::store::project::ProjectStore::load(project_path.clone())
        .expect("load project store")
        .create(
            "runtime-bootstrap-project".to_owned(),
            vec![ProjectRootPath(
                project_root.path().to_string_lossy().to_string(),
            )],
        )
        .expect("create project");
    let host = server::spawn_host_with_mock_backend(
        session_store_dir.path().join("sessions.json"),
        project_path,
        session_store_dir.path().join("settings.json"),
    )
    .expect("initialize host with mock backend");

    let HostEndpoint {
        mut events,
        commands,
    } = connect_runtime(host).await;

    match next_host_event(&mut events, "initial host bootstrap").await {
        HostEvent::HostBootstrap(payload) => {
            assert!(
                payload.projects.iter().any(|item| item.id == project.id),
                "HostBootstrap should include the persisted project"
            );
        }
        _ => panic!("expected initial HostBootstrap"),
    }

    let mut project_endpoint = commands
        .open_project(project.id.clone())
        .await
        .expect("bootstrapped project endpoint should be available");
    match timeout(Duration::from_secs(5), project_endpoint.events.recv())
        .await
        .expect("timed out waiting for project bootstrap")
        .expect("project event stream closed before bootstrap")
    {
        ProjectEvent::Bootstrap(payload) => {
            assert_eq!(payload.project.id, project.id);
            assert_eq!(payload.project.name, project.name);
            assert_eq!(payload.review_summaries.len(), 1);
            assert_eq!(
                payload.review_summaries[0].scope,
                ReviewSummaryScope::Workspace
            );
        }
        _ => panic!("expected ProjectBootstrap"),
    }
}

async fn send_chat_probe(
    tx: &mpsc::Sender<AgentProbe>,
    event: ChatEvent,
) -> Result<(), mpsc::error::SendError<AgentProbe>> {
    match event {
        ChatEvent::StreamEnd(data) => tx.send(AgentProbe::Final(data.message.content)).await,
        _ => Ok(()),
    }
}

async fn connect_runtime(host: server::HostHandle) -> HostEndpoint {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    let client_config = client::ClientConfig::current();

    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("server handshake failed");
        if let Err(err) = server::run_connection(conn, host).await {
            eprintln!("server connection loop failed: {err:?}");
        }
    });

    client::connect_host_endpoint(&client_config, client_stream)
        .await
        .expect("runtime handshake failed")
}

async fn next_host_event(events: &mut client::HostEvents, context: &str) -> HostEvent {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = events
                .recv()
                .await
                .unwrap_or_else(|| panic!("host event stream closed while waiting for {context}"));
            if !matches!(event, HostEvent::Voice(_)) {
                return event;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for host event: {context}"))
}

async fn next_session_count(
    rx: &mut mpsc::Receiver<protocol::SessionSummaryCountUpdatedPayload>,
    session_id: &protocol::SessionId,
    context: &str,
) -> protocol::SessionSummaryCountUpdatedPayload {
    timeout(Duration::from_secs(5), async {
        loop {
            let payload = rx
                .recv()
                .await
                .unwrap_or_else(|| panic!("host event loop closed while waiting for {context}"));
            if &payload.session_id == session_id {
                return payload;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
}

async fn next_backend_capacity_event(
    events: &mut client::HostEvents,
    context: &str,
) -> protocol::BackendCapacityPayload {
    loop {
        match next_host_event(events, context).await {
            HostEvent::BackendCapacity(payload) => return payload,
            HostEvent::AgentsViewPreferencesNotify(_)
            | HostEvent::TaskTokenUsage(_)
            | HostEvent::SessionList(_) => {}
            event => panic!(
                "expected BackendCapacity before {context}; got {:?}",
                std::mem::discriminant(&event)
            ),
        }
    }
}

async fn next_new_agent_event(
    events: &mut client::HostEvents,
    context: &str,
) -> client::AgentEndpoint {
    loop {
        match next_host_event(events, context).await {
            HostEvent::NewAgent(agent) => return agent,
            HostEvent::AgentsViewPreferencesNotify(_) | HostEvent::TaskTokenUsage(_) => {}
            event => panic!(
                "expected NewAgent before {context}; got {:?}",
                std::mem::discriminant(&event)
            ),
        }
    }
}

async fn next_agent_probe(rx: &mut mpsc::Receiver<AgentProbe>, context: &str) -> AgentProbe {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for agent probe: {context}"))
        .unwrap_or_else(|| panic!("agent probe channel closed while waiting for {context}"))
}

fn workspace_root() -> String {
    PathBuf::from(".")
        .canonicalize()
        .expect("canonicalize workspace root")
        .to_string_lossy()
        .into_owned()
}
