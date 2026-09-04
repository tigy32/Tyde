use settings_model::HostBootstrapPayload;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentId, AgentStartPayload, BackendKind, ChatEvent,
    CustomAgentNotifyPayload, Envelope, ExitPlanModeDecision, FrameKind, NewAgentPayload,
    QueuedMessagesPayload, SendMessagePayload, SendMessageToolResponse, SettingsWriteId,
    SettingsWriteResultPayload, SpawnAgentParams, SpawnAgentPayload, StreamPath,
    ToolExecutionCompletedData, ToolRequest,
};
use tyde_dev_driver::agent_control::AgentControlHandle;

#[allow(dead_code)]
const BUILTIN_TEAM_CUSTOM_AGENT_IDS: &[&str] = &[
    "tyde-team-lead",
    "tyde-code-reviewer",
    "tyde-frontend-engineer",
    "tyde-backend-engineer",
    "tyde-test-qa-engineer",
    "tyde-debugger",
];

#[allow(dead_code)]
pub fn is_builtin_team_custom_agent_notify(env: &Envelope) -> bool {
    if env.kind != FrameKind::CustomAgentNotify {
        return false;
    }
    let payload = env
        .parse_payload::<CustomAgentNotifyPayload>()
        .expect("parse CustomAgentNotifyPayload while checking built-in team custom agent");
    match payload {
        CustomAgentNotifyPayload::Upsert { custom_agent } => {
            BUILTIN_TEAM_CUSTOM_AGENT_IDS.contains(&custom_agent.id.0.as_str())
        }
        CustomAgentNotifyPayload::Delete { .. } => false,
    }
}

/// Frame kinds every connection may see at any time regardless of what it is
/// waiting for. Kept as a slice so strict waits can name it in their
/// `allowed_noise` list; [`is_routine_control_plane_frame`] is the predicate
/// form of the same set (plus built-in-team `CustomAgentNotify` upserts).
#[allow(dead_code)]
pub const ROUTINE_CONTROL_PLANE_KINDS: &[FrameKind] = &[
    FrameKind::SessionSettings,
    FrameKind::QueuedMessages,
    FrameKind::SessionSchemas,
    FrameKind::LaunchProfileCatalogNotify,
    FrameKind::BackendSetup,
];

#[allow(dead_code)]
pub fn is_routine_control_plane_frame(env: &Envelope) -> bool {
    is_builtin_team_custom_agent_notify(env) || ROUTINE_CONTROL_PLANE_KINDS.contains(&env.kind)
}

#[allow(dead_code)]
pub async fn expect_settings_write_applied(
    client: &mut client::Connection,
    write_id: &SettingsWriteId,
    context: &str,
) -> SettingsWriteResultPayload {
    let result = expect_settings_write_result(client, write_id, context).await;
    assert!(result.applied, "{context}: {:?}", result.field_errors);
    result
}

#[allow(dead_code)]
pub async fn expect_settings_write_result(
    client: &mut client::Connection,
    write_id: &SettingsWriteId,
    context: &str,
) -> SettingsWriteResultPayload {
    next_frame_matching_on(client, context, |env| {
        env.kind == FrameKind::SettingsWriteResult
            && env
                .parse_payload::<SettingsWriteResultPayload>()
                .is_ok_and(|result| result.write_id == *write_id)
    })
    .await
    .parse_payload()
    .expect("parse SettingsWriteResult")
}

/// `allowed_noise` list for a strict wait that tolerates the routine control
/// plane plus the caller's own ambient kinds — the slice form of
/// `if is_routine_control_plane_frame(&env) || matches!(env.kind, ...)`.
#[allow(dead_code)]
pub fn routine_control_plane_noise_plus(extra: &[FrameKind]) -> Vec<FrameKind> {
    ROUTINE_CONTROL_PLANE_KINDS
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect()
}

pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::WARN.into())
        .from_env_lossy();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

pub struct Fixture {
    pub client: client::Connection,
    #[allow(dead_code)]
    pub bootstrap: HostBootstrapPayload,
    #[allow(dead_code)]
    host: server::HostHandle,
    #[allow(dead_code)]
    session_store_dir: tempfile::TempDir,
    antigravity_conversations_dir: tempfile::TempDir,
}

impl Fixture {
    #[allow(dead_code)]
    pub async fn new() -> Self {
        Self::new_with_runtime_config(server::HostRuntimeConfig::default()).await
    }

