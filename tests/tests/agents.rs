mod fixture;

use fixture::Fixture;
use protocol::types::AgentClosedPayload;
use protocol::{
    AgentActivitySummaryPayload, AgentActivitySummaryState, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentControlStatus, AgentErrorCode, AgentErrorPayload, AgentOrigin,
    AgentRenamedPayload, AgentStartPayload, BackendConfigSnapshotsPayload, BackendKind,
    BackgroundAgentFeature, ChatEvent, ClientErrorCode, ClientErrorPayload, CommandErrorCode,
    CommandErrorPayload, Envelope, FetchSessionHistoryPayload, FrameKind, HostBootstrapPayload,
    HostLaunchProfileConfig, HostSettingValue, HostSettingsPayload, LaunchProfileCatalogPayload,
    LaunchProfileId, LaunchProfileKind, ListSessionsPayload, MessageMetadataUpdateData,
    MessageSender, MessageTokenUsage, NewAgentPayload, OrchestrationAgentOrigin,
    OrchestrationPayload, Project, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectId, ProjectNotifyPayload, ProjectRenamePayload, ProjectRootPath,
    SendMessagePayload, SendMessageToolResponse, SessionHistoryPayload, SessionListPayload,
    SessionSettingValue, SessionSettingsValues, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamEndData, StreamPath, TaskTokenUsagePayload, TaskTokenUsageScope,
    TaskTokenUsageStatus, TaskTokenUsageUnavailableReason, TokenUsageScope,
    TokenUsageUnavailableReason, ToolExecutionCompletedData, ToolExecutionResult, ToolRequest,
    ToolRequestType, write_envelope,
};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientRequest, Meta, NumberOrString,
        ProgressNotificationParam, ProgressToken, RawContent, ServerResult,
    },
    service::{NotificationContext, PeerRequestOptions, RoleClient},
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::future::{self, Future};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tyde_dev_driver::agent_control::{AgentControlHandle, SpawnRequest};

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }

        if env.kind == FrameKind::AgentBootstrap {
            let bootstrap: AgentBootstrapPayload = env.parse_payload().expect("AgentBootstrap");
            if let Some(first) = record_agent_bootstrap_events(&env.stream, bootstrap) {
                return first;
            }
            continue;
        }

        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionSchemas
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::BackendConfigSchemas
                | FrameKind::BackendConfigSnapshots
                | FrameKind::QueuedMessages
                | FrameKind::SessionList
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
        ) {
            continue;
        }

        return env;
    }
}

fn pending_agent_events() -> &'static Mutex<HashMap<StreamPath, VecDeque<Envelope>>> {
    static PENDING: OnceLock<Mutex<HashMap<StreamPath, VecDeque<Envelope>>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_agent_bootstrap_events(
    stream: &StreamPath,
    bootstrap: AgentBootstrapPayload,
) -> Option<Envelope> {
    let mut events = bootstrap
        .events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| agent_bootstrap_event_envelope(stream, index as u64, event));
    let first = events.next();
    let mut rest = events.collect::<VecDeque<_>>();
    if !rest.is_empty() {
        pending_agent_events()
            .lock()
            .expect("pending agent event mutex poisoned")
            .entry(stream.clone())
            .or_default()
            .append(&mut rest);
    }
    first
}

fn agent_bootstrap_event_envelope(
    stream: &StreamPath,
    seq: u64,
    event: AgentBootstrapEvent,
) -> Option<Envelope> {
    match event {
        AgentBootstrapEvent::AgentStart(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::AgentStart,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::AgentError(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::AgentError,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::SessionSettings(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::SessionSettings,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::QueuedMessages(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::QueuedMessages,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::ChatEvent(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::ChatEvent,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::AgentActivityStats(_)
        | AgentBootstrapEvent::HasPriorHistory { .. } => None,
    }
    .map(|result| result.expect("serialize synthetic bootstrap event"))
}

fn pop_pending_agent_event(stream: &StreamPath, kind: FrameKind) -> Option<Envelope> {
    let mut pending = pending_agent_events()
        .lock()
        .expect("pending agent event mutex poisoned");
    let queue = pending.get_mut(stream)?;
    let index = queue.iter().position(|env| env.kind == kind)?;
    let env = queue.remove(index);
    if queue.is_empty() {
        pending.remove(stream);
    }
    env
}

fn pop_front_pending_agent_event(stream: &StreamPath) -> Option<Envelope> {
    let mut pending = pending_agent_events()
        .lock()
        .expect("pending agent event mutex poisoned");
    let queue = pending.get_mut(stream)?;
    let env = queue.pop_front();
    if queue.is_empty() {
        pending.remove(stream);
    }
    env
}

fn push_pending_agent_event(env: Envelope) {
    if !env.stream.0.starts_with("/agent/") {
        return;
    }
    pending_agent_events()
        .lock()
        .expect("pending agent event mutex poisoned")
        .entry(env.stream.clone())
        .or_default()
        .push_back(env);
}

fn push_front_pending_agent_event(env: Envelope) {
    if !env.stream.0.starts_with("/agent/") {
        return;
    }
    pending_agent_events()
        .lock()
        .expect("pending agent event mutex poisoned")
        .entry(env.stream.clone())
        .or_default()
        .push_front(env);
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
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        let env = if env.kind == FrameKind::AgentBootstrap {
            let bootstrap: AgentBootstrapPayload = env.parse_payload().expect("AgentBootstrap");
            match record_agent_bootstrap_events(&env.stream, bootstrap) {
                Some(first) => first,
                None => continue,
            }
        } else {
            env
        };
        if env.kind == kind {
            return env;
        }
        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionSchemas
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::BackendConfigSchemas
                | FrameKind::BackendConfigSnapshots
                | FrameKind::QueuedMessages
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
        ) {
            continue;
        }
        // Skip other frame kinds while waiting for the target kind.
    }
}

/// Like expect_next_event but also skips proactive SessionList fan-outs that
/// are emitted on agent lifecycle transitions (start, terminate, rename).
async fn expect_chat_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = expect_next_event(client, context).await;
        if matches!(
            env.kind,
            FrameKind::SessionList | FrameKind::AgentActivityStats | FrameKind::TaskTokenUsage
        ) {
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

async fn send_client_error_report(client: &mut client::Connection, payload: &ClientErrorPayload) {
    let host_stream = single_host_stream(client);
    let seq = client
        .outgoing_seq
        .get(&host_stream)
        .copied()
        .expect("missing host stream sequence counter");
    let envelope =
        Envelope::from_payload(host_stream.clone(), FrameKind::ClientError, seq, payload)
            .expect("serialize ClientErrorPayload");
    client.outgoing_seq.insert(host_stream, seq + 1);
    write_envelope(&mut client.writer, &envelope)
        .await
        .expect("send ClientError frame");
}

fn single_host_stream(client: &client::Connection) -> StreamPath {
    let mut host_streams = client
        .outgoing_seq
        .keys()
        .filter(|stream| stream.0.starts_with("/host/"));
    let host_stream = host_streams
        .next()
        .cloned()
        .expect("client should have a host stream");
    assert!(
        host_streams.next().is_none(),
        "client should have exactly one host stream"
    );
    host_stream
}

async fn expect_turn(client: &mut client::Connection, stream: &StreamPath, expected_text: &str) {
    expect_turn_on_stream(client, stream, expected_text).await;
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env)))
                if fixture::is_builtin_team_custom_agent_notify(&env)
                    || matches!(
                        env.kind,
                        FrameKind::SessionSettings
                            | FrameKind::AgentsViewPreferencesNotify
                            | FrameKind::TeamPresetCatalogNotify
                            | FrameKind::SessionSchemas
                            | FrameKind::LaunchProfileCatalogNotify
                            | FrameKind::BackendSetup
                            | FrameKind::BackendConfigSchemas
                            | FrameKind::BackendConfigSnapshots
                            | FrameKind::QueuedMessages
                            | FrameKind::SessionList
                            | FrameKind::HostSettings
                            | FrameKind::WorkflowNotify
                            | FrameKind::AgentActivityStats
                            | FrameKind::TaskTokenUsage
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

async fn set_activity_summaries(client: &mut client::Connection, enabled: bool) {
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::BackgroundAgentFeatureEnabled {
                feature: BackgroundAgentFeature::AgentActivitySummaries,
                enabled,
            },
        })
        .await
        .expect("set activity summary setting");

    let env = expect_kind(
        client,
        FrameKind::HostSettings,
        "activity summary HostSettings",
    )
    .await;
    let payload: HostSettingsPayload = env.parse_payload().expect("parse HostSettings");
    assert_eq!(
        payload
            .settings
            .background_agent_features
            .agent_activity_summaries,
        enabled,
        "activity summary setting did not round-trip through HostSettings"
    );
}

async fn expect_activity_summary_matching(
    client: &mut client::Connection,
    agent_id: &protocol::AgentId,
    context: &str,
    mut matches_state: impl FnMut(&AgentActivitySummaryState) -> bool,
) -> AgentActivitySummaryPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for activity summary {context}"
        );
        let env = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before activity summary {context}"),
            Ok(Err(err)) => {
                panic!("next_event failed before activity summary {context}: {err:?}")
            }
            Err(_) => panic!("timed out waiting for activity summary {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = env.parse_payload().expect("parse unexpected NewAgent");
            panic!(
                "unexpected NewAgent while waiting for activity summary {context}: {}",
                payload.agent_id
            );
        }
        if env.kind != FrameKind::AgentActivitySummary {
            continue;
        }
        let payload: AgentActivitySummaryPayload =
            env.parse_payload().expect("parse AgentActivitySummary");
        assert_eq!(
            &payload.agent_id, agent_id,
            "activity summary emitted for unexpected agent"
        );
        if matches_state(&payload.state) {
            return payload;
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
    mcp_spawn_agent_with_arguments(
        url,
        json!({
            "workspace_roots": ["/tmp/agent-control-mcp-parent-url"],
            "prompt": prompt,
            "backend_kind": "claude",
            "name": name
        }),
    )
    .await
}

