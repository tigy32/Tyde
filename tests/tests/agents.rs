mod fixture;

use fixture::Fixture;
use protocol::types::AgentClosedPayload;
use protocol::{
    AgentErrorPayload, AgentOrigin, AgentRenamedPayload, AgentStartPayload, BackendKind, ChatEvent,
    CommandErrorCode, CommandErrorPayload, Envelope, FrameKind, ListSessionsPayload,
    NewAgentPayload, Project, ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload,
    ProjectId, ProjectNotifyPayload, ProjectRenamePayload, SessionListPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamPath,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tyde_dev_driver::agent_control::SpawnRequest;

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };

        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionList
        ) {
            continue;
        }

        return env;
    }
}

/// Wait for the first envelope of a specific kind, skipping noise frames.
async fn expect_kind(client: &mut client::Connection, kind: FrameKind, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
        ) {
            continue;
        }
        if env.kind == kind {
            return env;
        }
        // Skip other frame kinds while waiting for the target kind.
    }
}

/// Like expect_next_event but also skips proactive SessionList fan-outs that
/// are emitted on agent lifecycle transitions (start, terminate, rename).
async fn expect_chat_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::SessionList {
            continue;
        }
        return env;
    }
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    let env = expect_kind(client, FrameKind::CommandError, context).await;
    env.parse_payload()
        .expect("failed to parse CommandErrorPayload")
}

async fn expect_turn(client: &mut client::Connection, expected_text: &str) {
    let env = expect_chat_event(client, "TypingStatusChanged(true)").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    let env = expect_chat_event(client, "StreamStart").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamStart(..)));

    let env = expect_chat_event(client, "StreamDelta").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta, got {other:?}"),
    }

    let env = expect_chat_event(client, "StreamEnd").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let env = expect_chat_event(client, "TypingStatusChanged(false)").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env)))
                if matches!(
                    env.kind,
                    FrameKind::SessionSettings
                        | FrameKind::SessionSchemas
                        | FrameKind::BackendSetup
                        | FrameKind::QueuedMessages
                        | FrameKind::SessionList
                        | FrameKind::HostSettings
                ) =>
            {
                continue;
            }
            Ok(Ok(Some(env))) => panic!(
                "unexpected event before {context}: kind={} stream={}",
                env.kind, env.stream
            ),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

fn parse_http_url(url: &str) -> (&str, &str) {
    let without_scheme = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("expected http:// URL, got {url}"));
    let slash = without_scheme
        .find('/')
        .unwrap_or_else(|| panic!("expected path in URL {url}"));
    (&without_scheme[..slash], &without_scheme[slash..])
}

async fn post_json(url: &str, body: &Value) -> Value {
    let (addr, target) = parse_http_url(url);
    let mut stream = TcpStream::connect(addr)
        .await
        .unwrap_or_else(|err| panic!("connect {addr} failed: {err}"));
    let body_bytes = serde_json::to_vec(body).expect("serialize HTTP JSON body");
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write HTTP request header");
    stream
        .write_all(&body_bytes)
        .await
        .expect("write HTTP request body");
    stream.flush().await.expect("flush HTTP request");

    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .await
        .expect("read HTTP response");
    let header_end = response_bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response missing header terminator");
    let header = std::str::from_utf8(&response_bytes[..header_end]).expect("response header utf8");
    assert!(
        header.starts_with("HTTP/1.1 200"),
        "unexpected HTTP response header: {header}"
    );
    let body = std::str::from_utf8(&response_bytes[header_end + 4..]).expect("response body utf8");
    // The MCP server returns SSE (text/event-stream). Extract the JSON from the "data: " line.
    let json_str = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or_else(|| panic!("no SSE data line in response body: {body}"));
    serde_json::from_str(json_str).expect("parse SSE JSON response")
}

async fn mcp_spawn_agent(url: &str, prompt: &str, name: &str) -> protocol::AgentId {
    let response = post_json(
        url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_spawn_agent",
                "arguments": {
                    "workspace_roots": ["/tmp/agent-control-mcp-parent-url"],
                    "prompt": prompt,
                    "backend_kind": "claude",
                    "name": name
                }
            }
        }),
    )
    .await;
    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("MCP response missing result: {response}"));
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(!is_error, "MCP tool call failed: {response}");
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("MCP response missing content text: {response}"));
    let payload: Value = serde_json::from_str(text).expect("parse MCP tool payload JSON");
    let agent_id = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .expect("spawn result missing agent_id");
    protocol::AgentId(agent_id.to_string())
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectNotify);
    env.parse_payload()
        .expect("failed to parse ProjectNotifyPayload")
}

fn assert_completed_agent_result(
    result: &tyde_dev_driver::agent_control::AgentResult,
    expected_text: &str,
) {
    assert_eq!(result.status, "idle");
    assert!(
        result
            .message
            .as_deref()
            .is_some_and(|message| message.contains(expected_text)),
        "expected completed result message to contain '{expected_text}', got {:?}",
        result.message
    );
}

const MOCK_NATIVE_CHILD_SENTINEL: &str = "__mock_spawn_native_child__";

async fn expect_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(true)").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    let env = expect_chat_event_on_stream(client, stream, "StreamStart").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamStart(..)));

    let env = expect_chat_event_on_stream(client, stream, "StreamDelta").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta.text.contains(expected_text),
                "unexpected delta text on {}: {}",
                stream,
                delta.text,
            );
        }
        other => panic!("expected StreamDelta on {stream}, got {other:?}"),
    }

    let env = expect_chat_event_on_stream(client, stream, "StreamEnd").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));

    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(false)").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

async fn expect_chat_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> Envelope {
    loop {
        let env = expect_chat_event(client, context).await;
        if env.kind == FrameKind::ChatEvent && env.stream == *stream {
            return env;
        }
    }
}

