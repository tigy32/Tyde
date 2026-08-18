//! Harness for `conformance.rs`: boot a host, drive a real provider over the
//! real protocol, hand back the [`Turn`]s it produced.
//!
//! Helpers here return data for the test to judge. They fail only when they
//! cannot produce what was asked for, never on a backend contract — that split
//! is what keeps `conformance.rs` readable as a list of guarantees.

// Suppressed rather than fixed: Cargo compiles this file a second time as a
// test binary with no tests in it, where every item is unreachable.
#![allow(dead_code)]

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::FutureExt;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentCompactPayload, AgentErrorPayload, AgentId,
    AgentStartPayload, AskUserQuestion, BackendKind, ChatEvent, ClientErrorPayload,
    ContextCompactionNotifyPayload, ContextCompactionTimelineEvent, Envelope,
    FetchSessionHistoryPayload, FrameKind, HistoryPageRequestId, ListSessionsPayload,
    MessageSender, NewAgentPayload, QueuedMessagesPayload, SendMessagePayload,
    SendMessageToolResponse, SessionHistoryPayload, SessionId, SessionListPayload,
    SessionSettingValue, SessionSettingsValues, SessionSummary, SpawnAgentParams,
    SpawnAgentPayload, SpawnCostHint, StreamPath, ToolExecutionCompletedData, ToolExecutionOutcome,
    ToolExecutionResult, ToolRequest,
};
use serde_json::json;
use tyde_agent_adapter::BackendCapability;
use uuid::Uuid;

/// Control-plane replies do not wait on a model. Kept named because three call
/// sites share it; the per-turn waits are inline at their one use each.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// Seeded here and asserted in `conformance.rs`; shared so the two cannot drift.
pub const SCRATCH_DIR: &str = "scratch";

/// Every model name the provider may report for the pin this suite configures.
/// The first entry is the value fed to the backend config; the rest are aliases
/// the provider may report instead — Claude is configured as `haiku` and reports
/// its full id. Empty means this backend is not pinned.
///
/// A run that quietly escalates to an expensive model is a bill, not a result,
/// so the value that configures the host and the value the assertion expects
/// have to come from the same place.
pub fn pinned_models(backend: BackendKind) -> Vec<String> {
    match backend {
        BackendKind::Claude => vec!["haiku".to_owned(), "claude-haiku-4-5-20251001".to_owned()],
        // `codex.rs` documents this variable: pinning a different model without
        // a rebuild is how you tell model-specific drift from a real defect.
        BackendKind::Codex => vec![env_or("TYDE_CODEX_TEST_MODEL", "gpt-5.3-codex-spark")],
        _ => Vec::new(),
    }
}

/// Hermes takes its model as a per-spawn session setting, not a complexity-tier
/// config, and the value must be the *exact* string the schema publishes as a
/// select option — `validate_session_setting` compares `Select` values by
/// equality. Hermes encodes those options as JSON (`encode_model_option_value`,
/// `hermes.rs:5893`), so the older `"<model> --provider <provider>"` spelling
/// that `backend.rs:22322` still builds is rejected at startup, before its own
/// legacy parser ever sees it.
fn hermes_session_settings() -> SessionSettingsValues {
    let provider = env_or("TYDE_HERMES_TEST_PROVIDER", "openrouter");
    let model = env_or("TYDE_HERMES_TEST_MODEL", "minimax/minimax-m3");
    let mut values = SessionSettingsValues::default();
    values.0.insert(
        "model".to_owned(),
        SessionSettingValue::String(json!({"model": model, "provider": provider}).to_string()),
    );
    values.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String("none".to_owned()),
    );
    values
}

