mod fixture;
use fixture::{Fixture, TestAgent};
use protocol::*;
use server::backend::mock::{MockGateHandle, MockRequest, MockScript, MockTurn};

async fn configured() -> Fixture {
    let settings = settings_model::HostSettings {
        enabled_backends: vec![
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Hermes,
            BackendKind::Kiro,
        ],
        tyde_agent_control_mcp_enabled: false,
        resume_previous_agents: false,
        tychat: TychatSettings {
            enabled: true,
            backend_kind: Some(BackendKind::Claude),
            ..Default::default()
        },
        ..Default::default()
    };
    Fixture::new_with_runtime_config_and_settings_file(
        server::HostRuntimeConfig {
            tychat_bridge_disabled: true,
            ..Default::default()
        },
        &serde_json::json!({ "settings": settings }).to_string(),
    )
    .await
}

async fn pair(fixture: &mut Fixture) -> (TychatPairingId, TestAgent) {
    let host = fixture.tychat_host();
    let generation = host
        .install_tychat_pairing(
            server::tychat::SecretBotState(vec![7; 32]),
            TychatFingerprints {
                bot: "bot-fingerprint".into(),
                owner: "owner-fingerprint".into(),
            },
        )
        .await
        .expect("install verified pairing boundary");
    host.set_tychat_bridge_status(&generation, TychatBridgeStatus::Connected)
        .await
        .expect("connect bridge boundary");
    let frame = fixture
        .next_frame_matching("Tychat agent publication", |event| {
            event.kind == FrameKind::NewAgent
                && event
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|agent| agent.origin == AgentOrigin::Tychat)
        })
        .await;
    let new_agent: NewAgentPayload = frame.parse_payload().unwrap();
    let start = fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "Tychat agent stream",
        |event| {
            event.kind == FrameKind::AgentStart
                && event
                    .parse_payload::<AgentStartPayload>()
                    .is_ok_and(|start| start.agent_id == new_agent.agent_id)
        },
    )
    .await;
    let agent = TestAgent {
        stream: start.stream,
        new_agent,
    };
    fixture.finish_turn(&agent).await;
    let greeting = outbox(&host, &generation, 1).await;
    host.acknowledge_tychat_outbound(&generation, greeting.pending[0].message_id)
        .await
        .unwrap();
    (generation, agent)
}

fn message(id: &str, text: &str) -> TychatOwnerMessage {
    TychatOwnerMessage {
        message_id: TychatMessageId(id.into()),
        text: text.into(),
    }
}

