mod fixture;

use fixture::{Fixture, expect_settings_write_applied};
use protocol::types::AgentClosedPayload;
use protocol::{
    AgentActivitySummaryPayload, AgentActivitySummaryState, AgentBootstrapEvent,
    AgentBootstrapPayload, AgentControlStatus, AgentErrorCode, AgentErrorPayload, AgentOrigin,
    AgentRenamedPayload, AgentStartPayload, BackendConfigSnapshotsPayload, BackendKind, ChatEvent,
    ClientErrorCode, ClientErrorPayload, CommandErrorCode, CommandErrorPayload, Envelope,
    FetchSessionHistoryPayload, FrameKind, LaunchProfileCatalogPayload, LaunchProfileId,
    LaunchProfileKind, ListSessionsPayload, MessageMetadataUpdateData, MessageSender,
    MessageTokenUsage, NewAgentPayload, OrchestrationAgentOrigin, OrchestrationPayload, Project,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectId,
    ProjectNotifyPayload, ProjectRenamePayload, ProjectRootPath, SendMessagePayload,
    SendMessageToolResponse, SessionHistoryPayload, SessionId, SessionListPayload,
    SessionSettingValue, SessionSettingsValues, SpawnAgentParams, SpawnAgentPayload, StreamEndData,
    StreamPath, TaskTokenUsagePayload, TaskTokenUsageScope, TaskTokenUsageStatus,
    TaskTokenUsageUnavailableReason, TokenUsageScope, TokenUsageUnavailableReason,
    ToolExecutionCompletedData, ToolExecutionOutcome, ToolExecutionResult, ToolRequest,
    ToolRequestType, write_envelope,
};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientRequest, Meta, NumberOrString,
        ProgressNotificationParam, ProgressToken, RawContent, ServerResult,
    },
    service::{NotificationContext, PeerRequestOptions, RoleClient},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};
use settings_model::HostLaunchProfileConfig;
use settings_model::{HostBootstrapPayload, HostSettingsPayload};
use std::future::{self, Future};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tyde_dev_driver::agent_control::{AgentControlHandle, SpawnRequest};

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = fixture::next_frame_unpacking_agent_bootstrap_on(client, context).await;
        if fixture::is_routine_control_plane_frame(&env)
            || matches!(
                env.kind,
                FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::TeamPresetCatalogNotify
                    | FrameKind::BackendCapacity
                    | FrameKind::BackendConfigSchemas
                    | FrameKind::BackendConfigSnapshots
                    | FrameKind::AgentActivityChanged
                    | FrameKind::SessionList
                    | FrameKind::TaskTokenUsage
                    | FrameKind::WorkflowNotify
                    | FrameKind::ContextCompactionNotify
                    | FrameKind::ContextCompactionCapability
            )
            || env.stream.0.starts_with("/project/")
        {
            continue;
        }
        return env;
    }
}

fn pop_pending_agent_event(
    client: &client::Connection,
    stream: &StreamPath,
    kind: FrameKind,
) -> Option<Envelope> {
    fixture::pop_pending_frame_matching_on(client, |env| env.stream == *stream && env.kind == kind)
}

fn push_pending_agent_event(client: &client::Connection, env: Envelope) {
    if !env.stream.0.starts_with("/agent/") && env.kind != FrameKind::NewAgent {
        return;
    }
    fixture::push_pending_frame_on(client, env);
}

fn push_front_pending_agent_event(client: &client::Connection, env: Envelope) {
    if !env.stream.0.starts_with("/agent/") {
        return;
    }
    fixture::push_front_pending_frame_on(client, env);
}

/// Wait for the first envelope of a specific kind, skipping noise frames.
async fn expect_kind(client: &mut client::Connection, kind: FrameKind, context: &str) -> Envelope {
    fixture::next_logical_frame_matching_on(client, context, |env| env.kind == kind).await
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

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    fixture::assert_no_interesting_frame_on(client, duration, context, |env| {
        matches!(
            env.kind,
            FrameKind::AgentsViewPreferencesNotify
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::BackendCapacity
                | FrameKind::BackendConfigSchemas
                | FrameKind::BackendConfigSnapshots
                | FrameKind::SessionList
                | FrameKind::HostSettings
                | FrameKind::WorkflowNotify
                | FrameKind::AgentActivityStats
                | FrameKind::AgentActivityChanged
                | FrameKind::TaskTokenUsage
                | FrameKind::ContextCompactionNotify
                | FrameKind::ContextCompactionCapability
        ) || env.stream.0.starts_with("/project/")
    })
    .await;
}

async fn set_activity_summaries(client: &mut client::Connection, enabled: bool) {
    let write_id = client
        .replace_setting(
            "/background_agent_features/agent_activity_summaries",
            enabled,
            !enabled,
        )
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
    expect_settings_write_applied(client, &write_id, "activity summary result").await;
}

async fn expect_activity_summary_matching(
    client: &mut client::Connection,
    agent_id: &protocol::AgentId,
    context: &str,
    mut matches_state: impl FnMut(&AgentActivitySummaryState) -> bool,
) -> AgentActivitySummaryPayload {
    loop {
        let env = match client.next_event().await {
            Ok(Some(env)) => env,
            Ok(None) => panic!("connection closed before activity summary {context}"),
            Err(err) => {
                panic!("next_event failed before activity summary {context}: {err:?}")
            }
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
    post_json_with_headers(url, &[], body).await
}

async fn post_json_with_headers(url: &str, headers: &[(&str, &str)], body: &Value) -> Value {
    let (addr, target) = parse_http_url(url);
    let mut stream = TcpStream::connect(addr)
        .await
        .unwrap_or_else(|err| panic!("connect {addr} failed: {err}"));
    let body_bytes = serde_json::to_vec(body).expect("serialize HTTP JSON body");
    let extra_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {addr}\r\n{extra_headers}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

fn mcp_result_is_error(response: &Value) -> bool {
    response
        .get("result")
        .and_then(|result| result.get("isError").or_else(|| result.get("is_error")))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("MCP result missing isError: {response}"))
}

fn mcp_result_text(response: &Value) -> &str {
    response
        .get("result")
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("MCP response missing content text: {response}"))
}

async fn mcp_spawn_agent_as(
    caller: &server::AgentControlMcpCaller,
    arguments: Value,
) -> protocol::AgentId {
    mcp_spawn_agent_with_request(&caller.url, Some(&caller.authorization), arguments).await
}

/// Calls `tyde_spawn_agent` expecting a refusal, returning the error text.
///
/// The counterpart to [`mcp_spawn_agent_as`], which asserts success. A refused
/// spawn has to explain itself to the calling agent, so the message is the
/// value under test, not just the failure.
async fn mcp_spawn_agent_error_as(
    caller: &server::AgentControlMcpCaller,
    arguments: Value,
) -> String {
    let response = post_json_with_headers(
        &caller.url,
        &[("Authorization", caller.authorization.as_str())],
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
    assert!(
        mcp_result_is_error(&response),
        "expected the spawn to be refused, got: {response}"
    );
    mcp_result_text(&response).to_owned()
}

async fn mcp_spawn_agent_with_request(
    url: &str,
    authorization: Option<&str>,
    arguments: Value,
) -> protocol::AgentId {
    let headers = authorization
        .map(|value| vec![("Authorization", value)])
        .unwrap_or_default();
    let response = post_json_with_headers(
        url,
        &headers,
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

async fn mcp_await_agent(
    caller: &server::AgentControlMcpCaller,
    agent_id: &protocol::AgentId,
) -> Value {
    let response = post_json_with_headers(
        &caller.await_url,
        &[("Authorization", &caller.authorization)],
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

async fn mcp_list_agents(caller: &server::AgentControlMcpCaller) -> Value {
    let response = post_json_with_headers(
        &caller.url,
        &[("Authorization", &caller.authorization)],
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

async fn mcp_tool_call_as(
    caller: &server::AgentControlMcpCaller,
    await_surface: bool,
    tool_name: &str,
    arguments: Value,
) -> Value {
    let url = if await_surface {
        &caller.await_url
    } else {
        &caller.url
    };
    post_json_with_headers(
        url,
        &[("Authorization", &caller.authorization)],
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments
            }
        }),
    )
    .await
}

fn mcp_success_json(response: &Value) -> Value {
    assert!(
        !mcp_result_is_error(response),
        "MCP tool call failed: {response}"
    );
    serde_json::from_str(mcp_result_text(response)).expect("parse MCP success payload JSON")
}

async fn mcp_list_launch_options(caller: &server::AgentControlMcpCaller) -> Value {
    let response = mcp_tool_call_as(caller, false, "tyde_list_launch_options", json!({})).await;
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
        acp: None,
    }
}

async fn wait_for_ready_launch_profile(
    client: &mut client::Connection,
    profile_id: &str,
) -> LaunchProfileCatalogPayload {
    loop {
        let env = match client.next_event().await {
            Ok(Some(env)) => env,
            Ok(None) => panic!("connection closed before launch profile {profile_id} ready"),
            Err(err) => panic!("next_event failed before launch profile {profile_id}: {err:?}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }

        match env.kind {
            FrameKind::BackendConfigSchemas => {
                let _: protocol::BackendConfigSchemasPayload =
                    env.parse_payload().expect("BackendConfigSchemas payload");
            }
            FrameKind::BackendCapacity => {
                let _: protocol::BackendCapacityPayload =
                    env.parse_payload().expect("BackendCapacity payload");
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
            | FrameKind::SettingsWriteResult
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
    let awaited = (control.await_agents(Some(vec![protocol::AgentId(agent_id.to_owned())])))
        .await
        .unwrap_or_else(|err| panic!("agent control await failed for {context}: {err}"));
    assert!(
        awaited.still_thinking.is_empty(),
        "agent control await should observe {context} ready, got {awaited:?}"
    );
    assert_eq!(awaited.ready.len(), 1);
    assert_awaited_agent_idle(&awaited.ready[0]);
    awaited
}

fn assert_await_result_ready(body: &Value, agent_id: &protocol::AgentId, expected_status: &str) {
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
    assert_eq!(
        ready[0].get("status").and_then(Value::as_str),
        Some(expected_status)
    );
}

async fn wait_for_agent_control_status(
    caller: &server::AgentControlMcpCaller,
    agent_id: &protocol::AgentId,
    expected_status: &str,
) {
    loop {
        let agents = mcp_list_agents(caller).await;
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
        .read_agent_debug(
            protocol::AgentId(agent_id.to_string()),
            after_seq,
            None,
            None,
        )
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

const MOCK_TURN_TOKEN_TOTAL: u64 = 1590;
const MOCK_NATIVE_CHILD_TOKEN_TOTAL: u64 = 330;

async fn spawn_agent_control_parent(fixture: &mut Fixture, name: &str) -> NewAgentPayload {
    let prompt = format!("initialize {name}");
    // Completed-turn bootstrap omits historical typing transitions. The strict
    // live-turn oracle below needs an attached observer before output begins.
    let gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            name,
            MockScript::one(MockTurn::text_after_gate(
                format!("mock backend response to: {prompt}"),
                &gate,
            )),
        )
        .await;
    let agent = fixture.spawn(name, &prompt).await;
    drop(reservation);
    eprintln!("AGENT CONTROL FIXTURE attached before releasing initial turn");
    gate.release_one();
    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        &format!("mock backend response to: initialize {name}"),
    )
    .await;
    agent.new_agent
}

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

fn assert_known_turn_usage_with_unavailable_cumulative(
    usage: &Option<MessageTokenUsage>,
    expected_this_turn_total: u64,
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
    match &usage.cumulative {
        TokenUsageScope::Unavailable {
            reason: TokenUsageUnavailableReason::BackendDidNotReport,
        } => {}
        other => panic!("expected unavailable cumulative token usage, got {other:?}"),
    }
}

async fn expect_task_token_usage_matching(
    client: &mut client::Connection,
    root_agent_id: &protocol::AgentId,
    context: &str,
    mut matches_payload: impl FnMut(&TaskTokenUsagePayload) -> bool,
) -> TaskTokenUsagePayload {
    let mut observed = 0usize;
    loop {
        let env = match client.next_event().await {
            Ok(Some(env)) => env,
            Ok(None) => panic!("connection closed before task token usage {context}"),
            Err(err) => panic!("next_event failed before task token usage {context}: {err:?}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let _ = fixture::buffer_agent_bootstrap_on(client, &env);
            continue;
        }
        if env.kind != FrameKind::TaskTokenUsage {
            continue;
        }
        let payload: TaskTokenUsagePayload = env.parse_payload().expect("TaskTokenUsage");
        if &payload.root_agent_id == root_agent_id {
            observed += 1;
            eprintln!("Task token usage wait observed-count={observed}");
        }
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

fn assert_partial_task_scope(
    scope: &TaskTokenUsageScope,
    expected_total: u64,
    expected_unavailable_count: u32,
) {
    match scope {
        TaskTokenUsageScope::Partial {
            usage,
            unavailable_count,
            reasons,
        } => {
            assert_eq!(usage.total_tokens, expected_total);
            assert_eq!(*unavailable_count, expected_unavailable_count);
            assert_eq!(
                reasons,
                &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            );
        }
        other => panic!("expected partial task token usage, got {other:?}"),
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
    spawn_token_usage_agent_with_backend(client, name, prompt, BackendKind::Claude).await
}

async fn spawn_token_usage_agent_with_backend(
    client: &mut client::Connection,
    name: &str,
    prompt: &str,
    backend_kind: BackendKind,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![format!("/tmp/{name}")],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind,
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
    let agent = fixture
        .spawn_scripted(
            "mock-orchestration-agent",
            MockScript::one(MockTurn::orchestration_started()),
        )
        .await;

    let event = fixture
        .next_chat_event_matching(&agent, "mock orchestration ChatEvent", |_| true)
        .await;
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

async fn expect_strict_mock_turn(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) -> StreamEndData {
    let turn = fixture::finish_turn_on(client, stream).await;
    let mut events = turn.chat_events().into_iter();
    let typing = events.next();
    assert!(
        matches!(typing, Some(ChatEvent::TypingStatusChanged(true))),
        "expected TypingStatusChanged(true) on {stream}, got {typing:?}"
    );
    let start = events.next();
    assert!(
        matches!(start, Some(ChatEvent::StreamStart(..))),
        "expected StreamStart on {stream}, got {start:?}"
    );
    match events.next() {
        Some(ChatEvent::StreamDelta(delta)) => assert!(
            delta.text.contains(expected_text),
            "unexpected delta text on {stream}: {}",
            delta.text
        ),
        other => panic!("expected StreamDelta on {stream}, got {other:?}"),
    }
    let end = match events.next() {
        Some(ChatEvent::StreamEnd(end)) => {
            assert!(
                end.message.content.contains(expected_text),
                "unexpected stream end content on {stream}: {}",
                end.message.content
            );
            end
        }
        other => panic!("expected StreamEnd on {stream}, got {other:?}"),
    };
    let idle = events.next();
    assert!(
        matches!(idle, Some(ChatEvent::TypingStatusChanged(false))),
        "expected TypingStatusChanged(false) on {stream}, got {idle:?}"
    );
    let rest = events.collect::<Vec<_>>();
    assert!(
        rest.is_empty(),
        "unexpected trailing chat events in turn on {stream}: {rest:?}"
    );
    end
}

async fn expect_metadata_update_and_task_token_usage_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    root_agent_id: &protocol::AgentId,
    expected_total: u64,
    context: &str,
) -> (MessageMetadataUpdateData, TaskTokenUsagePayload) {
    let mut update = None;
    let mut token_usage = None;
    let mut typing_finished = false;
    loop {
        if typing_finished && let Some(found_update) = update.take() {
            if let Some(found_usage) = token_usage.take() {
                return (found_update, found_usage);
            }
            update = Some(found_update);
        }
        let env = match client.next_event().await {
            Ok(Some(env)) => env,
            Ok(None) => panic!("connection closed before {context}"),
            Err(err) => panic!("next_event failed before {context}: {err:?}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let _ = fixture::buffer_agent_bootstrap_on(client, &env);
            continue;
        }
        match env.kind {
            FrameKind::ChatEvent if env.stream == *stream => {
                let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
                match event {
                    ChatEvent::MessageMetadataUpdated(metadata) => update = Some(metadata),
                    ChatEvent::TypingStatusChanged(false) => typing_finished = true,
                    other => push_pending_agent_event(
                        client,
                        protocol::Envelope::from_payload(
                            stream.clone(),
                            FrameKind::ChatEvent,
                            env.seq,
                            &other,
                        )
                        .expect("serialize pending ChatEvent"),
                    ),
                }
            }
            FrameKind::ChatEvent => push_pending_agent_event(client, env),
            FrameKind::TaskTokenUsage => {
                let payload: TaskTokenUsagePayload = env.parse_payload().expect("TaskTokenUsage");
                if payload.root_agent_id == *root_agent_id
                    && payload.total.usage.total_tokens == expected_total
                    && matches!(payload.total.status, TaskTokenUsageStatus::Known)
                {
                    token_usage = Some(payload);
                }
            }
            FrameKind::SessionList | FrameKind::AgentActivityStats => {}
            _ => {}
        }
    }
}

async fn expect_raw_agent_bootstrap_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentBootstrapPayload {
    loop {
        let env = match client.next_event().await {
            Ok(Some(env)) => env,
            Ok(None) => panic!("connection closed before {context}"),
            Err(err) => panic!("next_event failed before {context}: {err:?}"),
        };
        if fixture::is_routine_control_plane_frame(&env)
            || matches!(
                env.kind,
                FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::TeamPresetCatalogNotify
                    | FrameKind::BackendConfigSchemas
                    | FrameKind::BackendConfigSnapshots
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
    loop {
        let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(true)").await;
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        match event {
            ChatEvent::TypingStatusChanged(false) => continue,
            ChatEvent::TypingStatusChanged(true) => break,
            other => panic!("expected TypingStatusChanged(true) on {stream}, got {other:?}"),
        }
    }

    expect_live_turn_after_typing_true_on_stream(client, stream, expected_text).await;
}

async fn expect_native_child_prompt_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    expected_prompt: &str,
    parent: &NewAgentPayload,
) {
    let env = expect_chat_event_on_stream(client, stream, "native child prompt").await;
    let event: ChatEvent = env.parse_payload().expect("parse native child prompt");
    assert!(
        matches!(
            event,
            ChatEvent::MessageAdded(ref message)
                if matches!(&message.sender, MessageSender::Agent { agent } if agent == &parent.name)
                    && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), .. }) if id == &parent.agent_id)
                    && message.content == expected_prompt
        ),
        "native child input must preserve the task and identify its parent, never the human"
    );
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
            if drain_pending_agent_control_child_initial_turn_trailer(client, stream, expected_text)
            {
                expect_typing_false_on_stream(client, stream).await;
            }
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
                if drain_pending_agent_control_child_initial_turn_trailer(
                    client,
                    stream,
                    expected_text,
                ) {
                    expect_typing_false_on_stream(client, stream).await;
                }
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

/// Consumes the initial turn's buffered trailer, reporting whether the replay
/// ended while the agent was still marked as typing.
///
/// An `AgentBootstrap` snapshot taken between the turn's `StreamEnd` and its
/// `TypingStatusChanged(false)` truthfully replays "still typing" — the turn had
/// not ended yet — because replayed history strips typing events while the
/// active-stream tail re-appends `TypingStatusChanged(true)`. Left queued, that
/// marker outlives the turn it belongs to and is later mistaken for the *next*
/// turn's `TypingStatusChanged(true)`, so that turn's `StreamStart` assertion
/// instead sees this turn's trailing `TypingStatusChanged(false)`. Draining
/// stops at it so the caller can require the still-owed `TypingStatusChanged`
/// rather than let it leak forward.
///
/// Scans for the stream's `ChatEvent` frames specifically: a bootstrap also
/// queues `AgentActivityStats`/`SessionSettings`/`QueuedMessages` ahead of the
/// trailer, and bailing at the first non-chat frame left the trailer buried.
fn drain_pending_agent_control_child_initial_turn_trailer(
    client: &client::Connection,
    stream: &StreamPath,
    expected_text: &str,
) -> bool {
    while let Some(env) = pop_pending_agent_event(client, stream, FrameKind::ChatEvent) {
        let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
        if matches!(event, ChatEvent::TypingStatusChanged(true)) {
            return true;
        }
        if is_agent_control_child_initial_turn_trailer(&event, expected_text) {
            continue;
        }
        push_front_pending_agent_event(client, env);
        return false;
    }
    false
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

async fn expect_typing_false_on_stream(client: &mut client::Connection, stream: &StreamPath) {
    let env = expect_chat_event_on_stream(client, stream, "TypingStatusChanged(false)").await;
    let event: ChatEvent = env.parse_payload().expect("failed to parse ChatEvent");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
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
            && fixture::tool_request_name(&request) == expected_tool_name
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
) -> StreamEndData {
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
            return end;
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
                request_id: protocol::HistoryPageRequestId(uuid::Uuid::new_v4().to_string()),
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
        if let Some(env) = pop_pending_agent_event(client, stream, FrameKind::ChatEvent) {
            return env;
        }
        let env = expect_chat_event(client, context).await;
        if env.kind == FrameKind::ChatEvent && env.stream == *stream {
            return env;
        }
        push_pending_agent_event(client, env);
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
        eprintln!("EXIT PLAN PAUSE stream={stream} saw_pause={saw_pause} event={event:?}");
        match event {
            ChatEvent::ToolRequest(request) => {
                assert_eq!(fixture::tool_request_name(&request), "ExitPlanMode");
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
        if let Some(env) = pop_pending_agent_event(client, stream, FrameKind::AgentError) {
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
        if let Some(env) = pop_pending_agent_event(client, stream, FrameKind::AgentError) {
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
        if let Some(env) = pop_pending_agent_event(client, stream, FrameKind::AgentError) {
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
                if fixture::is_routine_control_plane_frame(&env)
                    || matches!(
                        env.kind,
                        FrameKind::HostSettings
                            | FrameKind::AgentsViewPreferencesNotify
                            | FrameKind::TeamPresetCatalogNotify
                            | FrameKind::BackendConfigSchemas
                            | FrameKind::BackendConfigSnapshots
                            | FrameKind::SessionList
                            | FrameKind::TaskTokenUsage
                            | FrameKind::WorkflowNotify
                            | FrameKind::ContextCompactionNotify
                            | FrameKind::ContextCompactionCapability
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
            push_pending_agent_event(client, env);
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

async fn spawn_named_session(
    fixture: &mut Fixture,
    name: &str,
    backend_kind: BackendKind,
) -> (NewAgentPayload, SessionId) {
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: format!("create session for {name}"),
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn named session");
    let env = expect_kind(&mut fixture.client, FrameKind::NewAgent, "named NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse named NewAgent");
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "named AgentStart",
    )
    .await;
    expect_strict_mock_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        &format!("mock backend response to: create session for {name}"),
    )
    .await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list named session");
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "named SessionList",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse named SessionList");
    let session_id = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some(name))
        .expect("named session missing")
        .id
        .clone();
    (new_agent, session_id)
}

async fn expect_agent_start_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> AgentStartPayload {
    loop {
        if let Some(env) = pop_pending_agent_event(client, stream, FrameKind::AgentStart) {
            return env.parse_payload().expect("parse AgentStart");
        }
        let env = expect_next_event(client, context).await;
        if env.stream == *stream && env.kind == FrameKind::AgentError {
            let error: AgentErrorPayload = env.parse_payload().expect("agent startup error");
            assert!(
                !error.fatal,
                "agent startup failed with code {:?}",
                error.code
            );
        }
        if env.kind != FrameKind::AgentStart || env.stream != *stream {
            push_pending_agent_event(client, env);
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
    spawn_child_with_backend(
        client,
        parent_agent_id,
        name,
        prompt,
        workspace_root,
        BackendKind::Claude,
    )
    .await
}

async fn spawn_child_with_backend(
    client: &mut client::Connection,
    parent_agent_id: &protocol::AgentId,
    name: &str,
    prompt: &str,
    workspace_root: &str,
    backend_kind: BackendKind,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root.to_owned()],
                prompt: prompt.to_owned(),
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .unwrap_or_else(|error| panic!("spawn {name} failed: {error:?}"));

    let child_new = expect_new_agent(client, &format!("{name} NewAgent")).await;
    assert_eq!(child_new.parent_agent_id.as_ref(), Some(parent_agent_id));

    let child_start =
        expect_agent_start_on_stream(client, &child_new.instance_stream, &format!("{name} start"))
            .await;
    assert_eq!(child_start.origin, AgentOrigin::User);
    assert_eq!(child_start.parent_agent_id.as_ref(), Some(parent_agent_id));

    // A completed initial turn replays StreamStart without historical typing
    // markers. Use the bootstrap-aware oracle already used for MCP children.
    expect_agent_control_child_initial_turn_on_stream(
        client,
        &child_new.instance_stream,
        &format!("mock backend response to: {prompt}"),
    )
    .await;

    child_new
}

async fn spawn_parent_with_native_child(
    fixture: &mut Fixture,
    with_grandchild: bool,
) -> (
    NewAgentPayload,
    AgentStartPayload,
    NewAgentPayload,
    AgentStartPayload,
) {
    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-with-native-child",
            MockScript::one(if with_grandchild {
                MockTurn::text("mock backend response to: parent prompt")
                    .with_native_child_and_grandchild(
                        "mock-native-child",
                        "parent prompt",
                        "mock-native-grandchild",
                        "grandchild task",
                    )
            } else {
                MockTurn::text("mock backend response to: parent prompt")
                    .with_native_child("mock-native-child", "parent prompt")
            }),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("parent-with-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/sub-agent-parent".to_owned()],
                prompt: "parent prompt".to_owned(),
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

    let env = expect_next_event(&mut fixture.client, "parent NewAgent").await;
    assert_eq!(env.kind, FrameKind::NewAgent);
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse parent NewAgent");

    let env = expect_next_event(&mut fixture.client, "parent AgentStart").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
    assert_eq!(env.stream, parent_new.instance_stream);
    let parent_start: AgentStartPayload = env.parse_payload().expect("parse parent AgentStart");
    drop(reservation);

    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;

    let child_new = expect_new_agent(&mut fixture.client, "native child NewAgent").await;

    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "native child AgentStart",
    )
    .await;

    expect_native_child_prompt_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "parent prompt",
        &parent_new,
    )
    .await;
    expect_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "mock native child response to: parent prompt",
    )
    .await;

    (parent_new, parent_start, child_new, child_start)
}

#[tokio::test]
async fn turn_token_usage_is_known_cumulative_and_bootstrapped() {
    // AGY-shaped overlap uses the resumable Messages-mode fixture for durability.
    let mut fixture = Fixture::new().await;
    let start = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "token-usage-agent",
            MockScript::one(MockTurn::overlapping_usage(&start)).with_unbounded_echo(),
        )
        .await;
    let agent = spawn_token_usage_agent_with_backend(
        &mut fixture.client,
        "token-usage-agent",
        "first usage",
        BackendKind::Claude,
    )
    .await;
    drop(reservation);
    start.release_one();
    let generation = fixture::finish_turn_on(&mut fixture.client, &agent.instance_stream).await;
    let events = generation.chat_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ChatEvent::TypingStatusChanged(true)))
            .count(),
        1
    );
    let ends = events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::StreamEnd(end) => Some(end),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ends.len(), 2, "two responses in one generation");
    let first = ends[0];
    let second = ends[1];
    assert_known_turn_usage(&first.message.token_usage, 16_984, 16_984);
    assert_eq!(
        first
            .message
            .token_usage
            .as_ref()
            .expect("first usage")
            .request
            .known_usage()
            .expect("first request")
            .total_tokens,
        16_984
    );
    assert_known_turn_usage(&second.message.token_usage, 32_168, 32_168);
    let usage = second.message.token_usage.as_ref().expect("response usage");
    assert_eq!(
        usage.request.known_usage().expect("request").total_tokens,
        15_184
    );
    let total = usage.cumulative.known_usage().expect("cumulative");
    assert_eq!(total.input_tokens, 24_045);
    assert_eq!(total.prompt_tokens(), 40_173);
    assert_eq!(total.output_tokens, 8_123);
    assert_eq!(total.cached_prompt_tokens, Some(16_128));
    assert_eq!(total.reasoning_tokens, Some(4_521));
    assert_eq!(total.cache_creation_input_tokens, None);

    eprintln!("Usage accounting diagnostic: overlapping generation reconciled");
    fixture
        .client
        .send_message(&agent.instance_stream, "second usage".to_owned())
        .await
        .expect("send second generation");
    let generation = fixture::finish_turn_on(&mut fixture.client, &agent.instance_stream).await;
    let ends = generation
        .chat_events()
        .into_iter()
        .filter_map(|event| match event {
            ChatEvent::StreamEnd(end) => Some(end),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ends.len(), 1, "one response in the next generation");
    let next = &ends[0];
    assert_known_turn_usage(
        &next.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
        32_168 + MOCK_TURN_TOKEN_TOTAL,
    );
    let expected = next
        .message
        .token_usage
        .as_ref()
        .expect("next usage")
        .cumulative
        .known_usage()
        .expect("next cumulative")
        .clone();
    eprintln!("Usage accounting diagnostic: next generation reconciled");
    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let late_agent = bootstrapped_agent(&bootstrap, &agent.agent_id);
    eprintln!(
        "Usage reconnect diagnostic: subscriber_stream_changed={}",
        late_agent.instance_stream != agent.instance_stream
    );
    let task = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == agent.agent_id)
        .expect("task usage");
    assert_known_task_status(&task.total.status);
    assert_known_task_scope(&task.self_usage, expected.total_tokens);
    assert_eq!(
        task.total.usage.prompt_tokens,
        Some(expected.prompt_tokens())
    );
    assert_eq!(task.backends.len(), 1);
    assert_eq!(task.backends[0].backend_kind, BackendKind::Claude);
    assert_eq!(task.backends[0].total, task.total);
    assert_eq!(task.breakdown[0].model.as_deref(), Some("mock"));
    let session = task.root_session_id.clone().expect("usage session");
    let env = fixture::next_frame_matching_on(&mut late_client, "usage replay", |env| {
        env.kind == FrameKind::AgentBootstrap && env.stream == late_agent.instance_stream
    })
    .await;
    let replay: AgentBootstrapPayload = env.parse_payload().expect("usage replay");
    let stats = replay
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::AgentActivityStats(payload) => Some(&payload.stats),
            _ => None,
        })
        .expect("replayed stats");
    assert_eq!(stats.token_usage, expected);
    // Bootstrap folds metadata onto its response rather than replaying raw updates.
    let replayed_first = replay
        .events
        .iter()
        .find_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end))
                if end.message.message_id == first.message.message_id =>
            {
                Some(&end.message)
            }
            _ => None,
        })
        .expect("first response with folded late metadata");
    assert_known_turn_usage(&replayed_first.token_usage, 16_984, 32_168);
    assert_eq!(
        replayed_first
            .token_usage
            .as_ref()
            .expect("replayed usage")
            .request
            .known_usage()
            .expect("replayed request")
            .total_tokens,
        16_984
    );

    eprintln!("Usage accounting diagnostic: reconnect replay verified");
    fixture
        .client
        .close_agent(&agent.instance_stream)
        .await
        .expect("close usage agent");
    expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "usage agent closed",
    )
    .await;
    eprintln!("Usage accounting diagnostic: actor closed");
    let (_, closed) = fixture.connect_with_bootstrap().await;
    // Only live roots are advertised; retained usage becomes observable on resume.
    assert!(
        closed
            .task_token_usages
            .iter()
            .all(|usage| usage.root_session_id.as_ref() != Some(&session)),
        "closed task roots must not stay advertised"
    );
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("usage-reopened".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("reopen saved usage");
    let reopened = expect_new_agent(&mut fixture.client, "reopened usage agent").await;
    expect_agent_start_on_stream(
        &mut fixture.client,
        &reopened.instance_stream,
        "reopened usage ready",
    )
    .await;
    eprintln!("Usage accounting diagnostic: saved session reopened");
    let (_, reopened_usage) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        reopened_usage
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == reopened.agent_id)
            .expect("retained usage on reopen")
            .total,
        task.total
    );
    let restart = fixture.restart_host().await;
    let restored = fixture
        .restored_agents(&restart, std::slice::from_ref(&session))
        .await;
    let mut active = restored[&session].clone();
    eprintln!("Usage resume diagnostic: phase=restored_bootstrap_begin");
    let (_, ready) = fixture.connect_with_bootstrap().await;
    eprintln!("Usage resume diagnostic: phase=restored_bootstrap_received");
    let restored = ready
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored[&session].agent_id)
        .expect("restored usage");
    assert_eq!(restored.total, task.total);
    assert_eq!(restored.backends.len(), 1);
    assert_eq!(restored.backends[0].backend_kind, BackendKind::Claude);
    assert_eq!(restored.backends[0].total, task.backends[0].total);
    assert_eq!(restored.breakdown[0].model, task.breakdown[0].model);

    eprintln!("Usage resume diagnostic: phase=restored_usage_verified");
    let write_id = protocol::SettingsWriteId("accounting-manual-resume".to_owned());
    fixture
        .client
        .settings_write(protocol::SettingsWritePayload {
            write_id: write_id.clone(),
            ops: vec![protocol::SettingOp::Replace {
                path: "/resume_previous_agents".to_owned(),
                value: json!(false),
                expected: protocol::SettingExpectation::Value { value: json!(true) },
            }],
        })
        .await
        .expect("disable automatic restoration for manual resume");
    eprintln!("Usage resume diagnostic: phase=manual_restoration_write_sent");
    expect_settings_write_applied(&mut fixture.client, &write_id, "manual restoration saved").await;
    eprintln!("Usage resume diagnostic: phase=manual_restoration_write_applied");
    let mut expected_total = expected.total_tokens;
    let resume_cases = [(false, false), (true, false), (true, true)];
    for (index, (prompt_on_resume, torn_tail)) in resume_cases.into_iter().enumerate() {
        let gate = MockGateHandle::new();
        let held_text = format!("measured interrupted usage {index}");
        eprintln!("Usage resume diagnostic: case={index} phase=mock_control_begin");
        let control = fixture.mock_by_id(&active.agent_id).await;
        eprintln!("Usage resume diagnostic: case={index} phase=mock_control_received");
        control
            .enqueue(MockTurn::gated_text(held_text.clone(), &gate))
            .await;
        eprintln!("Usage resume diagnostic: case={index} phase=measured_turn_enqueued");
        fixture
            .client
            .send_message(&active.instance_stream, "measured before kill".to_owned())
            .await
            .expect("start measured generation before kill");
        eprintln!("Usage resume diagnostic: case={index} phase=measured_prompt_sent");
        let mut measured_events = 0;
        let measured = fixture::next_chat_event_matching_on(
            &mut fixture.client,
            &active.instance_stream,
            "measured response before idle",
            |event| {
                if measured_events < 16 {
                    eprintln!(
                        "Usage resume diagnostic: case={index} phase=measured_event ordinal={measured_events} event={} scripted_response={}",
                        match event {
                            ChatEvent::TypingStatusChanged(true) => "working",
                            ChatEvent::TypingStatusChanged(false) => "idle",
                            ChatEvent::StreamStart(_) => "response_start",
                            ChatEvent::StreamDelta(_) => "response_delta",
                            ChatEvent::StreamEnd(_) => "response_end",
                            _ => "other",
                        },
                        matches!(event, ChatEvent::StreamEnd(end) if end.message.content == held_text),
                    );
                }
                measured_events += 1;
                matches!(event, ChatEvent::StreamEnd(end) if end.message.content == held_text)
            },
        )
        .await;
        let ChatEvent::StreamEnd(measured) = measured else {
            unreachable!("matched measured response")
        };
        assert_known_turn_usage(
            &measured.message.token_usage,
            MOCK_TURN_TOKEN_TOTAL,
            expected_total + MOCK_TURN_TOKEN_TOTAL,
        );
        eprintln!("Usage resume diagnostic: case={index} phase=measured_usage_verified");
        gate.wait_until_entered().await;
        eprintln!("Usage resume diagnostic: case={index} phase=measured_gate_entered");
        let killed = fixture.relaunch_host_after_kill().await;
        eprintln!("Usage resume diagnostic: case={index} phase=killed_host_relaunched");
        assert!(
            killed.agents.is_empty(),
            "manual restoration remains disabled"
        );
        if torn_tail {
            use std::io::Write;
            let journal_path = fixture
                .store_dir()
                .join("transcripts")
                .join(format!("{}.jsonl", session.0));
            let mut journal = std::fs::OpenOptions::new()
                .append(true)
                .open(journal_path)
                .expect("open inactive fixture journal");
            journal
                .write_all(b"{\"interrupted_record\":")
                .expect("inject an uncommitted fixture tail");
            journal.sync_data().expect("persist fixture tail");
            eprintln!("Usage resume diagnostic: case={index} phase=torn_tail_injected");
        }
        let prompt = format!("accounting after interrupted resume {index}");
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                launch_context: protocol::AgentLaunchContext::Standard,
                name: Some("usage-after-interruption".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::Resume {
                    session_id: session.clone(),
                    prompt: prompt_on_resume.then(|| prompt.clone()),
                },
            })
            .await
            .expect("resume interrupted accounting");
        eprintln!("Usage resume diagnostic: case={index} phase=resume_requested");
        let resumed = expect_new_agent(&mut fixture.client, "interrupted usage resumed").await;
        eprintln!("Usage resume diagnostic: case={index} phase=resumed_agent_announced");
        expect_agent_start_on_stream(
            &mut fixture.client,
            &resumed.instance_stream,
            "interrupted usage start",
        )
        .await;
        eprintln!("Usage resume diagnostic: case={index} phase=resumed_agent_started");
        if !prompt_on_resume {
            fixture
                .client
                .send_message(&resumed.instance_stream, prompt.clone())
                .await
                .expect("prompt after interrupted resume");
        }
        eprintln!("Usage resume diagnostic: case={index} phase=resumed_prompt_submitted");
        let reply = format!("mock backend response to: {prompt}");
        let mut resumed_events = 0;
        let after = fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "new generation after interrupted resume",
            |env| {
                if env.stream != resumed.instance_stream {
                    return false;
                }
                if env.kind == FrameKind::AgentError {
                    let error: AgentErrorPayload = env.parse_payload().expect("resume error");
                    assert!(
                        !error.fatal,
                        "resume must remain usable, error code {:?}",
                        error.code
                    );
                }
                if env.kind != FrameKind::ChatEvent {
                    return false;
                }
                let event: ChatEvent = env.parse_payload().expect("resumed chat event");
                assert!(
                    !prompt_on_resume || !matches!(event, ChatEvent::TypingStatusChanged(false)),
                    "queued resume must not publish idle before its reply"
                );
                let matched = matches!(
                    &event,
                    ChatEvent::StreamEnd(end) if end.message.content.ends_with(&reply)
                );
                if resumed_events < 16 {
                    eprintln!(
                        "Usage resume diagnostic: case={index} phase=resumed_event ordinal={resumed_events} response_end={} echo_suffix_matches={matched}",
                        matches!(event, ChatEvent::StreamEnd(_)),
                    );
                }
                resumed_events += 1;
                matched
            },
        )
        .await;
        let ChatEvent::StreamEnd(after) = after.parse_payload().expect("resumed response") else {
            unreachable!("matched resumed response")
        };
        expected_total += MOCK_TURN_TOKEN_TOTAL * 2;
        assert_known_turn_usage(
            &after.message.token_usage,
            MOCK_TURN_TOKEN_TOTAL,
            expected_total,
        );
        eprintln!("Usage resume diagnostic: case={index} phase=resumed_usage_verified");
        let (_, bootstrap) = fixture.connect_with_bootstrap().await;
        eprintln!("Usage resume diagnostic: case={index} phase=resumed_bootstrap_received");
        let task = bootstrap
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == resumed.agent_id)
            .expect("interrupted generation retained");
        if torn_tail {
            match &task.self_usage {
                TaskTokenUsageScope::Partial {
                    usage,
                    unavailable_count: 1,
                    reasons,
                } => {
                    assert_eq!(usage.total_tokens, expected_total);
                    assert_eq!(
                        reasons,
                        &vec![TaskTokenUsageUnavailableReason::AccountingHistoryIncomplete]
                    );
                }
                _ => panic!("torn history must retain usage without certifying completeness"),
            }
            assert_eq!(
                task.total.status,
                TaskTokenUsageStatus::Partial {
                    unavailable_count: 1,
                    reasons: vec![TaskTokenUsageUnavailableReason::AccountingHistoryIncomplete],
                }
            );
        } else {
            assert_known_task_scope(&task.self_usage, expected_total);
            assert_known_task_status(&task.total.status);
        }
        assert_eq!(task.backends[0].total, task.total);
        let resumed_self = task.self_usage.clone();
        let resumed_total = task.total.clone();
        fixture
            .client
            .close_agent(&resumed.instance_stream)
            .await
            .expect("close resumed usage for durable replay");
        expect_kind(
            &mut fixture.client,
            FrameKind::AgentClosed,
            "resumed usage closed",
        )
        .await;
        eprintln!("Usage resume diagnostic: case={index} phase=resumed_actor_closed");
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                launch_context: protocol::AgentLaunchContext::Standard,
                name: Some("usage-durable-reopen".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::Resume {
                    session_id: session.clone(),
                    prompt: None,
                },
            })
            .await
            .expect("reopen resumed accounting journal");
        let reopened = expect_new_agent(&mut fixture.client, "durable usage reopened").await;
        expect_agent_start_on_stream(
            &mut fixture.client,
            &reopened.instance_stream,
            "durable usage ready",
        )
        .await;
        let (_, durable) = fixture.connect_with_bootstrap().await;
        let task = durable
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == reopened.agent_id)
            .expect("durable resumed usage");
        assert_eq!(task.self_usage, resumed_self);
        assert_eq!(task.total, resumed_total);
        assert_eq!(task.backends.len(), 1);
        assert_eq!(task.backends[0].backend_kind, BackendKind::Claude);
        assert_eq!(task.backends[0].total, task.total);
        assert_eq!(task.breakdown[0].model.as_deref(), Some("mock"));
        eprintln!("Usage resume diagnostic: case={index} phase=durable_usage_verified");
        active = reopened;
    }
    eprintln!(
        "Usage accounting diagnostic: interrupted generations retained across both resume prompt paths"
    );
}

#[tokio::test]
async fn late_metadata_usage_updates_cumulative_without_double_counting() {
    let mut fixture = Fixture::new().await;
    let checkpoint = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "late-token-usage-agent",
            MockScript::one(MockTurn::text_with_late_usage(
                "mock backend response to: first late",
            ))
            .then(MockTurn::text("mock backend response to: after late usage"))
            .then(MockTurn::generation_usage_coverage(&checkpoint))
            .with_unbounded_echo(),
        )
        .await;
    let new_agent =
        spawn_token_usage_agent(&mut fixture.client, "late-token-usage-agent", "first late").await;
    drop(reservation);

    let stream_end = expect_turn_stream_end_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: first late",
    )
    .await;
    assert_unavailable_turn_usage(&stream_end.message.token_usage);

    let (update, late_rollup) = expect_metadata_update_and_task_token_usage_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        &new_agent.agent_id,
        MOCK_TURN_TOKEN_TOTAL,
        "late metadata token usage and task rollup",
    )
    .await;
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
    assert_eq!(late_rollup.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL);
    assert_known_task_status(&late_rollup.total.status);
    assert_known_task_scope(&late_rollup.self_usage, MOCK_TURN_TOKEN_TOTAL);

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

    fixture
        .client
        .send_message(&new_agent.instance_stream, "coverage revisions".to_owned())
        .await
        .expect("start generation coverage");
    let unavailable = expect_stream_end_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "coverage response 1",
    )
    .await;
    assert_unavailable_turn_usage(&unavailable.message.token_usage);
    let covering = expect_stream_end_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "coverage response 2",
    )
    .await;
    assert_known_turn_usage(
        &covering.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL * 2,
        MOCK_TURN_TOKEN_TOTAL * 4,
    );
    checkpoint.wait_until_entered().await;
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    let task = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == new_agent.agent_id)
        .expect("covered unavailable response");
    assert_known_task_status(&task.total.status);
    let optional_fields = vec![
        protocol::TaskTokenUsageField::CachedPrompt,
        protocol::TaskTokenUsageField::CacheCreationInput,
        protocol::TaskTokenUsageField::Reasoning,
    ];
    assert_eq!(task.total.usage.partial_fields, optional_fields);

    let unavailable_id = unavailable
        .message
        .message_id
        .as_ref()
        .expect("response identity");
    checkpoint.release_one();
    fixture::next_logical_frame_matching_on(&mut fixture.client, "covered stale metadata", |env| {
        env.stream == new_agent.instance_stream
            && env.kind == FrameKind::ChatEvent
            && env.parse_payload::<ChatEvent>().is_ok_and(|event| {
                matches!(event, ChatEvent::MessageMetadataUpdated(update)
                    if &update.message_id == unavailable_id
                        && update.token_usage.as_ref().is_some_and(|usage| usage.turn.known_usage().is_none()))
            })
    })
    .await;
    checkpoint.wait_until_entered().await;
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    let task = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == new_agent.agent_id)
        .expect("generation optional coverage upgraded");
    assert_known_task_status(&task.total.status);
    assert_known_task_scope(&task.self_usage, MOCK_TURN_TOKEN_TOTAL * 4);
    assert!(task.total.usage.partial_fields.is_empty());

    checkpoint.release_one();
    let unreported = expect_stream_end_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "coverage response 3",
    )
    .await;
    assert_unavailable_turn_usage(&unreported.message.token_usage);
    let request_only = expect_stream_end_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "coverage response 4",
    )
    .await;
    let request_only = request_only
        .message
        .token_usage
        .expect("request-only usage");
    assert!(request_only.turn.known_usage().is_none());
    assert_eq!(
        request_only
            .request
            .known_usage()
            .expect("request")
            .total_tokens,
        MOCK_TURN_TOKEN_TOTAL
    );
    checkpoint.wait_until_entered().await;
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    let task = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == new_agent.agent_id)
        .expect("uncovered responses retained");
    assert_partial_task_scope(&task.self_usage, MOCK_TURN_TOKEN_TOTAL * 5, 1);
    assert_partial_task_scope(&task.breakdown[0].usage, MOCK_TURN_TOKEN_TOTAL * 5, 1);
    assert!(matches!(
        &task.total.status,
        TaskTokenUsageStatus::Partial { unavailable_count: 1, reasons }
            if reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
    ));
    assert_eq!(task.backends[0].total, task.total);
    assert_eq!(task.total.usage.partial_fields, optional_fields);

    checkpoint.release_one();
    let (update, task) = expect_metadata_update_and_task_token_usage_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        &new_agent.agent_id,
        MOCK_TURN_TOKEN_TOTAL * 6,
        "complete generation coverage",
    )
    .await;
    assert_known_turn_usage(
        &update.token_usage,
        MOCK_TURN_TOKEN_TOTAL * 4,
        MOCK_TURN_TOKEN_TOTAL * 6,
    );
    assert_known_task_scope(&task.self_usage, MOCK_TURN_TOKEN_TOTAL * 6);
    assert!(task.total.usage.partial_fields.is_empty());
    assert_eq!(task.backends[0].total, task.total);
    eprintln!("Usage accounting diagnostic: generation coverage and uncovered sources reconciled");
}

#[tokio::test]
async fn subagent_turn_token_usage_is_strictly_self() {
    let mut fixture = Fixture::new().await;
    let registration = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "strict-self-parent",
            MockScript::one(
                MockTurn::text("mock backend response to: parent prompt")
                    .with_gated_native_child_and_timing_tool(
                        "mock-native-child",
                        "parent prompt",
                        &registration,
                    ),
            ),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("strict-self-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/token-sub-agent-parent".to_owned()],
                prompt: "parent prompt".to_owned(),
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
    drop(reservation);
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

    registration.wait_until_entered().await;
    fixture::next_logical_frame_matching_on(&mut fixture.client, "native spawn request is durably observed", |env| env.stream == parent_new.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolRequest(request) if matches!(request.tool_type, ToolRequestType::AgentSpawn { .. })))).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let commit = fixture.on_next_session_commit(move || {
        entered_tx.send(()).expect("native commit entered");
        release_rx.recv().expect("native commit release");
    });
    registration.release_one();
    entered_rx.await.expect("native session commit pending");
    let (_, preparing) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        preparing.agents.len(),
        1,
        "a native relay must not activate before its session commit"
    );
    release_tx.send(()).expect("release native session commit");
    drop(commit);
    let child_new =
        expect_new_agent(&mut fixture.client, "strict-self native child NewAgent").await;
    expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "strict-self child start",
    )
    .await;
    expect_native_child_prompt_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "parent prompt",
        &parent_new,
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
    fixture::next_logical_frame_matching_on(&mut fixture.client, "native timing tool completion", |env| env.stream == child_new.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == "native-timing-fixture"))).await;
    let task_usage = expect_task_token_usage_matching(
        &mut fixture.client,
        &parent_new.agent_id,
        "native child known task usage",
        |payload| {
            payload.breakdown.iter().any(|agent| {
                agent.agent_id == child_new.agent_id
                    && agent
                        .usage
                        .known_usage()
                        .is_some_and(|usage| usage.total_tokens == MOCK_NATIVE_CHILD_TOKEN_TOTAL)
            })
        },
    )
    .await;
    assert_known_task_status(&task_usage.total.status);
    let (_, settled) = fixture.connect_with_bootstrap().await;
    let timing = &settled
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == parent_new.agent_id)
        .expect("native timing rollup")
        .timing;
    assert_eq!(timing.persistence_failed_agent_count, 0);
    assert!(
        timing
            .spans
            .iter()
            .any(|entry| entry.agent_id == child_new.agent_id
                && entry.span.tool_call_id == "native-timing-fixture"
                && entry.span.state == protocol::TaskTimingState::Completed)
    );
    let parent_session = task_usage
        .root_session_id
        .clone()
        .expect("native parent session");
    let before_total = task_usage.total.usage.total_tokens;
    let restart = fixture.relaunch_host_after_kill().await;
    let restored = fixture
        .restored_agents(&restart, std::slice::from_ref(&parent_session))
        .await;
    let (_, after) = fixture.connect_with_bootstrap().await;
    let usage = after
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored[&parent_session].agent_id)
        .expect("native child accounting after restart");
    assert_eq!(usage.total.usage.total_tokens, before_total);
    assert_eq!(usage.breakdown.len(), 2);
    let child = usage
        .breakdown
        .iter()
        .find(|entry| entry.origin == protocol::AgentOrigin::BackendNative)
        .expect("archived native descriptor");
    assert!(
        matches!(&child.usage, TaskTokenUsageScope::Partial { usage, reasons, .. } if usage.total_tokens == MOCK_NATIVE_CHILD_TOKEN_TOTAL && reasons.contains(&TaskTokenUsageUnavailableReason::ObservationInterrupted))
    );
    assert_eq!(child.navigation, protocol::TaskUsageNavigation::Unavailable);
    assert!(
        usage
            .timing
            .spans
            .iter()
            .any(|entry| entry.agent_id == child.agent_id
                && entry.span.tool_call_id == "native-timing-fixture"
                && entry.span.state == protocol::TaskTimingState::Completed)
    );
    let restored_parent = &restored[&parent_session];
    fixture
        .mock_by_id(&restored_parent.agent_id)
        .await
        .enqueue(
            MockTurn::text("parent opens another native child")
                .with_running_native_child_and_timing(
                    "native-open-before-kill",
                    "open native work",
                ),
        )
        .await;
    fixture
        .client
        .send_message(
            &restored_parent.instance_stream,
            "observe another native child".to_owned(),
        )
        .await
        .expect("start native hard-restart flow");
    expect_completed_turn_on_stream(
        &mut fixture.client,
        &restored_parent.instance_stream,
        "parent opens another native child",
    )
    .await;
    let running_child = expect_new_agent(
        &mut fixture.client,
        "running native child before hard restart",
    )
    .await;
    fixture::next_logical_frame_matching_on(&mut fixture.client, "native open span durable before hard restart", |env| env.stream == running_child.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "native-running-timing"))).await;
    let (_, before_kill) = fixture.connect_with_bootstrap().await;
    let before = before_kill
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored_parent.agent_id)
        .expect("before native hard restart")
        .total
        .usage
        .total_tokens;
    let restart = fixture.relaunch_host_after_kill().await;
    let restored = fixture
        .restored_agents(&restart, std::slice::from_ref(&parent_session))
        .await;
    let (_, after) = fixture.connect_with_bootstrap().await;
    let usage = after
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored[&parent_session].agent_id)
        .expect("native hard restart observations");
    assert_eq!(usage.total.usage.total_tokens, before);
    assert_eq!(usage.breakdown.len(), 3);
    assert!(
        usage
            .timing
            .spans
            .iter()
            .any(|entry| entry.agent_id == running_child.agent_id
                && entry.span.tool_call_id == "native-running-timing"
                && entry.span.state == protocol::TaskTimingState::Interrupted)
    );
    let (_, reconnect) = fixture.connect_with_bootstrap().await;
    let repeated = reconnect
        .task_token_usages
        .iter()
        .find(|item| item.root_agent_id == usage.root_agent_id)
        .expect("native reconnect observations");
    // Reconnect observations differed only by 67ms of the live root's idle
    // horizon. Freezing the whole summary rejects that correct observation;
    // require every other endpoint, total and retained tool to remain exact.
    let mut observed = repeated.timing.clone();
    let interval = observed.interval.as_mut().expect("reconnect interval");
    let earlier = usage.timing.interval.as_ref().expect("earlier interval");
    let idle_growth = interval
        .observed_through_ms
        .checked_sub(earlier.observed_through_ms)
        .expect("reconnect horizon cannot regress");
    interval.observed_through_ms = earlier.observed_through_ms;
    interval.wall_clock.idle_ms = interval
        .wall_clock
        .idle_ms
        .checked_sub(idle_growth)
        .expect("wall-clock growth is idle only");
    interval.agent_time.idle_ms = interval
        .agent_time
        .idle_ms
        .checked_sub(idle_growth)
        .expect("agent-time growth is idle only");
    let root = &mut observed
        .accounts
        .iter_mut()
        .find(|entry| entry.agent_id == usage.root_agent_id)
        .expect("live root account")
        .account;
    root.observed_through_ms = root
        .observed_through_ms
        .checked_sub(idle_growth)
        .expect("root horizon includes the same idle growth");
    root.totals.idle_ms = root
        .totals
        .idle_ms
        .checked_sub(idle_growth)
        .expect("root growth is idle only");
    let phase = root.phases.last_mut().expect("live root phase");
    assert_eq!(phase.phase, protocol::TaskTimePhase::Idle);
    assert_eq!(phase.end, protocol::TaskTimePhaseEnd::Open);
    phase.ended_at_ms = phase
        .ended_at_ms
        .checked_sub(idle_growth)
        .expect("open idle endpoint grows with the horizon");
    assert!(
        observed == usage.timing,
        "reconnect may extend only the live root's idle endpoint; lost native sources and all retained timing remain exact"
    );
}

/// The first turn's backend reports no usage: the turn surfaces as
/// Unavailable, never zero. A later known-usage turn then leaves the task
/// rollup Partial rather than pretending the total is complete.
#[tokio::test]
async fn missing_backend_usage_is_unavailable_not_zero_then_task_usage_stays_partial() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "mixed-token-usage-agent",
            MockScript::one(MockTurn::text_without_usage(
                "mock backend response to: first missing usage",
            ))
            .with_unbounded_echo(),
        )
        .await;

    let first = expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: first missing usage",
    )
    .await;
    assert_unavailable_turn_usage(&first.message.token_usage);
    let (mut usage_client, _) = fixture.connect_with_bootstrap().await;

    fixture
        .client
        .send_message(&agent.stream, "second known usage".to_owned())
        .await
        .expect("send known usage follow-up");
    let second = expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: second known usage",
    )
    .await;
    assert_known_turn_usage_with_unavailable_cumulative(
        &second.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
    );

    let payload = expect_task_token_usage_matching(
        &mut usage_client,
        &agent.new_agent.agent_id,
        "mixed known and missing self usage",
        |payload| {
            matches!(
                &payload.total.status,
                TaskTokenUsageStatus::Partial {
                    unavailable_count: 1,
                    reasons
                } if reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            ) && matches!(
                &payload.self_usage,
                TaskTokenUsageScope::Partial {
                    usage,
                    unavailable_count: 1,
                    reasons
                } if usage.total_tokens == MOCK_TURN_TOKEN_TOTAL
                    && reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
            )
        },
    )
    .await;
    assert_eq!(payload.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL);
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
        other => panic!("expected partial mixed-source aggregate, got {other:?}"),
    }
    assert_partial_task_scope(&payload.self_usage, MOCK_TURN_TOKEN_TOTAL, 1);
    assert_eq!(payload.descendant_count, 0);
    assert_eq!(payload.breakdown.len(), 1);
    assert_partial_task_scope(&payload.breakdown[0].usage, MOCK_TURN_TOKEN_TOTAL, 1);
    fixture
        .mock(&agent)
        .await
        .enqueue(MockTurn::text_without_usage("final unreported turn"))
        .await;
    fixture
        .client
        .send_message(&agent.stream, "last unreported".to_owned())
        .await
        .expect("send final missing-usage turn");
    expect_strict_mock_turn(&mut fixture.client, &agent.stream, "final unreported turn").await;
    let (_, final_live) = fixture.connect_with_bootstrap().await;
    let live = final_live
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == agent.new_agent.agent_id)
        .expect("final live partial own usage");
    assert!(matches!(
        live.total.status,
        TaskTokenUsageStatus::Partial {
            unavailable_count: 2,
            ..
        }
    ));
    let session = live.root_session_id.clone().expect("partial own session");
    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close with final missing usage");
    expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "partial own actor closed",
    )
    .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("partial-own-reopened".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume final missing usage");
    let reopened = expect_new_agent(&mut fixture.client, "partial own resumed").await;
    expect_agent_start_on_stream(
        &mut fixture.client,
        &reopened.instance_stream,
        "partial own ready",
    )
    .await;
    let restart = fixture.restart_host().await;
    let restored = fixture
        .restored_agents(&restart, std::slice::from_ref(&session))
        .await;
    let (_, after) = fixture.connect_with_bootstrap().await;
    let usage = after
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored[&session].agent_id)
        .expect("retained final missing usage");
    assert_eq!(usage.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL);
    assert!(
        matches!(
            usage.total.status,
            TaskTokenUsageStatus::Partial {
                unavailable_count: 2,
                ..
            }
        ),
        "close/restart must not conceal the last unreported turn"
    );
}

#[tokio::test]
async fn task_token_usage_rolls_up_updates_and_retains_closed_descendants() {
    let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
        BackendKind::Claude,
        BackendKind::Grok,
    ])
    .await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "usage-root",
            MockScript::one(MockTurn::text_with_cache_counter(
                "mock backend response to: root usage",
                Some(1),
            ))
            .with_unbounded_echo(),
        )
        .await;
    let root = spawn_token_usage_agent(&mut fixture.client, "usage-root", "root usage").await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root usage",
    )
    .await;
    drop(reservation);
    let (mut usage_client, _) = fixture.connect_with_bootstrap().await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "usage-child",
        "child usage",
        "/tmp/usage-child",
    )
    .await;
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Grok)
        .await;
    let grandchild_script = fixture
        .reserve_next_mock_launch(
            "usage-grandchild",
            MockScript::one(MockTurn::text_with_cache_counter(
                "mock backend response to: grandchild usage",
                None,
            ))
            .with_unbounded_echo(),
        )
        .await;
    let grandchild = spawn_child_with_backend(
        &mut fixture.client,
        &child.agent_id,
        "usage-grandchild",
        "grandchild usage",
        "/tmp/usage-grandchild",
        BackendKind::Grok,
    )
    .await;

    drop(grandchild_script);
    let payload = expect_task_token_usage_matching(
        &mut usage_client,
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
    assert_eq!(payload.backends.len(), 2);
    assert_eq!(payload.backends[0].backend_kind, BackendKind::Claude);
    assert_eq!(payload.backends[0].agent_count, 2);
    assert_eq!(payload.backends[0].reporting_agent_count, 2);
    assert_eq!(payload.backends[0].incomplete_agent_count, 0);
    assert_eq!(
        payload.backends[0].total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 2
    );
    assert_eq!(payload.backends[1].backend_kind, BackendKind::Grok);
    assert_eq!(payload.backends[1].agent_count, 1);
    assert_eq!(
        payload.backends[1].total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL
    );
    assert_eq!(
        payload.total.usage.cached_prompt_tokens,
        Some(801),
        "root reports 1 cached token and child reports 800; the grandchild's missing cache breakdown must not erase them"
    );
    assert!(
        payload
            .total
            .usage
            .partial_fields
            .contains(&protocol::TaskTokenUsageField::CachedPrompt)
    );
    assert_eq!(
        payload.total.usage.prompt_tokens,
        payload
            .total
            .usage
            .input_tokens
            .map(|uncached| uncached + 951),
        "unknown cache breakdowns must not erase known prompt input from another agent"
    );
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

    let host = fixture.host_for_test();
    let snapshot_gate = host.install_next_bootstrap_snapshot_reply_test_gate().await;
    let late_connection = tokio::spawn(fixture::connect_host(host));
    snapshot_gate.wait_until_entered().await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions while an agent snapshot reply is held");
    let sessions = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "host remains responsive during an admitted agent snapshot read",
    )
    .await;
    let _: SessionListPayload = sessions
        .parse_payload()
        .expect("session list during blocked usage read");
    let pending = spawn_token_usage_agent(
        &mut fixture.client,
        "usage-during-bootstrap",
        "pending usage",
    )
    .await;
    expect_completed_turn_on_stream(
        &mut fixture.client,
        &pending.instance_stream,
        "mock backend response to: pending usage",
    )
    .await;
    expect_task_token_usage_matching(
        &mut usage_client,
        &pending.agent_id,
        "pending agent settles while bootstrap is held",
        |usage| usage.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL,
    )
    .await;
    snapshot_gate.release_one();
    let (mut late_client, bootstrap) = late_connection.await.expect("late bootstrap task");
    assert!(
        !bootstrap
            .agents
            .iter()
            .any(|agent| agent.agent_id == pending.agent_id)
    );
    let advertised = expect_new_agent(&mut late_client, "pending NewAgent follows baseline").await;
    assert_eq!(advertised.agent_id, pending.agent_id);
    let pending_usage = expect_task_token_usage_matching(
        &mut late_client,
        &pending.agent_id,
        "settled pending usage follows its advertisement",
        |usage| usage.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL,
    )
    .await;
    let pending_entry = pending_usage
        .breakdown
        .iter()
        .find(|entry| entry.agent_id == pending.agent_id)
        .expect("pending agent usage breakdown");
    assert_eq!(
        pending_entry.navigation,
        protocol::TaskUsageNavigation::LiveAgent,
        "queued canonical usage must become navigable after ordered NewAgent delivery"
    );
    wall_clock_account(&pending_usage, &pending.agent_id);
    let bootstrapped = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == root.agent_id)
        .expect("HostBootstrap should include root task token usage");
    wall_clock_account(bootstrapped, &root.agent_id);
    assert_eq!(
        bootstrapped.total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 3
    );
    assert_eq!(bootstrapped.descendant_count, 2);

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
        &mut usage_client,
        &root.agent_id,
        "updated descendant aggregate",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 4,
    )
    .await;
    assert_known_task_status(&updated.total.status);
    assert_eq!(
        updated.descendant_usage.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 3
    );
    let child_entry = updated
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("updated child breakdown entry");
    assert_known_task_scope(&child_entry.usage, MOCK_TURN_TOKEN_TOTAL * 2);

    let completion_gate = MockGateHandle::new();
    fixture
        .mock_by_id(&grandchild.agent_id)
        .await
        .enqueue(MockTurn::text_completion_after_gate(
            "completion during close preparation",
            &completion_gate,
        ))
        .await;
    fixture
        .client
        .send_message(
            &grandchild.instance_stream,
            "finish while close prepares".to_owned(),
        )
        .await
        .expect("start overlapping completion");
    completion_gate.wait_until_entered().await;
    fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "source running before close preparation",
        |env| {
            env.stream == grandchild.instance_stream
                && env.kind == FrameKind::ChatEvent
                && env
                    .parse_payload::<ChatEvent>()
                    .is_ok_and(|event| matches!(event, ChatEvent::StreamDelta(_)))
        },
    )
    .await;
    let failed_commit = fixture.fail_next_session_commit();
    fixture
        .client
        .close_agent(&grandchild.instance_stream)
        .await
        .expect("request close with failed accounting save");
    let failure = expect_command_error(
        &mut fixture.client,
        "failed accounting save must be visible",
    )
    .await;
    assert_eq!(failure.code, CommandErrorCode::Internal);
    assert!(
        failure
            .message
            .contains("Cannot clear closed task restoration markers")
    );
    drop(failed_commit);
    let (mut contender, _) = fixture.connect_with_bootstrap().await;
    let close_preparation = fixture
        .host_for_test()
        .install_restore_marker_withdraw_test_gate()
        .await;
    fixture
        .client
        .close_agent(&grandchild.instance_stream)
        .await
        .expect("close grandchild");
    close_preparation.wait_until_entered().await;
    contender
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("must-not-escape-close".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(grandchild.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/closing-child".to_owned()],
                prompt: "must not run".to_owned(),
                images: None,
                backend_kind: BackendKind::Grok,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("concurrent child admission request");
    let blocked =
        expect_command_error(&mut contender, "close preparation blocks child admission").await;
    assert_eq!(blocked.code, CommandErrorCode::Conflict);
    assert!(blocked.message.contains("closing"));
    completion_gate.release_one();
    fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "completed own accounting while close preparation is held",
        |env| {
            env.stream == grandchild.instance_stream
                && env.kind == FrameKind::AgentActivityStats
                && env
                    .parse_payload::<protocol::AgentActivityStatsPayload>()
                    .is_ok_and(|stats| {
                        stats.stats.token_usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 2
                    })
        },
    )
    .await;
    close_preparation.release_one();
    close_preparation.wait_until_entered().await;
    // Inject the stale marker a finishing startup can write after the bulk clear.
    let session = payload
        .breakdown
        .iter()
        .find(|entry| entry.agent_id == grandchild.agent_id)
        .and_then(|entry| entry.session_id.clone())
        .expect("grandchild session for settled marker race");
    let marker_writer = rusqlite::Connection::open(
        server::store::session::SessionStore::database_path(&fixture.session_store_path()),
    )
    .expect("open marker fault writer");
    marker_writer
        .execute(
            "UPDATE sessions SET record=json_set(record, '$.restore_state', json(?1)) WHERE id=?2",
            rusqlite::params![
                serde_json::json!({
                    "agent_id": grandchild.agent_id,
                    "origin": protocol::AgentOrigin::User,
                    "workflow": null,
                })
                .to_string(),
                session.0
            ],
        )
        .expect("inject late ownership marker");
    drop(marker_writer);
    let failed_settled_marker = fixture.fail_next_session_commit();
    close_preparation.release_one();
    let failed = expect_command_error(
        &mut fixture.client,
        "settled marker persistence must fail explicitly",
    )
    .await;
    assert!(failed.message.contains("settled close"));
    assert_eq!(failed.code, CommandErrorCode::Internal);
    drop(failed_settled_marker);
    drop(close_preparation);
    fixture
        .client
        .close_agent(&grandchild.instance_stream)
        .await
        .expect("retry settled close after storage recovery");
    let closed = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "grandchild AgentClosed",
    )
    .await;
    let closed: AgentClosedPayload = closed.parse_payload().expect("AgentClosed payload");
    assert_eq!(closed.agent_id, grandchild.agent_id);

    let after_close = expect_task_token_usage_matching(
        &mut usage_client,
        &root.agent_id,
        "aggregate after grandchild close",
        |payload| payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 5,
    )
    .await;
    assert_eq!(after_close.descendant_count, 2);
    assert_eq!(
        after_close.backends[1].agent_count, 1,
        "closed descendants remain in backend rollups"
    );
    assert_eq!(
        after_close.backends[1].total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 2
    );
    let grandchild_entry = after_close
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&grandchild.agent_id))
        .expect("closed grandchild breakdown entry");
    assert_known_task_scope(&grandchild_entry.usage, MOCK_TURN_TOKEN_TOTAL * 2);
    let grandchild_session = grandchild_entry
        .session_id
        .clone()
        .expect("saved grandchild session");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("usage-grandchild-resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(child.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: grandchild_session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume saved child");
    let reopened = expect_new_agent(&mut fixture.client, "reopened grandchild").await;
    expect_agent_start_on_stream(
        &mut fixture.client,
        &reopened.instance_stream,
        "reopened child ready",
    )
    .await;
    fixture
        .client
        .close_agent(&reopened.instance_stream)
        .await
        .expect("close reopened child");
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "reopened child closed",
    )
    .await;
    assert_eq!(
        env.parse_payload::<AgentClosedPayload>()
            .expect("closed child event")
            .agent_id,
        reopened.agent_id
    );
    let (_, reclosed) = fixture.connect_with_bootstrap().await;
    let reclosed = reclosed
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == root.agent_id)
        .expect("reclosed child rollup");
    assert_eq!(
        reclosed.total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 5,
        "closing a resumed session must replace its saved descriptor"
    );
    assert_eq!(reclosed.breakdown.len(), 3);
    assert_eq!(
        reclosed
            .breakdown
            .iter()
            .filter(|entry| entry.session_id.as_ref() == Some(&grandchild_session))
            .count(),
        1
    );

    fixture
        .client
        .delete_session(protocol::DeleteSessionPayload {
            session_id: grandchild_session.clone(),
        })
        .await
        .expect("delete closed chat context");
    let (mut deletion_observer, deleted_bootstrap) = fixture.connect_with_bootstrap().await;
    let retained = if let Some(usage) = deleted_bootstrap.task_token_usages.iter().find(|usage| {
        usage.root_agent_id == root.agent_id
            && usage.breakdown.iter().any(|entry| {
                entry.session_id.as_ref() == Some(&grandchild_session)
                    && matches!(entry.navigation, protocol::TaskUsageNavigation::Unavailable)
            })
    }) {
        usage.clone()
    } else {
        expect_task_token_usage_matching(
            &mut deletion_observer,
            &root.agent_id,
            "deleted context retains task accounting",
            |usage| {
                usage.breakdown.iter().any(|entry| {
                    entry.session_id.as_ref() == Some(&grandchild_session)
                        && matches!(entry.navigation, protocol::TaskUsageNavigation::Unavailable)
                })
            },
        )
        .await
    };
    assert_eq!(
        retained.total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 5,
        "deleting chat context must not delete a task's observed accounting"
    );
    let root_session = after_close
        .root_session_id
        .clone()
        .expect("root session present");
    let child_session = after_close
        .breakdown
        .iter()
        .find(|entry| entry.agent_id == child.agent_id)
        .and_then(|entry| entry.session_id.clone())
        .expect("child session present");
    let fork_reservation = fixture
        .reserve_next_mock_launch(
            "usage-fork",
            MockScript::one(MockTurn::text("fork own usage completed")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("usage-fork".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: child_session.clone(),
                prompt: "fork own turn".to_owned(),
                images: None,
                access_mode: None,
            },
        })
        .await
        .expect("fork usage source");
    let fork = expect_new_agent(&mut fixture.client, "usage fork").await;
    expect_agent_start_on_stream(&mut fixture.client, &fork.instance_stream, "fork start").await;
    // Fork bootstrap includes inherited responses before its new own response.
    let mut inherited_responses = 0;
    loop {
        let env = expect_chat_event_on_stream(
            &mut fixture.client,
            &fork.instance_stream,
            "fork own response after inherited context",
        )
        .await;
        if let ChatEvent::StreamEnd(end) = env.parse_payload::<ChatEvent>().expect("fork response")
        {
            if end.message.content == "fork own usage completed" {
                break;
            }
            inherited_responses += 1;
        }
    }
    assert!(
        inherited_responses > 0,
        "fork must still display inherited context"
    );
    drop(fork_reservation);
    let (_, fork_ready) = fixture.connect_with_bootstrap().await;
    let fork_usage = fork_ready
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == fork.agent_id)
        .expect("fork usage");
    assert_eq!(
        fork_usage.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL,
        "inherited context is not own usage"
    );
    let fork_session = fork_usage.root_session_id.clone().expect("fork session");
    let bootstrap = fixture.restart_host().await;
    let restored_agents = fixture
        .restored_agents(
            &bootstrap,
            &[
                root_session.clone(),
                child_session.clone(),
                fork_session.clone(),
            ],
        )
        .await;
    let restored_agent = &restored_agents[&root_session];
    let (_, ready) = fixture.connect_with_bootstrap().await;
    let restored = ready
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == restored_agent.agent_id)
        .expect("restored task usage");
    eprintln!(
        "Restored usage scope: agents={} descendants={} incomplete={:?}",
        restored.breakdown.len(),
        restored.descendant_count,
        restored.total.status
    );
    assert_eq!(restored.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL * 5);
    let restored_fork = ready
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == restored_agents[&fork_session].agent_id)
        .expect("restored fork own usage");
    assert_eq!(
        restored_fork.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL,
        "restart must not allocate inherited fork history to the fork"
    );
    let restored_child = &restored_agents[&child_session];
    fixture
        .client
        .send_message(
            &restored_child.instance_stream,
            "after accounting restore".to_owned(),
        )
        .await
        .expect("follow-up after restore");
    expect_completed_turn_on_stream(
        &mut fixture.client,
        &restored_child.instance_stream,
        "mock backend response to: after accounting restore",
    )
    .await;
    let (_, updated) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        updated
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == restored_agent.agent_id)
            .expect("updated restored task")
            .total
            .usage
            .total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 6,
        "new observations add once after canonical hydration"
    );

    assert_eq!(
        restored.breakdown.len(),
        3,
        "restart must retain closed descendants without duplicating live sessions"
    );
    assert_eq!(
        restored
            .backends
            .iter()
            .find(|group| group.backend_kind == BackendKind::Grok)
            .expect("closed provider group")
            .agent_count,
        1
    );

    let before_mix = updated
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == restored_agent.agent_id)
        .expect("pre-mix authoritative usage")
        .clone();
    let (mut observer, _) = fixture.connect_with_bootstrap().await;
    fixture
        .mock_by_id(&restored_child.agent_id)
        .await
        .enqueue(
            MockTurn::text("mixed component coverage")
                .with_native_child_usage(
                    "total-only child",
                    "total only",
                    server::backend::mock::MockNativeChildUsage::TotalOnly(47),
                )
                .with_native_child_usage(
                    "partial-split child",
                    "partial split",
                    server::backend::mock::MockNativeChildUsage::SplitWithTotal(500),
                )
                .with_native_child_usage(
                    "mixed-optional child",
                    "mixed optional",
                    server::backend::mock::MockNativeChildUsage::MixedOptionalWithEqualTotal,
                ),
        )
        .await;
    fixture
        .client
        .send_message(
            &restored_child.instance_stream,
            "mixed component coverage".to_owned(),
        )
        .await
        .expect("start mixed reporting");
    expect_completed_turn_on_stream(
        &mut fixture.client,
        &restored_child.instance_stream,
        "mixed component coverage",
    )
    .await;
    let expected_total = MOCK_TURN_TOKEN_TOTAL * 7 + 47 + 500 + 363;
    for _ in 0..4 {
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "native component observation complete",
            |env| {
                env.stream != restored_child.instance_stream
                    && env.kind == FrameKind::ChatEvent
                    && env
                        .parse_payload::<ChatEvent>()
                        .is_ok_and(|event| matches!(event, ChatEvent::StreamEnd(_)))
            },
        )
        .await;
    }
    let mixed = expect_task_token_usage_matching(
        &mut observer,
        &restored_agent.agent_id,
        "split and total-only descendants",
        |payload| {
            payload.total.usage.total_tokens == expected_total && payload.breakdown.len() == 6
        },
    )
    .await;
    let (_, settled) = fixture.connect_with_bootstrap().await;
    let mixed = settled
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == mixed.root_agent_id)
        .expect("settled component accounting");
    assert_known_task_status(&mixed.total.status);
    assert_eq!(
        mixed.total.usage.output_tokens,
        before_mix
            .total
            .usage
            .output_tokens
            .map(|output| output + 340 + 80 + 88)
    );
    assert_eq!(
        mixed.total.usage.cached_prompt_tokens,
        before_mix
            .total
            .usage
            .cached_prompt_tokens
            .map(|cached| cached + 800)
    );
    assert_eq!(
        mixed.total.usage.prompt_tokens,
        before_mix
            .total
            .usage
            .prompt_tokens
            .map(|prompt| prompt + 2_100 + 250 + 275)
    );
    assert_eq!(
        mixed.total.usage.input_tokens,
        before_mix
            .total
            .usage
            .input_tokens
            .map(|input| input + 1_250 + 250 + 275)
    );
    assert_eq!(
        mixed.total.usage.reasoning_tokens,
        before_mix
            .total
            .usage
            .reasoning_tokens
            .map(|reasoning| reasoning + 120)
    );
    assert_eq!(
        mixed.total.usage.cache_creation_input_tokens,
        before_mix
            .total
            .usage
            .cache_creation_input_tokens
            .map(|created| created + 50)
    );
    for field in [
        protocol::TaskTokenUsageField::Prompt,
        protocol::TaskTokenUsageField::Input,
        protocol::TaskTokenUsageField::Output,
        protocol::TaskTokenUsageField::CachedPrompt,
        protocol::TaskTokenUsageField::Reasoning,
    ] {
        assert!(
            mixed.total.usage.partial_fields.contains(&field),
            "reported components must carry incomplete coverage"
        );
    }
    let total_only = mixed
        .breakdown
        .iter()
        .find(|entry| entry.name == "total-only child")
        .expect("total-only descendant");
    let amount = total_only
        .usage
        .reported_usage()
        .expect("authoritative combined total");
    assert_eq!(amount.total_tokens, 47);
    assert_eq!(amount.prompt_tokens, None);
    assert_eq!(amount.output_tokens, None);
    assert_eq!(amount.cached_prompt_tokens, None);
    assert_eq!(amount.reasoning_tokens, None);
    let partial_split = mixed
        .breakdown
        .iter()
        .find(|entry| entry.name == "partial-split child")
        .expect("partially split descendant");
    let amount = partial_split
        .usage
        .reported_usage()
        .expect("authoritative split subtotal");
    assert_eq!(amount.total_tokens, 500);
    assert_eq!(amount.prompt_tokens, Some(250));
    assert_eq!(amount.output_tokens, Some(80));
    assert!(
        amount
            .partial_fields
            .contains(&protocol::TaskTokenUsageField::Output)
    );
    let mixed_optional = mixed
        .breakdown
        .iter()
        .find(|entry| entry.name == "mixed-optional child")
        .expect("mixed optional coverage descendant");
    let amount = mixed_optional
        .usage
        .reported_usage()
        .expect("equal combined total split");
    assert_eq!(amount.total_tokens, 363);
    assert_eq!(amount.prompt_tokens, Some(275));
    assert_eq!(amount.output_tokens, Some(88));
    assert_eq!(amount.cached_prompt_tokens, Some(0));
    assert_eq!(amount.reasoning_tokens, Some(0));
    assert_eq!(
        amount.partial_fields,
        vec![
            protocol::TaskTokenUsageField::CachedPrompt,
            protocol::TaskTokenUsageField::CacheCreationInput,
            protocol::TaskTokenUsageField::Reasoning
        ],
        "equal total retains exact prompt/output but incomplete optional counters"
    );
    let claude = mixed
        .backends
        .iter()
        .find(|group| group.backend_kind == BackendKind::Claude)
        .expect("mixed Claude group");
    assert!(claude.total.usage.prompt_tokens.is_some());
    assert!(
        claude
            .total
            .usage
            .partial_fields
            .contains(&protocol::TaskTokenUsageField::Prompt)
    );
    assert_eq!(
        mixed
            .backends
            .iter()
            .find(|group| group.backend_kind == BackendKind::Grok),
        before_mix
            .backends
            .iter()
            .find(|group| group.backend_kind == BackendKind::Grok),
        "another provider's closed usage remains untouched"
    );
    let (mut replay_client, replay) = fixture.connect_with_bootstrap().await;
    let replayed = replay
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored_agent.agent_id)
        .expect("mixed usage reconnect");
    assert_eq!(replayed.total, mixed.total);
    for entry in [total_only, partial_split, mixed_optional] {
        let agent = replay
            .agents
            .iter()
            .find(|agent| agent.agent_id == entry.agent_id)
            .expect("native live descriptor");
        replay_client
            .close_agent(&agent.instance_stream)
            .await
            .expect("close mixed native child");
        let closed = expect_kind(
            &mut replay_client,
            FrameKind::AgentClosed,
            "mixed child closed",
        )
        .await;
        assert_eq!(
            closed
                .parse_payload::<AgentClosedPayload>()
                .expect("closed payload")
                .agent_id,
            entry.agent_id
        );
    }
    let (_, closed) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        closed
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == restored_agent.agent_id)
            .expect("closed mixed rollup")
            .total,
        mixed.total
    );
    let session = mixed.root_session_id.clone().expect("mixed root session");
    let restarted = fixture.relaunch_host_after_kill().await;
    let restored = fixture
        .restored_agents(&restarted, std::slice::from_ref(&session))
        .await;
    let (_, replay) = fixture.connect_with_bootstrap().await;
    let replayed = replay
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == restored[&session].agent_id)
        .expect("mixed usage restart");
    assert_eq!(replayed.total.usage, mixed.total.usage);
    assert_eq!(replayed.breakdown.len(), 6);
}

#[tokio::test]
async fn task_native_accounting_updates_after_idle_and_restores_known_zero() {
    use server::backend::Backend;
    let discovery =
        server::backend::mock::MockDiscovery::new(vec![Ok(server::backend::BackendDiscovery {
            schema: server::backend::codex::CodexBackend::session_settings_schema(),
            launch_profiles: Vec::new(),
        })]);
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        mock_backend_discovery: [(BackendKind::Codex, discovery.clone())]
            .into_iter()
            .collect(),
        ..Default::default()
    })
    .await;
    let write = fixture
        .client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Claude, BackendKind::Codex],
            Vec::<BackendKind>::new(),
        )
        .await
        .expect("enable native counter source");
    expect_settings_write_applied(&mut fixture.client, &write, "enable native counter source")
        .await;
    let gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "native-usage",
            MockScript::one(MockTurn::text_with_late_zero_native_usage(
                "native counter pending",
                &gate,
            )),
        )
        .await;
    let agent = spawn_token_usage_agent_with_backend(
        &mut fixture.client,
        "native-usage",
        "native source",
        BackendKind::Codex,
    )
    .await;
    expect_completed_turn_on_stream(
        &mut fixture.client,
        &agent.instance_stream,
        "native counter pending",
    )
    .await;
    gate.wait_until_entered().await;
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    let pending = bootstrap
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == agent.agent_id)
        .expect("native pending rollup");
    assert!(matches!(
        pending.total.status,
        TaskTokenUsageStatus::Unavailable { .. }
    ));
    gate.release_one();
    let reported = expect_task_token_usage_matching(
        &mut observer,
        &agent.agent_id,
        "late native zero",
        |payload| matches!(payload.total.status, TaskTokenUsageStatus::Known),
    )
    .await;
    assert_known_task_scope(&reported.self_usage, 0);
    let session = reported.root_session_id.expect("native session");
    drop(reservation);
    let first_restart_discovery = discovery.clone();
    let bootstrap = fixture
        .restart_host_with_runtime_config(move |config| {
            config
                .mock_backend_discovery
                .insert(BackendKind::Codex, first_restart_discovery);
        })
        .await;
    let agents = fixture
        .restored_agents(&bootstrap, std::slice::from_ref(&session))
        .await;
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    let restored = bootstrap
        .task_token_usages
        .iter()
        .find(|payload| payload.root_agent_id == agents[&session].agent_id)
        .expect("restored native rollup");
    assert_known_task_scope(&restored.self_usage, 0);
    assert_eq!(
        restored.backends[0].reporting_agent_count, 1,
        "reported zero is not missing after restart"
    );
    let gate = MockGateHandle::new();
    let restored_agent = &agents[&session];
    fixture
        .mock_by_id(&restored_agent.agent_id)
        .await
        .enqueue(MockTurn::text_with_late_native_usage(
            "native counter save pending",
            &gate,
            49,
        ))
        .await;
    fixture
        .client
        .send_message(
            &restored_agent.instance_stream,
            "counter save failure".to_owned(),
        )
        .await
        .expect("start native checkpoint failure flow");
    expect_completed_turn_on_stream(
        &mut fixture.client,
        &restored_agent.instance_stream,
        "native counter save pending",
    )
    .await;
    gate.wait_until_entered().await;
    let failed = fixture.fail_next_session_commit();
    gate.release_one();
    fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "native checkpoint failure is fatal and explicit",
        |env| {
            env.stream == restored_agent.instance_stream
                && env.kind == FrameKind::AgentError
                && env.parse_payload::<AgentErrorPayload>().is_ok_and(|error| {
                    error.fatal && error.message.contains("Task accounting could not be saved")
                })
        },
    )
    .await;
    drop(failed);
    let bootstrap = fixture
        .restart_host_with_runtime_config(move |config| {
            config
                .mock_backend_discovery
                .insert(BackendKind::Codex, discovery);
        })
        .await;
    let agents = fixture
        .restored_agents(&bootstrap, std::slice::from_ref(&session))
        .await;
    let (_, after) = fixture.connect_with_bootstrap().await;
    let usage = after
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == agents[&session].agent_id)
        .expect("native failed-checkpoint coverage");
    assert!(
        matches!(&usage.total.status, TaskTokenUsageStatus::Partial { reasons, .. } if reasons.contains(&TaskTokenUsageUnavailableReason::AccountingHistoryIncomplete)),
        "an older native checkpoint must not become complete after a failed save"
    );
    assert_eq!(
        usage.total.usage.total_tokens, 49,
        "the required final archive commit must preserve the exact latest native checkpoint"
    );
}

/// The agent's exclusive wall-clock account, checked for the invariants every
/// consumer relies on: one phase per moment, and a task interval whose primary
/// totals add up to exactly its elapsed time.
fn wall_clock_account<'a>(
    usage: &'a TaskTokenUsagePayload,
    agent_id: &protocol::AgentId,
) -> &'a protocol::AgentTimeAccount {
    let interval = usage
        .timing
        .interval
        .as_ref()
        .expect("task wall-clock interval");
    assert_eq!(
        interval.wall_clock.sum(),
        interval.observed_through_ms - interval.started_at_ms,
        "primary totals must reconcile to the elapsed interval"
    );
    let account = &usage
        .timing
        .accounts
        .iter()
        .find(|entry| &entry.agent_id == agent_id)
        .expect("agent wall-clock account")
        .account;
    assert_eq!(account.compacted_phase_count, 0);
    assert_eq!(
        account.totals.sum(),
        account.observed_through_ms - account.started_at_ms,
        "each accounted moment is in exactly one phase"
    );
    let mut through = account.started_at_ms;
    for segment in &account.phases {
        assert_eq!(
            segment.started_at_ms, through,
            "phases must cover the account without gaps or overlap"
        );
        through = segment.ended_at_ms;
    }
    assert_eq!(through, account.observed_through_ms);
    account
}

fn is_activity_change(
    env: &Envelope,
    stream: &StreamPath,
    activity: protocol::AgentActivity,
) -> bool {
    env.stream == *stream
        && env.kind == FrameKind::AgentActivityChanged
        && env
            .parse_payload::<protocol::AgentActivityChangedPayload>()
            .is_ok_and(|payload| payload.activity == activity)
}

fn phase_durations(
    account: &protocol::AgentTimeAccount,
) -> Vec<(protocol::TaskTimePhase, u64, protocol::TaskTimePhaseEnd)> {
    account
        .phases
        .iter()
        .map(|segment| {
            (
                segment.phase,
                segment.ended_at_ms - segment.started_at_ms,
                segment.end,
            )
        })
        .collect()
}

/// These are receipt-boundary measurements on the real server/protocol,
/// deliberately not provider or process-lifetime measurements.
#[tokio::test(start_paused = true)]
async fn task_timing_observes_overlap_background_completion_and_restart_without_replay() {
    let (clock_resumed_tx, clock_resumed_rx) = tokio::sync::oneshot::channel();
    let scenario = async {
        let mut fixture = Fixture::new().await;
        let root = fixture
            .spawn_scripted(
                "timing-root",
                MockScript::one(MockTurn::text("timing ready")),
            )
            .await;
        fixture.finish_turn(&root).await;
        let child = spawn_user_child(
            &mut fixture.client,
            &root.new_agent.agent_id,
            "timing-child",
            "child usage",
            "/tmp/timing-child",
        )
        .await;
        let foreground = MockGateHandle::new();
        let background = MockGateHandle::new();
        fixture
            .mock(&root)
            .await
            .enqueue(MockTurn::timing_flow(
                child.agent_id.clone(),
                &foreground,
                &background,
            ))
            .await;
        fixture
            .client
            .send_message(&root.stream, "observe tools".to_owned())
            .await
            .expect("send timing turn");
        foreground.wait_until_entered().await;
        fixture.next_chat_event_matching(&root, "wait request observed", |event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "timing-await")).await;
        let (_, bootstrap) = fixture.connect_with_bootstrap().await;
        let initial = bootstrap
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == root.new_agent.agent_id)
            .expect("task bootstrap");
        assert_eq!(
            initial.timing.spans.len(),
            2,
            "both live tool boundaries must be retained"
        );
        assert_eq!(
            initial.timing.command_count, 1,
            "waits are not command runs"
        );
        assert_eq!(initial.timing.summed_tool_ms, 0);
        assert_eq!(initial.timing.summed_dependency_wait_ms, 0);
        assert_eq!(
            initial.timing.process_lifetime,
            protocol::TimingAvailability::Unsupported
        );
        assert_eq!(
            initial.timing.provider_request_round_trip,
            protocol::TimingAvailability::Unsupported
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        foreground.release_one();
        background.wait_until_entered().await;
        fixture
            .next_chat_event_matching(&root, "background foreground-idle boundary", |event| {
                matches!(event, ChatEvent::TypingStatusChanged(false))
            })
            .await;
        let (_, bootstrap) = fixture.connect_with_bootstrap().await;
        let running = bootstrap
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == root.new_agent.agent_id)
            .expect("running task bootstrap");
        assert_eq!(running.timing.summed_tool_ms, 2_000);
        assert_eq!(
            running.timing.summed_dependency_wait_ms, 2_000,
            "parallel waits must stay separate from child/tool time"
        );
        assert!(running.timing.spans.iter().any(|entry| matches!(
            entry.span.category,
            protocol::TaskTimingCategory::Command { check: true, .. }
        ) && entry.span.state
            == protocol::TaskTimingState::Open));
        use protocol::TaskTimePhase::{
            Command, DependencyWait, HumanWait, Idle, ModelResponse, NotRunning, Tool, Unobserved,
        };
        use protocol::TaskTimePhaseEnd::{Ended, Interrupted, Open};
        let root_account = wall_clock_account(running, &root.new_agent.agent_id);
        assert_eq!(
            phase_durations(root_account),
            vec![(DependencyWait, 2_000, Ended), (Idle, 0, Open)],
            "the overlapping command is wall time already spent waiting on the child"
        );
        assert_eq!(root_account.totals.command_ms, 0);
        let interval = running.timing.interval.as_ref().expect("running interval");
        assert_eq!(interval.wall_clock.dependency_wait_ms, 2_000);
        assert_eq!(interval.wall_clock.sum(), 2_000);
        assert_eq!(interval.accounted_agent_count, 2);
        assert_eq!(
            wall_clock_account(running, &child.agent_id).totals.idle_ms,
            2_000
        );
        tokio::time::advance(Duration::from_secs(3)).await;
        background.release_one();
        fixture.next_chat_event_matching(&root, "background completion", |event| matches!(event, ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == "timing-check")).await;
        let (_, completed_bootstrap) = fixture.connect_with_bootstrap().await;
        let completed = completed_bootstrap
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == root.new_agent.agent_id)
            .expect("completed task bootstrap");
        assert_eq!(completed.timing.command_count, 1);
        assert_eq!(completed.timing.summed_tool_ms, 5_000);
        assert_eq!(completed.timing.summed_dependency_wait_ms, 2_000);
        assert!(
            completed
                .timing
                .spans
                .iter()
                .all(|entry| entry.span.state == protocol::TaskTimingState::Completed)
        );
        let root_account = wall_clock_account(completed, &root.new_agent.agent_id);
        let task_started_at = root_account.started_at_ms;
        assert_eq!(
            phase_durations(root_account),
            vec![(DependencyWait, 2_000, Ended), (Idle, 3_000, Open)],
            "a background command does not keep an idle agent working"
        );
        let interval = completed
            .timing
            .interval
            .as_ref()
            .expect("completed interval");
        assert_eq!(interval.started_at_ms, task_started_at);
        assert_eq!(interval.wall_clock.sum(), 5_000);
        assert_eq!(
            interval.agent_time.sum(),
            10_000,
            "summed agent time counts the idle child alongside the root"
        );
        let session = completed.root_session_id.clone().expect("root session");
        // Begin another open span, then restart. Historical tool replay must
        // neither add executions nor extend an open span across downtime.
        fixture
            .mock(&root)
            .await
            .enqueue(MockTurn::held_running_command(
                "interrupted-timing",
                server::backend::mock::RunningCommandShape::RequestAfterResponse,
            ))
            .await;
        fixture
            .client
            .send_message(&root.stream, "interrupt timing".to_owned())
            .await
            .expect("send interrupted timing turn");
        fixture.next_chat_event_matching(&root, "open span before restart", |event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "interrupted-timing")).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        fixture
            .client
            .close_agent(&root.stream)
            .await
            .expect("close open timing span");
        loop {
            let env = expect_kind(
                &mut fixture.client,
                FrameKind::AgentClosed,
                "timing root closed",
            )
            .await;
            if env
                .parse_payload::<AgentClosedPayload>()
                .expect("timing close event")
                .agent_id
                == root.new_agent.agent_id
            {
                break;
            }
        }
        tokio::time::advance(Duration::from_secs(4)).await;
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                launch_context: protocol::AgentLaunchContext::Standard,
                name: Some("timing-root-resumed".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::Resume {
                    session_id: session.clone(),
                    prompt: None,
                },
            })
            .await
            .expect("resume interrupted timing");
        let reopened = expect_new_agent(&mut fixture.client, "timing reopened").await;
        expect_agent_start_on_stream(
            &mut fixture.client,
            &reopened.instance_stream,
            "timing reopened ready",
        )
        .await;
        let (_, reopened_bootstrap) = fixture.connect_with_bootstrap().await;
        let reopened_usage = reopened_bootstrap
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == reopened.agent_id)
            .expect("reopened timing usage");
        assert_eq!(
            reopened_usage.timing.summed_tool_ms, 6_000,
            "close/resume must keep the last observed endpoint"
        );
        assert_eq!(reopened_usage.timing.spans.len(), 3);
        let reopened_account = wall_clock_account(reopened_usage, &reopened.agent_id);
        assert_eq!(reopened_account.started_at_ms, task_started_at);
        assert_eq!(
            phase_durations(reopened_account),
            vec![
                (DependencyWait, 2_000, Ended),
                (Idle, 3_000, Ended),
                (Command, 1_000, Ended),
                (NotRunning, 4_000, Ended),
                (Idle, 0, Open),
            ],
            "a clean close is a known endpoint followed by not-running time"
        );
        fixture
            .mock_by_id(&reopened.agent_id)
            .await
            .enqueue(MockTurn::held_running_command(
                "restart-open-timing",
                server::backend::mock::RunningCommandShape::RequestAfterResponse,
            ))
            .await;
        fixture
            .client
            .send_message(
                &reopened.instance_stream,
                "restart observed work".to_owned(),
            )
            .await
            .expect("start direct restart tool");
        fixture::next_logical_frame_matching_on(&mut fixture.client, "direct restart open observation", |env| env.stream == reopened.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "restart-open-timing"))).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let bootstrap = fixture.restart_host().await;
        let restored_agents = fixture
            .restored_agents(&bootstrap, std::slice::from_ref(&session))
            .await;
        let restored_agent = &restored_agents[&session];
        // Bootstrap may arrive before, during or after the restart turn.
        let (mut completion_observer, observed_bootstrap) = fixture.connect_with_bootstrap().await;
        let completion_stream = observed_bootstrap
            .agents
            .iter()
            .find(|agent| agent.agent_id == restored_agent.agent_id)
            .expect("observer's restored agent descriptor")
            .instance_stream
            .clone();
        eprintln!(
            "Restart completion observer: distinct_stream={}",
            completion_stream != restored_agent.instance_stream
        );
        let mut continuing = false;
        loop {
            let event = completion_observer
                .next_event()
                .await
                .expect("restart completion protocol read")
                .expect("restart completion connection stays open");
            if event.stream != completion_stream {
                continue;
            }
            match event.kind {
                FrameKind::AgentBootstrap => {
                    let bootstrap: protocol::AgentBootstrapPayload =
                        event.parse_payload().expect("restart completion bootstrap");
                    continuing |= bootstrap.events.iter().any(|event| {
                        matches!(
                            event,
                            protocol::AgentBootstrapEvent::ChatEvent(ChatEvent::RestartRecovery {
                                phase: protocol::RestartRecoveryPhase::Continuing
                            })
                        )
                    });
                    eprintln!(
                        "Restart completion bootstrap: continuing={continuing} activity={:?}",
                        bootstrap.activity
                    );
                    if continuing && bootstrap.activity == protocol::AgentActivity::Idle {
                        break;
                    }
                }
                FrameKind::ChatEvent => {
                    let chat: ChatEvent = event
                        .parse_payload()
                        .expect("restart completion chat event");
                    continuing |= matches!(
                        chat,
                        ChatEvent::RestartRecovery {
                            phase: protocol::RestartRecoveryPhase::Continuing
                        }
                    );
                    if continuing && matches!(chat, ChatEvent::TypingStatusChanged(false)) {
                        eprintln!("Restart completion: observed live idle");
                        break;
                    }
                }
                _ => {}
            }
        }
        let (_, replay) = fixture.connect_with_bootstrap().await;
        let restored = replay
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == restored_agent.agent_id)
            .expect("restored task timing");
        eprintln!(
            "Restored timing trace: spans={} covered={} persistence_failures={}",
            restored.timing.spans.len(),
            restored.timing.covered_agent_count,
            restored.timing.persistence_failed_agent_count
        );
        assert_eq!(
            restored.timing.spans.len(),
            4,
            "resume replay cannot duplicate tool spans"
        );
        assert_eq!(restored.timing.command_count, 3);
        assert_eq!(
            restored.timing.summed_tool_ms, 8_000,
            "restart cannot invent observed active downtime"
        );
        assert!(
            restored
                .timing
                .spans
                .iter()
                .any(|entry| entry.span.tool_call_id == "interrupted-timing"
                    && entry.span.state == protocol::TaskTimingState::Interrupted)
        );
        assert!(
            restored
                .timing
                .spans
                .iter()
                .any(|entry| entry.span.tool_call_id == "restart-open-timing"
                    && entry.span.state == protocol::TaskTimingState::Interrupted
                    && entry.span.observed_duration_ms == 2_000)
        );
        let restored_account = wall_clock_account(restored, &restored_agent.agent_id);
        assert_eq!(
            phase_durations(restored_account)[3..],
            [
                (NotRunning, 4_000, Ended),
                (Command, 2_000, Ended),
                (Idle, 0, Open)
            ],
            "a graceful restart ends the open phase at the durable stop"
        );
        // Human wait, then a crash: only the last durable observation is kept.
        let question = MockGateHandle::new();
        fixture
            .mock_by_id(&restored_agent.agent_id)
            .await
            .enqueue(MockTurn::blocking_question_request(
                "timing-question",
                &question,
            ))
            .await;
        fixture
            .client
            .send_message(&restored_agent.instance_stream, "ask timing".to_owned())
            .await
            .expect("ask timing question");
        question.wait_until_entered().await;
        question.release_one();
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "timing question awaits the user",
            |env| {
                is_activity_change(
                    env,
                    &restored_agent.instance_stream,
                    protocol::AgentActivity::AwaitingUser,
                )
            },
        )
        .await;
        // Connected clients see live waits grow and end, not only new bootstraps.
        let live_tail = |payload: &TaskTokenUsagePayload, count: usize| {
            let phases = phase_durations(wall_clock_account(payload, &restored_agent.agent_id));
            phases[phases.len().saturating_sub(count)..].to_vec()
        };
        fixture
            .host_for_test()
            .synchronize_task_usage_refresh_for_test()
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        expect_task_token_usage_matching(
            &mut fixture.client,
            &restored_agent.agent_id,
            "live human wait",
            |payload| live_tail(payload, 1) == [(HumanWait, 1_000, Open)],
        )
        .await;
        fixture
            .host_for_test()
            .synchronize_task_usage_refresh_for_test()
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        expect_task_token_usage_matching(
            &mut fixture.client,
            &restored_agent.agent_id,
            "human wait refreshes without a status change",
            |payload| live_tail(payload, 1) == [(HumanWait, 2_000, Open)],
        )
        .await;
        fixture
            .host_for_test()
            .synchronize_task_usage_refresh_for_test()
            .await;
        tokio::time::advance(Duration::from_secs(28)).await;
        expect_task_token_usage_matching(
            &mut fixture.client,
            &restored_agent.agent_id,
            "long human wait grows without status changes",
            |payload| live_tail(payload, 1) == [(HumanWait, 30_000, Open)],
        )
        .await;
        fixture
            .host_for_test()
            .synchronize_task_usage_refresh_for_test()
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        fixture
            .host_for_test()
            .synchronize_task_usage_refresh_for_test()
            .await;
        fixture
            .client
            .list_sessions(ListSessionsPayload::default())
            .await
            .expect("long-wait protocol barrier");
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "long waits do not retain a one-second refresh cadence",
            |env| {
                if env.kind == FrameKind::TaskTokenUsage {
                    let usage: TaskTokenUsagePayload =
                        env.parse_payload().expect("timing cadence frame");
                    assert!(
                        usage.root_agent_id != restored_agent.agent_id,
                        "no new task observation before the three-second refresh quantum"
                    );
                }
                env.kind == FrameKind::SessionList
            },
        )
        .await;
        tokio::time::advance(Duration::from_secs(2)).await;
        expect_task_token_usage_matching(
            &mut fixture.client,
            &restored_agent.agent_id,
            "long wait refreshes at its decayed cadence",
            |payload| live_tail(payload, 1) == [(HumanWait, 33_000, Open)],
        )
        .await;
        fixture
            .client
            .interrupt(&restored_agent.instance_stream)
            .await
            .expect("cancel timing question");
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "cancelled timing question settles idle",
            |env| {
                is_activity_change(
                    env,
                    &restored_agent.instance_stream,
                    protocol::AgentActivity::Idle,
                )
            },
        )
        .await;
        tokio::time::advance(Duration::from_secs(2)).await;
        fixture
            .host_for_test()
            .synchronize_task_usage_refresh_for_test()
            .await;
        expect_task_token_usage_matching(
            &mut fixture.client,
            &restored_agent.agent_id,
            "human wait ends live",
            |payload| live_tail(payload, 2) == [(HumanWait, 33_000, Ended), (Idle, 2_000, Open)],
        )
        .await;
        let killed = fixture.relaunch_host_after_kill().await;
        let killed_agents = fixture
            .restored_agents(&killed, std::slice::from_ref(&session))
            .await;
        let restored_agent = &killed_agents[&session];
        let (_, crashed) = fixture.connect_with_bootstrap().await;
        let crashed = crashed
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == restored_agent.agent_id)
            .expect("crash-restored timing");
        let crashed_account = wall_clock_account(crashed, &restored_agent.agent_id);
        assert_eq!(crashed_account.started_at_ms, task_started_at);
        assert_eq!(
            phase_durations(crashed_account)[5..],
            [
                (HumanWait, 33_000, Ended),
                (Idle, 0, Interrupted),
                (Unobserved, 2_000, Ended),
                (Idle, 0, Open),
            ],
            "time after the last durable observation is unobserved, never invented"
        );
        assert_eq!(crashed_account.totals.unobserved_ms, 2_000);
        // A tool left unfinished by its turn cannot classify the next turn.
        let model = MockGateHandle::new();
        let tool = MockGateHandle::new();
        fixture
            .mock_by_id(&restored_agent.agent_id)
            .await
            .enqueue(MockTurn::timing_model_then_unfinished_tool(
                "timing-read",
                &model,
                &tool,
            ))
            .await;
        fixture
            .client
            .send_message(&restored_agent.instance_stream, "read timing".to_owned())
            .await
            .expect("start unfinished tool turn");
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "model response starts",
            |env| {
                is_activity_change(
                    env,
                    &restored_agent.instance_stream,
                    protocol::AgentActivity::Thinking,
                )
            },
        )
        .await;
        model.wait_until_entered().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        model.release_one();
        fixture::next_logical_frame_matching_on(&mut fixture.client, "unfinished read requested", |env| env.stream == restored_agent.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolRequest(request) if request.tool_call_id == "timing-read"))).await;
        tool.wait_until_entered().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tool.release_one();
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "unfinished tool turn ends",
            |env| {
                is_activity_change(
                    env,
                    &restored_agent.instance_stream,
                    protocol::AgentActivity::Idle,
                )
            },
        )
        .await;
        let after = MockGateHandle::new();
        fixture
            .mock_by_id(&restored_agent.agent_id)
            .await
            .enqueue(MockTurn::gated_text("after unfinished tool", &after))
            .await;
        fixture
            .client
            .send_message(&restored_agent.instance_stream, "after read".to_owned())
            .await
            .expect("start turn after unfinished tool");
        fixture::next_logical_frame_matching_on(&mut fixture.client, "next turn starts", |env| {
            is_activity_change(
                env,
                &restored_agent.instance_stream,
                protocol::AgentActivity::Thinking,
            )
        })
        .await;
        after.wait_until_entered().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let (_, next_turn) = fixture.connect_with_bootstrap().await;
        let next_turn = next_turn
            .task_token_usages
            .iter()
            .find(|payload| payload.root_agent_id == restored_agent.agent_id)
            .expect("next-turn timing");
        assert_eq!(
            phase_durations(wall_clock_account(next_turn, &restored_agent.agent_id))[5..],
            [
                (HumanWait, 33_000, Ended),
                (Idle, 0, Interrupted),
                (Unobserved, 2_000, Ended),
                (ModelResponse, 1_000, Ended),
                (Tool, 1_000, Ended),
                (ModelResponse, 1_000, Open),
            ],
            "the next turn is model response, not the previous turn's tool"
        );
        assert!(
            next_turn
                .timing
                .spans
                .iter()
                .any(|entry| entry.span.tool_call_id == "timing-read"
                    && entry.span.state == protocol::TaskTimingState::Open),
            "ending the turn leaves the unfinished read open, not completed"
        );
        after.release_one();
        fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "turn after unfinished tool ends",
            |env| {
                is_activity_change(
                    env,
                    &restored_agent.instance_stream,
                    protocol::AgentActivity::Idle,
                )
            },
        )
        .await;
        tokio::time::resume();
        clock_resumed_tx.send(()).expect("clock resume observed");
        fixture
            .mock_by_id(&restored_agent.agent_id)
            .await
            .enqueue(MockTurn::text("retention source ready").with_tool_round_trips(65))
            .await;
        fixture
            .client
            .send_message(
                &restored_agent.instance_stream,
                "retain bounded observations".to_owned(),
            )
            .await
            .expect("run retention source");
        fixture::next_logical_frame_matching_on(&mut fixture.client, "root bounded trace complete", |env| env.stream == restored_agent.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == "retention-tool-64"))).await;
        for index in 0..4 {
            let name = format!("retention-child-{index}");
            let prompt = format!("retention-{index}");
            let reservation = fixture
                .reserve_next_mock_launch(
                    &name,
                    MockScript::one(
                        MockTurn::text(format!("mock backend response to: {prompt}"))
                            .with_tool_round_trips(65),
                    ),
                )
                .await;
            let child = spawn_user_child(
                &mut fixture.client,
                &restored_agent.agent_id,
                &name,
                &prompt,
                "/tmp/retention-child",
            )
            .await;
            drop(reservation);
            fixture::next_logical_frame_matching_on(&mut fixture.client, "child bounded trace complete", |env| env.stream == child.instance_stream && env.kind == FrameKind::ChatEvent && env.parse_payload::<ChatEvent>().is_ok_and(|event| matches!(event, ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == "retention-tool-64"))).await;
            let (_, own) = fixture.connect_with_bootstrap().await;
            assert_eq!(
                own.task_token_usages
                    .iter()
                    .find(|usage| usage.root_agent_id == restored_agent.agent_id)
                    .expect("bounded descendant traces")
                    .timing
                    .classified_tool_count,
                64 * (index + 2)
            );
        }
        let (_, bounded) = fixture.connect_with_bootstrap().await;
        let timing = &bounded
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == restored_agent.agent_id)
            .expect("bounded task trace")
            .timing;
        assert_eq!(timing.spans.len(), 256);
        assert_eq!(timing.omitted_span_count, 64);
        // Seven root drops (four timing spans, the question, the unfinished
        // read, one round trip) and one per child.
        assert_eq!(timing.dropped_span_count, 11);
    };
    tokio::pin!(scenario);
    tokio::pin!(clock_resumed_rx);
    loop {
        tokio::select! {
            biased;
            () = &mut scenario => return,
            result = &mut clock_resumed_rx => {
                result.expect("scenario resumes clock");
                break;
            }
            () = tokio::task::yield_now() => {},
        }
    }
    scenario.await;
}

/// A descendant whose first turn reports no usage first marks the whole
/// task Partial with an Unavailable descendant scope; once that same child
/// completes a known-usage turn, the descendant scope becomes Partial and
/// stays Partial after the child closes and across host bootstrap.
#[tokio::test]
async fn task_token_usage_marks_missing_then_partial_descendant_usage_partial() {
    let mut fixture = Fixture::new().await;
    let root =
        spawn_token_usage_agent(&mut fixture.client, "partial-descendant-root", "root usage").await;
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root usage",
    )
    .await;
    let (mut usage_client, _) = fixture.connect_with_bootstrap().await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "partial-descendant-child",
            MockScript::one(MockTurn::text_without_usage(
                "mock backend response to: child first missing usage",
            ))
            .with_unbounded_echo(),
        )
        .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "partial-descendant-child",
        "child first missing usage",
        "/tmp/partial-descendant-child",
    )
    .await;
    drop(reservation);

    // Before the child reports any usage, the aggregate is Partial and the
    // descendant scope is wholly Unavailable — never a fabricated zero.
    let missing = expect_task_token_usage_matching(
        &mut usage_client,
        &root.agent_id,
        "missing descendant aggregate",
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
    assert_eq!(missing.total.usage.total_tokens, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(missing.descendant_count, 1);
    match &missing.total.status {
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
    match &missing.descendant_usage.status {
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
    let missing_child_entry = missing
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("child breakdown entry");
    assert!(matches!(
        missing_child_entry.usage,
        TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::BackendDidNotReport
        }
    ));

    fixture
        .client
        .send_message(
            &child.instance_stream,
            "child second known usage".to_owned(),
        )
        .await
        .expect("send child known usage follow-up");
    let second_child = expect_completed_turn_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "mock backend response to: child second known usage",
    )
    .await;
    assert_known_turn_usage_with_unavailable_cumulative(
        &second_child.message.token_usage,
        MOCK_TURN_TOKEN_TOTAL,
    );

    let payload = expect_task_token_usage_matching(
        &mut usage_client,
        &root.agent_id,
        "partial descendant aggregate",
        |payload| {
            payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 2
                && matches!(
                    &payload.descendant_usage.status,
                    TaskTokenUsageStatus::Partial {
                        unavailable_count: 1,
                        reasons
                    } if reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
                )
                && payload.breakdown.iter().any(|entry| {
                    entry.agent_id == child.agent_id
                        && matches!(
                            &entry.usage,
                            TaskTokenUsageScope::Partial {
                                usage,
                                unavailable_count: 1,
                                reasons
                            } if usage.total_tokens == MOCK_TURN_TOKEN_TOTAL
                                && reasons == &vec![
                                    TaskTokenUsageUnavailableReason::BackendDidNotReport
                                ]
                        )
                })
        },
    )
    .await;
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
        other => panic!("expected partial descendant usage, got {other:?}"),
    }
    assert_known_task_scope(&payload.self_usage, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(
        payload.descendant_usage.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL
    );
    let child_entry = payload
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("child breakdown entry");
    assert_partial_task_scope(&child_entry.usage, MOCK_TURN_TOKEN_TOTAL, 1);

    fixture
        .client
        .close_agent(&child.instance_stream)
        .await
        .expect("close partial child");
    let closed = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "partial child AgentClosed",
    )
    .await;
    let closed: AgentClosedPayload = closed.parse_payload().expect("AgentClosed payload");
    assert_eq!(closed.agent_id, child.agent_id);

    let after_close = expect_task_token_usage_matching(
        &mut usage_client,
        &root.agent_id,
        "partial aggregate after child close",
        |payload| {
            payload.total.usage.total_tokens == MOCK_TURN_TOKEN_TOTAL * 2
                && payload.breakdown.iter().any(|entry| {
                    entry.agent_id == child.agent_id
                        && matches!(
                            &entry.usage,
                            TaskTokenUsageScope::Partial {
                                usage,
                                unavailable_count: 1,
                                reasons
                            } if usage.total_tokens == MOCK_TURN_TOKEN_TOTAL
                                && reasons == &vec![
                                    TaskTokenUsageUnavailableReason::BackendDidNotReport
                                ]
                        )
                })
        },
    )
    .await;
    assert_eq!(after_close.descendant_count, 1);
    match &after_close.descendant_usage.status {
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
        other => panic!("expected partial descendant usage after close, got {other:?}"),
    }

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let bootstrapped = bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == root.agent_id)
        .expect("HostBootstrap should include partial root task token usage");
    assert_eq!(
        bootstrapped.total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL * 2
    );
    match &bootstrapped.total.status {
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
        other => panic!("expected partial bootstrapped total usage, got {other:?}"),
    }
    let child_entry = bootstrapped
        .breakdown
        .iter()
        .find(|entry| entry.agent_id.eq(&child.agent_id))
        .expect("bootstrapped closed child breakdown entry");
    assert_partial_task_scope(&child_entry.usage, MOCK_TURN_TOKEN_TOTAL, 1);
}

#[tokio::test]
async fn task_token_usage_all_unavailable_omits_split_zeroes() {
    let mut fixture = Fixture::new().await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "unavailable-root",
            MockScript::one(MockTurn::text_without_usage(
                "mock backend response to: root missing usage",
            )),
        )
        .await;
    let root = spawn_token_usage_agent(
        &mut fixture.client,
        "unavailable-root",
        "root missing usage",
    )
    .await;
    drop(reservation);
    let _ = expect_completed_turn_on_stream(
        &mut fixture.client,
        &root.instance_stream,
        "mock backend response to: root missing usage",
    )
    .await;
    let (mut usage_client, _) = fixture.connect_with_bootstrap().await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "unavailable-child",
            MockScript::one(MockTurn::text_without_usage(
                "mock backend response to: child missing usage",
            )),
        )
        .await;
    let child = spawn_user_child(
        &mut fixture.client,
        &root.agent_id,
        "unavailable-child",
        "child missing usage",
        "/tmp/unavailable-child",
    )
    .await;
    drop(reservation);

    let payload = expect_task_token_usage_matching(
        &mut usage_client,
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

/// The basic agent journey end to end: a client error report is accepted
/// up front without disturbing the connection, the spawn publishes
/// NewAgent/AgentStart with user origin and no parent, two turns stream
/// strictly, and closing the agent emits AgentClosed, removes it from the
/// registry, and keeps it out of late-client replays.
#[tokio::test]
async fn agent_lifecycle() {
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;

    assert_eq!(env.kind, FrameKind::NewAgent);
    assert!(env.stream.0.starts_with("/host/"));

    let new_agent: NewAgentPayload = env
        .parse_payload()
        .expect("failed to parse NewAgentPayload");
    assert!(!new_agent.agent_id.0.is_empty());
    assert_eq!(new_agent.backend_kind, BackendKind::Claude);
    assert_eq!(new_agent.name, "test-agent");
    assert_eq!(new_agent.origin, AgentOrigin::User);
    assert_eq!(new_agent.parent_agent_id, None);
    let agent_stream = new_agent.instance_stream.clone();

    let start =
        expect_agent_start_on_stream(&mut fixture.client, &agent_stream, "AgentStart").await;
    assert!(!start.agent_id.0.is_empty());
    assert_eq!(start.backend_kind, BackendKind::Claude);
    assert_eq!(start.name, "test-agent");
    assert_eq!(start.origin, AgentOrigin::User);
    assert_eq!(start.parent_agent_id, None);

    expect_strict_mock_turn(
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
        .send_message(&agent_stream, "follow up".to_owned())
        .await
        .expect("send_message failed");

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent_stream,
        "mock backend response to: follow up",
    )
    .await;

    fixture
        .client
        .close_agent(&agent_stream)
        .await
        .expect("close_agent failed");

    let env = fixture
        .next_frame_matching("AgentClosed", |env| env.kind == FrameKind::AgentClosed)
        .await;
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
async fn agent_recovers_from_backend_errors_without_losing_stream_identity() {
    let mut fixture = Fixture::new().await;
    let agent = fixture.spawn("backend-recovery-agent", "hello").await;

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: hello",
    )
    .await;

    async fn assert_working_with_queue(
        fixture: &mut Fixture,
        agent: &fixture::TestAgent,
        expected_messages: &[&str],
    ) {
        let env = fixture::next_logical_frame_matching_on(
            &mut fixture.client,
            "queued input behind authoritative backend work",
            |env| {
                if env.stream != agent.stream {
                    return false;
                }
                match env.kind {
                    FrameKind::ChatEvent => {
                        let event: ChatEvent = env.parse_payload().expect("parse live event");
                        assert!(
                            !matches!(event, ChatEvent::TypingStatusChanged(false)),
                            "diagnostic events must not fabricate backend idle"
                        );
                        false
                    }
                    FrameKind::AgentActivityChanged => {
                        let state: protocol::AgentActivityChangedPayload =
                            env.parse_payload().expect("parse live activity");
                        assert_eq!(
                            state.activity,
                            protocol::AgentActivity::Thinking,
                            "the agent must remain working until backend idle"
                        );
                        false
                    }
                    FrameKind::QueuedMessages => env
                        .parse_payload::<protocol::QueuedMessagesPayload>()
                        .is_ok_and(|queue| queue.messages.len() == expected_messages.len()),
                    _ => false,
                }
            },
        )
        .await;
        let queue: protocol::QueuedMessagesPayload = env.parse_payload().expect("parse queue");
        assert!(
            queue
                .messages
                .iter()
                .map(|entry| entry.message.as_str())
                .eq(expected_messages.iter().copied()),
            "queued input must retain its delivery order"
        );
        let (mut other, bootstrap) = fixture.connect_with_bootstrap().await;
        let other_agent = bootstrapped_agent(&bootstrap, &agent.new_agent.agent_id);
        let state = expect_raw_agent_bootstrap_on_stream(
            &mut other,
            &other_agent.instance_stream,
            "working agent replay",
        )
        .await;
        assert_eq!(state.activity, protocol::AgentActivity::Thinking);
        assert!(
            state.events.iter().any(|event| matches!(
                event,
                AgentBootstrapEvent::QueuedMessages(queue)
                    if queue.messages.len() == expected_messages.len()
            )),
            "a reconnecting client must see the same pending input"
        );
    }

    // The old fixture expected invented idle after an error card or a failed
    // tool. The documented contract requires backend idle; both scenarios now
    // hold continuing work behind gates and assert that no idle is invented.
    for failed_tool in [true, false] {
        let diagnostic_gate = MockGateHandle::new();
        let idle_gate = MockGateHandle::new();
        let turn = if failed_tool {
            MockTurn::tool_failure_then_text("continued after failed tool", &diagnostic_gate)
        } else {
            MockTurn::error_before_typing(
                "mock backend diagnostic before typing",
                "continued after error card",
                &diagnostic_gate,
            )
        }
        .with_gate_before_idle(&idle_gate);
        fixture
            .mock(&agent)
            .await
            .enqueue_all([turn, MockTurn::echo(), MockTurn::echo()])
            .await;
        fixture
            .client
            .send_message(&agent.stream, "trigger diagnostic".to_owned())
            .await
            .expect("send diagnostic trigger");
        diagnostic_gate.wait_until_entered().await;
        let event = fixture
            .next_chat_event_matching(&agent, "backend diagnostic", |event| {
                if failed_tool {
                    matches!(event, ChatEvent::ToolExecutionCompleted(_))
                } else {
                    matches!(event, ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Error))
                }
            })
            .await;
        if let ChatEvent::ToolExecutionCompleted(completion) = event {
            assert!(fixture::tool_completion_failed(&completion));
            assert!(
                matches!(&completion.outcome,
                ToolExecutionOutcome::Failed { details: Some(details), .. }
                    if details.contains("history did not contain a tool_result")),
                "the regression must exercise the misleading diagnostic phrase"
            );
        }
        fixture
            .client
            .send_message(&agent.stream, "first queued input".to_owned())
            .await
            .expect("queue first follow-up");
        assert_working_with_queue(&mut fixture, &agent, &["first queued input"]).await;

        diagnostic_gate.release_one();
        idle_gate.wait_until_entered().await;
        let expected_reply = if failed_tool {
            "continued after failed tool"
        } else {
            "continued after error card"
        };
        fixture
            .next_chat_event_matching(&agent, "continuing backend response", |event| {
                assert!(
                    !matches!(event, ChatEvent::TypingStatusChanged(false)),
                    "a failed tool or an error card cannot end continuing work"
                );
                matches!(event, ChatEvent::StreamEnd(end) if end.message.content == expected_reply)
            })
            .await;
        fixture
            .client
            .send_message(&agent.stream, "second queued input".to_owned())
            .await
            .expect("queue follow-up after model response");
        assert_working_with_queue(
            &mut fixture,
            &agent,
            &["first queued input", "second queued input"],
        )
        .await;
        idle_gate.release_one();
        let event = fixture
            .next_chat_event_matching(&agent, "backend idle", |_| true)
            .await;
        assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
        for reply in ["first queued input", "second queued input"] {
            expect_strict_mock_turn(
                &mut fixture.client,
                &agent.stream,
                &format!("mock backend response to: {reply}"),
            )
            .await;
        }
    }

    let idle_gate = MockGateHandle::new();
    fixture
        .mock(&agent)
        .await
        .enqueue_all([
            MockTurn::cancelled("mock cancellation").with_gate_before_idle(&idle_gate),
            MockTurn::echo(),
            MockTurn::text_with_mid_turn_error(
                "mock backend response to: mid-turn error",
                "mock mid-turn diagnostic error",
            ),
            MockTurn::echo(),
        ])
        .await;
    fixture
        .client
        .send_message(&agent.stream, "trigger cancellation".to_owned())
        .await
        .expect("send cancellation trigger");
    idle_gate.wait_until_entered().await;
    fixture
        .next_chat_event_matching(&agent, "cancellation before backend idle", |event| {
            matches!(event, ChatEvent::OperationCancelled(_))
        })
        .await;
    fixture
        .client
        .send_message(&agent.stream, "after cancellation".to_owned())
        .await
        .expect("queue cancellation follow-up");
    assert_working_with_queue(&mut fixture, &agent, &["after cancellation"]).await;
    idle_gate.release_one();
    let event = fixture
        .next_chat_event_matching(&agent, "cancelled backend idle", |_| true)
        .await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));
    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: after cancellation",
    )
    .await;

    fixture
        .client
        .send_message(&agent.stream, "trigger mid-turn error".to_owned())
        .await
        .expect("send mid-turn error trigger failed");
    let event = fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |_| true)
        .await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));
    let event = fixture
        .next_chat_event_matching(&agent, "StreamStart", |_| true)
        .await;
    assert!(matches!(event, ChatEvent::StreamStart(..)));
    let event = fixture
        .next_chat_event_matching(&agent, "StreamDelta", |_| true)
        .await;
    assert!(matches!(event, ChatEvent::StreamDelta(..)));
    let event = fixture
        .next_chat_event_matching(&agent, "MessageAdded(Error)", |event| {
            matches!(
                event,
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Error)
            )
        })
        .await;
    let ChatEvent::MessageAdded(message) = event else {
        unreachable!("matched MessageAdded above");
    };
    assert!(
        message.content.contains("mock mid-turn diagnostic error"),
        "unexpected error message on {}: {}",
        agent.stream,
        message.content
    );
    let event = fixture
        .next_chat_event_matching(&agent, "StreamEnd", |_| true)
        .await;
    assert!(
        matches!(event, ChatEvent::StreamEnd(..)),
        "mid-turn error must not displace the turn's StreamEnd on {}, got {event:?}",
        agent.stream
    );
    let event = fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(false)", |_| true)
        .await;
    assert!(
        matches!(event, ChatEvent::TypingStatusChanged(false)),
        "turn must end via the backend idle marker on {}, got {event:?}",
        agent.stream
    );

    fixture
        .client
        .send_message(&agent.stream, "after mid-turn error".to_owned())
        .await
        .expect("send mid-turn follow-up failed");
    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: after mid-turn error",
    )
    .await;
}

#[tokio::test]
async fn agent_requeues_message_when_backend_is_busy_with_self_started_turn() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "busy-self-turn-agent",
            MockScript::one(MockTurn::text("mock backend response to: hello"))
                .with_unbounded_echo()
                .with_busy_self_turn_once(),
        )
        .await;

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: hello",
    )
    .await;

    let message = "follow up".to_owned();
    fixture
        .client
        .send_message(&agent.stream, message.clone())
        .await
        .expect("send busy-time message failed");

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: self-initiated wakeup",
    )
    .await;

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        &format!("mock backend response to: {message}"),
    )
    .await;
}

#[tokio::test]
async fn close_agent_recursively_closes_descendants_first() {
    let mut fixture = Fixture::new().await;

    let (parent_new, _parent_start, relay_child_new, _relay_child_start) =
        spawn_parent_with_native_child(&mut fixture, false).await;
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

/// Waits for the next `NewAgent`, setting aside unrelated frames.
///
/// A held turn keeps emitting on the parent's stream, so a spawn's `NewAgent`
/// is not necessarily the next frame on the connection.
async fn expect_new_agent(client: &mut client::Connection, context: &str) -> NewAgentPayload {
    let host_stream = single_host_stream(client);
    loop {
        if let Some(env) = pop_pending_agent_event(client, &host_stream, FrameKind::NewAgent) {
            return env.parse_payload().expect("parse pending NewAgent");
        }
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::NewAgent {
            return env.parse_payload().expect("parse NewAgent");
        }
        push_pending_agent_event(client, env);
    }
}

/// Spawns a child whose turn stays open until it is interrupted.
///
/// [`spawn_user_child`] waits for the child's turn to complete, so it cannot
/// produce the busy child this test needs.
async fn spawn_busy_user_child(
    fixture: &mut Fixture,
    parent_agent_id: &protocol::AgentId,
    name: &str,
    workspace_root: &str,
) -> NewAgentPayload {
    let reservation = fixture
        .reserve_next_mock_launch(
            name,
            MockScript::one(MockTurn::held_text(format!("{name} working"))),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace_root.to_owned()],
                prompt: format!("{name} working"),
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

    let child_new = expect_new_agent(&mut fixture.client, &format!("{name} NewAgent")).await;
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        &format!("{name} start"),
    )
    .await;
    drop(reservation);

    let env = expect_chat_event_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        &format!("{name} TypingStatusChanged(true)"),
    )
    .await;
    let event: ChatEvent = env.parse_payload().expect("parse busy child typing true");
    assert!(
        matches!(event, ChatEvent::TypingStatusChanged(true)),
        "{name} must be mid-turn before the close"
    );

    child_new
}

/// Closing a parent whose sub-agents are still running must actually close it.
///
/// This is the shape of a real wedge: an orchestrator held a turn open while
/// awaiting its children, so the close waited on an idle transition that could
/// not arrive. Nothing was removed from the registry, the agent stayed in the
/// Agents tab, and every later close was equally inert.
///
/// [`close_agent_recursively_closes_descendants_first`] covers the same
/// traversal with idle agents, which is why it never caught this: the deferred
/// close always resolved because those turns ended on their own. Every agent
/// here is deliberately mid-turn.
#[tokio::test]
async fn close_agent_with_busy_subagents_completes_and_removes_the_subtree() {
    let mut fixture = Fixture::new().await;

    let parent_reservation = fixture
        .reserve_next_mock_launch(
            "busy-close-parent",
            MockScript::one(MockTurn::held_text("parent awaiting children")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("busy-close-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/busy-close-parent".to_owned()],
                prompt: "parent awaiting children".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn busy-close parent failed");

    let parent_new = expect_new_agent(&mut fixture.client, "busy-close parent NewAgent").await;
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "busy-close parent start",
    )
    .await;
    drop(parent_reservation);
    let env = expect_chat_event_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "busy-close parent TypingStatusChanged(true)",
    )
    .await;
    let event: ChatEvent = env.parse_payload().expect("parse parent typing true");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    let first_child = spawn_busy_user_child(
        &mut fixture,
        &parent_new.agent_id,
        "busy-close-child-one",
        "/tmp/busy-close-child-one",
    )
    .await;
    let second_child = spawn_busy_user_child(
        &mut fixture,
        &parent_new.agent_id,
        "busy-close-child-two",
        "/tmp/busy-close-child-two",
    )
    .await;

    let expected_closed = [
        &parent_new.agent_id,
        &first_child.agent_id,
        &second_child.agent_id,
    ];
    let ids_before_close = fixture.agent_ids().await;
    for expected in expected_closed {
        assert!(
            ids_before_close.contains(expected),
            "agent {expected} should be registered before close"
        );
    }

    fixture
        .client
        .close_agent(&parent_new.instance_stream)
        .await
        .expect("close busy parent failed");

    // `expect_next_event` bounds each wait, so a close that defers to a turn
    // that never ends fails here instead of hanging the suite.
    let mut closed = Vec::new();
    loop {
        let env = expect_next_event(&mut fixture.client, "busy-close AgentClosed").await;
        if env.kind != FrameKind::AgentClosed {
            continue;
        }
        let payload: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
        let is_parent = payload.agent_id == parent_new.agent_id;
        closed.push(payload.agent_id);
        if is_parent {
            break;
        }
    }
    for expected in expected_closed {
        assert!(
            closed.contains(expected),
            "busy agent {expected} must be closed with the subtree: {closed:?}"
        );
    }

    // The registry removal and session-list refresh run after the close loop,
    // so a deferred close left every one of these agents visible in the UI with
    // no way to dismiss them.
    let ids_after_close = fixture.agent_ids().await;
    for expected in expected_closed {
        assert!(
            !ids_after_close.contains(expected),
            "busy agent {expected} must be removed from the registry after close"
        );
    }

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    for expected in expected_closed {
        assert!(
            !bootstrap
                .agents
                .iter()
                .any(|agent| &agent.agent_id == expected),
            "closed busy agent {expected} must not replay to late clients"
        );
    }
}

#[tokio::test]
async fn agent_control_spawn_is_rejected_while_the_parent_is_closing() {
    let mut fixture = Fixture::new().await;
    let shutdown_gate = MockGateHandle::new();
    let parent = fixture
        .spawn_scripted(
            "closing-spawn-parent",
            // The shutdown gate holds the closing window; ignoring interrupt
            // would instead wait the entire native close grace before this gate.
            MockScript::one(MockTurn::held_text(
                "mock backend held response to: parent never settles",
            ))
            .with_shutdown_gate(&shutdown_gate),
        )
        .await;
    let event = fixture
        .next_chat_event_matching(
            &parent,
            "closing-spawn parent TypingStatusChanged(true)",
            |_| true,
        )
        .await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    let caller = fixture
        .agent_control_caller(&parent.new_agent.agent_id)
        .await;

    fixture
        .client
        .close_agent(&parent.stream)
        .await
        .expect("close closing-spawn parent failed");
    (shutdown_gate.wait_until_entered()).await;

    let error = mcp_spawn_agent_error_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/closing-spawn-child"],
            // The spawn is refused while the parent is closing, so this prompt
            // never reaches a backend; it needs no scripted behavior.
            "prompt": "child of a closing parent",
            "backend_kind": "claude",
            "name": "closing-spawn-child"
        }),
    )
    .await;
    assert!(
        error.contains("closing"),
        "a spawn under a closing parent must say why it was refused, got: {error}"
    );
    assert_eq!(fixture.agent_ids().await.len(), 1);
    shutdown_gate.release_one();
    fixture
        .next_frame_matching("closing parent teardown completes without a child", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|closed| closed.agent_id == parent.new_agent.agent_id)
        })
        .await;
    assert!(fixture.agent_ids().await.is_empty());
}

#[tokio::test]
async fn close_agent_mid_turn_flushes_final_events_before_agent_closed() {
    let mut fixture = Fixture::new().await;
    let gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "close-mid-turn",
            MockScript::one(MockTurn::gated_text("close mid turn response", &gate)),
        )
        .await;

    let event = fixture
        .next_chat_event_matching(&agent, "TypingStatusChanged(true)", |_| true)
        .await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    gate.wait_until_entered().await;

    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close_agent failed");
    gate.release_one();

    let mut saw_stream_start = false;
    let mut saw_stream_delta = false;
    let mut saw_stream_end = false;
    let mut saw_typing_false = false;

    let env = fixture
        .next_frame_matching("close-mid-turn AgentClosed", |env| {
            if env.kind == FrameKind::ChatEvent && env.stream == agent.stream {
                let event: ChatEvent = env.parse_payload().expect("parse close-mid-turn ChatEvent");
                match event {
                    ChatEvent::StreamStart(_) => saw_stream_start = true,
                    ChatEvent::StreamDelta(_) => saw_stream_delta = true,
                    ChatEvent::StreamEnd(_) => saw_stream_end = true,
                    ChatEvent::TypingStatusChanged(false) => saw_typing_false = true,
                    _ => {}
                }
            }
            env.kind == FrameKind::AgentClosed
        })
        .await;
    let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
    assert_eq!(closed.agent_id, agent.new_agent.agent_id);

    assert!(saw_stream_start, "close should not drop StreamStart");
    assert!(saw_stream_delta, "close should not drop StreamDelta");
    assert!(saw_stream_end, "close should not drop StreamEnd");
    assert!(
        saw_typing_false,
        "close should not drop TypingStatusChanged(false)"
    );
}

/// A close runs the backend's teardown to completion; it is never put on a clock.
///
/// Teardown used to run under a 3s `BACKEND_SHUTDOWN_TIMEOUT`. Cancelling a
/// shutdown future mid-flight abandons whatever release work it had left, and
/// the timeout could not distinguish a backend that was wedged from one that was
/// simply still working — so it fired on healthy closes and reported an error
/// for them. Park the backend inside `shutdown` and the close must not finish:
/// the only thing that may end that wait is the backend actually returning.
///
/// The 4s window is deliberately longer than the timeout that used to be here,
/// so reintroducing any clock around backend shutdown turns this red.
#[tokio::test]
async fn close_agent_waits_for_backend_shutdown_instead_of_timing_it_out() {
    let mut fixture = Fixture::new().await;
    let shutdown_gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "close-waits-for-shutdown",
            MockScript::one(MockTurn::text("close waits for shutdown"))
                .with_shutdown_gate(&shutdown_gate),
        )
        .await;

    let event = fixture
        .next_chat_event_matching(&agent, "opening turn settles", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));

    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close_agent failed");

    // Happens-before edge: the backend is now parked inside `shutdown`, so any
    // AgentClosed after this point would mean the close abandoned teardown.
    shutdown_gate.wait_until_entered().await;

    fixture::assert_no_interesting_frame_on(
        &mut fixture.client,
        Duration::from_secs(4),
        "AgentClosed while the backend is still shutting down",
        |env| env.kind != FrameKind::AgentClosed,
    )
    .await;

    shutdown_gate.release_one();

    let env = fixture
        .next_frame_matching("close-waits-for-shutdown AgentClosed", |env| {
            env.kind == FrameKind::AgentClosed
        })
        .await;
    let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
    assert_eq!(closed.agent_id, agent.new_agent.agent_id);
}

#[tokio::test]
async fn agent_control_end_to_end_flow_uses_full_stack() {
    let mut fixture = Fixture::new().await;
    let write_id = fixture
        .client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Claude],
            Vec::<BackendKind>::new(),
        )
        .await
        .expect("enable Claude for launch-profile catalog");
    expect_settings_write_applied(
        &mut fixture.client,
        &write_id,
        "enable Claude for launch-profile catalog",
    )
    .await;
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
    assert_eq!(
        options.launch_profile_preference,
        settings_model::default_delegation_launch_profile_order()
    );
    assert!(options.backend_limits.is_empty());

    let live_preference = vec![
        LaunchProfileId("missing:dev-driver".to_owned()),
        LaunchProfileId("claude:default".to_owned()),
    ];
    let write_id = fixture
        .client
        .replace_setting(
            "/delegation_launch_profile_order",
            live_preference.clone(),
            settings_model::default_delegation_launch_profile_order(),
        )
        .await
        .expect("update live dev-driver preference");
    expect_settings_write_applied(
        &mut fixture.client,
        &write_id,
        "update live dev-driver preference",
    )
    .await;
    (async {
        loop {
            if control.list_launch_options().launch_profile_preference == live_preference {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

    fixture
        .host_for_test()
        .record_backend_capacity_for_test(
            BackendKind::Claude,
            protocol::BackendCapacityState::Known {
                report: protocol::CapacityReport {
                    source: protocol::CapacitySource::ClaudeRateLimitEvent,
                    observed_at_ms: Some(789),
                    plan: None,
                    buckets: Vec::new(),
                    coverage: protocol::CapacityCoverage::RepresentativeBucketOnly,
                },
            },
        )
        .await;
    (async {
        loop {
            let options = control.list_launch_options();
            if options.backend_limits.iter().any(|limits| {
                limits.backend_kind == BackendKind::Claude
                    && limits.report.observed_at_ms == Some(789)
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

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
    let root = control
        .read_agent_debug(
            protocol::AgentId(spawned.agent_id.clone()),
            None,
            None,
            None,
        )
        .await
        .expect("read root tool configuration");
    assert!(
        root.events
            .iter()
            .all(|event| !chat_event_contains(event, "excluded_tool_categories")),
        "a root keeps its user-question tools"
    );
    let child = control
        .spawn_agent(SpawnRequest {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: "Report inherited tool policy".to_owned(),
            backend_kind: BackendKind::Claude,
            launch_profile_id: Some(LaunchProfileId("claude:default".to_owned())),
            session_settings: None,
            parent_agent_id: Some(protocol::AgentId(spawned.agent_id.clone())),
            project_id: None,
            name: Some("policy-child".to_owned()),
            cost_hint: None,
            access_mode: Default::default(),
        })
        .await
        .expect("spawn child with server-owned tool policy");
    await_dev_driver_agent_ready(&control, &child.agent_id, "child policy turn").await;
    assert_read_agent_contains(
        &control,
        &child.agent_id,
        None,
        "[excluded_tool_categories: [AskUser]]",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_limits_spawn_selection_to_profile_backend_and_tier() {
    eprintln!("MCP LAUNCH ROUTING phase=fixture-start");
    // Mock execution does not supply dynamic discovery. Without this fixture
    // catalog Muse fails startup before the reserved turn can run.
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        mock_backend_discovery: [(
            BackendKind::Muse,
            server::backend::mock::MockDiscovery::new(vec![Ok(
                server::backend::BackendDiscovery {
                    schema: protocol::SessionSettingsSchema {
                        backend_kind: BackendKind::Muse,
                        fields: Vec::new(),
                        model_resolutions: Default::default(),
                    },
                    launch_profiles: Vec::new(),
                },
            )]),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    })
    .await;
    eprintln!("MCP LAUNCH ROUTING phase=fixture-ready");
    let write_id = fixture
        .client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Claude],
            Vec::<BackendKind>::new(),
        )
        .await
        .expect("enable Claude");
    expect_settings_write_applied(&mut fixture.client, &write_id, "enable Claude").await;

    let parent = spawn_agent_control_parent(&mut fixture, "launch-profile-parent").await;
    eprintln!("MCP LAUNCH ROUTING phase=parent-ready");
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let base_url = fixture.agent_control_http_url().await;
    let options = mcp_list_launch_options(&caller).await;
    let enabled_backends = options["enabled_backends"]
        .as_array()
        .expect("enabled backend list");
    assert_eq!(enabled_backends, &[json!("claude")]);
    assert_eq!(options["complexity_tiers_enabled"], false);
    assert_eq!(
        options["launch_profile_preference"],
        json!([
            "codex:default",
            "claude:default",
            "antigravity:default",
            "hermes:default"
        ])
    );
    assert_eq!(options["backend_limits"], json!([]));
    assert!(
        options["launch_profiles"]
            .as_array()
            .is_some_and(|profiles| {
                profiles.iter().any(|profile| {
                    profile["id"] == "claude:default" && profile["backend_kind"] == "claude"
                }) && profiles.iter().all(|profile| {
                    profile.get("session_settings").is_none()
                        && profile.get("model").is_none()
                        && profile.get("reasoning_effort").is_none()
                })
            })
    );
    assert!(
        options.get("catalog").is_none(),
        "catalog leaked in {options}"
    );
    assert!(
        options.get("session_schemas").is_none(),
        "session schemas leaked in {options}"
    );

    let anonymous = post_json(
        &base_url,
        &json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": { "name": "tyde_list_launch_options", "arguments": {} }
        }),
    )
    .await;
    assert!(mcp_result_is_error(&anonymous));
    assert!(mcp_result_text(&anonymous).contains("bearer credential"));

    let preference_write_id = fixture
        .client
        .replace_setting(
            "/delegation_launch_profile_order",
            vec![
                LaunchProfileId("missing:saved".to_owned()),
                LaunchProfileId("claude:default".to_owned()),
            ],
            settings_model::default_delegation_launch_profile_order(),
        )
        .await
        .expect("replace delegation preference");
    expect_settings_write_applied(
        &mut fixture.client,
        &preference_write_id,
        "replace delegation preference",
    )
    .await;
    fixture
        .host_for_test()
        .record_backend_capacity_for_test(
            BackendKind::Claude,
            protocol::BackendCapacityState::Known {
                report: protocol::CapacityReport {
                    source: protocol::CapacitySource::ClaudeRateLimitEvent,
                    observed_at_ms: Some(123_456),
                    plan: None,
                    buckets: vec![protocol::CapacityBucket {
                        id: protocol::CapacityBucketId::Claude {
                            limit: protocol::ClaudeLimitType::FiveHour,
                        },
                        label: "session limit".to_owned(),
                        measure: protocol::CapacityMeasure::UsedPercent {
                            used_percent: 25,
                            remaining_percent: 75,
                            provenance: protocol::ValueProvenance {
                                vendor_reported: true,
                            },
                        },
                        scope: protocol::CapacityScope::NotReported,
                        window: protocol::CapacityWindow::NotReported,
                        reset: protocol::CapacityReset::NotReported,
                        status: Some(protocol::CapacityBucketStatus::Allowed),
                    }],
                    coverage: protocol::CapacityCoverage::RepresentativeBucketOnly,
                },
            },
        )
        .await;
    let live_options = mcp_list_launch_options(&caller).await;
    assert_eq!(
        live_options["launch_profile_preference"],
        json!(["missing:saved", "claude:default"]),
        "the already-running caller must see the saved order, including a missing id"
    );
    assert_eq!(live_options["backend_limits"].as_array().unwrap().len(), 1);
    let limits = &live_options["backend_limits"][0];
    assert_eq!(limits["backend_kind"], "claude");
    assert_eq!(limits["report"]["observed_at_ms"], 123_456);
    assert_eq!(
        limits["report"]["buckets"][0]["measure"]["remaining_percent"],
        75
    );
    assert!(limits.get("state").is_none());
    assert!(limits.get("freshness").is_none());
    assert!(limits.get("stale_since_ms").is_none());

    fixture
        .host_for_test()
        .mark_backend_capacity_stale_for_test(BackendKind::Claude)
        .await;
    let stale_options = mcp_list_launch_options(&caller).await;
    assert_eq!(
        stale_options["backend_limits"][0]["report"],
        limits["report"]
    );
    assert!(stale_options["backend_limits"][0].get("stale").is_none());

    let tools_response = post_json_with_headers(
        &base_url,
        &[("Authorization", &caller.authorization)],
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
    )
    .await;
    let spawn_tool = tools_response["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .find(|tool| tool["name"] == "tyde_spawn_agent")
        .expect("tyde_spawn_agent tool");
    let properties = spawn_tool["inputSchema"]["properties"]
        .as_object()
        .expect("spawn input properties");
    assert!(properties.contains_key("backend_kind"));
    assert!(
        properties["backend_kind"]
            .to_string()
            .contains("#/$defs/BackendKindInput"),
        "the advertised backend selector must reference the typed backend enum"
    );
    assert!(
        spawn_tool["inputSchema"]["$defs"]["BackendKindInput"]["enum"]
            .as_array()
            .expect("advertised backend choices")
            .contains(&json!("muse")),
        "MCP callers must be offered Muse, not forced to select another provider"
    );
    assert!(properties.contains_key("launch_profile_id"));
    assert!(!properties.contains_key("session_settings"));
    assert!(!properties.contains_key("cost_hint"));
    eprintln!("MCP LAUNCH ROUTING phase=spawn-schema-verified");

    let agent_ids_before = fixture.agent_ids().await;
    let response = mcp_tool_call_as(
        &caller,
        false,
        "tyde_spawn_agent",
        json!({
            "workspace_roots": ["/tmp/agent-control-forbidden-selection"],
            "prompt": "must not spawn",
            "backend_kind": "claude",
            "session_settings": { "model": { "string": "haiku" } }
        }),
    )
    .await;
    let rejected = response.get("error").is_some()
        || response
            .get("result")
            .and_then(|result| result.get("isError").or_else(|| result.get("is_error")))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    assert!(
        rejected
            && (response.to_string().contains("unknown field")
                || response.to_string().contains("invalid")),
        "forbidden spawn selection must be rejected: {response}"
    );
    assert_eq!(fixture.agent_ids().await, agent_ids_before);

    let write_id = fixture
        .client
        .replace_setting("/complexity_tiers_enabled", true, false)
        .await
        .expect("enable complexity tiers");
    expect_settings_write_applied(
        &mut fixture.client,
        &write_id,
        "enable agent-control complexity tiers",
    )
    .await;
    let tier_options = mcp_list_launch_options(&caller).await;
    assert_eq!(tier_options["complexity_tiers_enabled"], true);

    let tier_tools_response = post_json_with_headers(
        &base_url,
        &[("Authorization", &caller.authorization)],
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    let tier_spawn_tool = tier_tools_response["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .find(|tool| tool["name"] == "tyde_spawn_agent")
        .expect("tyde_spawn_agent tool");
    assert!(
        tier_spawn_tool["inputSchema"]["properties"]
            .as_object()
            .expect("spawn input properties")
            .contains_key("cost_hint")
    );

    eprintln!("MCP LAUNCH ROUTING phase=tier-child-spawn");
    let agent_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/agent-control-backend-tier"],
            "prompt": "agent control profile and tier",
            "launch_profile_id": "claude:default",
            "cost_hint": "low",
            "name": "profile-tier child"
        }),
    )
    .await;
    let awaited = mcp_await_agent(&caller, &agent_id).await;
    assert_await_result_ready(&awaited, &agent_id, "idle");
    eprintln!("MCP LAUNCH ROUTING phase=tier-child-idle");

    let config_url = fixture.config_mcp_http_url().await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-enable");
    let enabled = config_agent_tool(
        &config_url,
        "tyde_config_set_setting",
        json!({
            "setting": {
                "setting": "enabled_backends",
                "enabled_backends": ["claude", "muse"]
            }
        }),
    )
    .await;
    assert!(!mcp_result_is_error(&enabled));
    eprintln!("MCP LAUNCH ROUTING phase=muse-settings-accepted");
    let mut received_settings = false;
    let mut received_catalog = false;
    let mut received_muse_schema = false;
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "MCP-enabled Muse settings, schema and launch catalog",
        |env| {
            if env.kind == FrameKind::SessionSchemas {
                let payload: protocol::SessionSchemasPayload = env.parse_payload().expect("session schemas");
                for schema in payload.schemas {
                    match schema {
                        protocol::SessionSchemaEntry::Ready { schema } if schema.backend_kind == BackendKind::Muse => {
                            eprintln!("MCP LAUNCH ROUTING phase=muse-schema state=ready");
                            received_muse_schema = true;
                        },
                        protocol::SessionSchemaEntry::Pending { backend_kind: BackendKind::Muse } => {
                            eprintln!("MCP LAUNCH ROUTING phase=muse-schema state=pending");
                        },
                        protocol::SessionSchemaEntry::Unavailable { backend_kind: BackendKind::Muse, .. } => {
                            panic!("MCP LAUNCH ROUTING phase=muse-schema state=unavailable: simulation discovery must be configured");
                        },
                        _ => {},
                    }
                }
            }
            received_settings |= env.kind == FrameKind::HostSettings
                && env
                    .parse_payload::<HostSettingsPayload>()
                    .is_ok_and(|payload| {
                        payload.settings.enabled_backends
                            == vec![BackendKind::Claude, BackendKind::Muse]
                    });
            received_catalog |= env.kind == FrameKind::LaunchProfileCatalogNotify
                && env.parse_payload::<LaunchProfileCatalogPayload>().is_ok_and(|payload| {
                    payload.catalog.entries.iter().any(|entry| {
                        matches!(entry, protocol::LaunchProfileEntry::Ready { profile }
                            if profile.id.0 == "muse:default" && profile.backend_kind == BackendKind::Muse)
                    })
                });
            received_settings && received_catalog && received_muse_schema
        },
    )
    .await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-settings-schema-and-catalog-observed");
    let options = mcp_list_launch_options(&caller).await;
    assert!(
        options["launch_profiles"]
            .as_array()
            .expect("current MCP launch choices")
            .iter()
            .any(|profile| profile["id"] == "muse:default" && profile["backend_kind"] == "muse")
    );

    let gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "mcp-muse-child",
            MockScript::one(MockTurn::text_after_gate("Muse-routed mock child", &gate)),
        )
        .await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-child-spawn");
    let child_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/agent-control-muse"],
            "prompt": "exercise explicit Muse routing",
            "backend_kind": "muse",
            "name": "mcp-muse-child"
        }),
    )
    .await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-child-spawn-accepted");
    drop(reservation);
    let child =
        expect_replayed_new_agent(&mut fixture.client, &child_id, "MCP Muse child NewAgent").await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-child-advertised");
    assert_eq!(child.backend_kind, BackendKind::Muse);
    assert_eq!(child.parent_agent_id.as_ref(), Some(&parent.agent_id));
    assert_eq!(child.origin, AgentOrigin::AgentControl);
    assert!(
        child.launch_profile_id.is_none(),
        "an explicit backend-only launch must stay backend-only"
    );
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "MCP Muse child AgentStart",
    )
    .await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-child-start-observed");
    assert_eq!(start.backend_kind, BackendKind::Muse);
    assert_eq!(start.parent_agent_id.as_ref(), Some(&parent.agent_id));
    assert_eq!(start.launch_profile_id, child.launch_profile_id);
    gate.release_one();
    eprintln!("MCP LAUNCH ROUTING phase=muse-script-turn-wait");
    expect_strict_mock_turn(
        &mut fixture.client,
        &child.instance_stream,
        "Muse-routed mock child",
    )
    .await;
    eprintln!("MCP LAUNCH ROUTING phase=muse-script-turn-complete");
    let awaited = mcp_await_agent(&caller, &child_id).await;
    assert_await_result_ready(&awaited, &child_id, "idle");
    eprintln!("MCP LAUNCH ROUTING phase=muse-child-idle");
}

/// An explicitly configured Hermes launch profile becomes Ready after the
/// schema refresh and remains available to the typed dev-driver surface.
#[tokio::test]
async fn explicit_hermes_launch_profile_spawns_on_dev_driver_surface() {
    let mut fixture = Fixture::new().await;
    let profile_write_id = fixture
        .client
        .replace_setting(
            "/launch_profiles/hermes:claude",
            hermes_claude_launch_profile(),
            serde_json::Value::Null,
        )
        .await
        .expect("configure explicit Hermes launch profile");
    expect_settings_write_applied(
        &mut fixture.client,
        &profile_write_id,
        "configure explicit Hermes launch profile",
    )
    .await;
    fixture
        .client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Hermes],
            Vec::<BackendKind>::new(),
        )
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

    let ids_before_dev_driver_spawn = fixture.agent_ids().await;
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
    assert!(
        !ids_before_dev_driver_spawn.contains(&protocol::AgentId(spawned.agent_id.clone())),
        "dev-driver spawn must create a new agent, not reuse an existing ready one"
    );
    assert_eq!(spawned.name, "explicit-hermes-dev-driver");
    let dev_driver_agent = control
        .list_agents()
        .await
        .into_iter()
        .find(|agent| agent.agent_id == spawned.agent_id)
        .expect("dev-driver spawn missing from agent list");
    assert_eq!(dev_driver_agent.name, "explicit-hermes-dev-driver");
    assert_eq!(dev_driver_agent.backend_kind, BackendKind::Hermes);
    assert_eq!(
        dev_driver_agent.workspace_roots,
        vec!["/tmp/agent-control-hermes-dev-driver".to_owned()]
    );

    await_dev_driver_agent_ready(
        &control,
        &spawned.agent_id,
        "explicit Hermes launch-profile turn",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_await_returns_while_exit_plan_mode_is_pending() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "plan-await-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;

    let startup_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "await-exit-plan-mode",
            MockScript::one(MockTurn::text_after_gate("plan agent ready", &startup_gate))
                .then(MockTurn::exit_plan_request("epm-await", "# mock plan"))
                .then(MockTurn::text("mock ExitPlanMode approved"))
                .then(MockTurn::text("follow-up response after plan approval")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("await-exit-plan-mode".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-exit-plan-mode".to_owned()],
                prompt: "initialize plan agent".to_owned(),
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
    drop(reservation);

    // Bootstrap replays pending requests but not their transient typing edge.
    // Subscribe before requesting the plan so this live-pause assertion is valid.
    startup_gate.wait_until_entered().await;
    eprintln!("EXIT PLAN AWAIT FIXTURE subscribed before requesting approval");
    startup_gate.release_one();
    expect_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "plan agent ready",
    )
    .await;
    fixture
        .client
        .send_message(
            &new_agent.instance_stream,
            "request plan approval".to_owned(),
        )
        .await
        .expect("request plan approval after subscription");

    let request =
        wait_for_exit_plan_mode_pause_on_stream(&mut fixture.client, &new_agent.instance_stream)
            .await;

    let pending_await = (mcp_await_agent(&caller, &new_agent.agent_id)).await;
    assert_await_result_ready(&pending_await, &new_agent.agent_id, "awaiting_user");

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
                if fixture::tool_completion_succeeded(&completion) =>
            {
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

    // The protocol write and HTTP await are independent connections: a sent
    // approval can still be pending when HTTP observes it. Gate the idle
    // assertion on the approval's actual completion, not write admission.
    eprintln!("EXIT PLAN AWAIT approval completion observed before idle query");
    let resumed_await = (mcp_await_agent(&caller, &new_agent.agent_id)).await;
    assert_await_result_ready(&resumed_await, &new_agent.agent_id, "idle");

    fixture
        .client
        .send_message(&new_agent.instance_stream, "after plan approval".to_owned())
        .await
        .expect("send follow-up after plan approval");
    expect_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "follow-up response after plan approval",
    )
    .await;
}

#[tokio::test]
async fn agent_control_http_await_stays_active_after_exit_plan_mode_approval() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "resuming-await-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;

    let startup_gate = MockGateHandle::new();
    let gate_before_completion = MockGateHandle::new();
    let gate_after_completion = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "await-exit-plan-mode-resume",
            MockScript::one(MockTurn::text_after_gate("plan agent ready", &startup_gate))
                .then(MockTurn::exit_plan_request_stream_end_first(
                    "mock-exit-plan-tool",
                    "# Plan\n\nApprove the mock plan.",
                    &gate_before_completion,
                    &gate_after_completion,
                ))
                .then(MockTurn::text("mock ExitPlanMode approved")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("await-exit-plan-mode-resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-exit-plan-mode-resume".to_owned()],
                prompt: "initialize plan agent".to_owned(),
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
    drop(reservation);

    // gated_text blocks after StreamEnd, allowing bootstrap to replay the turn
    // before this live-order assertion. Hold before all text events instead.
    startup_gate.wait_until_entered().await;
    eprintln!("EXIT PLAN FIXTURE attached before releasing startup text events");
    startup_gate.release_one();
    expect_turn_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "plan agent ready",
    )
    .await;
    fixture
        .client
        .send_message(
            &new_agent.instance_stream,
            "request plan approval".to_owned(),
        )
        .await
        .expect("request plan approval after subscription");

    let request =
        wait_for_exit_plan_mode_pause_on_stream(&mut fixture.client, &new_agent.instance_stream)
            .await;

    let pending_await = (mcp_await_agent(&caller, &new_agent.agent_id)).await;
    assert_await_result_ready(&pending_await, &new_agent.agent_id, "awaiting_user");

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

    wait_for_agent_control_status(&caller, &new_agent.agent_id, "thinking").await;
    gate_before_completion.wait_until_entered().await;
    gate_before_completion.release_one();

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
            && fixture::tool_completion_succeeded(&completion)
        {
            saw_completion = true;
        }
    }
    gate_after_completion.wait_until_entered().await;

    wait_for_agent_control_status(&caller, &new_agent.agent_id, "thinking").await;

    let await_while_resuming = tokio::time::timeout(
        Duration::from_millis(150),
        mcp_await_agent(&caller, &new_agent.agent_id),
    )
    .await;
    assert!(
        await_while_resuming.is_err(),
        "tyde_await_agents must not report ready between plan approval completion and resumed turn finish"
    );
    gate_after_completion.release_one();

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

    let finished_await = (mcp_await_agent(&caller, &new_agent.agent_id)).await;
    assert_await_result_ready(&finished_await, &new_agent.agent_id, "idle");
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
async fn agent_control_spawns_children_by_custom_agent_type() {
    let mut fixture = Fixture::new().await;

    let parent_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "typed-parent",
            MockScript::one(MockTurn::gated_text(
                "typed parent stays active",
                &parent_gate,
            )),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("typed-parent".to_owned()),
            custom_agent_id: Some(protocol::CustomAgentId("tyde-project-manager".to_owned())),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/typed-parent".to_owned()],
                prompt: "typed parent stays active".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn typed parent failed");
    let env = expect_next_event(&mut fixture.client, "typed parent NewAgent").await;
    let parent_new: NewAgentPayload = env.parse_payload().expect("parse typed parent NewAgent");
    let parent_start = expect_next_event(&mut fixture.client, "typed parent AgentStart").await;
    assert_eq!(
        parent_start
            .parse_payload::<AgentStartPayload>()
            .unwrap()
            .tyde_permissions,
        protocol::AgentTydePermissions::Agent
    );
    drop(reservation);
    parent_gate.wait_until_entered().await;
    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;

    assert_eq!(
        parent_new.tyde_permissions,
        protocol::AgentTydePermissions::Agent,
        "Human-created ordinary agents are not automatically orchestrators"
    );
    let (operator, operator_start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::HumanChat,
            name: Some("human-operator".to_owned()),
            custom_agent_id: Some(protocol::CustomAgentId("tyde-help".to_owned())),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "Inspect Tyde configuration".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    assert_eq!(
        operator.new_agent.tyde_permissions,
        protocol::AgentTydePermissions::Root
    );
    assert_eq!(
        operator_start.tyde_permissions,
        protocol::AgentTydePermissions::Root
    );
    fixture.finish_turn(&operator).await;
    for (agent_id, expected) in [
        (&parent_new.agent_id, "agent"),
        (&operator.new_agent.agent_id, "root"),
    ] {
        let mut config = fixture.agent_control_caller(agent_id).await;
        config.url = fixture.config_mcp_http_url().await;
        let guide = mcp_success_json(
            &mcp_tool_call_as(&config, false, "tyde_config_get_permissions", json!({})).await,
        );
        assert_eq!(guide["tyde_permissions"], expected);
        assert!(
            guide["guidance"]
                .as_str()
                .unwrap()
                .contains(if expected == "root" {
                    "any Tyde control"
                } else {
                    "own sub-agents"
                })
        );
    }

    fixture.client.close_agent(&operator.stream).await.unwrap();
    fixture
        .next_frame_matching("human-selected Operator closed", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|closed| closed.agent_id == operator.new_agent.agent_id)
        })
        .await;
    let (restored_operator, restored_start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: operator_start.session_id.unwrap(),
                prompt: None,
            },
        })
        .await;
    assert_eq!(
        restored_operator.new_agent.tyde_permissions,
        protocol::AgentTydePermissions::Root
    );
    assert_eq!(
        restored_start.tyde_permissions,
        protocol::AgentTydePermissions::Root,
        "Initial Operator authority, not only a later role assignment, survives saved-session resume"
    );

    let options = mcp_list_launch_options(&caller).await;
    let offered = options["custom_agents"]
        .as_array()
        .unwrap_or_else(|| panic!("launch options must list agent types: {options}"))
        .iter()
        .map(|agent| agent["id"].as_str().expect("agent type id").to_owned())
        .collect::<Vec<_>>();
    for expected in ["tyde-implementer", "tyde-lander", "tyde-team-lead"] {
        assert!(
            offered.iter().any(|id| id == expected),
            "launch options should offer {expected}: {offered:?}"
        );
    }
    assert!(
        !offered.iter().any(|id| id == "tyde-help"),
        "the host Operator must not be offered as a child agent type: {offered:?}"
    );

    let unknown = mcp_spawn_agent_error_as(
        &caller,
        json!({
            "prompt": "child of a missing type",
            "backend_kind": "claude",
            "custom_agent_id": "no-such-agent"
        }),
    )
    .await;
    assert!(
        unknown.contains("unknown custom_agent_id no-such-agent")
            && unknown.contains("tyde_list_launch_options"),
        "an unknown agent type must be refused with a discovery hint: {unknown}"
    );
    let operator = mcp_spawn_agent_error_as(
        &caller,
        json!({
            "prompt": "child operator",
            "backend_kind": "claude",
            "custom_agent_id": "tyde-help"
        }),
    )
    .await;
    assert!(
        operator.contains("cannot be spawned through agent control"),
        "agent control must not grant a child the Operator's host configuration tools: {operator}"
    );

    let child_agent_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/typed-child"],
            "prompt": "implement the typed child item",
            "backend_kind": "claude",
            "name": "typed-child",
            "custom_agent_id": "tyde-implementer"
        }),
    )
    .await;
    let implementer_id = Some(protocol::CustomAgentId("tyde-implementer".to_owned()));
    let child_new =
        expect_replayed_new_agent(&mut fixture.client, &child_agent_id, "typed child NewAgent")
            .await;
    assert_eq!(child_new.custom_agent_id, implementer_id);
    assert_eq!(
        child_new.parent_agent_id.as_ref(),
        Some(&parent_new.agent_id)
    );
    let child_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "typed child AgentStart",
    )
    .await;
    assert_eq!(child_start.custom_agent_id, implementer_id);
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "[instructions: You are Tyde's Implementer.",
    )
    .await;
    parent_gate.release_one();
}

#[tokio::test]
async fn agent_control_http_binds_parent_to_authenticated_caller() {
    let mut fixture = Fixture::new().await;

    let parent_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "mcp-parent",
            MockScript::one(MockTurn::gated_text("parent stays active", &parent_gate)),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("mcp-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/mcp-parent".to_owned()],
                prompt: "parent stays active".to_owned(),
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
    drop(reservation);

    parent_gate.wait_until_entered().await;

    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;
    let child_agent_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/agent-control-mcp-parent-url"],
            "prompt": "child from authenticated MCP parent",
            "backend_kind": "claude",
            "name": "mcp-child"
        }),
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

    parent_gate.release_one();
    let parent_session = parent_start.session_id.expect("parent session exists");
    let child_session = child_start.session_id.expect("child session exists");
    assert_eq!(child_start.origin, AgentOrigin::AgentControl);

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("request durable lineage before restart");
    let sessions = fixture::next_frame_matching_on(
        &mut fixture.client,
        "persisted MCP child lineage",
        |env| {
            env.kind == FrameKind::SessionList
                && env.parse_payload::<SessionListPayload>().is_ok_and(|list| {
                    list.sessions
                        .iter()
                        .any(|session| session.id == child_session)
                })
        },
    )
    .await
    .parse_payload::<SessionListPayload>()
    .expect("parse persisted session lineage");
    let saved_child = sessions
        .sessions
        .iter()
        .find(|session| session.id == child_session)
        .expect("saved MCP child");
    assert!(
        saved_child.parent_id.as_ref() == Some(&parent_session),
        "MCP spawning must persist the same parent shown by the live protocol"
    );
    fixture
        .host_for_test()
        .shutdown_agents_for_conformance()
        .await;

    // A client may resume the child before the restart pass reaches its parent.
    // Reusing that live child must not leave it permanently advertised as a root.
    let restoration_gate = server::new_spawn_operation_test_gate();
    let restoration_finished = server::new_spawn_operation_test_gate();
    let host = server::spawn_host_with_mock_backend_and_runtime_config(
        fixture.store_dir().join("sessions.json"),
        fixture.store_dir().join("projects.json"),
        fixture.store_dir().join("settings.json"),
        server::HostRuntimeConfig {
            skip_real_backend_probe: true,
            restoration_snapshot_test_gate: Some(restoration_gate.shared()),
            restoration_complete_test_gate: Some(restoration_finished.shared()),
            ..Default::default()
        },
    )
    .expect("restart host with restoration held");
    restoration_gate.wait_until_entered().await;
    host.set_session_schema_ready_for_test(BackendKind::Claude)
        .await;
    let discovery_guard = host.hold_session_schema_refresh_for_test().await;
    let (mut resumed_client, initial_bootstrap) = fixture::connect_host(host.clone()).await;
    assert!(initial_bootstrap.agents.is_empty());
    resumed_client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: child_session.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume MCP child before automatic parent restoration");
    let early_child =
        fixture::next_frame_matching_on(&mut resumed_client, "early child resume", |env| {
            env.kind == FrameKind::NewAgent
                && env
                    .parse_payload::<NewAgentPayload>()
                    .is_ok_and(|agent| agent.session_id.as_ref() == Some(&child_session))
        })
        .await
        .parse_payload::<NewAgentPayload>()
        .expect("parse early child resume");
    fixture::next_frame_matching_on(&mut resumed_client, "early child bootstrap", |env| {
        env.kind == FrameKind::AgentBootstrap && env.stream == early_child.instance_stream
    })
    .await;
    drop(discovery_guard);
    restoration_gate.release_one();
    restoration_finished.wait_until_entered().await;

    let (observer, restored) = fixture::connect_host(host.clone()).await;
    restoration_finished.release_one();
    host.shutdown_agents_for_conformance().await;
    drop(observer);
    let parents = restored
        .agents
        .iter()
        .filter(|agent| agent.session_id.as_ref() == Some(&parent_session))
        .collect::<Vec<_>>();
    let children = restored
        .agents
        .iter()
        .filter(|agent| agent.session_id.as_ref() == Some(&child_session))
        .collect::<Vec<_>>();
    assert_eq!(parents.len(), 1, "restore exactly one parent card");
    assert_eq!(
        children.len(),
        1,
        "reuse the early child without duplicating it"
    );
    assert!(
        children[0].parent_agent_id.as_ref() == Some(&parents[0].agent_id),
        "restart must advertise the MCP child under its restored parent, not as a root card"
    );
}

#[tokio::test]
async fn agent_control_depth_limit_removes_tools_and_rejects_recursion() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "depth-root").await;
    let parent_caller = fixture.agent_control_caller(&parent.agent_id).await;
    let child_id = mcp_spawn_agent_as(
        &parent_caller,
        json!({
            "workspace_roots": ["/tmp/depth-child"],
            "prompt": "depth two",
            "backend_kind": "claude",
            "name": "depth-child"
        }),
    )
    .await;
    let child =
        expect_replayed_new_agent(&mut fixture.client, &child_id, "depth child NewAgent").await;
    expect_agent_start_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "depth child AgentStart",
    )
    .await;
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "mock backend response to: depth two",
    )
    .await;

    let child_caller = fixture.agent_control_caller(&child_id).await;
    let grandchild_id = mcp_spawn_agent_as(
        &child_caller,
        json!({
            "workspace_roots": ["/tmp/depth-grandchild"],
            "prompt": "depth three",
            "backend_kind": "claude",
            "name": "depth-grandchild"
        }),
    )
    .await;
    let grandchild = expect_replayed_new_agent(
        &mut fixture.client,
        &grandchild_id,
        "depth grandchild NewAgent",
    )
    .await;
    expect_agent_start_on_stream(
        &mut fixture.client,
        &grandchild.instance_stream,
        "depth grandchild AgentStart",
    )
    .await;
    expect_agent_control_child_initial_turn_on_stream(
        &mut fixture.client,
        &grandchild.instance_stream,
        "mock backend response to: depth three",
    )
    .await;

    let control = fixture.connect_agent_control().await;
    let read = control
        .read_agent_debug(grandchild_id.clone(), None, None, None)
        .await
        .expect("read depth-three agent");
    assert!(
        !read
            .events
            .iter()
            .any(|event| chat_event_contains(event, "startup_mcp_servers")),
        "depth-three agents must not receive Tyde agent-control MCP tools: {:?}",
        read.events
    );

    let grandchild_caller = fixture.agent_control_caller(&grandchild_id).await;
    let error = mcp_spawn_agent_error_as(
        &grandchild_caller,
        json!({
            "workspace_roots": ["/tmp/depth-four"],
            "prompt": "must be rejected",
            "backend_kind": "claude"
        }),
    )
    .await;
    assert!(
        error.contains("depth limit of 3 reached"),
        "depth refusal must explain the configured limit: {error}"
    );
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
async fn agent_control_http_latest_empty_error_reconnect_and_debug_are_typed() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "typed-read-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;

    let empty_child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/typed-empty-child"],
            "prompt": "visible before empty output",
            "backend_kind": "claude",
            "name": "typed-empty-child"
        }),
    )
    .await;
    let empty_ready = mcp_await_agent(&caller, &empty_child).await;
    assert_await_result_ready(&empty_ready, &empty_child, "idle");
    fixture
        .mock_by_id(&empty_child)
        .await
        .enqueue(MockTurn::empty_agent_control_output())
        .await;
    let send_empty = mcp_tool_call_as(
        &caller,
        false,
        "tyde_send_agent_message",
        json!({
            "agent_id": empty_child.0,
            "message": "produce empty output"
        }),
    )
    .await;
    assert!(!mcp_result_is_error(&send_empty));
    let empty_ready = mcp_await_agent(&caller, &empty_child).await;
    assert_await_result_ready(&empty_ready, &empty_child, "idle");

    let empty_read = mcp_tool_call_as(
        &caller,
        false,
        "tyde_read_agent",
        json!({ "agent_id": empty_child.0 }),
    )
    .await;
    let empty_payload = mcp_success_json(&empty_read);
    assert_eq!(empty_payload["output"]["kind"], "empty");
    assert!(empty_payload["output"].get("text").is_none());

    let debug_read = mcp_tool_call_as(
        &caller,
        false,
        "tyde_read_agent_debug",
        json!({
            "agent_id": empty_child.0,
            "after_seq": 0,
            "limit": 50,
            "max_bytes": 262144
        }),
    )
    .await;
    let debug_payload = mcp_success_json(&debug_read);
    assert_eq!(debug_payload["agent_id"], empty_child.0);
    assert!(
        debug_payload["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
    assert!(debug_payload["next_after_seq"].as_u64().is_some());

    let failed_child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/typed-failed-child"],
            "prompt": "visible before terminal error",
            "backend_kind": "claude",
            "name": "typed-failed-child"
        }),
    )
    .await;
    let initially_ready = mcp_await_agent(&caller, &failed_child).await;
    assert_await_result_ready(&initially_ready, &failed_child, "idle");
    let close_gate = MockGateHandle::new();
    fixture
        .mock_by_id(&failed_child)
        .await
        .enqueue(MockTurn::busy_then_close_stream(&close_gate))
        .await;
    let terminate = mcp_tool_call_as(
        &caller,
        false,
        "tyde_send_agent_message",
        json!({
            "agent_id": failed_child.0,
            "message": "terminate the backend"
        }),
    )
    .await;
    assert!(!mcp_result_is_error(&terminate));
    close_gate.wait_until_entered().await;
    close_gate.release_one();
    let failed_ready = mcp_await_agent(&caller, &failed_child).await;
    assert_eq!(failed_ready["ready"][0]["status"], "failed");

    for reconnect in 0..2 {
        let error_read = mcp_tool_call_as(
            &caller,
            false,
            "tyde_read_agent",
            json!({ "agent_id": failed_child.0 }),
        )
        .await;
        let error_payload = mcp_success_json(&error_read);
        assert_eq!(error_payload["output"]["kind"], "error");
        assert_eq!(
            error_payload["output"]["error"]["agent_id"], failed_child.0,
            "reconnect {reconnect} must retain the newer typed error"
        );
    }
}

#[tokio::test]
async fn agent_control_http_credentials_scope_every_child_tool() {
    let mut fixture = Fixture::new().await;
    let parent_a = spawn_agent_control_parent(&mut fixture, "credential-parent-a").await;
    let parent_b = spawn_agent_control_parent(&mut fixture, "credential-parent-b").await;
    let caller_a = fixture.agent_control_caller(&parent_a.agent_id).await;
    let caller_b = fixture.agent_control_caller(&parent_b.agent_id).await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "credential-child-a",
            MockScript::one(MockTurn::empty_agent_control_output()),
        )
        .await;
    let child_a = mcp_spawn_agent_as(
        &caller_a,
        json!({
            "workspace_roots": ["/tmp/credential-child-a"],
            "prompt": "credential child a",
            "backend_kind": "claude",
            "name": "credential-child-a"
        }),
    )
    .await;
    drop(reservation);
    let reservation = fixture
        .reserve_next_mock_launch(
            "credential-child-b",
            MockScript::one(MockTurn::empty_agent_control_output())
                .then(MockTurn::text("global follow-up reply")),
        )
        .await;
    let child_b = mcp_spawn_agent_as(
        &caller_b,
        json!({
            "workspace_roots": ["/tmp/credential-child-b"],
            "prompt": "credential child b",
            "backend_kind": "claude",
            "name": "credential-child-b"
        }),
    )
    .await;
    drop(reservation);
    let _ = mcp_await_agent(&caller_a, &child_a).await;
    let _ = mcp_await_agent(&caller_b, &child_b).await;

    let listed = mcp_list_agents(&caller_a).await;
    let listed_ids = listed
        .as_array()
        .expect("list agents array")
        .iter()
        .filter_map(|agent| agent["agent_id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(listed_ids, vec![child_a.0.as_str()]);

    for (tool, await_surface, arguments) in [
        ("tyde_read_agent", false, json!({ "agent_id": child_b.0 })),
        ("tyde_close_agent", false, json!({ "agent_id": child_b.0 })),
        (
            "tyde_read_agent_debug",
            false,
            json!({ "agent_id": child_b.0 }),
        ),
        (
            "tyde_send_agent_message",
            false,
            json!({ "agent_id": child_b.0, "message": "spoofed" }),
        ),
        (
            "tyde_send_agent_message",
            false,
            json!({ "agent_id": child_b.0, "message": "spoofed", "interrupt": true }),
        ),
        (
            "tyde_await_agents",
            true,
            json!({ "agent_ids": [child_b.0] }),
        ),
    ] {
        let response = mcp_tool_call_as(&caller_a, await_surface, tool, arguments).await;
        assert!(
            mcp_result_is_error(&response),
            "{tool} must deny cross-parent target"
        );
        assert!(mcp_result_text(&response).contains("not a direct child"));
    }

    // Opting into global scope reaches another top-level agent's child; the
    // default stays child-scoped as asserted above.
    let global_listed = mcp_success_json(
        &mcp_tool_call_as(
            &caller_a,
            false,
            "tyde_list_agents",
            json!({ "global": true }),
        )
        .await,
    );
    let global_ids = global_listed
        .as_array()
        .expect("global list agents array")
        .iter()
        .filter_map(|agent| agent["agent_id"].as_str())
        .collect::<std::collections::HashSet<_>>();
    for expected in [&parent_a.agent_id, &parent_b.agent_id, &child_a, &child_b] {
        assert!(
            global_ids.contains(expected.0.as_str()),
            "global list must include every live host agent"
        );
    }
    let sent = mcp_tool_call_as(
        &caller_a,
        false,
        "tyde_send_agent_message",
        json!({ "agent_id": child_b.0, "message": "global follow-up", "global": true }),
    )
    .await;
    assert_eq!(mcp_success_json(&sent)["ok"], true);
    let awaited = mcp_tool_call_as(
        &caller_a,
        true,
        "tyde_await_agents",
        json!({ "agent_ids": [child_b.0], "global": true }),
    )
    .await;
    assert_eq!(mcp_success_json(&awaited)["ready"][0]["status"], "idle");
    let read = mcp_tool_call_as(
        &caller_a,
        false,
        "tyde_read_agent",
        json!({ "agent_id": child_b.0, "global": true }),
    )
    .await;
    assert_eq!(
        mcp_success_json(&read)["output"]["text"].as_str(),
        Some("global follow-up reply")
    );
    for (tool, await_surface, arguments, expected) in [
        (
            "tyde_send_agent_message",
            false,
            json!({ "agent_id": parent_a.agent_id.0, "message": "self", "global": true }),
            "caller itself",
        ),
        (
            "tyde_await_agents",
            true,
            json!({ "agent_ids": [parent_a.agent_id.0], "global": true }),
            "caller itself",
        ),
        (
            "tyde_read_agent",
            false,
            json!({ "agent_id": "00000000-0000-0000-0000-000000000000", "global": true }),
            "not a live agent",
        ),
    ] {
        let response = mcp_tool_call_as(&caller_a, await_surface, tool, arguments).await;
        assert!(
            mcp_result_is_error(&response),
            "{tool} must reject global self or unknown targets"
        );
        assert!(mcp_result_text(&response).contains(expected));
    }

    let base_url = fixture.agent_control_http_url().await;
    let list_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "tyde_list_agents", "arguments": {} }
    });
    let query_spoof = post_json(
        &format!("{base_url}?agent_id={}", parent_a.agent_id.0),
        &list_request,
    )
    .await;
    assert!(mcp_result_is_error(&query_spoof));
    assert!(mcp_result_text(&query_spoof).contains("bearer credential"));

    let header_spoof = post_json_with_headers(
        &base_url,
        &[("x-tyde-agent-id", parent_a.agent_id.0.as_str())],
        &list_request,
    )
    .await;
    assert!(mcp_result_is_error(&header_spoof));
    assert!(mcp_result_text(&header_spoof).contains("bearer credential"));

    let mismatched_query = post_json_with_headers(
        &format!("{base_url}?agent_id={}", parent_b.agent_id.0),
        &[("Authorization", &caller_a.authorization)],
        &list_request,
    )
    .await;
    assert!(mcp_result_is_error(&mismatched_query));
    assert!(mcp_result_text(&mismatched_query).contains("does not match"));

    let mismatched_header = post_json_with_headers(
        &base_url,
        &[
            ("Authorization", &caller_a.authorization),
            ("x-tyde-agent-id", parent_b.agent_id.0.as_str()),
        ],
        &list_request,
    )
    .await;
    assert!(mcp_result_is_error(&mismatched_header));
    assert!(mcp_result_text(&mismatched_header).contains("does not match"));

    let forged = post_json_with_headers(
        &base_url,
        &[(
            "Authorization",
            "Bearer v1.00000000-0000-0000-0000-000000000000.forged",
        )],
        &list_request,
    )
    .await;
    assert!(mcp_result_is_error(&forged));
    assert!(mcp_result_text(&forged).contains("invalid agent-control bearer"));

    let child_caller = fixture.agent_control_caller(&child_a).await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "credential-grandchild",
            MockScript::one(MockTurn::held_text("grandchild still active")),
        )
        .await;
    let grandchild = mcp_spawn_agent_as(
        &child_caller,
        json!({
            "workspace_roots": ["/tmp/credential-grandchild"],
            "prompt": "Hold until closed", "backend_kind": "claude",
            "name": "credential-grandchild"
        }),
    )
    .await;
    drop(reservation);

    let spawned_root = mcp_spawn_agent_as(
        &caller_a,
        json!({
            "workspace_roots": ["/tmp/credential-agent-root"],
            "prompt": "work independently", "backend_kind": "claude",
            "name": "agent-spawned-root", "global": true
        }),
    )
    .await;
    let spawned_root_caller = fixture.agent_control_caller(&spawned_root).await;
    let mut root_config = spawned_root_caller.clone();
    root_config.url = fixture.host_for_test().config_mcp_url_for_test().await;
    let guide = mcp_tool_call_as(
        &root_config,
        false,
        "tyde_config_get_permissions",
        json!({}),
    )
    .await;
    assert_eq!(
        mcp_success_json(&guide)["tyde_permissions"],
        "agent",
        "Parentless agent-created roots still default to ordinary Agent guidance"
    );

    // A sub-agent reports to its parent by finishing its turn: global scope is
    // neither advertised to it nor honored, so it cannot reach its parent or
    // peers, while it still manages its own children.
    for (caller, await_surface, expect_global) in [
        (&caller_a, false, true),
        (&caller_a, true, true),
        (&child_caller, false, false),
        (&child_caller, true, false),
        (&spawned_root_caller, false, true),
        (&spawned_root_caller, true, true),
    ] {
        let url = if await_surface {
            &caller.await_url
        } else {
            &caller.url
        };
        let tools_response = post_json_with_headers(
            url,
            &[("Authorization", &caller.authorization)],
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
        )
        .await;
        let tools = tools_response["result"]["tools"]
            .as_array()
            .expect("tools/list result");
        let global_tools = tools
            .iter()
            .filter(|tool| tool["inputSchema"]["properties"].get("global").is_some())
            .count();
        if expect_global {
            assert!(global_tools > 0, "top-level callers must see global scope");
        } else {
            assert_eq!(global_tools, 0, "sub-agents must not see global scope");
        }
        for tool in tools {
            let description = tool["description"].as_str().unwrap_or_default();
            assert!(
                !description.contains("global"),
                "{} description must not advertise global scope",
                tool["name"]
            );
        }
    }
    for (tool, await_surface, arguments) in [
        (
            "tyde_send_agent_message",
            false,
            json!({ "agent_id": parent_a.agent_id.0, "message": "to parent", "global": true }),
        ),
        (
            "tyde_send_agent_message",
            false,
            json!({ "agent_id": child_b.0, "message": "to peer", "global": true }),
        ),
        (
            "tyde_read_agent",
            false,
            json!({ "agent_id": parent_a.agent_id.0, "global": true }),
        ),
        (
            "tyde_await_agents",
            true,
            json!({ "agent_ids": [parent_a.agent_id.0], "global": true }),
        ),
        ("tyde_list_agents", false, json!({ "global": true })),
        ("tyde_list_workbenches", false, json!({ "global": true })),
        (
            "tyde_spawn_agent",
            false,
            json!({
                "workspace_roots": ["/tmp/credential-global-from-child"],
                "prompt": "escape", "backend_kind": "claude", "global": true
            }),
        ),
    ] {
        let response = mcp_tool_call_as(&child_caller, await_surface, tool, arguments).await;
        assert!(
            mcp_result_is_error(&response),
            "{tool} must reject global scope from a sub-agent"
        );
        assert!(mcp_result_text(&response).contains("only available to top-level agents"));
    }
    eprintln!("Role credential flow: reconnecting observer");
    let (mut role_client, role_snapshot) = fixture.connect_with_bootstrap().await;
    let role_stream = role_snapshot
        .agents
        .iter()
        .find(|agent| agent.agent_id == child_a)
        .unwrap()
        .instance_stream
        .clone();
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut role_client,
        &role_stream,
        "role observer attached",
    )
    .await;
    assert!(replay.events.iter().any(|event| matches!(event,
        AgentBootstrapEvent::AgentStart(start) if start.tyde_permissions == protocol::AgentTydePermissions::Agent)));
    eprintln!("Role credential flow: observer attached");
    for access in [
        protocol::AgentTydePermissions::Orchestrator,
        protocol::AgentTydePermissions::Root,
    ] {
        role_client
            .set_agent_tyde_permissions(&role_stream, access)
            .await
            .unwrap();
        eprintln!("Role credential flow: waiting for promoted state");
        fixture::next_frame_matching_on(&mut role_client, "promoted child role", |env| {
            env.stream == role_stream && env.kind == FrameKind::AgentTydePermissionsChanged
        })
        .await;
        for await_surface in [false, true] {
            let url = if await_surface {
                &child_caller.await_url
            } else {
                &child_caller.url
            };
            let catalog = post_json_with_headers(
                url,
                &[("Authorization", &child_caller.authorization)],
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            )
            .await;
            assert!(
                catalog["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["inputSchema"]["properties"].get("global").is_some()),
                "Explicitly promoted children advertise global controls"
            );
        }
        let listed = mcp_success_json(
            &mcp_tool_call_as(
                &child_caller,
                false,
                "tyde_list_agents",
                json!({"global":true}),
            )
            .await,
        );
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .any(|agent| agent["agent_id"] == parent_a.agent_id.0)
        );
        let read = mcp_tool_call_as(
            &child_caller,
            false,
            "tyde_read_agent",
            json!({"agent_id":parent_a.agent_id.0,"global":true}),
        )
        .await;
        assert!(
            !mcp_result_is_error(&read),
            "Promoted child can read its parent"
        );
        eprintln!("Role credential flow: promoted global await");
        let ready = mcp_success_json(
            &mcp_tool_call_as(
                &child_caller,
                true,
                "tyde_await_agents",
                json!({"agent_ids":[child_b.0],"global":true}),
            )
            .await,
        );
        assert_eq!(ready["ready"][0]["status"], "idle");
    }
    role_client
        .set_agent_tyde_permissions(&role_stream, protocol::AgentTydePermissions::Agent)
        .await
        .unwrap();
    fixture::next_frame_matching_on(&mut role_client, "child demoted", |env| {
        env.stream == role_stream && env.kind == FrameKind::AgentTydePermissionsChanged
    })
    .await;
    let denied = mcp_tool_call_as(
        &child_caller,
        false,
        "tyde_read_agent",
        json!({"agent_id":parent_a.agent_id.0,"global":true}),
    )
    .await;
    assert!(
        mcp_result_is_error(&denied),
        "Demotion restores the child scope contract"
    );

    let child_listed = mcp_list_agents(&child_caller).await;
    assert_eq!(
        child_listed
            .as_array()
            .expect("child list agents array")
            .iter()
            .filter_map(|agent| agent["agent_id"].as_str())
            .collect::<Vec<_>>(),
        vec![grandchild.0.as_str()]
    );
    for (caller, target) in [
        (&caller_a, &parent_a.agent_id),
        (&caller_a, &parent_b.agent_id),
        (&caller_a, &grandchild),
        (&child_caller, &parent_a.agent_id),
        (&caller_b, &child_a),
    ] {
        let denied = mcp_tool_call_as(
            caller,
            false,
            "tyde_close_agent",
            json!({"agent_id": target.0}),
        )
        .await;
        assert!(
            mcp_result_is_error(&denied),
            "Close must reject self, ancestors, indirect descendants, and unrelated agents"
        );
        assert!(mcp_result_text(&denied).contains("not a direct child"));
    }
    let anonymous = post_json(
        &base_url,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "tyde_close_agent", "arguments": {"agent_id": child_a.0}}
        }),
    )
    .await;
    assert!(mcp_result_is_error(&anonymous));
    assert!(mcp_result_text(&anonymous).contains("bearer credential"));
    let wrong_surface = mcp_tool_call_as(
        &caller_a,
        true,
        "tyde_close_agent",
        json!({"agent_id": child_a.0}),
    )
    .await;
    assert!(mcp_result_is_error(&wrong_surface));
    assert!(mcp_result_text(&wrong_surface).contains("not available"));
    let invalid = mcp_tool_call_as(
        &caller_a,
        false,
        "tyde_close_agent",
        json!({"agent_id": "invalid"}),
    )
    .await;
    assert!(mcp_result_is_error(&invalid));
    assert!(mcp_result_text(&invalid).contains("invalid agent_id"));

    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    for target in [
        &parent_a.agent_id,
        &parent_b.agent_id,
        &child_a,
        &child_b,
        &grandchild,
    ] {
        assert!(
            bootstrap.agents.iter().any(|a| &a.agent_id == target),
            "Denied closes must leave every agent open"
        );
    }
    let (mut backlogged, release_output) = fixture.connect_with_paused_output().await;
    let closed = mcp_tool_call_as(
        &caller_a,
        false,
        "tyde_close_agent",
        json!({"agent_id": child_a.0}),
    )
    .await;
    let closed = mcp_success_json(&closed);
    assert_eq!(closed["agent_id"], child_a.0);
    assert_eq!(closed["closed"], true);
    for expected in [&grandchild, &child_a] {
        let event = expect_kind(
            &mut observer,
            FrameKind::AgentClosed,
            "MCP subtree close fan-out",
        )
        .await;
        assert_eq!(
            &event
                .parse_payload::<AgentClosedPayload>()
                .unwrap()
                .agent_id,
            expected,
            "Descendants must close before their parent"
        );
    }
    release_output.send(()).expect("release subtree observer");
    let backlogged_host = fixture::next_frame_matching_on(
        &mut backlogged,
        "backlogged subtree registration",
        |env| env.kind == FrameKind::HostBootstrap,
    )
    .await
    .parse_payload::<HostBootstrapPayload>()
    .expect("backlogged host bootstrap");
    let mut bootstrapped = std::collections::HashSet::new();
    for expected in [&grandchild, &child_a] {
        let event =
            fixture::next_frame_matching_on(&mut backlogged, "backlogged subtree close", |env| {
                if env.kind == FrameKind::AgentBootstrap {
                    bootstrapped.insert(env.stream.clone());
                }
                env.kind == FrameKind::AgentClosed
            })
            .await;
        let closed = event.parse_payload::<AgentClosedPayload>().unwrap();
        assert_eq!(
            &closed.agent_id, expected,
            "Backpressure must preserve descendant-before-parent closure order"
        );
        let advertised = backlogged_host
            .agents
            .iter()
            .find(|agent| &agent.agent_id == expected)
            .expect("subtree advertised before close");
        assert!(
            bootstrapped.contains(&advertised.instance_stream),
            "Closure must follow the advertised agent bootstrap"
        );
    }
    assert!(
        mcp_list_agents(&caller_a)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    let siblings = mcp_list_agents(&caller_b).await;
    assert!(
        siblings
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == child_b.0)
    );
    let repeated = mcp_tool_call_as(
        &caller_a,
        false,
        "tyde_close_agent",
        json!({"agent_id": child_a.0}),
    )
    .await;
    assert!(
        mcp_result_is_error(&repeated),
        "Closing a missing child must fail visibly"
    );
    let revoked = mcp_tool_call_as(&child_caller, false, "tyde_list_agents", json!({})).await;
    assert!(
        mcp_result_is_error(&revoked),
        "Closed agents must lose their caller authority"
    );
    let (_late_client, after_close) = fixture.connect_with_bootstrap().await;
    assert!(
        !after_close
            .agents
            .iter()
            .any(|a| a.agent_id == child_a || a.agent_id == grandchild)
    );
    assert!(
        after_close
            .agents
            .iter()
            .any(|a| a.agent_id == parent_a.agent_id)
    );
    assert!(
        after_close
            .agents
            .iter()
            .any(|a| a.agent_id == parent_b.agent_id)
    );
    assert!(after_close.agents.iter().any(|a| a.agent_id == child_b));
}

#[tokio::test]
async fn agent_control_await_endpoint_isolated_and_remains_pending() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "isolated-await-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "isolated-await-child",
            MockScript::one(MockTurn::held_text("isolated await holding")),
        )
        .await;
    let held_child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/isolated-await-child"],
            "prompt": "isolated await",
            "backend_kind": "claude",
            "name": "isolated-await-child"
        }),
    )
    .await;
    drop(reservation);
    let held_new = expect_replayed_new_agent(
        &mut fixture.client,
        &held_child,
        "isolated await child NewAgent",
    )
    .await;

    for (url, expected, absent) in [
        (&caller.url, "tyde_read_agent", "tyde_await_agents"),
        (&caller.await_url, "tyde_await_agents", "tyde_read_agent"),
    ] {
        let response = post_json_with_headers(
            url,
            &[("Authorization", &caller.authorization)],
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
        )
        .await;
        let tools = response["result"]["tools"]
            .as_array()
            .expect("tools/list result");
        assert!(tools.iter().any(|tool| tool["name"] == expected));
        assert!(!tools.iter().any(|tool| tool["name"] == absent));
        assert_eq!(
            tools.iter().any(|tool| tool["name"] == "tyde_close_agent"),
            url == &caller.url
        );
    }

    let pending = mcp_tool_call_as(
        &caller,
        true,
        "tyde_await_agents",
        json!({ "agent_ids": [held_child.0] }),
    );
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(350), &mut pending)
            .await
            .is_err(),
        "dedicated await call must remain pending beyond the scaled prior boundary"
    );
    fixture
        .client
        .interrupt(&held_new.instance_stream)
        .await
        .expect("interrupt held await child");
    let response = (&mut pending).await;
    assert!(!mcp_result_is_error(&response));
}

#[tokio::test]
async fn agent_control_await_cut_short_by_host_restart_reports_typed_host_stopping() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "restart-await-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "restart-await-child",
            MockScript::one(MockTurn::held_text("restart await holding")),
        )
        .await;
    let held_child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/restart-await-child"],
            "prompt": "restart await",
            "backend_kind": "claude",
            "name": "restart-await-child"
        }),
    )
    .await;
    drop(reservation);
    expect_replayed_new_agent(
        &mut fixture.client,
        &held_child,
        "restart await child NewAgent",
    )
    .await;

    let pending = mcp_tool_call_as(
        &caller,
        true,
        "tyde_await_agents",
        json!({ "agent_ids": [held_child.0] }),
    );
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut pending)
            .await
            .is_err(),
        "await must stay parked while the child is still thinking"
    );

    let stop_gate = fixture.install_restart_stop_test_gate();
    let host = fixture.host_for_test();
    let shutdown = tokio::spawn(async move {
        host.shutdown_for_restart()
            .await
            .expect("restart must preserve required accounting");
    });
    stop_gate.wait_until_entered().await;
    let response = (&mut pending).await;
    assert!(
        mcp_result_is_error(&response),
        "an await the host cut short did not complete: {response}"
    );
    let payload: Value = serde_json::from_str(mcp_result_text(&response))
        .unwrap_or_else(|_| panic!("host-stopped await must return a typed payload: {response}"));
    assert_eq!(
        payload["code"],
        json!("host_stopping"),
        "the orchestrator must be able to tell a restart apart from a failed await: {payload}"
    );
    let message = payload["message"]
        .as_str()
        .unwrap_or_else(|| panic!("host-stopped await must explain itself: {payload}"));
    assert!(
        message.contains("restart") && message.contains("tyde_await_agents"),
        "the orchestrator must be told to re-issue its await after the restart: {message}"
    );

    stop_gate.release_one();
    shutdown.await.expect("restart shutdown task");
}

#[tokio::test]
async fn agent_control_http_await_emits_progress_notifications() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "await-progress-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "await-progress",
            MockScript::one(MockTurn::held_text("await progress holding")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("await-progress".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-progress".to_owned()],
                prompt: "await progress".to_owned(),
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
    drop(reservation);

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let bearer = caller
        .authorization
        .strip_prefix("Bearer ")
        .expect("fixture caller authorization must be bearer");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(caller.await_url.clone()).auth_header(bearer),
    );
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

    let server_result = (&mut response)
        .await
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    let reservation = fixture
        .reserve_next_mock_launch(
            "await-tool-child",
            MockScript::one(MockTurn::held_text("child awaited holding")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("await-tool-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: Some(parent_new.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/await-tool-child".to_owned()],
                prompt: "child awaited".to_owned(),
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
    drop(reservation);

    expect_stream_end_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "child awaited holding",
    )
    .await;

    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;
    wait_for_agent_control_status(&caller, &child_new.agent_id, "thinking").await;

    fixture
        .mock_by_id(&parent_new.agent_id)
        .await
        .enqueue(MockTurn::agent_control_await([child_new.agent_id.clone()]))
        .await;
    fixture
        .client
        .send_message(&parent_new.instance_stream, "await for child".to_owned())
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
    assert_eq!(
        tool_name, "tyde_await_agents",
        "the request must carry the provider's own tool name"
    );
    assert_eq!(
        tool_type,
        ToolRequestType::TydeAwaitAgents {
            agent_ids: vec![child_new.agent_id.clone()],
        }
    );

    let stall_seconds_write_id = fixture
        .client
        .replace_setting("/supervisor/stall_timeout_seconds", 1_u32, 1_800_u32)
        .await
        .expect("set supervisor stall timeout");
    expect_settings_write_applied(
        &mut fixture.client,
        &stall_seconds_write_id,
        "set supervisor stall timeout",
    )
    .await;
    let stall_enabled_write_id = fixture
        .client
        .replace_setting("/supervisor/stall_timeout_enabled", true, false)
        .await
        .expect("enable supervisor stall timeout");
    expect_settings_write_applied(
        &mut fixture.client,
        &stall_enabled_write_id,
        "enable supervisor stall timeout",
    )
    .await;
    let supervisor_write_id = fixture
        .client
        .replace_setting("/supervisor/enabled", true, false)
        .await
        .expect("enable supervisor");
    expect_settings_write_applied(
        &mut fixture.client,
        &supervisor_write_id,
        "enable supervisor",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;

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

    wait_for_agent_control_status(&caller, &child_new.agent_id, "idle").await;

    let completion = expect_tool_completion_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        &tool_call_id,
    )
    .await;
    assert_eq!(completion.tool_call_id, tool_call_id);
    assert!(
        fixture::tool_completion_succeeded(&completion),
        "await completion failed: {completion:?}"
    );
    let ToolExecutionOutcome::Succeeded {
        result:
            ToolExecutionResult::TydeAwaitAgents {
                ready,
                still_thinking,
            },
    } = completion.outcome
    else {
        panic!("expected typed await completion");
    };
    assert!(still_thinking.is_empty());
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].agent_id, child_new.agent_id);
    assert_eq!(ready[0].status, protocol::AgentControlStatus::Idle);

    expect_stream_end_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock agent-control await completed",
    )
    .await;
    expect_typing_false_on_stream(&mut fixture.client, &parent_new.instance_stream).await;

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_parent = bootstrapped_agent(&bootstrap, &parent_new.agent_id);
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &replayed_parent.instance_stream,
        "typed await replay",
    )
    .await;
    assert!(replay.events.iter().any(|event| matches!(
        event,
        AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(ToolRequest {
            tool_call_id: replayed_call_id,
            tool_type: ToolRequestType::TydeAwaitAgents { agent_ids },
            ..
        })) if replayed_call_id == &tool_call_id && agent_ids == &vec![child_new.agent_id.clone()]
    )));
    assert!(replay.events.iter().any(|event| matches!(
        event,
        AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
            if completion.tool_call_id == tool_call_id
                && matches!(
                    &completion.outcome,
                    ToolExecutionOutcome::Succeeded {
                        result: ToolExecutionResult::TydeAwaitAgents { ready, still_thinking }
                    }
                        if still_thinking.is_empty()
                            && ready.len() == 1
                            && ready[0].agent_id == child_new.agent_id
                            && ready[0].status == protocol::AgentControlStatus::Idle
                )
    )));
}

/// A default follow-up joins the running turn through native steering and
/// cancels and sends next when the backend cannot steer.
/// `interrupt: true` always cancels the running turn and sends next, even when
/// the backend could steer, without losing queued messages.
#[tokio::test]
async fn agent_control_send_message_steers_by_default_and_interrupt_cancels() {
    for native_steering in [false, true] {
        let mut fixture = Fixture::new().await;
        let parent = spawn_agent_control_parent(&mut fixture, "interrupt-parent").await;
        let caller = fixture.agent_control_caller(&parent.agent_id).await;
        let redirect_gate = MockGateHandle::new();
        let idle_gate = MockGateHandle::new();
        // A gate services fixture controls, not backend interrupt commands, so
        // the turn the redirect cancels is a held one.
        let mut script =
            MockScript::one(MockTurn::held_text("initial reply").with_tyde_permissions_probe());
        if !native_steering {
            script = script
                .then(MockTurn::held_text("first follow-up reply"))
                .then(MockTurn::held_text("second follow-up reply"));
        } else {
            script = script.then(MockTurn::held_text("completion-race reply"));
        }
        let mut script = script
            .then(MockTurn::gated_text("redirect reply", &redirect_gate))
            .then(MockTurn::text("queued reply"))
            .then(
                MockTurn::gated_text("idle redirect reply", &idle_gate)
                    .with_tyde_permissions_probe(),
            )
            .with_user_bubbles();
        if native_steering {
            script = script.with_mid_turn_steering();
        }
        let reservation = fixture
            .reserve_next_mock_launch("interrupt-child", script)
            .await;
        let child = mcp_spawn_agent_as(
            &caller,
            json!({
                "workspace_roots": ["/tmp/interrupt-child"],
                "prompt": "start held turn",
                "name": "interrupt-child",
                "backend_kind": "claude"
            }),
        )
        .await;
        drop(reservation);
        let child_new =
            expect_replayed_new_agent(&mut fixture.client, &child, "interrupt child").await;
        let stream = &child_new.instance_stream;
        assert_eq!(
            child_new.tyde_permissions,
            protocol::AgentTydePermissions::Agent
        );
        let mut config_caller = fixture.agent_control_caller(&child_new.agent_id).await;
        config_caller.url = fixture.host_for_test().config_mcp_url_for_test().await;
        let guide = mcp_tool_call_as(
            &config_caller,
            false,
            "tyde_config_get_permissions",
            json!({}),
        )
        .await;
        let guide = mcp_success_json(&guide);
        assert_eq!(guide["tyde_permissions"], "agent");
        assert!(
            guide["guidance"]
                .as_str()
                .unwrap()
                .contains("Do not message or interrupt peers or parents")
        );

        let probe = fixture::next_chat_event_matching_on(&mut fixture.client, stream, "startup credential role", |event| {
            matches!(event, ChatEvent::StreamEnd(end) if serde_json::from_str::<Value>(&end.message.content).is_ok())
        }).await;
        let ChatEvent::StreamEnd(probe) = probe else {
            unreachable!()
        };
        let probe: Value = serde_json::from_str(&probe.message.content).unwrap();
        assert_eq!(
            probe["tyde_permissions"], "agent",
            "The provisioned startup MCP credential must not be anonymous Root"
        );

        fixture::next_chat_event_matching_on(
            &mut fixture.client,
            stream,
            "held initial turn",
            |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "initial reply"),
        )
        .await;

        for immediate in [false, true] {
            for origin in [
                protocol::MessageOrigin::Agent {
                    agent_id: Some(parent.agent_id.clone()),
                    name: parent.name.clone(),
                },
                protocol::MessageOrigin::System,
            ] {
                let payload = SendMessagePayload {
                    message: "reserved-origin-probe".to_owned(),
                    images: None,
                    origin: Some(origin),
                    tool_response: None,
                };
                if immediate {
                    fixture
                        .client
                        .steer_message_payload(stream, payload)
                        .await
                        .unwrap();
                } else {
                    fixture
                        .client
                        .send_message_payload(stream, payload)
                        .await
                        .unwrap();
                }
                let error = fixture::next_frame_matching_on(&mut fixture.client, "reserved sender rejected", |env| {
                    if env.stream == *stream && env.kind == FrameKind::ChatEvent {
                        assert!(!matches!(env.parse_payload::<ChatEvent>().unwrap(),
                            ChatEvent::MessageAdded(message) if message.content == "reserved-origin-probe"),
                            "Rejected provenance must never be delivered");
                    }
                    if env.stream == *stream && env.kind == FrameKind::QueuedMessages {
                        assert!(env.parse_payload::<protocol::QueuedMessagesPayload>().unwrap().messages.is_empty(),
                            "Rejected provenance must never be queued");
                    }
                    env.kind == FrameKind::CommandError
                }).await;
                assert!(
                    error
                        .parse_payload::<CommandErrorPayload>()
                        .unwrap()
                        .message
                        .contains("reserved for the server")
                );
            }
        }

        for (expected, arguments) in [
            (
                "first follow-up",
                json!({ "agent_id": child.0, "message": "first follow-up" }),
            ),
            (
                "second follow-up",
                json!({ "agent_id": child.0, "message": "second follow-up", "interrupt": false }),
            ),
        ] {
            let response =
                mcp_tool_call_as(&caller, false, "tyde_send_agent_message", arguments).await;
            assert!(
                !mcp_result_is_error(&response),
                "default delivery must be accepted"
            );
            let mut cancellations = 0;
            loop {
                let event = fixture::next_chat_event_matching_on(
                    &mut fixture.client,
                    stream,
                    "delivered follow-up input",
                    |_| true,
                )
                .await;
                match event {
                    ChatEvent::OperationCancelled(_) => cancellations += 1,
                    ChatEvent::MessageAdded(message)
                        if matches!(&message.sender, MessageSender::Agent { agent } if agent == &parent.name)
                            && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), .. }) if id == &parent.agent_id)
                            && message.content == expected =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
            assert_eq!(
                cancellations,
                usize::from(!native_steering),
                "native steering must not cancel; unavailable steering must cancel before delivery"
            );
        }

        if native_steering {
            // The turn ends under the steer: the follow-up is sent as the next
            // turn, never parked in the editable queue or answered early.
            fixture
                .mock_by_id(&child)
                .await
                .end_held_turn_before_steer()
                .await;
            let delivered = mcp_tool_call_as(
                &caller,
                false,
                "tyde_send_agent_message",
                json!({"agent_id": child.0, "message": "completion-race follow-up"}),
            )
            .await;
            assert!(!mcp_result_is_error(&delivered));
            fixture::next_frame_matching_on(
                &mut fixture.client,
                "completion-race follow-up sent as the next turn",
                |env| {
                    if env.stream != *stream {
                        return false;
                    }
                    if env.kind == FrameKind::QueuedMessages {
                        assert!(
                            env.parse_payload::<protocol::QueuedMessagesPayload>()
                                .unwrap()
                                .messages
                                .iter()
                                .all(|entry| entry.message != "completion-race follow-up"),
                            "agent-control delivery must never become an editable queued message"
                        );
                    }
                    if env.kind != FrameKind::ChatEvent {
                        return false;
                    }
                    match env.parse_payload::<ChatEvent>().unwrap() {
                        ChatEvent::OperationCancelled(_) => {
                            panic!("a turn that ended on its own must not be cancelled")
                        }
                        ChatEvent::MessageAdded(message) => {
                            message.content == "completion-race follow-up"
                        }
                        _ => false,
                    }
                },
            )
            .await;
        }

        fixture
            .client
            .send_message(stream, "keep queued".to_owned())
            .await
            .expect("queue ordinary chat input");
        fixture::next_frame_matching_on(
            &mut fixture.client,
            "ordinary chat message queued",
            |env| {
                if env.stream != *stream {
                    return false;
                }
                if env.kind == FrameKind::ChatEvent {
                    let event: ChatEvent =
                        env.parse_payload().expect("queue transition chat event");
                    assert!(
                        !matches!(event, ChatEvent::OperationCancelled(_)),
                        "delivered follow-ups must not cause a late cancellation"
                    );
                }
                env.kind == FrameKind::QueuedMessages
                    && env
                        .parse_payload::<protocol::QueuedMessagesPayload>()
                        .is_ok_and(|payload| {
                            payload.messages.len() == 1
                                && payload.messages[0].message == "keep queued"
                        })
            },
        )
        .await;

        let (mut lazy, lazy_snapshot) = fixture::connect_mobile_client_with_bootstrap(
            fixture.host_for_test(),
            "permissions-observer",
        )
        .await;
        let lazy_stream = lazy_snapshot
            .agents
            .iter()
            .find(|agent| agent.agent_id == child)
            .unwrap()
            .instance_stream
            .clone();
        let mut created_steering = None;
        let mut unrelated_steering = None;
        for access in [
            protocol::AgentTydePermissions::Orchestrator,
            protocol::AgentTydePermissions::Root,
        ] {
            let steering_before = mcp_success_json(
                &mcp_tool_call_as(
                    &config_caller,
                    false,
                    "tyde_config_list_steering",
                    json!({}),
                )
                .await,
            );
            fixture
                .client
                .set_agent_tyde_permissions(stream, access)
                .await
                .expect("assign Tyde permissions");
            fixture::next_frame_matching_on(
                &mut fixture.client,
                "Tyde role updated without steering",
                |env| {
                    if env.stream == *stream && env.kind == FrameKind::ChatEvent {
                        assert!(
                            !matches!(
                                env.parse_payload::<ChatEvent>().unwrap(),
                                ChatEvent::OperationCancelled(_) | ChatEvent::MessageAdded(_)
                            ),
                            "Role changes must not inject messages or cancel the current turn"
                        );
                    }
                    env.stream == *stream
                        && env.kind == FrameKind::AgentTydePermissionsChanged
                        && env
                            .parse_payload::<protocol::AgentTydePermissionsChangedPayload>()
                            .is_ok_and(|update| update.access == access)
                },
            )
            .await;
            let update = fixture::next_frame_matching_on(
                &mut lazy,
                "permission update on the correct lazy-client stream",
                |env| {
                    env.kind == FrameKind::AgentTydePermissionsChanged
                        && env
                            .parse_payload::<protocol::AgentTydePermissionsChangedPayload>()
                            .is_ok_and(|payload| {
                                payload.agent_id == child && payload.access == access
                            })
                },
            )
            .await;
            if access == protocol::AgentTydePermissions::Orchestrator {
                assert!(
                    update.stream.0.starts_with("/host/"),
                    "Unattached subscribers receive role state on the host stream"
                );
                fixture::send_load_agent_on(&mut lazy, &lazy_stream).await;
                let loaded = expect_raw_agent_bootstrap_on_stream(
                    &mut lazy,
                    &lazy_stream,
                    "role state at lazy attach",
                )
                .await;
                assert!(loaded.events.iter().any(|event| matches!(event, AgentBootstrapEvent::AgentStart(start) if start.tyde_permissions == access)));
            } else {
                assert_eq!(
                    update.stream, lazy_stream,
                    "Attached subscribers receive the actor event, not another host notification"
                );
            }
            let steering_after = mcp_success_json(
                &mcp_tool_call_as(
                    &config_caller,
                    false,
                    "tyde_config_list_steering",
                    json!({}),
                )
                .await,
            );
            assert_eq!(
                steering_before, steering_after,
                "Role assignment must not create or edit steering entries"
            );
            if access == protocol::AgentTydePermissions::Orchestrator {
                let before = mcp_success_json(
                    &mcp_tool_call_as(&config_caller, false, "tyde_config_get_settings", json!({}))
                        .await,
                );
                let added = mcp_success_json(&mcp_tool_call_as(&config_caller, false, "tyde_config_upsert_steering", json!({"title":"Review final diff", "content":"Read the completed diff before reporting results."})).await);
                fixture::next_frame_matching_on(&mut fixture.client, "added steering is published", |env| env.kind == FrameKind::SteeringNotify && env.parse_payload::<protocol::SteeringNotifyPayload>().is_ok_and(|notify| matches!(notify, protocol::SteeringNotifyPayload::Upsert { steering } if serde_json::to_value(&steering.id).unwrap() == added["id"]))).await;
                let steering = mcp_success_json(
                    &mcp_tool_call_as(
                        &config_caller,
                        false,
                        "tyde_config_list_steering",
                        json!({}),
                    )
                    .await,
                );
                assert!(
                    steering
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["id"] == added["id"])
                );
                created_steering = Some(added["id"].clone());
                let unrelated = mcp_success_json(&mcp_tool_call_as(&config_caller, false, "tyde_config_upsert_steering",
                    json!({"title":"Keep guidance", "content":"Preserve this unrelated entry."})).await);
                unrelated_steering = Some(unrelated.clone());
                fixture::next_frame_matching_on(&mut fixture.client, "unrelated guidance published", |env| env.kind == FrameKind::SteeringNotify
                    && env.parse_payload::<protocol::SteeringNotifyPayload>().is_ok_and(|notify| matches!(notify,
                        protocol::SteeringNotifyPayload::Upsert { steering } if serde_json::to_value(&steering.id).unwrap() == unrelated["id"]))).await;
                let added_aspect = mcp_success_json(&mcp_tool_call_as(&config_caller, false, "tyde_config_upsert_review_aspect", json!({"aspect":{"name":"Ownership clarity", "description":"Check typed ownership", "instructions":"Verify explicit owners at boundaries.", "enabled":true}})).await);
                let after = mcp_success_json(
                    &mcp_tool_call_as(&config_caller, false, "tyde_config_get_settings", json!({}))
                        .await,
                );
                assert!(
                    after["review"]["aspects"]
                        .get(added_aspect["aspect_id"].as_str().unwrap())
                        .is_some()
                );
                for key in [
                    "enable_mobile_connections",
                    "enabled_backends",
                    "tyde_agent_control_mcp_enabled",
                    "tyde_debug_mcp_enabled",
                ] {
                    assert_eq!(
                        before[key], after[key],
                        "Adding improvement guidance must leave host access settings untouched"
                    );
                }
            }
            if access == protocol::AgentTydePermissions::Root {
                let id = created_steering.as_ref().unwrap();
                let unrelated = unrelated_steering.as_ref().unwrap();
                let updated = mcp_success_json(&mcp_tool_call_as(&config_caller, false, "tyde_config_upsert_steering",
                    json!({"steering_id":id, "title":"Review final changes", "content":"Updated review guidance."})).await);
                assert_eq!(&updated["id"], id);
                fixture::next_frame_matching_on(&mut fixture.client, "Root steering update published", |env| env.kind == FrameKind::SteeringNotify
                    && env.parse_payload::<protocol::SteeringNotifyPayload>().is_ok_and(|notify| matches!(notify,
                        protocol::SteeringNotifyPayload::Upsert { steering } if serde_json::to_value(&steering).unwrap() == updated))).await;
                let entries = mcp_success_json(
                    &mcp_tool_call_as(
                        &config_caller,
                        false,
                        "tyde_config_list_steering",
                        json!({}),
                    )
                    .await,
                );
                assert_eq!(
                    entries
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|entry| &entry["id"] == id)
                        .collect::<Vec<_>>(),
                    vec![&updated]
                );
                assert!(entries.as_array().unwrap().contains(unrelated));
                let deleted = mcp_success_json(
                    &mcp_tool_call_as(
                        &config_caller,
                        false,
                        "tyde_config_delete_steering",
                        json!({"steering_id":id}),
                    )
                    .await,
                );
                assert_eq!(deleted["deleted"], true);
                fixture::next_frame_matching_on(&mut fixture.client, "Root steering deletion published", |env| env.kind == FrameKind::SteeringNotify
                    && env.parse_payload::<protocol::SteeringNotifyPayload>().is_ok_and(|notify| matches!(notify,
                        protocol::SteeringNotifyPayload::Delete { id: deleted_id } if serde_json::to_value(&deleted_id).unwrap() == *id))).await;
                let entries = mcp_success_json(
                    &mcp_tool_call_as(
                        &config_caller,
                        false,
                        "tyde_config_list_steering",
                        json!({}),
                    )
                    .await,
                );
                assert!(
                    !entries
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| &entry["id"] == id)
                );
                assert!(entries.as_array().unwrap().contains(unrelated));
            }
            let guide = mcp_tool_call_as(
                &config_caller,
                false,
                "tyde_config_get_permissions",
                json!({}),
            )
            .await;
            let guide = mcp_success_json(&guide);
            assert_eq!(
                guide["tyde_permissions"],
                serde_json::to_value(access).unwrap()
            );
            assert!(guide["guidance"].as_str().unwrap().contains(
                if access == protocol::AgentTydePermissions::Orchestrator {
                    "Never edit or remove existing"
                } else {
                    "any Tyde control"
                }
            ));
        }

        let redirected = mcp_tool_call_as(
            &caller,
            false,
            "tyde_send_agent_message",
            json!({
                "agent_id": child.0, "message": "redirect now", "interrupt": true
            }),
        )
        .await;
        assert!(
            !mcp_result_is_error(&redirected),
            "interrupt delivery must be accepted"
        );
        let mut redirect_cancelled = false;
        fixture::next_frame_matching_on(
            &mut fixture.client,
            "interrupt cancels, then sends next",
            |env| {
                if env.stream != *stream {
                    return false;
                }
                if env.kind == FrameKind::QueuedMessages {
                    assert!(
                        env.parse_payload::<protocol::QueuedMessagesPayload>()
                            .unwrap()
                            .messages
                            .iter()
                            .all(|entry| entry.message != "redirect now"),
                        "agent-control delivery must never become an editable queued message"
                    );
                }
                if env.kind != FrameKind::ChatEvent {
                    return false;
                }
                match env.parse_payload::<ChatEvent>().unwrap() {
                    ChatEvent::OperationCancelled(_) => {
                        redirect_cancelled = true;
                        false
                    }
                    ChatEvent::MessageAdded(message) => {
                        let redirected = matches!(&message.sender, MessageSender::Agent { agent } if agent == &parent.name)
                            && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), .. }) if id == &parent.agent_id)
                            && message.content == "redirect now";
                        assert!(
                            !redirected || redirect_cancelled,
                            "the redirect is sent after its cancellation"
                        );
                        redirected
                    }
                    _ => false,
                }
            },
        )
        .await;
        redirect_gate.wait_until_entered().await;

        let await_redirect = mcp_await_agent(&caller, &child);
        tokio::pin!(await_redirect);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut await_redirect)
                .await
                .is_err(),
            "await must not report idle while the redirected turn is held"
        );
        redirect_gate.release_one();
        let ready = await_redirect.await;
        assert_await_result_ready(&ready, &child, "idle");
        fixture::next_chat_event_matching_on(
            &mut fixture.client,
            stream,
            "preserved queued input",
            |event| {
                matches!(event, ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::User) && message.content == "keep queued")
            },
        )
        .await;
        let final_reply = "queued reply";
        // Observing a user bubble only proves admission, not completion of
        // that turn, so read after the final reply and idle.
        fixture::next_chat_event_matching_on(
            &mut fixture.client,
            stream,
            "final reply",
            |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == final_reply),
        )
        .await;
        fixture::next_chat_event_matching_on(
            &mut fixture.client,
            stream,
            "final turn idle",
            |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
        )
        .await;
        let read = mcp_tool_call_as(
            &caller,
            false,
            "tyde_read_agent",
            json!({ "agent_id": child.0 }),
        )
        .await;
        let read = mcp_success_json(&read);
        assert!(read["output"]["text"].as_str() == Some(final_reply));

        let idle = mcp_tool_call_as(
            &caller,
            false,
            "tyde_send_agent_message",
            json!({
                "agent_id": child.0, "message": "redirect while idle", "interrupt": true
            }),
        )
        .await;
        assert!(
            !mcp_result_is_error(&idle),
            "idle interrupt must start a turn"
        );
        let probe = fixture::next_chat_event_matching_on(&mut fixture.client, stream, "startup credential observes role update", |event| {
            matches!(event, ChatEvent::StreamEnd(end) if serde_json::from_str::<Value>(&end.message.content).is_ok())
        }).await;
        let ChatEvent::StreamEnd(probe) = probe else {
            unreachable!()
        };
        assert_eq!(
            serde_json::from_str::<Value>(&probe.message.content).unwrap()["tyde_permissions"],
            "root"
        );
        idle_gate.wait_until_entered().await;
        let await_idle = mcp_await_agent(&caller, &child);
        tokio::pin!(await_idle);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut await_idle)
                .await
                .is_err(),
            "acknowledgment must publish the newly active turn before await"
        );
        idle_gate.release_one();
        assert_await_result_ready(&await_idle.await, &child, "idle");
        let read = mcp_tool_call_as(
            &caller,
            false,
            "tyde_read_agent",
            json!({ "agent_id": child.0 }),
        )
        .await;
        assert!(mcp_success_json(&read)["output"]["text"].as_str() == Some("idle redirect reply"));
        let (mut observer, snapshot) = fixture.connect_with_bootstrap().await;
        let restored = snapshot
            .agents
            .iter()
            .find(|agent| agent.agent_id == child)
            .unwrap();
        assert_eq!(
            restored.tyde_permissions,
            protocol::AgentTydePermissions::Root
        );
        let replay = expect_raw_agent_bootstrap_on_stream(
            &mut observer,
            &restored.instance_stream,
            "role replay",
        )
        .await;
        assert!(replay.events.iter().any(|event| matches!(event, AgentBootstrapEvent::AgentStart(start) if start.tyde_permissions == protocol::AgentTydePermissions::Root)));
        assert!(replay.events.iter().any(|event| matches!(event, AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) if matches!(&message.sender, MessageSender::Agent { agent } if agent == &parent.name) && message.content == "redirect now")));
        let root_caller = fixture.agent_control_caller(&child).await;
        let descendant = mcp_spawn_agent_as(
            &root_caller,
            json!({"prompt":"small child task", "name":"root-child", "backend_kind":"claude", "workspace_roots":["/tmp/interrupt-grandchild"]}),
        )
        .await;
        let descendant = expect_replayed_new_agent(
            &mut fixture.client,
            &descendant,
            "Root child is still Agent",
        )
        .await;
        assert_eq!(
            descendant.tyde_permissions,
            protocol::AgentTydePermissions::Agent,
            "Root authority is not inherited"
        );
        let saved_session = restored.session_id.clone().unwrap();
        fixture.mock_by_id(&child).await.assert_clean().await;
        fixture
            .client
            .close_agent(stream)
            .await
            .expect("close saved role");
        fixture::next_frame_matching_on(&mut fixture.client, "role session closed", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|event| event.agent_id == child)
        })
        .await;
        let (resumed, resumed_start) = fixture
            .spawn_with(SpawnAgentPayload {
                launch_context: protocol::AgentLaunchContext::Standard,
                name: None,
                custom_agent_id: None,
                project_id: None,
                parent_agent_id: Some(parent.agent_id.clone()),
                params: SpawnAgentParams::Resume {
                    session_id: saved_session,
                    prompt: None,
                },
            })
            .await;
        assert_eq!(
            resumed_start.tyde_permissions,
            protocol::AgentTydePermissions::Root
        );
        assert_eq!(
            resumed.new_agent.tyde_permissions,
            protocol::AgentTydePermissions::Root
        );

        fixture.mock(&resumed).await.assert_clean().await;
    }
}

/// Closing an agent refuses an agent-control interrupt still waiting for the
/// turn it cancelled to end; the message is neither sent nor queued.
#[tokio::test]
async fn closing_agent_refuses_held_agent_control_interrupt() {
    for backend_exit in [false, true] {
        let mut fixture = Fixture::new().await;
        let parent = spawn_agent_control_parent(&mut fixture, "held-control-parent").await;
        let caller = fixture.agent_control_caller(&parent.agent_id).await;
        let interrupt_gate = MockGateHandle::new();
        let reservation = fixture
            .reserve_next_mock_launch(
                "held-control-child",
                MockScript::one(MockTurn::held_text("held reply"))
                    .with_user_bubbles()
                    .with_native_child_idle_gate(&interrupt_gate),
            )
            .await;
        let child = mcp_spawn_agent_as(
            &caller,
            json!({
                "workspace_roots": ["/tmp/held-control-child"],
                "prompt": "start held turn",
                "name": "held-control-child",
                "backend_kind": "claude"
            }),
        )
        .await;
        drop(reservation);
        let child_new =
            expect_replayed_new_agent(&mut fixture.client, &child, "held control child").await;
        let stream = child_new.instance_stream.clone();
        fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &stream,
        "held turn",
        |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "held reply"),
    )
    .await;

        let mock = fixture.mock_by_id(&child).await;
        let refused = mcp_tool_call_as(
            &caller,
            false,
            "tyde_send_agent_message",
            json!({"agent_id": child.0, "message": "refused by close", "interrupt": true}),
        );
        tokio::pin!(refused);
        // The actor holds the message in the step that issues the interrupt, so a
        // close sent once the backend parks on that interrupt finds it held.
        tokio::select! {
            biased;
            () = interrupt_gate.wait_until_entered() => {}
            _ = &mut refused => panic!("a follow-up must not be answered before the turn it cancelled ends"),
        }
        if backend_exit {
            mock.close_stream().await;
        } else {
            fixture
                .client
                .close_agent(&stream)
                .await
                .expect("close held control agent");
        }
        let refused = refused.await;
        assert!(
            mcp_result_is_error(&refused),
            "closing must refuse a held control message"
        );
        assert!(mcp_result_text(&refused).contains(if backend_exit {
            "not running"
        } else {
            "closing"
        }));
        interrupt_gate.release_one();
        fixture::next_frame_matching_on(&mut fixture.client, "held-control child closed", |env| {
            if env.stream == stream && env.kind == FrameKind::QueuedMessages {
                assert!(
                    env.parse_payload::<protocol::QueuedMessagesPayload>()
                        .unwrap()
                        .messages
                        .iter()
                        .all(|entry| entry.message != "refused by close"),
                    "a refused control message must never be queued"
                );
            }
            if env.stream == stream && env.kind == FrameKind::ChatEvent {
                assert!(
                    !matches!(
                        env.parse_payload::<ChatEvent>().unwrap(),
                        ChatEvent::MessageAdded(message) if message.content == "refused by close"
                    ),
                    "a refused control message must never be sent"
                );
            }
            if backend_exit {
                env.stream == stream
                    && env.kind == FrameKind::AgentError
                    && env
                        .parse_payload::<AgentErrorPayload>()
                        .is_ok_and(|error| error.fatal)
            } else {
                env.kind == FrameKind::AgentClosed
            }
        })
        .await;
    }
}

#[tokio::test]
async fn prior_stop_fences_agent_control_interrupt_delivery() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "stop-control-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let stop = MockGateHandle::new();
    let finish = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "stop-control-child",
            MockScript::one(MockTurn::held_text("held reply"))
                .then(MockTurn::gated_text("follow-up reply", &finish))
                .with_user_bubbles()
                .with_native_child_idle_gate(&stop),
        )
        .await;
    let child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/stop-control-child"], "prompt": "hold work",
            "name": "stop-control-child", "backend_kind": "claude"
        }),
    )
    .await;
    drop(reservation);
    let new = expect_replayed_new_agent(&mut fixture.client, &child, "Stop control child").await;
    let stream = new.instance_stream;
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &stream,
        "held reply",
        |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "held reply"),
    )
    .await;
    let mock = fixture.mock_by_id(&child).await;
    fixture
        .client
        .interrupt(&stream)
        .await
        .expect("prior human Stop");
    stop.wait_until_entered().await;
    mock.wait_for_interrupt_request().await;
    let send = mcp_tool_call_as(
        &caller,
        false,
        "tyde_send_agent_message",
        json!({
            "agent_id": child.0, "message": "follow-up after Stop", "interrupt": true
        }),
    );
    tokio::pin!(send);
    tokio::select! {
        biased;
        () = mock.wait_for_interrupt_request() => {}
        _ = &mut send => panic!("no delivery before the Stop boundary"),
    }
    stop.release_one();
    assert!(
        !mcp_result_is_error(&send.await),
        "follow-up must be delivered after Stop"
    );
    finish.wait_until_entered().await;
    let mut delivered = 0;
    let mut replied = false;
    fixture::next_frame_matching_on(&mut fixture.client, "follow-up settled after Stop", |env| {
        if env.stream != stream { return false; }
        if env.kind == FrameKind::QueuedMessages {
            assert!(env.parse_payload::<protocol::QueuedMessagesPayload>().unwrap().messages.iter()
                .all(|entry| entry.message != "follow-up after Stop"), "control must never enter the editable queue");
        }
        if env.kind != FrameKind::ChatEvent { return false; }
        match env.parse_payload::<ChatEvent>().unwrap() {
            ChatEvent::MessageAdded(message) if message.content == "follow-up after Stop" => {
                assert!(matches!(message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(ref id), .. }) if id == &parent.agent_id));
                delivered += 1;
            }
            ChatEvent::StreamEnd(end) if end.message.content == "follow-up reply" => replied = true,
            _ => {}
        }
        replied
    }).await;
    assert_eq!(delivered, 1, "control is visible exactly once after Stop");
    finish.release_one();
    fixture::next_frame_matching_on(&mut fixture.client, "follow-up idle", |env| {
        if env.stream != stream || env.kind != FrameKind::ChatEvent {
            return false;
        }
        match env.parse_payload::<ChatEvent>().unwrap() {
            ChatEvent::MessageAdded(message) if message.content == "follow-up after Stop" => {
                panic!("control was delivered twice")
            }
            ChatEvent::OperationCancelled(_) => panic!("prior Stop cancelled the follow-up turn"),
            ChatEvent::TypingStatusChanged(false) => return true,
            _ => {}
        }
        false
    })
    .await;
    mock.assert_clean().await;
}

#[tokio::test]
async fn agent_control_steers_while_requested_compaction_waits_for_idle() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "compaction-control-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "compaction-control-child",
            MockScript::one(MockTurn::held_text("held initial response"))
                .with_mid_turn_steering()
                .with_user_bubbles(),
        )
        .await;
    let child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/compaction-control-child"],
            "prompt": "hold this turn",
            "name": "compaction-control-child",
            "backend_kind": "claude"
        }),
    )
    .await;
    drop(reservation);
    let child_new =
        expect_replayed_new_agent(&mut fixture.client, &child, "compaction child").await;
    let stream = &child_new.instance_stream;
    fixture::next_chat_event_matching_on(&mut fixture.client, stream, "held turn", |event| {
        matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "held initial response")
            || matches!(event, ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::Assistant { .. })
                    && message.content == "held initial response")
    })
    .await;
    eprintln!("COMPACTION CONTROL phase=held_response_observed");
    fixture
        .client
        .compact_agent(stream, protocol::AgentCompactPayload::default())
        .await
        .expect("request compaction behind held turn");
    let deferred =
        fixture::next_frame_matching_on(&mut fixture.client, "deferred compaction", |frame| {
            frame.stream == *stream && frame.kind == FrameKind::ContextCompactionNotify
        })
        .await;
    assert!(matches!(
        deferred
            .parse_payload::<protocol::ContextCompactionNotifyPayload>()
            .unwrap()
            .status,
        protocol::ContextCompactionStatus::Deferred {
            stage: protocol::CompactionStage::WaitingForIdle
        }
    ));
    eprintln!("COMPACTION CONTROL phase=deferred_notification_observed");
    let response = mcp_tool_call_as(
        &caller,
        false,
        "tyde_send_agent_message",
        json!({"agent_id": child.0, "message": "redirect before compaction"}),
    )
    .await;
    eprintln!("COMPACTION CONTROL phase=control_receipt_observed");
    assert!(
        !mcp_result_is_error(&response),
        "deferred compaction must not postpone a steerable control"
    );
    fixture::next_chat_event_matching_on(&mut fixture.client, stream, "actual control delivery", |event| {
        assert!(!matches!(event, ChatEvent::OperationCancelled(_)), "native steering must preserve its held turn");
        matches!(event, ChatEvent::MessageAdded(message) if message.content == "redirect before compaction")
    }).await;
}

#[tokio::test]
async fn agent_control_steer_waits_for_unstarted_turn() {
    const FOLLOW_UP: &str = "join when started";
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "turn-start-parent").await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let start = MockGateHandle::new();
    let waiting = MockGateHandle::new();
    let finish = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "turn-start-child",
            MockScript::one(MockTurn::text("ready reply"))
                .then(MockTurn::gated_text_after_turn_start(
                    "work", &start, &waiting, &finish,
                ))
                .with_mid_turn_steering()
                .with_user_bubbles(),
        )
        .await;
    let child = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/turn-start-child"],
            "prompt": "get ready",
            "name": "turn-start-child",
            "backend_kind": "claude"
        }),
    )
    .await;
    drop(reservation);
    let child_new =
        expect_replayed_new_agent(&mut fixture.client, &child, "turn start child").await;
    let stream = child_new.instance_stream.clone();
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &stream,
        "ready turn",
        |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "ready reply"),
    )
    .await;

    fixture
        .client
        .send_message(&stream, "start work".to_owned())
        .await
        .expect("send work turn");
    start.wait_until_entered().await;
    let follow_up = mcp_tool_call_as(
        &caller,
        false,
        "tyde_send_agent_message",
        json!({"agent_id": child.0, "message": FOLLOW_UP}),
    );
    tokio::pin!(follow_up);
    tokio::select! {
        biased;
        () = waiting.wait_until_entered() => {}
        _ = &mut follow_up => panic!("a follow-up must not be answered before its turn starts"),
    }
    start.release_one();
    let mut delivered = 0;
    let mut work_seen = false;
    fixture::next_frame_matching_on(&mut fixture.client, "visible steer in unfinished work", |env| {
        if env.stream != stream { return false; }
        if env.kind == FrameKind::QueuedMessages {
            assert!(env.parse_payload::<protocol::QueuedMessagesPayload>().unwrap().messages.iter()
                .all(|entry| entry.message != FOLLOW_UP), "control must never enter the editable queue");
        }
        if env.kind != FrameKind::ChatEvent { return false; }
        match env.parse_payload::<ChatEvent>().unwrap() {
            ChatEvent::MessageAdded(message) if message.content == FOLLOW_UP => {
                assert!(matches!(message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(ref id), .. }) if id == &parent.agent_id));
                delivered += 1;
            }
            ChatEvent::StreamEnd(end) if end.message.content == "work" => work_seen = true,
            ChatEvent::OperationCancelled(_) => panic!("steering cancelled the admitted work"),
            _ => {}
        }
        work_seen && delivered > 0
    }).await;
    assert_eq!(
        delivered, 1,
        "steer is visible exactly once while work remains unfinished"
    );
    assert!(
        !mcp_result_is_error(&follow_up.await),
        "steer receipt confirms actual delivery before turn end"
    );
    finish.release_one();
    fixture::next_frame_matching_on(&mut fixture.client, "work idle", |env| {
        if env.stream != stream {
            return false;
        }
        if env.kind == FrameKind::QueuedMessages {
            assert!(
                env.parse_payload::<protocol::QueuedMessagesPayload>()
                    .unwrap()
                    .messages
                    .iter()
                    .all(|entry| entry.message != FOLLOW_UP),
                "control must never enter the editable queue"
            );
        }
        if env.kind != FrameKind::ChatEvent {
            return false;
        }
        match env.parse_payload::<ChatEvent>().unwrap() {
            ChatEvent::MessageAdded(message) if message.content == FOLLOW_UP => {
                panic!("steer was delivered twice")
            }
            ChatEvent::OperationCancelled(_) => panic!("steering cancelled its turn"),
            ChatEvent::TypingStatusChanged(false) => return true,
            _ => {}
        }
        false
    })
    .await;
    fixture.mock_by_id(&child).await.assert_clean().await;
}

#[tokio::test]
async fn agent_control_send_message_is_typed_live_and_on_replay() {
    let mut fixture = Fixture::new().await;
    let parent = spawn_agent_control_parent(&mut fixture, "typed-send-parent").await;
    let recipient = protocol::AgentId("typed-send-recipient".to_owned());
    let message = "# Follow up\n\n- preserve `markdown`\n- keep exact bytes";

    fixture
        .mock_by_id(&parent.agent_id)
        .await
        .enqueue(MockTurn::agent_control_send_message(
            recipient.clone(),
            message,
            true,
        ))
        .await;
    fixture
        .client
        .send_message(&parent.instance_stream, "send typed message".to_owned())
        .await
        .expect("send typed agent-control message fixture");

    let event = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &parent.instance_stream,
        "tyde_send_agent_message ToolRequest",
        |event| {
            matches!(
                event,
                ChatEvent::ToolRequest(request)
                    if fixture::tool_request_name(request) == "tyde_send_agent_message"
            )
        },
    )
    .await;
    let ChatEvent::ToolRequest(request) = event else {
        unreachable!("matched ToolRequest above");
    };
    assert_eq!(
        fixture::tool_request_name(&request),
        "tyde_send_agent_message"
    );
    assert_eq!(
        request.tool_type,
        ToolRequestType::TydeSendAgentMessage {
            agent_id: recipient.clone(),
            message: message.to_owned(),
            interrupt: true,
        }
    );
    let event = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &parent.instance_stream,
        "tyde_send_agent_message ToolExecutionCompleted",
        |event| {
            matches!(
                event,
                ChatEvent::ToolExecutionCompleted(completion)
                    if completion.tool_call_id == request.tool_call_id
            )
        },
    )
    .await;
    let ChatEvent::ToolExecutionCompleted(completion) = event else {
        unreachable!("matched ToolExecutionCompleted above");
    };
    assert!(matches!(
        completion.outcome,
        ToolExecutionOutcome::Succeeded {
            result: ToolExecutionResult::TydeSendAgentMessage
        }
    ));
    let event = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &parent.instance_stream,
        "delivery StreamEnd",
        |event| matches!(event, ChatEvent::StreamEnd(_)),
    )
    .await;
    let ChatEvent::StreamEnd(end) = event else {
        unreachable!("matched StreamEnd above");
    };
    assert!(
        end.message
            .content
            .contains("mock agent-control message delivered"),
        "unexpected StreamEnd text on {}: {}",
        parent.instance_stream,
        end.message.content
    );
    let event = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &parent.instance_stream,
        "TypingStatusChanged(false)",
        |_| true,
    )
    .await;
    assert!(matches!(event, ChatEvent::TypingStatusChanged(false)));

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_parent = bootstrapped_agent(&bootstrap, &parent.agent_id);
    let env =
        fixture::next_frame_matching_on(&mut late_client, "typed send-message replay", |env| {
            env.kind == FrameKind::AgentBootstrap && env.stream == replayed_parent.instance_stream
        })
        .await;
    let replay: AgentBootstrapPayload = env
        .parse_payload()
        .expect("parse typed send-message replay AgentBootstrap");
    assert!(replay.events.iter().any(|event| matches!(
        event,
        AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(ToolRequest {
            tool_call_id,
            tool_type: ToolRequestType::TydeSendAgentMessage { agent_id, message: replayed, interrupt: true },
            ..
        })) if tool_call_id == &request.tool_call_id
            && agent_id == &recipient
            && replayed == message
    )));
    assert!(replay.events.iter().any(|event| matches!(
        event,
        AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
            if completion.tool_call_id == request.tool_call_id
                && matches!(
                    completion.outcome,
                    ToolExecutionOutcome::Succeeded {
                        result: ToolExecutionResult::TydeSendAgentMessage
                    }
                )
    )));
}

/// An explicit `parent_agent_id` tool argument requires an agent-control
/// bearer credential (an unauthenticated caller is refused with the typed
/// message) and, when the authenticated parent passes its own id, the child
/// binds to that parent and inherits its project.
#[tokio::test]
async fn agent_control_http_explicit_parent_requires_bearer_and_binds_child() {
    let mut fixture = Fixture::new().await;

    let base_url = fixture.agent_control_http_url().await;
    let unknown_parent_id = protocol::AgentId("11111111-1111-1111-1111-111111111111".to_owned());
    let response = post_json(
        &base_url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tyde_spawn_agent",
                "arguments": {
                    "workspace_roots": ["/tmp/unknown-parent-child"],
                    "prompt": "child with unknown parent",
                    "backend_kind": "claude",
                    "name": "unknown-parent-child",
                    "parent_agent_id": unknown_parent_id.0
                }
            }
        }),
    )
    .await;
    assert!(mcp_result_is_error(&response));
    assert!(mcp_result_text(&response).contains("requires an agent-control bearer credential"));

    let parent_project = create_project(
        &mut fixture.client,
        "Explicit Parent Project",
        vec!["/tmp/explicit-parent".to_owned()],
    )
    .await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
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

    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;
    let child_agent_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": project_roots(&parent_project),
            "prompt": "child with explicit parent",
            "backend_kind": "claude",
            "name": "explicit-child",
            "parent_agent_id": parent_new.agent_id.0
        }),
    )
    .await;

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
        "child spawned by its authenticated parent must inherit the parent project_id"
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;
    let inherited_child_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": project_roots(&parent_project),
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

    let override_child_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": project_roots(&override_project),
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

    let listed = mcp_list_agents(&caller).await;
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
        .find(|session| session.workspace_roots == project_roots(&parent_project))
        .expect("inherited child session missing from fresh host bootstrap");
    assert_eq!(
        inherited_session.project_id.as_ref(),
        Some(&parent_project.id)
    );
    let override_session = fresh_bootstrap
        .sessions
        .iter()
        .find(|session| session.workspace_roots == project_roots(&override_project))
        .expect("override child session missing from fresh host bootstrap");
    assert_eq!(
        override_session.project_id.as_ref(),
        Some(&override_project.id)
    );
}

/// A backend-native child is a first-class agent: it carries the
/// BackendNative origin and its parent on both NewAgent and AgentStart, it
/// appears in the parent's caller-scoped agent-control list, and it replays
/// to late subscribers — while the parent receives no completion notice
/// when the child's turn finishes.
#[tokio::test]
async fn backend_native_child_is_first_class_and_replays_to_late_subscribers() {
    let mut fixture = Fixture::new().await;

    let (parent_new, parent_start, child_new, child_start) =
        spawn_parent_with_native_child(&mut fixture, true).await;

    let host_stream = single_host_stream(&fixture.client);
    let grandchild_new: NewAgentPayload = loop {
        if let Some(env) =
            pop_pending_agent_event(&fixture.client, &host_stream, FrameKind::NewAgent)
        {
            break env
                .parse_payload()
                .expect("parse pending native grandchild NewAgent");
        }
        let env = expect_next_event(&mut fixture.client, "native grandchild NewAgent").await;
        assert!(
            !matches!(env.kind, FrameKind::AgentError | FrameKind::AgentClosed),
            "native grandchild registration must not fail or close an agent"
        );
        if env.kind == FrameKind::ChatEvent {
            assert!(
                !matches!(env.parse_payload::<ChatEvent>().expect("parse native subtree event"),
                ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error)),
                "native grandchild registration must not produce an error card"
            );
        }
        if env.kind == FrameKind::NewAgent {
            break env
                .parse_payload()
                .expect("parse native grandchild NewAgent");
        }
        push_pending_agent_event(&fixture.client, env);
    };
    assert!(
        grandchild_new.parent_agent_id.as_ref() == Some(&child_new.agent_id),
        "native grandchild NewAgent must identify its immediate parent, not the root"
    );
    let grandchild_start = expect_agent_start_on_stream(
        &mut fixture.client,
        &grandchild_new.instance_stream,
        "native grandchild AgentStart",
    )
    .await;
    assert_eq!(grandchild_new.origin, AgentOrigin::BackendNative);
    assert_eq!(grandchild_start.origin, AgentOrigin::BackendNative);
    assert!(
        grandchild_start.parent_agent_id.as_ref() == Some(&child_new.agent_id),
        "native grandchild AgentStart must identify its immediate parent"
    );
    expect_native_child_prompt_on_stream(
        &mut fixture.client,
        &grandchild_new.instance_stream,
        "grandchild task",
        &child_new,
    )
    .await;
    let grandchild_end = expect_completed_turn_on_stream(
        &mut fixture.client,
        &grandchild_new.instance_stream,
        "mock native child response to: grandchild task",
    )
    .await;
    assert_known_turn_usage(&grandchild_end.message.token_usage, 33, 33);
    let sessions = [
        parent_start.session_id.as_ref(),
        child_start.session_id.as_ref(),
        grandchild_start.session_id.as_ref(),
    ];
    assert!(
        sessions.iter().all(Option::is_some)
            && sessions
                .iter()
                .enumerate()
                .all(|(index, session)| !sessions[..index].contains(session)),
        "each native tree node must retain its own distinct session binding"
    );

    for (parent, child, prompt) in [
        (&parent_new, &child_new, "parent prompt"),
        (&child_new, &grandchild_new, "grandchild task"),
    ] {
        let request =
            expect_tool_request_on_stream(&mut fixture.client, &parent.instance_stream, "Task")
                .await;
        assert!(
            matches!(&request.tool_type, ToolRequestType::AgentSpawn {
            prompt: Some(task), name: Some(name), execution_mode: protocol::AgentExecutionMode::Background,
        } if task == prompt && name == &child.name),
            "each spawn tool request must belong to its immediate parent's stream"
        );
        loop {
            let env = expect_chat_event_on_stream(
                &mut fixture.client,
                &parent.instance_stream,
                "native descendant tool link",
            )
            .await;
            let event: ChatEvent = env
                .parse_payload()
                .expect("parse native descendant tool link");
            if let ChatEvent::ToolProgress(progress) = event {
                assert!(
                    progress.tool_call_id == request.tool_call_id,
                    "native descendant progress must decorate its own spawn request"
                );
                assert!(
                    matches!(&progress.update, protocol::ToolProgressUpdate::SubAgent(link)
                    if link.agent_id == child.agent_id && link.agent_name == child.name
                        && link.completed && link.status == protocol::SubAgentProgressStatus::Completed),
                    "spawn tool card must link to the child registered by the server"
                );
                break;
            }
        }
    }

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(200),
        "backend-native child completion follow-up",
    )
    .await;

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

    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;
    let listed = mcp_list_agents(&caller).await;
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(1),
        "caller-scoped agent-control list should contain only the native child"
    );
    let listed_child = mcp_listed_agent(&listed, &child_new.agent_id);
    assert_eq!(
        listed_child.get("parent_agent_id").and_then(Value::as_str),
        Some(parent_new.agent_id.0.as_str())
    );

    let child_caller = fixture.agent_control_caller(&child_new.agent_id).await;
    let child_listed = mcp_list_agents(&child_caller).await;
    assert_eq!(
        child_listed.as_array().map(Vec::len),
        Some(1),
        "native child's control scope must contain only its own descendant"
    );
    assert!(
        mcp_listed_agent(&child_listed, &grandchild_new.agent_id)
            .get("parent_agent_id")
            .and_then(Value::as_str)
            == Some(child_new.agent_id.0.as_str()),
        "caller-scoped listing must preserve the grandchild's immediate parent"
    );

    let (mut late_client, eager_bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_child_new = bootstrapped_agent(&eager_bootstrap, &child_new.agent_id);
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

    let (mut observer, bootstrap) = fixture::connect_mobile_client_with_bootstrap(
        fixture.host_for_test(),
        "native-tree-observer",
    )
    .await;
    let root = bootstrapped_agent(&bootstrap, &parent_new.agent_id);
    assert!(
        root.parent_agent_id.is_none(),
        "native descendants must not reparent the root"
    );
    assert_eq!(root.origin, AgentOrigin::User);
    // Lazy mobile bootstraps omit rollups; use the eager subscriber's usage snapshot.
    let usage = eager_bootstrap
        .task_token_usages
        .iter()
        .find(|usage| usage.root_agent_id == parent_new.agent_id)
        .expect("native tree task usage in host bootstrap");
    assert_known_task_status(&usage.total.status);
    assert_known_task_scope(&usage.self_usage, MOCK_TURN_TOKEN_TOTAL);
    assert_eq!(usage.descendant_count, 2);
    assert_eq!(
        usage.descendant_usage.usage.total_tokens,
        MOCK_NATIVE_CHILD_TOKEN_TOTAL + 33
    );
    assert_eq!(
        usage.total.usage.total_tokens,
        MOCK_TURN_TOKEN_TOTAL + MOCK_NATIVE_CHILD_TOKEN_TOTAL + 33
    );
    assert_eq!(usage.breakdown.len(), 3);

    for (agent, parent, depth, own_usage, response, task) in [
        (
            &parent_new,
            None,
            0,
            MOCK_TURN_TOKEN_TOTAL,
            "mock backend response to: parent prompt",
            Some((&child_new, "parent prompt")),
        ),
        (
            &child_new,
            Some(&parent_new),
            1,
            MOCK_NATIVE_CHILD_TOKEN_TOTAL,
            "mock native child response to: parent prompt",
            Some((&grandchild_new, "grandchild task")),
        ),
        (
            &grandchild_new,
            Some(&child_new),
            2,
            33,
            "mock native child response to: grandchild task",
            None,
        ),
    ] {
        let descriptor = bootstrapped_agent(&bootstrap, &agent.agent_id);
        assert!(
            descriptor.parent_agent_id.as_ref() == parent.map(|entry| &entry.agent_id),
            "replayed native tree must preserve immediate parent identities"
        );
        let entry = usage
            .breakdown
            .iter()
            .find(|entry| entry.agent_id == agent.agent_id)
            .expect("native tree usage entry");
        assert_eq!(entry.depth, depth);
        assert!(
            entry.parent_agent_id.as_ref() == parent.map(|entry| &entry.agent_id),
            "task usage must attribute each node to its immediate parent"
        );
        assert_known_task_scope(&entry.usage, own_usage);

        fixture::send_load_agent_on(&mut observer, &descriptor.instance_stream).await;
        let replay: AgentBootstrapPayload =
            fixture::next_frame_matching_on(&mut observer, "native tree agent replay", |env| {
                env.kind == FrameKind::AgentBootstrap && env.stream == descriptor.instance_stream
            })
            .await
            .parse_payload()
            .expect("parse native tree agent replay");
        let messages = replay
            .events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))
                    if matches!(message.sender, MessageSender::Assistant { .. }) =>
                {
                    Some(message)
                }
                AgentBootstrapEvent::ChatEvent(ChatEvent::StreamEnd(end)) => Some(&end.message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            messages.len(),
            1,
            "each transcript must contain only its own assistant response"
        );
        assert!(
            messages[0].content == response,
            "native descendant output must not bleed into an ancestor or sibling transcript"
        );
        assert_known_turn_usage(&messages[0].token_usage, own_usage, own_usage);
        if let Some(parent) = parent {
            assert!(replay.events.iter().any(|event| matches!(event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))
                    if matches!(&message.sender, MessageSender::Agent { agent } if agent == &parent.name)
                        && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), .. })
                            if id == &parent.agent_id))),
                "descendant transcript must attribute its input to the immediate parent");
        }
        let requests = replay
            .events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(ChatEvent::ToolRequest(request)) => Some(request),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            requests.len(),
            usize::from(task.is_some()),
            "spawn tool requests must not leak from descendants into an ancestor transcript"
        );
        if let Some((child, prompt)) = task {
            assert!(
                matches!(&requests[0].tool_type, ToolRequestType::AgentSpawn { prompt: Some(actual), .. }
                if actual == prompt),
                "replayed spawn card must retain its own task"
            );
            assert!(
                replay.events.iter().any(|event| matches!(event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::ToolProgress(progress))
                    if progress.tool_call_id == requests[0].tool_call_id
                        && matches!(&progress.update, protocol::ToolProgressUpdate::SubAgent(link)
                            if link.agent_id == child.agent_id))),
                "replayed spawn card must link only to the immediate child"
            );
        }
    }
}

#[tokio::test]
async fn native_progress_keeps_typed_backend_without_a_registry_record() {
    const CALL: &str = "native-progress";
    let native_id = protocol::AgentId("progress-only-child".to_owned());
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "native-progress-owner",
            MockScript::one(
                MockTurn::text("Delegated").with_open_native_subagent(CALL, native_id.clone()),
            ),
        )
        .await;
    let progress = fixture.next_chat_event_matching(&agent, "native backend provenance", |event| {
        matches!(event, ChatEvent::ToolProgress(progress) if progress.tool_call_id == CALL)
    }).await;
    let ChatEvent::ToolProgress(progress) = progress else {
        unreachable!()
    };
    let protocol::ToolProgressUpdate::SubAgent(sub) = progress.update else {
        unreachable!()
    };
    assert_eq!(sub.backend_kind, Some(agent.new_agent.backend_kind));
    assert_eq!(sub.agent_id, native_id);
    assert!(!sub.completed);

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        !bootstrap
            .agents
            .iter()
            .any(|agent| agent.agent_id == native_id),
        "native progress provenance cannot depend on a child registry record"
    );
    let replayed = bootstrapped_agent(&bootstrap, &agent.new_agent.agent_id);
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &replayed.instance_stream,
        "native progress replay",
    )
    .await;
    assert!(replay.events.iter().any(|event| matches!(event,
        AgentBootstrapEvent::ChatEvent(ChatEvent::ToolProgress(progress))
            if matches!(&progress.update, protocol::ToolProgressUpdate::SubAgent(sub)
                if sub.agent_id == native_id && sub.backend_kind == Some(agent.new_agent.backend_kind) && !sub.completed)
    )), "late clients receive the same typed native provenance");

    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close native progress owner");
    let terminal = fixture
        .next_chat_event_matching(&agent, "native progress stopped", |event| {
            matches!(event, ChatEvent::ToolProgress(progress)
            if matches!(&progress.update, protocol::ToolProgressUpdate::SubAgent(sub)
                if sub.agent_id == native_id && sub.completed))
        })
        .await;
    let ChatEvent::ToolProgress(terminal) = terminal else {
        unreachable!()
    };
    let protocol::ToolProgressUpdate::SubAgent(sub) = terminal.update else {
        unreachable!()
    };
    assert_eq!(sub.backend_kind, Some(agent.new_agent.backend_kind));
    assert_eq!(sub.status, protocol::SubAgentProgressStatus::Stopped);
}

/// A backend-native relay child accepts no direct input of any kind: a
/// checked agent-control delivery is refused without wedging the parent's
/// await or appending a secondary transcript error, its session is marked
/// non-resumable, and resuming it fails with a fatal typed startup error
/// while the connection stays usable.
#[tokio::test]
async fn backend_native_child_rejects_direct_input_and_resume() {
    let mut fixture = Fixture::new().await;

    let (parent, _parent_start, child, _child_start) =
        spawn_parent_with_native_child(&mut fixture, false).await;
    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    let initially_ready = mcp_await_agent(&caller, &child.agent_id).await;
    assert_eq!(initially_ready["ready"][0]["status"], "idle");
    assert_eq!(
        initially_ready["still_thinking"].as_array().map(Vec::len),
        Some(0)
    );

    let before_debug = mcp_tool_call_as(
        &caller,
        false,
        "tyde_read_agent_debug",
        json!({
            "agent_id": child.agent_id.0.clone(),
            "after_seq": 0,
            "limit": 200,
            "max_bytes": 262144
        }),
    )
    .await;
    let before_debug = mcp_success_json(&before_debug);
    let cursor = before_debug["next_after_seq"]
        .as_u64()
        .expect("relay debug cursor");

    let rejected = mcp_tool_call_as(
        &caller,
        false,
        "tyde_send_agent_message",
        json!({
            "agent_id": child.agent_id.0.clone(),
            "message": "relay must reject this checked delivery"
        }),
    )
    .await;
    assert!(mcp_result_is_error(&rejected));
    assert!(
        mcp_result_text(&rejected)
            .contains("backend-native relay agents do not accept direct input")
    );

    let after_rejection = mcp_await_agent(&caller, &child.agent_id).await;
    assert_eq!(after_rejection["ready"][0]["status"], "idle");
    assert_eq!(
        after_rejection["still_thinking"].as_array().map(Vec::len),
        Some(0),
        "rejected relay delivery must not leave a speculative Thinking status"
    );

    let after_debug = mcp_tool_call_as(
        &caller,
        false,
        "tyde_read_agent_debug",
        json!({
            "agent_id": child.agent_id.0.clone(),
            "after_seq": cursor,
            "limit": 200,
            "max_bytes": 262144
        }),
    )
    .await;
    let after_debug = mcp_success_json(&after_debug);
    assert_eq!(
        after_debug["events"].as_array().map(Vec::len),
        Some(0),
        "checked relay rejection must not append a secondary transcript error"
    );

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

    let parent_session = list
        .sessions
        .iter()
        .find(|session| session.user_alias.as_deref() == Some("parent-with-native-child"))
        .expect("missing parent session");
    let child_session = list
        .sessions
        .iter()
        .find(|session| session.alias.as_deref() == Some("mock-native-child"))
        .expect("missing backend-native child session");

    assert_eq!(child_session.parent_id.as_ref(), Some(&parent_session.id));
    assert!(
        !child_session.resumable,
        "backend-native child sessions must be marked non-resumable"
    );

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("resume-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: child_session.id.clone(),
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
                if payload.session_id.as_ref() == Some(&child_session.id) {
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
    assert_eq!(resumed_start.session_id.as_ref(), Some(&child_session.id));

    let error = expect_agent_error_containing(
        &mut fixture.client,
        &resumed_agent.instance_stream,
        &format!("cannot resume non-resumable session {}", child_session.id),
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
///
/// The parked relay must also keep rejecting direct input with the typed
/// relay rejection — not the router's generic "agent not running" error.
#[tokio::test]
async fn parked_backend_native_child_replays_to_late_clients_and_rejects_interrupt() {
    let mut fixture = Fixture::new().await;

    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-with-dropped-native-child",
            MockScript::one(
                MockTurn::text("mock backend response to: parent prompt")
                    .with_dropped_native_child("mock-native-child", "parent prompt"),
            ),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("parent-with-dropped-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/dropped-sub-agent-parent".to_owned()],
                prompt: "parent prompt".to_owned(),
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
    drop(reservation);

    expect_turn_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "mock backend response to: parent prompt",
    )
    .await;

    let child_new = expect_new_agent(&mut fixture.client, "native child NewAgent").await;

    expect_agent_start_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "native child AgentStart",
    )
    .await;

    expect_native_child_prompt_on_stream(
        &mut fixture.client,
        &child_new.instance_stream,
        "parent prompt",
        &parent_new,
    )
    .await;
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
async fn cancelling_parent_terminally_closes_live_native_child_and_replays_idle() {
    let mut fixture = Fixture::new().await;
    let idle_gate = MockGateHandle::new();
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    let reservation = fixture
        .reserve_next_mock_launch(
            "parent-with-live-native-child",
            MockScript::one(MockTurn::held_text_with_live_native_child(
                "mock backend held response to: parent",
                "mock-live-native-child",
                "live native child task",
            ))
            .with_native_child_idle_gate(&idle_gate),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("parent-with-live-native-child".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/live-native-child-parent".to_owned()],
                prompt: "parent".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn parent with live native child");

    let parent_env = expect_kind(&mut fixture.client, FrameKind::NewAgent, "parent NewAgent").await;
    let parent: NewAgentPayload = parent_env.parse_payload().expect("parse parent NewAgent");
    expect_agent_start_on_stream(&mut fixture.client, &parent.instance_stream, "parent start")
        .await;
    drop(reservation);
    for context in [
        "parent typing",
        "parent stream start",
        "parent stream delta",
    ] {
        let _ = expect_chat_event_on_stream(&mut fixture.client, &parent.instance_stream, context)
            .await;
    }

    let child = expect_new_agent(&mut fixture.client, "child NewAgent").await;
    expect_agent_start_on_stream(&mut fixture.client, &child.instance_stream, "child start").await;
    expect_native_child_prompt_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "live native child task",
        &parent,
    )
    .await;
    for context in ["child typing", "child stream start", "child stream delta"] {
        let _ =
            expect_chat_event_on_stream(&mut fixture.client, &child.instance_stream, context).await;
    }

    fixture
        .client
        .interrupt(&parent.instance_stream)
        .await
        .expect("interrupt parent");
    let stream_end = expect_chat_event_on_stream(
        &mut fixture.client,
        &child.instance_stream,
        "child StreamEnd",
    )
    .await;
    assert!(matches!(
        stream_end
            .parse_payload::<ChatEvent>()
            .expect("child StreamEnd"),
        ChatEvent::StreamEnd(_)
    ));
    // The shared cancellation helper also waits for idle, which this gate
    // deliberately holds until after the activity assertions.
    eprintln!("Native child lifecycle regression: waiting for cancellation before held idle");
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &child.instance_stream,
        "native child cancellation before held idle",
        |event| {
            if let ChatEvent::OperationCancelled(data) = event {
                assert!(data.message.contains("Parent agent turn ended"));
                true
            } else {
                assert!(!matches!(event, ChatEvent::TypingStatusChanged(false)));
                false
            }
        },
    )
    .await;

    let caller = fixture.agent_control_caller(&parent.agent_id).await;
    idle_gate.wait_until_entered().await;
    eprintln!("Native child lifecycle regression: backend idle held");
    let agents = mcp_list_agents(&caller).await;
    assert_eq!(
        mcp_listed_agent(&agents, &child.agent_id)
            .get("status")
            .and_then(Value::as_str),
        Some("thinking"),
        "native child cancellation must not claim idle before the backend marker"
    );
    let (mut working_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let working_child = bootstrapped_agent(&bootstrap, &child.agent_id);
    let working = expect_raw_agent_bootstrap_on_stream(
        &mut working_client,
        &working_child.instance_stream,
        "native child awaiting backend idle",
    )
    .await;
    assert_eq!(working.activity, protocol::AgentActivity::Thinking);
    idle_gate.release_one();
    expect_typing_false_on_stream(&mut fixture.client, &child.instance_stream).await;
    wait_for_agent_control_status(&caller, &child.agent_id, "idle").await;

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed_child = bootstrapped_agent(&bootstrap, &child.agent_id);
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &replayed_child.instance_stream,
        "late child bootstrap",
    )
    .await;
    assert_eq!(replay.activity, protocol::AgentActivity::Idle);
    let replayed_chat = replay
        .events
        .iter()
        .filter_map(|event| match event {
            AgentBootstrapEvent::ChatEvent(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    let cancel_index = replayed_chat
        .iter()
        .position(|event| matches!(event, ChatEvent::OperationCancelled(_)))
        .expect("late replay must contain child cancellation");
    assert!(
        !replayed_chat[cancel_index + 1..]
            .iter()
            .any(|event| matches!(event, ChatEvent::TypingStatusChanged(true)))
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn live_send_and_interrupt_do_not_reread_the_transcript() {
    use rustix::fs::inotify;
    use std::mem::MaybeUninit;

    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "journal-responsiveness",
            MockScript::one(MockTurn::text("previous response ".repeat(65_536)))
                .then(MockTurn::held_text("working after large transcript")),
        )
        .await;
    fixture
        .finish_turn(&agent)
        .await
        .assert_stream_end_contains("previous response");
    let journal = std::fs::read_dir(fixture.store_dir().join("transcripts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("persisted transcript");
    assert!(std::fs::metadata(&journal).unwrap().len() > 1_000_000);
    let watch =
        inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC).unwrap();
    inotify::add_watch(
        &watch,
        &journal,
        inotify::WatchFlags::ACCESS | inotify::WatchFlags::MODIFY,
    )
    .unwrap();

    fixture
        .client
        .send_message(&agent.stream, "Continue working".to_owned())
        .await
        .unwrap();
    fixture.next_chat_event_matching(&agent, "held response started", |event| {
        matches!(event, ChatEvent::StreamDelta(delta) if delta.text.contains("working after large transcript"))
    }).await;
    fixture.client.interrupt(&agent.stream).await.unwrap();
    fixture
        .next_chat_event_matching(
            &agent,
            "interrupt reaches backend and becomes idle",
            |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
        )
        .await;

    // Observe real file reads, rather than a machine-dependent latency limit:
    // rereading old payloads before every append is the source of the stall.
    let mut buffer = [MaybeUninit::uninit(); 1024];
    let mut reader = inotify::Reader::new(watch, &mut buffer);
    let mut writes = 0;
    loop {
        match reader.next() {
            Ok(event) => {
                assert!(
                    !event.events().contains(inotify::ReadFlags::ACCESS),
                    "live event handling reread the existing transcript before processing the interrupt"
                );
                if event.events().contains(inotify::ReadFlags::MODIFY) {
                    writes += 1;
                }
            }
            Err(rustix::io::Errno::AGAIN) => break,
            Err(error) => panic!("read transcript filesystem events: {error}"),
        }
    }
    assert!(
        writes > 0,
        "the send/interrupt flow must still be persisted"
    );
    let records = std::fs::read_to_string(&journal)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0]["sequence"].as_u64().unwrap()
                < pair[1]["sequence"].as_u64().unwrap())
    );
    let ids = records
        .iter()
        .map(|record| record["event_id"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), records.len());
    // held_text emits this assistant StreamEnd and no MessageAdded for the
    // request text. Persist the reply the client saw during the new turn.
    assert!(records.iter().any(|record| {
        matches!(
            serde_json::from_value::<ChatEvent>(record["event"].clone()).unwrap(),
            ChatEvent::StreamEnd(end) if end.message.content == "working after large transcript"
        )
    }));
}

#[tokio::test]
async fn interrupting_parent_keeps_agent_control_children_running() {
    let mut fixture = Fixture::new().await;

    let parent_reservation = fixture
        .reserve_next_mock_launch(
            "interrupt-parent",
            MockScript::one(MockTurn::held_text("parent waiting")),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("interrupt-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/interrupt-parent".to_owned()],
                prompt: "parent waiting".to_owned(),
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
    drop(parent_reservation);

    let env = expect_chat_event_on_stream(
        &mut fixture.client,
        &parent_new.instance_stream,
        "parent TypingStatusChanged(true)",
    )
    .await;
    let event: ChatEvent = env.parse_payload().expect("parse parent typing true");
    assert!(matches!(event, ChatEvent::TypingStatusChanged(true)));

    let caller = fixture.agent_control_caller(&parent_new.agent_id).await;
    let child_reservation = fixture
        .reserve_next_mock_launch(
            "agent-control-child",
            MockScript::one(MockTurn::text("agent-control child first response"))
                .then(MockTurn::text("agent-control child follow-up response")),
        )
        .await;
    let child_agent_id = mcp_spawn_agent_as(
        &caller,
        json!({
            "workspace_roots": ["/tmp/agent-control-mcp-parent-url"],
            "prompt": "agent-control child first",
            "backend_kind": "claude",
            "name": "agent-control-child"
        }),
    )
    .await;
    drop(child_reservation);

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
        "agent-control child first response",
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
        "agent-control child follow-up response",
    )
    .await;
}

#[tokio::test]
async fn backend_spawn_failure_emits_terminal_agent_error_without_panicking_host() {
    let mut fixture = Fixture::new().await;

    let reservation = fixture
        .reserve_next_mock_spawn_failure("spawn-failure", "mock backend forced spawn failure")
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("spawn-failure".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "trigger startup failure".to_owned(),
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
    drop(reservation);
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    expect_strict_mock_turn(
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

// Naming is actor-owned and off the spawn path so NewAgent and AgentStart are
// published under a provisional name before the generated rename arrives.
#[tokio::test]
async fn generated_name_publishes_provisionally_then_renames_authoritatively() {
    let mut fixture = Fixture::new().await;
    let gate = fixture.install_agent_name_test_gate().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "review logs __mock_async_generated_name__".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("send gated generated-name spawn");
    assert_eq!(gate.wait_until_entered().await, 0);
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "provisional NewAgent",
    )
    .await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse gated NewAgent");
    assert_eq!(new_agent.backend_kind, BackendKind::Claude);
    assert_ne!(new_agent.name, "Generated Async Name");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "provisional AgentStart",
    )
    .await;
    assert_eq!(start.name, new_agent.name);

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions while generated name is held");
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "provisional SessionList",
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse provisional SessionList");
    assert_eq!(
        list.sessions[0].alias.as_deref(),
        Some(new_agent.name.as_str())
    );
    assert_eq!(list.sessions[0].user_alias, None);

    gate.release_one();
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::AgentRenamed,
        "generated AgentRenamed",
    )
    .await;
    let renamed: AgentRenamedPayload = env.parse_payload().expect("parse generated rename");
    assert_eq!(renamed.agent_id, new_agent.agent_id);
    assert_eq!(renamed.name, "Generated Async Name");
    // The control-lane rename can overtake bulk-lane startup/ListSessions
    // snapshots: the failing run received the correct rename followed by
    // "Review Logs Mock-async-generated-name". Validate those provisional
    // snapshots and still require the generated alias within the deadline.
    let env = fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "generated SessionList",
        |env| {
            if env.kind != FrameKind::SessionList {
                return false;
            }
            let list: SessionListPayload = env.parse_payload().expect("parse rename snapshot");
            eprintln!("generated-name session snapshot: {list:?}");
            assert_eq!(list.sessions.len(), 1);
            assert_eq!(Some(&list.sessions[0].id), start.session_id.as_ref());
            assert_eq!(list.sessions[0].user_alias, None);
            let alias = list.sessions[0].alias.as_deref();
            if alias == Some("Generated Async Name") {
                return true;
            }
            assert_eq!(alias, Some(new_agent.name.as_str()));
            false
        },
    )
    .await;
    let list: SessionListPayload = env.parse_payload().expect("parse generated SessionList");
    assert_eq!(
        list.sessions[0].alias.as_deref(),
        Some("Generated Async Name")
    );
    assert_eq!(list.sessions[0].user_alias, None);
    assert_eq!(gate.wait_until_completed().await, 0);
}

#[tokio::test]
async fn user_rename_wins_race_with_generated_name() {
    let mut fixture = Fixture::new().await;
    let name_gate = fixture.install_agent_name_test_gate().await;
    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Rename Race Mock-async-generated-name");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "rename race __mock_async_generated_name__".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("send generated-name race spawn");
    assert_eq!(name_gate.wait_until_entered().await, 0);
    startup_gate.wait_until_entered().await;
    let env = expect_kind(&mut fixture.client, FrameKind::NewAgent, "race NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse race NewAgent");
    assert_eq!(new_agent.name, "Rename Race Mock-async-generated-name");

    fixture
        .client
        .set_agent_name(&new_agent.instance_stream, "User Wins".to_owned())
        .await
        .expect("send user rename before Bootstrap");
    startup_gate.wait_until_entered().await;

    name_gate.release_one();
    startup_gate.wait_until_entered().await;
    startup_gate.release_one();
    let bootstrap = expect_raw_agent_bootstrap_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "rename race AgentBootstrap",
    )
    .await;
    assert!(matches!(
        bootstrap.events.first(),
        Some(AgentBootstrapEvent::AgentStart(start))
            if start.name == "Rename Race Mock-async-generated-name"
    ));
    assert_eq!(name_gate.wait_until_completed().await, 0);

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("generated-name race barrier");
    let mut user_renames = 0;
    let mut saw_user_alias = false;
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "user rename notification and persisted alias",
        |env| {
            if env.kind == FrameKind::AgentRenamed {
                let renamed: AgentRenamedPayload = env.parse_payload().expect("parse race rename");
                assert_eq!(renamed.name, "User Wins");
                user_renames += 1;
            }
            if env.kind == FrameKind::SessionList {
                let list: SessionListPayload = env.parse_payload().expect("parse race SessionList");
                if list.sessions[0].user_alias.as_deref() == Some("User Wins") {
                    assert_eq!(
                        list.sessions[0].alias.as_deref(),
                        Some("Rename Race Mock-async-generated-name")
                    );
                    // Alias persistence can fan out SessionList before the agent
                    // emits AgentRenamed. A store snapshot is not a wire barrier
                    // for the independently delivered rename notification.
                    eprintln!("RENAME RACE alias snapshot received; rename_count={user_renames}");
                    saw_user_alias = true;
                }
            }
            saw_user_alias && user_renames > 0
        },
    )
    .await;
    assert_eq!(user_renames, 1, "pre-Bootstrap user rename must apply once");
}

#[tokio::test]
async fn spawn_operation_capacity_and_shutdown_are_typed_and_drained() {
    let mut fixture = Fixture::new().await;
    let gate = fixture
        .host_for_test()
        .install_spawn_operation_start_test_gate();
    let (active_limit, queue_capacity) = fixture.spawn_operation_limits_for_test();
    let spawn_payload = |index: usize| SpawnAgentPayload {
        launch_context: protocol::AgentLaunchContext::Standard,
        name: Some(format!("Capacity Spawn {index}")),
        custom_agent_id: None,
        parent_agent_id: None,
        project_id: None,
        params: SpawnAgentParams::New {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: format!("review logs {index}"),
            images: None,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            cost_hint: None,
            access_mode: Default::default(),
            session_settings: None,
        },
    };

    for index in 0..active_limit {
        fixture
            .client
            .spawn_agent(spawn_payload(index))
            .await
            .expect("submit active gated spawn");
    }
    for _ in 0..active_limit {
        gate.wait_until_entered().await;
    }
    for index in active_limit..=active_limit + queue_capacity {
        fixture
            .client
            .spawn_agent(spawn_payload(index))
            .await
            .expect("submit queued or saturated gated spawn");
    }
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("capacity routing barrier");

    let mut saturation_errors = 0;
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read capacity barrier event")
            .expect("connection remained open");
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("parse saturation error");
            assert_eq!(error.request_kind, FrameKind::SpawnAgent);
            assert_eq!(error.code, CommandErrorCode::Internal);
            assert!(error.message.contains("capacity unavailable"));
            saturation_errors += 1;
        }
        assert_ne!(env.kind, FrameKind::NewAgent);
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
    assert_eq!(saturation_errors, 1);
    fixture.shutdown_spawn_operations().await;
    let accepted = active_limit + queue_capacity;
    let mut shutdown_errors = 0;
    while shutdown_errors < accepted {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read shutdown terminal")
            .expect("connection remained open");
        assert_ne!(
            env.kind,
            FrameKind::NewAgent,
            "cancelled spawn must not publish"
        );
        if env.kind != FrameKind::CommandError {
            continue;
        }
        let error: CommandErrorPayload = env.parse_payload().expect("parse shutdown error");
        assert_eq!(error.request_kind, FrameKind::SpawnAgent);
        assert!(error.message.contains("host shut down"));
        shutdown_errors += 1;
    }
    assert_eq!(shutdown_errors, accepted);
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("post-shutdown publication barrier");
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read post-shutdown barrier")
            .expect("connection remained open");
        assert_ne!(
            env.kind,
            FrameKind::NewAgent,
            "drained spawn published after cancellation"
        );
        if env.kind == FrameKind::SessionList {
            let list: SessionListPayload = env.parse_payload().expect("parse shutdown SessionList");
            assert!(list.sessions.is_empty());
            break;
        }
    }

    let mut fixture = Fixture::new().await;
    let host = fixture.host_for_test();
    let (publication, cancelled) = host.install_next_agent_publication_start_test_gate().await;
    fixture
        .client
        .spawn_agent(spawn_payload(0))
        .await
        .expect("enqueue publication before owner execution");
    publication.wait_until_entered().await;
    let shutdown = tokio::spawn(async move {
        host.shutdown_spawn_operations().await;
    });
    cancelled
        .await
        .expect("publication reply cancelled before owner execution");
    publication.release_one();
    shutdown
        .await
        .expect("queued publication shutdown completed");
    let terminal = loop {
        let event = fixture
            .client
            .next_event()
            .await
            .expect("queued publication cancellation event")
            .expect("connection remains open");
        assert_ne!(
            event.kind,
            FrameKind::NewAgent,
            "cancelled unstarted publication must not send NewAgent"
        );
        if event.kind == FrameKind::CommandError {
            break event
                .parse_payload::<CommandErrorPayload>()
                .expect("queued publication terminal payload");
        }
    };
    assert_eq!(terminal.request_kind, FrameKind::SpawnAgent);
    assert!(terminal.message.contains("host shut down"));
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("drained publication barrier");
    loop {
        let event = fixture
            .client
            .next_event()
            .await
            .expect("drained publication event")
            .expect("connection remains open");
        assert_ne!(
            event.kind,
            FrameKind::NewAgent,
            "queued publication must not outlive its failure terminal"
        );
        if event.kind == FrameKind::CommandError {
            let error: CommandErrorPayload =
                event.parse_payload().expect("post-drain error payload");
            assert!(
                !error.message.contains("host shut down"),
                "one shutdown terminal per request"
            );
        }
        if event.kind == FrameKind::SessionList {
            break;
        }
    }
}

#[tokio::test]
async fn spawn_operation_panic_and_reconnect_have_one_typed_terminal() {
    let mut fixture = Fixture::new().await;
    let gate = fixture.install_agent_name_test_gate().await;
    let payload = SpawnAgentPayload {
        launch_context: protocol::AgentLaunchContext::Standard,
        name: None,
        custom_agent_id: None,
        parent_agent_id: None,
        project_id: None,
        params: SpawnAgentParams::New {
            workspace_roots: vec!["/tmp/test".to_owned()],
            prompt: "review reconnect logs".to_owned(),
            images: None,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            cost_hint: None,
            access_mode: Default::default(),
            session_settings: None,
        },
    };
    fixture
        .client
        .spawn_agent(payload.clone())
        .await
        .expect("submit reconnect spawn");
    assert_eq!(gate.wait_until_entered().await, 0);
    let new_agent = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "pre-reconnect exact-once NewAgent",
    )
    .await;
    let new_agent: NewAgentPayload = new_agent.parse_payload().expect("parse reconnect NewAgent");
    let (reconnected, bootstrap) = fixture.connect_with_bootstrap().await;
    let reconnected_agent = bootstrapped_agent(&bootstrap, &new_agent.agent_id);
    fixture.client = reconnected;
    gate.release_one();
    assert_eq!(gate.wait_until_completed().await, 0);
    let _ = expect_agent_start_on_stream(
        &mut fixture.client,
        &reconnected_agent.instance_stream,
        "reconnected AgentStart",
    )
    .await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("reconnect publication barrier");
    let mut duplicate_publications = 0;
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read reconnect barrier")
            .expect("connection remained open");
        if env.kind == FrameKind::NewAgent {
            duplicate_publications += 1;
        }
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
    assert_eq!(duplicate_publications, 0);

    let panic_gate = fixture
        .host_for_test()
        .install_spawn_operation_start_test_gate();
    let mut panic_payload = payload;
    panic_payload.name = Some("Controlled Spawn Panic".to_owned());
    fixture
        .client
        .spawn_agent(panic_payload)
        .await
        .expect("submit controlled panic spawn");
    panic_gate.wait_until_entered().await;
    panic_gate.panic_on_release();
    panic_gate.release_one();
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::CommandError,
        "controlled abnormal spawn terminal",
    )
    .await;
    let error: CommandErrorPayload = env.parse_payload().expect("parse abnormal terminal");
    assert_eq!(error.request_kind, FrameKind::SpawnAgent);
    assert_eq!(error.code, CommandErrorCode::Internal);
    assert!(error.message.contains("panicked"));
}

#[tokio::test]
async fn completed_spawn_is_not_reclassified_as_shutdown_cancellation() {
    let mut fixture = Fixture::new().await;
    let gate = fixture.install_spawn_operation_completion_test_gate();
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Completed Spawn".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "review completed spawn".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("submit spawn held after recording successful outcome");
    gate.wait_until_entered().await;

    fixture.shutdown_spawn_operations().await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("completed spawn shutdown barrier");

    let mut publications = 0;
    let mut terminals = 0;
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read completed spawn shutdown event")
            .expect("connection remained open");
        match env.kind {
            FrameKind::NewAgent => publications += 1,
            FrameKind::CommandError => terminals += 1,
            FrameKind::SessionList => break,
            _ => {}
        }
    }
    assert_eq!(
        publications, 1,
        "successful spawn must publish exactly once"
    );
    assert_eq!(
        terminals, 0,
        "successful spawn must not receive a shutdown terminal"
    );
}

#[tokio::test]
async fn published_spawn_is_not_reclassified_when_post_fanout_work_is_cancelled() {
    let mut fixture = Fixture::new().await;
    let gate = fixture.install_spawn_operation_publication_test_gate();
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Published Spawn".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "review published spawn".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("submit spawn held immediately after NewAgent publication");
    gate.wait_until_entered().await;

    fixture.shutdown_spawn_operations().await;
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("published spawn shutdown barrier");

    let mut publications = 0;
    let mut terminals = 0;
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read post-publication shutdown event")
            .expect("connection remained open");
        match env.kind {
            FrameKind::NewAgent => publications += 1,
            FrameKind::CommandError => terminals += 1,
            FrameKind::SessionList => break,
            _ => {}
        }
    }
    assert_eq!(
        publications, 1,
        "published spawn must remain visible exactly once"
    );
    assert_eq!(
        terminals, 0,
        "post-publication cancellation must not emit an error"
    );
}

#[tokio::test]
async fn detached_spawn_bootstraps_before_follow_up_is_admitted() {
    let mut fixture = Fixture::new().await;
    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Bootstrap First");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Bootstrap First".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "initial detached spawn turn".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("submit spawn held during backend startup");
    startup_gate.wait_until_entered().await;

    let env = expect_kind(&mut fixture.client, FrameKind::NewAgent, "early NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgent");
    for message in ["first immediate follow-up", "second immediate follow-up"] {
        fixture
            .client
            .send_message(&new_agent.instance_stream, message.to_owned())
            .await
            .expect("send immediate follow-up during startup");
    }
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions while advertised agent is still starting");
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read command while advertised agent is starting")
            .expect("connection remained open");
        assert_ne!(env.kind, FrameKind::CommandError);
        assert_ne!(env.kind, FrameKind::AgentError);
        if env.stream == new_agent.instance_stream {
            assert_ne!(
                env.kind,
                FrameKind::AgentBootstrap,
                "startup gate must still hold the actor bootstrap"
            );
        }
        if env.kind == FrameKind::SessionList {
            break;
        }
    }

    startup_gate.release_one();
    let bootstrap = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read startup AgentBootstrap")
            .expect("connection remained open");
        assert_ne!(env.kind, FrameKind::CommandError);
        if env.stream != new_agent.instance_stream {
            continue;
        }
        assert_eq!(
            env.kind,
            FrameKind::AgentBootstrap,
            "AgentBootstrap must be the first frame on the advertised agent stream"
        );
        assert_eq!(env.seq, 0);
        break env
            .parse_payload::<AgentBootstrapPayload>()
            .expect("parse startup AgentBootstrap");
    };
    assert!(
        bootstrap
            .events
            .iter()
            .all(|event| !matches!(event, AgentBootstrapEvent::AgentError(_))),
        "startup input admission must not synthesize an error"
    );

    let mock_turn_text = |prompt: &str| {
        format!(
            "[startup_mcp_servers: tyde-agent-control(http), tyde-agent-await(http), tyde-config(http)] \
             [steering: {}] [builtin_steering: {}] mock backend response to: {prompt}",
            fixture.bootstrap.steering[0].content.replace('\n', "\\n"),
            // The mock's startup summary renders newlines as literal \n.
            format!("{}\n\n{}", server::backend::AGENT_CONTROL_SPAWN_STEERING, "Tyde permissions are assigned by the human, independent of who created an agent or its parent. Before controlling Tyde, call tyde_config_get_permissions and obey its current role guidance. Ordinary agents work on their own task and children, not sideways build/commit coordination. Role changes are metadata, not steering entries or permission to interrupt anyone. This guidance does not grant operating-system privileges or waive confirmations.").replace('\n', "\\n")
        )
    };
    let expected = [
        mock_turn_text("initial detached spawn turn"),
        mock_turn_text("first immediate follow-up"),
        mock_turn_text("second immediate follow-up"),
    ];
    let mut completed = Vec::new();
    while completed.len() < expected.len() {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read live startup and pending-input turns")
            .expect("connection remained open");
        assert_ne!(env.kind, FrameKind::CommandError);
        if env.stream != new_agent.instance_stream {
            continue;
        }
        assert_ne!(env.kind, FrameKind::AgentError);
        if env.kind != FrameKind::ChatEvent {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("parse live ChatEvent");
        if let ChatEvent::StreamEnd(end) = event {
            completed.push(end.message.content);
        }
    }
    assert_eq!(
        completed, expected,
        "initial turn must stay first and startup inputs must drain FIFO"
    );
}

#[tokio::test]
async fn startup_fanout_advertises_all_subscribers_before_any_attach_wait() {
    let mut fixture = Fixture::new().await;
    let (mut second_client, second_bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(second_bootstrap.agents.is_empty());
    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Two Subscriber Startup");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Two Subscriber Startup".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "held two-subscriber startup".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn held two-subscriber agent");
    startup_gate.wait_until_entered().await;

    let first_env = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "first subscriber early NewAgent",
    )
    .await;
    let first: NewAgentPayload = first_env.parse_payload().expect("parse first NewAgent");
    let second_env = expect_kind(
        &mut second_client,
        FrameKind::NewAgent,
        "second subscriber early NewAgent",
    )
    .await;
    let second: NewAgentPayload = second_env.parse_payload().expect("parse second NewAgent");
    assert_eq!(second.agent_id, first.agent_id);

    for (client, stream, label) in [
        (
            &mut fixture.client,
            first.instance_stream.clone(),
            "first subscriber",
        ),
        (
            &mut second_client,
            second.instance_stream.clone(),
            "second subscriber",
        ),
    ] {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, client.next_event()).await {
                Err(_) => break,
                Ok(Ok(Some(env))) if env.stream != stream => continue,
                Ok(Ok(Some(env))) => panic!(
                    "{label} received pre-bootstrap agent event {} while startup was held",
                    env.kind
                ),
                Ok(Ok(None)) => panic!("{label} connection closed while startup was held"),
                Ok(Err(error)) => panic!("{label} read failed while startup was held: {error:?}"),
            }
        }
    }

    startup_gate.release_one();
    for (client, stream, label) in [
        (
            &mut fixture.client,
            first.instance_stream.clone(),
            "first subscriber",
        ),
        (
            &mut second_client,
            second.instance_stream.clone(),
            "second subscriber",
        ),
    ] {
        loop {
            let env = client
                .next_event()
                .await
                .expect("read released startup event")
                .expect("connection remained open");
            if env.stream != stream {
                continue;
            }
            assert_eq!(env.kind, FrameKind::AgentBootstrap, "{label}");
            assert_eq!(env.seq, 0, "{label} Bootstrap must be seq0");
            let bootstrap: AgentBootstrapPayload =
                env.parse_payload().expect("parse released AgentBootstrap");
            assert!(matches!(
                bootstrap.events.first(),
                Some(AgentBootstrapEvent::AgentStart(_))
            ));
            break;
        }
    }
}

#[tokio::test]
async fn cancelling_an_unclaimed_creation_reply_removes_its_agent() {
    for fail_cleanup in [false, true] {
        let mut fixture = Fixture::new().await;
        let host = fixture.host_for_test();
        let (handoff, discarded, cleaned) =
            host.install_next_agent_creation_reply_test_gate().await;
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                launch_context: protocol::AgentLaunchContext::Standard,
                name: Some("Unclaimed creation".into()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots: vec!["/tmp/test".into()],
                    prompt: "cancel before creation ownership transfers".into(),
                    images: None,
                    backend_kind: BackendKind::Claude,
                    launch_profile_id: None,
                    cost_hint: None,
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await
            .expect("request unclaimed creation");
        handoff.wait_until_entered().await;
        let (mut pending, bootstrap) = fixture.connect_with_bootstrap().await;
        assert!(
            bootstrap.agents.is_empty(),
            "creation has not published NewAgent"
        );
        assert!(
            !bootstrap.task_token_usages.is_empty(),
            "pending actor retains usage ownership"
        );
        for usage in &bootstrap.task_token_usages {
            for entry in &usage.breakdown {
                assert_eq!(
                    entry.navigation,
                    protocol::TaskUsageNavigation::Unavailable,
                    "a pending creation reply must not expose a missing live chat"
                );
            }
        }
        pending
            .list_sessions(ListSessionsPayload::default())
            .await
            .expect("pending bootstrap completion barrier");
        loop {
            let frame = pending
                .next_event()
                .await
                .expect("pending publication event")
                .expect("pending publication connection stays open");
            assert_ne!(
                frame.kind,
                FrameKind::NewAgent,
                "creation reply has not been claimed"
            );
            if frame.kind == FrameKind::TaskTokenUsage {
                let usage: protocol::TaskTokenUsagePayload =
                    frame.parse_payload().expect("pending usage payload");
                for entry in usage.breakdown {
                    assert_eq!(
                        entry.navigation,
                        protocol::TaskUsageNavigation::Unavailable,
                        "pending live publication must not expose a missing chat"
                    );
                }
            }
            if frame.kind == FrameKind::SessionList {
                break;
            }
        }
        let failure = if fail_cleanup {
            let agent_id = host
                .agent_ids()
                .await
                .into_iter()
                .next()
                .expect("unclaimed actor exists");
            host.wait_for_agent_startup_for_test(&agent_id)
                .await
                .expect("actual actor startup completed");
            let withdrawal = host.install_restore_marker_withdraw_test_gate().await;
            fixture.shutdown_spawn_operations().await;
            discarded.await.expect("creation reply discarded");
            withdrawal.wait_until_entered().await;
            let failure = fixture.fail_next_session_commit();
            withdrawal.release_one();
            Some((failure, withdrawal))
        } else {
            fixture.shutdown_spawn_operations().await;
            discarded.await.expect("creation reply discarded");
            None
        };
        drop(cleaned.await.expect("unclaimed cleanup settlement"));
        if fail_cleanup {
            loop {
                let frame = expect_kind(
                    &mut fixture.client,
                    FrameKind::CommandError,
                    "unclaimed cleanup failure",
                )
                .await;
                let error: CommandErrorPayload =
                    frame.parse_payload().expect("typed cleanup error");
                if error.operation == "cancel_unclaimed_agent" {
                    assert_eq!(error.code, CommandErrorCode::Internal);
                    assert_eq!(error.request_kind, FrameKind::SpawnAgent);
                    break;
                }
            }
            let (mut late, bootstrap) = fixture.connect_with_bootstrap().await;
            assert!(
                !bootstrap.task_token_usages.is_empty(),
                "failed cleanup must retain canonical ownership"
            );
            for usage in &bootstrap.task_token_usages {
                for entry in &usage.breakdown {
                    assert_eq!(
                        entry.navigation,
                        protocol::TaskUsageNavigation::Unavailable,
                        "unpublished retained cleanup must not enable a missing live chat"
                    );
                }
            }
            let frame = expect_kind(
                &mut late,
                FrameKind::CommandError,
                "retained cleanup failure replay",
            )
            .await;
            let error: CommandErrorPayload = frame
                .parse_payload()
                .expect("typed replayed cleanup failure");
            assert_eq!(error.operation, "cancel_unclaimed_agent");
            let retained_stream = error.stream;
            drop(failure);
            // An unpublished failure has no AgentStart/client sequence counter.
            // Recover through the typed error's explicit stream, not a fabricated advertisement.
            let close = Envelope::from_payload(
                retained_stream,
                FrameKind::CloseAgent,
                0,
                &protocol::CloseAgentPayload::default(),
            )
            .expect("serialize retained cleanup close");
            protocol::write_envelope(&mut late.writer, &close)
                .await
                .expect("retry retained cleanup after storage recovery");
            expect_kind(
                &mut late,
                FrameKind::AgentClosed,
                "recovered unclaimed cleanup",
            )
            .await;
            let (mut recovered, bootstrap) = fixture.connect_with_bootstrap().await;
            assert!(
                bootstrap.task_token_usages.is_empty(),
                "successful close removes retained ownership"
            );
            recovered
                .list_sessions(ListSessionsPayload::default())
                .await
                .expect("recovered bootstrap publication barrier");
            loop {
                let frame = recovered
                    .next_event()
                    .await
                    .expect("recovered state event")
                    .expect("recovered connection remains open");
                if frame.kind == FrameKind::CommandError {
                    let error: CommandErrorPayload =
                        frame.parse_payload().expect("typed post-recovery error");
                    assert_ne!(
                        error.operation, "cancel_unclaimed_agent",
                        "completed cleanup must not replay a stale failure"
                    );
                }
                if frame.kind == FrameKind::SessionList {
                    break;
                }
            }
            continue;
        }
        let (_, bootstrap) = fixture.connect_with_bootstrap().await;
        assert!(
            bootstrap.agents.is_empty(),
            "cancelled creation must not leave a live agent"
        );
        assert!(
            bootstrap.task_token_usages.is_empty(),
            "cancelled creation must not leave a live usage root"
        );
    }
}

#[tokio::test]
async fn cancelling_a_prepared_fanout_releases_subscriber_ordering_holds() {
    let mut fixture = Fixture::new().await;
    let (root, root_session) = spawn_named_session(
        &mut fixture,
        "Fanout cancellation root",
        BackendKind::Claude,
    )
    .await;
    let (mut observer, _) = fixture.connect_with_bootstrap().await;
    observer
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("subscribe observer to counts");
    let initial = expect_kind(
        &mut observer,
        FrameKind::SessionList,
        "observer session baseline",
    )
    .await;
    let initial: SessionListPayload = initial.parse_payload().expect("observer baseline payload");
    let before = initial
        .sessions
        .iter()
        .find(|session| session.id == root_session)
        .expect("root saved session")
        .message_count;
    let host = fixture.host_for_test();
    let (handoff, discarded) = host.install_next_fanout_handoff_test_gate().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Cancelled fanout child".into()),
            custom_agent_id: None,
            parent_agent_id: Some(root.agent_id.clone()),
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".into()],
                prompt: "cancel before publication".into(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("start fanout cancellation");
    handoff.wait_until_entered().await;
    fixture.shutdown_spawn_operations().await;
    discarded
        .await
        .expect("discarded reply enqueues hold release");
    fixture
        .client
        .send_message(&root.instance_stream, "after cancelled fanout".into())
        .await
        .expect("continue existing agent");
    loop {
        let frame = observer
            .next_event()
            .await
            .expect("read observer after fanout cancellation")
            .expect("observer remains connected");
        if frame.kind != FrameKind::SessionSummaryCountUpdated {
            continue;
        }
        let count: protocol::SessionSummaryCountUpdatedPayload =
            frame.parse_payload().expect("typed session count");
        if count.session_id == root_session && count.assistant_turn_count > before {
            break;
        }
    }
}

#[tokio::test]
async fn held_resume_does_not_block_later_connection_request() {
    let mut fixture = Fixture::new().await;
    let (original, session_id) =
        spawn_named_session(&mut fixture, "Resume HOL Source", BackendKind::Claude).await;
    fixture
        .client
        .close_agent(&original.instance_stream)
        .await
        .expect("close resume source");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "resume source AgentClosed",
    )
    .await;
    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(50),
        "resume source close drain",
    )
    .await;

    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Held Resume");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Held Resume".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: Some("resume while startup is held".to_owned()),
            },
        })
        .await
        .expect("send held resume");
    startup_gate.wait_until_entered().await;
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "held resume NewAgent",
    )
    .await;
    let resumed: NewAgentPayload = env.parse_payload().expect("parse held resume NewAgent");
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions behind held resume");
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read request behind held resume")
            .expect("connection remained open");
        if env.stream == resumed.instance_stream {
            panic!("held resume emitted {} before Bootstrap release", env.kind);
        }
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
    startup_gate.release_one();
}

#[tokio::test]
async fn held_fork_does_not_block_later_connection_request() {
    let mut fixture = Fixture::new().await;
    let (_source, session_id) =
        spawn_named_session(&mut fixture, "Fork HOL Source", BackendKind::Claude).await;
    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Held Fork");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Held Fork".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Fork {
                from_session_id: session_id,
                prompt: "fork while startup is held".to_owned(),
                images: None,
                access_mode: Default::default(),
            },
        })
        .await
        .expect("send held fork");
    startup_gate.wait_until_entered().await;
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "held fork NewAgent",
    )
    .await;
    let forked: NewAgentPayload = env.parse_payload().expect("parse held fork NewAgent");
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list sessions behind held fork");
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read request behind held fork")
            .expect("connection remained open");
        if env.stream == forked.instance_stream {
            panic!("held fork emitted {} before Bootstrap release", env.kind);
        }
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
    startup_gate.release_one();
}

/// An authoritatively Unavailable session schema surfaces as exactly one
/// fatal typed AgentError inside the seq-0 AgentBootstrap (never a router
/// CommandError) for both New spawns and Resumes, and repeated attempts of
/// either kind never trigger another schema probe.
///
/// The two lifecycle ops deliberately run on separate hosts: the New phase
/// uses a cold fixture where Unavailable is installed before any Codex use
/// (first-contact failure — the original condition of the former
/// `unavailable_new_schema_without_settings_is_typed_and_not_reprobed`,
/// preserved so a regression that depends on a previously warmed
/// schema/session path cannot mask it), and the Resume phase runs on the
/// fixture that owns the source session.
#[tokio::test]
async fn unavailable_session_schema_is_typed_and_not_reprobed_for_new_and_resume() {
    {
        let mut fixture = Fixture::new().await;
        let host = fixture.host_for_test();
        host.set_session_schema_unavailable_for_test(
            BackendKind::Codex,
            "controlled unavailable New schema",
        )
        .await;
        let probe_count = host.session_schema_probe_count_for_test().await;

        for attempt in 0..2 {
            fixture
                .client
                .spawn_agent(SpawnAgentPayload {
                    launch_context: protocol::AgentLaunchContext::Standard,
                    name: Some(format!("Unavailable New {attempt}")),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: vec!["/tmp/test".to_owned()],
                        prompt: format!("unavailable New attempt {attempt}"),
                        images: None,
                        backend_kind: BackendKind::Codex,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
                .expect("send unavailable-schema New");
            let env = expect_kind(
                &mut fixture.client,
                FrameKind::NewAgent,
                "unavailable-schema NewAgent",
            )
            .await;
            let spawned: NewAgentPayload = env
                .parse_payload()
                .expect("parse unavailable-schema NewAgent");
            let bootstrap = loop {
                let env = fixture
                    .client
                    .next_event()
                    .await
                    .expect("read unavailable-schema New AgentBootstrap")
                    .expect("connection remained open");
                assert_ne!(
                    env.kind,
                    FrameKind::CommandError,
                    "schema startup failure must not be a router frame"
                );
                if env.stream != spawned.instance_stream {
                    continue;
                }
                assert_eq!(env.kind, FrameKind::AgentBootstrap);
                assert_eq!(env.seq, 0);
                break env
                    .parse_payload::<AgentBootstrapPayload>()
                    .expect("parse unavailable-schema New AgentBootstrap");
            };
            let errors = bootstrap
                .events
                .iter()
                .filter_map(|event| match event {
                    AgentBootstrapEvent::AgentError(error) => Some(error),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                errors.len(),
                1,
                "schema failure must be emitted exactly once"
            );
            assert!(errors[0].fatal);
            assert_eq!(errors[0].code, AgentErrorCode::BackendFailed);
            assert!(
                errors[0]
                    .message
                    .contains("controlled unavailable New schema")
            );
            assert_eq!(
                host.session_schema_probe_count_for_test().await,
                probe_count,
                "authoritative Unavailable schema must not trigger another New probe"
            );
        }
    }

    let mut fixture = Fixture::new().await;
    let host = fixture.host_for_test();
    host.set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    let (original, session_id) = spawn_named_session(
        &mut fixture,
        "Unavailable Schema Source",
        BackendKind::Codex,
    )
    .await;
    fixture
        .client
        .close_agent(&original.instance_stream)
        .await
        .expect("close unavailable-schema source");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "unavailable-schema source AgentClosed",
    )
    .await;
    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(50),
        "unavailable-schema source close drain",
    )
    .await;

    host.set_session_schema_unavailable_for_test(
        BackendKind::Codex,
        "controlled unavailable schema",
    )
    .await;
    let probe_count = host.session_schema_probe_count_for_test().await;

    for attempt in 0..2 {
        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                launch_context: protocol::AgentLaunchContext::Standard,
                name: Some(format!("Unavailable Resume {attempt}")),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::Resume {
                    session_id: session_id.clone(),
                    prompt: Some(format!("unavailable attempt {attempt}")),
                },
            })
            .await
            .expect("send unavailable-schema resume");
        let env = expect_kind(
            &mut fixture.client,
            FrameKind::NewAgent,
            "unavailable-schema NewAgent",
        )
        .await;
        let resumed: NewAgentPayload = env
            .parse_payload()
            .expect("parse unavailable-schema NewAgent");
        let bootstrap = expect_raw_agent_bootstrap_on_stream(
            &mut fixture.client,
            &resumed.instance_stream,
            "unavailable-schema AgentBootstrap",
        )
        .await;
        let error = bootstrap
            .events
            .iter()
            .find_map(|event| match event {
                AgentBootstrapEvent::AgentError(error) => Some(error),
                _ => None,
            })
            .expect("unavailable schema must be fatal in Bootstrap");
        assert!(error.fatal);
        assert_eq!(error.code, AgentErrorCode::BackendFailed);
        assert!(error.message.contains("controlled unavailable schema"));
        assert_eq!(
            host.session_schema_probe_count_for_test().await,
            probe_count,
            "authoritative Unavailable schema must not trigger another probe"
        );
    }
}

#[tokio::test]
async fn terminal_startup_failure_bootstraps_before_immediate_rejection() {
    let mut fixture = Fixture::new().await;
    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Terminal Bootstrap First");
    let reservation = fixture
        .reserve_next_mock_spawn_failure(
            "Terminal Bootstrap First",
            "mock backend forced spawn failure",
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Terminal Bootstrap First".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "trigger startup failure".to_owned(),
                images: None,
                backend_kind: BackendKind::Tycode,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("submit terminal startup failure");
    startup_gate.wait_until_entered().await;

    let env = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "terminal startup-failure NewAgent",
    )
    .await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse terminal NewAgent");
    fixture
        .client
        .send_message(
            &new_agent.instance_stream,
            "immediate terminal follow-up".to_owned(),
        )
        .await
        .expect("enqueue immediate input for terminal actor");
    startup_gate.release_one();

    let bootstrap = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read terminal AgentBootstrap")
            .expect("connection remained open");
        assert_ne!(
            env.kind,
            FrameKind::CommandError,
            "terminal input rejection must not be a router frame"
        );
        if env.stream != new_agent.instance_stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::AgentBootstrap);
        assert_eq!(env.seq, 0);
        break env
            .parse_payload::<AgentBootstrapPayload>()
            .expect("parse terminal AgentBootstrap");
    };
    assert_eq!(
        bootstrap
            .events
            .iter()
            .filter(|event| matches!(
                event,
                AgentBootstrapEvent::AgentError(error)
                    if error.fatal
                        && error.code == AgentErrorCode::BackendFailed
                        && error.message.contains("mock backend forced spawn failure")
            ))
            .count(),
        1,
        "terminal Bootstrap must contain the fatal startup error exactly once"
    );
    // Clients render an agent's settings from its SessionSettings snapshot;
    // without one a failed agent's settings read as loading forever.
    assert_eq!(
        bootstrap
            .events
            .iter()
            .filter(|event| matches!(
                event,
                AgentBootstrapEvent::SessionSettings(settings) if settings.schema.is_none()
            ))
            .count(),
        1,
        "terminal Bootstrap must say the failed agent has no editable settings schema"
    );
    assert!(
        bootstrap.events.iter().all(|event| !matches!(
            event,
            AgentBootstrapEvent::AgentError(error)
                if !error.fatal && error.message == "agent not running"
        )),
        "the immediate input rejection must follow Bootstrap as a live actor event"
    );
    drop(reservation);

    let rejection = loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read live terminal input rejection")
            .expect("connection remained open");
        assert_ne!(
            env.kind,
            FrameKind::CommandError,
            "terminal input rejection must never come from the router"
        );
        if env.stream != new_agent.instance_stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::AgentError);
        break env
            .parse_payload::<protocol::AgentErrorPayload>()
            .expect("parse live terminal rejection");
    };
    assert_eq!(rejection.agent_id, new_agent.agent_id);
    assert_eq!(rejection.code, AgentErrorCode::Internal);
    assert_eq!(rejection.message, "agent not running");
    assert!(!rejection.fatal);

    let (mut late_client, late_host_bootstrap) = fixture.connect_with_bootstrap().await;
    let late_agent = bootstrapped_agent(&late_host_bootstrap, &new_agent.agent_id);
    let late_bootstrap = loop {
        let env = late_client
            .next_event()
            .await
            .expect("read late terminal replay")
            .expect("late connection remained open");
        if env.stream != late_agent.instance_stream {
            continue;
        }
        assert_eq!(env.kind, FrameKind::AgentBootstrap);
        assert_eq!(env.seq, 0);
        break env
            .parse_payload::<AgentBootstrapPayload>()
            .expect("parse late terminal AgentBootstrap");
    };
    assert_eq!(
        late_bootstrap
            .events
            .iter()
            .filter(|event| matches!(
                event,
                AgentBootstrapEvent::AgentError(error)
                    if error.fatal
                        && error.code == AgentErrorCode::BackendFailed
                        && error.message.contains("mock backend forced spawn failure")
            ))
            .count(),
        1
    );
    assert_eq!(
        late_bootstrap
            .events
            .iter()
            .filter(|event| matches!(
                event,
                AgentBootstrapEvent::AgentError(error)
                    if !error.fatal
                        && error.code == AgentErrorCode::Internal
                        && error.message == "agent not running"
            ))
            .count(),
        1,
        "late replay proves the live rejection came from the actor log, not the router"
    );
    assert_eq!(
        late_bootstrap
            .events
            .iter()
            .filter(|event| matches!(
                event,
                AgentBootstrapEvent::SessionSettings(settings) if settings.schema.is_none()
            ))
            .count(),
        1,
        "late replay must carry the failed agent's settings snapshot"
    );

    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("terminal exact-once barrier");
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read terminal exact-once barrier")
            .expect("connection remained open");
        assert_ne!(env.kind, FrameKind::CommandError);
        if env.stream == new_agent.instance_stream {
            assert_ne!(env.kind, FrameKind::AgentBootstrap);
            assert_ne!(
                env.kind,
                FrameKind::AgentError,
                "one input must append exactly one typed rejection"
            );
        }
        if env.kind == FrameKind::SessionList {
            break;
        }
    }
}

#[tokio::test]
async fn concurrent_spawn_shutdown_waiters_share_one_drain() {
    let fixture = Fixture::new().await;
    let gate = fixture.install_spawn_operation_drain_test_gate();
    let first_host = fixture.host_for_test();
    let second_host = fixture.host_for_test();
    let first = tokio::spawn(async move {
        first_host.shutdown_spawn_operations().await;
    });
    let second = tokio::spawn(async move {
        second_host.shutdown_spawn_operations().await;
    });

    gate.wait_until_entered().await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    gate.release_one();
    first.await.expect("first shutdown waiter completed");
    second.await.expect("second shutdown waiter completed");
}

#[tokio::test]
async fn cancelled_first_spawn_shutdown_waiter_does_not_own_drain() {
    let fixture = Fixture::new().await;
    let gate = fixture.install_spawn_operation_drain_test_gate();
    let first_host = fixture.host_for_test();
    let first = tokio::spawn(async move {
        first_host.shutdown_spawn_operations().await;
    });

    gate.wait_until_entered().await;
    first.abort();
    assert!(
        first
            .await
            .expect_err("first shutdown waiter was cancelled")
            .is_cancelled()
    );

    let second_host = fixture.host_for_test();
    let second = tokio::spawn(async move {
        second_host.shutdown_spawn_operations().await;
    });
    assert!(!second.is_finished());
    gate.release_one();
    second.await.expect("replacement shutdown waiter completed");
}

#[cfg(unix)]
#[tokio::test]
async fn uds_host_owner_waits_for_spawn_operation_drain_on_entry_failure() {
    let fixture = Fixture::new().await;
    let gate = fixture.install_spawn_operation_drain_test_gate();
    let occupied_path = tempfile::NamedTempFile::new().expect("create occupied UDS path");
    let socket_path = occupied_path.path().to_path_buf();
    let host = fixture.host_for_test();
    let listener = tokio::spawn(async move {
        server::listen_uds(socket_path, server::ServerConfig::current(), host).await
    });

    gate.wait_until_entered().await;
    assert!(
        !listener.is_finished(),
        "UDS lifecycle owner returned before the shared drain completed"
    );
    gate.release_one();
    assert!(
        listener
            .await
            .expect("UDS lifecycle task completed")
            .is_err(),
        "occupied UDS path must preserve the entry-point error"
    );
}

#[tokio::test]
async fn failed_generated_name_keeps_fallback_without_user_error() {
    let mut fixture = Fixture::new().await;
    let gate = fixture.install_agent_name_test_gate().await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: None,
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
                prompt: "__mock_fail_agent_name__".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("failed generated-name spawn write");
    assert_eq!(gate.wait_until_entered().await, 0);
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "name failure provisional NewAgent",
    )
    .await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse provisional NewAgent");
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "name failure AgentStart",
    )
    .await;
    assert_eq!(start.name, new_agent.name);

    gate.release_one();
    assert_eq!(gate.wait_until_completed().await, 0);
    fixture
        .client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("name failure publication barrier");
    loop {
        let env = fixture
            .client
            .next_event()
            .await
            .expect("read name failure publication barrier")
            .expect("connection remained open");
        assert!(
            env.kind != FrameKind::AgentError || env.stream != new_agent.instance_stream,
            "optional name generation failure must not enter the conversation"
        );
        if env.kind == FrameKind::SessionList {
            break;
        }
    }

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let late_agent = bootstrapped_agent(&bootstrap, &new_agent.agent_id);
    assert_eq!(late_agent.name, new_agent.name);
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &late_agent.instance_stream,
        "fallback generated-name replay",
    )
    .await;
    assert!(replay.events.iter().all(|event| !matches!(
        event,
        AgentBootstrapEvent::AgentError(error)
            if error.message.contains("mock agent name generation failure")
    )));
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

    let agent = fixture.spawn("summary-off", "summaries are disabled").await;
    assert!(matches!(
        agent.new_agent.activity_summary,
        AgentActivitySummaryState::Disabled
    ));

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
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
        vec![agent.new_agent.agent_id],
        "disabled summarizer must not register background agents"
    );
}

#[tokio::test]
async fn agent_activity_summaries_emit_fresh_mock_state_and_bootstrap() {
    let mut fixture = Fixture::new().await;
    set_activity_summaries(&mut fixture.client, true).await;

    let agent = fixture
        .spawn("summary-on", "summarize recent activity")
        .await;
    assert!(matches!(
        agent.new_agent.activity_summary,
        AgentActivitySummaryState::Empty
    ));

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: summarize recent activity",
    )
    .await;

    let pending = expect_activity_summary_matching(
        &mut fixture.client,
        &agent.new_agent.agent_id,
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
        &agent.new_agent.agent_id,
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
        vec![agent.new_agent.agent_id.clone()],
        "summary helper must not register transient agents"
    );

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrapped_agent(&bootstrap, &agent.new_agent.agent_id);
    assert!(matches!(
        replayed.activity_summary,
        AgentActivitySummaryState::Fresh { .. }
    ));

    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close summarized agent");
    let env = fixture
        .next_frame_matching("summarized AgentClosed", |env| {
            env.kind == FrameKind::AgentClosed
        })
        .await;
    let closed: AgentClosedPayload = env.parse_payload().expect("parse AgentClosed");
    assert_eq!(closed.agent_id, agent.new_agent.agent_id);
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
    let agent = fixture.spawn("summary-offswitch", "keep working").await;
    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: keep working",
    )
    .await;
    let (mut observer, _) = fixture.connect_with_bootstrap().await;
    let (generation, settled) = fixture
        .host_for_test()
        .install_next_activity_summary_generation_test_gate()
        .await;
    set_activity_summaries(&mut fixture.client, true).await;
    generation.wait_until_entered().await;
    eprintln!("SUMMARY OFFSWITCH phase=generation-held");
    let pending = expect_activity_summary_matching(
        &mut observer,
        &agent.new_agent.agent_id,
        "held Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;
    assert!(matches!(
        pending.state,
        AgentActivitySummaryState::Pending { .. }
    ));

    // HostSettings -> Disabled -> SettingsWriteResult is valid ordered delivery.
    // The write-result helper consumes intervening frames, so observe summary
    // state on another real connection rather than wait for a discarded Disabled.
    set_activity_summaries(&mut fixture.client, false).await;
    let disabled = expect_activity_summary_matching(
        &mut observer,
        &agent.new_agent.agent_id,
        "Disabled",
        |state| matches!(state, AgentActivitySummaryState::Disabled),
    )
    .await;
    assert!(matches!(
        disabled.state,
        AgentActivitySummaryState::Disabled
    ));
    eprintln!("SUMMARY OFFSWITCH phase=disabled-observed");
    generation.release_one();
    settled
        .await
        .expect("summary result handled after off switch");
    eprintln!("SUMMARY OFFSWITCH phase=result-settled");
    observer
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("summary publication barrier");
    loop {
        let frame = observer
            .next_event()
            .await
            .expect("off-switch completion event")
            .expect("off-switch connection remains open");
        assert_ne!(
            frame.kind,
            FrameKind::NewAgent,
            "summary helpers must remain unregistered"
        );
        if frame.kind == FrameKind::AgentActivitySummary {
            let summary: AgentActivitySummaryPayload =
                frame.parse_payload().expect("post-switch summary");
            assert_eq!(summary.agent_id, agent.new_agent.agent_id);
            assert!(
                matches!(summary.state, AgentActivitySummaryState::Disabled),
                "an in-flight result must not replace Disabled after completion"
            );
        }
        if frame.kind == FrameKind::SessionList {
            break;
        }
    }
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        bootstrap.agents.len(),
        1,
        "off switch must not register helper agents"
    );
    let replayed = bootstrapped_agent(&bootstrap, &agent.new_agent.agent_id);
    assert!(matches!(
        replayed.activity_summary,
        AgentActivitySummaryState::Disabled
    ));
    eprintln!("SUMMARY OFFSWITCH phase=disabled-replayed");
}

#[tokio::test]
async fn agent_activity_summaries_error_state_backs_off_retries() {
    let mut fixture = Fixture::new().await;
    set_activity_summaries(&mut fixture.client, true).await;

    // The assistant's echoed startup instructions exceed the 1200-character
    // activity excerpt. Emit the real user bubble so the failure scenario does
    // not depend on finding its marker beyond that intentional truncation.
    let reservation = fixture
        .reserve_next_mock_launch(
            "summary-error",
            MockScript::new().with_unbounded_echo().with_user_bubbles(),
        )
        .await;
    let agent = fixture
        .spawn(
            "summary-error",
            "__mock_fail_activity_summary__ keep working",
        )
        .await;
    drop(reservation);
    let user = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &agent.stream,
        "summary failure user input",
        |event| matches!(event, ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User)),
    )
    .await;
    assert!(
        matches!(user, ChatEvent::MessageAdded(message)
        if matches!(message.sender, MessageSender::User) && message.content.contains("__mock_fail_activity_summary__")),
        "the real user message must carry the summary failure scenario"
    );

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: __mock_fail_activity_summary__ keep working",
    )
    .await;
    let _pending = expect_activity_summary_matching(
        &mut fixture.client,
        &agent.new_agent.agent_id,
        "error Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;

    let error = expect_activity_summary_matching(
        &mut fixture.client,
        &agent.new_agent.agent_id,
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
        .send_message(&agent.stream, "activity after summary failure".to_owned())
        .await
        .expect("send follow-up after summary failure failed");
    let user = fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &agent.stream,
        "summary failure follow-up input",
        |event| matches!(event, ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User)),
    )
    .await;
    assert!(
        matches!(user, ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User)),
        "follow-up retains its real user bubble"
    );
    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
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

    let agent = fixture
        .spawn("summary-unchanged", "summarize unchanged history")
        .await;

    expect_strict_mock_turn(
        &mut fixture.client,
        &agent.stream,
        "mock backend response to: summarize unchanged history",
    )
    .await;
    let _pending = expect_activity_summary_matching(
        &mut fixture.client,
        &agent.new_agent.agent_id,
        "unchanged Pending",
        |state| matches!(state, AgentActivitySummaryState::Pending { .. }),
    )
    .await;
    let _fresh = expect_activity_summary_matching(
        &mut fixture.client,
        &agent.new_agent.agent_id,
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    expect_strict_mock_turn(
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
            launch_context: protocol::AgentLaunchContext::Standard,
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    let mut events = Vec::new();
    while events.len() < 14 {
        let env = expect_next_event(&mut fixture.client, "multiple agent events").await;
        if env.kind == FrameKind::AgentActivityStats {
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
            launch_context: protocol::AgentLaunchContext::Standard,
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

/// The project lifecycle end to end: an invalid create is refused with a
/// non-fatal typed CommandError that leaves the connection alive and
/// persists nothing, then a valid create/rename/add-root/delete each fan
/// out to every connected client and the deleted project stops replaying.
#[tokio::test]
async fn project_mutations_validate_fan_out_and_delete() {
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

    let (mut fresh_client, fresh_bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        fresh_bootstrap.projects.is_empty(),
        "invalid project_create should not persist any project"
    );
    expect_no_event(
        &mut fresh_client,
        Duration::from_millis(150),
        "invalid project_create should not persist any project",
    )
    .await;

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
            launch_context: protocol::AgentLaunchContext::Standard,
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

    expect_strict_mock_turn(
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
    let root = tempfile::tempdir().expect("create project source directory");
    let source = root.path().join("keep.txt");
    std::fs::write(&source, "Keep project source files").expect("write project source file");

    let project = create_project(
        &mut fixture.client,
        "Delete Guard",
        vec![root.path().to_string_lossy().into_owned()],
    )
    .await;

    let first_turn = MockGateHandle::new();
    let running_turn = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "delete-guard-agent",
            MockScript::one(MockTurn::text_after_gate(
                "mock backend response to: hold project",
                &first_turn,
            ))
            .then(MockTurn::text_completion_after_gate(
                "Turn survives project removal",
                &running_turn,
            ))
            .then(MockTurn::text(
                "mock backend response to: continue after project removal",
            )),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
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
    drop(reservation);
    first_turn.release_one();
    expect_strict_mock_turn(
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

    let startup_gate = fixture
        .host_for_test()
        .install_agent_startup_completion_test_gate("Delete startup");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Delete startup".into()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&project),
                prompt: "start after project removal".into(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn held startup in project");
    startup_gate.wait_until_entered().await;
    let starting = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "held project agent",
    )
    .await;
    let starting: NewAgentPayload = starting.parse_payload().expect("parse held project agent");
    let (mut lazy_observer, _) = fixture::connect_mobile_client_with_bootstrap(
        fixture.host_for_test(),
        "project-delete-observer",
    )
    .await;
    fixture
        .client
        .send_message(
            &new_agent.instance_stream,
            "remain active during project removal".into(),
        )
        .await
        .expect("start turn before project deletion");
    running_turn.wait_until_entered().await;
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "working before project deletion",
        |event| {
            event.stream == new_agent.instance_stream
                && event.kind == FrameKind::ChatEvent
                && matches!(
                    event.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::TypingStatusChanged(true))
                )
        },
    )
    .await;
    eprintln!(
        "PROJECT DELETE EVIDENCE: authoritative working event received; completion gate remains held"
    );

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: project.id.clone(),
        })
        .await
        .expect("project_delete failed");

    let mut detached_agents = Vec::new();
    let env = fixture::next_frame_matching_on(
        &mut fixture.client,
        "project delete with live ownership",
        |event| {
            if event.kind == FrameKind::AgentProjectChanged {
                let start: AgentStartPayload =
                    event.parse_payload().expect("parse canonical detachment");
                assert!(start.project_id.is_none());
                assert_eq!(start.workspace_roots, project_roots(&project));
                detached_agents.push(start.agent_id);
            }
            event.kind == FrameKind::ProjectNotify
        },
    )
    .await;
    for agent_id in [&new_agent.agent_id, &starting.agent_id] {
        assert!(
            detached_agents.contains(agent_id),
            "Deletion publishes live and starting agent ownership before removing the project"
        );
    }
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
    let lazy_change = fixture::next_frame_matching_on(
        &mut lazy_observer,
        "unattached agent project detachment",
        |event| event.kind == FrameKind::AgentProjectChanged,
    )
    .await;
    let lazy_start: AgentStartPayload = lazy_change
        .parse_payload()
        .expect("parse lazy project detachment");
    assert!(lazy_start.project_id.is_none());
    assert_eq!(lazy_start.workspace_roots, project_roots(&project));
    running_turn.release_one();
    let completed = fixture::next_frame_matching_on(
        &mut fixture.client,
        "preserved turn completion",
        |event| {
            event.stream == new_agent.instance_stream
                && event.kind == FrameKind::ChatEvent
                && matches!(
                    event.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::StreamEnd(_))
                )
        },
    )
    .await;
    assert!(
        matches!(completed.parse_payload::<ChatEvent>().expect("parse preserved turn completion"), ChatEvent::StreamEnd(end) if end.message.content.contains("Turn survives project removal")),
        "Deletion must preserve the in-flight turn"
    );
    eprintln!(
        "PROJECT DELETE EVIDENCE: live and starting ownership detached; in-flight turn completed after removal"
    );
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "original client observes preserved turn completion",
        |event| {
            event.stream == new_agent.instance_stream
                && event.kind == FrameKind::ChatEvent
                && matches!(
                    event.parse_payload::<ChatEvent>(),
                    Ok(ChatEvent::TypingStatusChanged(false))
                )
        },
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&source).expect("project deletion must preserve source files"),
        "Keep project source files"
    );
    fixture
        .client
        .send_message(
            &new_agent.instance_stream,
            "continue after project removal".into(),
        )
        .await
        .expect("existing agent must remain usable after project deletion");
    expect_strict_mock_turn(
        &mut fixture.client,
        &new_agent.instance_stream,
        "mock backend response to: continue after project removal",
    )
    .await;

    startup_gate.release_one();
    let started = expect_agent_start_on_stream(
        &mut fixture.client,
        &starting.instance_stream,
        "detached startup AgentStart",
    )
    .await;
    assert!(
        started.project_id.is_none(),
        "Startup must publish the detached ownership without losing the agent"
    );
    assert_eq!(started.workspace_roots, project_roots(&project));
    let (_observer, live_bootstrap) = fixture.connect_with_bootstrap().await;
    let live_agent = live_bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == new_agent.agent_id)
        .expect("project deletion must preserve the live agent");
    assert!(
        live_agent.project_id.is_none(),
        "Deleted project ownership must not replay on live agents"
    );
    assert_eq!(
        live_agent.workspace_roots,
        project_roots(&project),
        "Detachment must preserve the agent workspace"
    );
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
async fn spawn_with_missing_project_id_emits_terminal_agent_error() {
    let mut fixture = Fixture::new().await;
    let missing_project_id = ProjectId("11111111-1111-1111-1111-111111111111".to_owned());

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
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

/// A provider that streams one character at a time must not put one frame per
/// character on the wire.
///
/// Every event the server emits becomes an envelope, a transport frame, a Tauri
/// IPC message and a wasm dispatch. Measured on one 1,492-character answer,
/// Codex sends 1.44 characters per delta and Claude sends 44 — so the same
/// answer is ~1,000 events from one and ~34 from the other, and the client
/// inherits whichever the provider happened to choose. A client that cannot
/// keep up renders a turn the server has already finished, and acts on it: the
/// user presses stop against text that is already history, and the cancel
/// arrives after the turn it was meant for.
///
/// The text must survive exactly. Fewer events carrying different text would be
/// a worse bug than the one this fixes.
#[tokio::test]
async fn per_character_streaming_reaches_the_client_coalesced() {
    const ANSWER: &str = "The quick brown fox jumps over the lazy dog, and then keeps going for a \
                          while longer so that this answer is comfortably past one coalescing \
                          window in length.";

    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "coalescing-agent",
            MockScript::one(MockTurn::text_streamed_per_character(ANSWER)),
        )
        .await;

    let mut streamed = String::new();
    let mut deltas = 0usize;
    loop {
        let env = expect_chat_event_on_stream(&mut fixture.client, &agent.stream, "delta").await;
        match env.parse_payload().expect("parse ChatEvent") {
            ChatEvent::StreamDelta(delta) => {
                streamed.push_str(&delta.text);
                deltas += 1;
            }
            ChatEvent::StreamEnd(_) => break,
            _ => {}
        }
    }

    assert_eq!(
        streamed, ANSWER,
        "coalescing changed the text: the client assembled {streamed:?}"
    );
    assert!(
        deltas < ANSWER.chars().count(),
        "{} characters arrived as {deltas} delta(s), so every character still became its own \
         event and the client pays one frame, one IPC hop and one dispatch per character",
        ANSWER.chars().count()
    );
}

/// Pressing stop on a background command the backend no longer tracks closes
/// its card, so its in-flight tray row goes away.
///
/// A background card is opened by a progress snapshot and closed only by the
/// command's own terminal report. When the provider loses the command — its
/// process exited with the session, its task registry was drained, the CLI was
/// replaced — that report is never coming, and the card stays open forever:
/// the tray shows a row reading "Running" with a stop button that reports
/// nothing back, for a command that is not running. The stop button is the
/// user's way out of that state, so it has to be the thing that ends it.
///
/// The mock backend tracks no background commands at all, which is exactly the
/// state under test: a card is open and the backend can say nothing about it.
#[tokio::test(start_paused = true)]
async fn stopping_an_untracked_background_command_closes_its_card() {
    let scenario = async {
        const CARD: &str = "mock-orphaned-background-command";

        let mut fixture = Fixture::new().await;
        let agent = fixture
            .spawn_scripted(
                "orphaned-background-agent",
                MockScript::one(
                    MockTurn::text("mock backend response to: scripted launch")
                        .with_open_background_task(CARD),
                )
                .with_unbounded_echo(),
            )
            .await;

        // The card the tray offers a stop button on is the one whose progress says
        // it is cancellable and still running. Reaching for it any other way would
        // test something the button never does.
        let progress = fixture
            .next_chat_event_matching(&agent, "background command progress", |event| {
                matches!(
                    event,
                    ChatEvent::ToolProgress(progress)
                        if progress.tool_call_id == CARD
                            && progress.execution_mode == protocol::ToolExecutionMode::Background
                            && progress.cancellable
                )
            })
            .await;
        assert!(matches!(progress, ChatEvent::ToolProgress(_)));

        tokio::time::advance(Duration::from_secs(2)).await;
        fixture
            .client
            .cancel_background_task(
                &agent.stream,
                protocol::CancelBackgroundTaskPayload {
                    tool_call_id: CARD.to_owned(),
                },
            )
            .await
            .expect("cancel_background_task frame");

        let completion = fixture
            .next_chat_event_matching(&agent, "background card closed after stop", |event| {
                matches!(
                    event,
                    ChatEvent::ToolExecutionCompleted(completion)
                        if completion.tool_call_id == CARD
                )
            })
            .await;
        let ChatEvent::ToolExecutionCompleted(completion) = completion else {
            unreachable!("matched a ToolExecutionCompleted");
        };
        assert!(
            matches!(completion.outcome, ToolExecutionOutcome::Cancelled { .. }),
            "a command that was already gone must close as cancelled, not as a result nobody \
         observed: {:?}",
            completion.outcome
        );

        let (_, cancelled) = fixture.connect_with_bootstrap().await;
        let timing = &cancelled
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == agent.new_agent.agent_id)
            .expect("cancelled tool timing")
            .timing;
        let span = timing
            .spans
            .iter()
            .find(|entry| entry.span.tool_call_id == CARD)
            .expect("cancelled command span");
        assert_eq!(span.span.state, protocol::TaskTimingState::Cancelled);
        assert_eq!(span.span.observed_duration_ms, 2_000);
        tokio::time::advance(Duration::from_secs(7)).await;
        // Stopping the same card again must not manufacture a second outcome: the
        // card is closed, so there is nothing left to close.
        fixture
            .client
            .cancel_background_task(
                &agent.stream,
                protocol::CancelBackgroundTaskPayload {
                    tool_call_id: CARD.to_owned(),
                },
            )
            .await
            .expect("second cancel_background_task frame");
        fixture
            .client
            .send_message(&agent.stream, "after the stop".to_owned())
            .await
            .expect("send follow-up after stopping the background card");
        let next = fixture
            .next_chat_event_matching(&agent, "next event after a repeated stop", |event| {
                matches!(
                    event,
                    ChatEvent::ToolExecutionCompleted(completion) if completion.tool_call_id == CARD
                ) || matches!(event, ChatEvent::StreamEnd(_))
            })
            .await;
        assert!(
            matches!(next, ChatEvent::StreamEnd(_)),
            "a repeated stop closed the card twice: {next:?}"
        );
        let (_, settled) = fixture.connect_with_bootstrap().await;
        let timing = &settled
            .task_token_usages
            .iter()
            .find(|usage| usage.root_agent_id == agent.new_agent.agent_id)
            .expect("settled cancellation timing")
            .timing;
        assert_eq!(
            timing
                .spans
                .iter()
                .find(|entry| entry.span.tool_call_id == CARD)
                .expect("retained cancelled span")
                .span
                .observed_duration_ms,
            2_000,
            "server cancellation must stop accumulating idle time"
        );
        tokio::time::resume();
    };
    tokio::pin!(scenario);
    loop {
        tokio::select! { biased; () = &mut scenario => break, () = tokio::task::yield_now() => {} }
    }
}

#[tokio::test]
async fn viewed_images_reach_remote_clients_and_survive_file_deletion() {
    use base64::Engine;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("remote-preview.png");
    let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII=";
    std::fs::write(
        &path,
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap(),
    )
    .unwrap();
    let missing = directory.path().join("missing.png");
    let invalid = directory.path().join("not-an-image.png");
    std::fs::write(&invalid, "not an image").unwrap();
    let oversized = directory.path().join("oversized.png");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(9 * 1024 * 1024)
        .unwrap();

    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "image-preview",
            MockScript::one(
                MockTurn::text("Here is the image")
                    .with_view_image("image", path.to_string_lossy().into_owned())
                    .with_view_image("missing", missing.to_string_lossy().into_owned())
                    .with_view_image("invalid", invalid.to_string_lossy().into_owned())
                    .with_view_image("oversized", oversized.to_string_lossy().into_owned()),
            ),
        )
        .await;
    let completion =
        expect_tool_completion_on_stream(&mut fixture.client, &agent.stream, "image").await;
    let ToolExecutionOutcome::Succeeded {
        result:
            ToolExecutionResult::ViewImage {
                image,
                preview_error,
            },
    } = completion.outcome
    else {
        panic!("image tool must remain successful");
    };
    assert_eq!(preview_error, None);
    let image = image.expect("remote clients must receive image bytes, not just a server path");
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.data, data);
    for id in ["missing", "invalid", "oversized"] {
        let completion =
            expect_tool_completion_on_stream(&mut fixture.client, &agent.stream, id).await;
        assert!(
            matches!(completion.outcome, ToolExecutionOutcome::Succeeded {
            result: ToolExecutionResult::ViewImage { image: None, preview_error: Some(ref error) }
        } if !error.is_empty()),
            "{id} should explain preview unavailability without failing the tool"
        );
    }
    std::fs::remove_file(path).unwrap();
    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrapped_agent(&bootstrap, &agent.new_agent.agent_id);
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &replayed.instance_stream,
        "image replay",
    )
    .await;
    assert!(replay.events.iter().any(|event| matches!(event,
        AgentBootstrapEvent::ChatEvent(ChatEvent::ToolExecutionCompleted(completion))
            if completion.tool_call_id == "image" && matches!(&completion.outcome,
                ToolExecutionOutcome::Succeeded { result: ToolExecutionResult::ViewImage { image: Some(replayed), .. } }
                    if replayed == &image
            )
    )), "reconnecting must not depend on the temporary server file still existing");
}

async fn config_agent_tool(url: &str, name: &str, arguments: Value) -> Value {
    post_json(
        url,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .await
}

#[tokio::test]
async fn help_controls_top_level_agents_across_projects() {
    let mut fixture = Fixture::new().await;
    let url = fixture.config_mcp_http_url().await;
    // Fresh hosts enable no backends; configure one before asserting discovery.
    let configured = config_agent_tool(
        &url,
        "tyde_config_set_setting",
        json!({
            "setting": {"setting": "enabled_backends", "enabled_backends": ["claude"]}
        }),
    )
    .await;
    assert!(!mcp_result_is_error(&configured));
    let options = config_agent_tool(&url, "tyde_config_list_launch_options", json!({})).await;
    assert!(
        options.get("error").is_none(),
        "Help must expose launch discovery"
    );
    let options = mcp_success_json(&options);
    assert!(
        options["enabled_backends"]
            .as_array()
            .unwrap()
            .contains(&json!("claude"))
    );
    assert!(
        options["custom_agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["id"] == "tyde-project-manager"),
        "Help must discover agent types: {options}"
    );

    assert!(
        options["custom_agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["id"] == "tyde-help"),
        "Config launch discovery must offer the canonical Help preset"
    );
    let existing = spawn_agent_control_parent(&mut fixture, "existing top level").await;
    let ordinary = fixture.agent_control_caller(&existing.agent_id).await;
    let project_root = tempfile::tempdir().unwrap();
    let extra_root = tempfile::tempdir().unwrap();
    let project_root_path = project_root.path().to_string_lossy().into_owned();
    let extra_root_path = extra_root.path().to_string_lossy().into_owned();
    let created = config_agent_tool(
        &url,
        "tyde_config_create_project",
        json!({"name": "Draft project", "roots": [project_root_path]}),
    )
    .await;
    let created = mcp_success_json(&created);
    let project = match expect_project_notify(&mut fixture.client, "Help project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected Help-created project upsert, got {other:?}"),
    };
    assert_eq!(created["id"], project.id.0);
    assert_eq!(project.name, "Draft project");
    assert_eq!(project_roots(&project), vec![project_root_path.clone()]);

    let added = config_agent_tool(
        &url,
        "tyde_config_add_project_root",
        json!({"project_id": project.id.0, "root": extra_root_path}),
    )
    .await;
    let added = mcp_success_json(&added);
    assert_eq!(
        added["source"]["roots"],
        json!([project_root_path, extra_root_path])
    );
    match expect_project_notify(&mut fixture.client, "Help add root").await {
        ProjectNotifyPayload::Upsert { project: updated } => assert_eq!(
            project_roots(&updated),
            vec![project_root_path.clone(), extra_root_path.clone()]
        ),
        other => panic!("expected Help add-root upsert, got {other:?}"),
    }
    let removed = config_agent_tool(
        &url,
        "tyde_config_remove_project_root",
        json!({"project_id": project.id.0, "root": extra_root_path}),
    )
    .await;
    let removed = mcp_success_json(&removed);
    assert_eq!(removed["source"]["roots"], json!([project_root_path]));
    match expect_project_notify(&mut fixture.client, "Help remove root").await {
        ProjectNotifyPayload::Upsert { project: updated } => {
            assert_eq!(project_roots(&updated), vec![project_root_path.clone()])
        }
        other => panic!("expected Help remove-root upsert, got {other:?}"),
    }
    let renamed = config_agent_tool(
        &url,
        "tyde_config_rename_project",
        json!({"project_id": project.id.0, "name": "Other project"}),
    )
    .await;
    assert_eq!(mcp_success_json(&renamed)["name"], "Other project");
    let project = match expect_project_notify(&mut fixture.client, "Help rename").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected Help rename upsert, got {other:?}"),
    };
    assert_eq!(project.name, "Other project");
    let rejected = config_agent_tool(
        &url,
        "tyde_config_create_project",
        json!({"name": "Rootless", "roots": []}),
    )
    .await;
    assert!(
        mcp_result_is_error(&rejected),
        "Help project creation must share the UI's root validation"
    );

    let doomed_root = tempfile::tempdir().unwrap();
    let doomed = config_agent_tool(
        &url,
        "tyde_config_create_project",
        json!({"name": "Doomed", "roots": [doomed_root.path().to_string_lossy()]}),
    )
    .await;
    let doomed_id = mcp_success_json(&doomed)["id"].as_str().unwrap().to_owned();
    expect_project_notify(&mut fixture.client, "Help doomed create").await;
    let deleted = config_agent_tool(
        &url,
        "tyde_config_delete_project",
        json!({"project_id": doomed_id}),
    )
    .await;
    assert_eq!(mcp_success_json(&deleted)["deleted"], true);
    match expect_project_notify(&mut fixture.client, "Help delete").await {
        ProjectNotifyPayload::Delete { project: deleted } => assert_eq!(deleted.id.0, doomed_id),
        other => panic!("expected Help project delete, got {other:?}"),
    }
    let projects = config_agent_tool(&url, "tyde_config_list_projects", json!({})).await;
    let projects = mcp_success_json(&projects);
    assert!(
        projects
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == project.id.0)
    );
    assert!(
        projects
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != doomed_id.as_str()),
        "Help-deleted project must leave the project list"
    );

    let redirect_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Help-created top level",
            MockScript::one(MockTurn::exit_plan_request(
                "help-pending-plan",
                "# Pending plan",
            ))
            .then(MockTurn::held_text("default redirect reply"))
            .then(MockTurn::gated_text("redirect reply", &redirect_gate))
            .then(MockTurn::text("queued reply"))
            .then(MockTurn::text("idle reply"))
            .with_user_bubbles(),
        )
        .await;
    let spawned = config_agent_tool(
        &url,
        "tyde_config_spawn_agent",
        json!({
            "project_id": project.id.0, "prompt": "Begin", "name": "Help-created top level",
            "backend_kind": "claude", "custom_agent_id": "tyde-project-manager"
        }),
    )
    .await;
    let spawned = mcp_success_json(&spawned);
    let agent_id = protocol::AgentId(spawned["agent_id"].as_str().unwrap().to_owned());
    drop(reservation);
    let new =
        expect_replayed_new_agent(&mut fixture.client, &agent_id, "Help-created NewAgent").await;
    assert!(
        new.parent_agent_id.is_none(),
        "Help must create independent roots"
    );
    let project_manager = Some(protocol::CustomAgentId("tyde-project-manager".to_owned()));
    assert_eq!(new.custom_agent_id, project_manager);
    let start = expect_agent_start_on_stream(
        &mut fixture.client,
        &new.instance_stream,
        "Help-created start",
    )
    .await;
    assert_eq!(start.project_id, Some(project.id.clone()));
    assert_eq!(start.custom_agent_id, project_manager);
    assert!(start.parent_agent_id.is_none());
    assert_eq!(start.workspace_roots, project_roots(&project));
    fixture::expect_paused_tool_request_on(
        &mut fixture.client,
        &new.instance_stream,
        "ExitPlanMode",
    )
    .await;

    let listed = config_agent_tool(&url, "tyde_config_list_agents", json!({})).await;
    let listed = mcp_success_json(&listed);
    let agents = listed.as_array().unwrap();
    assert!(agents.iter().any(|a| a["agent_id"] == existing.agent_id.0));
    assert!(agents.iter().any(|a| a["agent_id"] == agent_id.0
        && a["status"] == "awaiting_user"
        && a["project_id"] == project.id.0
        && a["parent_agent_id"].is_null()));
    assert!(
        mcp_list_agents(&ordinary)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    let denied = mcp_tool_call_as(
        &ordinary,
        false,
        "tyde_send_agent_message",
        json!({"agent_id": agent_id.0, "message": "Not authorized"}),
    )
    .await;
    assert!(
        mcp_result_is_error(&denied),
        "Ordinary agent-control must remain child-scoped"
    );
    let injected_parent = config_agent_tool(&url, "tyde_config_spawn_agent", json!({
        "project_id": project.id.0, "prompt": "Invalid", "backend_kind": "claude", "parent_agent_id": existing.agent_id.0
    })).await;
    assert!(
        injected_parent.get("error").is_some() || mcp_result_is_error(&injected_parent),
        "Global creation must reject parent injection"
    );

    let delivered = config_agent_tool(
        &url,
        "tyde_config_send_agent_message",
        json!({"agent_id": agent_id.0, "message": "Continue without plan approval"}),
    )
    .await;
    assert!(!mcp_result_is_error(&delivered));
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &new.instance_stream,
        "Help default cancellation",
        |event| matches!(event, ChatEvent::OperationCancelled(_)),
    )
    .await;
    fixture::next_chat_event_matching_on(&mut fixture.client, &new.instance_stream, "Help default redirect", |event| {
        matches!(event, ChatEvent::MessageAdded(message) if matches!(&message.sender, MessageSender::Agent { agent } if agent == "Operator")
            && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: None, name }) if name == "Operator")
            && message.content == "Continue without plan approval")
    }).await;
    let listed = config_agent_tool(&url, "tyde_config_list_agents", json!({})).await;
    assert!(
        mcp_success_json(&listed)
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == agent_id.0 && a["status"] == "thinking")
    );

    fixture
        .client
        .send_message(&new.instance_stream, "Queue this".to_owned())
        .await
        .expect("queue ordinary chat message");
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "Help ordinary chat message queued",
        |env| {
            env.stream == new.instance_stream
                && env.kind == FrameKind::QueuedMessages
                && env
                    .parse_payload::<protocol::QueuedMessagesPayload>()
                    .is_ok_and(|p| p.messages.len() == 1 && p.messages[0].message == "Queue this")
        },
    )
    .await;
    let redirected = config_agent_tool(
        &url,
        "tyde_config_send_agent_message",
        json!({"agent_id": agent_id.0, "message": "Redirect now", "interrupt": true}),
    )
    .await;
    assert!(!mcp_result_is_error(&redirected));
    redirect_gate.wait_until_entered().await;
    fixture::next_chat_event_matching_on(&mut fixture.client, &new.instance_stream, "Help redirect", |event| {
        matches!(event, ChatEvent::MessageAdded(message) if matches!(&message.sender, MessageSender::Agent { agent } if agent == "Operator")
            && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: None, name }) if name == "Operator")
            && message.content == "Redirect now")
    }).await;
    redirect_gate.release_one();
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &new.instance_stream,
        "queued reply preserved",
        |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "queued reply"),
    )
    .await;
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &new.instance_stream,
        "queued turn idle",
        |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
    )
    .await;
    let idle = config_agent_tool(
        &url,
        "tyde_config_send_agent_message",
        json!({"agent_id": agent_id.0, "message": "Continue while idle"}),
    )
    .await;
    assert!(!mcp_result_is_error(&idle));
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &new.instance_stream,
        "idle agent restarted",
        |event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content == "idle reply"),
    )
    .await;
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &new.instance_stream,
        "idle follow-up complete",
        |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
    )
    .await;

    let parent_caller = fixture.agent_control_caller(&agent_id).await;
    let child = mcp_spawn_agent_as(
        &parent_caller,
        json!({"prompt": "Child task", "name": "Descendant", "backend_kind": "claude"}),
    )
    .await;
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap
            .agents
            .iter()
            .any(|a| a.agent_id == agent_id && a.parent_agent_id.is_none())
    );
    assert!(
        bootstrap
            .agents
            .iter()
            .any(|a| a.agent_id == child && a.parent_agent_id.as_ref() == Some(&agent_id))
    );
    let closed = config_agent_tool(
        &url,
        "tyde_config_close_agent",
        json!({"agent_id": agent_id.0}),
    )
    .await;
    assert!(!mcp_result_is_error(&closed));
    let mut closed_ids = std::collections::HashSet::new();
    while closed_ids.len() < 2 {
        let event = expect_kind(
            &mut observer,
            FrameKind::AgentClosed,
            "global close fan-out",
        )
        .await;
        closed_ids.insert(
            event
                .parse_payload::<AgentClosedPayload>()
                .unwrap()
                .agent_id,
        );
    }
    assert!(closed_ids.contains(&agent_id) && closed_ids.contains(&child));
    eprintln!(
        "HELP CONFIG subtree close observed count={}",
        closed_ids.len()
    );
    let listed = config_agent_tool(&url, "tyde_config_list_agents", json!({})).await;
    let listed = mcp_success_json(&listed);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == existing.agent_id.0)
    );
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == agent_id.0 || a["agent_id"] == child.0)
    );
    for (name, args) in [
        (
            "tyde_config_send_agent_message",
            json!({"agent_id": agent_id.0, "message": "After close"}),
        ),
        ("tyde_config_close_agent", json!({"agent_id": agent_id.0})),
        ("tyde_config_close_agent", json!({"agent_id": "invalid"})),
        (
            "tyde_config_send_agent_message",
            json!({"agent_id": existing.agent_id.0, "message": "  "}),
        ),
    ] {
        let failed = config_agent_tool(&url, name, args).await;
        assert!(
            mcp_result_is_error(&failed),
            "Invalid target/input must fail visibly"
        );
    }
    // Completed bootstraps omit historical typing transitions; attach before output.
    let authenticated_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Authenticated config root",
            MockScript::one(MockTurn::text_after_gate(
                "authenticated launch reply",
                &authenticated_gate,
            ))
            .then(MockTurn::text("authenticated follow-up reply"))
            .with_user_bubbles(),
        )
        .await;
    let mut config = ordinary.clone();
    config.url = url.clone();
    let created = mcp_success_json(
        &mcp_tool_call_as(
            &config,
            false,
            "tyde_config_spawn_agent",
            json!({"project_id":project.id.0, "prompt":"Authenticated configuration launch",
            "name":"Authenticated config root", "backend_kind":"claude"}),
        )
        .await,
    );
    drop(reservation);
    let target_id = protocol::AgentId(created["agent_id"].as_str().unwrap().to_owned());
    let target = expect_replayed_new_agent(
        &mut fixture.client,
        &target_id,
        "authenticated config publication",
    )
    .await;
    eprintln!("HELP CONFIG authenticated publication observed");
    expect_agent_start_on_stream(
        &mut fixture.client,
        &target.instance_stream,
        "authenticated config attached before launch output",
    )
    .await;
    authenticated_gate.release_one();
    for (task, follow_up) in [
        ("Authenticated configuration launch", false),
        ("Authenticated configuration follow-up", true),
    ] {
        eprintln!("HELP CONFIG authenticated identity wait follow_up={follow_up}");
        if follow_up {
            let sent = mcp_tool_call_as(
                &config,
                false,
                "tyde_config_send_agent_message",
                json!({"agent_id":target_id.0,"message":task}),
            )
            .await;
            assert!(!mcp_result_is_error(&sent));
        }
        let input = fixture::next_chat_event_matching_on(
            &mut fixture.client,
            &target.instance_stream,
            "authenticated config identity",
            |event| matches!(event, ChatEvent::MessageAdded(message) if message.content == task),
        )
        .await;
        assert!(
            matches!(input, ChatEvent::MessageAdded(message)
            if matches!(&message.sender, MessageSender::Agent {agent} if agent == &existing.name)
                && matches!(&message.origin, Some(protocol::MessageOrigin::Agent {agent_id:Some(id),name}) if id == &existing.agent_id && name == &existing.name)),
            "Authenticated config calls must not fall through to anonymous Operator"
        );
        eprintln!("HELP CONFIG authenticated idle wait follow_up={follow_up}");
        fixture::next_chat_event_matching_on(
            &mut fixture.client,
            &target.instance_stream,
            "authenticated config idle",
            |event| matches!(event, ChatEvent::TypingStatusChanged(false)),
        )
        .await;
        eprintln!("HELP CONFIG authenticated idle observed follow_up={follow_up}");
    }
    fixture.mock_by_id(&target_id).await.assert_clean().await;
    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let late_target = bootstrapped_agent(&bootstrap, &target_id);
    let replay = expect_raw_agent_bootstrap_on_stream(
        &mut late_client,
        &late_target.instance_stream,
        "authenticated config completed launch replay",
    )
    .await;
    let idle_transitions = replay
        .events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(false))
            )
        })
        .count();
    eprintln!(
        "HELP CONFIG completed bootstrap status={:?} events={} idle_transitions={idle_transitions}",
        replay.status,
        replay.events.len()
    );
    assert_eq!(replay.status, AgentControlStatus::Idle);
    for task in [
        "Authenticated configuration launch",
        "Authenticated configuration follow-up",
    ] {
        assert!(
            replay.events.iter().any(|event| matches!(event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message))
                if message.content == task
                    && matches!(&message.sender, MessageSender::Agent { agent } if agent == &existing.name)
                    && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), name }) if id == &existing.agent_id && name == &existing.name)
            )),
            "Completed bootstrap must preserve authenticated config identity"
        );
    }
}

#[tokio::test]
async fn human_home_help_hands_off_to_independent_project_owner() {
    use protocol::{AgentLaunchContext, AgentTydePermissions, CustomAgentId};
    let mut fixture = Fixture::new().await;
    assert_eq!(
        fixture
            .bootstrap
            .launch_profile_catalog
            .home_custom_agent_id,
        Some(CustomAgentId(protocol::OPERATOR_CUSTOM_AGENT_ID.to_owned()))
    );
    let url = fixture.config_mcp_http_url().await;
    let configured = config_agent_tool(
        &url,
        "tyde_config_set_setting",
        json!({
            "setting": {"setting": "enabled_backends", "enabled_backends": ["claude"]}
        }),
    )
    .await;
    assert!(!mcp_result_is_error(&configured));
    let deleted = config_agent_tool(
        &url,
        "tyde_config_delete_custom_agent",
        json!({"custom_agent_id": "tyde-help"}),
    )
    .await;
    assert!(
        mcp_result_is_error(&deleted),
        "The Home preset must remain available"
    );
    assert!(mcp_result_text(&deleted).contains("required Home launch preset"));
    let (help, help_start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: AgentLaunchContext::HumanChat,
            name: Some("Home setup guide".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: Vec::new(),
                prompt: "Set up my project".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    assert_eq!(
        help.new_agent.custom_agent_id,
        Some(CustomAgentId("tyde-help".to_owned()))
    );
    assert_eq!(help_start.custom_agent_id, help.new_agent.custom_agent_id);
    assert_eq!(help_start.project_id, None);
    assert_eq!(help_start.tyde_permissions, AgentTydePermissions::Root);
    assert_eq!(help.new_agent.tyde_permissions, AgentTydePermissions::Root);
    fixture.finish_turn(&help).await;
    eprintln!("HELP HANDOFF: fresh Home Help finished its initial turn");
    let mut config = fixture.agent_control_caller(&help.new_agent.agent_id).await;
    config.url = url;
    let options = mcp_success_json(
        &mcp_tool_call_as(&config, false, "tyde_config_list_launch_options", json!({})).await,
    );
    assert_eq!(options["enabled_backends"], json!(["claude"]));
    assert!(
        options["custom_agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "tyde-help")
    );
    let folder = tempfile::tempdir().unwrap();
    let created = mcp_success_json(
        &mcp_tool_call_as(
            &config,
            false,
            "tyde_config_create_project",
            json!({
                "name": "Approved project", "roots": [folder.path().to_string_lossy()]
            }),
        )
        .await,
    );
    let project_id = ProjectId(created["id"].as_str().unwrap().to_owned());
    let project = match expect_project_notify(&mut fixture.client, "approved project").await {
        ProjectNotifyPayload::Upsert { project } => project,
        _ => panic!("expected approved project"),
    };
    assert_eq!(project.id, project_id);
    let handoff_prompt = "Original task: build the app. Decision: reuse this repository. User approved this project and a Project Manager. Setup: folder ready. Next: inspect AGENTS.md.";
    let reservation = fixture
        .reserve_next_mock_launch(
            "Independent owner",
            MockScript::one(MockTurn::text("Owner received handoff")).with_user_bubbles(),
        )
        .await;
    let spawned = mcp_success_json(
        &mcp_tool_call_as(
            &config,
            false,
            "tyde_config_spawn_agent",
            json!({
                "project_id": project_id.0, "prompt": handoff_prompt, "name": "Independent owner",
                "backend_kind": "claude", "custom_agent_id": "tyde-project-manager"
            }),
        )
        .await,
    );
    eprintln!("HELP HANDOFF: independent config spawn returned");
    drop(reservation);
    let owner_id = protocol::AgentId(spawned["agent_id"].as_str().unwrap().to_owned());
    let owner =
        expect_replayed_new_agent(&mut fixture.client, &owner_id, "independent owner").await;
    let start =
        expect_agent_start_on_stream(&mut fixture.client, &owner.instance_stream, "owner start")
            .await;
    assert_eq!(
        start.custom_agent_id,
        Some(CustomAgentId("tyde-project-manager".to_owned()))
    );
    assert_eq!(start.project_id, Some(project_id.clone()));
    assert_eq!(start.workspace_roots, project_roots(&project));
    assert_eq!(start.tyde_permissions, AgentTydePermissions::Agent);
    assert!(start.parent_agent_id.is_none());
    assert!(owner.parent_agent_id.is_none());
    eprintln!("HELP HANDOFF: owner startup has the assigned project and Agent role");
    loop {
        let env = expect_chat_event_on_stream(
            &mut fixture.client,
            &owner.instance_stream,
            "owner original context",
        )
        .await;
        if let ChatEvent::MessageAdded(message) = env.parse_payload::<ChatEvent>().unwrap() {
            assert!(
                message.content == handoff_prompt,
                "Owner must receive the original task and decisions intact"
            );
            assert!(
                matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), .. }) if id == &help.new_agent.agent_id),
                "Owner input must carry Help's authenticated origin"
            );
            break;
        }
    }
    eprintln!("HELP HANDOFF: owner received the complete original context");
    loop {
        let env =
            expect_chat_event_on_stream(&mut fixture.client, &help.stream, "typed project handoff")
                .await;
        if matches!(env.parse_payload::<ChatEvent>().unwrap(), ChatEvent::AgentHandoff(h)
            if h.agent_id == owner_id && h.project_id == Some(project_id.clone()))
        {
            break;
        }
    }
    eprintln!("HELP HANDOFF: source received its live typed receipt");
    let (mut observer, snapshot) = fixture.connect_with_bootstrap().await;
    let help_replay_stream =
        bootstrapped_agent(&snapshot, &help.new_agent.agent_id).instance_stream;
    let owner_replay_stream = bootstrapped_agent(&snapshot, &owner_id).instance_stream;
    let mut help_replayed = false;
    let mut owner_replayed = false;
    while !help_replayed || !owner_replayed {
        let env = observer.next_event().await.unwrap().unwrap();
        if env.kind != FrameKind::AgentBootstrap {
            continue;
        }
        let replay: AgentBootstrapPayload = env.parse_payload().unwrap();
        if env.stream == help_replay_stream {
            assert!(replay.events.iter().any(|event| matches!(event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::AgentHandoff(h)) if h.agent_id == owner_id)),
                "Help's bootstrap must replay the typed handoff");
            help_replayed = true;
            eprintln!("HELP HANDOFF: source replay includes the receipt");
        }
        if env.stream == owner_replay_stream {
            assert!(replay.events.iter().any(|event| matches!(event,
                AgentBootstrapEvent::ChatEvent(ChatEvent::MessageAdded(message)) if message.content == handoff_prompt
                    && matches!(&message.origin, Some(protocol::MessageOrigin::Agent { agent_id: Some(id), .. }) if id == &help.new_agent.agent_id))),
                "Owner's bootstrap must retain the task and decisions");
            owner_replayed = true;
            eprintln!("HELP HANDOFF: owner replay includes the original context");
        }
    }
    let listed = mcp_success_json(
        &mcp_tool_call_as(&config, false, "tyde_config_list_agents", json!({})).await,
    );
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == help.new_agent.agent_id.0 && a["project_id"].is_null())
    );
    // Explicit selections and non-human contexts never inherit the Home default or authority.
    for (context, role, project, expected_role) in [
        (
            AgentLaunchContext::HumanChat,
            Some("tyde-default"),
            None,
            Some("tyde-default"),
        ),
        (
            AgentLaunchContext::HumanChat,
            Some("tyde-project-manager"),
            None,
            Some("tyde-project-manager"),
        ),
        (
            AgentLaunchContext::HumanChat,
            None,
            Some(project_id.clone()),
            None,
        ),
        (
            AgentLaunchContext::HumanChat,
            Some("tyde-help"),
            Some(project_id.clone()),
            Some("tyde-help"),
        ),
        (AgentLaunchContext::Standard, None, None, None),
        (
            AgentLaunchContext::Standard,
            Some("tyde-help"),
            None,
            Some("tyde-help"),
        ),
    ] {
        let (agent, start) = fixture
            .spawn_with(SpawnAgentPayload {
                launch_context: context,
                name: Some("Explicit context".to_owned()),
                custom_agent_id: role.map(|id| CustomAgentId(id.to_owned())),
                parent_agent_id: None,
                project_id: project,
                params: SpawnAgentParams::New {
                    workspace_roots: Vec::new(),
                    prompt: "Bounded question".to_owned(),
                    images: None,
                    backend_kind: BackendKind::Claude,
                    launch_profile_id: None,
                    cost_hint: None,
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await;
        assert_eq!(
            start.custom_agent_id,
            expected_role.map(|id| CustomAgentId(id.to_owned()))
        );
        assert_eq!(start.tyde_permissions, AgentTydePermissions::Agent);
        fixture.finish_turn(&agent).await;
    }
    fixture
        .client
        .send_message(&help.stream, "Another Home question".to_owned())
        .await
        .unwrap();
    fixture.finish_turn(&help).await;
    let session_id = help_start
        .session_id
        .as_ref()
        .expect("Help session must exist")
        .clone();
    let journal_path = fixture
        .store_dir()
        .join("transcripts")
        .join(format!("{}.jsonl", session_id.0));
    let journal_contents = std::fs::read(&journal_path).expect("Help transcript exists");
    std::fs::remove_file(&journal_path).unwrap();
    std::fs::create_dir(&journal_path).unwrap();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Owner without saved receipt",
            MockScript::one(MockTurn::text("Read-only owner")),
        )
        .await;
    let partial = mcp_tool_call_as(&config, false, "tyde_config_spawn_agent", json!({
        "project_id": project_id.0, "prompt": "Bounded task", "name": "Owner without saved receipt",
        "backend_kind": "claude", "custom_agent_id": "tyde-project-manager"
    })).await;
    drop(reservation);
    assert!(
        mcp_result_is_error(&partial),
        "A failed journal append must not acknowledge a saved handoff"
    );
    assert!(
        partial["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["text"].as_str().is_some_and(
                |text| text.contains("Agent created, but its handoff could not be recorded")
            )),
        "Partial success must explicitly identify the handoff persistence failure"
    );
    std::fs::remove_dir(&journal_path).unwrap();
    std::fs::write(&journal_path, journal_contents).unwrap();
    let unreadable = format!(
        "{{\"logical_session_id\":\"{}\",\"sequence\":9999,\"event_id\":\"unknown-handoff-schema\",\"visibility\":\"visible\",\"event\":{{\"type\":\"future_handoff_event\",\"payload\":{{}}}},\"timestamp_ms\":1}}\n",
        session_id.0
    );
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .unwrap()
            .write_all(unreadable.as_bytes())
            .unwrap();
    }
    for name in ["Unreadable handoff", "Cached unreadable handoff"] {
        let reservation = fixture
            .reserve_next_mock_launch(name, MockScript::one(MockTurn::text("Read-only owner")))
            .await;
        let partial = mcp_tool_call_as(
            &config,
            false,
            "tyde_config_spawn_agent",
            json!({
                "project_id": project_id.0, "prompt": "Bounded task", "name": name,
                "backend_kind": "claude", "custom_agent_id": "tyde-project-manager"
            }),
        )
        .await;
        drop(reservation);
        assert!(
            mcp_result_is_error(&partial),
            "An unreadable transcript must not acknowledge a replayable handoff, including cached appends"
        );
        assert!(mcp_result_text(&partial).contains("cannot replay after restart"));
    }
    assert!(
        std::fs::read_to_string(&journal_path)
            .unwrap()
            .contains(&unreadable),
        "An incompatible complete record must remain intact"
    );
    fixture.client.close_agent(&help.stream).await.unwrap();
    fixture
        .next_frame_matching("Help closed", |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<AgentClosedPayload>()
                    .is_ok_and(|p| p.agent_id == help.new_agent.agent_id)
        })
        .await;
    let independent = config_agent_tool(&config.url, "tyde_config_list_agents", json!({})).await;
    assert!(
        mcp_success_json(&independent)
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == owner_id.0),
        "Closing Help must not close the independent project owner"
    );
    fixture.restart_host().await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: AgentLaunchContext::Standard,
            name: Some("Resumed Help".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .unwrap();
    // Automatic restoration publishes other agents too, so the first NewAgent
    // is not necessarily the requested resume (it can be an Agent-role owner).
    let env = fixture::next_logical_frame_matching_on(&mut fixture.client, "resumed Help", |env| {
        env.kind == FrameKind::NewAgent
            && env
                .parse_payload::<NewAgentPayload>()
                .is_ok_and(|agent| agent.session_id.as_ref() == Some(&session_id))
    })
    .await;
    let resumed: NewAgentPayload = env.parse_payload().unwrap();
    assert_eq!(
        resumed.custom_agent_id,
        Some(CustomAgentId("tyde-help".to_owned()))
    );
    assert_eq!(resumed.project_id, None);
    eprintln!("HELP HANDOFF: exact saved Help session selected after restart");
    // A deliberately unreadable journal can report a nonfatal history warning;
    // errors on independently restored agents must not select the resume.
    let env = fixture::next_logical_frame_matching_on(&mut fixture.client, "resumed Help start", |env| {
        if env.stream != resumed.instance_stream { return false; }
        if env.kind == FrameKind::AgentError {
            let error: AgentErrorPayload = env.parse_payload().unwrap();
            // This injected journal loss now also emits the required typed-accounting
            // coverage warning; rejecting that warning would hide genuine incompleteness.
            let history_warning = error.message.contains("saved history") || error.message == "Saved task accounting is incomplete or unreadable; historical coverage cannot be certified complete.";
            eprintln!("HELP HANDOFF: resume error fatal={} code={:?} history_warning={history_warning}", error.fatal, error.code);
            assert!(!error.fatal && history_warning, "Only the deliberately unreadable history warning is expected");
        }
        assert!(!matches!(env.kind, FrameKind::CommandError | FrameKind::AgentClosed), "Resume must remain usable");
        env.kind == FrameKind::AgentStart
    }).await;
    let resumed_start: AgentStartPayload = env.parse_payload().unwrap();
    assert_eq!(resumed_start.tyde_permissions, AgentTydePermissions::Root);
    let (mut reader, snapshot) = fixture.connect_with_bootstrap().await;
    assert!(
        matches!(&snapshot.task_token_usages.iter().find(|usage| usage.root_agent_id == resumed.agent_id).expect("unreadable Help accounting").total.status, TaskTokenUsageStatus::Partial { reasons, .. } if reasons.contains(&TaskTokenUsageUnavailableReason::AccountingHistoryIncomplete)),
        "a usable resumed Help chat must not certify its unreadable accounting complete"
    );
    let replay_stream = bootstrapped_agent(&snapshot, &resumed.agent_id).instance_stream;
    loop {
        let env = reader.next_event().await.unwrap().unwrap();
        if env.kind != FrameKind::AgentBootstrap || env.stream != replay_stream {
            continue;
        }
        let replay: AgentBootstrapPayload = env.parse_payload().unwrap();
        let receipts: Vec<_> = replay
            .events
            .iter()
            .filter_map(|event| match event {
                AgentBootstrapEvent::ChatEvent(ChatEvent::AgentHandoff(handoff)) => Some(handoff),
                _ => None,
            })
            .collect();
        assert_eq!(
            receipts.len(),
            1,
            "Fresh-host replay only includes the receipt before the unreadable record"
        );
        assert_eq!(receipts[0].agent_id, owner_id);
        break;
    }
    assert!(
        std::fs::read_to_string(&journal_path)
            .unwrap()
            .contains(&unreadable),
        "Fresh-host replay must not replace incompatible bytes"
    );
}
