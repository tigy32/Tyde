mod fixture;
use base64::Engine;
use fixture::Fixture;
use protocol::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};

async fn snapshot(
    fixture: &mut Fixture,
    status: impl Fn(&DotsConnectionStatus) -> bool,
) -> DotsStatePayload {
    fixture
        .next_frame_matching("Dots connection state", |event| {
            event.kind == FrameKind::DotsState
                && event
                    .parse_payload::<DotsStatePayload>()
                    .is_ok_and(|state| status(&state.status))
        })
        .await
        .parse_payload()
        .expect("typed Dots state")
}

async fn rpc(
    client: &reqwest::Client,
    origin: &str,
    token: &str,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{origin}/mcp"))
        .bearer_auth(token)
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
        .send()
        .await
        .expect("MCP HTTP boundary");
    assert!(
        response.status().is_success(),
        "MCP request must reach the real host"
    );
    let result: serde_json::Value = response.json().await.expect("MCP response envelope");
    assert!(result.get("error").is_none(), "MCP operation must succeed");
    result["result"].clone()
}

#[tokio::test]
async fn dots_pairing_controls_the_real_host_and_disable_revokes_transport() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut service = tokio::process::Command::new("node")
        .arg(root.join("services/chatgpt-dots/serve.mjs"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(false)
        .spawn()
        .expect("start real Dots service");
    let mut lines = BufReader::new(service.stdout.take().unwrap()).lines();
    let origin = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .expect("service startup deadline")
        .expect("service output")
        .expect("service endpoint");
    let settings = settings_model::HostSettings {
        enabled_backends: vec![BackendKind::Claude],
        default_backend: Some(BackendKind::Claude),
        resume_previous_agents: false,
        launch_profiles: [(
            LaunchProfileId("claude:dots".into()),
            settings_model::HostLaunchProfileConfig {
                id: LaunchProfileId("claude:dots".into()),
                label: "Dots explicit profile".into(),
                description: None,
                backend_kind: BackendKind::Claude,
                session_settings: SessionSettingsValues(std::collections::HashMap::from([
                    ("model".into(), SessionSettingValue::String("sonnet".into())),
                    ("effort".into(), SessionSettingValue::String("high".into())),
                ])),
                acp: None,
            },
        )]
        .into(),
        ..Default::default()
    };
    let (configuration_gate, configuration_ready) = tokio::sync::watch::channel(true);
    let mut fixture = Fixture::new_with_runtime_config_and_settings_file(
        server::HostRuntimeConfig {
            dots_service_origin: Some(url::Url::parse(&origin).unwrap()),
            dots_configuration_gate: Some(configuration_ready),
            ..Default::default()
        },
        &json!({"settings":settings}).to_string(),
    )
    .await;
    assert!(matches!(
        fixture.bootstrap.dots.status,
        DotsConnectionStatus::Disabled
    ));
    fixture
        .client
        .replace_setting("/chatgpt_dots_enabled", true, false)
        .await
        .expect("enable through real protocol");
    let original_pairing = snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::AwaitingPairing)
    })
    .await;
    assert!(
        original_pairing.pairing_code.is_some(),
        "Host must publish a pairing code"
    );
    fixture
        .client
        .dots_command(DotsCommandPayload::Retry)
        .await
        .unwrap();
    let pairing = snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::AwaitingPairing)
    })
    .await;
    assert!(
        pairing.pairing_code != original_pairing.pairing_code,
        "Retry must replace the pending pairing code"
    );
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let callback = "https://chatgpt.com/connector_platform_oauth_redirect";
    let registration: serde_json::Value = http
        .post(format!("{origin}/oauth/register"))
        .json(&json!({"redirect_uris":[callback], "token_endpoint_auth_method":"none"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let mut authorize = url::Url::parse(&format!("{origin}/oauth/authorize")).unwrap();
    authorize.query_pairs_mut().extend_pairs([
        ("client_id", registration["client_id"].as_str().unwrap()),
        ("redirect_uri", callback),
        ("resource", pairing.mcp_url.as_deref().unwrap()),
        ("response_type", "code"),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge.as_str()),
        ("state", "flow"),
        ("scope", "tyde:control"),
    ]);
    let html = http
        .get(authorize)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let transaction = html
        .split("name=\"transaction\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let superseded = http
        .post(format!("{origin}/oauth/authorize"))
        .header("Origin", &origin)
        .form(&[
            ("transaction", transaction),
            (
                "pairing_code",
                original_pairing.pairing_code.as_deref().unwrap(),
            ),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        superseded.status() == reqwest::StatusCode::BAD_REQUEST,
        "A superseded pairing code must no longer authorize access"
    );
    let approved = http
        .post(format!("{origin}/oauth/authorize"))
        .header("Origin", &origin)
        .form(&[
            ("transaction", transaction),
            ("pairing_code", pairing.pairing_code.as_deref().unwrap()),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        approved.status().is_redirection(),
        "Pairing must authorize the callback"
    );
    let location = url::Url::parse(
        approved
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let code = location
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    let tokens: serde_json::Value = http
        .post(format!("{origin}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", registration["client_id"].as_str().unwrap()),
            ("redirect_uri", callback),
            ("resource", pairing.mcp_url.as_deref().unwrap()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = tokens["access_token"]
        .as_str()
        .expect("OAuth access token present");
    let connected = snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::Connected)
    })
    .await;
    assert!(
        connected.pairing_code.is_none(),
        "Consumed pairing code must disappear from UI state"
    );
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.dots_secret_path()).unwrap()).unwrap();
    let mut request = format!(
        "{}/hosts/{}/connect",
        origin.replacen("http", "ws", 1),
        credential["host_id"].as_str().unwrap()
    )
    .into_client_request()
    .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", credential["host_key"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    let (mut replacement_socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("Host owner can replace the real service connection");
    snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::Failed { .. })
    })
    .await;
    replacement_socket.close(None).await.unwrap();
    fixture
        .client
        .dots_command(DotsCommandPayload::Retry)
        .await
        .unwrap();
    snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::Connected)
    })
    .await;
    let initialized = rpc(&http, &origin, token, "initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"boundary-test", "version":"1"}})).await;
    assert!(initialized["capabilities"]["tools"].is_object());
    let tools = rpc(&http, &origin, token, "tools/list", json!({})).await;
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "tyde_spawn_agent")
    );
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| matches!(
                tool["name"].as_str(),
                Some("tyde_spawn_agent" | "tyde_send_agent_message" | "tyde_interrupt_agent")
            ))
            .all(|tool| tool["annotations"]["readOnlyHint"] == false
                && tool["annotations"]["destructiveHint"] == true),
        "Remote mutations must advertise their destructive capability to ChatGPT"
    );
    let project_root = tempfile::tempdir().unwrap();
    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Dots project".into(),
            roots: vec![ProjectRootPath(
                project_root.path().to_str().unwrap().into(),
            )],
        })
        .await
        .unwrap();
    let project: ProjectNotifyPayload = fixture.next_frame_matching("Project created through protocol", |event| event.kind == FrameKind::ProjectNotify && event.parse_payload::<ProjectNotifyPayload>().is_ok_and(|p| matches!(p, ProjectNotifyPayload::Upsert {project} if project.name == "Dots project"))).await.parse_payload().unwrap();
    let ProjectNotifyPayload::Upsert { project } = project else {
        unreachable!()
    };
    let projects = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_list_projects", "arguments":{}}),
    )
    .await;
    let discovered = projects["structuredContent"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["id"] == project.id.0)
        .expect("Dots discovers the created project");
    assert!(
        discovered["name"] == "Dots project"
            && discovered["source"] == serde_json::to_value(&project.source).unwrap(),
        "Dots receives the project's displayed name and workspace metadata"
    );
    let options = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_list_launch_options", "arguments":{}}),
    )
    .await;
    assert!(
        options["structuredContent"]["result"]["launch_profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|profile| profile["id"] == "claude:dots"),
        "Dots must discover the explicitly configured launch profile"
    );
    assert!(
        options["structuredContent"]["result"]["default_launch_profile_id"] != "claude:dots",
        "The selected profile is not the host default"
    );
    let spawned = rpc(&http, &origin, token, "tools/call", json!({"name":"tyde_spawn_agent", "arguments":{"project_id":discovered["id"], "prompt":"Reply with a short greeting", "name":"Dots worker", "launch_profile_id":"claude:dots"}})).await;
    assert!(
        spawned["isError"] != true,
        "Agent must launch through production control path"
    );
    let agent_id = spawned["structuredContent"]["result"]["agent_id"]
        .as_str()
        .expect("Spawn returns explicit agent identity");
    let new_agent: NewAgentPayload = fixture
        .next_frame_matching("Remote spawn reaches the client", |event| {
            event.kind == FrameKind::NewAgent
                && event
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|agent| agent.agent_id.0 == agent_id)
        })
        .await
        .parse_payload()
        .unwrap();
    let remote = fixture::TestAgent {
        stream: new_agent.instance_stream.clone(),
        new_agent,
    };
    let start = fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "Remote agent bootstrap",
        |event| event.stream == remote.stream && event.kind == FrameKind::AgentStart,
    )
    .await;
    let start: AgentStartPayload = start.parse_payload().unwrap();
    assert!(
        start.launch_profile_id == Some(LaunchProfileId("claude:dots".into())),
        "The client must receive the explicitly selected launch profile"
    );
    let settings = fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "Selected profile's effective settings",
        |event| event.stream == remote.stream && event.kind == FrameKind::SessionSettings,
    )
    .await;
    let settings: SessionSettingsPayload = settings.parse_payload().unwrap();
    assert!(
        settings.values.0.get("model") == Some(&SessionSettingValue::String("sonnet".into()))
            && settings.values.0.get("effort") == Some(&SessionSettingValue::String("high".into())),
        "The client must receive the selected profile's effective model and effort"
    );
    let remote_reply = fixture
        .next_chat_event_matching(&remote, "Remote launch completes", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    fixture
        .next_chat_event_matching(&remote, "Remote launch becomes idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    let agents = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_list_agents", "arguments":{}}),
    )
    .await;
    assert!(
        agents["structuredContent"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["agent_id"] == agent_id)
    );
    let read = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_read_agent", "arguments":{"agent_id":agent_id}}),
    )
    .await;
    let read: AgentControlReadResult =
        serde_json::from_value(read["structuredContent"]["result"].clone()).unwrap();
    assert!(
        read.agent_id == remote.new_agent.agent_id,
        "Read returns the requested agent identity"
    );
    assert!(
        read.status == AgentControlStatus::Idle,
        "Completed remote launch is idle"
    );
    let ChatEvent::StreamEnd(remote_reply) = remote_reply else {
        unreachable!()
    };
    assert!(
        read.output
            == AgentControlOutput::Message {
                text: remote_reply.message.content
            },
        "Read output must exactly match the client's completed assistant message"
    );
    use server::backend::mock::{MockScript, MockTurn};
    let held = fixture
        .spawn_scripted(
            "Dots controlled turn",
            MockScript::one(MockTurn::held_text("Initial held reply"))
                .then(MockTurn::held_echo())
                .then(MockTurn::echo())
                .with_mid_turn_steering()
                .with_user_bubbles(),
        )
        .await;
    fixture.next_chat_event_matching(&held, "Initial held response", |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "Initial held reply")).await;
    let held_read = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_read_agent", "arguments":{"agent_id":held.new_agent.agent_id}}),
    )
    .await;
    let held_read: AgentControlReadResult =
        serde_json::from_value(held_read["structuredContent"]["result"].clone()).unwrap();
    assert!(
        held_read.agent_id == held.new_agent.agent_id,
        "Held read preserves agent identity"
    );
    assert!(
        held_read.status == AgentControlStatus::Thinking,
        "A held turn remains thinking even after assistant output"
    );
    assert!(
        held_read.output
            == AgentControlOutput::Message {
                text: "Initial held reply".into()
            },
        "Held read returns the exact completed assistant output"
    );
    let steered = rpc(&http, &origin, token, "tools/call", json!({"name":"tyde_send_agent_message", "arguments":{"agent_id":held.new_agent.agent_id, "message":"Steer this held turn", "interrupt":false}})).await;
    assert!(steered["isError"] != true);
    let mut cancelled = false;
    fixture.next_chat_event_matching(&held, "Steering visible to the client", |event| {
        cancelled |= matches!(event, ChatEvent::OperationCancelled(_));
        matches!(event, ChatEvent::MessageAdded(message) if message.content == "Steer this held turn")
    }).await;
    assert!(!cancelled, "Steering must preserve the current turn");
    let interrupted = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_interrupt_agent", "arguments":{"agent_id":held.new_agent.agent_id}}),
    )
    .await;
    assert!(interrupted["isError"] != true);
    fixture
        .next_chat_event_matching(
            &held,
            "Standalone interruption reaches the client",
            |event| matches!(event, ChatEvent::OperationCancelled(_)),
        )
        .await;
    fixture
        .next_chat_event_matching(&held, "Interrupted turn becomes idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    rpc(&http, &origin, token, "tools/call", json!({"name":"tyde_send_agent_message", "arguments":{"agent_id":held.new_agent.agent_id, "message":"Start another held turn", "interrupt":false}})).await;
    fixture
        .next_chat_event_matching(&held, "Follow-up starts a new turn", |event| {
            matches!(event, ChatEvent::StreamStart(_))
        })
        .await;
    let replaced = rpc(&http, &origin, token, "tools/call", json!({"name":"tyde_send_agent_message", "arguments":{"agent_id":held.new_agent.agent_id, "message":"Reply to the replacement task", "interrupt":true}})).await;
    assert!(replaced["isError"] != true);
    fixture
        .next_chat_event_matching(&held, "Replacement cancels the held turn", |event| {
            matches!(event, ChatEvent::OperationCancelled(_))
        })
        .await;
    let replacement = fixture.next_chat_event_matching(&held, "Replacement reply reaches the client", |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content.contains("Reply to the replacement task"))).await;
    fixture
        .next_chat_event_matching(&held, "Replacement finishes", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    let completed_read = rpc(
        &http,
        &origin,
        token,
        "tools/call",
        json!({"name":"tyde_read_agent", "arguments":{"agent_id":held.new_agent.agent_id}}),
    )
    .await;
    let completed_read: AgentControlReadResult =
        serde_json::from_value(completed_read["structuredContent"]["result"].clone()).unwrap();
    assert!(
        completed_read.agent_id == held.new_agent.agent_id,
        "Completed read preserves agent identity"
    );
    assert!(
        completed_read.status == AgentControlStatus::Idle,
        "Replacement turn becomes idle"
    );
    let ChatEvent::StreamEnd(replacement) = replacement else {
        unreachable!()
    };
    assert!(
        completed_read.output
            == AgentControlOutput::Message {
                text: replacement.message.content
            },
        "Completed read returns exactly the assistant message observed over the protocol"
    );
    configuration_gate.send_replace(false);
    let disabled_write = fixture
        .client
        .replace_setting("/chatgpt_dots_enabled", false, true)
        .await
        .unwrap();
    fixture
        .next_frame_matching(
            "Disable setting committed before transport reconfiguration",
            |event| {
                event.kind == FrameKind::SettingsWriteResult
                    && event
                        .parse_payload::<SettingsWriteResultPayload>()
                        .is_ok_and(|result| result.write_id == disabled_write && result.applied)
            },
        )
        .await;
    let guarded = http.post(format!("{origin}/mcp")).bearer_auth(token).json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tyde_spawn_agent","arguments":{"project_id":project.id,"prompt":"Must not run during disable window"}}})).send().await.unwrap();
    assert!(
        guarded.status().is_success(),
        "The connected transport must reach the independent host enforcement boundary"
    );
    let guarded: serde_json::Value = guarded.json().await.unwrap();
    // Remote MCP is the renamed public error; keep the transport and no-spawn guards.
    assert!(
        guarded["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Remote MCP is disabled")),
        "Committed disabled settings must reject the MCP operation before transport teardown"
    );
    fixture::assert_no_interesting_frame_on(
        &mut fixture.client,
        std::time::Duration::from_millis(100),
        "disabled MCP spawn",
        |event| event.kind != FrameKind::NewAgent,
    )
    .await;
    configuration_gate.send_replace(true);
    snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::Disabled)
    })
    .await;
    let disabled = http.post(format!("{origin}/mcp")).bearer_auth(token).json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tyde_spawn_agent","arguments":{"project_id":project.id,"prompt":"Must not run"}}})).send().await.unwrap();
    assert!(
        !disabled.status().is_success(),
        "Disabled connection must reject remote work"
    );
    fixture
        .client
        .replace_setting("/chatgpt_dots_enabled", true, false)
        .await
        .unwrap();
    snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::Connected)
    })
    .await;
    fixture
        .client
        .dots_command(DotsCommandPayload::Disconnect)
        .await
        .unwrap();
    snapshot(&mut fixture, |status| {
        matches!(status, DotsConnectionStatus::AwaitingPairing)
    })
    .await;
    let revoked = http
        .post(format!("{origin}/mcp"))
        .bearer_auth(token)
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);
    drop(fixture);
    drop(service.stdin.take());
    let exit = tokio::time::timeout(std::time::Duration::from_secs(10), service.wait())
        .await
        .expect("Service disposal deadline")
        .expect("Service exits cleanly");
    assert!(exit.success(), "Worker subprocess must dispose its runtime");
}
