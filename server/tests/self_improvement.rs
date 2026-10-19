mod fixture;
use fixture::{Fixture, TestAgent, expect_settings_write_applied, next_frame_matching_on};
use protocol::*;
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};

fn config(mode: LearningMode) -> LearningConfig {
    LearningConfig {
        enabled: true,
        mode,
        analyzer: Some(LearningAnalyzer {
            backend_kind: BackendKind::Codex,
            model: "test-model".into(),
        }),
        ..Default::default()
    }
}
fn steering(content: &str) -> LearningRule {
    LearningRule::Steering {
        steering: Steering {
            id: SteeringId("learned-format".into()),
            scope: SteeringScope::Host,
            title: "Readable changes".into(),
            content: content.into(),
        },
    }
}
fn aspect(instructions: &str) -> LearningRule {
    LearningRule::ReviewAspect {
        id: "learned-defect".into(),
        scope: SteeringScope::Host,
        aspect: ReviewAspectConfig {
            name: "Navigation state".into(),
            description: "Preserve user state".into(),
            instructions: instructions.into(),
            enabled: true,
        },
    }
}
fn operation(before: Option<LearningRule>, after: LearningRule, input: &str) -> LearningOperation {
    LearningOperation {
        before,
        after,
        reason: "Explicit durable human feedback".into(),
        origin_id: HumanInputId(input.into()),
    }
}
fn response(operations: Vec<LearningOperation>) -> String {
    serde_json::to_string(&LearningAnalysis { operations }).expect("encode analyzer response")
}
async fn save(fixture: &mut Fixture, before: LearningConfig, after: LearningConfig) {
    let id = fixture
        .client
        .replace_setting("/self_improvement/host", after, before)
        .await
        .expect("write learning settings");
    expect_settings_write_applied(&mut fixture.client, &id, "persist learning settings").await;
}
async fn human(
    fixture: &mut Fixture,
    agent: &TestAgent,
    id: &str,
    action: HumanInputAction,
    message: &str,
) {
    fixture
        .client
        .human_input(
            &agent.stream,
            HumanInputPayload {
                id: HumanInputId(id.into()),
                action,
                input: SendMessagePayload {
                    message: message.into(),
                    images: None,
                    origin: None,
                    tool_response: None,
                },
            },
        )
        .await
        .expect("send real human protocol input");
}
fn assert_healthy_learning(state: &SelfImprovementState) {
    for scope in &state.scopes {
        assert!(
            !matches!(scope.status, LearningStatus::Failed { .. }),
            "Unexpected analyzer failure before the protocol flow completed: {:?}",
            scope.status
        );
    }
}
async fn state(
    fixture: &mut Fixture,
    check: impl Fn(&SelfImprovementState) -> bool,
) -> SelfImprovementState {
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_healthy_learning(&bootstrap.self_improvement);
    if check(&bootstrap.self_improvement) {
        return bootstrap.self_improvement;
    }
    next_frame_matching_on(&mut observer, "self-improvement state", |frame| {
        frame.kind == FrameKind::SelfImprovementNotify
            && frame
                .parse_payload::<SelfImprovementState>()
                .is_ok_and(|state| {
                    assert_healthy_learning(&state);
                    check(&state)
                })
    })
    .await
    .parse_payload()
    .expect("parse learning state")
}
async fn canonical_steering(fixture: &mut Fixture, content: &str) {
    next_frame_matching_on(&mut fixture.client, "canonical steering write", |frame| frame.kind == FrameKind::SteeringNotify && frame.parse_payload::<SteeringNotifyPayload>().is_ok_and(|notify| matches!(notify, SteeringNotifyPayload::Upsert { steering } if steering.content == content))).await;
}