/// Selection only. Whether a backend can actually run is the server's question,
/// and it answers it authoritatively when the spawn fails — a check here would
/// be a second, divergent copy of six different installation rules.
fn enabled_backends() -> Vec<BackendKind> {
    assert_eq!(
        std::env::var("TYDE_RUN_REAL_AI_TESTS").ok().as_deref(),
        Some("1"),
        "set TYDE_RUN_REAL_AI_TESTS=1 to authorize the paid conformance suite"
    );
    match std::env::var("TYDE_REAL_BACKENDS") {
        // Antigravity is excluded from real-backend conformance, matching
        // `backend.rs`. `BackendKind` has no enumeration to derive this from —
        // the server hand-writes the same list in `backend/mod.rs:1250` and
        // `host.rs:19689`.
        Err(_) => vec![
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Acp,
            BackendKind::Hermes,
            BackendKind::Tycode,
        ],
        Ok(configured) => {
            let mut selected = Vec::new();
            for value in configured
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                let backend = match value.to_ascii_lowercase().as_str() {
                    "claude" => BackendKind::Claude,
                    "codex" => BackendKind::Codex,
                    "kiro" | "acp" => BackendKind::Acp,
                    "hermes" => BackendKind::Hermes,
                    "tycode" => BackendKind::Tycode,
                    other => panic!("unknown backend {other:?} in TYDE_REAL_BACKENDS"),
                };
                if !selected.contains(&backend) {
                    selected.push(backend);
                }
            }
            assert!(!selected.is_empty(), "TYDE_REAL_BACKENDS selected nothing");
            selected
        }
    }
}

/// One prompt and everything the client received in response to it.
///
/// Carries the prompt so a failure names the turn that broke by what it asked
/// for. An index into a conversation would not survive reordering, and would
/// tell the reader nothing.
pub struct Turn {
    backend: BackendKind,
    prompt: String,
    events: Vec<ChatEvent>,
}

impl Turn {
    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    /// Prefix for this turn's assertion failures. Not an identity — it names the
    /// turn by what it asked for, which an index into the conversation could not.
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{} turn {prompt:?}", backend_label(self.backend))
    }

    pub fn tool_requests(&self) -> impl Iterator<Item = &ToolRequest> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolRequest(request) => Some(request),
            _ => None,
        })
    }

    pub fn tool_completions(&self) -> impl Iterator<Item = &ToolExecutionCompletedData> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolExecutionCompleted(completion) => Some(completion),
            _ => None,
        })
    }

    /// Failure-message material. Nothing asserts on it.
    pub fn tool_request_names(&self) -> Vec<String> {
        self.tool_requests()
            .map(|request| format!("{}({})", tool_kind(request), request.tool_call_id))
            .collect()
    }

    /// Failure-message material. Nothing asserts on it — the outcome is
    /// summarised rather than `Debug`-printed because the full result payload
    /// buries the one thing that matters, which tool produced what.
    pub fn completion_summaries(&self) -> Vec<String> {
        self.tool_completions()
            .map(|completion| {
                let outcome = match &completion.outcome {
                    ToolExecutionOutcome::Succeeded { result } => {
                        format!("ok:{}", result_kind(result))
                    }
                    ToolExecutionOutcome::Failed { message, .. } => format!("failed:{message}"),
                    ToolExecutionOutcome::Cancelled { message } => format!("cancelled:{message}"),
                };
                format!("{}=>{outcome}", completion.tool_call_id)
            })
            .collect()
    }

    /// The text the user ends up looking at. Falls back to accumulated deltas
    /// for backends whose `StreamEnd` omits the assembled content.
    pub fn final_text(&self) -> String {
        let mut streamed = String::new();
        let mut last_final = String::new();
        for event in &self.events {
            match event {
                ChatEvent::StreamStart(_) => streamed.clear(),
                ChatEvent::StreamDelta(delta) => streamed.push_str(&delta.text),
                ChatEvent::StreamEnd(end) => {
                    let content = if end.message.content.trim().is_empty() {
                        streamed.trim().to_owned()
                    } else {
                        end.message.content.clone()
                    };
                    if !content.trim().is_empty() {
                        last_final = content;
                    }
                }
                _ => {}
            }
        }
        last_final
    }
}

