use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use client::ClientConfig;
use protocol::{
    AgentErrorPayload, AgentId, AgentStartPayload, BackendKind, ChatEvent, FrameKind, HostSettings,
    HostSettingsPayload, NewAgentPayload, ProjectId, SendMessagePayload, SpawnAgentParams,
    SpawnAgentPayload, SpawnCostHint, StreamPath,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use uuid::Uuid;

const DEFAULT_IDLE_TIMEOUT_MS: u64 = 60_000;
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
const BOOTSTRAP_QUIET_PERIOD: Duration = Duration::from_millis(50);
const WALL_TIMEOUT_MULTIPLIER: u64 = 10;
const COMMAND_BUFFER: usize = 32;
const AGENT_CONTROL_HOST_BIND_ENV: &str = "TYDE_AGENT_CONTROL_HOST_BIND_ADDR";
const LEGACY_DEV_HOST_BIND_ENV: &str = "TYDE_DEV_HOST_BIND_ADDR";
const AGENT_CONTROL_HOST_UDS_ENV: &str = "TYDE_AGENT_CONTROL_HOST_UDS_PATH";

#[derive(Debug, Clone)]
struct AgentState {
    agent_id: AgentId,
    name: String,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
    project_id: Option<ProjectId>,
    parent_agent_id: Option<AgentId>,
    created_at_ms: u64,
    instance_stream: StreamPath,
    /// True between StreamStart and StreamEnd.
    is_thinking: bool,
    /// True after at least one turn has completed since the last send/spawn.
    turn_completed: bool,
    /// Set when a fatal AgentError arrives.
    terminated: bool,
    last_message: Option<String>,
    last_error: Option<String>,
    activity_counter: u64,
}

impl AgentState {
    /// An agent is "active" (worth waiting on) if it's currently thinking
    /// or hasn't completed its first turn yet and hasn't terminated.
    fn is_active(&self) -> bool {
        !self.terminated && (self.is_thinking || !self.turn_completed)
    }

    /// Derived status string for MCP API responses.
    fn status_label(&self) -> &'static str {
        if self.terminated {
            "terminated"
        } else if self.is_thinking {
            "thinking"
        } else {
            "idle"
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SnapshotState {
    host_settings: Option<HostSettings>,
    agents: HashMap<AgentId, AgentState>,
    connection_error: Option<String>,
    version: u64,
}

#[derive(Debug)]
struct PendingSpawn {
    expected_name: String,
    expected_backend_kind: BackendKind,
    expected_workspace_roots: Vec<String>,
    expected_project_id: Option<ProjectId>,
    expected_parent_agent_id: Option<AgentId>,
    reply: oneshot::Sender<Result<SpawnAgentResult, String>>,
}

impl PendingSpawn {
    fn matches(&self, payload: &NewAgentPayload) -> bool {
        payload.name == self.expected_name
            && payload.backend_kind == self.expected_backend_kind
            && payload.workspace_roots == self.expected_workspace_roots
            && payload.project_id == self.expected_project_id
            && payload.parent_agent_id == self.expected_parent_agent_id
    }
}

struct RuntimeState {
    snapshot: SnapshotState,
    pending_spawn: Option<PendingSpawn>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            snapshot: SnapshotState::default(),
            pending_spawn: None,
        }
    }
}

enum Command {
    Spawn {
        request: SpawnRequest,
        reply: oneshot::Sender<Result<SpawnAgentResult, String>>,
    },
    SendMessage {
        agent_id: AgentId,
        message: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Interrupt {
        agent_id: AgentId,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Debug, Clone)]
pub enum AgentControlTarget {
    Tcp(SocketAddr),
    Uds(PathBuf),
}

impl AgentControlTarget {
    pub fn from_args_env(args: &[String]) -> Result<Self, String> {
        match args {
            [flag, value] if flag == "--tcp" => {
                let addr = value
                    .parse::<SocketAddr>()
                    .map_err(|err| format!("invalid --tcp address '{value}': {err}"))?;
                Ok(Self::Tcp(addr))
            }
            [flag, value] if flag == "--uds" => Ok(Self::Uds(PathBuf::from(value))),
            [] => Self::from_env(),
            _ => Err(
                "usage: tyde-dev-driver agent-control [--tcp 127.0.0.1:7777 | --uds /path/to/socket]"
                    .to_string(),
            ),
        }
    }

    fn from_env() -> Result<Self, String> {
        let tcp = std::env::var(AGENT_CONTROL_HOST_BIND_ENV)
            .ok()
            .or_else(|| std::env::var(LEGACY_DEV_HOST_BIND_ENV).ok());
        let uds = std::env::var(AGENT_CONTROL_HOST_UDS_ENV).ok();

        match (tcp, uds) {
            (Some(addr), None) => {
                let addr = addr.parse::<SocketAddr>().map_err(|err| {
                    format!(
                        "invalid {AGENT_CONTROL_HOST_BIND_ENV}/{LEGACY_DEV_HOST_BIND_ENV} value '{addr}': {err}"
                    )
                })?;
                Ok(Self::Tcp(addr))
            }
            (None, Some(path)) => Ok(Self::Uds(PathBuf::from(path))),
            (Some(_), Some(_)) => Err(format!(
                "set either {AGENT_CONTROL_HOST_BIND_ENV} or {AGENT_CONTROL_HOST_UDS_ENV}, not both"
            )),
            (None, None) => Err(format!(
                "missing target endpoint; pass --tcp/--uds or set {AGENT_CONTROL_HOST_BIND_ENV}"
            )),
        }
    }
}

#[derive(Clone)]
pub struct AgentControlHandle {
    command_tx: mpsc::Sender<Command>,
    snapshot_rx: watch::Receiver<SnapshotState>,
}

impl AgentControlHandle {
    pub async fn connect(target: AgentControlTarget) -> Result<Self, String> {
        let connection = match target {
            AgentControlTarget::Tcp(addr) => {
                let stream = TcpStream::connect(addr)
                    .await
                    .map_err(|err| format!("failed to connect to host endpoint {addr}: {err}"))?;
                client::connect(&ClientConfig::current(), stream)
                    .await
                    .map_err(|err| format!("Tyde host handshake failed: {err:?}"))?
            }
            AgentControlTarget::Uds(path) => client::connect_uds(&path, &ClientConfig::current())
                .await
                .map_err(|err| {
                    format!("Tyde host handshake failed for {}: {err:?}", path.display())
                })?,
        };

        Self::from_connection(connection).await
    }

    pub async fn from_connection(connection: client::Connection) -> Result<Self, String> {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_BUFFER);
        let (snapshot_tx, snapshot_rx) = watch::channel(SnapshotState::default());
        let (ready_tx, ready_rx) = oneshot::channel();

        tokio::spawn(async move {
            run_runtime(connection, command_rx, snapshot_tx, ready_tx).await;
        });

        ready_rx
            .await
            .map_err(|_| "agent-control runtime exited before bootstrap completed".to_string())??;

        Ok(Self {
            command_tx,
            snapshot_rx,
        })
    }

    pub async fn spawn_agent(&self, request: SpawnRequest) -> Result<SpawnAgentResult, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Spawn {
                request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "agent-control runtime is not available".to_string())?;

        timeout(SPAWN_TIMEOUT, reply_rx)
            .await
            .map_err(|_| "timed out waiting for Tyde to announce the new agent".to_string())?
            .map_err(|_| "agent-control runtime dropped spawn response".to_string())?
    }

    pub async fn send_message(&self, agent_id: AgentId, message: String) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(Command::SendMessage {
                agent_id,
                message,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "agent-control runtime is not available".to_string())?;

        reply_rx
            .await
            .map_err(|_| "agent-control runtime dropped send_message response".to_string())?
    }

    pub async fn interrupt(&self, agent_id: AgentId) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Interrupt {
                agent_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "agent-control runtime is not available".to_string())?;

        reply_rx
            .await
            .map_err(|_| "agent-control runtime dropped interrupt response".to_string())?
    }

    fn snapshot(&self) -> SnapshotState {
        self.snapshot_rx.borrow().clone()
    }

    pub async fn list_agents(&self) -> Vec<AgentOverview> {
        let mut agents = self
            .snapshot()
            .agents
            .values()
            .map(agent_overview_from_state)
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.created_at_ms.cmp(&right.created_at_ms));
        agents
    }

    pub async fn await_agents(
        &self,
        requested_ids: Option<Vec<AgentId>>,
        timeout_ms: Option<u64>,
    ) -> Result<AwaitAgentsResult, String> {
        let idle_timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS));
        let wall_timeout = Duration::from_millis(
            timeout_ms
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_MS)
                .saturating_mul(WALL_TIMEOUT_MULTIPLIER),
        );
        let wall_started = Instant::now();
        let mut snapshot_rx = self.snapshot_rx.clone();

        let mut watched_activity =
            watched_activity_map(&snapshot_rx.borrow(), requested_ids.as_deref())?;
        let mut idle_deadline = Instant::now() + idle_timeout;

        loop {
            let snapshot = snapshot_rx.borrow().clone();
            let watched_ids = resolve_watched_ids(&snapshot, requested_ids.as_deref())?;
            let ready = ready_agents_from_snapshot(&snapshot, &watched_ids);
            let still_running = still_running_agent_ids(&snapshot, &watched_ids);

            if !ready.is_empty() || still_running.is_empty() {
                return Ok(AwaitAgentsResult {
                    ready,
                    still_running,
                });
            }

            let wall_deadline = wall_started + wall_timeout;
            let next_deadline = std::cmp::min(idle_deadline, wall_deadline);
            let sleep_for = next_deadline.saturating_duration_since(Instant::now());

            match timeout(sleep_for, snapshot_rx.changed()).await {
                Ok(Ok(())) => {
                    let snapshot = snapshot_rx.borrow().clone();
                    let updated_activity =
                        watched_activity_map(&snapshot, requested_ids.as_deref())?;
                    if updated_activity != watched_activity {
                        watched_activity = updated_activity;
                        idle_deadline = Instant::now() + idle_timeout;
                    }
                }
                Ok(Err(_)) => {
                    let error = snapshot_rx
                        .borrow()
                        .connection_error
                        .clone()
                        .unwrap_or_else(|| "agent-control runtime stopped".to_string());
                    return Err(error);
                }
                Err(_) => {
                    let snapshot = snapshot_rx.borrow().clone();
                    let watched_ids = resolve_watched_ids(&snapshot, requested_ids.as_deref())?;
                    return Ok(AwaitAgentsResult {
                        ready: ready_agents_from_snapshot(&snapshot, &watched_ids),
                        still_running: still_running_agent_ids(&snapshot, &watched_ids),
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub workspace_roots: Vec<String>,
    pub prompt: String,
    pub backend_kind: BackendKind,
    pub parent_agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub name: String,
    pub cost_hint: Option<SpawnCostHint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpawnAgentResult {
    pub agent_id: String,
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentResult {
    pub agent_id: String,
    pub status: String,
    pub message: Option<String>,
    pub error: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AwaitAgentsResult {
    pub ready: Vec<AgentResult>,
    pub still_running: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentOverview {
    pub agent_id: String,
    pub name: String,
    pub backend_kind: BackendKind,
    pub status: String,
    pub workspace_roots: Vec<String>,
    pub parent_agent_id: Option<String>,
    pub project_id: Option<String>,
    pub created_at_ms: u64,
    pub last_message: Option<String>,
    pub error: Option<String>,
    pub summary: Option<String>,
}

fn agent_result_from_state(state: &AgentState) -> AgentResult {
    AgentResult {
        agent_id: state.agent_id.0.clone(),
        status: state.status_label().to_string(),
        message: state.last_message.clone(),
        error: state.last_error.clone(),
        summary: summary_from(state.last_message.as_deref(), state.last_error.as_deref()),
    }
}

fn agent_overview_from_state(state: &AgentState) -> AgentOverview {
    AgentOverview {
        agent_id: state.agent_id.0.clone(),
        name: state.name.clone(),
        backend_kind: state.backend_kind,
        status: state.status_label().to_string(),
        workspace_roots: state.workspace_roots.clone(),
        parent_agent_id: state.parent_agent_id.as_ref().map(|value| value.0.clone()),
        project_id: state.project_id.as_ref().map(|value| value.0.clone()),
        created_at_ms: state.created_at_ms,
        last_message: state.last_message.clone(),
        error: state.last_error.clone(),
        summary: summary_from(state.last_message.as_deref(), state.last_error.as_deref()),
    }
}

fn summary_from(message: Option<&str>, error: Option<&str>) -> Option<String> {
    let source = message.filter(|value| !value.trim().is_empty()).or(error)?;
    let line = source.lines().next().unwrap_or(source).trim();
    if line.is_empty() {
        return None;
    }
    if line.len() <= 160 {
        Some(line.to_string())
    } else {
        Some(format!("{}...", &line[..157]))
    }
}

fn resolve_watched_ids(
    snapshot: &SnapshotState,
    requested_ids: Option<&[AgentId]>,
) -> Result<Vec<AgentId>, String> {
    match requested_ids {
        Some(ids) => {
            for id in ids {
                if !snapshot.agents.contains_key(id) {
                    return Err(format!("unknown agent_id {}", id.0));
                }
            }
            Ok(ids.to_vec())
        }
        None => Ok(snapshot
            .agents
            .values()
            .filter(|agent| agent.is_active())
            .map(|agent| agent.agent_id.clone())
            .collect()),
    }
}

fn watched_activity_map(
    snapshot: &SnapshotState,
    requested_ids: Option<&[AgentId]>,
) -> Result<HashMap<AgentId, u64>, String> {
    let watched_ids = resolve_watched_ids(snapshot, requested_ids)?;
    Ok(watched_ids
        .into_iter()
        .map(|agent_id| {
            let activity = snapshot
                .agents
                .get(&agent_id)
                .map(|agent| agent.activity_counter)
                .unwrap_or(0);
            (agent_id, activity)
        })
        .collect())
}

fn ready_agents_from_snapshot(
    snapshot: &SnapshotState,
    watched_ids: &[AgentId],
) -> Vec<AgentResult> {
    watched_ids
        .iter()
        .filter_map(|agent_id| snapshot.agents.get(agent_id))
        .filter(|agent| !agent.is_active())
        .map(agent_result_from_state)
        .collect()
}

fn still_running_agent_ids(snapshot: &SnapshotState, watched_ids: &[AgentId]) -> Vec<String> {
    watched_ids
        .iter()
        .filter_map(|agent_id| snapshot.agents.get(agent_id))
        .filter(|agent| agent.is_active())
        .map(|agent| agent.agent_id.0.clone())
        .collect()
}

async fn run_runtime(
    mut connection: client::Connection,
    mut command_rx: mpsc::Receiver<Command>,
    snapshot_tx: watch::Sender<SnapshotState>,
    ready_tx: oneshot::Sender<Result<(), String>>,
) {
    let mut state = RuntimeState::new();
    let bootstrap_result = bootstrap_runtime(&mut connection, &mut state, &snapshot_tx).await;
    let _ = ready_tx.send(bootstrap_result.clone());
    if let Err(error) = bootstrap_result {
        let mut snapshot = state.snapshot.clone();
        snapshot.connection_error = Some(error);
        let _ = snapshot_tx.send(snapshot);
        return;
    }

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    return;
                };

                match command {
                    Command::Spawn { request, reply } => {
                        if state.pending_spawn.is_some() {
                            let _ = reply.send(Err("another spawn is already pending on this MCP connection".to_string()));
                            continue;
                        }
                        let payload = SpawnAgentPayload {
                            name: request.name.clone(),
                            parent_agent_id: request.parent_agent_id.clone(),
                            project_id: request.project_id.clone(),
                            params: SpawnAgentParams::New {
                                workspace_roots: request.workspace_roots.clone(),
                                prompt: request.prompt,
                                images: None,
                                backend_kind: request.backend_kind,
                                cost_hint: request.cost_hint,
                            },
                        };
                        if let Err(err) = connection.spawn_agent(payload).await {
                            let _ = reply.send(Err(format!("failed to send spawn_agent to Tyde host: {err:?}")));
                            continue;
                        }
                        state.pending_spawn = Some(PendingSpawn {
                            expected_name: request.name,
                            expected_backend_kind: request.backend_kind,
                            expected_workspace_roots: request.workspace_roots,
                            expected_project_id: request.project_id,
                            expected_parent_agent_id: request.parent_agent_id,
                            reply,
                        });
                    }
                    Command::SendMessage { agent_id, message, reply } => {
                        let Some(stream) = state
                            .snapshot
                            .agents
                            .get(&agent_id)
                            .map(|agent| agent.instance_stream.clone())
                        else {
                            let _ = reply.send(Err(format!("unknown agent_id {}", agent_id.0)));
                            continue;
                        };
                        match connection.send_message_payload(
                            &stream,
                            SendMessagePayload {
                                message,
                                images: None,
                            },
                        ).await {
                            Ok(()) => {
                                let agent = state
                                    .snapshot
                                    .agents
                                    .get_mut(&agent_id)
                                    .expect("agent must still exist after send_message");
                                agent.turn_completed = false;
                                agent.activity_counter =
                                    agent.activity_counter.saturating_add(1);
                                publish_snapshot(&mut state.snapshot, &snapshot_tx);
                                let _ = reply.send(Ok(()));
                            }
                            Err(err) => {
                                let _ = reply.send(Err(format!("failed to send agent message: {err:?}")));
                            }
                        }
                    }
                    Command::Interrupt { agent_id, reply } => {
                        let Some(agent) = state.snapshot.agents.get(&agent_id) else {
                            let _ = reply.send(Err(format!("unknown agent_id {}", agent_id.0)));
                            continue;
                        };
                        let stream = agent.instance_stream.clone();
                        let result = connection
                            .interrupt(&stream)
                            .await
                            .map_err(|err| format!("failed to interrupt agent: {err:?}"));
                        let _ = reply.send(result);
                    }
                }
            }
            incoming = connection.next_event() => {
                match incoming {
                    Ok(Some(envelope)) => {
                        apply_envelope(&mut state.snapshot, &envelope);
                        if envelope.kind == FrameKind::NewAgent {
                            let payload: NewAgentPayload = envelope
                                .parse_payload()
                                .expect("validated NewAgent payload should parse");
                            if let Some(pending) = state.pending_spawn.take() {
                                if pending.matches(&payload) {
                                    let status_label = state
                                        .snapshot
                                        .agents
                                        .get(&payload.agent_id)
                                        .map(|agent| agent.status_label())
                                        .unwrap_or("thinking");
                                    let _ = pending.reply.send(Ok(SpawnAgentResult {
                                        agent_id: payload.agent_id.0.clone(),
                                        name: payload.name,
                                        status: status_label.to_string(),
                                    }));
                                } else {
                                    state.pending_spawn = Some(pending);
                                }
                            }
                        }
                        publish_snapshot(&mut state.snapshot, &snapshot_tx);
                    }
                    Ok(None) => {
                        let message = "Tyde host connection closed".to_string();
                        fail_runtime(&mut state, &snapshot_tx, message);
                        return;
                    }
                    Err(err) => {
                        let message = format!("Tyde host connection failed: {err:?}");
                        fail_runtime(&mut state, &snapshot_tx, message);
                        return;
                    }
                }
            }
        }
    }
}

async fn bootstrap_runtime(
    connection: &mut client::Connection,
    state: &mut RuntimeState,
    snapshot_tx: &watch::Sender<SnapshotState>,
) -> Result<(), String> {
    let first = connection
        .next_event()
        .await
        .map_err(|err| format!("failed to read initial host event: {err:?}"))?
        .ok_or_else(|| "Tyde host closed before sending initial HostSettings".to_string())?;

    if first.kind != FrameKind::HostSettings {
        return Err(format!(
            "expected initial HostSettings event from Tyde host, received {}",
            first.kind
        ));
    }
    apply_envelope(&mut state.snapshot, &first);
    publish_snapshot(&mut state.snapshot, snapshot_tx);

    loop {
        match timeout(BOOTSTRAP_QUIET_PERIOD, connection.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                apply_envelope(&mut state.snapshot, &envelope);
                publish_snapshot(&mut state.snapshot, snapshot_tx);
            }
            Ok(Ok(None)) => {
                return Err("Tyde host connection closed during bootstrap replay".to_string());
            }
            Ok(Err(err)) => {
                return Err(format!("failed while consuming bootstrap replay: {err:?}"));
            }
            Err(_) => return Ok(()),
        }
    }
}

fn publish_snapshot(snapshot: &mut SnapshotState, snapshot_tx: &watch::Sender<SnapshotState>) {
    snapshot.version = snapshot.version.saturating_add(1);
    let _ = snapshot_tx.send(snapshot.clone());
}

fn fail_runtime(
    state: &mut RuntimeState,
    snapshot_tx: &watch::Sender<SnapshotState>,
    message: String,
) {
    if let Some(pending) = state.pending_spawn.take() {
        let _ = pending.reply.send(Err(message.clone()));
    }
    state.snapshot.connection_error = Some(message);
    publish_snapshot(&mut state.snapshot, snapshot_tx);
}

fn apply_envelope(snapshot: &mut SnapshotState, envelope: &protocol::Envelope) {
    match envelope.kind {
        FrameKind::HostSettings => {
            let payload: HostSettingsPayload = envelope
                .parse_payload()
                .expect("validated HostSettings payload should parse");
            snapshot.host_settings = Some(payload.settings);
        }
        FrameKind::NewAgent => {
            let payload: NewAgentPayload = envelope
                .parse_payload()
                .expect("validated NewAgent payload should parse");
            let activity = snapshot
                .agents
                .get(&payload.agent_id)
                .map(|agent| agent.activity_counter.saturating_add(1))
                .unwrap_or(1);
            snapshot.agents.insert(
                payload.agent_id.clone(),
                AgentState {
                    agent_id: payload.agent_id,
                    name: payload.name,
                    backend_kind: payload.backend_kind,
                    workspace_roots: payload.workspace_roots,
                    project_id: payload.project_id,
                    parent_agent_id: payload.parent_agent_id,
                    created_at_ms: payload.created_at_ms,
                    instance_stream: payload.instance_stream,
                    is_thinking: false,
                    turn_completed: false,
                    terminated: false,
                    last_message: None,
                    last_error: None,
                    activity_counter: activity,
                },
            );
        }
        FrameKind::AgentStart => {
            let payload: AgentStartPayload = envelope
                .parse_payload()
                .expect("validated AgentStart payload should parse");
            let stream_agent_id = parse_agent_id_from_stream(&envelope.stream);
            assert_eq!(
                stream_agent_id, payload.agent_id,
                "agent_start payload agent_id {} must match stream {}",
                payload.agent_id.0, envelope.stream.0
            );
            if let Some(agent) = snapshot.agents.get_mut(&payload.agent_id) {
                agent.activity_counter = agent.activity_counter.saturating_add(1);
            }
        }
        FrameKind::ChatEvent => {
            let payload: ChatEvent = envelope
                .parse_payload()
                .expect("validated ChatEvent payload should parse");
            let agent_id = parse_agent_id_from_stream(&envelope.stream);
            let agent = snapshot.agents.get_mut(&agent_id).unwrap_or_else(|| {
                panic!(
                    "chat event arrived for unknown agent stream {}",
                    envelope.stream.0
                )
            });
            agent.activity_counter = agent.activity_counter.saturating_add(1);
            match payload {
                ChatEvent::TypingStatusChanged(typing) => {
                    agent.is_thinking = typing;
                }
                ChatEvent::StreamStart(_) => {
                    agent.last_error = None;
                }
                ChatEvent::StreamDelta(_) | ChatEvent::StreamReasoningDelta(_) => {}
                ChatEvent::StreamEnd(data) => {
                    agent.turn_completed = true;
                    agent.last_message = Some(data.message.content);
                    agent.last_error = None;
                }
                ChatEvent::OperationCancelled(data) => {
                    agent.turn_completed = true;
                    agent.last_message = Some(data.message);
                }
                ChatEvent::MessageAdded(_)
                | ChatEvent::ToolRequest(_)
                | ChatEvent::ToolExecutionCompleted(_)
                | ChatEvent::TaskUpdate(_)
                | ChatEvent::RetryAttempt(_) => {}
            }
        }
        FrameKind::AgentError => {
            let payload: AgentErrorPayload = envelope
                .parse_payload()
                .expect("validated AgentError payload should parse");
            let stream_agent_id = parse_agent_id_from_stream(&envelope.stream);
            assert_eq!(
                stream_agent_id, payload.agent_id,
                "agent_error payload agent_id {} must match stream {}",
                payload.agent_id.0, envelope.stream.0
            );
            let agent = snapshot
                .agents
                .get_mut(&payload.agent_id)
                .unwrap_or_else(|| {
                    panic!(
                        "agent error arrived for unknown agent stream {}",
                        envelope.stream.0
                    )
                });
            agent.activity_counter = agent.activity_counter.saturating_add(1);
            agent.last_error = Some(payload.message.clone());
            if payload.fatal || payload.message == "agent not running" {
                agent.is_thinking = false;
                agent.turn_completed = true;
                agent.terminated = true;
            }
        }
        _ => {}
    }
}

fn parse_agent_id_from_stream(stream: &StreamPath) -> AgentId {
    let mut segments = stream.0.split('/');
    let leading = segments.next();
    let topic = segments.next();
    let agent_id = segments.next();
    let instance_id = segments.next();
    let trailing = segments.next();

    assert_eq!(leading, Some(""), "stream must be absolute: {}", stream.0);
    assert_eq!(
        topic,
        Some("agent"),
        "stream must be /agent/...: {}",
        stream.0
    );
    let Some(agent_id) = agent_id else {
        panic!("missing agent_id in stream {}", stream.0);
    };
    let Some(instance_id) = instance_id else {
        panic!("missing instance_id in stream {}", stream.0);
    };
    assert!(
        trailing.is_none(),
        "unexpected extra stream segment in {}",
        stream.0
    );

    Uuid::parse_str(agent_id)
        .unwrap_or_else(|err| panic!("invalid agent_id {agent_id} in {}: {err}", stream.0));
    Uuid::parse_str(instance_id)
        .unwrap_or_else(|err| panic!("invalid instance_id {instance_id} in {}: {err}", stream.0));
    AgentId(agent_id.to_string())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackendKindInput {
    Tycode,
    Kiro,
    Claude,
    Codex,
    Gemini,
}

impl From<BackendKindInput> for BackendKind {
    fn from(value: BackendKindInput) -> Self {
        match value {
            BackendKindInput::Tycode => Self::Tycode,
            BackendKindInput::Kiro => Self::Kiro,
            BackendKindInput::Claude => Self::Claude,
            BackendKindInput::Codex => Self::Codex,
            BackendKindInput::Gemini => Self::Gemini,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CostHintInput {
    Low,
    Med,
    High,
}

impl From<CostHintInput> for SpawnCostHint {
    fn from(value: CostHintInput) -> Self {
        match value {
            CostHintInput::Low => Self::Low,
            CostHintInput::Med => Self::Medium,
            CostHintInput::High => Self::High,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpawnAgentToolInput {
    workspace_roots: Vec<String>,
    prompt: String,
    backend_kind: Option<BackendKindInput>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    cost_hint: Option<CostHintInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunAgentToolInput {
    workspace_roots: Vec<String>,
    prompt: String,
    backend_kind: Option<BackendKindInput>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    cost_hint: Option<CostHintInput>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct SpawnRequestInput {
    workspace_roots: Vec<String>,
    prompt: String,
    backend_kind: Option<BackendKindInput>,
    parent_agent_id: Option<String>,
    project_id: Option<String>,
    name: Option<String>,
    cost_hint: Option<CostHintInput>,
}

impl From<SpawnAgentToolInput> for SpawnRequestInput {
    fn from(value: SpawnAgentToolInput) -> Self {
        Self {
            workspace_roots: value.workspace_roots,
            prompt: value.prompt,
            backend_kind: value.backend_kind,
            parent_agent_id: value.parent_agent_id,
            project_id: value.project_id,
            name: value.name,
            cost_hint: value.cost_hint,
        }
    }
}

impl From<&RunAgentToolInput> for SpawnRequestInput {
    fn from(value: &RunAgentToolInput) -> Self {
        Self {
            workspace_roots: value.workspace_roots.clone(),
            prompt: value.prompt.clone(),
            backend_kind: value.backend_kind,
            parent_agent_id: value.parent_agent_id.clone(),
            project_id: value.project_id.clone(),
            name: value.name.clone(),
            cost_hint: value.cost_hint,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AwaitAgentsToolInput {
    agent_ids: Option<Vec<String>>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendAgentMessageToolInput {
    agent_id: String,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CancelAgentToolInput {
    agent_id: String,
    timeout_ms: Option<u64>,
}

fn parse_agent_id(input: &str) -> Result<AgentId, String> {
    Uuid::parse_str(input).map_err(|err| format!("invalid agent_id '{input}': {err}"))?;
    Ok(AgentId(input.to_string()))
}

fn parse_project_id(input: &str) -> Result<ProjectId, String> {
    Uuid::parse_str(input).map_err(|err| format!("invalid project_id '{input}': {err}"))?;
    Ok(ProjectId(input.to_string()))
}

fn build_spawn_request(
    snapshot: &SnapshotState,
    input: SpawnRequestInput,
) -> Result<SpawnRequest, String> {
    let SpawnRequestInput {
        workspace_roots,
        prompt,
        backend_kind,
        parent_agent_id,
        project_id,
        name,
        cost_hint,
    } = input;

    if workspace_roots.is_empty() {
        return Err("workspace_roots must contain at least one root".to_string());
    }
    if workspace_roots.iter().any(|root| root.trim().is_empty()) {
        return Err("workspace_roots must not contain empty values".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }

    let backend_kind = backend_kind
        .map(BackendKind::from)
        .or_else(|| {
            snapshot
                .host_settings
                .as_ref()
                .and_then(|settings| settings.default_backend)
        })
        .ok_or_else(|| {
            "backend_kind is required because the host has no default_backend".to_string()
        })?;

    let parent_agent_id = parent_agent_id.as_deref().map(parse_agent_id).transpose()?;
    let project_id = project_id.as_deref().map(parse_project_id).transpose()?;
    let name = name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_agent_name(backend_kind));

    Ok(SpawnRequest {
        workspace_roots,
        prompt,
        backend_kind,
        parent_agent_id,
        project_id,
        name,
        cost_hint: cost_hint.map(SpawnCostHint::from),
    })
}

fn default_agent_name(backend_kind: BackendKind) -> String {
    let prefix = match backend_kind {
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Gemini => "gemini",
    };
    let id = Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &id[..8])
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse<T> {
    jsonrpc: &'static str,
    id: Value,
    result: T,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorResponse {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcErrorObject,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallToolParams {
    name: String,
    arguments: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    protocol_version: &'static str,
    capabilities: InitializeCapabilities,
    server_info: ServerInfoPayload,
    instructions: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeCapabilities {
    tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolsCapability {
    list_changed: bool,
}

#[derive(Debug, Serialize)]
struct ServerInfoPayload {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct ToolsListResult {
    tools: Vec<ToolDefinition>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallResult {
    content: Vec<TextContent>,
    is_error: bool,
}

#[derive(Debug, Serialize)]
struct TextContent {
    #[serde(rename = "type")]
    type_name: &'static str,
    text: String,
}

impl ToolCallResult {
    fn json<T: Serialize>(value: T) -> Self {
        Self {
            content: vec![TextContent {
                type_name: "text",
                text: serde_json::to_string(&value)
                    .expect("tool result serialization should not fail"),
            }],
            is_error: false,
        }
    }

    fn text_error(message: impl Into<String>) -> Self {
        Self {
            content: vec![TextContent {
                type_name: "text",
                text: message.into(),
            }],
            is_error: true,
        }
    }
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "tyde_spawn_agent",
            description: "Spawn a Tyde agent and return immediately with its agent_id. Use this when you want to launch multiple agents in parallel and then wait for them with tyde_await_agent.",
            input_schema: spawn_agent_schema(),
        },
        ToolDefinition {
            name: "tyde_run_agent",
            description: "Spawn a Tyde agent and block until its next turn completes, is cancelled, or fails. Returns the latest message and status.",
            input_schema: run_agent_schema(),
        },
        ToolDefinition {
            name: "tyde_await_agent",
            description: "Block until one or more watched agents stop running. Returns the ready agents and the IDs that are still running. If agent_ids is omitted, watches all currently running agents.",
            input_schema: await_agent_schema(),
        },
        ToolDefinition {
            name: "tyde_send_agent_message",
            description: "Send a follow-up message to an existing Tyde agent.",
            input_schema: send_message_schema(),
        },
        ToolDefinition {
            name: "tyde_cancel_agent",
            description: "Interrupt a running Tyde agent and then wait for its next non-running state.",
            input_schema: cancel_agent_schema(),
        },
        ToolDefinition {
            name: "tyde_list_agents",
            description: "List all agents currently known to this Tyde host connection.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

fn backend_kind_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["tycode", "kiro", "claude", "codex", "gemini"]
    })
}

fn cost_hint_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["low", "med", "high"]
    })
}

fn spawn_agent_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "workspace_roots": { "type": "array", "items": { "type": "string" } },
            "prompt": { "type": "string" },
            "backend_kind": backend_kind_schema(),
            "parent_agent_id": { "type": "string" },
            "project_id": { "type": "string" },
            "name": { "type": "string" },
            "cost_hint": cost_hint_schema()
        },
        "required": ["workspace_roots", "prompt"],
        "additionalProperties": false
    })
}

fn run_agent_schema() -> Value {
    let mut schema = spawn_agent_schema();
    schema["properties"]["timeout_ms"] = json!({ "type": "integer", "minimum": 0 });
    schema
}

fn await_agent_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_ids": { "type": "array", "items": { "type": "string" } },
            "timeout_ms": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": false
    })
}

fn send_message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "string" },
            "message": { "type": "string" }
        },
        "required": ["agent_id", "message"],
        "additionalProperties": false
    })
}

fn cancel_agent_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "string" },
            "timeout_ms": { "type": "integer", "minimum": 0 }
        },
        "required": ["agent_id"],
        "additionalProperties": false
    })
}

async fn dispatch_tool(control: &AgentControlHandle, params: CallToolParams) -> ToolCallResult {
    match params.name.as_str() {
        "tyde_spawn_agent" => {
            let input = match parse_tool_input::<SpawnAgentToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            let request = match build_spawn_request(&control.snapshot(), input.into()) {
                Ok(request) => request,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match control.spawn_agent(request).await {
                Ok(result) => ToolCallResult::json(result),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_run_agent" => {
            let input = match parse_tool_input::<RunAgentToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            let request = match build_spawn_request(&control.snapshot(), (&input).into()) {
                Ok(request) => request,
                Err(err) => return ToolCallResult::text_error(err),
            };
            let spawned = match control.spawn_agent(request).await {
                Ok(result) => result,
                Err(err) => return ToolCallResult::text_error(err),
            };
            match control
                .await_agents(
                    Some(vec![AgentId(spawned.agent_id.clone())]),
                    input.timeout_ms,
                )
                .await
            {
                Ok(result) => {
                    ToolCallResult::json(result.ready.into_iter().next().unwrap_or(AgentResult {
                        agent_id: spawned.agent_id,
                        status: "thinking".to_string(),
                        message: None,
                        error: None,
                        summary: None,
                    }))
                }
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_await_agent" => {
            let input = match parse_tool_input::<AwaitAgentsToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            let agent_ids = match input.agent_ids {
                Some(values) => {
                    let mut parsed = Vec::with_capacity(values.len());
                    for value in values {
                        match parse_agent_id(&value) {
                            Ok(agent_id) => parsed.push(agent_id),
                            Err(err) => return ToolCallResult::text_error(err),
                        }
                    }
                    Some(parsed)
                }
                None => None,
            };
            match control.await_agents(agent_ids, input.timeout_ms).await {
                Ok(result) => ToolCallResult::json(result),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_send_agent_message" => {
            let input = match parse_tool_input::<SendAgentMessageToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            let agent_id = match parse_agent_id(&input.agent_id) {
                Ok(agent_id) => agent_id,
                Err(err) => return ToolCallResult::text_error(err),
            };
            if input.message.trim().is_empty() {
                return ToolCallResult::text_error("message must not be empty");
            }
            match control.send_message(agent_id, input.message).await {
                Ok(()) => ToolCallResult::json(json!({ "ok": true })),
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_cancel_agent" => {
            let input = match parse_tool_input::<CancelAgentToolInput>(params.arguments) {
                Ok(input) => input,
                Err(err) => return ToolCallResult::text_error(err),
            };
            let agent_id = match parse_agent_id(&input.agent_id) {
                Ok(agent_id) => agent_id,
                Err(err) => return ToolCallResult::text_error(err),
            };
            if let Err(err) = control.interrupt(agent_id.clone()).await {
                return ToolCallResult::text_error(err);
            }
            match control
                .await_agents(Some(vec![agent_id]), input.timeout_ms.or(Some(10_000)))
                .await
            {
                Ok(result) => {
                    ToolCallResult::json(result.ready.into_iter().next().unwrap_or(AgentResult {
                        agent_id: input.agent_id,
                        status: "thinking".to_string(),
                        message: None,
                        error: None,
                        summary: None,
                    }))
                }
                Err(err) => ToolCallResult::text_error(err),
            }
        }
        "tyde_list_agents" => ToolCallResult::json(control.list_agents().await),
        other => ToolCallResult::text_error(format!("unknown tool '{other}'")),
    }
}

fn parse_tool_input<T: for<'de> Deserialize<'de>>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| format!("invalid tool arguments: {err}"))
}

async fn handle_request<W: AsyncWrite + Unpin>(
    control: &AgentControlHandle,
    writer: &mut W,
    request: JsonRpcRequest,
) -> Result<(), String> {
    match request.method.as_str() {
        "initialize" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: InitializeResult {
                        protocol_version: "2025-03-26",
                        capabilities: InitializeCapabilities {
                            tools: ToolsCapability { list_changed: false },
                        },
                        server_info: ServerInfoPayload {
                            name: "tyde-agent-control",
                            version: "0.0.0",
                        },
                        instructions: "Tools for orchestrating Tyde2 coding agents over the real Tyde host protocol. Use tyde_run_agent for one-shot tasks and tyde_spawn_agent + tyde_await_agent for fan-out workflows.".to_string(),
                    },
                },
            )
            .await
        }
        "notifications/initialized" => Ok(()),
        "ping" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: json!({}),
                },
            )
            .await
        }
        "tools/list" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: ToolsListResult {
                        tools: tool_definitions(),
                    },
                },
            )
            .await
        }
        "tools/call" => {
            let Some(id) = request.id else {
                return Ok(());
            };
            let params: CallToolParams =
                serde_json::from_value(request.params.unwrap_or_else(|| json!({})))
                    .map_err(|err| format!("invalid tools/call params: {err}"))?;
            let result = dispatch_tool(control, params).await;
            write_mcp_message(
                writer,
                &JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result,
                },
            )
            .await
        }
        "notifications/cancelled" => Ok(()),
        other => {
            if let Some(id) = request.id {
                write_mcp_message(
                    writer,
                    &JsonRpcErrorResponse {
                        jsonrpc: "2.0",
                        id,
                        error: JsonRpcErrorObject {
                            code: -32601,
                            message: format!("method not found: {other}"),
                        },
                    },
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn read_mcp_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>, String> {
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|err| format!("failed to read MCP header: {err}"))?;

        if read == 0 {
            if content_length.is_none() {
                return Ok(None);
            }
            return Err("unexpected EOF while reading MCP headers".to_string());
        }

        if line == "\r\n" || line == "\n" {
            break;
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|err| format!("invalid Content-Length header '{trimmed}': {err}"))?;
            content_length = Some(parsed);
        }
    }

    let Some(content_length) = content_length else {
        return Err("missing Content-Length header".to_string());
    };
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|err| format!("failed to read MCP body: {err}"))?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| format!("failed to parse MCP JSON body: {err}"))
}