    /// Like [`Fixture::new`] but actually probes the real backend CLIs
    /// (`<cli> --version`, codex model discovery, etc.). Spawning real
    /// subprocesses costs several seconds per fixture, so only the handful of
    /// tests asserting on backend-setup *contents* should use this with the
    /// exact enabled backends they exercise — everyone else gets the fast stub
    /// via `new`/`new_with_runtime_config`.
    #[allow(dead_code)]
    pub async fn new_with_real_backend_probe_for_enabled_backends(
        enabled_backends: Vec<BackendKind>,
    ) -> Self {
        Self::new_with_runtime_config_inner(
            server::HostRuntimeConfig::default(),
            false,
            Some(enabled_backends),
            true,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn new_with_runtime_config_and_real_backend_probe_for_enabled_backends(
        runtime_config: server::HostRuntimeConfig,
        enabled_backends: Vec<BackendKind>,
    ) -> Self {
        Self::new_with_runtime_config_inner(runtime_config, false, Some(enabled_backends), true)
            .await
    }

    #[allow(dead_code)]
    pub async fn new_with_real_tycode_backend() -> Self {
        Self::new_with_runtime_config_inner(
            server::HostRuntimeConfig::default(),
            false,
            Some(vec![BackendKind::Claude]),
            false,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn new_with_test_backend(backend_kind: BackendKind) -> Self {
        Self::new_with_runtime_config_inner(
            server::HostRuntimeConfig::default(),
            true,
            Some(vec![backend_kind]),
            false,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn new_with_runtime_config(runtime_config: server::HostRuntimeConfig) -> Self {
        Self::new_with_runtime_config_inner(runtime_config, true, None, true).await
    }

    async fn new_with_runtime_config_inner(
        mut runtime_config: server::HostRuntimeConfig,
        skip_real_backend_probe: bool,
        enabled_backends: Option<Vec<BackendKind>>,
        use_mock_backend: bool,
    ) -> Self {
        init_tracing();

        // Real backend probing spawns `<cli> --version` for every backend and
        // runs codex model discovery (a network RPC) on every host spawn —
        // several seconds each, paid once per fixture. The default test
        // fixture skips it so the suite stays fast; tests that assert on probe
        // output opt back in via `new_with_real_backend_probe`.
        runtime_config.skip_real_backend_probe = skip_real_backend_probe;

        let antigravity_conversations_dir =
            tempfile::tempdir().expect("create Antigravity conversations tempdir");
        runtime_config.antigravity_conversations_dir =
            Some(antigravity_conversations_dir.path().to_path_buf());
        let session_store_dir = tempfile::tempdir().expect("create session tempdir");
        let session_path = session_store_dir.path().join("sessions.json");
        let project_path = session_store_dir.path().join("projects.json");
        let settings_path = session_store_dir.path().join("settings.json");
        if let Some(enabled_backends) = enabled_backends {
            let store = server::store::settings::HostSettingsStore::load(settings_path.clone())
                .expect("load fixture settings store");
            let mut settings = store.get().expect("read fixture settings store");
            settings.enabled_backends = enabled_backends;
            store
                .replace(settings)
                .expect("seed fixture enabled backends");
        }
        let host = if use_mock_backend {
            server::spawn_host_with_mock_backend_and_runtime_config(
                session_path,
                project_path,
                settings_path,
                runtime_config,
            )
        } else {
            server::spawn_host_with_store_paths_and_runtime_config(
                session_path,
                project_path,
                settings_path,
                runtime_config,
            )
        }
        .expect("initialize fixture host");
        let (client, bootstrap) = connect_client_with_bootstrap(host.clone()).await;

        Self {
            client,
            bootstrap,
            host,
            session_store_dir,
            antigravity_conversations_dir,
        }
    }

    #[allow(dead_code)]
    pub async fn connect(&self) -> client::Connection {
        connect_client(self.host.clone()).await
    }

    #[allow(dead_code)]
    pub async fn reconnect(&mut self) {
        self.client = connect_client(self.host.clone()).await;
    }

    #[allow(dead_code)]
    pub async fn connect_with_bootstrap(&self) -> (client::Connection, HostBootstrapPayload) {
        connect_client_with_bootstrap(self.host.clone()).await
    }

    #[allow(dead_code)]
    pub async fn connect_agent_control(&self) -> AgentControlHandle {
        let client = connect_raw_client(self.host.clone()).await;
        AgentControlHandle::from_connection(client)
            .await
            .expect("agent-control connection should bootstrap")
    }

    #[allow(dead_code)]
    pub async fn connect_fresh_host(&self) -> client::Connection {
        let host = server::spawn_host_with_mock_backend_and_runtime_config(
            self.session_store_path(),
            self.project_store_path(),
            self.settings_store_path(),
            self.fresh_host_runtime_config(),
        )
        .expect("initialize fresh host with existing stores");
        connect_client(host).await
    }

    #[allow(dead_code)]
    pub async fn connect_fresh_host_with_bootstrap(
        &self,
    ) -> (client::Connection, HostBootstrapPayload) {
        let host = server::spawn_host_with_mock_backend_and_runtime_config(
            self.session_store_path(),
            self.project_store_path(),
            self.settings_store_path(),
            self.fresh_host_runtime_config(),
        )
        .expect("initialize fresh host with existing stores");
        connect_client_with_bootstrap(host).await
    }

    #[allow(dead_code)]
    pub async fn agent_ids(&self) -> Vec<AgentId> {
        self.host.agent_ids().await
    }

    #[allow(dead_code)]
    pub async fn install_agent_name_test_gate(&self) -> server::InstalledAgentNameGate {
        self.host.install_agent_name_test_gate().await
    }

    #[allow(dead_code)]
    pub fn install_spawn_operation_completion_test_gate(
        &self,
    ) -> server::InstalledSpawnOperationTestGate {
        self.host.install_spawn_operation_completion_test_gate()
    }

    #[allow(dead_code)]
    pub fn install_spawn_operation_drain_test_gate(
        &self,
    ) -> server::InstalledSpawnOperationTestGate {
        self.host.install_spawn_operation_drain_test_gate()
    }

    #[allow(dead_code)]
    pub fn install_spawn_operation_publication_test_gate(
        &self,
    ) -> server::InstalledSpawnOperationTestGate {
        self.host.install_spawn_operation_publication_test_gate()
    }

    #[allow(dead_code)]
    pub fn host_for_test(&self) -> server::HostHandle {
        self.host.clone()
    }

    #[allow(dead_code)]
    pub fn spawn_operation_limits_for_test(&self) -> (usize, usize) {
        self.host.spawn_operation_limits_for_test()
    }

    #[allow(dead_code)]
    pub async fn shutdown_spawn_operations(&self) {
        self.host.shutdown_spawn_operations().await;
    }

    #[allow(dead_code)]
    pub async fn agent_control_http_url(&self) -> String {
        self.host.agent_control_mcp_url().await
    }

    #[allow(dead_code)]
    pub async fn agent_control_caller(&self, agent_id: &AgentId) -> server::AgentControlMcpCaller {
        self.host
            .agent_control_mcp_caller(agent_id)
            .await
            .expect("active agent should receive agent-control credentials")
    }

    #[allow(dead_code)]
    pub fn install_workbench_remove_test_hook(&self) -> server::InstalledWorkbenchRemoveHook {
        self.host.install_workbench_remove_test_hook()
    }

    #[allow(dead_code)]
    pub async fn review_mcp_http_url(&self) -> String {
        self.host.review_mcp_url().await
    }

    #[allow(dead_code)]
    pub async fn workflow_mcp_http_url(&self) -> String {
        self.host.workflow_mcp_url().await
    }

    fn session_store_path(&self) -> PathBuf {
        self.session_store_dir.path().join("sessions.json")
    }

    fn project_store_path(&self) -> PathBuf {
        self.session_store_dir.path().join("projects.json")
    }

    fn settings_store_path(&self) -> PathBuf {
        self.session_store_dir.path().join("settings.json")
    }

    fn fresh_host_runtime_config(&self) -> server::HostRuntimeConfig {
        server::HostRuntimeConfig {
            antigravity_conversations_dir: Some(
                self.antigravity_conversations_dir.path().to_path_buf(),
            ),
            skip_real_backend_probe: true,
            ..server::HostRuntimeConfig::default()
        }
    }

    #[allow(dead_code)]
    pub fn store_dir(&self) -> &Path {
        self.session_store_dir.path()
    }

    #[allow(dead_code)]
    pub fn antigravity_conversations_dir(&self) -> &Path {
        self.antigravity_conversations_dir.path()
    }
}

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TestAgent {
    pub stream: StreamPath,
    #[allow(dead_code)]
    pub new_agent: NewAgentPayload,
}

pub struct Turn {
    pub frames: Vec<Envelope>,
}

impl Turn {
    #[allow(dead_code)]
    pub fn chat_events(&self) -> Vec<ChatEvent> {
        self.frames
            .iter()
            .filter(|env| env.kind == FrameKind::ChatEvent)
            .map(|env| env.parse_payload().expect("parse ChatEvent"))
            .collect()
    }

    #[allow(dead_code)]
    pub fn queued_message_snapshots(&self) -> Vec<QueuedMessagesPayload> {
        self.frames
            .iter()
            .filter(|env| env.kind == FrameKind::QueuedMessages)
            .map(|env| env.parse_payload().expect("parse QueuedMessagesPayload"))
            .collect()
    }

    #[allow(dead_code)]
    pub fn saw_queue_drained(&self) -> bool {
        self.queued_message_snapshots()
            .iter()
            .any(|snapshot| snapshot.messages.is_empty())
    }

    #[allow(dead_code)]
    pub fn assert_stream_end_contains(&self, needle: &str) {
        assert!(
            self.chat_events().iter().any(|event| matches!(
                event,
                ChatEvent::StreamEnd(end) if end.message.content.contains(needle)
            )),
            "no StreamEnd containing {needle:?} in turn; events: {:?}",
            self.chat_events()
        );
    }

    #[allow(dead_code)]
    pub fn assert_tool_completed(&self, tool_call_id: &str) {
        let events = self.chat_events();
        assert!(
            events.iter().any(|event| matches!(
                event,
                ChatEvent::ToolExecutionCompleted(completion)
                    if completion.tool_call_id == tool_call_id
                        && tool_completion_succeeded(completion)
            )),
            "no successful ToolExecutionCompleted for {tool_call_id:?} in turn; events: {:?}",
            events
        );
    }
}

#[allow(dead_code)]
pub async fn next_frame_matching_on(
    client: &mut client::Connection,
    context: &str,
    mut matches: impl FnMut(&Envelope) -> bool,
) -> Envelope {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    let mut skipped: Vec<String> = Vec::new();
    loop {
        let env = match tokio::time::timeout_at(deadline, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed waiting for {context}"),
            Ok(Err(err)) => panic!("next_event failed waiting for {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}; skipped frames: {skipped:#?}"),
        };
        if matches(&env) {
            return env;
        }
        if env.kind == FrameKind::CustomAgentNotify {
            let _ = is_builtin_team_custom_agent_notify(&env);
        }
        skipped.push(format!("{:?} on {:?}", env.kind, env.stream));
    }
}

/// Wait for a matching frame and reject frames outside `allowed_noise`.
#[allow(dead_code)]
pub async fn next_frame_matching_strict_on(
    client: &mut client::Connection,
    context: &str,
    allowed_noise: &[FrameKind],
    matches: impl FnMut(&Envelope) -> bool,
) -> Envelope {
    next_frame_matching_strict_inner(client, context, allowed_noise, matches, false).await
}

/// Raw-stream variant for tests that assert on normally filtered voice frames.
#[allow(dead_code)]
pub async fn next_raw_frame_matching_strict_on(
    client: &mut client::Connection,
    context: &str,
    allowed_noise: &[FrameKind],
    matches: impl FnMut(&Envelope) -> bool,
) -> Envelope {
    next_frame_matching_strict_inner(client, context, allowed_noise, matches, true).await
}

async fn next_frame_matching_strict_inner(
    client: &mut client::Connection,
    context: &str,
    allowed_noise: &[FrameKind],
    mut matches: impl FnMut(&Envelope) -> bool,
    raw: bool,
) -> Envelope {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    let mut skipped: Vec<String> = Vec::new();
    loop {
        let env = if raw {
            let env = match tokio::time::timeout_at(deadline, client.reader.read_envelope()).await {
                Ok(Ok(Some(env))) => env,
                Ok(Ok(None)) => panic!("connection closed waiting for {context}"),
                Ok(Err(err)) => panic!("read_envelope failed waiting for {context}: {err:?}"),
                Err(_) => panic!("timed out waiting for {context}; skipped frames: {skipped:#?}"),
            };
            client
                .incoming_seq
                .validate(&env.stream, env.seq, env.kind)
                .expect("incoming sequence must be valid");
            env
        } else {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok(Some(env))) => env,
                Ok(Ok(None)) => panic!("connection closed waiting for {context}"),
                Ok(Err(err)) => panic!("next_event failed waiting for {context}: {err:?}"),
                Err(_) => panic!("timed out waiting for {context}; skipped frames: {skipped:#?}"),
            }
        };
        if matches(&env) {
            return env;
        }
        assert!(
            allowed_noise.contains(&env.kind) || is_builtin_team_custom_agent_notify(&env),
            "unexpected {:?} frame on {:?} while waiting for {context}; skipped frames: {skipped:#?}",
            env.kind,
            env.stream
        );
        skipped.push(format!("{:?} on {:?}", env.kind, env.stream));
    }
}

#[allow(dead_code)]
pub async fn next_interesting_frame_on(
    client: &mut client::Connection,
    context: &str,
    mut is_additional_noise: impl FnMut(&Envelope) -> bool,
) -> Envelope {
    next_frame_matching_on(client, context, |env| {
        !is_routine_control_plane_frame(env) && !is_additional_noise(env)
    })
    .await
}

#[allow(dead_code)]
pub async fn assert_no_interesting_frame_on(
    client: &mut client::Connection,
    duration: Duration,
    context: &str,
    mut is_additional_noise: impl FnMut(&Envelope) -> bool,
) {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        match tokio::time::timeout_at(deadline, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env)))
                if is_routine_control_plane_frame(&env) || is_additional_noise(&env) => {}
            Ok(Ok(Some(env))) => panic!(
                "unexpected event before {context}: kind={} stream={}",
                env.kind, env.stream
            ),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        }
    }
}