async fn outbox(
    host: &server::HostHandle,
    generation: &TychatPairingId,
    count: usize,
) -> TychatOutboundSnapshot {
    let mut changed = host.subscribe_tychat().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = host.tychat_outbound(generation).await.unwrap();
            if snapshot.pending.len() == count {
                return snapshot;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("outbound state reaches expected count")
}

async fn ready(host: &server::HostHandle) {
    let mut changed = host.subscribe_tychat().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while host.tychat_state().await.agent_id.is_none() {
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("Tychat singleton is ready after replay");
}

async fn typing(host: &server::HostHandle, generation: &TychatPairingId, expected: bool) {
    let mut changed = host.subscribe_tychat().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while host.tychat_outbound(generation).await.unwrap().typing != expected {
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("Tychat typing follows server activity");
}

#[tokio::test]
async fn singleton_settings_resume_reset_and_secret_boundary() {
    let mut fixture = configured().await;
    assert!(fixture.bootstrap.agents.is_empty());
    let capable: Vec<_> = fixture
        .bootstrap
        .tychat
        .backend_capabilities
        .iter()
        .filter(|entry| entry.mid_turn == MidTurnSteeringCapability::Supported)
        .map(|entry| entry.backend_kind)
        .collect();
    assert_eq!(
        capable,
        vec![BackendKind::Claude, BackendKind::Codex, BackendKind::Hermes]
    );
    let (generation, agent) = pair(&mut fixture).await;
    let host = fixture.tychat_host();
    assert_eq!(host.agent_ids().await.len(), 1);
    assert!(
        host.install_tychat_pairing(
            server::tychat::SecretBotState(vec![9]),
            TychatFingerprints {
                bot: "other".into(),
                owner: "owner".into()
            }
        )
        .await
        .is_err()
    );
    fixture.client.close_agent(&agent.stream).await.unwrap();
    let denied = fixture
        .next_frame_matching("close singleton rejected", |frame| {
            frame.kind == FrameKind::CommandError
        })
        .await;
    assert!(!denied.payload.is_null());
    assert_eq!(host.agent_ids().await.len(), 1);
    let original = fixture
        .agent_session_ids()
        .await
        .into_iter()
        .next()
        .unwrap();
    let instructions = server::backend::mock::session_builtin_steering(&original).unwrap();
    assert!(instructions.contains("global: true"));
    assert!(instructions.contains("workbench"));
    assert_eq!(
        server::backend::mock::session_startup_mcp_servers(&original).unwrap(),
        vec!["tyde-agent-control(http)", "tyde-agent-await(http)"]
    );

    let invalid = fixture
        .client
        .replace_setting(
            "/tychat/backend_kind",
            BackendKind::Kiro,
            BackendKind::Claude,
        )
        .await
        .unwrap();
    assert!(
        !fixture::expect_settings_write_result(
            &mut fixture.client,
            &invalid,
            "reject non-steering backend"
        )
        .await
        .applied
    );
    let update = fixture
        .client
        .replace_setting(
            "/tychat/session_settings",
            SessionSettingsValues(std::collections::HashMap::from([(
                "model".into(),
                SessionSettingValue::String("haiku".into()),
            )])),
            SessionSettingsValues::default(),
        )
        .await
        .unwrap();
    fixture::expect_settings_write_applied(&mut fixture.client, &update, "apply live settings")
        .await;
    assert_eq!(
        host.tychat_state().await.settings_application,
        TychatSettingsApplication::Live
    );
    let disable = fixture
        .client
        .replace_setting("/tychat/enabled", false, true)
        .await
        .unwrap();
    fixture::expect_settings_write_applied(&mut fixture.client, &disable, "disable Tychat").await;
    assert!(host.agent_ids().await.is_empty());
    assert_eq!(
        host.tychat_state().await.settings_application,
        TychatSettingsApplication::Inactive
    );
    assert!(
        host.deliver_tychat_message(&generation, message("disabled", "not admitted"))
            .await
            .is_err()
    );
    let enable = fixture
        .client
        .replace_setting("/tychat/enabled", true, false)
        .await
        .unwrap();
    fixture::expect_settings_write_applied(&mut fixture.client, &enable, "reenable Tychat").await;
    assert!(
        fixture.agent_session_ids().await == vec![original.clone()],
        "resume preserves the backend session"
    );
    host.set_tychat_bridge_status(
        &generation,
        TychatBridgeStatus::Paused {
            reason: "Owner verification required".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        host.deliver_tychat_message(&generation, message("paused", "not admitted"))
            .await
            .is_err()
    );
    host.set_tychat_bridge_status(&generation, TychatBridgeStatus::Connected)
        .await
        .unwrap();
    let access = fixture
        .client
        .replace_setting(
            "/tychat/access_mode",
            BackendAccessMode::ReadOnly,
            BackendAccessMode::Unrestricted,
        )
        .await
        .unwrap();
    fixture::expect_settings_write_applied(
        &mut fixture.client,
        &access,
        "stage immutable settings",
    )
    .await;
    assert_eq!(
        host.tychat_state().await.settings_application,
        TychatSettingsApplication::AppliesOnReset
    );
    let prior_settings = host.tychat_settings().await.unwrap();
    let mut staged = prior_settings.clone();
    staged.backend_kind = Some(BackendKind::Codex);
    staged.session_settings = SessionSettingsValues::default();
    let change_backend = fixture
        .client
        .replace_setting("/tychat", staged, prior_settings)
        .await
        .unwrap();
    fixture::expect_settings_write_applied(
        &mut fixture.client,
        &change_backend,
        "stage another steer-capable backend",
    )
    .await;
    assert_eq!(
        host.tychat_state().await.status,
        TychatBridgeStatus::Connected
    );
    assert_eq!(
        host.tychat_state().await.settings_application,
        TychatSettingsApplication::AppliesOnReset
    );
    let url = fixture
        .client
        .replace_setting(
            "/tychat/api_base_url",
            "http://localhost:5000/api/v1",
            "https://chat.tyggs.com",
        )
        .await
        .unwrap();
    assert!(
        !fixture::expect_settings_write_result(
            &mut fixture.client,
            &url,
            "the host owns the Tychat origin"
        )
        .await
        .applied
    );

    let bootstrap = fixture.restart_host().await;
    let host = fixture.tychat_host();
    ready(&host).await;
    assert_eq!(host.agent_ids().await.len(), 1);
    assert!(
        fixture.agent_session_ids().await == vec![original.clone()],
        "resume preserves the backend session"
    );
    assert!(host.tychat_bot_state().await.is_some());
    let secret = serde_json::to_string(&vec![7u8; 32]).unwrap();
    assert!(
        !serde_json::to_string(&bootstrap).unwrap().contains(&secret),
        "bootstrap must not contain bot state"
    );
    let bytes = std::fs::read(fixture.tychat_secret_path()).unwrap();
    assert!(
        String::from_utf8(bytes).unwrap().contains(&secret),
        "bot state must be persisted in host secret storage"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(fixture.tychat_secret_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(
        host.tychat_bot_state().await.unwrap().0 == generation,
        "restart retains the pairing generation"
    );
    let restore_backend = fixture
        .client
        .replace_setting(
            "/tychat/backend_kind",
            BackendKind::Claude,
            BackendKind::Codex,
        )
        .await
        .unwrap();
    fixture::expect_settings_write_applied(
        &mut fixture.client,
        &restore_backend,
        "select the reset backend",
    )
    .await;
    fixture
        .client
        .tychat_command(TychatCommandPayload::ResetAgent)
        .await
        .unwrap();
    fixture
        .next_frame_matching("reset settings applied", |frame| {
            frame.kind == FrameKind::TychatState
                && frame
                    .parse_payload::<TychatStatePayload>()
                    .is_ok_and(|state| {
                        state.settings_application == TychatSettingsApplication::Live
                    })
        })
        .await;
    assert_eq!(host.agent_ids().await.len(), 1);
    assert!(
        fixture.agent_session_ids().await != vec![original.clone()],
        "reset creates a fresh backend session"
    );
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            parent_agent_id: None,
            project_id: None,
            custom_agent_id: None,
            params: SpawnAgentParams::Resume {
                session_id: original,
                prompt: None,
            },
        })
        .await
        .unwrap();
    fixture
        .next_frame_matching("archived Tychat session stays host-owned", |frame| {
            frame.kind == FrameKind::CommandError
        })
        .await;
    assert_eq!(host.agent_ids().await.len(), 1);
    fixture
        .client
        .tychat_command(TychatCommandPayload::Unpair)
        .await
        .unwrap();
    fixture
        .next_frame_matching("unpaired", |frame| {
            frame.kind == FrameKind::TychatState
                && frame
                    .parse_payload::<TychatStatePayload>()
                    .is_ok_and(|state| state.status == TychatBridgeStatus::Unpaired)
        })
        .await;
    assert!(host.tychat_bot_state().await.is_none());
    assert!(host.agent_ids().await.is_empty());
    assert!(
        !String::from_utf8(std::fs::read(fixture.tychat_secret_path()).unwrap())
            .unwrap()
            .contains(&secret),
        "unpair erases persisted bot state"
    );
}

#[tokio::test]
async fn always_steer_and_outbox_survive_restart() {
    let mut fixture = configured().await;
    let gate = MockGateHandle::new();
    let ui_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Tychat agent",
            MockScript::one(MockTurn::text("Ready"))
                .then(MockTurn::gated_text("final reply", &gate))
                .then(MockTurn::gated_text("UI turn final", &ui_gate))
                .with_mid_turn_steering()
                .with_user_bubbles(),
        )
        .await;
    let (generation, agent) = pair(&mut fixture).await;
    drop(reservation);
    let host = fixture.tychat_host();
    assert_eq!(
        host.deliver_tychat_message(&generation, message("one", "start"))
            .await
            .unwrap()
            .path,
        TychatDeliveryPath::Started
    );
    gate.wait_until_entered().await;
    typing(&host, &generation, true).await;
    let steered = host
        .deliver_tychat_message(&generation, message("two", "redirect"))
        .await
        .unwrap();
    assert_eq!(steered.path, TychatDeliveryPath::Steered);
    assert_eq!(
        host.deliver_tychat_message(&generation, message("two", "redirect"))
            .await
            .unwrap(),
        steered
    );
    let control = fixture.mock(&agent).await;
    assert_eq!(
        control
            .requests()
            .await
            .iter()
            .filter(|request| matches!(request, MockRequest::Steer(_)))
            .count(),
        1
    );
    gate.release_one();
    let turn = fixture.finish_turn(&agent).await;
    for frame in &turn.frames {
        if frame.kind == FrameKind::QueuedMessages {
            assert!(
                frame
                    .parse_payload::<QueuedMessagesPayload>()
                    .unwrap()
                    .messages
                    .is_empty()
            );
        }
    }
    let first_users = turn
        .chat_events()
        .into_iter()
        .filter_map(|event| match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User) => {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        first_users.len() == 2,
        "deduplicated Tychat send and steer each echo exactly once"
    );
    for (index, id) in ["one", "two"].into_iter().enumerate() {
        assert!(
            first_users[index].origin
                == Some(MessageOrigin::Tychat {
                    message_id: TychatMessageId(id.into())
                }),
            "send and steer preserve their exact typed transport identity"
        );
    }
    control.hold_user_bubbles(true).await;
    fixture
        .client
        .send_message(&agent.stream, "typed in Tyde".into())
        .await
        .unwrap();
    ui_gate.wait_until_entered().await;
    let interleaved = host
        .deliver_tychat_message(&generation, message("three", "phone during UI turn"))
        .await
        .unwrap();
    assert_eq!(interleaved.path, TychatDeliveryPath::Steered);
    control.hold_user_bubbles(false).await;
    ui_gate.release_one();
    let interleaved_turn = fixture.finish_turn(&agent).await;
    let users = interleaved_turn
        .chat_events()
        .into_iter()
        .filter_map(|event| match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User) => {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        users.len() == 2,
        "interleaved inputs each produce exactly one user bubble"
    );
    assert!(
        users[0].content == "typed in Tyde" && users[0].origin.is_none(),
        "UI input must not consume a Tychat origin while its echo is in flight"
    );
    assert!(
        users[1].content == "phone during UI turn"
            && users[1].origin
                == Some(MessageOrigin::Tychat {
                    message_id: TychatMessageId("three".into())
                }),
        "phone input retains its own typed origin, not one correlated by echo order"
    );
    let pending = outbox(&host, &generation, 2).await;
    assert!(
        pending.pending[1].text == "UI turn final",
        "UI-started turns use the same outbound journal"
    );
    assert!(
        pending.pending[0].text == "final reply",
        "send the final assistant reply"
    );
    typing(&host, &generation, false).await;
    use blake2::{Blake2s256, Digest};
    let record = &pending.pending[0];
    let digest = Blake2s256::new()
        .chain_update(b"tyde.tychat.outbound.v1\0")
        .chain_update((record.agent_id.0.len() as u64).to_be_bytes())
        .chain_update(record.agent_id.0.as_bytes())
        .chain_update(record.turn_id.0.to_be_bytes())
        .finalize();
    assert_eq!(
        &record.message_id.0,
        &digest[..16],
        "outbound id is determined solely by the recorded agent and turn"
    );
    fixture.restart_host().await;
    let host = fixture.tychat_host();
    ready(&host).await;
    assert!(
        host.tychat_outbound(&generation).await.unwrap().pending == pending.pending,
        "restart preserves the exact outbox identities and bodies without replay duplicates"
    );
    for message in &pending.pending {
        host.acknowledge_tychat_outbound(&generation, message.message_id)
            .await
            .unwrap();
    }
    fixture.restart_host().await;
    ready(&fixture.tychat_host()).await;
    assert!(
        fixture
            .tychat_host()
            .tychat_outbound(&generation)
            .await
            .unwrap()
            .pending
            .is_empty()
    );
}

#[tokio::test]
async fn unsupported_interrupts_and_questions_use_typed_answers() {
    let mut fixture = configured().await;
    let question_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Tychat agent",
            MockScript::one(MockTurn::text("Ready"))
                .then(MockTurn::held_text("interrupted response"))
                .then(MockTurn::text("replacement reply"))
                .then(MockTurn::exit_plan_request(
                    "tychat-plan",
                    "Review the plan",
                ))
                .then(MockTurn::text("plan approved"))
                .then(MockTurn::blocking_question_request(
                    "tychat-question",
                    &question_gate,
                ))
                .then(MockTurn::text("answer received")),
        )
        .await;
    let (generation, agent) = pair(&mut fixture).await;
    drop(reservation);
    let host = fixture.tychat_host();
    host.deliver_tychat_message(&generation, message("one", "start"))
        .await
        .unwrap();
    fixture
        .next_frame_matching("running turn", |frame| {
            frame.kind == FrameKind::ChatEvent
                && frame.stream == agent.stream
                && matches!(
                    frame.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::StreamEnd(_))
                )
        })
        .await;
    assert_eq!(
        host.deliver_tychat_message(&generation, message("two", "replace"))
            .await
            .unwrap()
            .path,
        TychatDeliveryPath::InterruptedThenStarted
    );
    let control = fixture.mock(&agent).await;
    assert!(
        control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt))
    );
    outbox(&host, &generation, 2).await;
    host.deliver_tychat_message(&generation, message("three", "plan"))
        .await
        .unwrap();
    fixture
        .expect_paused_tool_request(&agent, "ExitPlanMode")
        .await;
    assert!(
        host.deliver_tychat_message(&generation, message("ambiguous", "maybe"))
            .await
            .is_err()
    );
    assert_eq!(
        host.deliver_tychat_message(&generation, message("four", "1"))
            .await
            .unwrap()
            .path,
        TychatDeliveryPath::Answered
    );
    assert!(control.requests().await.iter().any(|request| matches!(
        request,
        MockRequest::ToolResponse(SendMessageToolResponse::ExitPlanMode {
            decision: ExitPlanModeDecision::Approve,
            ..
        })
    )));
    let pending = outbox(&host, &generation, 4).await;
    assert!(
        pending
            .pending
            .iter()
            .any(|message| message.text.contains("1. Approve"))
    );
    host.deliver_tychat_message(&generation, message("five", "ask"))
        .await
        .unwrap();
    question_gate.wait_until_entered().await;
    question_gate.release_one();
    fixture
        .expect_paused_tool_request(&agent, "AskUserQuestion")
        .await;
    let receipt = host
        .deliver_tychat_message(&generation, message("six", "blue"))
        .await
        .unwrap();
    assert_eq!(receipt.path, TychatDeliveryPath::Answered);
    let pending = outbox(&host, &generation, 6).await;
    assert!(
        pending
            .pending
            .iter()
            .any(|message| message.text.contains("1. Choose a color"))
    );
    assert!(control.requests().await.iter().any(|request| matches!(request,
        MockRequest::ToolResponse(SendMessageToolResponse::AskUserQuestion { answer, .. }) if answer == "blue")));
    assert!(control.violations().await.is_empty());
}