async fn write_mcp_message<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), String> {
    let body =
        serde_json::to_vec(value).map_err(|err| format!("failed to serialize MCP JSON: {err}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|err| format!("failed to write MCP header: {err}"))?;
    writer
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write MCP body: {err}"))?;
    writer
        .flush()
        .await
        .map_err(|err| format!("failed to flush MCP output: {err}"))?;
    Ok(())
}

pub async fn run_stdio_server(target: AgentControlTarget) -> Result<(), String> {
    let control = AgentControlHandle::connect(target).await?;
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = stdout;

    loop {
        let Some(message) = read_mcp_message(&mut reader).await? else {
            return Ok(());
        };
        let request: JsonRpcRequest = serde_json::from_value(message)
            .map_err(|err| format!("invalid JSON-RPC request: {err}"))?;
        handle_request(&control, &mut writer, request).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connect_runtime(host: server::HostHandle) -> AgentControlHandle {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();

        tokio::spawn(async move {
            let conn = server::accept(&server_config, server_stream)
                .await
                .expect("server handshake failed");
            if let Err(err) = server::run_connection(conn, host).await {
                panic!("server connection loop failed: {err:?}");
            }
        });

        let connection = client::connect(&ClientConfig::current(), client_stream)
            .await
            .expect("client handshake failed");
        AgentControlHandle::from_connection(connection)
            .await
            .expect("agent-control runtime should bootstrap")
    }

    fn test_host() -> (server::HostHandle, tempfile::TempDir) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let session_path = tempdir.path().join("sessions.json");
        let project_path = tempdir.path().join("projects.json");
        let settings_path = tempdir.path().join("settings.json");
        (
            server::spawn_host_with_mock_backend(session_path, project_path, settings_path)
                .expect("spawn mock host"),
            tempdir,
        )
    }

    #[tokio::test]
    async fn run_agent_returns_completed_turn() {
        let (host, _tempdir) = test_host();
        let control = connect_runtime(host).await;
        let request = SpawnRequest {
            workspace_roots: vec!["/tmp/test".to_string()],
            prompt: "hello".to_string(),
            backend_kind: BackendKind::Claude,
            parent_agent_id: None,
            project_id: None,
            name: "test-agent".to_string(),
            cost_hint: None,
        };

        let spawned = control
            .spawn_agent(request)
            .await
            .expect("spawn should succeed");
        let awaited = control
            .await_agents(Some(vec![AgentId(spawned.agent_id.clone())]), Some(5_000))
            .await
            .expect("await should succeed");

        let result = awaited.ready.first().expect("agent should be ready");
        assert_eq!(result.status, "idle");
        assert!(
            result
                .message
                .as_deref()
                .is_some_and(|message| message.contains("mock backend response to: hello"))
        );
    }

    #[tokio::test]
    async fn send_message_updates_existing_agent() {
        let (host, _tempdir) = test_host();
        let control = connect_runtime(host).await;
        let request = SpawnRequest {
            workspace_roots: vec!["/tmp/test".to_string()],
            prompt: "first".to_string(),
            backend_kind: BackendKind::Claude,
            parent_agent_id: None,
            project_id: None,
            name: "send-message-agent".to_string(),
            cost_hint: None,
        };

        let spawned = control
            .spawn_agent(request)
            .await
            .expect("spawn should succeed");
        let agent_id = AgentId(spawned.agent_id.clone());
        let _ = control
            .await_agents(Some(vec![agent_id.clone()]), Some(5_000))
            .await
            .expect("initial await should succeed");

        control
            .send_message(agent_id.clone(), "follow up".to_string())
            .await
            .expect("send_message should succeed");
        let awaited = control
            .await_agents(Some(vec![agent_id.clone()]), Some(5_000))
            .await
            .expect("follow-up await should succeed");

        let result = awaited.ready.first().expect("agent should be ready");
        assert!(
            result
                .message
                .as_deref()
                .is_some_and(|message| message.contains("mock backend response to: follow up"))
        );

        let agents = control.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, agent_id.0);
        assert_eq!(agents[0].status, "idle");
    }
}