async fn mcp_spawn_agent_with_arguments(url: &str, arguments: Value) -> protocol::AgentId {
    let response = post_json(
        url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_spawn_agent",
                "arguments": arguments
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

async fn mcp_await_agent(url: &str, agent_id: &protocol::AgentId) -> Value {
    let response = post_json(
        url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_await_agents",
                "arguments": {
                    "agent_ids": [agent_id.0.clone()]
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
        .unwrap_or_else(|| panic!("MCP result missing isError: {response}"));
    assert!(!is_error, "MCP tool call failed: {response}");
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("MCP response missing content text: {response}"));
    serde_json::from_str(text).expect("parse MCP tool payload JSON")
}

async fn mcp_list_agents(url: &str) -> Value {
    let response = post_json(
        url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_list_agents",
                "arguments": {}
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
        .unwrap_or_else(|| panic!("MCP result missing isError: {response}"));
    assert!(!is_error, "MCP tool call failed: {response}");
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("MCP response missing content text: {response}"));
    serde_json::from_str(text).expect("parse MCP tool payload JSON")
}

async fn mcp_list_launch_options(url: &str) -> Value {
    let response = post_json(
        url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_list_launch_options",
                "arguments": {}
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
        .unwrap_or_else(|| panic!("MCP result missing isError: {response}"));
    assert!(!is_error, "MCP tool call failed: {response}");
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("MCP response missing content text: {response}"));
    serde_json::from_str(text).expect("parse MCP tool payload JSON")
}

fn hermes_claude_session_settings() -> SessionSettingsValues {
    let mut settings = SessionSettingsValues::default();
    settings.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String("high".to_owned()),
    );
    settings
        .0
        .insert("fast".to_owned(), SessionSettingValue::Bool(true));
    settings
}

fn hermes_claude_launch_profile() -> HostLaunchProfileConfig {
    HostLaunchProfileConfig {
        id: LaunchProfileId("hermes:claude".to_owned()),
        label: "Hermes: Claude".to_owned(),
        description: Some("Launch Hermes with an explicit Claude preset.".to_owned()),
        backend_kind: BackendKind::Hermes,
        session_settings: hermes_claude_session_settings(),
    }
}

async fn wait_for_ready_launch_profile(
    client: &mut client::Connection,
    profile_id: &str,
) -> LaunchProfileCatalogPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            panic!("timed out waiting for ready launch profile {profile_id}");
        }

        let env = match tokio::time::timeout(deadline - now, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before launch profile {profile_id} ready"),
            Ok(Err(err)) => panic!("next_event failed before launch profile {profile_id}: {err:?}"),
            Err(_) => panic!("timed out waiting for launch profile {profile_id}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }

        match env.kind {
            FrameKind::BackendConfigSchemas => {
                let _: protocol::BackendConfigSchemasPayload =
                    env.parse_payload().expect("BackendConfigSchemas payload");
            }
            FrameKind::BackendConfigSnapshots => {
                let _: BackendConfigSnapshotsPayload =
                    env.parse_payload().expect("BackendConfigSnapshots payload");
            }
            FrameKind::LaunchProfileCatalogNotify => {
                let payload: LaunchProfileCatalogPayload = env
                    .parse_payload()
                    .expect("LaunchProfileCatalogNotify payload");
                for entry in &payload.catalog.entries {
                    match entry {
                        protocol::LaunchProfileEntry::Ready { profile }
                            if profile.id.0.as_str() == profile_id =>
                        {
                            return payload;
                        }
                        protocol::LaunchProfileEntry::Unavailable { id, .. }
                            if id.0.as_str() == profile_id => {}
                        _ => {}
                    }
                }
            }
            FrameKind::HostSettings
            | FrameKind::SessionSchemas
            | FrameKind::BackendSetup
            | FrameKind::AgentsViewPreferencesNotify
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList
            | FrameKind::WorkflowNotify => {}
            other => panic!(
                "unexpected event while waiting for launch profile {profile_id}: kind={other} stream={}",
                env.stream
            ),
        }
    }
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

fn assert_awaited_agent_idle(result: &tyde_dev_driver::agent_control::AwaitAgentStatus) {
    assert_eq!(result.status, AgentControlStatus::Idle);
}

async fn await_dev_driver_agent_ready(
    control: &AgentControlHandle,
    agent_id: &str,
    context: &str,
) -> tyde_dev_driver::agent_control::AwaitAgentsResult {
    let awaited = tokio::time::timeout(
        Duration::from_secs(15),
        control.await_agents(Some(vec![protocol::AgentId(agent_id.to_owned())]), None),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
    .unwrap_or_else(|err| panic!("agent control await failed for {context}: {err}"));
    assert!(
        awaited.still_thinking.is_empty(),
        "agent control await should observe {context} ready, got {awaited:?}"
    );
    assert_eq!(awaited.ready.len(), 1);
    assert_awaited_agent_idle(&awaited.ready[0]);
    awaited
}

fn assert_await_result_ready(body: &Value, agent_id: &protocol::AgentId) {
    let ready = body
        .get("ready")
        .and_then(Value::as_array)
        .expect("await result missing ready array");
    let still_thinking = body
        .get("still_thinking")
        .and_then(Value::as_array)
        .expect("await result missing still_thinking array");
    assert!(
        still_thinking.is_empty(),
        "plan-pending await should not report still_thinking: {body}"
    );
    assert_eq!(
        ready.len(),
        1,
        "await result should contain one ready agent"
    );
    assert_eq!(
        ready[0].get("agent_id").and_then(Value::as_str),
        Some(agent_id.0.as_str())
    );
    assert_eq!(ready[0].get("status").and_then(Value::as_str), Some("idle"));
}

async fn wait_for_agent_control_status(
    url: &str,
    agent_id: &protocol::AgentId,
    expected_status: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let agents = mcp_list_agents(url).await;
        let matching = agents
            .as_array()
            .unwrap_or_else(|| panic!("list agents result was not an array: {agents}"))
            .iter()
            .find(|agent| {
                agent.get("agent_id").and_then(Value::as_str) == Some(agent_id.0.as_str())
            })
            .unwrap_or_else(|| panic!("agent {} missing from list result: {agents}", agent_id.0));
        let status = matching
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("agent status missing from list result: {agents}"));
        if status == expected_status {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for agent {} status {expected_status}; last status: {status}",
            agent_id.0,
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn mcp_listed_agent<'a>(agents: &'a Value, agent_id: &protocol::AgentId) -> &'a Value {
    agents
        .as_array()
        .unwrap_or_else(|| panic!("list agents result was not an array: {agents}"))
        .iter()
        .find(|agent| agent.get("agent_id").and_then(Value::as_str) == Some(agent_id.0.as_str()))
        .unwrap_or_else(|| panic!("agent {} missing from list result: {agents}", agent_id.0))
}

struct ProgressRecorder {
    tx: tokio::sync::mpsc::UnboundedSender<ProgressNotificationParam>,
}

impl ClientHandler for ProgressRecorder {
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let _ = self.tx.send(params);
        future::ready(())
    }
}

async fn assert_read_agent_contains(
    control: &AgentControlHandle,
    agent_id: &str,
    after_seq: Option<u64>,
    expected_text: &str,
) -> Option<u64> {
    let read = control
        .read_agent(protocol::AgentId(agent_id.to_string()), after_seq, None)
        .await
        .expect("agent control read should succeed");
    assert!(
        read.events
            .iter()
            .any(|event| chat_event_contains(event, expected_text)),
        "expected read output to contain '{expected_text}', got {:?}",
        read.events
    );
    read.next_after_seq
}

fn chat_event_contains(event: &Envelope, expected_text: &str) -> bool {
    if event.kind != FrameKind::ChatEvent {
        return false;
    }
    let chat_event: ChatEvent = event
        .parse_payload()
        .expect("agent read ChatEvent should parse");
    match chat_event {
        ChatEvent::StreamEnd(data) => data.message.content.contains(expected_text),
        ChatEvent::OperationCancelled(data) => data.message.contains(expected_text),
        ChatEvent::MessageAdded(message) => message.content.contains(expected_text),
        ChatEvent::StreamDelta(delta) => delta.text.contains(expected_text),
        _ => false,
    }
}

const MOCK_NATIVE_CHILD_SENTINEL: &str = "__mock_spawn_native_child__";
const MOCK_NATIVE_CHILD_AND_DROP_SENTINEL: &str = "__mock_spawn_native_child_and_drop__";
const MOCK_ERROR_WITHOUT_IDLE_SENTINEL: &str = "__mock_error_without_idle__";
const MOCK_TOOL_FAILURE_WITHOUT_IDLE_SENTINEL: &str = "__mock_tool_failure_without_idle__";
const MOCK_AGENT_CONTROL_AWAIT_SENTINEL: &str = "__mock_agent_control_await__";
const MOCK_LATE_USAGE_SENTINEL: &str = "__mock_late_usage__";
const MOCK_NO_USAGE_SENTINEL: &str = "__mock_no_usage__";
const MOCK_ORCHESTRATION_SENTINEL: &str = "__mock_orchestration__";
const MOCK_TURN_TOKEN_TOTAL: u64 = 1590;
const MOCK_NATIVE_CHILD_TOKEN_TOTAL: u64 = 330;

fn assert_known_turn_usage(
    usage: &Option<MessageTokenUsage>,
    expected_this_turn_total: u64,
    expected_agent_total: u64,
) {
    let usage = usage.as_ref().expect("expected token usage");
    assert_eq!(
        usage
            .turn
            .known_usage()
            .expect("expected known turn usage")
            .total_tokens,
        expected_this_turn_total
    );
    assert_eq!(
        usage
            .cumulative
            .known_usage()
            .expect("expected known cumulative usage")
            .total_tokens,
        expected_agent_total
    );
}

fn assert_unavailable_turn_usage(usage: &Option<MessageTokenUsage>) {
    match usage.as_ref().map(|usage| &usage.turn) {
        Some(TokenUsageScope::Unavailable {
            reason: TokenUsageUnavailableReason::BackendDidNotReport,
        }) => {}
        other => panic!("expected unavailable turn token usage, got {other:?}"),
    }
}

async fn expect_task_token_usage_matching(
    client: &mut client::Connection,
    root_agent_id: &protocol::AgentId,
    context: &str,
    mut matches_payload: impl FnMut(&TaskTokenUsagePayload) -> bool,
) -> TaskTokenUsagePayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for task token usage {context}"
        );
        let env = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before task token usage {context}"),
            Ok(Err(err)) => panic!("next_event failed before task token usage {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for task token usage {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let bootstrap: AgentBootstrapPayload = env.parse_payload().expect("AgentBootstrap");
            let _ = record_agent_bootstrap_events(&env.stream, bootstrap);
            continue;
        }
        if env.kind != FrameKind::TaskTokenUsage {
            continue;
        }
        let payload: TaskTokenUsagePayload = env.parse_payload().expect("TaskTokenUsage");
        if &payload.root_agent_id == root_agent_id && matches_payload(&payload) {
            return payload;
        }
    }
}

fn assert_known_task_scope(scope: &TaskTokenUsageScope, expected_total: u64) {
    match scope {
        TaskTokenUsageScope::Known { usage } => {
            assert_eq!(usage.total_tokens, expected_total);
        }
        other => panic!("expected known task token usage, got {other:?}"),
    }
}

fn assert_known_task_status(status: &TaskTokenUsageStatus) {
    assert_eq!(status, &TaskTokenUsageStatus::Known);
}

async fn spawn_token_usage_agent(
    client: &mut client::Connection,
    name: &str,
    prompt: &str,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![format!("/tmp/{name}")],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .unwrap_or_else(|error| panic!("spawn {name} failed: {error:?}"));

    let env = expect_next_event(client, &format!("{name} NewAgent")).await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    expect_agent_start_on_stream(client, &new_agent.instance_stream, &format!("{name} start"))
        .await;
    new_agent
}

#[tokio::test]
async fn orchestration_chat_events_are_observable_from_mock_backend() {
    let mut fixture = Fixture::new().await;
    let new_agent = spawn_token_usage_agent(
        &mut fixture.client,
        "mock-orchestration-agent",
        MOCK_ORCHESTRATION_SENTINEL,
    )
    .await;

    let env = expect_chat_event_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock orchestration ChatEvent",
    )
    .await;
    let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
    match event {
        ChatEvent::Orchestration(event) => {
            assert_eq!(event.agent_id.0, "mock-root");
            assert_eq!(event.agent_type.0, "swarm");
            match event.payload {
                OrchestrationPayload::AgentStarted {
                    parent_agent_id,
                    task_preview,
                    origin,
                    depth,
                    interactive,
                    model,
                } => {
                    assert_eq!(parent_agent_id, None);
                    assert_eq!(task_preview, "mock orchestration");
                    assert!(matches!(origin, OrchestrationAgentOrigin::Root));
                    assert_eq!(depth, 1);
                    assert!(interactive);
                    assert_eq!(model, None);
                }
                other => panic!("expected AgentStarted orchestration payload, got {other:?}"),
            }
        }
        other => panic!("expected Orchestration ChatEvent, got {other:?}"),
    }
}

async fn expect_turn_stream_end_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) -> StreamEndData {
    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(true)").await;
    let event: ChatEvent = env
        .parse_payload()
        .expect("parse TypingStatusChanged(true)");
    assert!(
        matches!(event, ChatEvent::TypingStatusChanged(true)),
        "expected TypingStatusChanged(true) on {stream}, got {event:?}"
    );

    let env = expect_chat_event_on_stream(client, stream, "StreamStart").await;
    let event: ChatEvent = env.parse_payload().expect("parse StreamStart");
    assert!(
        matches!(event, ChatEvent::StreamStart(..)),
        "expected StreamStart on {stream}, got {event:?}"
    );

    let env = expect_chat_event_on_stream(client, stream, "StreamDelta").await;
    let event: ChatEvent = env.parse_payload().expect("parse StreamDelta");
    match event {
        ChatEvent::StreamDelta(delta) => assert!(
            delta.text.contains(expected_text),
            "unexpected delta text on {stream}: {}",
            delta.text
        ),
        other => panic!("expected StreamDelta on {stream}, got {other:?}"),
    }

    let env = expect_chat_event_on_stream(client, stream, "StreamEnd").await;
    let event: ChatEvent = env.parse_payload().expect("parse StreamEnd");
    match event {
        ChatEvent::StreamEnd(data) => {
            assert!(
                data.message.content.contains(expected_text),
                "unexpected stream end content on {stream}: {}",
                data.message.content
            );
            data
        }
        other => panic!("expected StreamEnd on {stream}, got {other:?}"),
    }
}

async fn expect_completed_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) -> StreamEndData {
    let end = expect_turn_stream_end_on_stream(client, stream, expected_text).await;
    expect_typing_false_on_stream(client, stream).await;
    end
}

async fn expect_metadata_update_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
) -> MessageMetadataUpdateData {
    loop {
        let env = expect_chat_event_on_stream(client, stream, "MessageMetadataUpdated").await;
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if let ChatEvent::MessageMetadataUpdated(update) = event {
            return update;
        }
    }
}

async fn expect_raw_agent_bootstrap_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentBootstrapPayload {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env)
            || matches!(
                env.kind,
                FrameKind::SessionSettings
                    | FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::TeamPresetCatalogNotify
                    | FrameKind::SessionSchemas
                    | FrameKind::LaunchProfileCatalogNotify
                    | FrameKind::BackendSetup
                    | FrameKind::BackendConfigSchemas
                    | FrameKind::BackendConfigSnapshots
                    | FrameKind::QueuedMessages
                    | FrameKind::SessionList
                    | FrameKind::WorkflowNotify
            )
        {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap && env.stream == *stream {
            return env.parse_payload().expect("parse AgentBootstrap");
        }
    }
}

async fn expect_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(true)").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(
        matches!(event, ChatEvent::TypingStatusChanged(true)),
        "expected TypingStatusChanged(true) on {stream}, got {event:?}"
    );

    expect_live_turn_after_typing_true_on_stream(client, stream, expected_text).await;
}