async fn expect_agent_error_message(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_message: &str,
    context: &str,
) -> AgentErrorPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::AgentError || env.stream != *stream {
            continue;
        }
        let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
        if payload.message == expected_message {
            return payload;
        }
    }
}

async fn expect_connection_close(client: &mut client::Connection, context: &str) {
    loop {
        match client
            .next_event()
            .await
            .expect("next_event while waiting for connection close failed")
        {
            None => return,
            Some(env)
                if matches!(
                    env.kind,
                    FrameKind::HostSettings
                        | FrameKind::SessionSettings
                        | FrameKind::SessionSchemas
                        | FrameKind::BackendSetup
                        | FrameKind::QueuedMessages
                        | FrameKind::SessionList
                ) =>
            {
                continue;
            }
            Some(env) => panic!(
                "expected connection close before {context}, got kind={} stream={}",
                env.kind, env.stream
            ),
        }
    }
}

async fn expect_replayed_new_agent(
    client: &mut client::Connection,
    agent_id: &protocol::AgentId,
    context: &str,
) -> NewAgentPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
        if &payload.agent_id == agent_id {
            return payload;
        }
    }
}

async fn expect_agent_start_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::AgentStart || env.stream != *stream {
            continue;
        }
        return env.parse_payload().expect("parse AgentStart");
    }
}

