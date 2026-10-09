mod fixture;

use std::{process::Stdio, time::Duration};

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use fixture::Fixture;
use protocol::*;
use server::backend::mock::{MockGateHandle, MockRequest, MockScript, MockTurn};
use tokio::io::{AsyncBufReadExt, BufReader};
use tychat_bot::{BotState, owner_kit::OwnerKit};

const STEP: Duration = Duration::from_secs(45);

struct LocalTychat {
    child: AsyncGroupChild,
    build_log: tempfile::NamedTempFile,
    base: url::Url,
}

impl Drop for LocalTychat {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl LocalTychat {
    async fn start() -> Self {
        assert!(
            std::env::var("TYDE_RUN_TYCHAT_TESTS").is_ok_and(|value| value == "1"),
            "explicit Tychat integration opt-in required"
        );
        let repo = std::env::var_os("TYCHAT_REPO")
            .expect("TYCHAT_REPO must name the local Tychat worktree");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let log = tempfile::NamedTempFile::new().unwrap();
        let mut command = tokio::process::Command::new("cargo");
        command
            .current_dir(repo)
            .args(["run", "-p", "tychat-server", "--bin", "local"])
            .env("TYCHAT_ENVIRONMENT", "local")
            .env("TYCHAT_STORE_BACKEND", "memory")
            .env("TYCHAT_ALLOW_DEV_IDENTITY", "1")
            .env("TYCHAT_SERVER_ADDR", address.to_string())
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTC_WRAPPER")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log.reopen().unwrap());
        let mut local = Self {
            child: command
                .group_spawn()
                .expect("start real Tychat local binary"),
            build_log: log,
            base: url::Url::parse(&format!("http://{address}")).unwrap(),
        };
        let mut output = BufReader::new(local.child.inner().stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(300), async {
            while let Some(line) = output.next_line().await.unwrap() {
                if line.starts_with("tychat local server listening on http://") {
                    return;
                }
            }
            panic!(
                "Tychat exited before readiness; build log at {}",
                local.build_log.path().display()
            );
        })
        .await
        .expect("real Tychat local binary readiness deadline");
        local
    }
}

async fn state_when(
    host: &server::HostHandle,
    predicate: impl Fn(&TychatStatePayload) -> bool,
) -> TychatStatePayload {
    let mut changed = host.subscribe_tychat().await;
    tokio::time::timeout(STEP, async {
        loop {
            let state = host.tychat_state().await;
            if predicate(&state) {
                return state;
            }
            assert!(
                !matches!(state.status, TychatBridgeStatus::Failed { .. }),
                "bridge entered Failed before the expected state"
            );
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("Tychat host state transition")
}

async fn drained(host: &server::HostHandle, generation: &TychatPairingId) {
    let mut changed = host.subscribe_tychat().await;
    tokio::time::timeout(STEP, async {
        while !host
            .tychat_outbound(generation)
            .await
            .unwrap()
            .pending
            .is_empty()
        {
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("bridge acknowledges its durable outbox");
}

#[tokio::test]
#[ignore = "requires TYDE_RUN_TYCHAT_TESTS=1 and a real local TYCHAT_REPO"]
async fn real_tychat_pair_steer_chunk_restart_and_revoke() {
    server::install_default_crypto_provider();
    let mut local = LocalTychat::start().await;
    let owner = OwnerKit::create(&local.base, "tyde-bridge").await.unwrap();
    let created = owner.create_bot("Tyde integration").await.unwrap();
    let settings = settings_model::HostSettings {
        enabled_backends: vec![BackendKind::Claude],
        resume_previous_agents: false,
        tyde_agent_control_mcp_enabled: false,
        tychat: TychatSettings {
            enabled: true,
            backend_kind: Some(BackendKind::Claude),
            api_base_url: local.base.to_string().trim_end_matches('/').into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut fixture =
        Fixture::new_with_settings_file(&serde_json::json!({ "settings": settings }).to_string())
            .await;
    let host = fixture.tychat_host();
    let gate = MockGateHandle::new();
    let reply = format!("{}{}tail", "🦀".repeat(4100), " ".repeat(8100));
    let reservation = fixture
        .reserve_next_mock_launch(
            "Tychat agent",
            MockScript::one(MockTurn::gated_text(reply.clone(), &gate))
                .with_mid_turn_steering()
                .with_user_bubbles(),
        )
        .await;
    fixture
        .client
        .tychat_command(TychatCommandPayload::Pair {
            code: created.pairing_code.clone(),
        })
        .await
        .unwrap();
    let paired = state_when(&host, |state| {
        state.agent_id.is_some() && state.status == TychatBridgeStatus::AwaitingOwnerConfirmation
    })
    .await;
    let agent = paired.agent_id.unwrap();
    let fingerprints = paired.fingerprints.unwrap();
    assert!(
        fingerprints.owner == owner.fingerprint(),
        "pairing pins the actual owner"
    );
    let (generation, _, secret) = host.tychat_bot_state().await.unwrap();
    let saved: BotState = serde_json::from_slice(&secret.0).unwrap();
    assert!(
        saved.bot_id() == created.bot.bot_id,
        "pair command durably saves the redeemed bot"
    );
    drop(saved);
    drop(secret);
    drop(reservation);
    let chat = owner
        .confirm_bot(&created.bot.bot_id, &fingerprints.bot)
        .await
        .unwrap();
    state_when(&host, |state| state.status == TychatBridgeStatus::Connected).await;
    tokio::time::timeout(STEP, gate.wait_until_entered())
        .await
        .unwrap();
    assert!(
        host.tychat_outbound(&generation).await.unwrap().typing,
        "typing follows the held server turn"
    );
    let sent = owner
        .send_text(&chat, "redirect this held turn")
        .await
        .unwrap();
    let delivered = state_when(&host, |state| {
        state
            .last_delivery
            .as_ref()
            .is_some_and(|receipt| receipt.path == TychatDeliveryPath::Steered)
    })
    .await;
    let receipt = delivered.last_delivery.unwrap();
    let control = fixture.mock_by_id(&agent).await;
    let requests = control.requests().await;
    assert!(requests.iter().any(|request| matches!(request, MockRequest::Steer(payload) if payload.message == "redirect this held turn" && payload.origin == Some(MessageOrigin::Tychat { message_id: receipt.message_id.clone() }))), "verified owner message crosses the real protocol into typed steering");
    owner
        .wait_for_read_receipt(&chat, sent.seq, STEP)
        .await
        .unwrap();
    eprintln!(
        "PASS real pairing, owner verification, held-turn steering, typing source and read receipt"
    );

    let acknowledgement = host.install_tychat_outbound_ack_gate().await;
    gate.release_one();
    tokio::time::timeout(STEP, acknowledgement.wait_until_entered())
        .await
        .unwrap();
    let before = owner.bot_texts(&chat, sent.seq).await.unwrap();
    assert!(
        before.len() == 3,
        "long UTF-16 reply is split into three complete parts"
    );
    let joined: String = before
        .iter()
        .enumerate()
        .map(|(index, (_, text))| {
            assert!(
                text.encode_utf16().count() <= 8000,
                "each actual Tychat message obeys its UTF-16 limit"
            );
            text.strip_prefix(&format!("[Part {}]\n", index + 1))
                .expect("ordered chunk label")
        })
        .collect();
    assert!(
        joined == reply,
        "splitting preserves all Unicode and whitespace without truncation"
    );
    let pending = host.tychat_outbound(&generation).await.unwrap();
    assert!(
        pending.pending.len() == 1 && !pending.typing,
        "send succeeded while acknowledgement remains deliberately uncommitted"
    );
    fixture.restart_host().await;
    let host = fixture.tychat_host();
    let resumed = state_when(&host, |state| {
        state.status == TychatBridgeStatus::Connected && state.agent_id.is_some()
    })
    .await;
    drained(&host, &generation).await;
    assert!(
        owner.bot_texts(&chat, sent.seq).await.unwrap() == before,
        "restart retries real successful appends with identical IDs and creates no duplicate replies"
    );
    drop(acknowledgement);
    eprintln!(
        "PASS deterministic chunk retries across Tyde restart after remote success and before local acknowledgement"
    );

    let control = fixture.mock_by_id(&resumed.agent_id.unwrap()).await;
    control.enqueue(MockTurn::text("reply after restart")).await;
    let after = owner
        .send_text(&chat, "new turn after restart")
        .await
        .unwrap();
    let (_, text) = owner
        .wait_for_bot_text(&chat, after.seq, STEP)
        .await
        .unwrap();
    assert!(
        text == "reply after restart",
        "restarted bridge processes the next owner turn"
    );
    owner
        .wait_for_read_receipt(&chat, after.seq, STEP)
        .await
        .unwrap();
    drained(&host, &generation).await;
    assert!(
        control.violations().await.is_empty(),
        "no ordinary input was queued into the held backend"
    );

    owner.delete_bot(&created.bot.bot_id).await.unwrap();
    state_when(&host, |state| {
        state.status == TychatBridgeStatus::Unpaired && state.agent_id.is_none()
    })
    .await;
    assert!(
        host.tychat_bot_state().await.is_none(),
        "revocation erases secret bot state"
    );
    eprintln!("PASS subsequent turn and owner revocation remove the singleton and credentials");

    let replacement = owner.create_bot("Tyde replacement").await.unwrap();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Tychat agent",
            MockScript::one(MockTurn::text("replacement greeting")).with_mid_turn_steering(),
        )
        .await;
    fixture
        .client
        .tychat_command(TychatCommandPayload::Pair {
            code: replacement.pairing_code,
        })
        .await
        .unwrap();
    let replacement_state = state_when(&host, |state| {
        state.agent_id.is_some() && state.status == TychatBridgeStatus::AwaitingOwnerConfirmation
    })
    .await;
    drop(reservation);
    let replacement_chat = owner
        .confirm_bot(
            &replacement.bot.bot_id,
            &replacement_state.fingerprints.unwrap().bot,
        )
        .await
        .unwrap();
    state_when(&host, |state| state.status == TychatBridgeStatus::Connected).await;
    let (_, greeting) = owner
        .wait_for_bot_text(&replacement_chat, 0, STEP)
        .await
        .unwrap();
    assert!(
        greeting == "replacement greeting",
        "replacement SDK is live before explicit unpair"
    );
    fixture
        .client
        .tychat_command(TychatCommandPayload::Unpair)
        .await
        .unwrap();
    state_when(&host, |state| {
        state.status == TychatBridgeStatus::Unpaired && state.agent_id.is_none()
    })
    .await;
    assert!(
        host.tychat_bot_state().await.is_none(),
        "unpair stops the real SDK and erases its state"
    );
    fixture.restart_host().await;
    let host = fixture.tychat_host();
    assert!(
        host.tychat_bot_state().await.is_none(),
        "unpair remains erased after restart"
    );
    assert!(
        fixture.agent_session_ids().await.is_empty(),
        "unpaired singleton is not resurrected"
    );
    owner.delete_bot(&replacement.bot.bot_id).await.unwrap();
    eprintln!("PASS re-pair, explicit Settings unpair and erased state across restart");
    host.shutdown_for_restart().await;
    local.child.kill().await.unwrap();
}