async fn expect_live_turn_after_typing_true_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let env = expect_chat_event_on_stream(client, stream, "StreamStart").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(
        matches!(event, ChatEvent::StreamStart(..)),
        "expected StreamStart on {stream}, got {event:?}"
    );

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
    assert!(
        matches!(event, ChatEvent::StreamEnd(..)),
        "expected StreamEnd on {stream}, got {event:?}"
    );

    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(false)").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(
        matches!(event, ChatEvent::TypingStatusChanged(false)),
        "expected TypingStatusChanged(false) on {stream}, got {event:?}"
    );
}

async fn expect_agent_control_child_initial_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let env = expect_chat_event_on_stream(client, stream, "agent-control child initial turn").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    match event {
        ChatEvent::TypingStatusChanged(true) => {
            expect_live_turn_after_typing_true_on_stream(client, stream, expected_text).await;
        }
        ChatEvent::StreamStart(_) => {
            expect_agent_control_child_replayed_stream_tail(client, stream, expected_text).await;
        }
        ChatEvent::MessageAdded(message) => {
            assert!(
                matches!(message.sender, MessageSender::Assistant { .. }),
                "expected Assistant MessageAdded on {stream}, got {:?}",
                message.sender
            );
            assert!(
                message.content.contains(expected_text),
                "unexpected MessageAdded text on {}: {}",
                stream,
                message.content
            );
            drain_pending_agent_control_child_initial_turn_trailer(stream, expected_text);
        }
        other => {
            panic!("expected agent-control child initial turn on {stream}, got {other:?}");
        }
    }
}

async fn expect_agent_control_child_replayed_stream_tail(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let mut saw_expected_text = false;
    loop {
        let env =
            expect_chat_event_on_stream(client, stream, "agent-control child stream tail").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        match event {
            ChatEvent::StreamDelta(delta) => {
                if delta.text.contains(expected_text) {
                    saw_expected_text = true;
                }
            }
            ChatEvent::StreamEnd(end) => {
                if end.message.content.contains(expected_text) {
                    saw_expected_text = true;
                }
                assert!(
                    saw_expected_text,
                    "agent-control child stream on {stream} ended without expected text {expected_text:?}"
                );
                drain_pending_agent_control_child_initial_turn_trailer(stream, expected_text);
                return;
            }
            ChatEvent::StreamReasoningDelta(_)
            | ChatEvent::ToolRequest(_)
            | ChatEvent::ToolProgress(_)
            | ChatEvent::ToolExecutionCompleted(_) => {}
            other => {
                panic!("unexpected event in agent-control child stream on {stream}: {other:?}");
            }
        }
    }
}

fn drain_pending_agent_control_child_initial_turn_trailer(
    stream: &StreamPath,
    expected_text: &str,
) {
    while let Some(env) = pop_front_pending_agent_event(stream) {
        if env.kind != FrameKind::ChatEvent {
            push_front_pending_agent_event(env);
            return;
        }
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if is_agent_control_child_initial_turn_trailer(&event, expected_text) {
            continue;
        }
        push_front_pending_agent_event(env);
        return;
    }
}

fn is_agent_control_child_initial_turn_trailer(event: &ChatEvent, expected_text: &str) -> bool {
    match event {
        ChatEvent::TypingStatusChanged(false) => true,
        ChatEvent::MessageAdded(message) => {
            matches!(message.sender, MessageSender::Assistant { .. })
                && message.content.contains(expected_text)
        }
        _ => false,
    }
}

async fn expect_error_message_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    loop {
        let env = expect_chat_event_on_stream(client, stream, "MessageAdded(Error)").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if let ChatEvent::MessageAdded(message) = event
            && matches!(message.sender, MessageSender::Error)
        {
            assert!(
                message.content.contains(expected_text),
                "unexpected error message on {}: {}",
                stream,
                message.content
            );
            return;
        }
    }
}

async fn expect_typing_false_on_stream(client: &mut client::Connection, stream: &StreamPath) {
    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(false)").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
}

async fn expect_failed_tool_completion_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_error: &str,
) {
    loop {
        let env = expect_chat_event_on_stream(client, stream, "ToolExecutionCompleted").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if let ChatEvent::ToolExecutionCompleted(completion) = event {
            assert!(
                !completion.success,
                "expected failed tool completion on {stream}, got {completion:?}"
            );
            let error = completion.error.as_deref().unwrap_or_default();
            assert!(
                error.contains(expected_error),
                "unexpected tool completion error on {stream}: {error}"
            );
            return;
        }
    }
}

async fn expect_tool_request_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_tool_name: &str,
) -> ToolRequest {
    loop {
        let env = expect_chat_event_on_stream(client, stream, expected_tool_name).await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if let ChatEvent::ToolRequest(request) = event
            && request.tool_name == expected_tool_name
        {
            return request;
        }
    }
}

async fn expect_tool_completion_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_tool_call_id: &str,
) -> ToolExecutionCompletedData {
    loop {
        let env = expect_chat_event_on_stream(client, stream, "ToolExecutionCompleted").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if let ChatEvent::ToolExecutionCompleted(completion) = event
            && completion.tool_call_id == expected_tool_call_id
        {
            return completion;
        }
    }
}

async fn expect_stream_end_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    loop {
        let env = expect_chat_event_on_stream(client, stream, "StreamEnd").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if let ChatEvent::StreamEnd(end) = event {
            assert!(
                end.message.content.contains(expected_text),
                "unexpected StreamEnd text on {}: {}",
                stream,
                end.message.content
            );
            return;
        }
    }
}

async fn expect_operation_cancelled_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) {
    let mut saw_cancel = false;

    loop {
        let env = expect_chat_event_on_stream(client, stream, "OperationCancelled").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        match event {
            ChatEvent::OperationCancelled(data) => {
                assert!(
                    data.message.contains(expected_text),
                    "unexpected cancellation message on {}: {}",
                    stream,
                    data.message
                );
                saw_cancel = true;
            }
            ChatEvent::TypingStatusChanged(false) if saw_cancel => return,
            _ => {}
        }
    }
}

async fn expect_replayed_turn_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    agent_id: &protocol::AgentId,
    expected_text: &str,
) {
    client
        .fetch_session_history(
            stream,
            FetchSessionHistoryPayload {
                agent_id: agent_id.clone(),
                before_seq: None,
                limit: 10,
            },
        )
        .await
        .expect("fetch_session_history failed");

    let history = loop {
        let env = expect_next_event(client, "SessionHistory").await;
        if env.kind != FrameKind::SessionHistory || env.stream != *stream {
            continue;
        }
        break env
            .parse_payload::<SessionHistoryPayload>()
            .expect("parse SessionHistoryPayload");
    };
    assert!(
        history.events.iter().any(|event| match event {
            ChatEvent::MessageAdded(message) => message.content.contains(expected_text),
            ChatEvent::StreamEnd(data) => data.message.content.contains(expected_text),
            ChatEvent::StreamDelta(delta) => delta.text.contains(expected_text),
            _ => false,
        }),
        "expected fetched session history on {stream} to contain {expected_text:?}, got {:?}",
        history.events
    );
}

async fn expect_chat_event_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> Envelope {
    loop {
        if let Some(env) = pop_pending_agent_event(stream, FrameKind::ChatEvent) {
            return env;
        }
        let env = expect_chat_event(client, context).await;
        if env.kind == FrameKind::ChatEvent && env.stream == *stream {
            return env;
        }
        push_pending_agent_event(env);
    }
}

async fn wait_for_exit_plan_mode_pause_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
) -> ToolRequest {
    let mut tool_request = None;
    let mut saw_pause = false;
    loop {
        let env = expect_chat_event_on_stream(client, stream, "ExitPlanMode pause").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        match event {
            ChatEvent::ToolRequest(request) => {
                assert_eq!(request.tool_name, "ExitPlanMode");
                assert!(matches!(
                    &request.tool_type,
                    protocol::ToolRequestType::ExitPlanMode { .. }
                ));
                tool_request = Some(request);
            }
            ChatEvent::TypingStatusChanged(false) => saw_pause = true,
            _ => {}
        }
        if saw_pause && let Some(request) = tool_request {
            return request;
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
        if let Some(env) = pop_pending_agent_event(stream, FrameKind::AgentError) {
            let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
            if payload.message == expected_message {
                return payload;
            }
        }
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

async fn expect_agent_error_containing(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_message: &str,
    context: &str,
) -> AgentErrorPayload {
    loop {
        if let Some(env) = pop_pending_agent_event(stream, FrameKind::AgentError) {
            let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
            if payload.message.contains(expected_message) {
                return payload;
            }
        }
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::AgentError || env.stream != *stream {
            continue;
        }
        let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
        if payload.message.contains(expected_message) {
            return payload;
        }
    }
}

async fn expect_agent_error_message_without(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_message: &str,
    forbidden_message: &str,
    context: &str,
) -> AgentErrorPayload {
    loop {
        if let Some(env) = pop_pending_agent_event(stream, FrameKind::AgentError) {
            let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
            assert_ne!(
                payload.message, forbidden_message,
                "unexpected AgentError while waiting for {context}"
            );
            if payload.message == expected_message {
                return payload;
            }
        }
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::AgentError || env.stream != *stream {
            continue;
        }
        let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
        assert_ne!(
            payload.message, forbidden_message,
            "unexpected AgentError while waiting for {context}"
        );
        if payload.message == expected_message {
            return payload;
        }
    }
}

async fn expect_no_agent_error_message(
    client: &mut client::Connection,
    stream: &StreamPath,
    forbidden_message: &str,
    duration: Duration,
    context: &str,
) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env)))
                if fixture::is_builtin_team_custom_agent_notify(&env)
                    || matches!(
                        env.kind,
                        FrameKind::HostSettings
                            | FrameKind::AgentsViewPreferencesNotify
                            | FrameKind::SessionSettings
                            | FrameKind::TeamPresetCatalogNotify
                            | FrameKind::SessionSchemas
                            | FrameKind::LaunchProfileCatalogNotify
                            | FrameKind::BackendSetup
                            | FrameKind::BackendConfigSchemas
                            | FrameKind::BackendConfigSnapshots
                            | FrameKind::QueuedMessages
                            | FrameKind::SessionList
                            | FrameKind::TaskTokenUsage
                            | FrameKind::WorkflowNotify
                    ) =>
            {
                continue;
            }
            Ok(Ok(Some(env))) => {
                if env.kind != FrameKind::AgentError || env.stream != *stream {
                    continue;
                }
                let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentError");
                assert_ne!(
                    payload.message, forbidden_message,
                    "unexpected AgentError before {context}"
                );
            }
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
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
            push_pending_agent_event(env);
            continue;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
        if &payload.agent_id == agent_id {
            return payload;
        }
    }
}

fn bootstrapped_agent(
    bootstrap: &HostBootstrapPayload,
    agent_id: &protocol::AgentId,
) -> NewAgentPayload {
    bootstrap
        .agents
        .iter()
        .find(|agent| &agent.agent_id == agent_id)
        .cloned()
        .expect("agent missing from HostBootstrap")
}

async fn expect_agent_start_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        if let Some(env) = pop_pending_agent_event(stream, FrameKind::AgentStart) {
            return env.parse_payload().expect("parse AgentStart");
        }
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::AgentStart || env.stream != *stream {
            push_pending_agent_event(env);
            continue;
        }
        return env.parse_payload().expect("parse AgentStart");
    }
}

async fn spawn_user_child(
    client: &mut client::Connection,
    parent_agent_id: &protocol::AgentId,
    name: &str,
    prompt: &str,
    workspace_root: &str,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root.to_owned()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .unwrap_or_else(|error| panic!("spawn {name} failed: {error:?}"));

    let env = expect_next_event(client, &format!("{name} NewAgent")).await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let child_new: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
    assert_eq!(child_new.parent_agent_id.as_ref(), Some(parent_agent_id));

    let child_start =
        expect_agent_start_on_stream(client, &child_new.instance_stream, &format!("{name} start"))
            .await;
    assert_eq!(child_start.origin, AgentOrigin::User);
    assert_eq!(child_start.parent_agent_id.as_ref(), Some(parent_agent_id));

    expect_turn_on_stream(
        client,
        &child_new.instance_stream,
        &format!("mock backend response to: {prompt}"),
    )
    .await;

    child_new
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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
async fn turn_token_usage_is_known_cumulative_and_bootstrapped() {
    let mut fixture = Fixture::new().await;
    let new_agent =
        spawn_token_usage_agent(&mut fixture.client, "token-usage-agent", "first usage").await;

    let first = expect_completed_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: first usage",
    )
    .await;
    assert_eq!(
        first
            .message
            .token_usage
            .as_ref()
            .expect("first turn usage")
            .request
            .known_usage()
            .expect("first request usage")
            .total_tokens,
        MOCK_TURN_TOKEN_TOTAL
    );
    assert_known_turn_usage(
        &first.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
        MOCK_TURN_TOKEN_TOTAL,
    );

    fixture
        .client
        .send_message(&new_agent.instance_stream, "second usage".to_owned())
        .await
        .expect("send second usage message");
    let second = expect_completed_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: second usage",
    )
    .await;
    assert_known_turn_usage(
        &second.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
        MOCK_TURN_TOKEN_TOTAL * 2,
    );

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let late_agent = bootstrapped_agent(&bootstrap, &new_agent.agent_id);
    let agent_bootstrap = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &late_agent.instance_stream,
        "late client AgentBootstrap",
    )
    .await;
    let stats = agent_bootstrap
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentActivityStats(payload) => Some(payload),
            _ => None,
        })
        .expect("AgentBootstrap should carry AgentActivityStats");
    assert_eq!(
        stats.stats.token_usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 2
    );
}