#[tokio::test]
async fn busy_human_inputs_suggest_apply_automatic_edit_and_conflicting_undo() {
    let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
    ])
    .await;
    let main_gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "Author",
            MockScript::one(MockTurn::gated_text("Main author finished", &main_gate))
                .with_mid_turn_steering()
                .with_unbounded_echo(),
        )
        .await;
    main_gate.wait_until_entered().await;
    let analyzer_gate = MockGateHandle::new();
    fixture
        .host_for_test()
        .set_learning_script_for_test(
            MockScript::one(MockTurn::gated_text(
                response(vec![
                    operation(
                        None,
                        steering("Keep changes focused and explain the user-visible result."),
                        "human-one",
                    ),
                    operation(
                        None,
                        aspect("Check that navigation preserves drafts and scroll position."),
                        "human-one",
                    ),
                ]),
                &analyzer_gate,
            ))
            .then(MockTurn::text(response(Vec::new()))),
        )
        .expect("install analyzer at backend boundary");
    save(
        &mut fixture,
        LearningConfig::default(),
        config(LearningMode::Suggest),
    )
    .await;
    human(&mut fixture, &agent, "human-one", HumanInputAction::Send, "For future work, keep changes focused and check that navigation preserves drafts and scroll position.").await;
    // This event cannot arrive if analysis incorrectly waits for author Idle:
    // the author's explicit completion gate remains closed throughout.
    analyzer_gate.wait_until_entered().await;
    human(
        &mut fixture,
        &agent,
        "human-two",
        HumanInputAction::Steer,
        "Looks bad; cut this beta after the current task.",
    )
    .await;
    let queued = state(&mut fixture, |state| {
        state
            .scopes
            .iter()
            .any(|scope| scope.scope == SteeringScope::Host && scope.queued == 1)
    })
    .await;
    assert!(
        queued.changes.is_empty(),
        "Suggest must not write before analysis completes"
    );
    // A replay with the same accepted identity is not new human input.
    human(
        &mut fixture,
        &agent,
        "human-two",
        HumanInputAction::Steer,
        "Looks bad; cut this beta after the current task.",
    )
    .await;
    analyzer_gate.release_one();
    let suggested = state(&mut fixture, |state| {
        state.changes.len() == 2 && state.usage.len() == 2
    })
    .await;
    assert!(
        suggested
            .changes
            .iter()
            .all(|change| matches!(change.status, LearningChangeStatus::Pending)),
        "Suggest only creates pending proposals"
    );
    assert_eq!(
        suggested
            .usage
            .iter()
            .map(|usage| usage.input_ids.len())
            .sum::<usize>(),
        2,
        "Each accepted identity is analyzed once"
    );
    let reconnect = fixture.connect_with_bootstrap().await;
    assert!(
        reconnect.1.steering.is_empty(),
        "Suggest cannot alter canonical steering"
    );
    assert!(
        reconnect.1.settings.review.aspects.is_empty(),
        "Suggest cannot alter canonical aspects"
    );
    for change in &suggested.changes {
        fixture
            .client
            .self_improvement_action(SelfImprovementAction::Apply {
                id: change.id.clone(),
            })
            .await
            .expect("apply proposal");
    }
    let applied = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .filter(|change| matches!(change.status, LearningChangeStatus::Applied))
            .count()
            == 2
    })
    .await;
    assert!(
        applied
            .changes
            .iter()
            .all(|change| !change.origin_message.is_empty())
    );
    fixture.host_for_test().set_learning_script_for_test(MockScript::one(MockTurn::text(response(vec![
        operation(Some(steering("Keep changes focused and explain the user-visible result.")), steering("Keep changes focused; explain the user-visible result and remaining limitations."), "human-three"),
        operation(Some(aspect("Check that navigation preserves drafts and scroll position.")), aspect("Check that navigation preserves drafts, scroll position, and the active selection."), "human-three"),
    ])))).expect("install automatic analyzer");
    save(
        &mut fixture,
        config(LearningMode::Suggest),
        config(LearningMode::Automatic),
    )
    .await;
    human(&mut fixture, &agent, "human-three", HumanInputAction::Send, "For future work also explain limitations and check that navigation preserves the active selection.").await;
    let automatic = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .filter(|change| matches!(change.status, LearningChangeStatus::Applied))
            .count()
            == 4
    })
    .await;
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        bootstrap.steering.len(),
        1,
        "Automatic must edit the same canonical rule instead of duplicating it"
    );
    assert_eq!(bootstrap.settings.review.aspects.len(), 1);
    assert!(
        bootstrap.settings.review.aspects["learned-defect"]
            .instructions
            .contains("active selection")
    );
    let mut human_owned = bootstrap.steering[0].clone();
    human_owned.content = "Human-owned instruction must remain intact.".into();
    observer
        .steering_upsert(SteeringUpsertPayload {
            steering: human_owned,
        })
        .await
        .expect("human canonical edit");
    let updated = automatic
        .changes
        .iter()
        .find(|change| {
            matches!(change.operation.after, LearningRule::Steering { .. })
                && change.operation.before.is_some()
        })
        .expect("automatic steering edit");
    // Wait for the human write itself, not an elapsed delay, before Undo.
    canonical_steering(&mut fixture, "Human-owned instruction must remain intact.").await;
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Undo {
            id: updated.id.clone(),
        })
        .await
        .expect("request conflicting undo");
    let conflict = state(&mut fixture, |state| {
        state.changes.iter().any(|change| {
            change.id == updated.id
                && matches!(change.status, LearningChangeStatus::Conflict { .. })
        })
    })
    .await;
    assert!(conflict.error.is_some());
    let persisted = fixture.connect_with_bootstrap().await.1;
    assert_eq!(
        persisted.steering[0].content,
        "Human-owned instruction must remain intact."
    );
    main_gate.release_one();
    fixture
        .next_chat_event_matching(&agent, "author completion before host restart", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    let before_restart = fixture.connect_with_bootstrap().await.1.self_improvement;
    let restored = fixture.restart_host().await;
    assert_eq!(
        restored.self_improvement.changes, before_restart.changes,
        "Restart must retain the same audit identities and applied revisions, not apply them twice"
    );
    assert_eq!(
        restored.self_improvement.usage, before_restart.usage,
        "Restart must not schedule another batch for accepted feedback"
    );
    assert_eq!(
        restored.steering[0].content,
        "Human-owned instruction must remain intact."
    );
}

