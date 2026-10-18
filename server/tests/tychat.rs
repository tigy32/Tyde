mod fixture;
use fixture::{Fixture, TestAgent};
use protocol::*;
use server::backend::mock::{MockGateHandle, MockRequest, MockScript, MockTurn};

async fn configured() -> Fixture {
    configured_with_bridge_disabled(true).await
}

async fn configured_with_bridge_disabled(bridge_disabled: bool) -> Fixture {
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
            tychat_bridge_disabled: bridge_disabled,
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
    assert_eq!(new_agent.tyde_permissions, AgentTydePermissions::Root);
    assert_eq!(
        start
            .parse_payload::<AgentStartPayload>()
            .unwrap()
            .tyde_permissions,
        AgentTydePermissions::Root
    );
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
    (async {
        loop {
            let snapshot = host.tychat_outbound(generation).await.unwrap();
            if snapshot.pending.len() == count {
                return snapshot;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
}

async fn ready(host: &server::HostHandle) {
    let mut changed = host.subscribe_tychat().await;
    (async {
        while host.tychat_state().await.agent_id.is_none() {
            changed.changed().await.unwrap();
        }
    })
    .await;
}

async fn typing(host: &server::HostHandle, generation: &TychatPairingId, expected: bool) {
    let mut changed = host.subscribe_tychat().await;
    (async {
        while host.tychat_outbound(generation).await.unwrap().typing != expected {
            changed.changed().await.unwrap();
        }
    })
    .await;
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
    assert!(instructions.contains("global: true") && instructions.contains("tychat_send_message"));
    let operator = server::backend::mock::session_instructions(&original)
        .expect("the Tychat agent runs as the Tyde Operator by default");
    assert!(operator.contains("top-level coordinator") && operator.contains("workbench"));
    let operator_tools = vec![
        "tyde-config(http)",
        "tyde-agent-control(http)",
        "tyde-agent-await(http)",
        "tyde-tychat(http)",
    ];
    assert_eq!(
        server::backend::mock::session_startup_mcp_servers(&original).unwrap(),
        operator_tools
    );

    let caller = fixture
        .agent_control_caller(&agent.new_agent.agent_id)
        .await;
    let response = reqwest::Client::new()
        .post(fixture.config_mcp_http_url().await)
        .header("Authorization", caller.authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(
            &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"tyde_config_get_permissions","arguments":{}}}),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(
        response
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("MCP response data"),
    )
    .unwrap();
    let guide: serde_json::Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(guide["tyde_permissions"], "root");
    assert!(
        guide["guidance"]
            .as_str()
            .unwrap()
            .contains("any Tyde control")
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
    for failure in [
        server::PendingMockLaunchBehavior::FailSpawn {
            message: "controlled startup failure".into(),
        },
        server::PendingMockLaunchBehavior::Launch(
            server::backend::mock::MockLaunch::CloseBeforeResumeBarrier,
        ),
    ] {
        let reservation = fixture
            .reserve_mock_launch_behaviors(vec![("Tychat agent".into(), failure)])
            .await;
        let enable = fixture
            .client
            .replace_setting("/tychat/enabled", true, false)
            .await
            .unwrap();
        fixture::expect_settings_write_applied(
            &mut fixture.client,
            &enable,
            "resume failure settles without a local deadline",
        )
        .await;
        let (_, failed) = fixture.connect_with_bootstrap().await;
        assert!(matches!(
            failed.tychat.status,
            TychatBridgeStatus::Failed { .. }
        ));
        assert!(failed.tychat.agent_id.is_none() && failed.agents.is_empty());
        assert!(failed.tychat.fingerprints.is_some());
        let (mut observer, agentless) = fixture.connect_with_bootstrap().await;
        assert!(agentless.tychat.agent_id.is_none());
        assert_eq!(
            agentless.tychat.settings_application,
            TychatSettingsApplication::Inactive
        );
        host.set_tychat_bridge_status(&generation, TychatBridgeStatus::Connected)
            .await
            .unwrap();
        fixture::next_frame_matching_on(&mut observer, "agentless bridge connected", |env| {
            env.kind == FrameKind::TychatState
                && matches!(
                    env.parse_payload::<TychatStatePayload>().unwrap().status,
                    TychatBridgeStatus::Connected
                )
        })
        .await;
        let disable = observer
            .replace_setting("/tychat/enabled", false, true)
            .await
            .unwrap();
        let mut applied = false;
        let mut notified = false;
        fixture::next_frame_matching_on(&mut observer, "agentless paired settings wake", |env| {
            match env.kind {
                FrameKind::SettingsWriteResult => {
                    let result: SettingsWriteResultPayload = env.parse_payload().unwrap();
                    assert!(result.write_id == disable);
                    assert!(result.applied && result.field_errors.is_empty());
                    applied = true;
                }
                FrameKind::TychatState => {
                    let state: TychatStatePayload = env.parse_payload().unwrap();
                    assert!(state.agent_id.is_none());
                    assert_eq!(
                        state.settings_application,
                        TychatSettingsApplication::Inactive
                    );
                    notified = true;
                }
                _ => {}
            }
            applied && notified
        })
        .await;
        drop(reservation);
        let retry = fixture
            .client
            .replace_setting("/tychat/enabled", true, false)
            .await
            .unwrap();
        fixture::expect_settings_write_applied(&mut fixture.client, &retry, "retry saved session")
            .await;
        let (_, resumed) = fixture.connect_with_bootstrap().await;
        assert_eq!(resumed.agents.len(), 1);
        assert!(
            resumed.agents.first().is_some_and(|agent| {
                agent.origin == AgentOrigin::Tychat
                    && Some(&agent.agent_id) == resumed.tychat.agent_id.as_ref()
                    && agent.session_id.as_ref() == Some(&original)
            }),
            "retry after failed resume must publish the canonical saved session"
        );
        let disable = fixture
            .client
            .replace_setting("/tychat/enabled", false, true)
            .await
            .unwrap();
        fixture::expect_settings_write_applied(
            &mut fixture.client,
            &disable,
            "disable after successful retry",
        )
        .await;
    }
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
    assert!(
        !fixture::expect_settings_write_result(
            &mut fixture.client,
            &access,
            "the Tychat agent always runs unrestricted"
        )
        .await
        .applied
    );
    let operator_id = Some(CustomAgentId(OPERATOR_CUSTOM_AGENT_ID.to_owned()));
    let missing = fixture
        .client
        .replace_setting(
            "/tychat/custom_agent_id",
            Some(CustomAgentId("deleted-agent".into())),
            operator_id.clone(),
        )
        .await
        .unwrap();
    assert!(
        !fixture::expect_settings_write_result(
            &mut fixture.client,
            &missing,
            "reject an unknown custom agent"
        )
        .await
        .applied
    );
    let custom_agent = fixture
        .client
        .replace_setting(
            "/tychat/custom_agent_id",
            Some(CustomAgentId("tyde-default".into())),
            operator_id,
        )
        .await
        .unwrap();
    fixture::expect_settings_write_applied(
        &mut fixture.client,
        &custom_agent,
        "stage another custom agent",
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

    // Eager stream attachment holds spawn until replay settles. Reproduce a
    // host restart before clients reconnect so lifecycle admission can observe it.
    let readiness = host.install_agent_resume_readiness_test_gate("Tychat agent");
    let restoration = server::new_spawn_operation_test_gate();
    fixture
        .restart_host_without_client(|config| {
            config.restoration_complete_test_gate = Some(restoration.shared());
        })
        .await;
    readiness.wait_until_settling().await;
    eprintln!("Tychat restore regression: replay settled, input admission held");
    readiness.wait_until_observed().await;
    eprintln!("Tychat restore regression: lifecycle waiter observed pending completion");
    let held_host = fixture.tychat_host();
    let (mut observer, held) =
        fixture::connect_mobile_client_with_bootstrap(held_host.clone(), "restore-observer").await;
    assert!(
        held.tychat.agent_id.is_none(),
        "held startup must not publish a canonical Tychat binding"
    );
    // An idempotent acknowledgement is a durable write while startup is awaiting the actor.
    held_host
        .acknowledge_tychat_outbound(&generation, TychatOutboundId([0; 16]))
        .await
        .unwrap();
    fixture::next_logical_frame_matching_on(
        &mut observer,
        "Tychat persistence remains responsive during startup",
        |frame| {
            frame.kind == FrameKind::TychatState
                && frame
                    .parse_payload::<TychatStatePayload>()
                    .is_ok_and(|state| state.agent_id.is_none())
        },
    )
    .await;
    eprintln!("Tychat restore regression: persistence completed while startup held");
    drop(observer);
    readiness.release();
    restoration.wait_until_entered().await;
    eprintln!("Tychat restore regression: host restoration settled");
    let (client, bootstrap) = fixture.connect_with_bootstrap().await;
    fixture.client = client;
    assert!(
        bootstrap.tychat.agent_id.is_some(),
        "successful resume must retain the canonical Tychat agent"
    );
    assert_eq!(
        bootstrap
            .agents
            .iter()
            .filter(|agent| agent.origin == AgentOrigin::Tychat)
            .count(),
        1,
        "host truth must expose exactly one restored Tychat agent"
    );
    restoration.release_one();
    let host = fixture.tychat_host();
    assert_eq!(host.agent_ids().await.len(), 1);
    assert!(
        fixture.agent_session_ids().await == vec![original.clone()],
        "resume preserves the backend session"
    );
    assert_eq!(
        server::backend::mock::session_startup_mcp_servers(&original).unwrap(),
        operator_tools,
        "resume keeps the custom agent the session started with"
    );
    assert!(host.tychat_bot_state().await.unwrap().is_some());
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
        host.tychat_bot_state().await.unwrap().unwrap().0 == generation,
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
    let fresh = fixture.agent_session_ids().await;
    assert!(
        fresh != vec![original.clone()],
        "reset creates a fresh backend session"
    );
    // All controlled agents now have config MCP, so tool presence cannot
    // identify the selected preset. Assert the typed selection and the actual
    // startup instructions echoed through a subsequent protocol-visible turn.
    let (mut reset_client, reset_snapshot) = fixture.connect_with_bootstrap().await;
    let reset_agent = &reset_snapshot.agents[0];
    assert_eq!(
        reset_agent.custom_agent_id,
        Some(CustomAgentId("tyde-default".to_owned())),
        "reset preserves the explicit staged agent selection"
    );
    assert_eq!(reset_agent.tyde_permissions, AgentTydePermissions::Root);
    fixture::next_frame_matching_on(&mut reset_client, "reset agent attached", |env| {
        env.stream == reset_agent.instance_stream && env.kind == FrameKind::AgentBootstrap
    })
    .await;
    reset_client
        .send_message(
            &reset_agent.instance_stream,
            "Inspect reset startup".to_owned(),
        )
        .await
        .unwrap();
    let turn = fixture::finish_turn_on(&mut reset_client, &reset_agent.instance_stream).await;
    let reply = turn
        .chat_events()
        .into_iter()
        .find_map(|event| match event {
            ChatEvent::StreamEnd(end) => Some(end.message.content),
            _ => None,
        })
        .expect("reset startup reply");
    assert!(reply.contains("[startup_mcp_servers:"));
    assert!(
        !reply.contains("[instructions:"),
        "Default must not retain the Operator's custom instructions"
    );
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
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
    assert!(host.tychat_bot_state().await.unwrap().is_none());
    assert!(host.agent_ids().await.is_empty());
    assert!(
        !String::from_utf8(std::fs::read(fixture.tychat_secret_path()).unwrap())
            .unwrap()
            .contains(&secret),
        "unpair erases persisted bot state"
    );
}

#[tokio::test]
async fn private_journal_creation_replacement_and_unpair_survive_restart() {
    let mut fixture = configured_with_bridge_disabled(false).await;
    let (_, agent) = pair(&mut fixture).await;
    let (paired_client, paired_bootstrap) = fixture::connect_host(fixture.tychat_host()).await;
    drop(paired_client);
    let paired = paired_bootstrap.tychat;
    assert!(paired.agent_id.as_ref() == Some(&agent.new_agent.agent_id));
    assert!(paired.settings_application == TychatSettingsApplication::Live);
    assert!(paired.fingerprints.is_some());
    let original_session = paired_bootstrap
        .agents
        .iter()
        .find(|descriptor| Some(&descriptor.agent_id) == paired.agent_id.as_ref())
        .unwrap()
        .session_id
        .clone();
    assert!(original_session.is_some());
    fixture
        .tychat_host()
        .cancel_tychat_bridge_task_for_test()
        .await
        .expect("the real bridge task has completed with cancellation");
    fixture
        .client
        .tychat_command(TychatCommandPayload::Unpair)
        .await
        .unwrap();
    let failed_disconnect = fixture
        .next_frame_matching("unpair after an actual bridge task failure", |frame| {
            frame.kind == FrameKind::CommandError
                || (frame.kind == FrameKind::TychatState
                    && frame
                        .parse_payload::<TychatStatePayload>()
                        .is_ok_and(|state| state.status == TychatBridgeStatus::Unpaired))
        })
        .await;
    assert!(
        failed_disconnect.kind == FrameKind::CommandError,
        "a failed bridge task must not be reported as successful disconnection"
    );
    let error: CommandErrorPayload = failed_disconnect.parse_payload().unwrap();
    assert!(
        error
            .message
            .contains("Tychat bridge task failed before disconnect")
    );
    let (observer, preserved) = fixture::connect_host(fixture.tychat_host()).await;
    drop(observer);
    assert!(preserved.tychat.fingerprints == paired.fingerprints);
    assert!(preserved.tychat.agent_id == paired.agent_id && preserved.agents.len() == 1);
    assert!(
        preserved.agents[0].session_id == original_session,
        "a failed disconnect must preserve the paired canonical session"
    );

    fixture
        .client
        .tychat_command(TychatCommandPayload::ResetAgent)
        .await
        .unwrap();
    let replaced: TychatStatePayload = fixture
        .next_frame_matching("reset replaces the existing private journal", |frame| {
            frame.kind == FrameKind::TychatState
                && frame
                    .parse_payload::<TychatStatePayload>()
                    .is_ok_and(|state| {
                        state.agent_id.is_some()
                            && state.agent_id != paired.agent_id
                            && state.settings_application == TychatSettingsApplication::Live
                    })
        })
        .await
        .parse_payload()
        .unwrap();
    assert!(
        replaced.fingerprints == paired.fingerprints,
        "reset preserves the paired identity while replacing the journal"
    );
    let (reset_client, reset_bootstrap) = fixture::connect_host(fixture.tychat_host()).await;
    drop(reset_client);
    let replacement_session = reset_bootstrap
        .agents
        .iter()
        .find(|agent| Some(&agent.agent_id) == replaced.agent_id.as_ref())
        .unwrap()
        .session_id
        .clone()
        .unwrap();

    let bootstrap = fixture.restart_host().await;
    let resumed = if bootstrap.tychat.agent_id.is_some()
        && bootstrap.tychat.settings_application == TychatSettingsApplication::Live
    {
        bootstrap.tychat
    } else {
        fixture
            .next_frame_matching(
                "replacement journal reloads through the protocol",
                |frame| {
                    frame.kind == FrameKind::TychatState
                        && frame
                            .parse_payload::<TychatStatePayload>()
                            .is_ok_and(|state| {
                                state.agent_id.is_some()
                                    && state.settings_application == TychatSettingsApplication::Live
                            })
                },
            )
            .await
            .parse_payload()
            .unwrap()
    };
    assert!(
        resumed.fingerprints == paired.fingerprints,
        "a new host reloads the durable pairing rather than losing it"
    );
    let (resumed_client, resumed_bootstrap) = fixture::connect_host(fixture.tychat_host()).await;
    drop(resumed_client);
    let resumed_session = resumed_bootstrap
        .agents
        .iter()
        .find(|agent| Some(&agent.agent_id) == resumed.agent_id.as_ref())
        .unwrap()
        .session_id
        .as_ref()
        .unwrap();
    assert!(
        resumed_session == &replacement_session,
        "a new host reloads the replacement session, not the original session"
    );

    // This fixture's verified-boundary secret is intentionally not an SDK BotState: no network is used.
    let (observer, bridge_bootstrap) = fixture::connect_host(fixture.tychat_host()).await;
    drop(observer);
    if !matches!(
        bridge_bootstrap.tychat.status,
        TychatBridgeStatus::Failed { .. }
    ) {
        fixture
            .next_frame_matching(
                "SDK rejects the fixture-only secret before network access",
                |frame| {
                    frame.kind == FrameKind::TychatState
                        && frame
                            .parse_payload::<TychatStatePayload>()
                            .is_ok_and(|state| {
                                matches!(state.status, TychatBridgeStatus::Failed { .. })
                            })
                },
            )
            .await;
    }
    let settings_path = fixture.settings_store_path();
    let settings_bytes = std::fs::read(&settings_path).unwrap();
    std::fs::write(&settings_path, b"invalid fixture settings").unwrap();
    let host = fixture.tychat_host();
    let generation = host.tychat_bot_state().await.unwrap().unwrap().0;
    host.set_tychat_bridge_status(&generation, TychatBridgeStatus::Connected)
        .await
        .unwrap();
    fixture
        .next_frame_matching("bridge exits on a real host persistence failure", |frame| {
            frame.kind == FrameKind::TychatState
                && frame
                    .parse_payload::<TychatStatePayload>()
                    .is_ok_and(|state| {
                        matches!(state.status, TychatBridgeStatus::Failed { reason }
                if reason.starts_with("Failed to parse settings store"))
                    })
        })
        .await;
    std::fs::write(&settings_path, settings_bytes).unwrap();

    fixture
        .client
        .tychat_command(TychatCommandPayload::Unpair)
        .await
        .unwrap();
    let unpair_result = fixture
        .next_frame_matching("unpair after a terminated bridge", |frame| {
            frame.kind == FrameKind::CommandError
                || (frame.kind == FrameKind::TychatState
                    && frame
                        .parse_payload::<TychatStatePayload>()
                        .is_ok_and(|state| state.status == TychatBridgeStatus::Unpaired))
        })
        .await;
    assert!(
        unpair_result.kind == FrameKind::TychatState,
        "a terminated transport must not prevent explicit unpair"
    );
    let unpaired: TychatStatePayload = unpair_result.parse_payload().unwrap();
    assert!(unpaired.fingerprints.is_none() && unpaired.agent_id.is_none());
    let restoration = server::new_spawn_operation_test_gate();
    let bootstrap = fixture
        .restart_host_with_runtime_config(|config| {
            config.restoration_complete_test_gate = Some(restoration.shared());
        })
        .await;
    assert!(bootstrap.tychat.status == TychatBridgeStatus::Unpaired);
    assert!(bootstrap.tychat.fingerprints.is_none() && bootstrap.tychat.agent_id.is_none());
    assert!(bootstrap.agents.is_empty());
    restoration.wait_until_entered().await;
    restoration.release_one();

    // Exercise installation after disconnect acknowledgement at the verified
    // pairing boundary, without a pairing service or SDK network connection.
    let host = fixture.tychat_host();
    let installation = host
        .install_tychat_test_gate(server::tychat::TychatTestBoundary::PairingInstall)
        .await;
    let disconnected = host
        .install_tychat_test_gate(server::tychat::TychatTestBoundary::UnpairDisconnected)
        .await;
    let pairing_host = host.clone();
    let pairing = tokio::spawn(async move {
        pairing_host
            .install_tychat_pairing(
                server::tychat::SecretBotState(vec![7; 32]),
                TychatFingerprints {
                    bot: "bot-fingerprint".into(),
                    owner: "owner-fingerprint".into(),
                },
            )
            .await
    });
    installation.wait_until_entered().await;
    fixture
        .client
        .tychat_command(TychatCommandPayload::Unpair)
        .await
        .unwrap();
    disconnected.wait_until_entered().await;
    installation.release_one();
    pairing
        .await
        .unwrap()
        .expect("concurrent verified pairing installation completes");
    let (_, installed) = fixture.connect_with_bootstrap().await;
    assert!(installed.tychat.fingerprints.is_some());
    assert!(installed.tychat.agent_id.is_some() && installed.agents.len() == 1);
    assert_eq!(
        installed.tychat.status,
        TychatBridgeStatus::AwaitingOwnerConfirmation,
        "disconnected transport must not process the newly installed generation"
    );
    disconnected.release_one();
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "concurrent installation unpaired",
        |frame| {
            assert!(
                frame.kind != FrameKind::CommandError,
                "concurrent unpair must complete without a command error"
            );
            if frame.kind != FrameKind::TychatState {
                return false;
            }
            let state: TychatStatePayload = frame.parse_payload().unwrap();
            assert!(
                !matches!(state.status, TychatBridgeStatus::Failed { .. }),
                "no stale transport callback may fail the pairing transition"
            );
            state.status == TychatBridgeStatus::Unpaired
        },
    )
    .await;
    let (_, erased) = fixture.connect_with_bootstrap().await;
    assert_eq!(erased.tychat.status, TychatBridgeStatus::Unpaired);
    assert!(erased.tychat.fingerprints.is_none() && erased.tychat.agent_id.is_none());
    assert!(erased.agents.is_empty());
    // Reload after restoration settles: the current projection alone cannot prove durable erasure.
    let restoration = server::new_spawn_operation_test_gate();
    fixture
        .restart_host_without_client(|config| {
            config.restoration_complete_test_gate = Some(restoration.shared());
        })
        .await;
    restoration.wait_until_entered().await;
    restoration.release_one();
    let (_, reloaded) = fixture.connect_with_bootstrap().await;
    assert_eq!(reloaded.tychat.status, TychatBridgeStatus::Unpaired);
    assert!(reloaded.tychat.fingerprints.is_none() && reloaded.tychat.agent_id.is_none());
    assert!(reloaded.agents.is_empty());
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

#[tokio::test]
async fn send_tool_replaces_the_final_only_when_called_last() {
    let mut fixture = configured().await;
    let gate = MockGateHandle::new();
    let work_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Tychat agent",
            MockScript::one(MockTurn::text("Ready"))
                .then(MockTurn::tychat_send_then_text(
                    "On it!",
                    &gate,
                    "unsent final",
                ))
                .then(MockTurn::text("plain final"))
                .then(MockTurn::tychat_send_then_text_with_work(
                    "Checking",
                    &work_gate,
                    true,
                    "worked final",
                )),
        )
        .await;
    let (generation, _agent) = pair(&mut fixture).await;
    drop(reservation);
    let host = fixture.tychat_host();

    host.deliver_tychat_message(&generation, message("one", "do the thing"))
        .await
        .unwrap();
    let acknowledged = outbox(&host, &generation, 1).await;
    assert_eq!(
        acknowledged.pending[0].text, "On it!",
        "the tool message is journaled while the turn is still running"
    );
    assert!(
        acknowledged.typing,
        "the turn that sent the tool message has not ended yet"
    );
    host.acknowledge_tychat_outbound(&generation, acknowledged.pending[0].message_id)
        .await
        .unwrap();
    gate.release_one();
    typing(&host, &generation, false).await;

    host.deliver_tychat_message(&generation, message("two", "just answer"))
        .await
        .unwrap();
    let mut changed = host.subscribe_tychat().await;
    let replies = (async {
        loop {
            let snapshot = host.tychat_outbound(&generation).await.unwrap();
            if snapshot
                .pending
                .last()
                .is_some_and(|message| message.text == "plain final")
            {
                return snapshot;
            }
            changed.changed().await.unwrap();
        }
    })
    .await;
    assert_eq!(
        replies
            .pending
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec!["plain final"],
        "a turn that used tychat_send_message does not also send its final"
    );
    assert_ne!(replies.pending[0].turn_id, acknowledged.pending[0].turn_id);
    host.acknowledge_tychat_outbound(&generation, replies.pending[0].message_id)
        .await
        .unwrap();

    host.deliver_tychat_message(&generation, message("three", "go check"))
        .await
        .unwrap();
    let checking = outbox(&host, &generation, 1).await;
    assert_eq!(checking.pending[0].text, "Checking");
    host.acknowledge_tychat_outbound(&generation, checking.pending[0].message_id)
        .await
        .unwrap();
    work_gate.release_one();
    let followed_up = (async {
        loop {
            let snapshot = host.tychat_outbound(&generation).await.unwrap();
            if !snapshot.typing && snapshot.pending.len() == 1 {
                return snapshot;
            }
            changed.changed().await.unwrap();
        }
    })
    .await;
    assert_eq!(
        followed_up.pending[0].text, "worked final",
        "a tool call after tychat_send_message means the final follows up"
    );
    assert_eq!(followed_up.pending[0].turn_id, checking.pending[0].turn_id);
}

#[tokio::test]
async fn beta10_stores_upgrade_to_the_operator() {
    let mut fixture = configured().await;
    let (generation, _agent) = pair(&mut fixture).await;
    let journal_path = fixture.tychat_secret_path();
    let settings_path = journal_path.with_file_name("settings.json");
    let beta10 = |value: &mut serde_json::Value, pointer: &str| {
        let tychat = value.pointer_mut(pointer).unwrap().as_object_mut().unwrap();
        assert!(tychat.remove("custom_agent_id").is_some());
        tychat.insert("api_base_url".into(), "https://chat.tyggs.com".into());
        tychat.insert("access_mode".into(), "unrestricted".into());
    };
    fixture
        .restart_host_with_runtime_config(|_| {
            for (path, pointer) in [
                (&settings_path, "/settings/tychat"),
                (&journal_path, "/session/settings"),
            ] {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                beta10(&mut value, pointer);
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        })
        .await;
    let host = fixture.tychat_host();
    ready(&host).await;
    assert_eq!(
        host.tychat_bot_state().await.unwrap().unwrap().0,
        generation
    );
    assert_eq!(
        host.tychat_settings().await.unwrap().custom_agent_id,
        Some(CustomAgentId(OPERATOR_CUSTOM_AGENT_ID.to_owned())),
        "an upgraded host runs the Tychat agent as the Operator"
    );
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    let persisted = persisted.pointer("/settings/tychat").unwrap();
    assert!(persisted.get("api_base_url").is_none() && persisted.get("access_mode").is_none());
    assert_eq!(
        host.tychat_state().await.settings_application,
        TychatSettingsApplication::AppliesOnReset,
        "the beta.10 session ran without a custom agent until it is reset"
    );
}