#[tokio::test]
async fn late_metadata_usage_updates_cumulative_without_double_counting() {
    let mut fixture = Fixture::new().await;
    let new_agent = spawn_token_usage_agent(
        &mut fixture.client,
        "late-token-usage-agent",
        &format!("first late {MOCK_LATE_USAGE_SENTINEL}"),
    )
    .await;

    let stream_end = expect_turn_stream_end_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: first late",
    )
    .await;
    assert_unavailable_turn_usage(&stream_end.message.token_usage);

    let update =
        expect_metadata_update_on_stream(&mut fixture.client, &new_agent.instance_stream).await;
    assert_eq!(
        update
            .token_usage
            .as_ref()
            .expect("late metadata token usage")
            .request
            .known_usage()
            .expect("late metadata request usage")
            .total_tokens,
        MOCK_TURN_TOKEN_TOTAL
    );
    assert_known_turn_usage(
        &update.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
        MOCK_TURN_TOKEN_TOTAL,
    );
    expect_typing_false_on_stream(&mut fixture.client, &new_agent.instance_stream).await;

    fixture
        .client
        .send_message(&new_agent.instance_stream, "after late usage".to_owned())
        .await
        .expect("send follow-up after late metadata");
    let second = expect_completed_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: after late usage",
    )
    .await;
    assert_known_turn_usage(
        &second.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
        MOCK_TURN_TOKEN_TOTAL * 2,
    );
}

#[tokio::test]
async fn subagent_turn_token_usage_is_strictly_self() {
    let mut fixture = Fixture::new().await;
    let parent_prompt = format!("parent prompt {MOCK_NATIVE_CHILD_SENTINEL}");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("strict-self-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/token-sub-agent-parent".to_owned()],
                prompt: parent_prompt,
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent with native child failed");

    let env = expect_next_event(&mut fixture.client, "strict-self parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "strict-self parent start",
    )
    .await;
    let parent_end = expect_completed_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;
    assert_known_turn_usage(
        &parent_end.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
        MOCK_TURN_TOKEN_TOTAL,
    );

    let env = expect_next_event(&mut fixture.client, "strict-self native child NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let child_new: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "strict-self child start",
    )
    .await;
    let child_end = expect_completed_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock native child response to: parent prompt",
    )
    .await;
    assert_known_turn_usage(
        &child_end.message.token_usage,
        MOCK_NATIVE_CHILD_TOKEN_TOTAL,
        MOCK_NATIVE_CHILD_TOKEN_TOTAL,
    );
}

#[tokio::test]
async fn missing_backend_usage_is_unavailable_not_zero() {
    let mut fixture = Fixture::new().await;
    let new_agent = spawn_token_usage_agent(
        &mut fixture.client,
        "unavailable-token-usage-agent",
        &format!("missing usage {MOCK_NO_USAGE_SENTINEL}"),
    )
    .await;

    let stream_end = expect_completed_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: missing usage",
    )
    .await;
    assert_unavailable_turn_usage(&stream_end.message.token_usage);
}

#[tokio::test]
async fn task_token_usage_rolls_up_parent_child_and_grandchild() {
    let mut fixture = Fixture::new().await;
    let root = spawn_token_usage_agent(&mut fixture.client, "usage-root", "root usage").await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root usage",
    )
    .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "usage-child",
        "child usage",
        "/tmp/usage-child",
    )
    .await;
    let grandchild = spawn_user_child(
        &mut fixture.client,
        &child.agent_id,
        "usage-grandchild",
        "grandchild usage",
        "/tmp/usage-grandchild",
    )
    .await;

    let payload = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "root child grandchild aggregate",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 3,
    )
    .await;
    assert_known_task_status(&payload.total.status);
    assert_known_task_status(&payload.descendant_usage.status);
    assert_known_task_scope(&payload.self_usage, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(
        payload.descendant_usage.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 2
    );
    assert_eq!(payload.descendant_count, 2);
    assert_eq!(payload.breakdown.len(), 3);
    assert_eq!(&payload.breakdown[0].agent_id, &root.agent_id);
    assert_eq!(payload.breakdown[0].depth, 0);
    assert_eq!(payload.breakdown[0].tree_index, 0);
    assert_eq!(payload.breakdown[0].parent_agent_id, None);
    assert_eq!(payload.breakdown[0].backend_kind, BackendKind::Claude);
    assert_eq!(payload.breakdown[0].model.as_deref(), Some("mock"));
    assert_known_task_scope(&payload.breakdown[0].usage, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(&payload.breakdown[1].agent_id, &child.agent_id);
    assert_eq!(
        payload.breakdown[1].parent_agent_id.as_ref(),
        Some(&root.agent_id)
    );
    assert_eq!(payload.breakdown[1].depth, 1);
    assert_eq!(payload.breakdown[1].tree_index, 1);
    assert_known_task_scope(&payload.breakdown[1].usage, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(&payload.breakdown[2].agent_id, &grandchild.agent_id);
    assert_eq!(
        payload.breakdown[2].parent_agent_id.as_ref(),
        Some(&child.agent_id)
    );
    assert_eq!(payload.breakdown[2].depth, 2);
    assert_eq!(payload.breakdown[2].tree_index, 2);
    assert_known_task_scope(&payload.breakdown[2].usage, MOCK_TURN_TOKEN_TOTAL);

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let bootstrapped = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == root.agent_id)
        .expect("HostBootstrap should include root task token usage");
    assert_eq!(
        bootstrapped.total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 3
    );
    assert_eq!(bootstrapped.descendant_count, 2);
}

#[tokio::test]
async fn task_token_usage_marks_missing_descendant_usage_partial() {
    let mut fixture = Fixture::new().await;
    let root = spawn_token_usage_agent(&mut fixture.client, "partial-root", "root usage").await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root usage",
    )
    .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "partial-child",
        &format!("child missing usage {MOCK_NO_USAGE_SENTINEL}"),
        "/tmp/partial-child",
    )
    .await;

    let payload = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "partial aggregate",
        |payload| {
            matches!(
                &payload.total.status,
                TaskTokenUsageStatus::Partial {
                    unavailable_count: 1,
                    reasons
                } if reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            )
        },
    )
    .await;
    assert_eq!(payload.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(payload.descendant_count, 1);
    match &payload.total.status {
        TaskTokenUsageStatus::Partial {
            unavailable_count,
            reasons,
        } => {
            assert_eq!(*unavailable_count, 1);
            assert_eq!(
                reasons,
                &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            );
        }
        other => panic!("expected partial aggregate, got {other:?}"),
    }
    match &payload.descendant_usage.status {
        TaskTokenUsageStatus::Unavailable {
            unavailable_count,
            reasons,
        } => {
            assert_eq!(*unavailable_count, 1);
            assert_eq!(
                reasons,
                &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            );
        }
        other => panic!("expected unavailable descendant usage, got {other:?}"),
    }
    let child_entry = payload
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("child breakdown entry");
    assert!(matches!(
        child_entry.usage,
        TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::BackendDidNotReport
        }
    ));
}

#[tokio::test]
async fn task_token_usage_all_unavailable_omits_split_zeroes() {
    let mut fixture = Fixture::new().await;
    let root = spawn_token_usage_agent(
        &mut fixture.client,
        "unavailable-root",
        &format!("root missing usage {MOCK_NO_USAGE_SENTINEL}"),
    )
    .await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root missing usage",
    )
    .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "unavailable-child",
        &format!("child missing usage {MOCK_NO_USAGE_SENTINEL}"),
        "/tmp/unavailable-child",
    )
    .await;

    let payload = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "all-unavailable aggregate",
        |payload| {
            matches!(
                &payload.total.status,
                TaskTokenUsageStatus::Unavailable {
                    unavailable_count: 2,
                    reasons
                } if reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            )
        },
    )
    .await;
    assert_eq!(payload.total.usage.total_tokens, 0);
    assert_eq!(payload.total.usage.input_tokens, None);
    assert_eq!(payload.total.usage.output_tokens, None);
    assert_eq!(payload.descendant_usage.usage.total_tokens, 0);
    assert_eq!(payload.descendant_usage.usage.input_tokens, None);
    assert_eq!(payload.descendant_usage.usage.output_tokens, None);
    assert_eq!(payload.descendant_count, 1);
    assert!(matches!(
        payload.self_usage,
        TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::BackendDidNotReport
        }
    ));
    let child_entry = payload
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("child breakdown entry");
    assert!(matches!(
        child_entry.usage,
        TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::BackendDidNotReport
        }
    ));
}

#[tokio::test]
async fn task_token_usage_updates_when_child_usage_changes() {
    let mut fixture = Fixture::new().await;
    let root = spawn_token_usage_agent(&mut fixture.client, "update-root", "root usage").await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root usage",
    )
    .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "update-child",
        "child first usage",
        "/tmp/update-child",
    )
    .await;
    let first = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "initial child aggregate",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 2,
    )
    .await;
    assert_known_task_status(&first.total.status);

    fixture
        .client
        .send_message(&child.instance_stream, "child second usage".to_owned())
        .await
        .expect("send child follow-up");
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "mock backend response to: child second usage",
    )
    .await;

    let updated = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "updated child aggregate",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 3,
    )
    .await;
    assert_known_task_status(&updated.total.status);
    assert_eq!(
        updated.descendant_usage.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 2
    );
    let child_entry = updated
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("child breakdown entry");
    assert_known_task_scope(&child_entry.usage, MOCK_TURN_TOKEN_TOTAL * 2);
}

#[tokio::test]
async fn task_token_usage_keeps_closed_child_in_live_root_rollup() {
    let mut fixture = Fixture::new().await;
    let root = spawn_token_usage_agent(&mut fixture.client, "closed-root", "root usage").await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root usage",
    )
    .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "closed-child",
        "child usage",
        "/tmp/closed-child",
    )
    .await;
    let before_close = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "aggregate before child close",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 2,
    )
    .await;
    assert_known_task_status(&before_close.total.status);

    fixture
        .client
        .close_agent(&child.instance_stream)
        .await
        .expect("close child");
    let closed = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "closed child AgentClosed",
    )
    .await;
    let payload: AgentClosedPayload = closed.parse_payload().expect("AgentClosed payload");
    assert_eq!(payload.agent_id, child.agent_id);

    let after_close = expect_task_token_usage_matching(
        &mut fixture.client,
        &root.agent_id,
        "aggregate immediately after child close",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 2,
    )
    .await;
    assert_eq!(after_close.descendant_count, 1);
    let child_entry = after_close
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("closed child breakdown entry");
    assert_known_task_scope(&child_entry.usage, MOCK_TURN_TOKEN_TOTAL);
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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
    let start =
        expect_agent_start_on_stream(&mut fixture.client, &agent_stream, "AgentStart").await;
    assert!(!start.agent_id.0.is_empty());
    assert_eq!(start.backend_kind, BackendKind::Claude);
    assert_eq!(start.name, "test-agent");

    // 4. Receive mock's initial turn: StreamStart → StreamDelta → StreamEnd
    expect_turn(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: hello",
    )
    .await;

    // 5. Send a follow-up message
    fixture
        .client
        .send_message(&agent_stream, "follow up".to_owned())
        .await
        .expect("send_message failed");

    // 6. Receive follow-up turn: StreamStart → StreamDelta → StreamEnd
    expect_turn(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: follow up",
    )
    .await;
}

#[tokio::test]
async fn agent_recovers_after_backend_error_without_idle() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("error-recovery-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);

    expect_turn_on_stream(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: hello",
    )
    .await;

    fixture
        .client
        .send_message(&agent_stream, MOCK_ERROR_WITHOUT_IDLE_SENTINEL.to_owned())
        .await
        .expect("send error sentinel failed");

    expect_error_message_on_stream(
        &mut fixture.client,
        &agent_stream,
        "mock backend emitted error without idle",
    )
    .await;
    expect_typing_false_on_stream(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "after backend error".to_owned())
        .await
        .expect("send follow-up failed");

    expect_turn_on_stream(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: after backend error",
    )
    .await;
}

#[tokio::test]
async fn agent_recovers_after_backend_tool_failure_without_idle() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("tool-failure-recovery-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);

    expect_turn_on_stream(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: hello",
    )
    .await;

    fixture
        .client
        .send_message(
            &agent_stream,
            MOCK_TOOL_FAILURE_WITHOUT_IDLE_SENTINEL.to_owned(),
        )
        .await
        .expect("send tool failure sentinel failed");

    expect_failed_tool_completion_on_stream(
        &mut fixture.client,
        &agent_stream,
        "history did not contain a tool_result",
    )
    .await;
    expect_typing_false_on_stream(&mut fixture.client, &agent_stream).await;

    fixture
        .client
        .send_message(&agent_stream, "after backend tool failure".to_owned())
        .await
        .expect("send follow-up failed");

    expect_turn_on_stream(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: after backend tool failure",
    )
    .await;
}