async fn spawn_parent_with_native_child(
    client: &mut client::Connection,
) -> (
    NewAgentPayload,
    AgentStartPayload,
    NewAgentPayload,
    AgentStartPayload,
) {
    let parent_prompt = format!("parent prompt {MOCK_NATIVE_CHILD_SENTINEL}");
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("parent-with-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/sub-agent-parent".to_owned()],
                prompt: parent_prompt,
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent with native child failed");

    let env = expect_next_event(client, "parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");

    let env = expect_next_event(client, "parent AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, parent_new.instance_stream);
    let parent_start: AgentStartPayload = env.parse_payload().expect("parse parent AgentStart");

    expect_turn_on_stream(
        client,
        &parent_new.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;

    let env = expect_next_event(client, "native child NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let child_new: NewAgentPayload = env.parse_payload().expect("parse native child NewAgent");

    let env = expect_next_event(client, "native child AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, child_new.instance_stream);
    let child_start: AgentStartPayload =
        env.parse_payload().expect("parse native child AgentStart");

    expect_turn_on_stream(
        client,
        &child_new.instance_stream,
        "mock native child response to: parent prompt",
    )
    .await;

    (parent_new, parent_start, child_new, child_start)
}

#[tokio::test]
async fn agent_lifecycle() {
    let mut fixture = Fixture::new().await;

    // 1. Spawn an agent
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("test-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    // 2. Receive NewAgent on host stream
    let env = expect_next_event(&mut fixture.client, "NewAgent").await;

    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload");
    assert!(!new_agent.agent_id.0.is_empty());
    assert_eq!(new_agent.backend_kind, BackendKind::Claude);
    assert_eq!(new_agent.name, "test-agent");
    let agent_stream = new_agent.instance_stream.clone();

    // 3. Receive AgentStart
    let env = expect_next_event(&mut fixture.client, "AgentStart").await;

    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);
    assert_eq!(env.seq, 0);

    let start: AgentStartPayload = env
        .parse_payload()
        .expect("failed to parse AgentStartPayload");
    assert!(!start.agent_id.0.is_empty());
    assert_eq!(start.backend_kind, BackendKind::Claude);
    assert_eq!(start.name, "test-agent");

    // 4. Receive mock's initial turn: StreamStart → StreamDelta → StreamEnd
    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    // 5. Send a follow-up message
    fixture
        .client
        .send_message(&agent_stream, "follow up".to_owned())
        .await
        .expect("send_message failed");

    // 6. Receive follow-up turn: StreamStart → StreamDelta → StreamEnd
    expect_turn(&mut fixture.client, "mock backend response to: follow up").await;
}

#[tokio::test]
async fn close_agent_emits_agent_closed_and_removes_agent_from_registry() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("close-me".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "close-agent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse close-agent NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "close-agent AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);

    expect_turn(&mut fixture.client, "mock backend response to: hello").await;
    assert!(
        fixture.agent_ids().await.contains(&new_agent.agent_id),
        "agent should be registered before close"
    );

    fixture
        .client
        .close_agent(&agent_stream)
        .await
        .expect("close_agent failed");

    let env = expect_kind(&mut fixture.client, FrameKind::AgentClosed, "AgentClosed").await;
    let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
    assert_eq!(closed.agent_id, new_agent.agent_id);
    assert!(
        !fixture.agent_ids().await.contains(&new_agent.agent_id),
        "agent should be removed from registry after close"
    );

    let mut late_client = fixture.connect().await;
    expect_no_event(
        &mut late_client,
        Duration::from_millis(200),
        "closed agent should not replay to new clients",
    )
    .await;
}

#[tokio::test]
async fn close_agent_mid_turn_flushes_final_events_before_agent_closed() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("close-mid-turn".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "__mock_slow__ close mid turn".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "close-mid-turn NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse close-mid-turn NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "close-mid-turn AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);

    let env = expect_chat_event_on_stream(
        &mut fixture.client,
        &agent_stream,
        "TypingStatusChanged(true)",
    )
    .await;
    let event: ChatEvent = env
        .parse_payload()
        .expect("parse close-mid-turn typing true");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    fixture
        .client
        .close_agent(&agent_stream)
        .await
        .expect("close_agent failed");

    let mut saw_stream_start = false;
    let mut saw_stream_delta = false;
    let mut saw_stream_end = false;
    let mut saw_typing_false = false;

    loop {
        let env = expect_next_event(&mut fixture.client, "close-mid-turn trailing events").await;
        if env.kind == FrameKind::AgentClosed {
            let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
            assert_eq!(closed.agent_id, new_agent.agent_id);
            break;
        }
        if env.kind != FrameKind::ChatEvent || env.stream != agent_stream {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse close-mid-turn ChatEvent");
        match event {
            ChatEvent::StreamStart(_) => saw_stream_start = true,
            ChatEvent::StreamDelta(_) => saw_stream_delta = true,
            ChatEvent::StreamEnd(_) => saw_stream_end = true,
            ChatEvent::TypingStatusChanged(false) => saw_typing_false = true,
            _ => {}
        }
    }

    assert!(saw_stream_start, "close should not drop StreamStart");
    assert!(saw_stream_delta, "close should not drop StreamDelta");
    assert!(saw_stream_end, "close should not drop StreamEnd");
    assert!(
        saw_typing_false,
        "close should not drop TypingStatusChanged(false)"
    );
}

#[tokio::test]
async fn agent_control_end_to_end_flow_uses_full_stack() {
    let fixture = Fixture::new().await;
    let control = fixture.connect_agent_control().await;

    let spawned = control
        .spawn_agent(SpawnRequest {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: "agent control hello".to_owned(),
            backend_kind: BackendKind::Claude,
            parent_agent_id: None,
            project_id: None,
            name: Some("agent-control".to_owned()),
            cost_hint: None,
        })
        .await
        .expect("agent control spawn should succeed");

    let listed_before_wait = control.list_agents().await;
    assert_eq!(listed_before_wait.len(), 1);
    assert_eq!(listed_before_wait[0].agent_id, spawned.agent_id);
    assert_eq!(listed_before_wait[0].name, "agent-control");
    assert_eq!(listed_before_wait[0].backend_kind, BackendKind::Claude);
    assert_eq!(
        listed_before_wait[0].workspace_roots,
        vec!["/tmp/test".to_owned()]
    );

    let awaited = control
        .await_agents(
            Some(vec![protocol::AgentId(spawned.agent_id.clone())]),
            Some(5_000),
        )
        .await
        .expect("agent control await should succeed");
    assert!(awaited.still_running.is_empty());
    assert_eq!(awaited.ready.len(), 1);
    assert_completed_agent_result(
        &awaited.ready[0],
        "mock backend response to: agent control hello",
    );

    let listed_after_wait = control.list_agents().await;
    assert_eq!(listed_after_wait.len(), 1);
    assert_eq!(listed_after_wait[0].status, "idle");
    assert!(
        listed_after_wait[0].last_message.as_deref().is_some_and(
            |message| message.contains("mock backend response to: agent control hello")
        )
    );

    control
        .send_message(
            protocol::AgentId(spawned.agent_id.clone()),
            "agent control follow up".to_owned(),
        )
        .await
        .expect("agent control send_message should succeed");

    let awaited_follow_up = control
        .await_agents(
            Some(vec![protocol::AgentId(spawned.agent_id.clone())]),
            Some(5_000),
        )
        .await
        .expect("agent control follow-up await should succeed");
    assert!(awaited_follow_up.still_running.is_empty());
    assert_eq!(awaited_follow_up.ready.len(), 1);
    assert_completed_agent_result(
        &awaited_follow_up.ready[0],
        "mock backend response to: agent control follow up",
    );

    let listed_after_follow_up = control.list_agents().await;
    assert_eq!(listed_after_follow_up.len(), 1);
    assert_eq!(listed_after_follow_up[0].status, "idle");
    assert!(
        listed_after_follow_up[0]
            .last_message
            .as_deref()
            .is_some_and(
                |message| message.contains("mock backend response to: agent control follow up")
            )
    );
    assert!(
        listed_after_follow_up[0].summary.is_some(),
        "agent control list should surface a summary once a turn completes"
    );
}

#[tokio::test]
async fn agent_control_spawn_without_name_returns_generated_name() {
    let fixture = Fixture::new().await;
    let control = fixture.connect_agent_control().await;

    let spawned = control
        .spawn_agent(SpawnRequest {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: "review auth logs".to_owned(),
            backend_kind: BackendKind::Claude,
            parent_agent_id: None,
            project_id: None,
            name: None,
            cost_hint: None,
        })
        .await
        .expect("agent control spawn without name should succeed");

    assert_eq!(spawned.name, "Review Auth Logs");

    let listed = control.list_agents().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Review Auth Logs");
    assert_eq!(listed[0].agent_id, spawned.agent_id);
}

#[tokio::test]
async fn agent_control_http_infers_parent_agent_id_from_request_url() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("mcp-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/mcp-parent".to_owned()],
                prompt: "__mock_slow__ parent stays active".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "mcp parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse mcp parent NewAgent");

    let env = expect_next_event(&mut fixture.client, "mcp parent AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let parent_start: AgentStartPayload = env.parse_payload().expect("parse mcp parent AgentStart");
    assert_eq!(parent_start.parent_agent_id, None);

    let base_url = fixture.agent_control_http_url().await;
    let child_agent_id = mcp_spawn_agent(
        &format!("{base_url}?agent_id={}", parent_new.agent_id.0),
        "child from inferred MCP parent",
        "mcp-child",
    )
    .await;

    let child_new =
        expect_replayed_new_agent(&mut fixture.client, &child_agent_id, "mcp child NewAgent").await;
    assert_eq!(
        child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mcp child AgentStart",
    )
    .await;
    assert_eq!(
        child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );
}

#[tokio::test]
async fn agent_control_http_respects_explicit_parent_agent_id_in_tool_arguments() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("explicit-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/explicit-parent".to_owned()],
                prompt: "parent".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "explicit parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse explicit parent NewAgent");

    let _ = expect_next_event(&mut fixture.client, "explicit parent AgentStart").await;
    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent",
    )
    .await;

    let base_url = fixture.agent_control_http_url().await;
    let response = post_json(
        &base_url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_spawn_agent",
                "arguments": {
                    "workspace_roots": ["/tmp/explicit-parent-child"],
                    "prompt": "child with explicit parent",
                    "backend_kind": "claude",
                    "name": "explicit-child",
                    "parent_agent_id": parent_new.agent_id.0
                }
            }
        }),
    )
    .await;

    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("MCP response missing result: {response}"));
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(!is_error, "MCP tool call failed: {response}");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("missing content text");
    let payload: Value = serde_json::from_str(text).expect("parse spawn result JSON");
    let child_agent_id = protocol::AgentId(
        payload["agent_id"]
            .as_str()
            .expect("missing agent_id")
            .to_owned(),
    );

    let child_new = expect_replayed_new_agent(
        &mut fixture.client,
        &child_agent_id,
        "explicit child NewAgent",
    )
    .await;
    assert_eq!(
        child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id),
        "child spawned with explicit parent_agent_id must have that parent set"
    );

    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "explicit child AgentStart",
    )
    .await;
    assert_eq!(
        child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id),
        "AgentStart must reflect explicit parent_agent_id"
    );
}

