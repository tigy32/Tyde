mod fixture;

use fixture::Fixture;
use protocol::{ChatEvent, FrameKind, NewAgentPayload, SpawnAgentParams, SpawnAgentPayload};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};

// Archived roots are intentionally absent from client rollups until live again
// (task_token_usage_rollups_from_snapshots filters roots by live_agent_ids).
// Audit the real durable storage boundary as well as the client recovery below.
fn persisted_accounting(fixture: &Fixture) -> Vec<String> {
    let connection = rusqlite::Connection::open(
        server::store::session::SessionStore::database_path(&fixture.session_store_path()),
    )
    .expect("open real session database");
    connection
        .prepare("SELECT record FROM task_usage_archive ORDER BY agent_id")
        .expect("read durable accounting")
        .query_map([], |row| row.get(0))
        .expect("query durable accounting")
        .collect::<Result<_, _>>()
        .expect("decode durable accounting")
}

#[tokio::test]
async fn restart_diagnostics_attribute_missing_checkpoints_without_replacing_accounting() {
    let logs = tempfile::tempdir().expect("private diagnostic directory");
    let path = logs.path().join("tyde-shell.log.test");
    // The native logger's default level is info. Keep only this content-free target
    // in this disk capture; other server diagnostics are outside this contract.
    tracing_subscriber::fmt()
        .with_env_filter("off,server::restart_accounting=info")
        .with_ansi(false)
        .with_writer(std::fs::File::create(&path).expect("diagnostic disk writer"))
        .try_init()
        .expect("install diagnostic logger");

    let mut clean = Fixture::new().await;
    let agent = clean
        .spawn_scripted(
            "private-agent-name",
            MockScript::one(MockTurn::text("private-reply")),
        )
        .await;
    clean.finish_turn(&agent).await;
    let original_id = agent.new_agent.agent_id.clone();
    let (_, before_shutdown) = clean.connect_with_bootstrap().await;
    let session = before_shutdown
        .agents
        .iter()
        .find(|item| item.agent_id == original_id)
        .and_then(|item| item.session_id.clone())
        .expect("typed live session");
    clean
        .host_for_test()
        .shutdown_for_restart()
        .await
        .expect("clean shutdown");
    clean
        .host_for_test()
        .wait_agent_termination_for_test(&agent.new_agent.agent_id)
        .await;
    let lines = std::fs::read_to_string(&path).expect("read default-level disk logs");
    for phase in [
        "ShutdownStarted",
        "AgentsPrepared",
        "CheckpointPublished",
        "AccountingCommitted",
    ] {
        assert!(
            lines.contains(&format!("phase={phase}")),
            "clean shutdown phase missing"
        );
    }
    assert!(
        lines.contains("acknowledged=true"),
        "stop acknowledgement observed"
    );
    assert!(
        lines.contains("termination=Returned"),
        "normal return classified"
    );

    // Abort after the stop was accepted, while the actor still owns its reply
    // and before it publishes final accounting. The real shutdown must refuse.
    let mut cancelled = clean;
    let restoration = server::new_spawn_operation_test_gate();
    let archive = cancelled
        .restart_host_with_runtime_config(|config| {
            config.restoration_snapshot_test_gate = Some(restoration.shared());
        })
        .await;
    restoration.wait_until_entered().await;
    let archived = persisted_accounting(&cancelled);
    assert!(
        !archived.is_empty(),
        "prior real shutdown provides durable accounting"
    );
    let shutdown_gate = MockGateHandle::new();
    let reservation = cancelled
        .reserve_next_mock_launch(
            "private-agent-name",
            MockScript::one(MockTurn::held_text("private-running-reply"))
                .with_shutdown_gate(&shutdown_gate),
        )
        .await;
    restoration.release_one();
    let restored = cancelled
        .restored_agents(&archive, std::slice::from_ref(&session))
        .await;
    drop(reservation);
    let restored = &restored[&session];
    assert!(
        restored.agent_id == original_id,
        "restoration retains the original identity"
    );
    let agent = fixture::TestAgent {
        stream: restored.instance_stream.clone(),
        new_agent: restored.clone(),
    };
    cancelled
        .client
        .send_message(&agent.stream, "private-active-message".into())
        .await
        .unwrap();
    cancelled
        .next_chat_event_matching(&agent, "active turn", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(true))
        })
        .await;
    cancelled
        .client
        .send_message(&agent.stream, "private-queued-message".into())
        .await
        .unwrap();
    cancelled.expect_queued_messages(&agent, 1).await;
    let (_, live_before) = cancelled.connect_with_bootstrap().await;
    let known_before = live_before
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == original_id)
        .expect("typed live accounting before fault")
        .total
        .usage
        .clone();
    assert!(
        known_before.total_tokens > 0,
        "measured usage exists before fault"
    );
    let host = cancelled.host_for_test();
    let stopping = host.clone();
    let attempt = tokio::spawn(async move { stopping.shutdown_for_restart().await });
    shutdown_gate.wait_until_entered().await;
    host.hard_stop_for_conformance().await;
    host.wait_agent_termination_for_test(&agent.new_agent.agent_id)
        .await;
    let error = attempt
        .await
        .expect("shutdown owner settled")
        .expect_err("missing checkpoint must block restart");
    assert!(
        error
            == "The actor ended without a final task accounting checkpoint; durable observations were not replaced",
        "preserve the original explicit failure"
    );
    let lines = std::fs::read_to_string(&path).expect("read cancellation diagnostics");
    let tail = lines
        .split("phase=ShutdownSettled success=true")
        .last()
        .unwrap();
    for expected in [
        "phase=CancellationRequested",
        "termination=Dropped",
        "phase=CheckpointMissing",
        "command_sent=true",
        "acknowledged=false",
        "checkpoint_present=false",
        "success=false",
    ] {
        assert!(
            tail.contains(expected),
            "cancellation failure attribution missing"
        );
    }
    assert!(
        !tail.contains("phase=AccountingCommitted"),
        "no diagnostic success on refusal"
    );
    let missing = tail
        .lines()
        .find(|line| line.contains("phase=CheckpointMissing"))
        .expect("missing checkpoint diagnostic");
    let slot = missing
        .split("accounting_actor=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    assert!(
        tail.lines()
            .any(|line| line.contains(&format!("accounting_actor={slot} "))
                && line.contains("phase=ActorTerminated")
                && line.contains("termination=Dropped")),
        "failure correlates to the terminated actor without exposing its identity"
    );

    let run = missing
        .split("diagnostic_run=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    assert!(
        tail.lines()
            .any(|line| line.contains(&format!("diagnostic_run={run} "))
                && line.contains(&format!("accounting_actor={slot} "))
                && line.contains("phase=ActorTerminated")),
        "termination correlation includes the diagnostic process incarnation"
    );

    let restoration = server::new_spawn_operation_test_gate();
    cancelled
        .relaunch_host_after_kill_with_runtime_config(|config| {
            config.restoration_snapshot_test_gate = Some(restoration.shared());
        })
        .await;
    restoration.wait_until_entered().await;
    assert!(
        persisted_accounting(&cancelled) == archived,
        "refused shutdown did not replace any durable accounting observation"
    );
    let reservation = cancelled
        .reserve_next_mock_launch(
            "private-agent-name",
            MockScript::one(MockTurn::held_text("private-continued-reply")),
        )
        .await;
    restoration.release_one();
    let mut continuing = false;
    let mut queued = false;
    let mut same_identity = false;
    while !continuing || !queued || !same_identity {
        let frame = fixture::next_logical_frame_on(
            &mut cancelled.client,
            "restored interrupted continuity",
        )
        .await;
        match frame.kind {
            FrameKind::NewAgent => {
                let item: NewAgentPayload = frame.parse_payload().unwrap();
                if item.session_id.as_ref() == Some(&session) {
                    assert!(
                        item.agent_id == original_id,
                        "recovery retains the original agent identity"
                    );
                    same_identity = true;
                }
            }
            FrameKind::ChatEvent => {
                if matches!(
                    frame.parse_payload::<ChatEvent>().unwrap(),
                    ChatEvent::RestartRecovery {
                        phase: protocol::RestartRecoveryPhase::Continuing
                    }
                ) {
                    continuing = true;
                }
            }
            FrameKind::QueuedMessages => {
                let messages: protocol::QueuedMessagesPayload = frame.parse_payload().unwrap();
                queued |= messages.messages.len() == 1;
            }
            _ => {}
        }
    }
    drop(reservation);
    let (_, recovered) = cancelled.connect_with_bootstrap().await;
    let known_after = &recovered
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == original_id)
        .expect("typed accounting after recovery")
        .total
        .usage;
    assert!(
        known_after.total_tokens >= known_before.total_tokens,
        "restored client accounting retains all known tokens"
    );
    cancelled
        .host_for_test()
        .shutdown_for_restart()
        .await
        .expect("recovered host closes with actual final accounting");

    // The existing startup gate panics inside the real actor's polled future.
    // This is a diagnostic fault injection, not a claimed updater root cause.
    let mut panicked = Fixture::new().await;
    let host = panicked.host_for_test();
    let gate = host.install_agent_startup_backend_ready_test_gate("private-panicked-agent");
    panicked
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: Default::default(),
            name: Some("private-panicked-agent".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".into()],
                prompt: "private-panic-input".into(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .unwrap();
    let agent: NewAgentPayload =
        fixture::next_frame_matching_on(&mut panicked.client, "starting agent", |env| {
            env.kind == FrameKind::NewAgent
        })
        .await
        .parse_payload()
        .unwrap();
    gate.wait_until_entered().await;
    gate.panic_on_release();
    gate.release_one();
    host.wait_agent_termination_for_test(&agent.agent_id).await;
    assert!(
        host.shutdown_for_restart().await.is_err(),
        "panic must not bypass final accounting"
    );
    let lines = std::fs::read_to_string(&path).expect("read unwind diagnostics");
    assert!(
        lines.contains("termination=Unwinding"),
        "panic unwind class recorded without payload"
    );
    let missing = lines
        .lines()
        .rfind(|line| line.contains("phase=CheckpointMissing"))
        .expect("panic missing checkpoint");
    assert!(
        missing.contains("termination=Unwinding"),
        "missing checkpoint retains termination class"
    );
    for secret in [
        "private-",
        "controlled spawn operation test panic",
        "/tmp/test",
        &agent.agent_id.0,
    ] {
        assert!(
            !lines.contains(secret),
            "diagnostics must not retain names, payloads, paths or native identities"
        );
    }
    let exported = tokio::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tools/export-restart-diagnostics.py"
        ))
        .arg(logs.path())
        .output()
        .await
        .expect("export existing real disk logs");
    assert!(exported.status.success(), "redacted disk export succeeds");
    let output = String::from_utf8(exported.stdout).expect("UTF-8 diagnostic export");
    let records = output
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("structured redacted export")
        })
        .collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|record| record["phase"] == "CheckpointMissing"
                && record["termination"] == "Unwinding"),
        "export retains the observed failure class"
    );
    assert!(
        records
            .iter()
            .any(|record| record["phase"] == "CheckpointMissing"
                && record["termination"] == "Dropped"),
        "export retains the cancelled-stop failure class"
    );
    assert!(
        !output.contains("private-") && !output.contains(&agent.agent_id.0),
        "export contains no private identity or content"
    );
    let corrupt = logs.path().join("tyde-shell.log.corrupt");
    std::fs::copy(&path, &corrupt).expect("retain real disk log for corruption probe");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(corrupt)
        .unwrap()
        .write_all(&[0xff])
        .unwrap();
    let rejected = tokio::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tools/export-restart-diagnostics.py"
        ))
        .arg(logs.path())
        .output()
        .await
        .expect("export damaged existing logs");
    assert!(
        !rejected.status.success(),
        "damaged encoding must not silently degrade evidence"
    );
    assert!(
        String::from_utf8(rejected.stderr).unwrap().trim()
            == "Unreadable Tyde tracing input; export incomplete",
        "unreadable input failure must not expose bytes or paths"
    );
    eprintln!("{output}");
    eprintln!(
        "Restart diagnostics disk boundary: clean return, accepted-stop cancellation and startup unwind PASS"
    );
}