#[tokio::test]
async fn client_error_report_is_accepted_before_agent_flow() {
    let mut fixture = Fixture::new().await;

    send_client_error_report(
        &mut fixture.client,
        &ClientErrorPayload {
            code: ClientErrorCode::ProtocolParse,
            message: "failed to parse host frame".to_owned(),
            raw_context: Some("{not valid protocol json".to_owned()),
        },
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("after-client-error-report".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/client-error-report".to_owned()],
                prompt: "hello after client error report".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent after client error report failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent after client error report").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("parse NewAgent after client error report");
    assert_eq!(new_agent.name, "after-client-error-report");

    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "AgentStart after client error report",
    )
    .await;

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: hello after client error report",
    )
    .await;
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "close-agent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse close-agent NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    expect_agent_start_on_stream(&mut fixture.client, &agent_stream, "close-agent AgentStart")
        .await;

    expect_turn(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: hello",
    )
    .await;
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

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.agents.is_empty(),
        "closed agent should not replay to new clients"
    );
    expect_no_event(
        &mut late_client,
        Duration::from_millis(200),
        "closed agent should not replay to new clients",
    )
    .await;
}

#[tokio::test]
async fn close_agent_recursively_closes_descendants_first() {
    let mut fixture = Fixture::new().await;

    let (parent_new, _parent_start, relay_child_new, _relay_child_start) =
        spawn_parent_with_native_child(&mut fixture.client).await;
    let user_child_new = spawn_user_child(
        &mut fixture.client,
        &parent_new.agent_id,
        "close-user-child",
        "user child",
        "/tmp/close-user-child",
    )
    .await;
    let grandchild_new = spawn_user_child(
        &mut fixture.client,
        &user_child_new.agent_id,
        "close-grandchild",
        "grandchild",
        "/tmp/close-grandchild",
    )
    .await;

    let ids_before_close = fixture.agent_ids().await;
    for expected in [
        &parent_new.agent_id,
        &relay_child_new.agent_id,
        &user_child_new.agent_id,
        &grandchild_new.agent_id,
    ] {
        assert!(
            ids_before_close.contains(expected),
            "agent {expected} should be registered before close"
        );
    }

    fixture
        .client
        .close_agent(&parent_new.instance_stream)
        .await
        .expect("close parent failed");

    let mut closed_order = Vec::new();
    loop {
        let env = expect_next_event(&mut fixture.client, "recursive AgentClosed").await;
        if env.kind != FrameKind::AgentClosed {
            continue;
        }
        let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
        let is_parent = closed.agent_id == parent_new.agent_id;
        closed_order.push(closed.agent_id);
        if is_parent {
            break;
        }
    }

    let position = |agent_id: &protocol::AgentId| {
        closed_order
            .iter()
            .position(|closed| closed == agent_id)
            .unwrap_or_else(|| panic!("missing AgentClosed for {agent_id}: {closed_order:?}"))
    };
    let parent_position = position(&parent_new.agent_id);
    assert_eq!(
        parent_position,
        closed_order.len() - 1,
        "parent must be closed after descendants"
    );
    assert!(position(&relay_child_new.agent_id) < parent_position);
    assert!(position(&user_child_new.agent_id) < parent_position);
    assert!(position(&grandchild_new.agent_id) < position(&user_child_new.agent_id));

    let ids_after_close = fixture.agent_ids().await;
    for closed in [
        &parent_new.agent_id,
        &relay_child_new.agent_id,
        &user_child_new.agent_id,
        &grandchild_new.agent_id,
    ] {
        assert!(
            !ids_after_close.contains(closed),
            "agent {closed} should be removed after recursive close"
        );
    }

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    for closed in [
        &parent_new.agent_id,
        &relay_child_new.agent_id,
        &user_child_new.agent_id,
        &grandchild_new.agent_id,
    ] {
        assert!(
            !bootstrap
                .agents
                .iter()
                .any(|agent| &agent.agent_id == closed),
            "closed descendant {closed} should not replay to late clients"
        );
    }
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable Claude for launch-profile catalog");
    let control = fixture.connect_agent_control().await;
    let options = control.list_launch_options();
    assert!(
        options.catalog.entries.iter().any(|entry| {
            matches!(
                entry,
                protocol::LaunchProfileEntry::Ready { profile }
                    if profile.id.0 == "claude:default"
                        && profile.kind == LaunchProfileKind::BackendDefault
            )
        }),
        "dev-driver launch options should include claude:default"
    );

    let spawned = control
        .spawn_agent(SpawnRequest {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: "agent control hello".to_owned(),
            backend_kind: BackendKind::Claude,
            launch_profile_id: Some(LaunchProfileId("claude:default".to_owned())),
            session_settings: None,
            parent_agent_id: None,
            project_id: None,
            name: Some("agent-control".to_owned()),
            cost_hint: None,
            access_mode: Default::default(),
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

    await_dev_driver_agent_ready(&control, &spawned.agent_id, "initial agent-control turn").await;
    let cursor = assert_read_agent_contains(
        &control,
        &spawned.agent_id,
        None,
        "mock backend response to: agent control hello",
    )
    .await;

    let listed_after_wait = control.list_agents().await;
    assert_eq!(listed_after_wait.len(), 1);
    assert_eq!(listed_after_wait[0].status, AgentControlStatus::Idle);

    control
        .send_message(
            protocol::AgentId(spawned.agent_id.clone()),
            "agent control follow up".to_owned(),
        )
        .await
        .expect("agent control send_message should succeed");

    await_dev_driver_agent_ready(&control, &spawned.agent_id, "agent-control follow-up turn").await;
    assert_read_agent_contains(
        &control,
        &spawned.agent_id,
        cursor,
        "mock backend response to: agent control follow up",
    )
    .await;

    let listed_after_follow_up = control.list_agents().await;
    assert_eq!(listed_after_follow_up.len(), 1);
    assert_eq!(listed_after_follow_up[0].status, AgentControlStatus::Idle);
}

#[tokio::test]
async fn agent_control_dev_driver_spawns_explicit_hermes_launch_profile_after_schema_refresh() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::LaunchProfiles {
                profiles: vec![hermes_claude_launch_profile()],
            },
        })
        .await
        .expect("configure explicit Hermes launch profile");
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Hermes],
            },
        })
        .await
        .expect("enable Hermes");

    wait_for_ready_launch_profile(&mut fixture.client, "hermes:claude").await;

    let control = fixture.connect_agent_control().await;
    let options = control.list_launch_options();
    assert!(
        options.catalog.entries.iter().any(|entry| {
            matches!(
                entry,
                protocol::LaunchProfileEntry::Ready { profile }
                    if profile.id.0 == "hermes:claude"
                        && profile.kind == LaunchProfileKind::Custom
                        && profile.backend_kind == BackendKind::Hermes
                        && profile.session_settings == hermes_claude_session_settings()
            )
        }),
        "dev-driver launch options should include ready hermes:claude"
    );

    let spawned = control
        .spawn_agent(SpawnRequest {
            workspace_roots: vec!["/tmp/agent-control-hermes-dev-driver".to_owned()],
            prompt: "agent control explicit Hermes launch profile".to_owned(),
            backend_kind: BackendKind::Hermes,
            launch_profile_id: Some(LaunchProfileId("hermes:claude".to_owned())),
            session_settings: None,
            parent_agent_id: None,
            project_id: None,
            name: Some("explicit-hermes-dev-driver".to_owned()),
            cost_hint: None,
            access_mode: Default::default(),
        })
        .await
        .expect("agent control spawn should succeed");

    await_dev_driver_agent_ready(
        &control,
        &spawned.agent_id,
        "explicit Hermes launch-profile turn",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_discovers_and_spawns_launch_profiles() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable Claude");

    let base_url = fixture.agent_control_http_url().await;
    let options = mcp_list_launch_options(&base_url).await;
    let entries = options["catalog"]["entries"]
        .as_array()
        .expect("launch option entries");
    assert!(
        entries
            .iter()
            .any(|entry| entry["state"] == "ready" && entry["profile"]["id"] == "claude:default"),
        "expected claude:default in {options}"
    );

    let agent_id = mcp_spawn_agent_with_arguments(
        &base_url,
        json!({
            "workspace_roots": ["/tmp/agent-control-launch-profile"],
            "prompt": "agent control launch profile",
            "launch_profile_id": "claude:default",
            "session_settings": {
                "model": { "string": "haiku" }
            },
            "name": "profile child"
        }),
    )
    .await;
    let awaited = mcp_await_agent(&base_url, &agent_id).await;
    assert_await_result_ready(&awaited, &agent_id);
}

#[tokio::test]
async fn agent_control_http_spawns_explicit_hermes_launch_profile_after_schema_refresh() {
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::LaunchProfiles {
                profiles: vec![hermes_claude_launch_profile()],
            },
        })
        .await
        .expect("configure explicit Hermes launch profile");
    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Hermes],
            },
        })
        .await
        .expect("enable Hermes");

    let catalog = wait_for_ready_launch_profile(&mut fixture.client, "hermes:claude").await;
    assert!(
        catalog.catalog.entries.iter().any(|entry| {
            matches!(
                entry,
                protocol::LaunchProfileEntry::Ready { profile }
                    if profile.id.0 == "hermes:claude"
                        && profile.kind == LaunchProfileKind::Custom
                        && profile.backend_kind == BackendKind::Hermes
                        && profile.session_settings == hermes_claude_session_settings()
            )
        }),
        "expected ready hermes:claude in {catalog:?}"
    );

    let base_url = fixture.agent_control_http_url().await;
    let options = mcp_list_launch_options(&base_url).await;
    let entries = options["catalog"]["entries"]
        .as_array()
        .expect("launch option entries");
    assert!(
        entries.iter().any(|entry| {
            entry["state"] == "ready"
                && entry["profile"]["id"] == "hermes:claude"
                && entry["profile"]["backend_kind"] == "hermes"
        }),
        "expected ready hermes:claude in {options}"
    );

    let agent_id = mcp_spawn_agent_with_arguments(
        &base_url,
        json!({
            "workspace_roots": ["/tmp/agent-control-hermes-launch-profile"],
            "prompt": "agent control explicit Hermes launch profile",
            "launch_profile_id": "hermes:claude",
            "name": "explicit hermes profile"
        }),
    )
    .await;
    let awaited = mcp_await_agent(&base_url, &agent_id).await;
    assert_await_result_ready(&awaited, &agent_id);
}