#[tokio::test]
async fn turn_end_race_starts_without_queueing() {
    let mut fixture = configured().await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "Tychat agent",
            MockScript::one(MockTurn::text("Ready"))
                .then(MockTurn::held_text("first final"))
                .then(MockTurn::text("second final"))
                .with_mid_turn_steering(),
        )
        .await;
    let (generation, agent) = pair(&mut fixture).await;
    drop(reservation);
    let host = fixture.tychat_host();
    host.deliver_tychat_message(&generation, message("one", "start"))
        .await
        .unwrap();
    fixture
        .next_frame_matching("held first turn", |frame| {
            frame.stream == agent.stream
                && frame.kind == FrameKind::ChatEvent
                && matches!(
                    frame.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::StreamEnd(_))
                )
        })
        .await;
    let control = fixture.mock(&agent).await;
    control.end_held_turn_before_steer().await;
    let receipt = host
        .deliver_tychat_message(&generation, message("two", "next"))
        .await
        .unwrap();
    assert_eq!(receipt.path, TychatDeliveryPath::StartedAfterRace);
    let pending = outbox(&host, &generation, 2).await;
    assert!(
        pending
            .pending
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
            == vec!["first final", "second final"],
        "race closes each turn once and publishes both final responses in order"
    );
    assert_ne!(pending.pending[0].message_id, pending.pending[1].message_id);
    assert_ne!(pending.pending[0].turn_id, pending.pending[1].turn_id);
    assert!(control.violations().await.is_empty());
    assert!(
        !control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt))
    );
}