#[tokio::test]
async fn agent_origin_is_user_for_normal_spawns() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("user-origin".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/user-origin".to_owned()],
                prompt: "user origin".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("user spawn failed");

    let env = expect_next_event(&mut fixture.client, "user-origin NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let user_new: NewAgentPayload = env.parse_payload().expect("parse user-origin NewAgent");
    assert_eq!(user_new.origin, AgentOrigin::User);
    assert_eq!(user_new.parent_agent_id, None);

    let env = expect_next_event(&mut fixture.client, "user-origin AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let user_start: AgentStartPayload = env.parse_payload().expect("parse user-origin AgentStart");
    assert_eq!(user_start.origin, AgentOrigin::User);
    assert_eq!(user_start.parent_agent_id, None);

    expect_turn_on_stream(
        &mut fixture.client,
        &user_new.instance_stream,
        "mock backend response to: user origin",
    )
    .await;
}

#[tokio::test]
async fn backend_native_child_is_first_class_and_replays_to_late_subscribers() {
    let mut fixture = Fixture::new().await;

    let (parent_new, _parent_start, child_new, child_start) =
        spawn_parent_with_native_child(&mut fixture.client).await;

    assert_eq!(child_new.origin, AgentOrigin::BackendNative);
    assert_eq!(child_start.origin, AgentOrigin::BackendNative);
    assert_eq!(
        child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id),
        "backend-native child must point to its live parent",
    );
    assert_eq!(
        child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id),
        "backend-native child AgentStart must point to its live parent",
    );

    let control = fixture.connect_agent_control().await;
    let listed = control.list_agents().await;
    assert_eq!(
        listed.len(),
        2,
        "agent-control list should include native child"
    );
    let listed_child = listed
        .iter()
        .find(|agent| agent.agent_id == child_new.agent_id.0)
        .expect("native child missing from agent-control list");
    assert_eq!(
        listed_child.parent_agent_id.as_deref(),
        Some(parent_new.agent_id.0.as_str())
    );

    let mut late_client = fixture.connect().await;
    let replayed_child_new =
        expect_replayed_new_agent(&mut late_client, &child_new.agent_id, "late child NewAgent")
            .await;
    assert_eq!(replayed_child_new.origin, AgentOrigin::BackendNative);
    assert_eq!(
        replayed_child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    let replayed_child_start = expect_agent_start_on_stream(
        &mut late_client,
        &replayed_child_new.instance_stream,
        "late child AgentStart",
    )
    .await;
    assert_eq!(replayed_child_start.origin, AgentOrigin::BackendNative);
    assert_eq!(
        replayed_child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    expect_turn_on_stream(
        &mut late_client,
        &replayed_child_new.instance_stream,
        "mock native child response to: parent prompt",
    )
    .await;
}

#[tokio::test]
async fn backend_native_child_does_not_emit_completion_notice_to_parent() {
    let mut fixture = Fixture::new().await;

    let _ = spawn_parent_with_native_child(&mut fixture.client).await;

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(200),
        "backend-native child completion follow-up",
    )
    .await;
}

#[tokio::test]
async fn backend_native_child_sessions_are_non_resumable() {
    let mut fixture = Fixture::new().await;

    let _ = spawn_parent_with_native_child(&mut fixture.client).await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after native child spawn failed");

    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "native child SessionList",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse native child SessionList");
    assert_eq!(list.sessions.len(), 2);

    let parent = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some("parent-with-native-child"))
        .expect("missing parent session");
    let child = list
        .sessions
        .iter()
        .find(|session| session.alias.as_deref() == Some("mock-native-child"))
        .expect("missing backend-native child session");

    assert_eq!(child.parent_id.as_ref(), Some(&parent.id));
    assert!(
        !child.resumable,
        "backend-native child sessions must be marked non-resumable"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resume-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: child.id.clone(),
                prompt: Some("should fail".to_owned()),
            },
        })
        .await
        .expect("resume-native-child write failed");

    expect_connection_close(
        &mut fixture.client,
        "resume of non-resumable backend-native child session",
    )
    .await;
}