#[tokio::test]
async fn agent_control_http_await_returns_while_exit_plan_mode_is_pending() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("await-exit-plan-mode".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-exit-plan-mode".to_owned()],
                prompt: "__mock_exit_plan_mode__".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn ExitPlanMode agent failed");

    let env = expect_next_event(&mut fixture.client, "ExitPlanMode NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse ExitPlanMode NewAgent");

    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "ExitPlanMode AgentStart",
    )
    .await;
    assert_eq!(start.agent_id, new_agent.agent_id);

    let request =
        wait_for_exit_plan_mode_pause_on_stream(&mut fixture.client, &new_agent.instance_stream)
            .await;

    let base_url = fixture.agent_control_http_url().await;
    let pending_await = tokio::time::timeout(
        Duration::from_secs(2),
        mcp_await_agent(&base_url, &new_agent.agent_id),
    )
    .await
    .expect("tyde_await_agents must return while plan approval is pending");
    assert_await_result_ready(&pending_await, &new_agent.agent_id);

    fixture
        .client
        .send_message_payload(
            &new_agent.instance_stream,
            SendMessagePayload {
                message: String::new(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::ExitPlanMode {
                    tool_call_id: request.tool_call_id,
                    decision: protocol::ExitPlanModeDecision::Approve,
                    feedback: None,
                }),
            },
        )
        .await
        .expect("send ExitPlanMode approval");

    let resumed_await = tokio::time::timeout(
        Duration::from_secs(5),
        mcp_await_agent(&base_url, &new_agent.agent_id),
    )
    .await
    .expect("tyde_await_agents must return after plan approval resumes the turn");
    assert_await_result_ready(&resumed_await, &new_agent.agent_id);

    let mut saw_completion = false;
    let mut saw_approval = false;
    let mut saw_final_idle = false;
    loop {
        let env = expect_chat_event_on_stream(
            &mut fixture.client,
            &new_agent.instance_stream,
            "ExitPlanMode approval completion",
        )
        .await;
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_name == "ExitPlanMode" =>
            {
                assert!(completion.success);
                saw_completion = true;
            }
            ChatEvent::StreamEnd(end)
                if end.message.content.contains("mock ExitPlanMode approved") =>
            {
                saw_approval = true;
            }
            ChatEvent::TypingStatusChanged(false) if saw_approval => {
                saw_final_idle = true;
            }
            _ => {}
        }
        if saw_completion && saw_approval && saw_final_idle {
            break;
        }
    }

    fixture
        .client
        .send_message(&new_agent.instance_stream, "after plan approval".to_owned())
        .await
        .expect("send follow-up after plan approval");
    expect_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: after plan approval",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_await_stays_active_after_exit_plan_mode_approval() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("await-exit-plan-mode-resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-exit-plan-mode-resume".to_owned()],
                prompt: "__mock_exit_plan_mode_stream_end_first__".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn stream-end-first ExitPlanMode agent failed");

    let env = expect_next_event(
        &mut fixture.client,
        "stream-end-first ExitPlanMode NewAgent",
    )
    .await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("parse stream-end-first NewAgent");

    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "stream-end-first ExitPlanMode AgentStart",
    )
    .await;
    assert_eq!(start.agent_id, new_agent.agent_id);

    let request =
        wait_for_exit_plan_mode_pause_on_stream(&mut fixture.client, &new_agent.instance_stream)
            .await;

    let base_url = fixture.agent_control_http_url().await;
    let pending_await = tokio::time::timeout(
        Duration::from_secs(2),
        mcp_await_agent(&base_url, &new_agent.agent_id),
    )
    .await
    .expect("tyde_await_agents must return while stream-end-first plan approval is pending");
    assert_await_result_ready(&pending_await, &new_agent.agent_id);

    fixture
        .client
        .send_message_payload(
            &new_agent.instance_stream,
            SendMessagePayload {
                message: String::new(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::ExitPlanMode {
                    tool_call_id: request.tool_call_id,
                    decision: protocol::ExitPlanModeDecision::Approve,
                    feedback: None,
                }),
            },
        )
        .await
        .expect("send stream-end-first ExitPlanMode approval");

    wait_for_agent_control_status(
        &base_url,
        &new_agent.agent_id,
        "thinking",
        Duration::from_millis(500),
    )
    .await;

    let mut saw_completion = false;
    while !saw_completion {
        let env = expect_chat_event_on_stream(
            &mut fixture.client,
            &new_agent.instance_stream,
            "stream-end-first ExitPlanMode completion",
        )
        .await;
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if let ChatEvent::ToolExecutionCompleted(completion) = event
            && completion.tool_name == "ExitPlanMode"
        {
            assert!(completion.success);
            saw_completion = true;
        }
    }

    wait_for_agent_control_status(
        &base_url,
        &new_agent.agent_id,
        "thinking",
        Duration::from_millis(500),
    )
    .await;

    let await_while_resuming = tokio::time::timeout(
        Duration::from_millis(150),
        mcp_await_agent(&base_url, &new_agent.agent_id),
    )
    .await;
    assert!(
        await_while_resuming.is_err(),
        "tyde_await_agents must not report ready between plan approval completion and resumed turn finish"
    );

    let mut saw_approval = false;
    let mut saw_final_idle = false;
    loop {
        let env = expect_chat_event_on_stream(
            &mut fixture.client,
            &new_agent.instance_stream,
            "stream-end-first resumed turn finish",
        )
        .await;
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        match event {
            ChatEvent::StreamEnd(end)
                if end.message.content.contains("mock ExitPlanMode approved") =>
            {
                saw_approval = true;
            }
            ChatEvent::TypingStatusChanged(false) if saw_approval => {
                saw_final_idle = true;
            }
            _ => {}
        }
        if saw_approval && saw_final_idle {
            break;
        }
    }

    let finished_await = tokio::time::timeout(
        Duration::from_secs(2),
        mcp_await_agent(&base_url, &new_agent.agent_id),
    )
    .await
    .expect("tyde_await_agents must return after stream-end-first resumed turn finishes");
    assert_await_result_ready(&finished_await, &new_agent.agent_id);
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
            launch_profile_id: None,
            session_settings: None,
            parent_agent_id: None,
            project_id: None,
            name: None,
            cost_hint: None,
            access_mode: Default::default(),
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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
    assert_eq!(child_new.project_id, None);

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
    assert_eq!(child_start.project_id, None);
}

#[tokio::test]
async fn agent_control_http_rejects_unknown_tool_arguments() {
    let fixture = Fixture::new().await;
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
                    "workspace_roots": ["/tmp/reject-unknown-args"],
                    "prompt": "this should not spawn",
                    "backendKind": "tycode",
                    "name": "unknown-args-child"
                }
            }
        }),
    )
    .await;

    let rpc_error = response.get("error").is_some();
    let tool_error = response
        .get("result")
        .and_then(|result| {
            result
                .get("isError")
                .or_else(|| result.get("is_error"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    assert!(
        rpc_error || tool_error,
        "unknown tool arguments must be rejected, got {response}"
    );
    assert!(
        response.to_string().contains("backendKind")
            || response.to_string().contains("unknown field")
            || response.to_string().contains("invalid"),
        "rejection should mention invalid/unknown argument, got {response}"
    );
    assert!(
        fixture.agent_ids().await.is_empty(),
        "unknown tool arguments must not fall back to the default backend and spawn an agent"
    );
}

#[tokio::test]
async fn agent_control_http_await_emits_progress_notifications() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("await-progress".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-progress".to_owned()],
                prompt: "__mock_hold_until_interrupt__ await progress".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn held agent failed");

    let env = expect_next_event(&mut fixture.client, "await progress NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse await progress NewAgent");

    let base_url = fixture.agent_control_http_url().await;
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let transport = StreamableHttpClientTransport::from_uri(base_url);
    let service = ProgressRecorder { tx: progress_tx }
        .serve(transport)
        .await
        .expect("connect to agent-control MCP");
    let progress_token = ProgressToken(NumberOrString::String("await-progress-test".into()));
    let arguments = json!({
        "agent_ids": [new_agent.agent_id.0.clone()]
    })
    .as_object()
    .cloned();
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams {
        meta: None,
        name: "tyde_await_agents".into(),
        arguments,
        task: None,
    }));
    let handle = service
        .send_request_with_option(
            request,
            PeerRequestOptions {
                timeout: None,
                meta: Some(Meta::with_progress_token(progress_token.clone())),
            },
        )
        .await
        .expect("send await request");
    let response = handle.await_response();
    tokio::pin!(response);

    let progress = tokio::select! {
        progress = progress_rx.recv() => {
            progress.expect("progress channel should stay open")
        }
        result = &mut response => {
            panic!("await request completed before progress notification: {result:?}");
        }
    };
    assert_eq!(progress.progress_token, progress_token);
    assert!(progress.progress >= 1.0);
    assert!(
        progress
            .message
            .as_deref()
            .is_some_and(|message| message.contains("Waiting for 1 Tyde agent")),
        "unexpected progress message: {:?}",
        progress.message
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut response)
            .await
            .is_err(),
        "await request must not return while the agent is still thinking"
    );

    fixture
        .client
        .interrupt(&new_agent.instance_stream)
        .await
        .expect("interrupt held agent");

    let server_result = tokio::time::timeout(Duration::from_secs(5), &mut response)
        .await
        .expect("await tool should finish after interrupt")
        .expect("await tool response should succeed");
    let ServerResult::CallToolResult(result) = server_result else {
        panic!("expected CallToolResult, got {server_result:?}");
    };
    assert_eq!(result.is_error, Some(false));
    let content = result
        .content
        .first()
        .expect("await result should include content");
    let RawContent::Text(text) = &content.raw else {
        panic!("expected text await result, got {:?}", content.raw);
    };
    let body: Value = serde_json::from_str(&text.text).expect("parse await result JSON");
    assert_eq!(
        body.get("ready")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or_default(),
        1
    );

    service.cancel().await.expect("cancel MCP client");
}

#[tokio::test]
async fn agent_control_await_tool_call_emits_correlated_completion_when_child_becomes_ready() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("await-tool-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-tool-parent".to_owned()],
                prompt: "parent ready".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "await-tool parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");

    let parent_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "await-tool parent AgentStart",
    )
    .await;
    assert_eq!(parent_start.agent_id, parent_new.agent_id);

    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent ready",
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("await-tool-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_new.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-tool-child".to_owned()],
                prompt: "__mock_hold_until_interrupt__ child awaited".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn child failed");

    let env = expect_next_event(&mut fixture.client, "await-tool child NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let child_new: NewAgentPayload = env.parse_payload().expect("parse child NewAgent");
    assert_eq!(
        child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "await-tool child AgentStart",
    )
    .await;
    assert_eq!(child_start.agent_id, child_new.agent_id);
    assert_eq!(
        child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    expect_stream_end_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend held response to: __mock_hold_until_interrupt__ child awaited",
    )
    .await;

    let base_url = fixture.agent_control_http_url().await;
    wait_for_agent_control_status(
        &base_url,
        &child_new.agent_id,
        "thinking",
        Duration::from_secs(1),
    )
    .await;

    fixture
        .client
        .send_message(
            &parent_new.instance_stream,
            format!(
                "{} {}",
                MOCK_AGENT_CONTROL_AWAIT_SENTINEL, child_new.agent_id.0
            ),
        )
        .await
        .expect("send parent await tool prompt");

    let await_request = expect_tool_request_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "tyde_await_agents",
    )
    .await;
    let ToolRequest {
        tool_call_id,
        tool_name,
        tool_type,
    } = await_request;
    assert_eq!(tool_name, "tyde_await_agents");
    let ToolRequestType::Other { args } = tool_type else {
        panic!("expected tyde_await_agents ToolRequest to use Other args");
    };
    assert_eq!(
        args.get("agent_ids")
            .and_then(Value::as_array)
            .and_then(|agent_ids| agent_ids.first())
            .and_then(Value::as_str),
        Some(child_new.agent_id.0.as_str())
    );

    fixture
        .client
        .interrupt(&child_new.instance_stream)
        .await
        .expect("interrupt held child");
    expect_operation_cancelled_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend interrupted held turn",
    )
    .await;

    wait_for_agent_control_status(
        &base_url,
        &child_new.agent_id,
        "idle",
        Duration::from_secs(2),
    )
    .await;

    let completion = expect_tool_completion_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        &tool_call_id,
    )
    .await;
    assert_eq!(completion.tool_call_id, tool_call_id);
    assert_eq!(completion.tool_name, "tyde_await_agents");
    assert!(
        completion.success,
        "await completion failed: {completion:?}"
    );
    let ToolExecutionResult::Other { result } = completion.tool_result else {
        panic!("expected await completion to carry MCP result JSON");
    };
    assert_await_result_ready(&result, &child_new.agent_id);

    expect_stream_end_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock agent-control await completed",
    )
    .await;
    expect_typing_false_on_stream(&mut fixture.client, &parent_new.instance_stream).await;
}