#[tokio::test]
async fn disabled_unsupported_replay_and_restart_do_not_schedule_or_apply() {
    let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
    ])
    .await;
    let agent = fixture.spawn("Author", "One-off task").await;
    fixture.finish_turn(&agent).await;
    human(
        &mut fixture,
        &agent,
        "disabled-input",
        HumanInputAction::Send,
        "Always retain my explicit constraints.",
    )
    .await;
    fixture.finish_turn(&agent).await;
    let mut unsupported = config(LearningMode::Automatic);
    unsupported
        .analyzer
        .as_mut()
        .expect("selected model")
        .backend_kind = BackendKind::Muse;
    save(&mut fixture, LearningConfig::default(), unsupported.clone()).await;
    human(
        &mut fixture,
        &agent,
        "unsupported-input",
        HumanInputAction::Send,
        "Always retain my explicit constraints.",
    )
    .await;
    fixture.finish_turn(&agent).await;
    let snapshot = state(&mut fixture, |state| {
        state
            .scopes
            .iter()
            .any(|scope| matches!(scope.status, LearningStatus::Unsupported { .. }))
    })
    .await;
    assert!(snapshot.usage.is_empty());
    assert!(snapshot.changes.is_empty());
    let bootstrap = fixture.restart_host().await;
    assert_eq!(bootstrap.settings.self_improvement.host, unsupported);
    let restored = state(&mut fixture, |state| {
        state
            .scopes
            .iter()
            .any(|scope| matches!(scope.status, LearningStatus::Unsupported { .. }))
    })
    .await;
    assert!(restored.usage.is_empty());
    assert!(restored.changes.is_empty());
}