#[tokio::test]
async fn cancelling_parent_cascades_to_user_children_and_closes_relay_children() {
    let mut fixture = Fixture::new().await;

    let (parent_new, _parent_start, relay_new, _relay_start) =
        spawn_parent_with_native_child(&mut fixture.client).await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("user-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_new.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/user-child".to_owned()],
                prompt: "user child".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn user child failed");

    let env = expect_next_event(&mut fixture.client, "user child NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let user_child_new: NewAgentPayload = env.parse_payload().expect("parse user child NewAgent");
    assert_eq!(
        user_child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    let env = expect_next_event(&mut fixture.client, "user child AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let user_child_start: AgentStartPayload =
        env.parse_payload().expect("parse user child AgentStart");
    assert_eq!(user_child_start.origin, AgentOrigin::User);
    assert_eq!(
        user_child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    expect_turn_on_stream(
        &mut fixture.client,
        &user_child_new.instance_stream,
        "mock backend response to: user child",
    )
    .await;

    fixture
        .client
        .interrupt(&parent_new.instance_stream)
        .await
        .expect("interrupt parent failed");

    let parent_err = expect_agent_error_message(
        &mut fixture.client,
        &parent_new.instance_stream,
        "agent backend closed",
        "parent termination after interrupt",
    )
    .await;
    assert!(parent_err.fatal);

    let user_child_err = expect_agent_error_message(
        &mut fixture.client,
        &user_child_new.instance_stream,
        "agent backend closed",
        "user child termination after parent interrupt",
    )
    .await;
    assert!(
        user_child_err.fatal,
        "user-spawned child should terminate when parent is cancelled"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    fixture
        .client
        .send_message(&user_child_new.instance_stream, "after cancel".to_owned())
        .await
        .expect("send_message to terminated user child should still write protocol frame");
    let user_child_not_running = expect_agent_error_message(
        &mut fixture.client,
        &user_child_new.instance_stream,
        "agent not running",
        "terminated user child should reject follow-up input",
    )
    .await;
    assert!(!user_child_not_running.fatal);

    fixture
        .client
        .send_message(&relay_new.instance_stream, "after relay close".to_owned())
        .await
        .expect("send_message to terminated relay child should still write protocol frame");
    let relay_not_running = expect_agent_error_message(
        &mut fixture.client,
        &relay_new.instance_stream,
        "agent not running",
        "relay child should terminate after parent backend closes its event channel",
    )
    .await;
    assert!(!relay_not_running.fatal);
}

#[tokio::test]
async fn backend_spawn_failure_emits_terminal_agent_error_without_panicking_host() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("spawn-failure".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "__mock_fail_spawn__".to_owned(),
                images: None,
                backend_kind: BackendKind::Tycode,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "failed NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse failed NewAgent");
    assert_eq!(new_agent.name, "spawn-failure");
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "failed AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);
    let start: AgentStartPayload = env.parse_payload().expect("parse failed AgentStart");
    assert_eq!(start.name, "spawn-failure");
    assert_eq!(start.backend_kind, BackendKind::Tycode);

    let env = expect_next_event(&mut fixture.client, "fatal AgentError").await;
    assert_eq!(env.kind, FrameKind::AgentError);
    assert_eq!(env.stream, agent_stream);
    let err: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
    assert!(
        err.fatal,
        "startup failure should terminate the agent stream"
    );
    assert_eq!(err.code, protocol::AgentErrorCode::BackendFailed);
    assert!(
        err.message.contains("mock backend forced spawn failure"),
        "unexpected startup failure message: {}",
        err.message
    );

    fixture
        .client
        .send_message(&agent_stream, "after failure".to_owned())
        .await
        .expect("send_message after failure should still write protocol frame");

    let env = expect_next_event(&mut fixture.client, "agent not running error").await;
    assert_eq!(env.kind, FrameKind::AgentError);
    assert_eq!(env.stream, agent_stream);
    let err: AgentErrorPayload = env.parse_payload().expect("parse post-failure AgentError");
    assert!(!err.fatal, "follow-up router error should not be fatal");
    assert_eq!(err.message, "agent not running");
}

#[tokio::test]
async fn spawn_without_name_generates_short_name_and_persists_alias() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "review auth logs".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn without explicit name failed");

    let env = expect_next_event(&mut fixture.client, "generated-name NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse generated NewAgent");
    assert_eq!(new_agent.name, "Review Auth Logs");

    let env = expect_next_event(&mut fixture.client, "generated-name AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let start: AgentStartPayload = env.parse_payload().expect("parse generated AgentStart");
    assert_eq!(start.name, "Review Auth Logs");

    expect_turn(
        &mut fixture.client,
        "mock backend response to: review auth logs",
    )
    .await;

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions after generated name failed");

    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "generated-name SessionList",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse generated SessionList");
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(list.sessions[0].alias.as_deref(), Some("Review Auth Logs"));
    assert_eq!(list.sessions[0].user_alias, None);
}

#[tokio::test]
async fn renaming_agent_updates_live_streams_and_replay() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Original Name".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn for rename test failed");

    let env = expect_next_event(&mut fixture.client, "rename test NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse rename NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "rename test AgentStart").await;
    let start: AgentStartPayload = env.parse_payload().expect("parse rename AgentStart");
    assert_eq!(start.name, "Original Name");

    expect_turn(&mut fixture.client, "mock backend response to: hello").await;

    fixture
        .client
        .set_agent_name(&agent_stream, "Renamed Agent".to_owned())
        .await
        .expect("set_agent_name failed");

    let env = expect_next_event(&mut fixture.client, "AgentRenamed").await;
    assert_eq!(env.kind, FrameKind::AgentRenamed);
    let renamed: AgentRenamedPayload = env.parse_payload().expect("parse AgentRenamed");
    assert_eq!(renamed.agent_id, new_agent.agent_id);
    assert_eq!(renamed.name, "Renamed Agent");

    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "rename SessionList",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse rename SessionList");
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(
        list.sessions[0].user_alias.as_deref(),
        Some("Renamed Agent")
    );

    let mut late_client = fixture.connect().await;
    let env = expect_next_event(&mut late_client, "late renamed NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let replayed_agent: NewAgentPayload = env.parse_payload().expect("parse replayed NewAgent");
    assert_eq!(replayed_agent.name, "Renamed Agent");

    let env = expect_next_event(&mut late_client, "late renamed AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let replayed_start: AgentStartPayload = env.parse_payload().expect("parse replayed AgentStart");
    assert_eq!(replayed_start.name, "Renamed Agent");
}

#[tokio::test]
async fn multiple_agents() {
    let mut fixture = Fixture::new().await;

    // Spawn two agents
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("first".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "agent one".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn first agent failed");

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("second".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "agent two".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn second agent failed");

    // Collect all events from both agents, filtering out SessionSettings/SessionSchemas.
    // Each agent produces:
    //   NewAgent (host stream) + AgentStart + TypingStatusChanged(true) + StreamStart + StreamDelta + StreamEnd + TypingStatusChanged(false)
    // Two agents = 14 events total (after filtering).
    let mut events = Vec::new();
    while events.len() < 14 {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("next_event failed")
            .expect("connection closed before all events received");
        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionList
        ) {
            continue;
        }
        events.push(env);
    }

    let new_agent_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == FrameKind::NewAgent)
        .collect();
    assert_eq!(new_agent_events.len(), 2, "expected 2 NewAgent events");

    // Collect unique agent streams from NewAgent payloads
    let streams: std::collections::HashSet<String> = new_agent_events
        .iter()
        .map(|env| {
            let payload: NewAgentPayload = env
                .parse_payload()
                .expect("failed to parse NewAgentPayload");
            payload.instance_stream.0
        })
        .collect();
    assert_eq!(
        streams.len(),
        2,
        "expected events on exactly 2 agent streams"
    );

    // For each stream, verify the agent event sequence
    for stream in &streams {
        let stream_events: Vec<_> = events
            .iter()
            .filter(|e| e.stream.0 == *stream && e.kind != FrameKind::NewAgent)
            .collect();

        assert_eq!(
            stream_events.len(),
            6,
            "expected 6 events on stream {stream}",
        );

        // First event must be AgentStart at seq 0
        assert_eq!(stream_events[0].kind, FrameKind::AgentStart);
        assert_eq!(stream_events[0].seq, 0);

        // Remaining 5 must be ChatEvents with increasing seqs.
        // SessionSettings events can now be interleaved on the agent stream and
        // filtered out above, so ChatEvent sequence numbers are no longer
        // guaranteed to be contiguous after filtering.
        let mut prev_seq = stream_events[0].seq;
        for env in &stream_events[1..] {
            assert_eq!(env.kind, FrameKind::ChatEvent);
            assert!(
                env.seq > prev_seq,
                "expected increasing seqs on stream {stream}, got {} after {}",
                env.seq,
                prev_seq
            );
            prev_seq = env.seq;
        }

        // Parse the ChatEvents: TypingStatusChanged(true), StreamStart, StreamDelta, StreamEnd, TypingStatusChanged(false)
        let event: ChatEvent = stream_events[1]
            .parse_payload()
            .expect("failed to parse TypingStatusChanged(true)");
        assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

        let event: ChatEvent = stream_events[2]
            .parse_payload()
            .expect("failed to parse StreamStart");
        assert!(matches!(event, ChatEvent::StreamStart(..)));

        let event: ChatEvent = stream_events[3]
            .parse_payload()
            .expect("failed to parse StreamDelta");
        assert!(matches!(event, ChatEvent::StreamDelta(..)));

        let event: ChatEvent = stream_events[4]
            .parse_payload()
            .expect("failed to parse StreamEnd");
        assert!(matches!(event, ChatEvent::StreamEnd(..)));

        let event: ChatEvent = stream_events[5]
            .parse_payload()
            .expect("failed to parse TypingStatusChanged(false)");
        assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
    }
}

#[tokio::test]
async fn late_joining_client_gets_replay() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("replay-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "late join replay".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    // Client 1: NewAgent on host stream.
    let env = expect_next_event(&mut fixture.client, "NewAgent for client 1").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let client1_new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload for client 1");
    let agent_id = client1_new_agent.agent_id.clone();
    let client1_instance_stream = client1_new_agent.instance_stream.clone();

    // Client 1: AgentStart replay baseline.
    let env = expect_next_event(&mut fixture.client, "AgentStart for client 1").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, client1_instance_stream);
    assert_eq!(env.seq, 0);

    let client1_start: AgentStartPayload = env
        .parse_payload()
        .expect("failed to parse AgentStartPayload for client 1");
    assert_eq!(client1_start.agent_id, agent_id);

    // Client 1: TypingStatusChanged(true) -> StreamStart -> StreamDelta -> StreamEnd -> TypingStatusChanged(false).
    let mut client1_chat_payloads = Vec::new();

    let env = expect_chat_event(
        &mut fixture.client,
        "TypingStatusChanged(true) for client 1",
    )
    .await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse TypingStatusChanged(true) for client 1");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));
    client1_chat_payloads.push(env.payload.clone());

    let env = expect_chat_event(&mut fixture.client, "StreamStart for client 1").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamStart for client 1");
    assert!(matches!(event, ChatEvent::StreamStart(..)));
    client1_chat_payloads.push(env.payload.clone());

    let env = expect_chat_event(&mut fixture.client, "StreamDelta for client 1").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamDelta for client 1");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta
                    .text
                    .contains("mock backend response to: late join replay"),
                "unexpected StreamDelta text for client 1: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta for client 1, got {other:?}"),
    }
    client1_chat_payloads.push(env.payload.clone());

    let env = expect_chat_event(&mut fixture.client, "StreamEnd for client 1").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamEnd for client 1");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));
    client1_chat_payloads.push(env.payload.clone());

    let env = expect_chat_event(
        &mut fixture.client,
        "TypingStatusChanged(false) for client 1",
    )
    .await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse TypingStatusChanged(false) for client 1");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
    client1_chat_payloads.push(env.payload.clone());

    // Client 2 connects late and should receive NewAgent + full replay on its own instance stream.
    let mut client2 = fixture.connect().await;

    let env = expect_next_event(&mut client2, "NewAgent for client 2").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let client2_new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload for client 2");
    assert_eq!(client2_new_agent.agent_id, agent_id);
    assert_ne!(
        client2_new_agent.instance_stream, client1_instance_stream,
        "late-joining client must get a distinct instance stream",
    );
    let client2_instance_stream = client2_new_agent.instance_stream.clone();

    let env = expect_next_event(&mut client2, "AgentStart for client 2").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, client2_instance_stream);
    assert_eq!(env.seq, 0, "replayed AgentStart must be seq 0");

    let client2_start: AgentStartPayload = env
        .parse_payload()
        .expect("failed to parse AgentStartPayload for client 2");
    assert_eq!(client2_start.agent_id, agent_id);
    assert_eq!(client2_start.name, client1_start.name);
    assert_eq!(client2_start.backend_kind, client1_start.backend_kind);
    assert_eq!(client2_start.workspace_roots, client1_start.workspace_roots);
    assert_eq!(client2_start.parent_agent_id, client1_start.parent_agent_id);
    assert_eq!(client2_start.created_at_ms, client1_start.created_at_ms);

    let mut client2_chat_payloads = Vec::new();

    let env = expect_next_event(&mut client2, "TypingStatusChanged(true) for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse TypingStatusChanged(true) for client 2");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));
    client2_chat_payloads.push(env.payload.clone());

    let env = expect_next_event(&mut client2, "StreamStart for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client2_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamStart for client 2");
    assert!(matches!(event, ChatEvent::StreamStart(..)));
    client2_chat_payloads.push(env.payload.clone());

    let env = expect_next_event(&mut client2, "StreamDelta for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client2_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamDelta for client 2");
    match &event {
        ChatEvent::StreamDelta(delta) => {
            assert!(
                delta
                    .text
                    .contains("mock backend response to: late join replay"),
                "unexpected StreamDelta text for client 2: {}",
                delta.text,
            );
        }
        other => panic!("expected StreamDelta for client 2, got {other:?}"),
    }
    client2_chat_payloads.push(env.payload.clone());

    let env = expect_next_event(&mut client2, "StreamEnd for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client2_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamEnd for client 2");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));
    client2_chat_payloads.push(env.payload.clone());

    let env = expect_next_event(&mut client2, "TypingStatusChanged(false) for client 2").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse TypingStatusChanged(false) for client 2");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
    client2_chat_payloads.push(env.payload.clone());

    assert_eq!(
        client2_chat_payloads.len(),
        client1_chat_payloads.len(),
        "late-joining client should replay same number of ChatEvents",
    );
    assert_eq!(
        client2_chat_payloads, client1_chat_payloads,
        "replayed ChatEvent payloads must match original client payloads",
    );
}