#[tokio::test]
async fn agent_control_http_respects_explicit_parent_agent_id_in_tool_arguments() {
    let mut fixture = Fixture::new().await;
    let parent_project = create_project(
        &mut fixture.client,
        "Explicit Parent Project",
        vec!["/tmp/explicit-parent".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("explicit-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(parent_project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&parent_project),
                prompt: "parent".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "explicit parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse explicit parent NewAgent");
    assert_eq!(parent_new.project_id.as_ref(), Some(&parent_project.id));

    let parent_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "explicit parent AgentStart",
    )
    .await;
    assert_eq!(parent_start.project_id.as_ref(), Some(&parent_project.id));
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
    assert_eq!(
        child_new.project_id.as_ref(),
        Some(&parent_project.id),
        "child spawned with explicit parent_agent_id and no caller must inherit parent project_id"
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
    assert_eq!(
        child_start.project_id.as_ref(),
        Some(&parent_project.id),
        "AgentStart must inherit explicit parent's project_id"
    );
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend response to: child with explicit parent",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_unknown_parent_does_not_fabricate_project_id() {
    let mut fixture = Fixture::new().await;
    let base_url = fixture.agent_control_http_url().await;
    let unknown_parent_id = protocol::AgentId("11111111-1111-1111-1111-111111111111".to_owned());
    let child_agent_id = mcp_spawn_agent_with_arguments(
        &base_url,
        json!({
            "workspace_roots": ["/tmp/unknown-parent-child"],
            "prompt": "child with unknown parent",
            "backend_kind": "claude",
            "name": "unknown-parent-child",
            "parent_agent_id": unknown_parent_id.0.clone()
        }),
    )
    .await;

    let child_new = expect_replayed_new_agent(
        &mut fixture.client,
        &child_agent_id,
        "unknown parent child NewAgent",
    )
    .await;
    assert_eq!(child_new.parent_agent_id.as_ref(), Some(&unknown_parent_id));
    assert_eq!(child_new.project_id, None);

    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "unknown parent child AgentStart",
    )
    .await;
    assert_eq!(
        child_start.parent_agent_id.as_ref(),
        Some(&unknown_parent_id)
    );
    assert_eq!(child_start.project_id, None);
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend response to: child with unknown parent",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_inherits_project_id_from_parent_unless_overridden() {
    let mut fixture = Fixture::new().await;
    let parent_project = create_project(
        &mut fixture.client,
        "Agent Control Parent Project",
        vec!["/tmp/agent-control-parent-project".to_owned()],
    )
    .await;
    let override_project = create_project(
        &mut fixture.client,
        "Agent Control Override Project",
        vec!["/tmp/agent-control-override-project".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("project-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(parent_project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&parent_project),
                prompt: "parent project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn project parent failed");

    let env = expect_next_event(&mut fixture.client, "project parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse project parent NewAgent");
    assert_eq!(parent_new.project_id.as_ref(), Some(&parent_project.id));

    let parent_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "project parent AgentStart",
    )
    .await;
    assert_eq!(parent_start.project_id.as_ref(), Some(&parent_project.id));
    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent project",
    )
    .await;

    let base_url = fixture.agent_control_http_url().await;
    let caller_url = format!("{base_url}?agent_id={}", parent_new.agent_id.0);
    let inherited_child_root = "/tmp/agent-control-inherited-child";
    let inherited_child_id = mcp_spawn_agent_with_arguments(
        &caller_url,
        json!({
            "workspace_roots": [inherited_child_root],
            "prompt": "child inherits project",
            "backend_kind": "claude",
            "name": "inherited-project-child"
        }),
    )
    .await;

    let inherited_child_new = expect_replayed_new_agent(
        &mut fixture.client,
        &inherited_child_id,
        "inherited project child NewAgent",
    )
    .await;
    assert_eq!(
        inherited_child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );
    assert_eq!(
        inherited_child_new.project_id.as_ref(),
        Some(&parent_project.id)
    );

    let inherited_child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &inherited_child_new.instance_stream,
        "inherited project child AgentStart",
    )
    .await;
    assert_eq!(
        inherited_child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );
    assert_eq!(
        inherited_child_start.project_id.as_ref(),
        Some(&parent_project.id)
    );
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &inherited_child_new.instance_stream,
        "mock backend response to: child inherits project",
    )
    .await;

    let override_child_root = "/tmp/agent-control-override-child";
    let override_child_id = mcp_spawn_agent_with_arguments(
        &caller_url,
        json!({
            "workspace_roots": [override_child_root],
            "prompt": "child overrides project",
            "backend_kind": "claude",
            "name": "override-project-child",
            "project_id": override_project.id.0.clone()
        }),
    )
    .await;

    let override_child_new = expect_replayed_new_agent(
        &mut fixture.client,
        &override_child_id,
        "override project child NewAgent",
    )
    .await;
    assert_eq!(
        override_child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );
    assert_eq!(
        override_child_new.project_id.as_ref(),
        Some(&override_project.id)
    );

    let override_child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &override_child_new.instance_stream,
        "override project child AgentStart",
    )
    .await;
    assert_eq!(
        override_child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );
    assert_eq!(
        override_child_start.project_id.as_ref(),
        Some(&override_project.id)
    );
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &override_child_new.instance_stream,
        "mock backend response to: child overrides project",
    )
    .await;

    let listed = mcp_list_agents(&base_url).await;
    assert_eq!(
        mcp_listed_agent(&listed, &inherited_child_id)
            .get("project_id")
            .and_then(Value::as_str),
        Some(parent_project.id.0.as_str())
    );
    assert_eq!(
        mcp_listed_agent(&listed, &override_child_id)
            .get("project_id")
            .and_then(Value::as_str),
        Some(override_project.id.0.as_str())
    );

    let (_fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let inherited_session = fresh_bootstrap
        .sessions
        .iter()
        .find(|session| session.workspace_roots == vec![inherited_child_root.to_owned()])
        .expect("inherited child session missing from fresh host bootstrap");
    assert_eq!(
        inherited_session.project_id.as_ref(),
        Some(&parent_project.id)
    );
    let override_session = fresh_bootstrap
        .sessions
        .iter()
        .find(|session| session.workspace_roots == vec![override_child_root.to_owned()])
        .expect("override child session missing from fresh host bootstrap");
    assert_eq!(
        override_session.project_id.as_ref(),
        Some(&override_project.id)
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_child_new = bootstrapped_agent(&bootstrap, &child_new.agent_id);
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

    expect_replayed_turn_on_stream(
        &mut late_client,
        &replayed_child_new.instance_stream,
        &replayed_child_new.agent_id,
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

    let resumed_agent = loop {
        let env = expect_next_event(
            &mut fixture.client,
            "resume of non-resumable backend-native child session",
        )
        .await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env
                    .parse_payload()
                    .expect("parse non-resumable child resume NewAgent");
                if payload.session_id.as_ref() == Some(&child.id) {
                    break payload;
                }
            }
            FrameKind::CommandError => {
                let error: CommandErrorPayload = env
                    .parse_payload()
                    .expect("parse unexpected non-resumable child resume CommandError");
                panic!(
                    "non-resumable backend-native child resume should become agent startup failure, not CommandError: {error:?}"
                );
            }
            _ => {}
        }
    };

    let resumed_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &resumed_agent.instance_stream,
        "non-resumable child resume AgentStart",
    )
    .await;
    assert_eq!(resumed_start.session_id.as_ref(), Some(&child.id));

    let error = expect_agent_error_containing(
        &mut fixture.client,
        &resumed_agent.instance_stream,
        &format!("cannot resume non-resumable session {}", child.id),
        "non-resumable child resume AgentError",
    )
    .await;
    assert_eq!(error.code, AgentErrorCode::Unsupported);
    assert!(error.fatal);

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("connection should remain usable after non-resumable child resume failure");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "SessionList after non-resumable child resume failure",
    )
    .await;
}

/// Regression: when a backend-native child's event stream closes (e.g. the
/// backend finishes the sub-agent turn and drops its emitter handle), the
/// relay agent actor used to just `return`, leaving a dead mpsc sender in
/// the registry. The next host-stream replay called `snapshot()` on that
/// handle and panicked. The fix parks the relay actor on event-stream close
/// so Snapshot/ReadOutput/Attach keep working until the host explicitly
/// closes the agent.
#[tokio::test]
async fn backend_native_child_with_closed_event_stream_still_replays_to_late_clients() {
    let mut fixture = Fixture::new().await;

    let parent_prompt = format!("parent prompt {MOCK_NATIVE_CHILD_AND_DROP_SENTINEL}");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("parent-with-dropped-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/dropped-sub-agent-parent".to_owned()],
                prompt: parent_prompt,
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent with native child failed");

    let parent_new_env = expect_next_event(&mut fixture.client, "parent NewAgent").await;
    assert_eq!(parent_new_env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = parent_new_env.parse_payload().expect("parse NewAgent");

    let parent_start_env = expect_next_event(&mut fixture.client, "parent AgentStart").await;
    assert_eq!(parent_start_env.kind, FrameKind::AgentStart);

    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;

    let child_new_env = expect_next_event(&mut fixture.client, "native child NewAgent").await;
    assert_eq!(child_new_env.kind, FrameKind::NewAgent);
    let child_new: NewAgentPayload = child_new_env
        .parse_payload()
        .expect("parse native child NewAgent");

    let child_start_env = expect_next_event(&mut fixture.client, "native child AgentStart").await;
    assert_eq!(child_start_env.kind, FrameKind::AgentStart);

    expect_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock native child response to: parent prompt",
    )
    .await;

    // After the child's turn ends, the mock dropped the emitter handle so
    // the relay actor's backend event stream is closed. Give the actor a
    // beat to process the None and transition into its parked state.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect a late client — this triggers register_host_stream, which
    // snapshots every registered agent. Without the park, this panics the
    // server and the client sees a closed connection.
    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_child_new = bootstrapped_agent(&bootstrap, &child_new.agent_id);
    assert_eq!(replayed_child_new.origin, AgentOrigin::BackendNative);

    let replayed_child_start = expect_agent_start_on_stream(
        &mut late_client,
        &replayed_child_new.instance_stream,
        "late client native child AgentStart",
    )
    .await;
    assert_eq!(replayed_child_start.origin, AgentOrigin::BackendNative);
}

#[tokio::test]
async fn interrupting_parked_backend_native_child_emits_relay_rejection() {
    let mut fixture = Fixture::new().await;

    let parent_prompt = format!("parent prompt {MOCK_NATIVE_CHILD_AND_DROP_SENTINEL}");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("parent-with-dropped-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/dropped-sub-agent-parent".to_owned()],
                prompt: parent_prompt,
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent with native child failed");

    let parent_new_env = expect_next_event(&mut fixture.client, "parent NewAgent").await;
    assert_eq!(parent_new_env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = parent_new_env.parse_payload().expect("parse NewAgent");

    let parent_start_env = expect_next_event(&mut fixture.client, "parent AgentStart").await;
    assert_eq!(parent_start_env.kind, FrameKind::AgentStart);

    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;

    let child_new_env = expect_next_event(&mut fixture.client, "native child NewAgent").await;
    assert_eq!(child_new_env.kind, FrameKind::NewAgent);
    let child_new: NewAgentPayload = child_new_env
        .parse_payload()
        .expect("parse native child NewAgent");

    let child_start_env = expect_next_event(&mut fixture.client, "native child AgentStart").await;
    assert_eq!(child_start_env.kind, FrameKind::AgentStart);

    expect_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock native child response to: parent prompt",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    fixture
        .client
        .interrupt(&child_new.instance_stream)
        .await
        .expect("interrupt parked backend-native child failed");

    let err = expect_agent_error_message_without(
        &mut fixture.client,
        &child_new.instance_stream,
        "backend-native relay agents do not accept direct input",
        "agent not running",
        "parked backend-native child relay rejection",
    )
    .await;
    assert!(!err.fatal);
    expect_no_agent_error_message(
        &mut fixture.client,
        &child_new.instance_stream,
        "agent not running",
        Duration::from_millis(100),
        "router generic error after relay rejection",
    )
    .await;
}

#[tokio::test]
async fn interrupting_parent_keeps_agent_control_children_running() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("interrupt-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/interrupt-parent".to_owned()],
                prompt: "__mock_hold_until_interrupt__ parent waiting".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent failed");

    let env = expect_next_event(&mut fixture.client, "interrupt parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");

    let env = expect_next_event(&mut fixture.client, "interrupt parent AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, parent_new.instance_stream);

    let env = expect_chat_event_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "parent TypingStatusChanged(true)",
    )
    .await;
    let event: ChatEvent = env.parse_payload().expect("parse parent typing true");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    let base_url = fixture.agent_control_http_url().await;
    let child_agent_id = mcp_spawn_agent(
        &format!("{base_url}?agent_id={}", parent_new.agent_id.0),
        "__mock_slow__ agent-control child first",
        "agent-control-child",
    )
    .await;

    let child_new = expect_replayed_new_agent(
        &mut fixture.client,
        &child_agent_id,
        "agent-control child NewAgent",
    )
    .await;
    assert_eq!(child_new.origin, AgentOrigin::AgentControl);
    assert_eq!(
        child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "agent-control child AgentStart",
    )
    .await;
    assert_eq!(child_start.origin, AgentOrigin::AgentControl);
    assert_eq!(
        child_start.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );

    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend response to: __mock_slow__ agent-control child first",
    )
    .await;

    fixture
        .client
        .interrupt(&parent_new.instance_stream)
        .await
        .expect("interrupt parent failed");
    expect_operation_cancelled_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend interrupted held turn",
    )
    .await;

    fixture
        .client
        .send_message(
            &child_new.instance_stream,
            "agent-control child follow-up".to_owned(),
        )
        .await
        .expect("child should accept follow-up after parent interrupt");
    expect_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock backend response to: agent-control child follow-up",
    )
    .await;
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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

    let err = expect_agent_error_containing(
        &mut fixture.client,
        &agent_stream,
        "mock backend forced spawn failure",
        "fatal AgentError",
    )
    .await;
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
        .interrupt(&agent_stream)
        .await
        .expect("interrupt terminal agent should still write protocol frame");

    let interrupt_not_running = expect_agent_error_message(
        &mut fixture.client,
        &agent_stream,
        "agent not running",
        "terminal agent interrupt should reject with generic not-running",
    )
    .await;
    assert!(!interrupt_not_running.fatal);

    fixture
        .client
        .send_message(&agent_stream, "after failure".to_owned())
        .await
        .expect("send_message after failure should still write protocol frame");

    let err = expect_agent_error_message(
        &mut fixture.client,
        &agent_stream,
        "agent not running",
        "agent not running error",
    )
    .await;
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn without explicit name failed");

    let env = expect_next_event(&mut fixture.client, "generated-name NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse generated NewAgent");
    assert_eq!(new_agent.name, "Review Auth Logs");

    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "generated-name AgentStart",
    )
    .await;
    assert_eq!(start.name, "Review Auth Logs");

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
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
async fn agent_activity_summaries_default_off_stay_disabled() {
    let mut fixture = Fixture::new().await;
    assert!(
        fixture
            .bootstrap
            .settings
            .background_agent_features
            .auto_generate_agent_names
    );
    assert!(
        !fixture
            .bootstrap
            .settings
            .background_agent_features
            .agent_activity_summaries
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("summary-off".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "summaries are disabled".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn with summaries off failed");

    let env = expect_next_event(&mut fixture.client, "summaries-off NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse summaries-off NewAgent");
    assert!(matches!(
        new_agent.activity_summary,
        AgentActivitySummaryState::Disabled
    ));
    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "summaries-off AgentStart",
    )
    .await;

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: summaries are disabled",
    )
    .await;
    expect_no_event(
        &mut fixture.client,
        Duration::from_secs(1),
        "activity summary while feature is disabled",
    )
    .await;
    assert_eq!(
        fixture.agent_ids().await,
        vec![new_agent.agent_id],
        "disabled summarizer must not register background agents"
    );
}