#[tokio::test]
async fn rejected_author_input_does_not_consume_identity_or_start_learning() {
    let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
    ])
    .await;
    let failed_gate = MockGateHandle::new();
    let rejected = fixture
        .spawn_scripted(
            "Rejected author",
            MockScript::one(MockTurn::busy_then_close_stream(&failed_gate)),
        )
        .await;
    failed_gate.wait_until_entered().await;
    failed_gate.release_one();
    next_frame_matching_on(&mut fixture.client, "parked author failure", |frame| {
        frame.stream == rejected.stream
            && frame.kind == FrameKind::AgentError
            && frame
                .parse_payload::<AgentErrorPayload>()
                .is_ok_and(|error| error.fatal)
    })
    .await;
    let live_gate = MockGateHandle::new();
    let live = fixture
        .spawn_scripted(
            "Live author",
            MockScript::one(MockTurn::gated_text("Author completed", &live_gate))
                .with_unbounded_echo(),
        )
        .await;
    live_gate.wait_until_entered().await;
    let analyzer_gate = MockGateHandle::new();
    fixture
        .host_for_test()
        .set_learning_script_for_test(MockScript::one(MockTurn::gated_text(
            response(vec![operation(
                None,
                steering("Keep future work focused on the requested change."),
                "retry-after-rejection",
            )]),
            &analyzer_gate,
        )))
        .expect("install analyzer");
    save(
        &mut fixture,
        LearningConfig::default(),
        config(LearningMode::Automatic),
    )
    .await;
    human(
        &mut fixture,
        &rejected,
        "retry-after-rejection",
        HumanInputAction::Send,
        "For future work, keep changes focused.",
    )
    .await;
    next_frame_matching_on(
        &mut fixture.client,
        "human author admission rejection",
        |frame| {
            frame.stream == rejected.stream
                && frame.kind == FrameKind::AgentError
                && frame
                    .parse_payload::<AgentErrorPayload>()
                    .is_ok_and(|error| !error.fatal)
        },
    )
    .await;
    human(
        &mut fixture,
        &live,
        "retry-after-rejection",
        HumanInputAction::Send,
        "For future work, keep changes focused.",
    )
    .await;
    analyzer_gate.wait_until_entered().await;
    analyzer_gate.release_one();
    let result = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|change| matches!(change.status, LearningChangeStatus::Applied))
    })
    .await;
    assert_eq!(result.changes.len(), 1);
    assert!(
        result
            .changes
            .iter()
            .all(|change| change.origin_agent_id == live.new_agent.agent_id),
        "Only the author actor that actually accepted the input may originate learning"
    );
    assert_eq!(
        result
            .usage
            .iter()
            .map(|usage| usage.input_ids.len())
            .sum::<usize>(),
        1
    );
    live_gate.release_one();
    fixture
        .next_chat_event_matching(
            &live,
            "accepted author completes after background learning",
            |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
        )
        .await;
}

