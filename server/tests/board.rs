mod fixture;

use fixture::{Fixture, TestAgent, next_frame_matching_on, send_host_payload};
use protocol::*;
use serde_json::{Value, json};

async fn state(
    f: &mut Fixture,
    predicate: impl Fn(&BoardStatePayload) -> bool,
) -> BoardStatePayload {
    state_on(&mut f.client, predicate).await
}
async fn state_on(
    client: &mut client::Connection,
    predicate: impl Fn(&BoardStatePayload) -> bool,
) -> BoardStatePayload {
    next_frame_matching_on(client, "board state", |frame| {
        frame.kind == FrameKind::BoardState
            && frame
                .parse_payload::<BoardStatePayload>()
                .is_ok_and(|board| predicate(&board))
    })
    .await
    .parse_payload()
    .expect("typed board state")
}
async fn send(f: &mut Fixture, command: BoardCommandPayload) {
    f.client
        .board_command(command)
        .await
        .expect("send typed board command");
}
async fn read(f: &mut Fixture, id: &ProjectId) -> BoardStatePayload {
    send(
        f,
        BoardCommandPayload::Read(BoardReadPayload {
            project_id: id.clone(),
        }),
    )
    .await;
    // Keep the primary viewer subscribed; a fresh viewer avoids queued snapshots.
    // Both connections must share the live registry, not start another host.
    let (mut client, _) = f.connect_with_bootstrap().await;
    send_host_payload(
        &mut client,
        FrameKind::BoardCommand,
        &BoardCommandPayload::Read(BoardReadPayload {
            project_id: id.clone(),
        }),
    )
    .await;
    next_frame_matching_on(&mut client, "fresh board subscription", |frame| {
        frame.kind == FrameKind::BoardState
    })
    .await
    .parse_payload()
    .expect("board subscription state")
}

async fn observe(f: &Fixture, id: &ProjectId) -> client::Connection {
    let (mut client, _) = f.connect_with_bootstrap().await;
    client
        .board_command(BoardCommandPayload::Read(BoardReadPayload {
            project_id: id.clone(),
        }))
        .await
        .expect("subscribe board observer");
    state_on(&mut client, |board| &board.project_id == id).await;
    client
}