pub fn tool_kind(request: &ToolRequest) -> &'static str {
    use protocol::ToolRequestType as T;
    match request.tool_type {
        T::ModifyFile { .. } => "modify_file",
        T::RunCommand { .. } => "run_command",
        T::ReadFiles { .. } => "read_files",
        T::SearchTypes { .. } => "search_types",
        T::GetTypeDocs { .. } => "get_type_docs",
        T::AskUserQuestion { .. } => "ask_user_question",
        T::ExitPlanMode { .. } => "exit_plan_mode",
        T::AgentSpawn { .. } => "agent_spawn",
        T::GenerateImage { .. } => "generate_image",
        T::WebSearch { .. } => "web_search",
        T::ViewImage { .. } => "view_image",
        T::Sleep { .. } => "sleep",
        T::TydeSendAgentMessage { .. } => "tyde_send_agent_message",
        T::TydeAwaitAgents { .. } => "tyde_await_agents",
        T::Other { .. } => "other",
    }
}

fn result_kind(result: &ToolExecutionResult) -> &'static str {
    use ToolExecutionResult as R;
    match result {
        R::ModifyFile { .. } => "modify_file",
        R::RunCommand { .. } => "run_command",
        R::ReadFiles { .. } => "read_files",
        R::SearchTypes { .. } => "search_types",
        R::GetTypeDocs { .. } => "get_type_docs",
        R::TydeSendAgentMessage => "tyde_send_agent_message",
        R::TydeAwaitAgents { .. } => "tyde_await_agents",
        R::GenerateImage { .. } => "generate_image",
        R::WebSearch => "web_search",
        R::ViewImage => "view_image",
        R::Sleep => "sleep",
        R::Other { .. } => "other",
    }
}

pub struct Host {
    client: client::Connection,
    handle: server::HostHandle,
    backend_kind: BackendKind,
    workspace: PathBuf,
}

impl Host {
    async fn new(backend_kind: BackendKind, store: &Path, workspace: &Path) -> Self {
        std::fs::write(workspace.join("README.txt"), "tyde conformance workspace")
            .expect("seed workspace");
        // Something for the destructive-command turn to remove. A directory
        // rather than a file because the providers that gate risky commands
        // gate on *recursion* — a plain `rm <file>` passes every such check, so
        // a scenario built on one asserts nothing about the gate.
        std::fs::create_dir_all(workspace.join(SCRATCH_DIR)).expect("seed scratch directory");
        std::fs::write(workspace.join(SCRATCH_DIR).join("notes.txt"), "scratch")
            .expect("seed scratch file");

        let settings_path = store.join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&host_settings(backend_kind)).expect("serialize settings"),
        )
        .expect("seed settings store");

        let handle = server::spawn_host_with_store_paths(
            store.join("sessions.json"),
            store.join("projects.json"),
            settings_path,
        )
        .expect("initialize host with real backends");

        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_config = server::ServerConfig::current();
        let client_config = client::ClientConfig::current();
        let connection_host = handle.clone();
        tokio::spawn(async move {
            let conn = server::accept(&server_config, server_stream)
                .await
                .expect("server handshake failed");
            if let Err(err) = server::run_connection(conn, connection_host).await {
                eprintln!("server connection loop failed: {err:?}");
            }
        });
        let client = client::connect(&client_config, client_stream)
            .await
            .expect("client handshake failed");

        Self {
            client,
            handle,
            backend_kind,
            workspace: workspace.to_path_buf(),
        }
    }

    pub fn backend(&self) -> BackendKind {
        self.backend_kind
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn workspace_roots(&self) -> Vec<String> {
        vec![self.workspace.to_string_lossy().into_owned()]
    }

    async fn next_envelope(&mut self, timeout: Duration, context: &str) -> Envelope {
        let backend_kind = self.backend_kind;
        match tokio::time::timeout(timeout, self.client.next_event()).await {
            Ok(Ok(Some(envelope))) => envelope,
            Ok(Ok(None)) => panic!("connection closed while waiting for {context}"),
            Ok(Err(error)) => panic!("next_event failed waiting for {context}: {error:?}"),
            Err(_) => panic!(
                "{backend_kind:?} timed out after {}s waiting for {context}",
                timeout.as_secs()
            ),
        }
    }
}