#[tokio::test]
async fn journal_failure_and_replay_preserve_author_once_and_accounting_failure_is_explicit() {
    let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
    ])
    .await;
    let main_gate = MockGateHandle::new();
    let author = fixture
        .spawn_scripted(
            "Author",
            MockScript::one(MockTurn::gated_text("Main turn completed", &main_gate))
                .then(MockTurn::text("Queued input completed"))
                .with_unbounded_echo(),
        )
        .await;
    main_gate.wait_until_entered().await;
    eprintln!("LEARNING JOURNAL phase=author_held");
    let analyzer_gate = MockGateHandle::new();
    fixture
        .host_for_test()
        .set_learning_script_for_test(
            MockScript::one(MockTurn::gated_text(response(Vec::new()), &analyzer_gate))
                .then(MockTurn::text(response(vec![operation(
                    None,
                    steering("Keep future changes focused."),
                    "projection-failure",
                )])))
                .then(MockTurn::text(response(Vec::new()))),
        )
        .expect("install analyzer");
    save(
        &mut fixture,
        LearningConfig::default(),
        config(LearningMode::Suggest),
    )
    .await;
    // Exercise the real persistence boundary: creating the transaction file
    // must fail, while the last committed journal remains untouched.
    let transaction = fixture
        .session_store_path()
        .with_file_name("self-improvement.json.tmp");
    std::fs::create_dir(&transaction).expect("make journal transaction unwritable as a file");
    human(
        &mut fixture,
        &author,
        "persist-and-replay",
        HumanInputAction::Send,
        "One-off task, not a durable instruction.",
    )
    .await;
    let failed = state(&mut fixture, |state| {
        state
            .error
            .as_ref()
            .is_some_and(|error| error.contains("could not be journalled"))
    })
    .await;
    eprintln!("LEARNING JOURNAL phase=persistence_failure_observed");
    assert!(failed.usage.is_empty());
    assert!(failed.changes.is_empty());
    std::fs::remove_dir(&transaction).expect("restore journal transaction path");
    human(
        &mut fixture,
        &author,
        "persist-and-replay",
        HumanInputAction::Send,
        "One-off task, not a durable instruction.",
    )
    .await;
    analyzer_gate.wait_until_entered().await;
    eprintln!("LEARNING JOURNAL phase=retry_analyzer_held");
    analyzer_gate.release_one();
    let recovered = state(&mut fixture, |state| {
        state.usage.len() == 1 && state.error.is_none()
    })
    .await;
    assert_eq!(
        recovered.usage[0].input_ids.len(),
        1,
        "Failed persistence must not consume the identity or schedule analysis"
    );
    eprintln!("LEARNING JOURNAL phase=retry_accounted");
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    let stream = bootstrap
        .agents
        .iter()
        .find(|entry| entry.agent_id == author.new_agent.agent_id)
        .expect("author descriptor")
        .instance_stream
        .clone();
    let replay: AgentBootstrapPayload =
        next_frame_matching_on(&mut observer, "author queue after replay", |frame| {
            frame.kind == FrameKind::AgentBootstrap && frame.stream == stream
        })
        .await
        .parse_payload()
        .expect("parse author replay");
    let queued = replay.events.iter().find_map(|event| match event {
        AgentBootstrapEvent::QueuedMessages(snapshot) => Some(snapshot.messages.len()),
        _ => None,
    });
    assert_eq!(
        queued,
        Some(1),
        "Retrying failed learning must not duplicate already accepted author input"
    );
    eprintln!("LEARNING JOURNAL phase=replay_has_one_queued_input");
    main_gate.release_one();
    // Settings-result waits may already have consumed the initial busy edge.
    // The queued response cannot precede gate release on this observer.
    fixture::next_chat_event_matching_on(
        &mut observer,
        &stream,
        "accepted queued input completion",
        |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "Queued input completed"),
    )
    .await;
    fixture::next_chat_event_matching_on(
        &mut observer,
        &stream,
        "author idle after queued input completion",
        |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
    )
    .await;
    eprintln!("LEARNING JOURNAL phase=queued_input_completed_and_idle");
    human(
        &mut fixture,
        &author,
        "projection-failure",
        HumanInputAction::Send,
        "For future work, keep changes focused.",
    )
    .await;
    let proposed = state(&mut fixture, |state| state.changes.len() == 1).await;
    eprintln!("LEARNING JOURNAL phase=projection_proposal_observed");
    let gate = fixture
        .host_for_test()
        .install_learning_projection_test_gate()
        .await;
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Apply {
            id: proposed.changes[0].id.clone(),
        })
        .await
        .expect("apply before canonical projection fault");
    gate.wait_until_entered().await;
    eprintln!("LEARNING JOURNAL phase=canonical_commit_projection_held");
    let settings_path = fixture.settings_store_path();
    let settings = std::fs::read(&settings_path).expect("retain committed settings");
    std::fs::write(&settings_path, "{").expect("exercise real canonical parse failure");
    gate.release_one();
    let mut projected_early = false;
    let failed: SelfImprovementState = next_frame_matching_on(
        &mut fixture.client,
        "explicit canonical projection failure",
        |frame| {
            if frame.kind == FrameKind::SteeringNotify {
                projected_early = true;
            }
            frame.kind == FrameKind::SelfImprovementNotify
                && frame
                    .parse_payload::<SelfImprovementState>()
                    .is_ok_and(|state| {
                        state.error.as_ref().is_some_and(|error| {
                            error.contains("canonical steering or settings could not be read")
                        })
                    })
        },
    )
    .await
    .parse_payload()
    .expect("parse projection failure");
    eprintln!("LEARNING JOURNAL phase=canonical_projection_failure_observed");
    assert!(!projected_early, "Read both canonical stores before fanout");
    assert_eq!(failed.canonical_revision, proposed.canonical_revision);
    assert!(matches!(
        failed.changes[0].status,
        LearningChangeStatus::Applied
    ));
    std::fs::write(&settings_path, settings).expect("restore canonical settings");
    human(
        &mut fixture,
        &author,
        "projection-recovery",
        HumanInputAction::Send,
        "One-off task, not a durable instruction.",
    )
    .await;
    canonical_steering(&mut fixture, "Keep future changes focused.").await;
    let recovered = state(&mut fixture, |state| {
        state.canonical_revision > failed.canonical_revision
            && state.error.is_none()
            && state.usage.len() == 3
    })
    .await;
    eprintln!("LEARNING JOURNAL phase=canonical_projection_recovered");
    assert_eq!(recovered.changes.len(), 1, "Reprojection must not reapply");
    fixture.host_for_test().stop_learning_for_test().await;
    eprintln!("LEARNING JOURNAL phase=accounting_actor_stopped");
    let incomplete = fixture.connect_with_bootstrap().await.1;
    eprintln!("LEARNING JOURNAL phase=incomplete_accounting_bootstrapped");
    assert!(!incomplete.task_token_usages.is_empty());
    assert!(
        incomplete
            .task_token_usages
            .iter()
            .all(|usage| match &usage.total.status {
                TaskTokenUsageStatus::Partial { reasons, .. }
                | TaskTokenUsageStatus::Unavailable { reasons, .. } =>
                    reasons.contains(&TaskTokenUsageUnavailableReason::HelperAccountingUnavailable),
                TaskTokenUsageStatus::Known => false,
            }),
        "An unavailable analyzer accounting source must never be published as a complete author-only total"
    );
}