async fn lanes(
    f: &mut Fixture,
    board: &BoardStatePayload,
    lanes: Vec<BoardLane>,
) -> BoardStatePayload {
    send(
        f,
        BoardCommandPayload::Lanes(BoardLanesPayload {
            project_id: board.project_id.clone(),
            expected_version: board.version,
            lanes,
        }),
    )
    .await;
    state(f, |next| next.version == board.version + 1).await
}
#[track_caller]
fn card<'a>(board: &'a BoardStatePayload, id: &BoardCardId) -> &'a BoardCard {
    board
        .cards
        .iter()
        .find(|card| &card.id == id)
        .expect("board card")
}
async fn move_card(
    f: &mut Fixture,
    board: &BoardStatePayload,
    id: &BoardCardId,
    destination: BoardDestination,
) -> BoardStatePayload {
    let version = card(board, id).version;
    send(
        f,
        BoardCommandPayload::Move(BoardMovePayload {
            project_id: board.project_id.clone(),
            card_id: id.clone(),
            destination,
            expected_version: version,
            position: Some(7),
            note: Some("Ready for review".into()),
        }),
    )
    .await;
    state(f, |next| {
        next.cards
            .iter()
            .any(|card| &card.id == id && card.version == version + 1)
    })
    .await
}
async fn spawn(f: &mut Fixture, project: &ProjectId, name: &str) -> (TestAgent, String) {
    let gate = server::backend::mock::MockGateHandle::new();
    let reservation = f
        .reserve_next_mock_launch(
            name,
            server::backend::mock::MockScript::one(server::backend::mock::MockTurn::gated_echo(
                &gate,
            ))
            .with_unbounded_echo()
            .with_compaction_coordinator(
                server::backend::compaction::BackendCompactionCoordinator::LegacyReplacement,
            ),
        )
        .await;
    let (agent, _) = f
        .spawn_with(SpawnAgentPayload {
            launch_context: AgentLaunchContext::Standard,
            name: Some(name.into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp".into()],
                prompt: "Show configuration".into(),
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
    gate.wait_until_entered().await;
    let reply = f
        .next_chat_event_matching(&agent, "launch configuration", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    let ChatEvent::StreamEnd(reply) = reply else {
        unreachable!()
    };
    gate.release_one();
    f.next_chat_event_matching(&agent, "launch settled", |event| {
        matches!(event, ChatEvent::TypingStatusChanged(false))
    })
    .await;
    let text = reply.message.content;
    (agent, text)
}
async fn rpc(caller: &server::AgentControlMcpCaller, method: &str, params: Value) -> Value {
    let response = reqwest::Client::new()
        .post(&caller.url)
        .header("Authorization", &caller.authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .expect("MCP HTTP exchange");
    assert!(response.status().is_success());
    let body = response.text().await.expect("MCP response body");
    let data = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("MCP event data");
    serde_json::from_str(data).expect("MCP JSON")
}
async fn tools(caller: &server::AgentControlMcpCaller) -> Vec<String> {
    rpc(caller, "tools/list", json!({})).await["result"]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| name.starts_with("tyde_board_"))
        .map(str::to_owned)
        .collect()
}
async fn role(f: &mut Fixture, agent: &TestAgent, access: AgentTydePermissions) {
    f.client
        .set_agent_tyde_permissions(&agent.stream, access)
        .await
        .expect("set role");
    next_frame_matching_on(&mut f.client, "role changed", |frame| {
        frame.kind == FrameKind::AgentTydePermissionsChanged
    })
    .await;
}

async fn compact(f: &mut Fixture, agent: &TestAgent) -> TestAgent {
    let old = agent.new_agent.agent_id.clone();
    f.client
        .compact_agent(&agent.stream, AgentCompactPayload::default())
        .await
        .expect("compact card");
    let mut replacement = None;
    next_frame_matching_on(&mut f.client, "board compaction", |frame| {
        if frame.kind == FrameKind::NewAgent {
            let agent: NewAgentPayload = frame.parse_payload().expect("replacement agent");
            if agent.agent_id != old {
                replacement = Some(agent);
            }
        }
        if frame.kind == FrameKind::AgentCompactNotify {
            let notice: AgentCompactNotifyPayload =
                frame.parse_payload().expect("compaction notice");
            assert_ne!(
                notice.status,
                AgentCompactStatus::Failed,
                "card compaction must succeed"
            );
            if notice.status == AgentCompactStatus::Completed {
                assert!(
                    notice.new_agent_id.as_ref().is_some_and(|id| id != &old),
                    "legacy replacement compaction must advertise a new agent"
                );
                return true;
            }
            return false;
        }
        false
    })
    .await;
    next_frame_matching_on(&mut f.client, "old card agent closed", |frame| {
        frame.kind == FrameKind::AgentClosed
            && frame
                .parse_payload::<AgentClosedPayload>()
                .is_ok_and(|closed| closed.agent_id == old)
    })
    .await;
    let replacement = replacement.expect("replacement advertised");
    TestAgent {
        stream: replacement.instance_stream.clone(),
        new_agent: replacement,
    }
}
async fn move_project(f: &mut Fixture, agent: &TestAgent, project: &ProjectId) {
    f.client
        .move_agent(protocol::types::AgentMovePayload {
            request_id: "board-project-transfer".into(),
            agent_id: agent.new_agent.agent_id.clone(),
            project_id: Some(project.clone()),
        })
        .await
        .expect("move project");
    let result: protocol::types::AgentMoveResultPayload =
        next_frame_matching_on(&mut f.client, "project transfer", |frame| {
            frame.kind == FrameKind::AgentMoveResult
        })
        .await
        .parse_payload()
        .expect("project move result");
    assert!(result.result.is_ok(), "project transfer must succeed");
}

#[tokio::test]
async fn board_lanes_cards_tools_steering_and_restart() {
    let mut f = Fixture::new().await;
    let root = tempfile::tempdir().expect("project root");
    for args in [
        vec!["init"],
        vec![
            "-c",
            "user.name=Board test",
            "-c",
            "user.email=board@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "Initial",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("initialize project repository")
                .status
                .success()
        );
    }
    f.client
        .project_create(ProjectCreatePayload {
            name: "Board".into(),
            roots: vec![ProjectRootPath(root.path().to_string_lossy().into_owned())],
        })
        .await
        .expect("create project");
    let frame = next_frame_matching_on(&mut f.client, "project", |frame| {
        frame.kind == FrameKind::ProjectNotify
    })
    .await;
    let ProjectNotifyPayload::Upsert { project } = frame.parse_payload().expect("project notify")
    else {
        panic!("project upsert")
    };
    f.client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: project.id.clone(),
            branch: GitBranchName("board-flow".into()),
            name: "Board workbench".into(),
        })
        .await
        .expect("create workbench");
    let frame = next_frame_matching_on(&mut f.client,"workbench",|frame| frame.kind == FrameKind::ProjectNotify && frame.parse_payload::<ProjectNotifyPayload>().is_ok_and(|p| matches!(p,ProjectNotifyPayload::Upsert { project: p } if p.parent_project_id() == Some(&project.id)))).await;
    let ProjectNotifyPayload::Upsert { project: workbench } =
        frame.parse_payload().expect("workbench notify")
    else {
        panic!("workbench upsert")
    };
    let mut board = read(&mut f, &project.id).await;
    assert_eq!(
        board
            .lanes
            .iter()
            .map(|lane| lane.name.as_str())
            .collect::<Vec<_>>(),
        ["Active", "Done"]
    );
    assert!(!board.customized);
    let (mut a, before) = spawn(&mut f, &project.id, "Card A").await;
    assert!(!before.contains("Lanes. Call tyde_board_read"));
    let caller = f.agent_control_caller(&a.new_agent.agent_id).await;
    assert!(tools(&caller).await.is_empty());
    role(&mut f, &a, AgentTydePermissions::Orchestrator).await;
    assert_eq!(tools(&caller).await, ["tyde_board_lanes"]);
    let mut unchanged_lanes = board.lanes.clone();
    unchanged_lanes[0].instructions = Some("  ".into());
    let unchanged = rpc(
        &caller,
        "tools/call",
        json!({"name":"tyde_board_lanes","arguments":{"lanes":unchanged_lanes}}),
    )
    .await;
    assert_ne!(unchanged["result"]["isError"], true);
    assert_eq!(tools(&caller).await, ["tyde_board_lanes"]);
    let mut edited = board.lanes.clone();
    edited[0].name = "In Progress".into();
    edited[0].instructions = Some("Keep work scoped.".into());
    board = lanes(&mut f, &board, edited).await;
    for (name, expected) in [
        ("   ", "Lane names can't be blank"),
        ("active", "Lane names must be unique"),
    ] {
        let mut rejected = board.lanes.clone();
        rejected[1].name = name.to_owned();
        rejected[0].name = "Active".to_owned();
        send(
            &mut f,
            BoardCommandPayload::Lanes(BoardLanesPayload {
                project_id: board.project_id.clone(),
                expected_version: board.version,
                lanes: rejected,
            }),
        )
        .await;
        let error: BoardErrorPayload =
            next_frame_matching_on(&mut f.client, "rejected lane name", |frame| {
                frame.kind == FrameKind::BoardError
            })
            .await
            .parse_payload()
            .expect("typed lane error");
        assert_eq!(error.code, BoardErrorCode::InvalidInput);
        assert_eq!(error.message, expected);
    }
    assert!(board.customized);
    let inventory = tools(&caller).await;
    assert_eq!(inventory.len(), 3);
    let (b, after) = spawn(&mut f, &workbench.id, "Card B").await;
    assert!(after.contains("Lanes. Call tyde_board_read to see which one you're in.\\n\\nIn Progress: Keep work scoped."));
    let b_caller = f.agent_control_caller(&b.new_agent.agent_id).await;
    let inventory = tools(&b_caller).await;
    assert!(
        inventory.contains(&"tyde_board_read".into())
            && inventory.contains(&"tyde_board_move".into())
    );
    assert!(!inventory.contains(&"tyde_board_lanes".into()));
    let read_response = rpc(
        &b_caller,
        "tools/call",
        json!({"name":"tyde_board_read","arguments":{}}),
    )
    .await;
    let own: BoardToolReadResult = serde_json::from_str(
        read_response["result"]["content"][0]["text"]
            .as_str()
            .expect("board tool text"),
    )
    .expect("typed own-card read");
    assert!(
        own.board
            .cards
            .iter()
            .any(|card| card.id == own.own_card_id && card.agent_id == b.new_agent.agent_id)
    );
    let denied = rpc(&b_caller,"tools/call",json!({"name":"tyde_board_move","arguments":{"card_id":a.new_agent.agent_id.0,"destination":{"kind":"lane","name":"Done"}}})).await;
    assert_eq!(denied["result"]["isError"], true);
    board = read(&mut f, &project.id).await;
    let aid = BoardCardId(a.new_agent.agent_id.0.clone());
    let bid = BoardCardId(b.new_agent.agent_id.0.clone());
    assert!(board.cards.iter().all(|card| !card.retained));
    board = move_card(
        &mut f,
        &board,
        &aid,
        BoardDestination::Lane {
            name: "Done".into(),
        },
    )
    .await;
    assert_eq!(card(&board, &aid).history.len(), 1);
    assert!(matches!(
        card(&board, &aid).history[0].actor,
        BoardActor::User
    ));
    send(
        &mut f,
        BoardCommandPayload::Move(BoardMovePayload {
            project_id: project.id.clone(),
            card_id: aid.clone(),
            destination: BoardDestination::Lane {
                name: "In Progress".into(),
            },
            note: None,
            position: None,
            expected_version: 0,
        }),
    )
    .await;
    let error: BoardErrorPayload = next_frame_matching_on(&mut f.client, "stale move", |frame| {
        frame.kind == FrameKind::BoardError
    })
    .await
    .parse_payload()
    .expect("typed stale error");
    assert_eq!(error.code, BoardErrorCode::StaleVersion);
    board = move_card(
        &mut f,
        &board,
        &bid,
        BoardDestination::Parent {
            card_id: aid.clone(),
        },
    )
    .await;
    assert_eq!(
        card(&board, &bid).location,
        BoardLocation::Parent {
            card_id: aid.clone()
        }
    );
    eprintln!("BOARD FLOW phase=retained_compaction");
    let prior = card(&board, &aid).clone();
    let mut observer = observe(&f, &project.id).await;
    // The resumed session inherits its coordinator from this replacement.
    let replacement = f
        .reserve_next_mock_launch(
            "Card A",
            server::backend::mock::MockScript::new()
                .with_unbounded_echo()
                .with_compaction_coordinator(
                    server::backend::compaction::BackendCompactionCoordinator::LegacyReplacement,
                ),
        )
        .await;
    let old_agent = a.new_agent.agent_id.clone();
    a = compact(&mut f, &a).await;
    drop(replacement);
    board = state_on(&mut observer, |next| {
        next.cards
            .iter()
            .any(|card| card.id == aid && card.agent_id == a.new_agent.agent_id)
            && !next
                .live_agents
                .iter()
                .any(|agent| agent.agent_id == old_agent)
    })
    .await;
    let rotated = card(&board, &aid);
    assert!(rotated.history == prior.history && rotated.location == prior.location);
    assert_eq!(rotated.version, prior.version);
    assert_eq!(rotated.position, prior.position);
    assert_eq!(
        card(&board, &bid).location,
        BoardLocation::Parent {
            card_id: aid.clone()
        }
    );
    for destination in [
        BoardDestination::Parent {
            card_id: bid.clone(),
        },
        BoardDestination::Lane {
            name: "Missing".into(),
        },
    ] {
        send(
            &mut f,
            BoardCommandPayload::Move(BoardMovePayload {
                project_id: project.id.clone(),
                card_id: aid.clone(),
                destination,
                note: None,
                position: None,
                expected_version: card(&board, &aid).version,
            }),
        )
        .await;
        let error: BoardErrorPayload =
            next_frame_matching_on(&mut f.client, "invalid destination", |frame| {
                frame.kind == FrameKind::BoardError
            })
            .await
            .parse_payload()
            .expect("typed destination error");
        assert!(matches!(
            error.code,
            BoardErrorCode::InvalidInput | BoardErrorCode::NotFound
        ));
        if error.code == BoardErrorCode::NotFound {
            assert!(error.message.contains("In Progress") && error.message.contains("Done"));
        }
    }
    board = move_card(
        &mut f,
        &board,
        &aid,
        BoardDestination::Lane {
            name: "In Progress".into(),
        },
    )
    .await;
    assert_eq!(
        card(&board, &bid).location,
        BoardLocation::Parent {
            card_id: aid.clone()
        }
    );
    board = move_card(
        &mut f,
        &board,
        &bid,
        BoardDestination::Lane {
            name: "Done".into(),
        },
    )
    .await;
    assert_eq!(
        card(&board, &bid).location,
        BoardLocation::Lane {
            project_id: board.project_id.clone(),
            lane_id: board.lanes[1].id.clone()
        }
    );
    // The MCP own-card path uses the credential, not an argument supplied by the model.
    let result = rpc(&b_caller,"tools/call",json!({"name":"tyde_board_move","arguments":{"destination":{"kind":"lane","name":"Done"},"note":"Own move"}})).await;
    assert_ne!(result["result"]["isError"], true);
    board = state(&mut f, |next| {
        next.cards
            .iter()
            .find(|card| card.id == bid)
            .and_then(|card| card.history.last())
            .is_some_and(|movement| movement.note.as_deref() == Some("Own move"))
    })
    .await;
    assert!(matches!(
        card(&board, &bid).history.last().expect("tool move").actor,
        BoardActor::Agent { .. }
    ));
    let mut remaining = vec![board.lanes[0].clone()];
    remaining[0].instructions = None;
    board = lanes(&mut f, &board, remaining).await;
    assert_eq!(
        card(&board, &bid).location,
        BoardLocation::Lane {
            project_id: board.project_id.clone(),
            lane_id: board.lanes[0].id.clone()
        }
    );
    let (mut c, cleared) = spawn(&mut f, &project.id, "Never moved").await;
    eprintln!("BOARD FLOW phase=untouched_compaction");
    let cid = BoardCardId(c.new_agent.agent_id.0.clone());
    c = compact(&mut f, &c).await;
    board = state_on(&mut observer, |next| {
        next.cards
            .iter()
            .any(|card| card.id == cid && card.agent_id == c.new_agent.agent_id)
    })
    .await;
    assert!(!card(&board, &cid).retained && card(&board, &cid).history.is_empty());
    assert!(!cleared.contains("Lanes. Call tyde_board_read"));
    for agent in [&a, &b, &c] {
        f.client
            .close_agent(&agent.stream)
            .await
            .expect("close agent");
        next_frame_matching_on(&mut f.client, "closed", |frame| {
            frame.kind == FrameKind::AgentClosed
                && frame
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|closed| closed.agent_id == agent.new_agent.agent_id)
        })
        .await;
    }
    board = state_on(&mut observer, |next| next.live_agents.is_empty()).await;
    assert_eq!(board.cards.len(), 2);
    let prior = card(&board, &aid).clone();
    eprintln!("BOARD FLOW phase=resume");
    let session = prior.session_id.clone().expect("retained session");
    // Restart disconnects every board viewer; resume/compaction must not depend on subscriptions.
    drop(observer);
    f.restart_host().await;
    let resume_gate = server::backend::mock::MockGateHandle::new();
    let reservation = f
        .reserve_next_mock_launch(
            "Resumed card A",
            server::backend::mock::MockScript::one(
                server::backend::mock::MockTurn::text_after_gate("Resume ready", &resume_gate),
            )
            .with_unbounded_echo()
            .with_compaction_coordinator(
                server::backend::compaction::BackendCompactionCoordinator::LegacyReplacement,
            ),
        )
        .await;
    let (mut resumed, _) = f
        .spawn_with(SpawnAgentPayload {
            launch_context: AgentLaunchContext::Standard,
            name: Some("Resumed card A".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: Some("Continue".into()),
            },
        })
        .await;
    drop(reservation);
    eprintln!("BOARD FLOW phase=resume_attached");
    resume_gate.wait_until_entered().await;
    resume_gate.release_one();
    f.finish_turn(&resumed)
        .await
        .assert_stream_end_contains("Resume ready");
    eprintln!("BOARD FLOW phase=resume_turn_completed");
    let gate = server::backend::mock::MockGateHandle::new();
    let replacement = f
        .reserve_next_mock_launch(
            "Resumed card A",
            server::backend::mock::MockScript::one(
                server::backend::mock::MockTurn::text_after_gate("Replacement ready", &gate),
            )
            .with_unbounded_echo()
            .with_compaction_coordinator(
                server::backend::compaction::BackendCompactionCoordinator::LegacyReplacement,
            ),
        )
        .await;
    resumed = compact(&mut f, &resumed).await;
    drop(replacement);
    gate.wait_until_entered().await;
    gate.release_one();
    f.finish_turn(&resumed)
        .await
        .assert_stream_end_contains("Replacement ready");
    let reopened = read(&mut f, &project.id).await;
    assert_eq!(reopened.cards.len(), 2);
    assert!(card(&reopened, &aid).agent_id == resumed.new_agent.agent_id);
    assert!(card(&reopened, &aid).history == prior.history);
    send(
        &mut f,
        BoardCommandPayload::Read(BoardReadPayload {
            project_id: project.id.clone(),
        }),
    )
    .await;
    board = state(&mut f, |next| {
        next.cards
            .iter()
            .any(|card| card.id == aid && card.agent_id == resumed.new_agent.agent_id)
    })
    .await;
    assert_eq!(board.cards.len(), 2);
    assert!(card(&board, &aid).history == prior.history);
    let session = card(&board, &aid)
        .session_id
        .clone()
        .expect("rotated retained session");
    let caller = f.agent_control_caller(&resumed.new_agent.agent_id).await;
    let result = rpc(&caller,"tools/call",json!({"name":"tyde_board_move","arguments":{"destination":{"kind":"lane","name":"In Progress"}}})).await;
    assert_ne!(result["result"]["isError"], true);
    board = state(&mut f, |next| {
        next.cards
            .iter()
            .any(|card| card.id == aid && card.version == prior.version + 1)
    })
    .await;
    eprintln!("BOARD FLOW phase=project_transfer");
    let before_transfer = card(&board, &aid).history.len();
    let other_root = tempfile::tempdir().expect("second project root");
    f.client
        .project_create(ProjectCreatePayload {
            name: "Other Board".into(),
            roots: vec![ProjectRootPath(
                other_root.path().to_string_lossy().into_owned(),
            )],
        })
        .await
        .expect("second project");
    let frame = next_frame_matching_on(&mut f.client,"second project",|frame| frame.kind == FrameKind::ProjectNotify && frame.parse_payload::<ProjectNotifyPayload>().is_ok_and(|p| matches!(p,ProjectNotifyPayload::Upsert { project } if project.name == "Other Board"))).await;
    let ProjectNotifyPayload::Upsert { project: other } =
        frame.parse_payload().expect("second project")
    else {
        panic!("project upsert")
    };
    read(&mut f, &other.id).await;
    let mut original_observer = observe(&f, &project.id).await;
    let mut other_observer = observe(&f, &other.id).await;
    move_project(&mut f, &resumed, &other.id).await;
    state_on(&mut original_observer, |next| {
        !next.cards.iter().any(|card| card.id == aid)
    })
    .await;
    let transferred = state_on(&mut other_observer, |next| {
        next.cards.iter().any(|card| card.id == aid)
    })
    .await;
    assert_eq!(card(&transferred, &aid).history.len(), before_transfer + 1);
    let transfer = card(&transferred, &aid)
        .history
        .last()
        .expect("project transfer history");
    assert!(
        matches!(&transfer.from, BoardLocation::Lane { project_id, .. } if project_id == &project.id)
    );
    assert!(
        matches!(&transfer.to, BoardLocation::Lane { project_id, .. } if project_id == &other.id)
    );
    let transferred = move_card(
        &mut f,
        &transferred,
        &aid,
        BoardDestination::Lane {
            name: "Done".into(),
        },
    )
    .await;
    assert_eq!(card(&transferred, &aid).history.len(), before_transfer + 2);
    move_project(&mut f, &resumed, &project.id).await;
    state_on(&mut other_observer, |next| {
        !next.cards.iter().any(|card| card.id == aid)
    })
    .await;
    board = state_on(&mut original_observer, |next| {
        next.cards.iter().any(|card| card.id == aid)
    })
    .await;
    assert_eq!(card(&board, &aid).history.len(), before_transfer + 3);
    let database = rusqlite::Connection::open(server::store::session::SessionStore::database_path(
        &f.session_store_path(),
    ))
    .expect("open compaction storage boundary");
    database.execute_batch("CREATE TRIGGER fail_board_rotation BEFORE UPDATE ON board_cards BEGIN SELECT RAISE(ABORT,'injected board commit failure'); END;").expect("inject board transaction failure");
    f.client
        .compact_agent(&resumed.stream, AgentCompactPayload::default())
        .await
        .expect("request failing atomic compaction");
    let failure: AgentCompactNotifyPayload =
        next_frame_matching_on(&mut f.client, "atomic compaction failure", |frame| {
            frame.kind == FrameKind::AgentCompactNotify
                && frame
                    .parse_payload::<AgentCompactNotifyPayload>()
                    .is_ok_and(|notice| notice.status == AgentCompactStatus::Failed)
        })
        .await
        .parse_payload()
        .expect("typed compaction failure");
    assert!(
        failure
            .message
            .as_ref()
            .is_some_and(|message| message.contains("compaction metadata failed"))
    );
    database
        .execute_batch("DROP TRIGGER fail_board_rotation;")
        .expect("remove injected failure");
    f.client
        .send_message(&resumed.stream, "Continue after failed compaction".into())
        .await
        .expect("original card reservation released");
    f.finish_turn(&resumed)
        .await
        .assert_stream_end_contains("Continue after failed compaction");
    let mut closed_viewer = observe(&f, &project.id).await;
    f.client
        .close_agent(&resumed.stream)
        .await
        .expect("close resumed card");
    state_on(&mut closed_viewer, |next| {
        next.navigation.iter().any(|entry| {
            entry.card_id == aid
                && matches!(
                    &entry.target,
                    BoardNavigationTarget::Saved { session_id, backend_kind, project_id }
                        if *session_id == session
                            && *backend_kind == resumed.new_agent.backend_kind
                            && project_id.as_ref() == Some(&project.id)
                )
        })
    })
    .await;
    next_frame_matching_on(&mut f.client, "resumed card closed", |frame| {
        frame.kind == FrameKind::AgentClosed
            && frame
                .parse_payload::<AgentClosedPayload>()
                .is_ok_and(|closed| closed.agent_id == resumed.new_agent.agent_id)
    })
    .await;
    f.client
        .delete_session(DeleteSessionPayload {
            session_id: session,
        })
        .await
        .expect("delete retained session");
    state(&mut f, |next| {
        next.navigation.iter().any(|entry| {
            entry.card_id == aid && matches!(entry.target, BoardNavigationTarget::Unavailable)
        })
    })
    .await;
    f.client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
            force: false,
        })
        .await
        .expect("remove workbench");
    board = state(&mut f, |next| {
        next.navigation.iter().any(|entry| {
            entry.card_id == bid && matches!(entry.target, BoardNavigationTarget::Unavailable)
        })
    })
    .await;
    let history = card(&board, &bid).history.clone();
    assert_eq!(card(&board, &bid).title, "Card B");
    eprintln!("BOARD FLOW phase=restart");
    f.restart_host().await;
    board = read(&mut f, &project.id).await;
    assert!(card(&board, &bid).history == history);
    assert_eq!(board.cards.len(), 2);
    assert_eq!(card(&board, &bid).title, "Card B");
    board = move_card(
        &mut f,
        &board,
        &bid,
        BoardDestination::Parent {
            card_id: aid.clone(),
        },
    )
    .await;
    let version = card(&board, &aid).version;
    send(
        &mut f,
        BoardCommandPayload::Remove {
            project_id: project.id.clone(),
            card_id: aid.clone(),
            expected_version: version,
        },
    )
    .await;
    board = state(&mut f, |next| {
        next.cards.len() == 1 && next.cards[0].id == bid
    })
    .await;
    assert!(matches!(
        card(&board, &bid).location,
        BoardLocation::Lane { .. }
    ));
    let database = rusqlite::Connection::open(server::store::session::SessionStore::database_path(
        &f.session_store_path(),
    ))
    .expect("open board storage boundary");
    database
        .execute(
            "UPDATE board_cards SET record='invalid-json' WHERE id=?",
            [&bid.0],
        )
        .expect("inject storage decode failure");
    f.client
        .spawn_agent(SpawnAgentPayload {
            launch_context: AgentLaunchContext::Standard,
            name: Some("Error notification trigger".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec![],
                prompt: "Ready".into(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("trigger live board publication");
    let error: BoardErrorPayload =
        next_frame_matching_on(&mut f.client, "subscription storage failure", |frame| {
            frame.kind == FrameKind::BoardError
        })
        .await
        .parse_payload()
        .expect("typed subscription failure");
    assert_eq!(error.code, BoardErrorCode::Storage);
}
