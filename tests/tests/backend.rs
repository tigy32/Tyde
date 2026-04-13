mod fixture;

use std::time::Duration;

use protocol::{
    BackendKind, ChatEvent, Envelope, FrameKind, NewAgentPayload, SpawnAgentParams,
    SpawnAgentPayload, SpawnCostHint,
};

const REAL_BACKEND_TIMEOUT: Duration = Duration::from_secs(60);

fn binary_available(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn cost_hint_for(backend_kind: BackendKind) -> Option<SpawnCostHint> {
    match backend_kind {
        BackendKind::Codex => Some(SpawnCostHint::Medium),
        _ => Some(SpawnCostHint::Low),
    }
}

/// Fixture that uses real backends (not mock) so backend_kind dispatch is tested.
struct RealBackendFixture {
    client: client::Connection,
    #[allow(dead_code)]
    session_store_dir: tempfile::TempDir,
}

impl RealBackendFixture {
    async fn new() -> Self {
        fixture::init_tracing();

        let session_store_dir = tempfile::tempdir().expect("create session tempdir");
        let session_path = session_store_dir.path().join("sessions.json");
        let project_path = session_store_dir.path().join("projects.json");
        // Real backends — NOT mock
        let host = server::spawn_host_with_store_paths(session_path, project_path)
            .expect("initialize host with real backends");

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

        let client = client::connect(&client_config, client_stream)
            .await
            .expect("client handshake failed");

        Self {
            client,
            session_store_dir,
        }
    }
}

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(REAL_BACKEND_TIMEOUT, client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

/// Spawn an agent through the protocol and consume events until the first turn completes.
/// Asserts NewAgent, AgentStart, and at least one StreamStart..StreamEnd cycle with text.
async fn say_hi_via_protocol(client: &mut client::Connection, backend_kind: BackendKind) {
    client
        .spawn_agent(SpawnAgentPayload {
            name: "say-hi".to_owned(),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp".to_owned()],
                prompt: Some("Say hi! Reply with a short greeting.".to_owned()),
                backend_kind,
                cost_hint: cost_hint_for(backend_kind),
            },
        })
        .await
        .expect("spawn_agent failed");

    // NewAgent on host stream
    let env = expect_next_event(client, "NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    assert_eq!(new_agent.backend_kind, backend_kind);
    let agent_stream = new_agent.instance_stream;

    // AgentStart on agent stream
    let env = expect_next_event(client, "AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);

    // Consume ChatEvents until first StreamEnd. Real backends may emit
    // TaskUpdate, ToolRequest, etc. before/between stream events.
    let mut got_stream_start = false;
    let mut got_text = false;
    loop {
        let env = expect_next_event(client, "ChatEvent").await;
        assert_eq!(env.kind, FrameKind::ChatEvent);
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::StreamStart(_) => {
                got_stream_start = true;
            }
            ChatEvent::StreamDelta(delta) => {
                if !delta.text.is_empty() {
                    got_text = true;
                }
            }
            ChatEvent::StreamEnd(data) => {
                if !data.message.content.trim().is_empty() {
                    got_text = true;
                }
                assert!(got_stream_start, "received StreamEnd before StreamStart");
                break;
            }
            _ => {} // TaskUpdate, ToolRequest, etc. are fine
        }
    }
    assert!(got_stream_start, "never received StreamStart");
    assert!(got_text, "never received any text from backend");
}

// ---------------------------------------------------------------------------
// Real backend tests — skip if binary not installed, 60s timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claude_say_hi() {
    if !binary_available("claude") {
        eprintln!("SKIPPED: claude not installed");
        return;
    }
    let mut fixture = RealBackendFixture::new().await;
    say_hi_via_protocol(&mut fixture.client, BackendKind::Claude).await;
}

#[tokio::test]
async fn codex_say_hi() {
    if !binary_available("codex") {
        eprintln!("SKIPPED: codex not installed");
        return;
    }
    let mut fixture = RealBackendFixture::new().await;
    say_hi_via_protocol(&mut fixture.client, BackendKind::Codex).await;
}

#[tokio::test]
async fn gemini_say_hi() {
    if !binary_available("gemini") {
        eprintln!("SKIPPED: gemini not installed");
        return;
    }
    let mut fixture = RealBackendFixture::new().await;
    say_hi_via_protocol(&mut fixture.client, BackendKind::Gemini).await;
}