#[tokio::test]
async fn project_mutations_fan_out_and_delete() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Tyde".to_owned(),
            roots: vec!["/tmp/tyde".to_owned()],
        })
        .await
        .expect("project_create failed");

    let created = match expect_project_notify(&mut fixture.client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    };
    assert_eq!(created.name, "Tyde");
    assert_eq!(created.roots, vec!["/tmp/tyde".to_owned()]);

    let mut client2 = fixture.connect().await;
    let replayed = match expect_project_notify(&mut client2, "project replay on connect").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected replayed upsert project notification, got {other:?}"),
    };
    assert_eq!(replayed, created);

    fixture
        .client
        .project_rename(ProjectRenamePayload {
            id: created.id.clone(),
            name: "Tyde Renamed".to_owned(),
        })
        .await
        .expect("project_rename failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project rename").await {
            ProjectNotifyPayload::Upsert { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(project.name, "Tyde Renamed");
                assert_eq!(project.roots, vec!["/tmp/tyde".to_owned()]);
            }
            other => panic!("expected renamed project notification, got {other:?}"),
        }
    }

    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: created.id.clone(),
            root: "/tmp/tyde-extra".to_owned(),
        })
        .await
        .expect("project_add_root failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project add root").await {
            ProjectNotifyPayload::Upsert { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(
                    project.roots,
                    vec!["/tmp/tyde".to_owned(), "/tmp/tyde-extra".to_owned()]
                );
            }
            other => panic!("expected root-added project notification, got {other:?}"),
        }
    }

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: created.id.clone(),
        })
        .await
        .expect("project_delete failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project delete").await {
            ProjectNotifyPayload::Delete { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(project.name, "Tyde Renamed");
                assert_eq!(
                    project.roots,
                    vec!["/tmp/tyde".to_owned(), "/tmp/tyde-extra".to_owned()]
                );
            }
            other => panic!("expected deleted project notification, got {other:?}"),
        }
    }

    let mut client3 = fixture.connect().await;
    expect_no_event(
        &mut client3,
        Duration::from_millis(150),
        "deleted project should not replay to new clients",
    )
    .await;
}