#[tokio::test]
async fn project_scope_reconfiguration_undo_and_human_ownership_are_enforced() {
    let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
    ])
    .await;
    let root = tempfile::tempdir().expect("project directory");
    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Learning scope".into(),
            roots: vec![ProjectRootPath(root.path().to_string_lossy().into_owned())],
        })
        .await
        .expect("create project");
    let project: ProjectNotifyPayload =
        next_frame_matching_on(&mut fixture.client, "project created", |frame| {
            frame.kind == FrameKind::ProjectNotify
        })
        .await
        .parse_payload()
        .expect("project notify");
    let ProjectNotifyPayload::Upsert { project } = project else {
        panic!("expected created project");
    };
    let reservation = fixture
        .reserve_next_mock_launch("Scoped author", MockScript::new().with_unbounded_echo())
        .await;
    let (author, _) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: AgentLaunchContext::Standard,
            name: Some("Scoped author".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec![root.path().to_string_lossy().into_owned()],
                prompt: "One-off initial task".into(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    drop(reservation);
    fixture.finish_turn(&author).await;
    let scope = SteeringScope::Project(project.id.clone());
    let scoped_rule = |text: &str| LearningRule::Steering {
        steering: Steering {
            id: SteeringId("project-learned".into()),
            scope: scope.clone(),
            title: "Project feedback".into(),
            content: text.into(),
        },
    };
    let shutdown = MockGateHandle::new();
    fixture
        .host_for_test()
        .set_learning_script_for_test(
            MockScript::one(MockTurn::text(response(Vec::new()))).with_shutdown_gate(&shutdown),
        )
        .expect("install no-op analyzer");
    let suggest = config(LearningMode::Suggest);
    let write = fixture
        .client
        .replace_setting(
            "/self_improvement/projects",
            std::collections::BTreeMap::from([(project.id.clone(), suggest.clone())]),
            std::collections::BTreeMap::<ProjectId, LearningConfig>::new(),
        )
        .await
        .expect("enable project only");
    expect_settings_write_applied(&mut fixture.client, &write, "project config").await;
    human(
        &mut fixture,
        &author,
        "project-noop",
        HumanInputAction::Send,
        "Looks bad; cut this beta once.",
    )
    .await;
    let noop = state(&mut fixture, |state| state.usage.len() == 1).await;
    assert!(noop.changes.is_empty());
    assert_eq!(noop.usage[0].scope, scope);
    assert!(
        !fixture
            .connect_with_bootstrap()
            .await
            .1
            .settings
            .self_improvement
            .host
            .enabled
    );
    // Nonhuman follow-ups and their assistant output cannot create a pass.
    fixture
        .client
        .send_message_payload(
            &author.stream,
            SendMessagePayload {
                message: "Internal instruction".into(),
                images: None,
                origin: Some(MessageOrigin::Supervisor),
                tool_response: None,
            },
        )
        .await
        .expect("send nonhuman follow-up");
    fixture.finish_turn(&author).await;
    fixture.finish_turn(&author).await;
    assert_eq!(
        fixture
            .connect_with_bootstrap()
            .await
            .1
            .self_improvement
            .usage
            .len(),
        1
    );
    let automatic = config(LearningMode::Automatic);
    let write = fixture
        .client
        .replace_setting(
            "/self_improvement/projects",
            std::collections::BTreeMap::from([(project.id.clone(), automatic.clone())]),
            std::collections::BTreeMap::from([(project.id.clone(), suggest)]),
        )
        .await
        .expect("switch mode");
    expect_settings_write_applied(&mut fixture.client, &write, "automatic project config").await;
    shutdown.wait_until_entered().await;
    let generated = scoped_rule("Keep project changes focused.");
    let revised = scoped_rule("Explain focused project changes.");
    fixture
        .host_for_test()
        .set_learning_script_for_test(
            MockScript::one(MockTurn::text(response(vec![operation(
                None,
                generated.clone(),
                "project-durable",
            )])))
            .then(MockTurn::text(response(vec![operation(
                Some(revised.clone()),
                scoped_rule("Replace the human text."),
                "project-protected",
            )])))
            .then(MockTurn::text(response(vec![operation(
                None,
                steering("Cannot escape this project."),
                "project-escape",
            )]))),
        )
        .expect("install replacement context");
    human(
        &mut fixture,
        &author,
        "project-durable",
        HumanInputAction::Send,
        "For future work in this project, keep changes focused.",
    )
    .await;
    let waiting = state(&mut fixture, |state| {
        state
            .scopes
            .iter()
            .any(|job| job.scope == scope && job.queued == 1)
    })
    .await;
    assert!(
        waiting
            .scopes
            .iter()
            .any(|job| job.scope == scope && matches!(job.status, LearningStatus::Ready)),
        "Replacement must wait for the old native context's shutdown acknowledgement"
    );
    shutdown.release_one();
    let applied = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|change| matches!(change.status, LearningChangeStatus::Applied))
    })
    .await;
    let change = applied.changes[0].clone();
    assert_eq!(change.scope, scope);
    assert_eq!(change.operation.after, generated);
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Undo {
            id: change.id.clone(),
        })
        .await
        .expect("undo generated rule");
    state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|item| item.id == change.id && matches!(item.status, LearningChangeStatus::Undone))
    })
    .await;
    assert!(fixture.connect_with_bootstrap().await.1.steering.is_empty());
    let LearningRule::Steering {
        steering: human_rule,
    } = revised.clone()
    else {
        unreachable!()
    };
    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: human_rule,
        })
        .await
        .expect("human canonical edit");
    canonical_steering(&mut fixture, "Explain focused project changes.").await;
    human(
        &mut fixture,
        &author,
        "project-protected",
        HumanInputAction::Send,
        "Always replace that instruction.",
    )
    .await;
    let protected = state(&mut fixture, |state| {
        state.changes.iter().any(|change| change.requires_approval)
    })
    .await;
    assert!(
        protected
            .changes
            .iter()
            .any(|change| change.requires_approval
                && matches!(change.status, LearningChangeStatus::Pending))
    );
    assert_eq!(
        fixture.connect_with_bootstrap().await.1.steering[0].content,
        "Explain focused project changes."
    );
    let pending_id = protected
        .changes
        .iter()
        .find(|change| change.requires_approval)
        .expect("protected proposal")
        .id
        .clone();
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Dismiss {
            id: pending_id.clone(),
        })
        .await
        .expect("dismiss protected proposal");
    state(&mut fixture, |state| {
        state.changes.iter().any(|change| {
            change.id == pending_id && matches!(change.status, LearningChangeStatus::Dismissed)
        })
    })
    .await;
    human(
        &mut fixture,
        &author,
        "project-escape",
        HumanInputAction::Send,
        "A project instruction cannot grant host-wide authority.",
    )
    .await;
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    let failed = if bootstrap
        .self_improvement
        .scopes
        .iter()
        .any(|job| job.scope == scope && matches!(job.status, LearningStatus::Failed { .. }))
    {
        bootstrap.self_improvement
    } else {
        next_frame_matching_on(&mut observer, "scope violation rejected", |frame| {
            frame.kind == FrameKind::SelfImprovementNotify
                && frame
                    .parse_payload::<SelfImprovementState>()
                    .is_ok_and(|state| {
                        state.scopes.iter().any(|job| {
                            job.scope == scope
                                && matches!(job.status, LearningStatus::Failed { .. })
                        })
                    })
        })
        .await
        .parse_payload()
        .expect("scope failure")
    };
    assert_eq!(failed.usage.len(), 4);
    assert_eq!(fixture.connect_with_bootstrap().await.1.steering.len(), 1);
}