type ConnectionKey = (StreamPath, usize);
type PendingFrames = HashMap<ConnectionKey, VecDeque<Envelope>>;

fn pending_frames() -> &'static Mutex<PendingFrames> {
    static PENDING: OnceLock<Mutex<PendingFrames>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn connection_key(client: &client::Connection) -> ConnectionKey {
    let mut streams = client
        .outgoing_seq
        .keys()
        .filter(|stream| stream.0.starts_with("/host/"));
    let stream = streams
        .next()
        .cloned()
        .expect("missing host stream for test connection");
    assert!(
        streams.next().is_none(),
        "test connection has multiple host streams"
    );
    let writer = &*client.writer as *const (dyn tokio::io::AsyncWrite + Unpin + Send);
    (stream, writer as *const () as usize)
}

#[allow(dead_code)]
pub fn push_pending_frame_on(client: &client::Connection, env: Envelope) {
    push_pending_frames_on(client, [env]);
}

#[allow(dead_code)]
pub fn push_front_pending_frame_on(client: &client::Connection, env: Envelope) {
    pending_frames()
        .lock()
        .expect("pending frame lock poisoned")
        .entry(connection_key(client))
        .or_default()
        .push_front(env);
}

#[allow(dead_code)]
pub fn push_pending_frames_on(
    client: &client::Connection,
    frames: impl IntoIterator<Item = Envelope>,
) {
    let mut frames = frames.into_iter().collect::<VecDeque<_>>();
    if frames.is_empty() {
        return;
    }
    pending_frames()
        .lock()
        .expect("pending frame lock poisoned")
        .entry(connection_key(client))
        .or_default()
        .append(&mut frames);
}