#[tokio::test]
async fn project_replay_happens_before_agent_replay() {
    let mut fixture = Fixture::new().await;

    let project = create_project(
        &mut fixture.client,
        "Project Agent",
        vec!["/tmp/project-agent".to_owned()],
    )
    .await;
    let sibling = create_project(
        &mut fixture.client,
        "Project Sibling",
        vec!["/tmp/project-sibling".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("project-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project.roots.clone(),
                prompt: "hello from project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent with project failed");

    let env = expect_next_event(&mut fixture.client, "project new agent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse project NewAgent");
    assert_eq!(new_agent.project_id.as_ref(), Some(&project.id));

    let env = expect_next_event(&mut fixture.client, "project agent start").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let start: AgentStartPayload = env.parse_payload().expect("parse project AgentStart");
    assert_eq!(start.project_id.as_ref(), Some(&project.id));

    expect_turn(
        &mut fixture.client,
        "mock backend response to: hello from project",
    )
    .await;

    let mut client2 = fixture.connect().await;

    let replayed_first = expect_project_notify(&mut client2, "first project replay").await;
    let replayed_second = expect_project_notify(&mut client2, "second project replay").await;
    let replayed_projects = vec![replayed_first, replayed_second]
        .into_iter()
        .map(|payload| match payload {
            ProjectNotifyPayload::Upsert { project } => project,
            other => panic!("expected replayed upsert project notification, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(replayed_projects, vec![project.clone(), sibling]);

    let env = expect_next_event(&mut client2, "new agent after project replay").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let replayed_agent: NewAgentPayload = env.parse_payload().expect("parse replayed NewAgent");
    assert_eq!(replayed_agent.project_id.as_ref(), Some(&project.id));

    let env = expect_next_event(&mut client2, "agent start after project replay").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    let replayed_start: AgentStartPayload = env.parse_payload().expect("parse replayed AgentStart");
    assert_eq!(replayed_start.project_id.as_ref(), Some(&project.id));
}

#[tokio::test]
async fn projects_persist_to_disk_and_replay_from_fresh_host() {
    let mut fixture = Fixture::new().await;

    let project_a = create_project(
        &mut fixture.client,
        "Persist A",
        vec!["/tmp/persist-a".to_owned()],
    )
    .await;
    let project_b = create_project(
        &mut fixture.client,
        "Persist B",
        vec![
            "/tmp/persist-b".to_owned(),
            "/tmp/persist-b-extra".to_owned(),
        ],
    )
    .await;

    let mut fresh_client = fixture.connect_fresh_host().await;

    let replayed_a = match expect_project_notify(&mut fresh_client, "persisted project A").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected persisted upsert project notification, got {other:?}"),
    };
    let replayed_b = match expect_project_notify(&mut fresh_client, "persisted project B").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected persisted upsert project notification, got {other:?}"),
    };

    assert_eq!(vec![replayed_a, replayed_b], vec![project_a, project_b]);
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "fresh host should replay exactly the persisted projects",
    )
    .await;
}

#[tokio::test]
async fn project_delete_is_rejected_when_a_session_still_references_it() {
    let mut fixture = Fixture::new().await;

    let project = create_project(
        &mut fixture.client,
        "Delete Guard",
        vec!["/tmp/delete-guard".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("delete-guard-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project.roots.clone(),
                prompt: "hold project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn delete guard agent failed");

    let _ = expect_next_event(&mut fixture.client, "delete guard NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "delete guard AgentStart").await;
    expect_turn(
        &mut fixture.client,
        "mock backend response to: hold project",
    )
    .await;

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: project.id.clone(),
        })
        .await
        .expect("project_delete write failed");

    let error = expect_command_error(&mut fixture.client, "project delete rejection").await;
    assert_eq!(error.operation, "project_delete");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("referenced by session"),
        "unexpected project_delete error: {}",
        error.message
    );

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(150),
        "connection should stay open after rejected project delete",
    )
    .await;

    let mut fresh_client = fixture.connect_fresh_host().await;
    match expect_project_notify(&mut fresh_client, "project survives rejected delete").await {
        ProjectNotifyPayload::Upsert { project: replayed } => assert_eq!(replayed, project),
        other => panic!("expected surviving project upsert after rejected delete, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_project_input_surfaces_command_error_and_keeps_connection_alive() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Invalid".to_owned(),
            roots: vec!["/tmp/dup".to_owned(), "/tmp/dup".to_owned()],
        })
        .await
        .expect("project_create write failed");

    let error = expect_command_error(&mut fixture.client, "invalid project_create").await;
    assert_eq!(error.operation, "project_create");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error.message.contains("roots must be unique"),
        "unexpected project_create error: {}",
        error.message
    );

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(150),
        "connection should stay open after invalid project_create",
    )
    .await;

    let mut fresh_client = fixture.connect_fresh_host().await;
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "invalid project_create should not persist any project",
    )
    .await;
}

#[tokio::test]
async fn spawn_with_missing_project_id_closes_the_connection() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("missing-project-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(ProjectId("11111111-1111-1111-1111-111111111111".to_owned())),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/missing-project".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent write failed");

    loop {
        match fixture
            .client
            .next_event()
            .await
            .expect("next_event after missing project spawn failed")
        {
            None => break,
            Some(env)
                if matches!(
                    env.kind,
                    FrameKind::SessionSettings
                        | FrameKind::SessionSchemas
                        | FrameKind::BackendSetup
                        | FrameKind::QueuedMessages
                        | FrameKind::SessionList
                        | FrameKind::HostSettings
                ) =>
            {
                continue;
            }
            Some(env) => panic!(
                "spawning with a missing project should terminate the connection, got: {} on {}",
                env.kind, env.stream
            ),
        }
    }

    let mut fresh_client = fixture.connect_fresh_host().await;
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "missing-project spawn should not persist any project state",
    )
    .await;
}

async fn create_project(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots,
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create helper").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}