pub struct Agent {
    agent_id: AgentId,
    stream: StreamPath,
    /// What the server replayed in `AgentBootstrap` before the first live event:
    /// empty for a fresh spawn, the restored conversation for a resume.
    pub replayed_history: Vec<ChatEvent>,
}

pub async fn spawn_agent(host: &mut Host, prompt: &str) -> Agent {
    let backend_kind = host.backend_kind;
    let workspace_roots = host.workspace_roots();
    host.client
        .spawn_agent(SpawnAgentPayload {
            name: Some("conformance".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots,
                prompt: prompt.to_owned(),
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: Some(SpawnCostHint::Low),
                access_mode: Default::default(),
                session_settings: (backend_kind == BackendKind::Hermes)
                    .then(hermes_session_settings),
            },
        })
        .await
        .expect("spawn_agent failed");
    await_agent_start(host, "spawn").await
}

pub async fn resume_agent(host: &mut Host, session_id: &SessionId) -> Agent {
    host.client
        .spawn_agent(SpawnAgentPayload {
            name: Some("conformance-resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume spawn_agent failed");
    await_agent_start(host, "resume").await
}

async fn await_agent_start(host: &mut Host, context: &str) -> Agent {
    let backend_kind = host.backend_kind;
    let mut new_agent: Option<NewAgentPayload> = None;
    let mut replayed_history = Vec::new();

    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, context).await;
        fail_on_agent_error(&envelope, context);

        if envelope.kind == FrameKind::NewAgent {
            let payload: NewAgentPayload = envelope.parse_payload().expect("parse NewAgent");
            assert_eq!(
                payload.backend_kind, backend_kind,
                "{context}: server started the wrong backend"
            );
            new_agent = Some(payload);
            continue;
        }

        let Some(agent) = new_agent.as_ref() else {
            continue;
        };
        if envelope.stream != agent.instance_stream {
            continue;
        }

        let started = match envelope.kind {
            FrameKind::AgentStart => {
                let _: AgentStartPayload = envelope.parse_payload().expect("parse AgentStart");
                true
            }
            FrameKind::AgentBootstrap => {
                let payload: AgentBootstrapPayload =
                    envelope.parse_payload().expect("parse AgentBootstrap");
                let mut started = false;
                for event in payload.events {
                    match event {
                        AgentBootstrapEvent::ChatEvent(event) => replayed_history.push(event),
                        AgentBootstrapEvent::AgentStart(_) => started = true,
                        AgentBootstrapEvent::AgentError(error) => {
                            panic!("{context}: agent failed to start: {}", error.message)
                        }
                        _ => {}
                    }
                }
                started
            }
            _ => false,
        };

        if started {
            return Agent {
                agent_id: agent.agent_id.clone(),
                stream: agent.instance_stream.clone(),
                replayed_history,
            };
        }
    }
}

/// Send without collecting, so the caller can do something to an agent that is
/// still mid-turn.
pub async fn send_prompt(host: &mut Host, agent: &Agent, prompt: &str) {
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");
}

pub async fn ask(host: &mut Host, agent: &Agent, prompt: impl AsRef<str>) -> Turn {
    let prompt = prompt.as_ref();
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");
    collect_turn(host, agent, prompt).await
}

/// Every chat event from the user echo through the turn going idle. Going idle
/// with no assistant response at all is a failure, not a turn.
pub async fn collect_turn(host: &mut Host, agent: &Agent, prompt: &str) -> Turn {
    let backend = host.backend_kind;
    let label = backend_label(backend);
    let context = format!("{label} turn for prompt {prompt:?}");
    let mut events = Vec::new();
    let mut saw_echo = false;
    let mut saw_stream_end = false;

    loop {
        // Sized for several model round trips plus real tool execution.
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }

        for event in chat_events_in(&envelope) {
            eprintln!("{label} {event:?}");

            if let ChatEvent::MessageAdded(message) = &event
                && matches!(message.sender, MessageSender::User)
                && message.content.contains(prompt)
            {
                saw_echo = true;
            }
            if !saw_echo {
                continue;
            }

            let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
            if matches!(event, ChatEvent::StreamEnd(_)) {
                saw_stream_end = true;
            }
            events.push(event);

            if idle {
                assert!(
                    saw_stream_end,
                    "{context}: backend went idle without producing any assistant response"
                );
                return Turn {
                    backend,
                    prompt: prompt.to_owned(),
                    events,
                };
            }
        }
    }
}