#[allow(dead_code)]
pub fn pop_pending_frame_on(client: &client::Connection) -> Option<Envelope> {
    pop_pending_frame_matching_on(client, |_| true)
}

#[allow(dead_code)]
pub fn pop_pending_frame_matching_on(
    client: &client::Connection,
    mut matches: impl FnMut(&Envelope) -> bool,
) -> Option<Envelope> {
    let key = connection_key(client);
    let mut pending = pending_frames()
        .lock()
        .expect("pending frame lock poisoned");
    let queue = pending.get_mut(&key)?;
    let index = queue.iter().position(&mut matches)?;
    let env = queue.remove(index);
    if queue.is_empty() {
        pending.remove(&key);
    }
    env
}

#[allow(dead_code)]
pub fn agent_bootstrap_frames(env: &Envelope) -> VecDeque<Envelope> {
    assert_eq!(env.kind, FrameKind::AgentBootstrap);
    let payload: AgentBootstrapPayload = env.parse_payload().expect("parse AgentBootstrapPayload");
    payload
        .events
        .into_iter()
        .filter_map(|event| {
            let result = match event {
                AgentBootstrapEvent::AgentStart(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::AgentStart,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::AgentError(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::AgentError,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::SessionSettings(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::SessionSettings,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::QueuedMessages(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::QueuedMessages,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::AgentActivityStats(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::AgentActivityStats,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::ContextCompaction(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::ContextCompactionNotify,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::ContextCompactionCapability(payload) => {
                    Envelope::from_payload(
                        env.stream.clone(),
                        FrameKind::ContextCompactionCapability,
                        env.seq,
                        &payload,
                    )
                }
                AgentBootstrapEvent::ChatEvent(payload) => Envelope::from_payload(
                    env.stream.clone(),
                    FrameKind::ChatEvent,
                    env.seq,
                    &payload,
                ),
                AgentBootstrapEvent::HasPriorHistory { .. } => return None,
            };
            Some(result.expect("serialize AgentBootstrap event"))
        })
        .collect()
}

#[allow(dead_code)]
pub fn buffer_agent_bootstrap_on(client: &client::Connection, env: &Envelope) -> Option<Envelope> {
    let mut frames = agent_bootstrap_frames(env);
    let first = frames.pop_front();
    push_pending_frames_on(client, frames);
    first
}

#[allow(dead_code)]
pub async fn next_frame_unpacking_agent_bootstrap_on(
    client: &mut client::Connection,
    context: &str,
) -> Envelope {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    loop {
        let env = match tokio::time::timeout_at(deadline, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed waiting for {context}"),
            Ok(Err(err)) => panic!("next_event failed waiting for {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if env.kind != FrameKind::AgentBootstrap {
            return env;
        }
        if let Some(first) = buffer_agent_bootstrap_on(client, &env) {
            return first;
        }
    }
}

#[allow(dead_code)]
pub async fn next_logical_frame_on(client: &mut client::Connection, context: &str) -> Envelope {
    if let Some(env) = pop_pending_frame_on(client) {
        return env;
    }
    next_frame_unpacking_agent_bootstrap_on(client, context).await
}

#[allow(dead_code)]
pub async fn next_logical_frame_matching_on(
    client: &mut client::Connection,
    context: &str,
    mut matches: impl FnMut(&Envelope) -> bool,
) -> Envelope {
    if let Some(env) = pop_pending_frame_matching_on(client, &mut matches) {
        return env;
    }
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    let mut skipped = Vec::new();
    loop {
        let env = match tokio::time::timeout_at(deadline, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed waiting for {context}"),
            Ok(Err(err)) => panic!("next_event failed waiting for {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}; skipped frames: {skipped:#?}"),
        };
        if env.kind == FrameKind::AgentBootstrap {
            push_pending_frames_on(client, agent_bootstrap_frames(&env));
            if let Some(env) = pop_pending_frame_matching_on(client, &mut matches) {
                return env;
            }
            continue;
        }
        if matches(&env) {
            return env;
        }
        if env.kind == FrameKind::CustomAgentNotify {
            let _ = is_builtin_team_custom_agent_notify(&env);
        }
        skipped.push(format!("{:?} on {:?}", env.kind, env.stream));
    }
}

#[allow(dead_code)]
pub async fn next_chat_event_matching_on(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
    mut matches: impl FnMut(&ChatEvent) -> bool,
) -> ChatEvent {
    let mut found = None;
    next_logical_frame_matching_on(client, context, |env| {
        if env.kind != FrameKind::ChatEvent || env.stream != *stream {
            return false;
        }
        let event: ChatEvent = env.parse_payload().expect("parse ChatEvent");
        if matches(&event) {
            found = Some(event);
            true
        } else {
            false
        }
    })
    .await;
    found.expect("matched chat event")
}

#[allow(dead_code)]
pub async fn expect_paused_tool_request_on(
    client: &mut client::Connection,
    stream: &StreamPath,
    tool_name: &str,
) -> ToolRequest {
    let mut request = None;
    let mut paused = false;
    while !(paused && request.is_some()) {
        let context = format!("paused {tool_name} tool request");
        let event = next_chat_event_matching_on(client, stream, &context, |_| true).await;
        match event {
            ChatEvent::ToolRequest(r) => {
                assert_eq!(
                    tool_request_name(&r),
                    tool_name,
                    "unexpected tool request while waiting for {tool_name}"
                );
                request = Some(r);
            }
            ChatEvent::TypingStatusChanged(false) => paused = true,
            _ => {}
        }
    }
    request.expect("tool request present when loop exits")
}

#[allow(dead_code)]
pub fn tool_request_name(request: &ToolRequest) -> &str {
    match &request.tool_type {
        protocol::ToolRequestType::ModifyFile { .. } => "modify_file",
        protocol::ToolRequestType::RunCommand { .. } => "Bash",
        protocol::ToolRequestType::ReadFiles { .. } => "read_files",
        protocol::ToolRequestType::SearchTypes { .. } => "search_types",
        protocol::ToolRequestType::GetTypeDocs { .. } => "get_type_docs",
        protocol::ToolRequestType::AskUserQuestion { .. } => "AskUserQuestion",
        protocol::ToolRequestType::ExitPlanMode { .. } => "ExitPlanMode",
        protocol::ToolRequestType::AgentSpawn { .. } => "Task",
        protocol::ToolRequestType::GenerateImage { .. } => "generate_image",
        protocol::ToolRequestType::WebSearch { .. } => "web_search",
        protocol::ToolRequestType::ViewImage { .. } => "view_image",
        protocol::ToolRequestType::Sleep { .. } => "sleep",
        protocol::ToolRequestType::TydeSendAgentMessage { .. } => "tyde_send_agent_message",
        protocol::ToolRequestType::TydeAwaitAgents { .. } => "tyde_await_agents",
        protocol::ToolRequestType::Other { args } => args
            .get("name")
            .or_else(|| args.get("tool_name"))
            .or_else(|| args.get("tool"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("other"),
    }
}

#[allow(dead_code)]
pub fn tool_completion_succeeded(completion: &ToolExecutionCompletedData) -> bool {
    matches!(
        completion.outcome,
        protocol::ToolExecutionOutcome::Succeeded { .. }
    )
}

#[allow(dead_code)]
pub fn tool_completion_failed(completion: &ToolExecutionCompletedData) -> bool {
    matches!(
        completion.outcome,
        protocol::ToolExecutionOutcome::Failed { .. }
            | protocol::ToolExecutionOutcome::Cancelled { .. }
    )
}

/// Wait for a `QueuedMessages` snapshot with exactly `count` entries on
/// `stream`, unwrapping `AgentBootstrap` replays.
///
/// `stream` must be an **instance** stream that *this* connection actually
/// receives. A fresh or reconnected client gets the agent's replay on a new
/// instance stream (`/agent/<agent_id>/<new_instance_id>`), so passing the
/// spawning connection's `agent.stream` to a second connection silently skips
/// the whole replay until the wait times out. Use
/// [`expect_agent_queued_messages_on`] for those waits.
#[allow(dead_code)]
pub async fn expect_queued_messages_on(
    client: &mut client::Connection,
    stream: &StreamPath,
    count: usize,
) -> QueuedMessagesPayload {
    let context = format!("QueuedMessages with {count} entries");
    let mut found = None;
    next_logical_frame_matching_on(client, &context, |env| {
        if env.stream != *stream {
            return false;
        }
        match env.kind {
            FrameKind::QueuedMessages => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("parse QueuedMessagesPayload");
                if payload.messages.len() == count {
                    found = Some(payload);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    })
    .await;
    found.expect("matched queued messages snapshot")
}

/// Agent-scoped [`expect_queued_messages_on`]: matches a `QueuedMessages`
/// snapshot with `count` entries on **any** instance stream of `agent_id`
/// (`/agent/<agent_id>/…`), so it works for a fresh subscriber whose replay
/// arrives on an instance stream the spawning connection never saw.
#[allow(dead_code)]
pub async fn expect_agent_queued_messages_on(
    client: &mut client::Connection,
    agent_id: &AgentId,
    count: usize,
) -> QueuedMessagesPayload {
    let prefix = format!("/agent/{}/", agent_id.0);
    let context = format!("QueuedMessages with {count} entries on {prefix}*");
    let mut found = None;
    next_logical_frame_matching_on(client, &context, |env| {
        if !env.stream.0.starts_with(&prefix) {
            return false;
        }
        match env.kind {
            FrameKind::QueuedMessages => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("parse QueuedMessagesPayload");
                if payload.messages.len() == count {
                    found = Some(payload);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    })
    .await;
    found.expect("matched queued messages snapshot")
}

#[allow(dead_code)]
pub async fn finish_turn_on(client: &mut client::Connection, stream: &StreamPath) -> Turn {
    let mut frames = Vec::new();
    let mut saw_busy = false;
    loop {
        let env =
            next_logical_frame_matching_on(client, "next turn frame", |env| env.stream == *stream)
                .await;
        let typing = (env.kind == FrameKind::ChatEvent)
            .then(|| env.parse_payload::<ChatEvent>().expect("parse ChatEvent"))
            .and_then(|event| match event {
                ChatEvent::TypingStatusChanged(active) => Some(active),
                _ => None,
            });
        frames.push(env);
        match typing {
            Some(true) => saw_busy = true,
            Some(false) if saw_busy => return Turn { frames },
            _ => {}
        }
    }
}

impl Fixture {
    /// Spawn a mock-backend agent with the fixture's default parameters
    /// (`/tmp/test` workspace, Claude, default access mode) and wait for it to
    /// start. Use [`Fixture::spawn_with`] when the test cares about the spawn
    /// parameters or about the `AgentStart` payload.
    #[allow(dead_code)]
    pub async fn spawn(&mut self, name: &str, prompt: &str) -> TestAgent {
        self.spawn_with(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/test".to_owned()],
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
        .0
    }

    /// Spawn an agent with a script installed before its launch turn starts.
    #[allow(dead_code)]
    pub async fn spawn_scripted(
        &mut self,
        name: &str,
        script: server::backend::mock::MockScript,
    ) -> TestAgent {
        let reservation = self.host.reserve_next_mock_launch(name, script).await;
        let agent = self.spawn(name, "scripted launch").await;
        drop(reservation);
        agent
    }

    /// Reserve the host's next mock launch directly. Prefer
    /// [`Fixture::spawn_scripted`]; this exists so tests of the reservation
    /// surface itself can exercise a name mismatch, and for agents the
    /// server spawns on the test's behalf.
    #[allow(dead_code)]
    pub async fn reserve_next_mock_launch(
        &self,
        name: &str,
        script: server::backend::mock::MockScript,
    ) -> server::MockLaunchReservation {
        self.host.reserve_next_mock_launch(name, script).await
    }

    /// Reserve the next mock launch for `name` to fail with `message`.
    #[allow(dead_code)]
    pub async fn reserve_next_mock_spawn_failure(
        &self,
        name: &str,
        message: &str,
    ) -> server::MockLaunchReservation {
        self.host
            .reserve_next_mock_spawn_failure(name, message)
            .await
    }

    /// Reserve a resume whose event stream closes before replay completes.
    #[allow(dead_code)]
    pub async fn reserve_next_mock_resume_closing_before_barrier(
        &self,
        name: &str,
    ) -> server::MockLaunchReservation {
        self.host
            .reserve_next_mock_resume_closing_before_barrier(name)
            .await
    }

    /// The live mock-backend control handle for `agent`, retrieved through
    /// the agent actor. Panics if the agent is not running a mock backend.
    #[allow(dead_code)]
    pub async fn mock(&self, agent: &TestAgent) -> server::backend::mock::MockControl {
        self.mock_by_id(&agent.new_agent.agent_id).await
    }

    /// Resolve a live mock backend by agent id.
    #[allow(dead_code)]
    pub async fn mock_by_id(&self, agent_id: &AgentId) -> server::backend::mock::MockControl {
        self.host
            .mock_control(agent_id)
            .await
            .expect("agent has no live mock backend to control")
    }

    /// [`Fixture::spawn`] with explicit spawn parameters, returning the parsed
    /// `AgentStart` payload alongside the agent. Waiting is identical: an
    /// `AgentError`, `CommandError` or `AgentClosed` seen before `NewAgent` or
    /// before the agent's `AgentStart` fails the test.
    #[allow(dead_code)]
    pub async fn spawn_with(
        &mut self,
        payload: SpawnAgentPayload,
    ) -> (TestAgent, AgentStartPayload) {
        self.client
            .spawn_agent(payload)
            .await
            .expect("spawn_agent failed");
        let env = next_logical_frame_matching_on(&mut self.client, "NewAgent", |env| {
            assert!(
                !matches!(
                    env.kind,
                    FrameKind::AgentError | FrameKind::CommandError | FrameKind::AgentClosed
                ),
                "error frame while waiting for NewAgent: {:?} on {:?}",
                env.kind,
                env.stream
            );
            env.kind == FrameKind::NewAgent
        })
        .await;
        let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
        let stream = new_agent.instance_stream.clone();
        let mut start = None;
        next_logical_frame_matching_on(&mut self.client, "AgentStart", |env| {
            assert!(
                !matches!(
                    env.kind,
                    FrameKind::AgentError | FrameKind::CommandError | FrameKind::AgentClosed
                ),
                "error frame while waiting for AgentStart: {:?} on {:?}",
                env.kind,
                env.stream
            );
            if env.stream != stream {
                return false;
            }
            if env.kind == FrameKind::AgentStart {
                start = Some(env.parse_payload().expect("parse AgentStartPayload"));
                true
            } else {
                false
            }
        })
        .await;
        (
            TestAgent { stream, new_agent },
            start.expect("matched AgentStart payload"),
        )
    }

    #[allow(dead_code)]
    pub async fn next_frame_matching(
        &mut self,
        context: &str,
        matches: impl FnMut(&Envelope) -> bool,
    ) -> Envelope {
        next_frame_matching_on(&mut self.client, context, matches).await
    }

    #[allow(dead_code)]
    pub async fn next_chat_event_matching(
        &mut self,
        agent: &TestAgent,
        context: &str,
        matches: impl FnMut(&ChatEvent) -> bool,
    ) -> ChatEvent {
        next_chat_event_matching_on(&mut self.client, &agent.stream, context, matches).await
    }

    #[allow(dead_code)]
    pub async fn expect_paused_tool_request(
        &mut self,
        agent: &TestAgent,
        tool_name: &str,
    ) -> ToolRequest {
        expect_paused_tool_request_on(&mut self.client, &agent.stream, tool_name).await
    }

    #[allow(dead_code)]
    pub async fn expect_queued_messages(
        &mut self,
        agent: &TestAgent,
        count: usize,
    ) -> QueuedMessagesPayload {
        expect_queued_messages_on(&mut self.client, &agent.stream, count).await
    }

    #[allow(dead_code)]
    pub async fn approve_exit_plan_mode(&mut self, agent: &TestAgent, request: &ToolRequest) {
        self.client
            .send_message_payload(
                &agent.stream,
                SendMessagePayload {
                    message: String::new(),
                    images: None,
                    origin: None,
                    tool_response: Some(SendMessageToolResponse::ExitPlanMode {
                        tool_call_id: request.tool_call_id.clone(),
                        decision: ExitPlanModeDecision::Approve,
                        feedback: None,
                    }),
                },
            )
            .await
            .expect("send ExitPlanMode approval");
    }

    #[allow(dead_code)]
    pub async fn finish_turn(&mut self, agent: &TestAgent) -> Turn {
        finish_turn_on(&mut self.client, &agent.stream).await
    }
}

/// Connect a client to a host the test spawned itself (rather than one owned
/// by a [`Fixture`]) and consume its `HostBootstrap` frame.
#[allow(dead_code)]
pub async fn connect_host(host: server::HostHandle) -> (client::Connection, HostBootstrapPayload) {
    connect_client_with_bootstrap(host).await
}

async fn connect_client(host: server::HostHandle) -> client::Connection {
    connect_client_with_bootstrap(host).await.0
}

async fn connect_client_with_bootstrap(
    host: server::HostHandle,
) -> (client::Connection, HostBootstrapPayload) {
    let mut client = connect_raw_client(host).await;

    let env = {
        let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
        let next_event = client.next_event();
        tokio::pin!(next_event);
        loop {
            tokio::select! {
                biased;
                result = &mut next_event => match result {
                    Ok(Some(env)) => break env,
                    Ok(None) => panic!("connection closed before initial host bootstrap"),
                    Err(err) => panic!("initial host bootstrap read failed: {err:?}"),
                },
                _ = tokio::task::yield_now() => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for initial host bootstrap"
                    );
                }
            }
        }
    };
    assert_eq!(
        env.kind,
        FrameKind::HostBootstrap,
        "first host event on connect must be HostBootstrap"
    );
    let bootstrap: HostBootstrapPayload = env.parse_payload().expect("parse HostBootstrapPayload");

    (client, bootstrap)
}

/// Connect as a paired mobile device over the real mobile connection path,
/// which registers the host stream with `AgentReplayMode::Lazy`: no agent
/// stream is attached until the client sends `LoadAgent`. Returns the client
/// with its `HostBootstrap` already parsed.
#[allow(dead_code)]
pub async fn connect_mobile_client_with_bootstrap(
    host: server::HostHandle,
    device_id: &str,
) -> (client::Connection, HostBootstrapPayload) {
    let mut client = connect_raw_mobile_client(host, device_id).await;
    let env = next_frame_matching_on(&mut client, "mobile HostBootstrap", |env| {
        env.kind == FrameKind::HostBootstrap
    })
    .await;
    let bootstrap: HostBootstrapPayload = env
        .parse_payload()
        .expect("parse mobile HostBootstrapPayload");
    (client, bootstrap)
}

#[allow(dead_code)]
pub async fn connect_raw_mobile_client(
    host: server::HostHandle,
    device_id: &str,
) -> client::Connection {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    let client_config = client::ClientConfig::current();
    let device_id = protocol::MobileDeviceId(device_id.to_owned());

    tokio::spawn(async move {
        let conn = server::accept(&server_config, server_stream)
            .await
            .expect("mobile handshake failed");
        if let Err(err) = server::run_mobile_connection(conn, host, device_id).await {
            eprintln!("mobile connection loop failed: {err:?}");
        }
    });

    client::connect(&client_config, client_stream)
        .await
        .expect("mobile client handshake failed")
}

/// Send `LoadAgent` on `agent_stream` — the lazy client's request to attach
/// an agent's instance stream — without waiting for the reply.
#[allow(dead_code)]
pub async fn send_load_agent_on(client: &mut client::Connection, agent_stream: &StreamPath) {
    let seq = client
        .outgoing_seq
        .get(agent_stream)
        .copied()
        .unwrap_or_else(|| panic!("no outgoing sequence for agent stream {agent_stream}"));
    let envelope = Envelope::from_payload(
        agent_stream.clone(),
        FrameKind::LoadAgent,
        seq,
        &protocol::LoadAgentPayload {},
    )
    .expect("serialize LoadAgent");
    client.outgoing_seq.insert(agent_stream.clone(), seq + 1);
    protocol::write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write LoadAgent");
}

async fn connect_raw_client(host: server::HostHandle) -> client::Connection {
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

    client::connect(&client_config, client_stream)
        .await
        .expect("client handshake failed")
}