#[tokio::test]
async fn agent_activity_summaries_emit_fresh_mock_state_and_bootstrap() {
    let mut fixture = Fixture::new().await;
    set_activity_summaries(&mut fixture.client, true).await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("summary-on".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "summarize recent activity".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn with summaries on failed");

    let env = expect_next_event(&mut fixture.client, "summaries-on NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse summaries-on NewAgent");
    assert!(matches!(
        new_agent.activity_summary,
        AgentActivitySummaryState::Empty
    ));
    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "summaries-on AgentStart",
    )
    .await;

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: summarize recent activity",
    )
    .await;

    let pending = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;
    assert!(matches!(
        pending.state,
        AgentActivitySummaryState::Pending { previous: None, .. }
    ));

    let fresh = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "Fresh",
        |state| matches!(state, AgentActivitySummaryState::Fresh { .. }),
    )
    .await;
    let AgentActivitySummaryState::Fresh { summary } = fresh.state else {
        panic!("expected Fresh summary");
    };
    assert_eq!(
        summary.text,
        "Mock summary: agent is working on recent activity"
    );
    assert!(summary.generated_at_ms > 0);
    assert!(summary.source_through_seq.is_some());
    assert_eq!(
        fixture.agent_ids().await,
        vec![new_agent.agent_id.clone()],
        "summary helper must not register transient agents"
    );

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrapped_agent(&bootstrap, &new_agent.agent_id);
    assert!(matches!(
        replayed.activity_summary,
        AgentActivitySummaryState::Fresh { .. }
    ));

    fixture
        .client
        .close_agent(&new_agent.instance_stream)
        .await
        .expect("close summarized agent");
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "summarized AgentClosed",
    )
    .await;
    let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
    assert_eq!(closed.agent_id, new_agent.agent_id);
    assert!(
        fixture.agent_ids().await.is_empty(),
        "closed summarized agent should be removed from registry"
    );
    let (_after_close_client, after_close_bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        after_close_bootstrap.agents.is_empty(),
        "closed summarized agent should not replay activity state"
    );
}

#[tokio::test]
async fn disabling_activity_summaries_discards_in_flight_result() {
    let mut fixture = Fixture::new().await;
    set_activity_summaries(&mut fixture.client, true).await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("summary-offswitch".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "__mock_slow_activity_summary__ keep working".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn slow summary agent failed");

    let env = expect_next_event(&mut fixture.client, "slow-summary NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse slow-summary NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "slow-summary AgentStart",
    )
    .await;

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: __mock_slow_activity_summary__ keep working",
    )
    .await;
    let _pending = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "slow Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;

    set_activity_summaries(&mut fixture.client, false).await;
    let disabled = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "Disabled",
        |state| matches!(state, AgentActivitySummaryState::Disabled),
    )
    .await;
    assert!(matches!(
        disabled.state,
        AgentActivitySummaryState::Disabled
    ));

    expect_no_event(
        &mut fixture.client,
        Duration::from_secs(3),
        "late activity summary after disabling",
    )
    .await;
    assert_eq!(
        fixture.agent_ids().await,
        vec![new_agent.agent_id],
        "off switch must not leave registered summary agents"
    );
}

#[tokio::test]
async fn agent_activity_summaries_error_state_backs_off_retries() {
    let mut fixture = Fixture::new().await;
    set_activity_summaries(&mut fixture.client, true).await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("summary-error".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "__mock_fail_activity_summary__ keep working".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn failing summary agent failed");

    let env = expect_next_event(&mut fixture.client, "failing-summary NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse failing-summary NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "failing-summary AgentStart",
    )
    .await;

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: __mock_fail_activity_summary__ keep working",
    )
    .await;
    let _pending = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "error Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;

    let error = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "Error",
        |state| matches!(state, AgentActivitySummaryState::Error { .. }),
    )
    .await;
    let AgentActivitySummaryState::Error {
        message, previous, ..
    } = error.state
    else {
        panic!("expected Error summary state");
    };
    assert_eq!(message, "mock activity summary failure");
    assert!(previous.is_none());

    fixture
        .client
        .send_message(
            &new_agent.instance_stream,
            "activity after summary failure".to_owned(),
        )
        .await
        .expect("send follow-up after summary failure failed");
    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: activity after summary failure",
    )
    .await;
    expect_no_event(
        &mut fixture.client,
        Duration::from_secs(7),
        "immediate activity summary retry after failure",
    )
    .await;
}

#[tokio::test]
async fn agent_activity_summaries_unchanged_history_does_not_resummarize() {
    let mut fixture = Fixture::new().await;
    set_activity_summaries(&mut fixture.client, true).await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("summary-unchanged".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "summarize unchanged history".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn unchanged summary agent failed");

    let env = expect_next_event(&mut fixture.client, "unchanged-summary NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("parse unchanged-summary NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "unchanged-summary AgentStart",
    )
    .await;

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: summarize unchanged history",
    )
    .await;
    let _pending = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "unchanged Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;
    let _fresh = expect_activity_summary_matching(
        &mut fixture.client,
        &new_agent.agent_id,
        "unchanged Fresh",
        |state| matches!(state, AgentActivitySummaryState::Fresh { .. }),
    )
    .await;

    expect_no_event(
        &mut fixture.client,
        Duration::from_secs(7),
        "second activity summary for unchanged history",
    )
    .await;
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn for rename test failed");

    let env = expect_next_event(&mut fixture.client, "rename test NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse rename NewAgent");
    let agent_stream = new_agent.instance_stream.clone();

    let start =
        expect_agent_start_on_stream(&mut fixture.client, &agent_stream, "rename test AgentStart")
            .await;
    assert_eq!(start.name, "Original Name");

    expect_turn(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: hello",
    )
    .await;

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

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_agent = bootstrapped_agent(&bootstrap, &new_agent.agent_id);
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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
        let env = expect_next_event(&mut fixture.client, "multiple agent events").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::SessionSettings
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionSchemas
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::BackendConfigSchemas
                | FrameKind::BackendConfigSnapshots
                | FrameKind::QueuedMessages
                | FrameKind::SessionList
                | FrameKind::AgentActivityStats
                | FrameKind::TaskTokenUsage
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
            .filter(|e| {
                e.stream.0 == *stream
                    && e.kind != FrameKind::NewAgent
                    && e.kind != FrameKind::AgentActivityStats
            })
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
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
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

    // Client 1: live stream remains granular.
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
    let env = expect_chat_event(&mut fixture.client, "StreamStart for client 1").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamStart for client 1");
    assert!(matches!(event, ChatEvent::StreamStart(..)));
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
    let env = expect_chat_event(&mut fixture.client, "StreamEnd for client 1").await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    assert_eq!(env.stream, client1_instance_stream);
    let event: ChatEvent = env
        .parse_payload()
        .expect("failed to parse StreamEnd for client 1");
    assert!(matches!(event, ChatEvent::StreamEnd(..)));
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
    // Client 2 connects late and should receive NewAgent + full replay on its own instance stream.
    let (mut client2, bootstrap) = fixture.connect_with_bootstrap().await;
    let client2_new_agent = bootstrapped_agent(&bootstrap, &agent_id);
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

    expect_replayed_turn_on_stream(
        &mut client2,
        &client2_instance_stream,
        &agent_id,
        "mock backend response to: late join replay",
    )
    .await;
}

#[tokio::test]
async fn project_mutations_fan_out_and_delete() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Tyde".to_owned(),
            roots: vec![ProjectRootPath("/tmp/tyde".to_owned())],
        })
        .await
        .expect("project_create failed");

    let created = match expect_project_notify(&mut fixture.client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    };
    assert_eq!(created.name, "Tyde");
    assert_eq!(project_roots(&created), vec!["/tmp/tyde".to_owned()]);

    let (mut client2, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrap
        .projects
        .first()
        .cloned()
        .expect("project missing from HostBootstrap");
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
                assert_eq!(project_roots(&project), vec!["/tmp/tyde".to_owned()]);
            }
            other => panic!("expected renamed project notification, got {other:?}"),
        }
    }

    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: created.id.clone(),
            root: ProjectRootPath("/tmp/tyde-extra".to_owned()),
        })
        .await
        .expect("project_add_root failed");

    for client in [&mut fixture.client, &mut client2] {
        match expect_project_notify(client, "project add root").await {
            ProjectNotifyPayload::Upsert { project } => {
                assert_eq!(project.id, created.id);
                assert_eq!(
                    project_roots(&project),
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
                    project_roots(&project),
                    vec!["/tmp/tyde".to_owned(), "/tmp/tyde-extra".to_owned()]
                );
            }
            other => panic!("expected deleted project notification, got {other:?}"),
        }
    }

    let (mut client3, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.projects.is_empty(),
        "deleted project should not replay to new clients"
    );
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
                workspace_roots: project_roots(&project),
                prompt: "hello from project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent with project failed");

    let env = expect_next_event(&mut fixture.client, "project new agent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse project NewAgent");
    assert_eq!(new_agent.project_id.as_ref(), Some(&project.id));

    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "project agent start",
    )
    .await;
    assert_eq!(start.project_id.as_ref(), Some(&project.id));

    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: hello from project",
    )
    .await;

    let (mut client2, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_projects = bootstrap.projects.clone();
    assert_eq!(replayed_projects, vec![project.clone(), sibling]);

    let replayed_agent = bootstrapped_agent(&bootstrap, &new_agent.agent_id);
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

    let (mut fresh_client, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;

    assert_eq!(bootstrap.projects, vec![project_a, project_b]);
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "fresh host should replay exactly the persisted projects",
    )
    .await;
}

#[tokio::test]
async fn project_delete_detaches_sessions_that_reference_it() {
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
                workspace_roots: project_roots(&project),
                prompt: "hold project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn delete guard agent failed");

    let env = expect_next_event(&mut fixture.client, "delete guard NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse delete guard NewAgent");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "delete guard AgentStart",
    )
    .await;
    expect_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: hold project",
    )
    .await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "session list after delete guard spawn",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse spawn SessionList");
    let session_id = list
        .sessions
        .iter()
        .find(|session| session.project_id.as_ref() == Some(&project.id))
        .map(|session| session.id.clone())
        .expect("spawned session should reference project before delete");

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: project.id.clone(),
        })
        .await
        .expect("project_delete failed");

    let env = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "project delete notify",
    )
    .await;
    match env
        .parse_payload::<ProjectNotifyPayload>()
        .expect("parse project delete notify")
    {
        ProjectNotifyPayload::Delete { project: deleted } => assert_eq!(deleted.id, project.id),
        other => panic!("expected project delete notification, got {other:?}"),
    }
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "session list after project delete",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse SessionList");
    let session = list
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("detached session should remain in list");
    assert_eq!(session.project_id, None);

    let (_fresh_client, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap.projects.is_empty(),
        "deleted project should not replay"
    );
    let session = bootstrap
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("detached session should replay");
    assert_eq!(session.project_id, None);
}

#[tokio::test]
async fn invalid_project_input_surfaces_command_error_and_keeps_connection_alive() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Invalid".to_owned(),
            roots: vec![
                ProjectRootPath("/tmp/dup".to_owned()),
                ProjectRootPath("/tmp/dup".to_owned()),
            ],
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

    let (mut fresh_client, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap.projects.is_empty(),
        "invalid project_create should not persist any project"
    );
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "invalid project_create should not persist any project",
    )
    .await;
}

#[tokio::test]
async fn spawn_with_missing_project_id_emits_terminal_agent_error() {
    let mut fixture = Fixture::new().await;
    let missing_project_id = ProjectId("11111111-1111-1111-1111-111111111111".to_owned());

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("missing-project-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(missing_project_id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/missing-project".to_owned()],
                prompt: "hello".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent write failed");

    let env = expect_next_event(&mut fixture.client, "missing project NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
    assert_eq!(new_agent.name, "missing-project-agent");
    assert_eq!(new_agent.project_id, None);
    let agent_stream = new_agent.instance_stream.clone();

    let env = expect_next_event(&mut fixture.client, "missing project AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, agent_stream);
    let start: AgentStartPayload = env.parse_payload().expect("parse AgentStartPayload");
    assert_eq!(start.project_id, None);

    let err = expect_agent_error_containing(
        &mut fixture.client,
        &agent_stream,
        &format!("cannot spawn agent in missing project {missing_project_id}"),
        "missing project AgentError",
    )
    .await;
    assert!(err.fatal, "missing project should terminate the agent");
    assert_eq!(err.code, protocol::AgentErrorCode::BackendFailed);
    assert!(
        err.message.contains(&format!(
            "cannot spawn agent in missing project {missing_project_id}"
        )),
        "unexpected missing project error: {}",
        err.message
    );

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(150),
        "connection should stay open after missing-project spawn rejection",
    )
    .await;

    let (mut fresh_client, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap.projects.is_empty(),
        "missing-project spawn should not persist any project state"
    );
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
            roots: roots.into_iter().map(ProjectRootPath).collect(),
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create helper").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

fn project_roots(project: &Project) -> Vec<String> {
    project
        .root_paths()
        .into_iter()
        .map(|root| root.0)
        .collect()
}