fn chat_events_in(envelope: &Envelope) -> Vec<ChatEvent> {
    match envelope.kind {
        FrameKind::ChatEvent => vec![envelope.parse_payload().expect("parse ChatEvent")],
        FrameKind::AgentBootstrap => {
            let payload: AgentBootstrapPayload =
                envelope.parse_payload().expect("parse AgentBootstrap");
            payload
                .events
                .into_iter()
                .filter_map(|event| match event {
                    AgentBootstrapEvent::ChatEvent(event) => Some(event),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn fail_on_agent_error(envelope: &Envelope, context: &str) {
    if envelope.kind == FrameKind::AgentError {
        let error: AgentErrorPayload = envelope.parse_payload().expect("parse AgentError");
        panic!(
            "{context}: backend reported an agent error: {}",
            error.message
        );
    }
}

/// Collect over `window` without requiring a turn boundary. A background task
/// reports its terminal state on its own schedule, long after the turn that
/// started it closed, so no turn-shaped collector can observe it.
pub async fn drain_events_for(host: &mut Host, window: Duration) -> Vec<ChatEvent> {
    let deadline = tokio::time::Instant::now() + window;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, host.client.next_event()).await {
            Ok(Ok(Some(envelope))) => {
                fail_on_agent_error(&envelope, "settle");
                events.extend(chat_events_in(&envelope));
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => panic!("settle next_event failed: {error:?}"),
            Err(_) => break,
        }
    }
    events
}

/// Returns what arrives during shutdown, which is the emitter's last chance to
/// report violations it recorded after the final turn ended.
pub async fn close_agent(host: &mut Host, agent: &Agent) -> Vec<ChatEvent> {
    host.client
        .close_agent(&agent.stream)
        .await
        .expect("close_agent failed");
    drain_events_for(host, Duration::from_secs(2)).await
}

/// A question the backend asked, plus everything the client saw for a while
/// afterwards while deliberately not answering it.
pub struct Question {
    backend: BackendKind,
    prompt: String,
    request: ToolRequest,
    question: AskUserQuestion,
    events: Vec<ChatEvent>,
}

impl Question {
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{} question {prompt:?}", backend_label(self.backend))
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    pub fn tool_call_id(&self) -> &str {
        &self.request.tool_call_id
    }

    pub fn question(&self) -> &AskUserQuestion {
        &self.question
    }

    /// Chosen from what the provider actually offered rather than from the
    /// prompt: answering with a label the backend did not send would test the
    /// test, not the tool.
    pub fn first_option(&self) -> Option<&str> {
        self.question
            .options
            .first()
            .map(|option| option.label.as_str())
    }

    pub fn completions(&self) -> impl Iterator<Item = &ToolExecutionCompletedData> {
        let tool_call_id = self.request.tool_call_id.clone();
        self.events.iter().filter_map(move |event| match event {
            ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == tool_call_id =>
            {
                Some(completion)
            }
            _ => None,
        })
    }
}

/// How long the client sits quiet with an unanswered question on screen.
///
/// An interactive tool is the one kind that is *supposed* to outlive the turn
/// that asked it, so this window is the only place where a backend that
/// terminalizes the card behind the user's back becomes observable.
const QUESTION_SETTLE: Duration = Duration::from_secs(10);

/// Ask something that should make the backend put a question to the user, and
/// return once it has — then keep listening without answering.
pub async fn ask_question(host: &mut Host, agent: &Agent, prompt: &str) -> Question {
    let backend = host.backend_kind;
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");

    let context = format!("{} question for prompt {prompt:?}", backend_label(backend));
    let mut events = Vec::new();
    let mut asked = None;
    while asked.is_none() {
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }
        for event in chat_events_in(&envelope) {
            if let ChatEvent::ToolRequest(request) = &event
                && let protocol::ToolRequestType::AskUserQuestion { questions } = &request.tool_type
                && let Some(question) = questions.first()
            {
                asked = Some((request.clone(), question.clone()));
            }
            events.push(event);
        }
    }
    let (request, question) = asked.expect("loop exits only once a question was seen");
    events.extend(drain_events_for(host, QUESTION_SETTLE).await);

    Question {
        backend,
        prompt: prompt.to_owned(),
        request,
        question,
        events,
    }
}

/// Answer through the typed tool-response path the UI uses, not as chat text.
pub async fn answer_question(
    host: &mut Host,
    agent: &Agent,
    question: &Question,
    answer: &str,
) -> Turn {
    host.client
        .send_message_payload(
            &agent.stream,
            SendMessagePayload {
                message: answer.to_owned(),
                images: None,
                origin: None,
                tool_response: Some(SendMessageToolResponse::AskUserQuestion {
                    tool_call_id: question.tool_call_id().to_owned(),
                    answer: answer.to_owned(),
                }),
            },
        )
        .await
        .expect("answer send_message failed");
    collect_until_idle(host, agent, &format!("answer {answer:?}")).await
}

/// Collect to the next idle without waiting for a user echo. A tool response is
/// not a chat message, so it never produces one.
pub async fn collect_until_idle(host: &mut Host, agent: &Agent, label: &str) -> Turn {
    let backend = host.backend_kind;
    let context = format!("{} {label}", backend_label(backend));
    let mut events = Vec::new();
    loop {
        let envelope = host.next_envelope(Duration::from_secs(240), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.stream != agent.stream {
            continue;
        }
        for event in chat_events_in(&envelope) {
            let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
            events.push(event);
            if idle {
                return Turn {
                    backend,
                    prompt: label.to_owned(),
                    events,
                };
            }
        }
    }
}

pub async fn cancel_turn(host: &mut Host, agent: &Agent) -> Vec<ChatEvent> {
    host.client
        .interrupt(&agent.stream)
        .await
        .expect("interrupt failed");
    drain_events_for(host, Duration::from_secs(10)).await
}

/// Send an ordinary message and fail fast if the server parks it in the queue.
///
/// A wedged agent queues instead of refusing, and the queue is silent: without
/// watching for the snapshot frame this reads as a turn that never arrives, and
/// fails minutes later pointing at the wrong thing.
pub async fn ask_expecting_delivery(host: &mut Host, agent: &Agent, prompt: &str) -> Turn {
    let backend = host.backend_kind;
    host.client
        .send_message(&agent.stream, prompt.to_owned())
        .await
        .expect("send_message failed");

    let context = format!("{} delivery of {prompt:?}", backend_label(backend));
    loop {
        let envelope = host.next_envelope(Duration::from_secs(60), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.kind == FrameKind::QueuedMessages {
            let queued: QueuedMessagesPayload =
                envelope.parse_payload().expect("parse QueuedMessages");
            assert!(
                !queued
                    .messages
                    .iter()
                    .any(|entry| entry.message.contains(prompt)),
                "{context}: the agent queued this message instead of running it, so it believes a \
                 turn is still open. The chat accepts input and never answers."
            );
            continue;
        }
        if envelope.stream != agent.stream {
            continue;
        }
        if chat_events_in(&envelope).iter().any(|event| {
            matches!(event, ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::User)
                    && message.content.contains(prompt))
        }) {
            break;
        }
    }
    collect_until_idle(host, agent, &format!("turn for {prompt:?}")).await
}

/// One compaction and every chat event the client saw while it ran.
pub struct Compaction {
    backend: BackendKind,
    events: Vec<ChatEvent>,
    terminal: ContextCompactionNotifyPayload,
}

impl Compaction {
    pub fn label(&self) -> String {
        format!("{} compaction", backend_label(self.backend))
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    /// The durable timeline markers this compaction produced. One compaction is
    /// one marker; more than one is a duplicate row in the user's transcript.
    pub fn markers(&self) -> impl Iterator<Item = &ContextCompactionTimelineEvent> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ContextCompaction(marker) => Some(marker),
            _ => None,
        })
    }

    pub fn terminal(&self) -> &ContextCompactionNotifyPayload {
        &self.terminal
    }
}

/// How long the collector keeps listening after the terminal notify.
///
/// The durable marker and the terminal notify are ordered with respect to each
/// other, but a backend's own *observation* of the same compaction reaches the
/// agent loop on a different channel and lands tens of milliseconds later.
/// Returning at the notify would make a second marker unobservable.
const COMPACTION_SETTLE: Duration = Duration::from_secs(5);

/// Compact through the same client message the UI's compact button sends, and
/// return once the operation reports a terminal status.
pub async fn compact(host: &mut Host, agent: &Agent) -> Compaction {
    let backend = host.backend_kind;
    host.client
        .compact_agent(
            &agent.stream,
            AgentCompactPayload {
                summary_prompt: None,
                max_summary_bytes: None,
            },
        )
        .await
        .expect("compact_agent failed");

    let context = format!("{} compaction", backend_label(backend));
    let mut events = Vec::new();
    let terminal = loop {
        // Summarizing the whole conversation is a model round trip.
        let envelope = host.next_envelope(Duration::from_secs(300), &context).await;
        fail_on_agent_error(&envelope, &context);
        fail_on_client_error(&envelope, &context);
        if envelope.kind == FrameKind::ContextCompactionNotify {
            let notify: ContextCompactionNotifyPayload = envelope
                .parse_payload()
                .expect("parse ContextCompactionNotify");
            if notify.status.is_terminal() {
                break notify;
            }
            continue;
        }
        // Unfiltered by stream, unlike a turn: the compaction marker is
        // broadcast by the agent actor rather than echoed back on the stream the
        // request went out on, and there is only ever one agent here.
        events.extend(chat_events_in(&envelope));
    };
    events.extend(drain_events_for(host, COMPACTION_SETTLE).await);

    Compaction {
        backend,
        events,
        terminal,
    }
}

/// A refused control request answers on this frame rather than the agent
/// stream, so without it a rejected compaction reads as a 300s timeout.
fn fail_on_client_error(envelope: &Envelope, context: &str) {
    if envelope.kind == FrameKind::ClientError {
        let error: ClientErrorPayload = envelope.parse_payload().expect("parse ClientError");
        panic!(
            "{context}: server rejected the request with {:?}: {}",
            error.code, error.message
        );
    }
}

/// Fails when the backend stored anything other than exactly one session, since
/// there is then no single session to hand back.
pub async fn stored_session(host: &mut Host) -> SessionSummary {
    let backend_kind = host.backend_kind;
    host.client
        .list_sessions(ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let payload: SessionListPayload = loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SessionList").await;
        if envelope.kind == FrameKind::SessionList {
            break envelope.parse_payload().expect("parse SessionList");
        }
    };
    let mut sessions: Vec<_> = payload
        .sessions
        .into_iter()
        .filter(|session| session.backend_kind == backend_kind)
        .collect();
    assert_eq!(
        sessions.len(),
        1,
        "{backend_kind:?}: expected exactly one stored session, found {}",
        sessions.len()
    );
    sessions.remove(0)
}

/// The paged-history path is separate server code from the bootstrap replay and
/// has broken on its own; the UI uses both.
pub async fn history_page(host: &mut Host, agent: &Agent) -> SessionHistoryPayload {
    let request_id = HistoryPageRequestId(Uuid::new_v4().to_string());
    host.client
        .fetch_session_history(
            &agent.stream,
            FetchSessionHistoryPayload {
                agent_id: agent.agent_id.clone(),
                request_id: request_id.clone(),
                before_seq: None,
                limit: 200,
            },
        )
        .await
        .expect("fetch_session_history failed");

    loop {
        let envelope = host.next_envelope(CONTROL_TIMEOUT, "SessionHistory").await;
        if envelope.kind != FrameKind::SessionHistory {
            continue;
        }
        let candidate: SessionHistoryPayload =
            envelope.parse_payload().expect("parse SessionHistory");
        if candidate.request_id == request_id {
            return candidate;
        }
    }
}

fn backend_label(backend_kind: BackendKind) -> &'static str {
    match backend_kind {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Tycode => "tycode",
        BackendKind::Acp => "kiro",
        BackendKind::Hermes => "hermes",
    }
}

fn host_settings(backend_kind: BackendKind) -> serde_json::Value {
    let mut settings = json!({
        "settings": {
            "enabled_backends": [backend_kind],
            "default_backend": backend_kind,
            "complexity_tiers_enabled": true,
            "tyde_agent_control_mcp_enabled": true
        }
    });
    // Both tiers get the same pin: a scenario must not be able to escalate cost
    // by asking for something the host considers hard.
    match backend_kind {
        BackendKind::Claude => {
            let model = &pinned_models(backend_kind)[0];
            let tier = json!({"model": {"string": model}, "effort": {"string": "low"}});
            settings["settings"]["backend_tier_configs"] =
                json!({"claude": {"low": tier, "high": tier}});
        }
        BackendKind::Codex => {
            let model = &pinned_models(backend_kind)[0];
            let tier = json!({"model": {"string": model}, "reasoning_effort": {"string": "low"}});
            settings["settings"]["backend_tier_configs"] =
                json!({"codex": {"low": tier, "high": tier}});
        }
        // Hermes pins through per-spawn session settings instead; see
        // `hermes_session_settings`.
        _ => {}
    }
    settings
}

/// Run `scenario` against each backend in turn, on a thread with a stack deep
/// enough for the recursion real backends hit while decoding JSON.
///
/// The scratch directories outlive the [`Host`] deliberately: teardown kills a
/// provider subprocess that is still running in the workspace, so removing the
/// workspace first would pull the ground out from under it.
/// `requires` narrows the run to backends declaring every listed capability.
/// A gated scenario that matches nothing still reports PASS and nextest cannot
/// report a skip from inside a test, so the empty case is announced instead.
pub fn run_scenario<F, Fut>(requires: &[BackendCapability], scenario: F)
where
    F: Fn(Host) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    init_tracing();
    let (backends, skipped): (Vec<_>, Vec<_>) = enabled_backends().into_iter().partition(|kind| {
        let capabilities = server::backend::capabilities_for_backend_kind(*kind);
        requires
            .iter()
            .all(|requirement| capabilities.contains(*requirement))
    });
    if backends.is_empty() {
        eprintln!("COVERAGE: no enabled backend declares {requires:?}; this test asserts nothing");
    } else {
        eprintln!("COVERAGE {requires:?}: {backends:?}, skipped {skipped:?}");
    }

    let result = std::thread::Builder::new()
        .name("conformance".to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime")
                .block_on(async move {
                    for backend_kind in backends {
                        let store = tempfile::tempdir().expect("create store tempdir");
                        let workspace = tempfile::tempdir().expect("create workspace tempdir");
                        let host = Host::new(backend_kind, store.path(), workspace.path()).await;
                        let handle = host.handle.clone();

                        let outcome = AssertUnwindSafe(scenario(host)).catch_unwind().await;

                        // Runs even when the scenario panicked: a leaked
                        // provider subprocess keeps costing money.
                        if tokio::time::timeout(
                            Duration::from_secs(30),
                            handle.shutdown_agents_for_conformance(),
                        )
                        .await
                        .is_err()
                        {
                            eprintln!("{backend_kind:?}: backend shutdown timed out");
                        }
                        if let Err(panic) = outcome {
                            std::panic::resume_unwind(panic);
                        }
                    }
                });
        })
        .expect("spawn conformance thread")
        .join();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Honours `RUST_LOG`, so a failing run can be re-run with backend tracing
/// turned up without touching code. `RUST_LOG=server::backend::codex=trace`
/// prints the verbatim JSON of every app-server notification next to Tyde's
/// derived view of it; see `CodexSession::trace_notification_structure`.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
