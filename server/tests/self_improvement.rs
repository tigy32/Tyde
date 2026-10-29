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
/// The analyzer names a target, an update flag, an identifier, and text; the
/// server resolves scope and the current rule.
fn response(operations: Vec<LearningOperation>) -> String {
    let operations = operations
        .into_iter()
        .map(|operation| {
            let (target, id, title, text) = match operation.after {
                LearningRule::Steering { steering } => (
                    LearningTarget::Steering,
                    steering.id.0,
                    steering.title,
                    steering.content,
                ),
                LearningRule::ReviewAspect { id, aspect, .. } => (
                    LearningTarget::ReviewAspect,
                    id,
                    aspect.name,
                    aspect.instructions,
                ),
            };
            LearningProposal {
                origin_id: operation.origin_id,
                target,
                update: operation.before.is_some(),
                id,
                title,
                text,
                reason: operation.reason,
            }
        })
        .collect();
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
async fn author_delivery_error(
    fixture: &mut Fixture,
    agent: &TestAgent,
    id: &str,
) -> AgentErrorPayload {
    let frame = next_frame_matching_on(
        &mut fixture.client,
        "existing human action delivery outcome",
        |frame| match frame.kind {
            FrameKind::AgentError => frame.stream == agent.stream,
            FrameKind::HumanInputStatus => frame
                .parse_payload::<HumanInputStatusPayload>()
                .is_ok_and(|status| {
                    status.agent_id == agent.new_agent.agent_id && status.id.0 == id
                }),
            _ => false,
        },
    )
    .await;
    assert_eq!(
        frame.kind,
        FrameKind::AgentError,
        "Send and Steer must preserve the client-visible agent error channel"
    );
    let error: AgentErrorPayload = frame.parse_payload().expect("agent delivery error");
    assert_eq!(error.agent_id, agent.new_agent.agent_id);
    assert!(!error.fatal, "input delivery must not terminate its author");
    error
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
    let shipped_guidance = fixture.bootstrap.steering[0].clone();
    let mut edited_guidance = shipped_guidance.clone();
    edited_guidance.content =
        "Keep one feedback policy per entry, including approved exceptions.".into();
    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: edited_guidance.clone(),
        })
        .await
        .expect("edit default before analyzer starts");
    canonical_steering(&mut fixture, &edited_guidance.content).await;
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
            MockScript::one(MockTurn::gated_text_for_input(
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
                format!("\n{}\n", edited_guidance.content),
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
    let task = reconnect
        .1
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == agent.new_agent.agent_id)
        .expect("author task rollup");
    assert_eq!(
        task.breakdown.len(),
        1,
        "Analyzer batches must not become descendant agents"
    );
    assert_eq!(
        task.helper_usage.len(),
        2,
        "Each observed analyzer source is attached once"
    );
    let analyzer = task
        .backends
        .iter()
        .find(|group| group.backend_kind == BackendKind::Codex)
        .expect("analyzer-only backend retained in task overview");
    assert_eq!(analyzer.agent_count, 0);
    assert_eq!(analyzer.analyzer_batch_count, 2);
    let source_tokens: u64 = task
        .helper_usage
        .iter()
        .map(|source| {
            source
                .usage
                .reported_usage()
                .expect("reported analyzer usage")
                .total_tokens
        })
        .sum();
    assert!(
        source_tokens > 0,
        "The boundary must exercise real nonzero mock accounting"
    );
    assert_eq!(
        analyzer.total.usage.total_tokens, source_tokens,
        "Backend rollups include analyzer sources, not an allocation to the author backend"
    );
    assert_eq!(
        task.total.usage.total_tokens,
        task.backends
            .iter()
            .map(|group| group.total.usage.total_tokens)
            .sum::<u64>(),
        "Task and backend rollups must represent identical source scopes"
    );
    assert!(
        reconnect.1.steering == vec![edited_guidance],
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
        2,
        "Automatic must edit the same canonical rule instead of duplicating it"
    );
    assert_eq!(bootstrap.settings.review.aspects.len(), 1);
    assert!(
        bootstrap.settings.review.aspects["learned-defect"]
            .instructions
            .contains("active selection")
    );
    let mut human_owned = bootstrap
        .steering
        .iter()
        .find(|rule| rule.id.0 == "learned-format")
        .expect("learned rule")
        .clone();
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
    assert!(
        conflict.error.is_none(),
        "The typed proposal Conflict owns the failure; it must not also leak into unrelated scopes as a host error"
    );
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Dismiss {
            id: updated.id.clone(),
        })
        .await
        .expect("reject dismissing an applied conflict");
    let rejected: CommandErrorPayload = next_frame_matching_on(
        &mut fixture.client,
        "applied conflict cannot be dismissed",
        |frame| frame.kind == FrameKind::CommandError,
    )
    .await
    .parse_payload()
    .expect("command error");
    assert!(
        matches!(rejected.context, Some(CommandErrorContext::SelfImprovementChange { id }) if id == updated.id)
    );
    let persisted = fixture.connect_with_bootstrap().await.1;
    assert!(persisted.self_improvement.error.is_none());
    assert!(
        persisted
            .self_improvement
            .changes
            .iter()
            .any(|change| change.id == updated.id
                && matches!(change.status, LearningChangeStatus::Conflict { .. })
                && change.applied_revision.is_some())
    );
    assert_eq!(
        persisted
            .steering
            .iter()
            .find(|rule| rule.id.0 == "learned-format")
            .expect("learned rule")
            .content,
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
        restored
            .steering
            .iter()
            .find(|rule| rule.id.0 == "learned-format")
            .expect("learned rule")
            .content,
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
    let advertised = &fixture.bootstrap.self_improvement.analyzers;
    assert_eq!(advertised.len(), server::backend::SUPPORTED_BACKENDS.len());
    for kind in server::backend::SUPPORTED_BACKENDS {
        assert!(
            advertised.iter().any(|option| option.backend_kind == kind
                && option.capability == LearningAnalyzerCapability::TextAnalysis),
            "Every supported backend must be advertised for ordinary text analysis"
        );
    }
    // A fresh NewAgent precedes session binding; AgentStart carries the
    // persisted session that restart restoration republishes.
    let (agent, agent_start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: AgentLaunchContext::Standard,
            name: Some("Author".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".into()],
                prompt: "One-off task".into(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    let session = agent_start.session_id.expect("persisted author session");
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
    // The user chose best-effort analysis on every supported backend, so
    // unsupported coverage now uses the removed backend, not a live provider.
    let mut unsupported = config(LearningMode::Automatic);
    unsupported
        .analyzer
        .as_mut()
        .expect("selected model")
        .backend_kind = BackendKind::Tycode;
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
    // Restoration publishes NewAgent asynchronously after host bootstrap.
    // Drain its exact session before spawn() waits for the next NewAgent.
    let restored_agents = fixture.restored_agents(&bootstrap, &[session]).await;
    assert_eq!(restored_agents.len(), 1);
    eprintln!("LEARNING FENCED phase=restored_author_bootstrap_joined");
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

    // Launch input is accepted feedback too. Prepare the author while its
    // analyzer is disabled so the fenced reply belongs to fenced-input.
    let author = fixture.spawn("Fenced author", "One-off task").await;
    fixture.finish_turn(&author).await;
    eprintln!("LEARNING FENCED phase=author_prepared_analyzer_disabled");
    let fenced = format!(
        "```json\n{}\n```",
        response(vec![operation(
            None,
            steering("Retain explicit constraints."),
            "fenced-input",
        )])
    );
    fixture
        .host_for_test()
        .set_learning_script_for_test(MockScript::one(MockTurn::text(fenced)))
        .expect("install fenced analyzer reply");
    save(&mut fixture, unsupported, config(LearningMode::Automatic)).await;
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(bootstrap.self_improvement.usage.is_empty());
    assert!(bootstrap.self_improvement.changes.is_empty());
    eprintln!("LEARNING FENCED phase=enabled_before_explicit_feedback");
    human(
        &mut fixture,
        &author,
        "fenced-input",
        HumanInputAction::Send,
        "Always retain my explicit constraints.",
    )
    .await;
    let rejected: SelfImprovementState =
        next_frame_matching_on(&mut observer, "fenced analyzer run settled", |frame| {
            frame.kind == FrameKind::SelfImprovementNotify
                && frame
                    .parse_payload::<SelfImprovementState>()
                    .is_ok_and(|state| {
                        state.usage.len() == 1
                            && state
                                .scopes
                                .iter()
                                .all(|scope| !matches!(scope.status, LearningStatus::Running))
                    })
        })
        .await
        .parse_payload()
        .expect("parse fenced analyzer state");
    eprintln!("LEARNING FENCED phase=invalid_reply_settled");
    assert!(
        rejected.scopes.iter().any(|scope| matches!(
            &scope.status,
            LearningStatus::Failed { reason }
                if reason == "Analyzer returned invalid structured results; no changes applied"
        )),
        "A fenced reply must fail as invalid structured results, got {:?}",
        rejected
            .scopes
            .iter()
            .map(|scope| &scope.status)
            .collect::<Vec<_>>()
    );
    assert!(
        rejected.changes.is_empty(),
        "A reply outside the canonical JSON contract must not propose or apply a rule"
    );
    assert!(
        fixture
            .connect_with_bootstrap()
            .await
            .1
            .steering
            .iter()
            .all(|steering| steering.content != "Retain explicit constraints."),
        "Automatic mode must not apply a rule from a non-canonical reply"
    );
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
    for action in [HumanInputAction::Send, HumanInputAction::Steer] {
        human(
            &mut fixture,
            &rejected,
            "retry-after-rejection",
            action,
            "For future work, keep changes focused.",
        )
        .await;
        let refused = author_delivery_error(&mut fixture, &rejected, "retry-after-rejection").await;
        assert!(!refused.message.is_empty());
    }
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
    for (id, action, message) in [
        (
            "accepted-with-send-warning",
            HumanInputAction::Send,
            "Send once with learning unavailable",
        ),
        (
            "accepted-with-steer-warning",
            HumanInputAction::Steer,
            "Steer once with learning unavailable",
        ),
    ] {
        human(&mut fixture, &author, id, action, message).await;
        let warning = author_delivery_error(&mut fixture, &author, id).await;
        assert!(warning.message.contains("The author accepted this input"));
        assert!(warning.message.contains("learning is unavailable"));
        assert!(warning.message.contains("Do not resend the author input"));
        fixture::next_chat_event_matching_on(
            &mut observer,
            &stream,
            "accepted input completes despite unavailable learning",
            |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content.contains(message)),
        )
        .await;
        fixture::next_chat_event_matching_on(
            &mut observer,
            &stream,
            "author settles after accepted input warning",
            |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
        )
        .await;
        let requests = fixture.mock(&author).await.requests().await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| {
                    matches!(request, server::backend::mock::MockRequest::Input(input) if input.message == message)
                })
                .count(),
            1,
            "a learning warning must not duplicate accepted author input"
        );
    }
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
    let outside = fixture
        .bootstrap
        .steering
        .first()
        .expect("a rule outside this project")
        .clone();
    assert_ne!(outside.scope, scope);
    let escape = LearningRule::Steering {
        steering: Steering {
            content: "Cannot escape this project.".into(),
            ..outside.clone()
        },
    };
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
                Some(LearningRule::Steering {
                    steering: outside.clone(),
                }),
                escape,
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
    assert!(fixture.connect_with_bootstrap().await.1.steering == fixture.bootstrap.steering);
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Restore {
            id: change.id.clone(),
        })
        .await
        .expect("restore undone proposal");
    let restored = state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == change.id && matches!(item.status, LearningChangeStatus::Pending)
        })
    })
    .await;
    let restored = restored
        .changes
        .iter()
        .find(|item| item.id == change.id)
        .expect("restored proposal");
    assert_eq!(restored.operation, change.operation);
    assert_eq!(restored.scope, change.scope);
    assert_eq!(restored.created_at_ms, change.created_at_ms);
    assert_eq!(
        restored.expected_revision,
        change.applied_revision.expect("first application") + 1
    );
    assert!(
        restored.requires_approval
            && restored.applied_revision.is_none()
            && restored.last_edit_id.is_none()
    );
    let first_restore_revision = restored.expected_revision;
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Apply {
            id: change.id.clone(),
        })
        .await
        .expect("accept restored proposal");
    let reapplied = state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == change.id && matches!(item.status, LearningChangeStatus::Applied)
        })
    })
    .await;
    assert_eq!(
        reapplied
            .changes
            .iter()
            .find(|item| item.id == change.id)
            .expect("reapplied")
            .applied_revision,
        Some(first_restore_revision + 1)
    );
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Undo {
            id: change.id.clone(),
        })
        .await
        .expect("undo restored application");
    state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|item| item.id == change.id && matches!(item.status, LearningChangeStatus::Undone))
    })
    .await;
    assert!(fixture.connect_with_bootstrap().await.1.steering == fixture.bootstrap.steering);
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Restore {
            id: change.id.clone(),
        })
        .await
        .expect("restore a second cycle");
    let restored_again = state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == change.id && matches!(item.status, LearningChangeStatus::Pending)
        })
    })
    .await;
    assert_eq!(
        restored_again
            .changes
            .iter()
            .find(|item| item.id == change.id)
            .expect("restored again")
            .expected_revision,
        first_restore_revision + 2
    );
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Dismiss {
            id: change.id.clone(),
        })
        .await
        .expect("dismiss restored proposal");
    state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == change.id && matches!(item.status, LearningChangeStatus::Dismissed)
        })
    })
    .await;
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Restore {
            id: change.id.clone(),
        })
        .await
        .expect("restore dismissed proposal");
    let restored_dismissal = state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == change.id && matches!(item.status, LearningChangeStatus::Pending)
        })
    })
    .await;
    assert_eq!(
        restored_dismissal
            .changes
            .iter()
            .find(|item| item.id == change.id)
            .expect("restored dismissal")
            .expected_revision,
        first_restore_revision + 2
    );
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Dismiss {
            id: change.id.clone(),
        })
        .await
        .expect("dismiss again before canonical edit");
    state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == change.id && matches!(item.status, LearningChangeStatus::Dismissed)
        })
    })
    .await;
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
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Restore {
            id: change.id.clone(),
        })
        .await
        .expect("request invalid restore");
    let rejected: CommandErrorPayload = next_frame_matching_on(
        &mut fixture.client,
        "restore protects canonical human text",
        |frame| frame.kind == FrameKind::CommandError,
    )
    .await
    .parse_payload()
    .expect("restore error");
    assert!(
        matches!(rejected.context, Some(CommandErrorContext::SelfImprovementChange { id }) if id == change.id)
    );
    let rejected_restore = fixture.connect_with_bootstrap().await.1.self_improvement;
    assert!(rejected_restore.error.is_none());
    let unchanged = rejected_restore
        .changes
        .iter()
        .find(|item| item.id == change.id)
        .expect("dismissed proposal");
    assert!(matches!(unchanged.status, LearningChangeStatus::Dismissed));
    assert_eq!(unchanged.expected_revision, first_restore_revision + 2);
    human(
        &mut fixture,
        &author,
        "project-protected",
        HumanInputAction::Send,
        "Always replace that instruction.",
    )
    .await;
    let protected = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|change| change.operation.origin_id.0 == "project-protected")
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
        fixture
            .connect_with_bootstrap()
            .await
            .1
            .steering
            .into_iter()
            .find(|rule| rule.id.0 == "project-learned")
            .expect("project learned rule")
            .content,
        "Explain focused project changes."
    );
    let pending_id = protected
        .changes
        .iter()
        .find(|change| change.operation.origin_id.0 == "project-protected")
        .expect("protected proposal")
        .id
        .clone();
    let original = protected
        .changes
        .iter()
        .find(|item| item.id == pending_id)
        .expect("pending operation")
        .operation
        .clone();
    let mut untitled = original.after.clone();
    if let LearningRule::Steering { steering } = &mut untitled {
        steering.title.clear();
    }
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Edit {
            id: pending_id.clone(),
            request_id: LearningEditId("edit-empty-title".into()),
            expected: Box::new(original.clone()),
            after: untitled,
            reason: original.reason.clone(),
        })
        .await
        .expect("request untitled edit");
    let untitled: CommandErrorPayload = next_frame_matching_on(
        &mut fixture.client,
        "empty-title edit identifies its correction",
        |frame| frame.kind == FrameKind::CommandError,
    )
    .await
    .parse_payload()
    .expect("empty-title error");
    assert_eq!(untitled.message, "Enter a rule title");
    assert_eq!(untitled.request_id.as_deref(), Some("edit-empty-title"));
    assert!(
        matches!(untitled.context, Some(CommandErrorContext::SelfImprovementChange { id }) if id == pending_id)
    );
    let after_untitled = fixture.connect_with_bootstrap().await.1.self_improvement;
    assert!(after_untitled.error.is_none());
    assert!(
        after_untitled
            .changes
            .iter()
            .find(|item| item.id == pending_id)
            == protected.changes.iter().find(|item| item.id == pending_id),
        "An invalid edit preserves the entire proposal and durable acknowledgment"
    );
    let edited_rule = scoped_rule("Retain explicit project edits.");
    let edit_a = SelfImprovementAction::Edit {
        id: pending_id.clone(),
        request_id: LearningEditId("edit-a1".into()),
        expected: Box::new(original.clone()),
        after: edited_rule.clone(),
        reason: original.reason.clone(),
    };
    fixture
        .client
        .self_improvement_action(edit_a.clone())
        .await
        .expect("first client edit");
    let edited_a = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|item| item.last_edit_id == Some(LearningEditId("edit-a1".into())))
    })
    .await;
    let edited_a = edited_a
        .changes
        .iter()
        .find(|item| item.id == pending_id)
        .expect("edited operation")
        .clone();
    fixture
        .client
        .self_improvement_action(edit_a)
        .await
        .expect("replay durable accepted edit");
    let (mut other_client, _) = fixture.connect_with_bootstrap().await;
    other_client
        .self_improvement_action(SelfImprovementAction::Edit {
            id: pending_id.clone(),
            request_id: LearningEditId("edit-b1".into()),
            expected: Box::new(edited_a.operation.clone()),
            after: edited_rule.clone(),
            reason: original.reason.clone(),
        })
        .await
        .expect("other client edit with identical content");
    let edited_b = state(&mut fixture, |state| {
        state
            .changes
            .iter()
            .any(|item| item.last_edit_id == Some(LearningEditId("edit-b1".into())))
    })
    .await;
    let edited_b = edited_b
        .changes
        .iter()
        .find(|item| item.id == pending_id)
        .expect("other client accepted edit")
        .clone();
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Edit {
            id: pending_id.clone(),
            request_id: LearningEditId("edit-a2".into()),
            expected: Box::new(original.clone()),
            after: scoped_rule("Do not overwrite another client."),
            reason: original.reason.clone(),
        })
        .await
        .expect("request stale edit");
    let stale: CommandErrorPayload = next_frame_matching_on(
        &mut fixture.client,
        "stale edit fenced after replay",
        |frame| frame.kind == FrameKind::CommandError,
    )
    .await
    .parse_payload()
    .expect("stale edit error");
    assert_eq!(
        stale.request_id.as_deref(),
        Some("edit-a2"),
        "An idempotent earlier replay must not fail"
    );
    assert_eq!(stale.request_kind, FrameKind::SelfImprovementAction);
    assert!(
        matches!(stale.context, Some(CommandErrorContext::SelfImprovementChange { id }) if id == pending_id)
    );
    let after_stale = fixture.connect_with_bootstrap().await.1.self_improvement;
    assert!(after_stale.error.is_none());
    assert!(
        after_stale
            .changes
            .iter()
            .find(|item| item.id == pending_id)
            == Some(&edited_b),
        "Rejected stale edit must preserve the entire proposal without printing author input"
    );
    let blocked_transaction = fixture
        .session_store_path()
        .with_file_name("self-improvement.json.tmp");
    std::fs::create_dir(&blocked_transaction)
        .expect("block journal transaction with owned directory");
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Edit {
            id: pending_id.clone(),
            request_id: LearningEditId("edit-a-io".into()),
            expected: Box::new(edited_b.operation.clone()),
            after: scoped_rule("This must not acknowledge a failed save."),
            reason: original.reason.clone(),
        })
        .await
        .expect("request edit with failing durable write");
    let io: CommandErrorPayload = next_frame_matching_on(
        &mut fixture.client,
        "journal failure is proposal scoped",
        |frame| frame.kind == FrameKind::CommandError,
    )
    .await
    .parse_payload()
    .expect("durable write error");
    assert_eq!(io.request_id.as_deref(), Some("edit-a-io"));
    assert!(
        matches!(io.context, Some(CommandErrorContext::SelfImprovementChange { id }) if id == pending_id)
    );
    let after_io = fixture.connect_with_bootstrap().await.1.self_improvement;
    assert!(after_io.error.is_none());
    assert!(
        after_io.changes.iter().find(|item| item.id == pending_id) == Some(&edited_b),
        "Failed durable edit must preserve the entire proposal without printing author input"
    );
    std::fs::remove_dir(&blocked_transaction).expect("remove owned transaction blocker");
    let LearningRule::Steering {
        steering: same_canonical,
    } = revised.clone()
    else {
        unreachable!()
    };
    other_client
        .steering_upsert(SteeringUpsertPayload {
            steering: same_canonical,
        })
        .await
        .expect("bump canonical revision without changing text");
    canonical_steering(&mut fixture, "Explain focused project changes.").await;
    fixture
        .client
        .self_improvement_action(SelfImprovementAction::Apply {
            id: pending_id.clone(),
        })
        .await
        .expect("request stale canonical apply");
    state(&mut fixture, |state| {
        state.changes.iter().any(|item| {
            item.id == pending_id
                && matches!(item.status, LearningChangeStatus::Conflict { .. })
                && item.applied_revision.is_none()
        })
    })
    .await;
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
    let reconnected = fixture.connect_with_bootstrap().await.1;
    let dismissed = reconnected
        .self_improvement
        .changes
        .iter()
        .find(|change| change.id == pending_id)
        .expect("dismissed audit row survives reconnect");
    assert_eq!(dismissed.scope, scope);
    assert!(matches!(dismissed.status, LearningChangeStatus::Dismissed));
    assert_eq!(
        dismissed.operation.after, edited_rule,
        "Dismissal retains the proposed rule for truthful history"
    );
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
    let steering = fixture.connect_with_bootstrap().await.1.steering;
    assert_eq!(steering.len(), 2);
    assert!(
        steering.contains(&outside),
        "A project analyzer must not edit a rule outside its scope"
    );
}
