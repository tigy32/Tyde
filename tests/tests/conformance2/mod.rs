mod control;

use protocol::{
    AgentInput, AskUserQuestion, BackendKind, ChatEvent, ChatMessage, ChatMessageId,
    MessageMetadataUpdateData, MessageSender, MessageTokenUsage, ModelRequestTokenUsage,
    SendMessagePayload, SendMessageToolResponse, SessionId, SessionSettingValue,
    SessionSettingsValues, TaskList, ToolExecutionCompletedData, ToolExecutionOutcome,
    ToolExecutionResult, ToolRequest, ToolRequestType, ToolUseData,
};
use server::backend::{Backend, BackendEvent, BackendSpawnConfig, EventStream, SendOutcome};
use std::path::Path;
use std::time::Duration;
use tyde_agent_adapter::BackendCapability;

pub const SCRATCH_DIR: &str = "scratch";

pub struct Turn {
    pub backend: BackendKind,
    pub capabilities: tyde_agent_adapter::BackendCapabilities,
    pub expected_models: Vec<String>,
    pub prompt: String,
    pub events: Vec<ChatEvent>,
    pub model_requests: Vec<ModelRequestTokenUsage>,
}

impl Turn {
    pub fn model_requests(&self) -> &[ModelRequestTokenUsage] {
        &self.model_requests
    }

    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// Whether the backend that produced this turn claims a capability.
    ///
    /// Mirrors `Harness::declares` for assertions that run without a
    /// host in hand, so a check can gate on a declaration rather than on
    /// whether the data it wanted happens to be present. Gating on the data is
    /// how an assertion excuses itself from the very defect it exists to catch.
    pub fn declares(&self, capability: BackendCapability) -> bool {
        self.capabilities.contains(capability)
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    pub fn user_messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User) => {
                Some(message)
            }
            _ => None,
        })
    }

    /// Prefix for this turn's assertion failures. Not an identity — it names the
    /// turn by what it asked for, which an index into the conversation could not.
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{:?} turn {prompt:?}", self.backend)
    }

    pub fn tool_requests(&self) -> impl Iterator<Item = &ToolRequest> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolRequest(request) => Some(request),
            _ => None,
        })
    }

    /// The assistant messages the client materialized, in stream order.
    ///
    /// Each one is supposed to be exactly one provider response, and its
    /// `tool_calls` are the calls that response issued — the client's only
    /// handle on which response a tool card belongs to.
    pub fn assistant_messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::StreamEnd(end) => Some(&end.message),
            _ => None,
        })
    }

    pub fn tool_completions(&self) -> impl Iterator<Item = &ToolExecutionCompletedData> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolExecutionCompleted(completion) => Some(completion),
            _ => None,
        })
    }

    /// Every tool call an assistant response declared, in stream order.
    ///
    /// [`Turn::tool_requests`] carries Tyde's *normalized* executable form,
    /// which deliberately drops the provider's own tool name — a `ToolRequest`
    /// says "run this command", not "the model called `mcp__probe__record`".
    /// The declaration is the only place the provider name and the raw
    /// arguments survive, so anything asserting on which tool the model picked
    /// or what it passed has to read them from here.
    pub fn tool_declarations(&self) -> impl Iterator<Item = &ToolUseData> {
        self.events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::StreamEnd(end) => Some(&end.message.tool_calls),
                ChatEvent::MessageAdded(message) => Some(&message.tool_calls),
                _ => None,
            })
            .flatten()
    }

    /// The provider tool name behind a request, or `None` if no response in the
    /// turn declared it. `assert_every_request_was_declared` is what turns that
    /// `None` into a failure; callers here can assume a declared request.
    pub fn declared_name(&self, tool_call_id: &str) -> Option<&str> {
        self.tool_declarations()
            .find(|call| call.tool_call_id == tool_call_id)
            .map(|call| call.name.as_str())
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

    /// Everything the turn streamed, whether or not it became a message.
    ///
    /// [`Turn::final_text`] reads the assembled `StreamEnd`, which a cancelled
    /// turn is required *not* to produce: the partial deltas of an aborted
    /// response never become a message. This is the only view of what the user
    /// actually watched appear.
    pub fn streamed_text(&self) -> String {
        self.events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::StreamDelta(delta) => Some(delta.text.as_str()),
                _ => None,
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

    /// Late metadata applied on top of what `StreamEnd` carried, one entry per
    /// response that ended up with a value.
    ///
    /// Reading `StreamEnd` alone would miss every backend that reports usage
    /// after the response is assembled, which is the ordinary case rather than
    /// the exception: a provider knows its output count once the request
    /// finishes, not while it is still streaming.
    fn merged_metadata<T: Clone>(
        &self,
        on_message: impl Fn(&ChatMessage) -> Option<&T>,
        on_update: impl Fn(&MessageMetadataUpdateData) -> Option<&T>,
    ) -> Vec<T> {
        let mut responses: Vec<(Option<ChatMessageId>, Option<T>)> = Vec::new();
        for event in &self.events {
            match event {
                ChatEvent::StreamEnd(end) => responses.push((
                    end.message.message_id.clone(),
                    on_message(&end.message).cloned(),
                )),
                ChatEvent::MessageMetadataUpdated(update) => {
                    if let Some(value) = on_update(update)
                        && let Some(slot) = responses
                            .iter_mut()
                            .find(|(id, _)| id.as_ref() == Some(&update.message_id))
                    {
                        slot.1 = Some(value.clone());
                    }
                }
                _ => {}
            }
        }
        responses
            .into_iter()
            .filter_map(|(_, value)| value)
            .collect()
    }

    /// Token usage per provider response, as the client finally holds it.
    pub fn reported_usage(&self) -> Vec<MessageTokenUsage> {
        self.merged_metadata(
            |message| message.token_usage.as_ref(),
            |update| update.token_usage.as_ref(),
        )
    }

    /// Every task list this turn pushed, in order.
    pub fn task_updates(&self) -> impl Iterator<Item = &TaskList> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::TaskUpdate(list) => Some(list),
            _ => None,
        })
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

fn init_workspace_repo(workspace: &Path) {
    for args in [
        ["init", "-q", "-b", "main"].as_slice(),
        ["config", "user.email", "conformance@tyde.test"].as_slice(),
        ["config", "user.name", "Tyde Conformance"].as_slice(),
        ["add", "-A"].as_slice(),
        ["commit", "-q", "-m", "conformance workspace"].as_slice(),
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .unwrap_or_else(|err| panic!("run git {args:?}: {err}"));
        assert!(
            output.status.success(),
            "git {args:?} failed seeding the conformance workspace: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

pub fn authorize_paid_run() {
    assert_eq!(
        std::env::var("TYDE_RUN_REAL_AI_TESTS").ok().as_deref(),
        Some("1"),
        "real backend conformance requires TYDE_RUN_REAL_AI_TESTS=1"
    );
}

pub fn backend_selected(name: &str) -> bool {
    let selection = std::env::var("TYDE_REAL_BACKENDS")
        .expect("set TYDE_REAL_BACKENDS to the backends intended for this run");
    let selected: Vec<_> = selection.split(',').map(str::trim).collect();
    assert!(
        !selected.is_empty()
            && selected.iter().all(|kind| [
                "claude",
                "codex",
                "hermes",
                "antigravity",
                "kiro",
                "grok",
                "opencode"
            ]
            .contains(kind)),
        "invalid TYDE_REAL_BACKENDS={selection:?}"
    );
    selected.contains(&name)
}

pub struct Profile {
    pub expected_models: Vec<String>,
    pub settings: SessionSettingsValues,
}

impl Profile {
    pub fn new(models: &[&str], settings: &[(&str, &str)]) -> Self {
        Self {
            expected_models: models.iter().map(|value| (*value).to_owned()).collect(),
            settings: SessionSettingsValues(
                settings
                    .iter()
                    .map(|(key, value)| {
                        (
                            (*key).to_owned(),
                            SessionSettingValue::String((*value).to_owned()),
                        )
                    })
                    .collect(),
            ),
        }
    }

    pub fn codex() -> Self {
        let model =
            std::env::var("TYDE_CODEX_TEST_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".to_owned());
        Self::new(&[&model], &[("model", &model), ("reasoning_effort", "low")])
    }

    pub fn hermes() -> Self {
        let provider =
            std::env::var("TYDE_HERMES_TEST_PROVIDER").unwrap_or_else(|_| "openrouter".to_owned());
        let model = std::env::var("TYDE_HERMES_TEST_MODEL")
            .unwrap_or_else(|_| "deepseek/deepseek-v4-flash-0731".to_owned());
        let reasoning =
            std::env::var("TYDE_HERMES_TEST_REASONING").unwrap_or_else(|_| "low".to_owned());
        let selected = serde_json::json!({"model": model, "provider": provider}).to_string();
        Self::new(
            &[],
            &[("model", &selected), ("reasoning_effort", &reasoning)],
        )
    }
}

pub struct Agent {
    pub session_id: SessionId,
    pub replayed_history: Vec<ChatEvent>,
}

pub struct Harness<B: Backend> {
    backend: Option<B>,
    events: Option<EventStream>,
    workspace: tempfile::TempDir,
    profile: Profile,
    pub config: BackendSpawnConfig,
    pub test_name: String,
    pub observer: std::sync::Arc<Observer>,
    last_session_id: Option<SessionId>,
    control: Option<(
        std::sync::Arc<control::ControlService<B>>,
        tokio::task::JoinHandle<()>,
    )>,
}

impl<B: Backend> Harness<B> {
    pub fn new(profile: Profile, test_name: &str) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let workspace = tempfile::Builder::new()
            .prefix("tyde-conformance2-")
            .tempdir_in("/tmp")
            .expect("create conformance workspace");
        std::fs::write(
            workspace.path().join("README.txt"),
            "tyde conformance workspace",
        )
        .expect("seed workspace");
        std::fs::create_dir(workspace.path().join(SCRATCH_DIR)).expect("seed scratch directory");
        std::fs::write(
            workspace.path().join(SCRATCH_DIR).join("notes.txt"),
            "scratch",
        )
        .expect("seed scratch file");
        init_workspace_repo(workspace.path());
        let mut config = BackendSpawnConfig::default();
        config.session_settings = Some(profile.settings.clone());
        let observer = std::sync::Arc::new(Observer::default());
        config.subagent_emitter = Some(observer.clone());
        Self {
            backend: None,
            events: None,
            workspace,
            profile,
            config,
            test_name: test_name.to_owned(),
            observer,
            last_session_id: None,
            control: None,
        }
    }

    pub async fn install_agent_control(&mut self) {
        assert!(
            self.backend.is_none(),
            "install MCP tools before starting a backend"
        );
        let (service, task) = control::ControlService::start(self.config.clone()).await;
        service.configure(&service.root_id, &mut self.config);
        self.control = Some((service, task));
    }

    pub async fn finish(&mut self) {
        self.shutdown().await;
        if let Some((service, task)) = self.control.take() {
            service.shutdown().await;
            for child in service.children.lock().expect("children").iter() {
                assert!(
                    child.failure.is_none(),
                    "child backend failed: {:?}",
                    child.failure
                );
                super::assert_no_error_message("child shutdown", &child.events);
            }
            task.abort();
            let _ = task.await;
        }
    }

    pub fn backend(&self) -> BackendKind {
        B::session_settings_schema().backend_kind
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    pub fn declares(&self, capability: BackendCapability) -> bool {
        B::capabilities().contains(capability)
    }

    pub fn workspace_roots(&self) -> Vec<String> {
        vec![self.workspace.path().to_string_lossy().into_owned()]
    }

    pub async fn shutdown(&mut self) -> Vec<ChatEvent> {
        if let Some(backend) = self.backend.take() {
            backend.shutdown().await;
        }
        let mut closing = Vec::new();
        if let Some(mut stream) = self.events.take() {
            loop {
                match tokio::time::timeout(Duration::from_secs(30), stream.recv_backend())
                    .await
                    .expect("backend shutdown left its event stream open")
                {
                    Some(BackendEvent::Chat(event)) => closing.push(event),
                    Some(BackendEvent::ModelRequestTokenUsage(_) | BackendEvent::Compaction(_)) => {
                    }
                    None => break,
                }
            }
        }
        closing
    }
}

pub fn user_message(prompt: &str) -> SendMessagePayload {
    SendMessagePayload {
        message: prompt.to_owned(),
        images: None,
        origin: None,
        tool_response: None,
    }
}

pub async fn spawn_agent<B: Backend>(host: &mut Harness<B>, prompt: &str) -> Agent {
    assert!(
        host.backend.is_none(),
        "close the previous session before spawning"
    );
    let (backend, events) = B::spawn(
        host.workspace_roots(),
        host.config.clone(),
        user_message(prompt),
    )
    .await
    .expect("spawn through Backend trait");
    let session_id = backend.session_id();
    host.last_session_id = Some(session_id.clone());
    host.backend = Some(backend);
    host.events = Some(events);
    Agent {
        session_id,
        replayed_history: Vec::new(),
    }
}

pub async fn ask<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: impl AsRef<str>,
) -> Turn {
    let prompt = prompt.as_ref();
    let backend = host.backend.as_ref().expect("backend must be running");
    assert_eq!(
        backend.session_id(),
        agent.session_id,
        "input addressed the wrong session"
    );
    assert!(
        matches!(
            backend
                .send_with_outcome(AgentInput::SendMessage(user_message(prompt)))
                .await,
            SendOutcome::Accepted
        ),
        "backend did not accept {prompt:?}"
    );
    collect_turn(host, agent, prompt).await
}

pub async fn collect_turn<B: Backend>(host: &mut Harness<B>, _agent: &Agent, prompt: &str) -> Turn {
    let mut turn = Turn {
        backend: host.backend(),
        capabilities: B::capabilities(),
        expected_models: host.profile.expected_models.clone(),
        prompt: prompt.to_owned(),
        events: Vec::new(),
        model_requests: Vec::new(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    let stream = host
        .events
        .as_mut()
        .expect("backend event stream must be open");
    let mut saw_stream_end = false;
    loop {
        let event = tokio::time::timeout_at(deadline, stream.recv_backend())
            .await
            .unwrap_or_else(|_| panic!("{}: timed out collecting turn", turn.label()))
            .unwrap_or_else(|| panic!("{}: backend closed mid-turn", turn.label()));
        eprintln!("{} {event:?}", turn.label());
        match event {
            BackendEvent::Chat(event) => {
                saw_stream_end |= matches!(event, ChatEvent::StreamEnd(_));
                let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
                turn.events.push(event);
                if idle {
                    assert!(
                        saw_stream_end,
                        "{}: backend went idle without an assistant response",
                        turn.label()
                    );
                    return turn;
                }
            }
            BackendEvent::ModelRequestTokenUsage(usage) => turn.model_requests.push(usage),
            BackendEvent::Compaction(_) => {}
        }
    }
}

pub async fn ask_with_images<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
    images: Vec<protocol::ImageData>,
) -> Turn {
    let mut input = user_message(prompt);
    input.images = Some(images);
    assert!(
        matches!(
            host.backend
                .as_ref()
                .expect("backend must be running")
                .send_with_outcome(AgentInput::SendMessage(input))
                .await,
            SendOutcome::Accepted
        ),
        "backend refused image input"
    );
    collect_turn(host, agent, prompt).await
}

pub async fn install_mcp_server<B: Backend>(
    host: &mut Harness<B>,
    name: &str,
    command: &str,
    args: Vec<String>,
) {
    host.config
        .startup_mcp_servers
        .push(server::backend::StartupMcpServer {
            name: name.to_owned(),
            supports_parallel_tool_calls: true,
            transport: server::backend::StartupMcpTransport::Stdio {
                command: command.to_owned(),
                args,
                env: Default::default(),
            },
        });
}

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
        format!("{:?} question {prompt:?}", self.backend)
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

pub enum InterruptTrigger {
    /// Once the model has streamed this many characters of text, which is both
    /// proof a response is open and a measure of how far into it the stop
    /// lands.
    ///
    /// How deep matters: a stop sent at the first delta arrives before the
    /// provider has committed to a long answer and is the easy case, while the
    /// failure users report is a stop that lands well inside a long message and
    /// is held until the model finishes writing it.
    ///
    /// Counted in characters rather than deltas because a delta count measures
    /// the transport's chunking, not progress through the answer. Measured on
    /// the same prompt, Claude put 5 numbers in a delta on one run and 24 on
    /// another, and Hermes emits 2 characters at a time — so any delta
    /// threshold deep enough to be interesting for one backend is unreachable
    /// for another, and "unreachable" here means the turn ends before the
    /// interrupt is ever sent.
    AfterStreamedChars(usize),
    /// Once a shell command card has opened, plus [`TOOL_STARTUP_GRACE`].
    AfterCommandRequest,
    ResponseContaining(&'static str),
}

/// How long the client waits for an interrupted turn to report idle.
///
/// Generous next to the sub-second cancellations backends manage today, and far
/// shorter than the ordinary 240s turn budget: a turn that simply runs to
/// completion has to fail here rather than pass four minutes later.
const INTERRUPT_DEADLINE: Duration = Duration::from_secs(45);

/// A tool card opens when the request is emitted, which is before the process
/// behind it has done anything. Interrupting in that window can be satisfied by
/// a backend that had nothing to stop yet, so the command is given a moment to
/// really be running.
const TOOL_STARTUP_GRACE: Duration = Duration::from_secs(3);

/// A turn that was interrupted, and where the interrupt falls in it.
pub struct Interrupted {
    turn: Turn,
    after_completed_response: bool,
    /// How long between sending the interrupt and the turn reporting idle.
    /// `None` when it never did within [`INTERRUPT_DEADLINE`] — deciding
    /// whether that is a defect belongs to `conformance2.rs`.
    settled_in: Option<Duration>,
}

impl Interrupted {
    pub fn after_completed_response(&self) -> bool {
        self.after_completed_response
    }

    /// The turn itself, for the assertions that do not care that it was cut
    /// short.
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn label(&self) -> String {
        self.turn.label()
    }

    pub fn events(&self) -> &[ChatEvent] {
        self.turn.events()
    }

    pub fn settled_in(&self) -> Option<Duration> {
        self.settled_in
    }

    pub fn deadline(&self) -> Duration {
        INTERRUPT_DEADLINE
    }
}

impl<B: Backend> Harness<B> {
    fn turn(&self, prompt: &str) -> Turn {
        Turn {
            backend: self.backend(),
            capabilities: B::capabilities(),
            expected_models: self.profile.expected_models.clone(),
            prompt: prompt.to_owned(),
            events: Vec::new(),
            model_requests: Vec::new(),
        }
    }

    async fn next_chat(&mut self, deadline: tokio::time::Instant) -> Option<ChatEvent> {
        let events = self
            .events
            .as_mut()
            .expect("backend event stream must be open");
        loop {
            match tokio::time::timeout_at(deadline, events.recv_backend()).await {
                Ok(Some(BackendEvent::Chat(event))) => {
                    eprintln!("{} {event:?}", self.test_name);
                    return Some(event);
                }
                Ok(Some(BackendEvent::ModelRequestTokenUsage(_) | BackendEvent::Compaction(_))) => {
                }
                Ok(None) => panic!("{}: backend closed its event stream", self.test_name),
                Err(_) => return None,
            }
        }
    }
}

pub async fn send_prompt<B: Backend>(host: &mut Harness<B>, agent: &Agent, prompt: &str) {
    let backend = host.backend.as_ref().expect("backend must be running");
    assert_eq!(backend.session_id(), agent.session_id);
    assert!(
        matches!(
            backend
                .send_with_outcome(AgentInput::SendMessage(user_message(prompt)))
                .await,
            SendOutcome::Accepted
        ),
        "backend did not accept {prompt:?}"
    );
}

pub async fn drain_events_for<B: Backend>(
    host: &mut Harness<B>,
    window: Duration,
) -> Vec<ChatEvent> {
    let deadline = tokio::time::Instant::now() + window;
    let mut events = Vec::new();
    while let Some(event) = host.next_chat(deadline).await {
        events.push(event);
    }
    events
}

pub async fn ask_expecting_delivery<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
) -> Turn {
    ask(host, agent, prompt).await
}

pub async fn cancel_background_task<B: Backend>(
    host: &mut Harness<B>,
    _agent: &Agent,
    tool_call_id: &str,
) {
    let outcome = host
        .backend
        .as_ref()
        .expect("backend must be running")
        .cancel_background_task(tool_call_id)
        .await;
    assert_eq!(
        outcome,
        server::backend::CancelBackgroundTaskOutcome::Cancelled,
        "backend did not cancel the running background command"
    );
}

pub async fn cancel_turn<B: Backend>(host: &mut Harness<B>, _agent: &Agent) -> Vec<ChatEvent> {
    assert!(
        host.backend
            .as_ref()
            .expect("backend must be running")
            .interrupt()
            .await,
        "backend refused to interrupt the active turn"
    );
    drain_events_for(host, Duration::from_secs(10)).await
}

pub async fn interrupt_turn<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
    trigger: InterruptTrigger,
) -> Interrupted {
    send_prompt(host, agent, prompt).await;
    let mut turn = host.turn(prompt);
    let mut streamed = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let event = host
            .next_chat(deadline)
            .await
            .expect("interrupt trigger timed out");
        let fire = match (&event, &trigger) {
            (ChatEvent::StreamDelta(delta), InterruptTrigger::AfterStreamedChars(wanted)) => {
                streamed += delta.text.chars().count();
                streamed >= *wanted
            }
            // Grok can read its bundled skill before launching the command.
            // Cancelling that read never exercises foreground process cancellation.
            (ChatEvent::ToolRequest(request), InterruptTrigger::AfterCommandRequest) => {
                matches!(request.tool_type, ToolRequestType::RunCommand { .. })
            }
            (ChatEvent::StreamEnd(end), InterruptTrigger::ResponseContaining(marker)) => {
                end.message.content.contains(marker)
            }
            (ChatEvent::TypingStatusChanged(false), _) => panic!(
                "{}: turn went idle before the interrupt trigger ({streamed} characters streamed)",
                turn.label()
            ),
            _ => false,
        };
        turn.events.push(event);
        if fire {
            break;
        }
    }
    if matches!(trigger, InterruptTrigger::AfterCommandRequest) {
        tokio::time::sleep(TOOL_STARTUP_GRACE).await;
    }
    let sent_at = tokio::time::Instant::now();
    assert!(
        host.backend
            .as_ref()
            .expect("backend must be running")
            .interrupt()
            .await,
        "backend refused to interrupt the active turn"
    );
    let deadline = sent_at + INTERRUPT_DEADLINE;
    let after_completed_response = matches!(trigger, InterruptTrigger::ResponseContaining(_));
    let mut saw_cancellation = false;
    let mut settled_in = None;
    while let Some(event) = host.next_chat(deadline).await {
        let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
        saw_cancellation |= matches!(event, ChatEvent::OperationCancelled(_));
        turn.events.push(event);
        if idle && (!after_completed_response || saw_cancellation) {
            settled_in = Some(sent_at.elapsed());
            break;
        }
    }
    Interrupted {
        turn,
        after_completed_response,
        settled_in,
    }
}

pub async fn ask_question<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
) -> Question {
    send_prompt(host, agent, prompt).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    let mut events = Vec::new();
    let (request, question) = loop {
        let event = host
            .next_chat(deadline)
            .await
            .expect("backend did not ask a question");
        let asked = match &event {
            ChatEvent::ToolRequest(request) => match &request.tool_type {
                protocol::ToolRequestType::AskUserQuestion { questions } => questions
                    .first()
                    .map(|question| (request.clone(), question.clone())),
                _ => None,
            },
            _ => None,
        };
        events.push(event);
        if let Some(asked) = asked {
            break asked;
        }
    };
    events.extend(drain_events_for(host, Duration::from_secs(10)).await);
    Question {
        backend: host.backend(),
        prompt: prompt.to_owned(),
        request,
        question,
        events,
    }
}

pub async fn answer_question<B: Backend>(
    host: &mut Harness<B>,
    _agent: &Agent,
    question: &Question,
    answer: &str,
) -> Turn {
    let payload = SendMessagePayload {
        message: answer.to_owned(),
        images: None,
        origin: None,
        tool_response: Some(SendMessageToolResponse::AskUserQuestion {
            tool_call_id: question.tool_call_id().to_owned(),
            answer: answer.to_owned(),
        }),
    };
    assert!(
        matches!(
            host.backend
                .as_ref()
                .expect("backend must be running")
                .send_with_outcome(AgentInput::SendMessage(payload))
                .await,
            SendOutcome::Accepted
        ),
        "backend refused the question's answer"
    );
    let mut turn = host.turn(&format!("answer {answer:?}"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let event = host
            .next_chat(deadline)
            .await
            .expect("answer turn timed out");
        let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
        turn.events.push(event);
        if idle {
            return turn;
        }
    }
}

pub async fn install_skill<B: Backend>(
    host: &mut Harness<B>,
    name: &str,
    description: &str,
    body: &str,
) {
    let directory = host.workspace.path().join("conformance-skills").join(name);
    std::fs::create_dir_all(&directory).expect("create conformance skill directory");
    let path = directory.join("SKILL.md");
    std::fs::write(&path, body).expect("write conformance skill body");
    host.config
        .resolved_spawn_config
        .skills
        .push(server::backend::ResolvedSkill {
            id: protocol::SkillId(name.to_owned()),
            name: name.to_owned(),
            title: None,
            description: Some(description.to_owned()),
            source_dir: directory,
            skill_md_path: path,
        });
    host.config.resolved_spawn_config.skill_selection =
        server::backend::SkillSelection::AllInstalled;
    host.config.resolved_spawn_config.skill_delivery = B::skill_delivery();
}

#[derive(Default)]
pub struct Observer {
    capacity: std::sync::Mutex<Vec<(BackendKind, protocol::BackendCapacityState)>>,
    pub children: std::sync::Arc<std::sync::Mutex<Vec<ChildObservation>>>,
}

pub struct ChildObservation {
    pub id: protocol::AgentId,
    pub tool_use_id: String,
    pub name: String,
    pub description: String,
    pub agent_type: String,
    pub session_id_hint: Option<SessionId>,
    pub events: Vec<ChatEvent>,
    pub model_requests: Vec<ModelRequestTokenUsage>,
    pub total_usage: Vec<u64>,
}

impl server::backend::SubAgentEmitter for Observer {
    fn on_backend_capacity(&self, kind: BackendKind, state: protocol::BackendCapacityState) {
        self.capacity
            .lock()
            .expect("capacity observations")
            .push((kind, state));
    }

    fn on_subagent_spawned(
        &self,
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        session_id_hint: Option<SessionId>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<server::backend::SubAgentHandle, String>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let id = protocol::AgentId(uuid::Uuid::new_v4().to_string());
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (model_usage_tx, mut model_usage_rx) = tokio::sync::mpsc::unbounded_channel();
            let (total_usage_tx, mut total_usage_rx) = tokio::sync::mpsc::unbounded_channel();
            let (name_update_tx, mut name_update_rx) = tokio::sync::mpsc::unbounded_channel();
            let index = {
                let mut children = self.children.lock().expect("child observations");
                let index = children.len();
                children.push(ChildObservation {
                    id: id.clone(),
                    tool_use_id,
                    name,
                    description,
                    agent_type,
                    session_id_hint,
                    events: Vec::new(),
                    model_requests: Vec::new(),
                    total_usage: Vec::new(),
                });
                index
            };
            let children = self.children.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(event) = event_rx.recv() => children.lock().expect("child observations")[index].events.push(event),
                        Some(usage) = model_usage_rx.recv() => children.lock().expect("child observations")[index].model_requests.push(usage),
                        Some(usage) = total_usage_rx.recv() => children.lock().expect("child observations")[index].total_usage.push(usage),
                        Some(name) = name_update_rx.recv() => children.lock().expect("child observations")[index].name = name,
                        else => break,
                    }
                }
            });
            Ok(server::backend::SubAgentHandle {
                event_tx,
                model_usage_tx,
                total_usage_tx,
                agent_id: id,
                name_update_tx: Some(name_update_tx),
            })
        })
    }
}

pub struct CapacityObservation {
    pub backend_kind: BackendKind,
    pub state: protocol::BackendCapacityState,
    pub refreshable: bool,
}

impl<B: Backend> Harness<B> {
    pub async fn await_known_capacity(&self) -> CapacityObservation {
        if self.backend.is_none() {
            return self.refresh_capacity_and_await_report().await;
        }
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some((kind, state)) = self
                    .observer
                    .capacity
                    .lock()
                    .expect("capacity observations")
                    .iter()
                    .rev()
                    .find(|(_, state)| {
                        matches!(state, protocol::BackendCapacityState::Known { .. })
                    })
                    .cloned()
                {
                    return CapacityObservation {
                        backend_kind: kind,
                        state,
                        refreshable: B::capabilities()
                            .contains(BackendCapability::OutOfBandCapacity),
                    };
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("backend did not publish Known capacity")
    }

    pub async fn refresh_capacity_and_await_report(&self) -> CapacityObservation {
        let state = B::read_capacity_out_of_band(&server::backend::BackendProbeContext {
            workspace_roots: self.workspace_roots(),
            ..Default::default()
        })
        .await;
        assert!(
            matches!(state, protocol::BackendCapacityState::Known { .. }),
            "capacity read did not return Known: {state:?}"
        );
        CapacityObservation {
            backend_kind: self.backend(),
            state,
            refreshable: B::capabilities().contains(BackendCapability::OutOfBandCapacity),
        }
    }
}

pub fn duplicate_tool_completion_count(turn: &Turn) -> usize {
    let mut ids = std::collections::HashSet::new();
    turn.tool_completions()
        .filter(|completion| !ids.insert(&completion.tool_call_id))
        .count()
}

pub async fn collect_native_subagent_turn<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
    final_markers: &[&str],
) -> Turn {
    let mut turn = collect_turn(host, agent, prompt).await;
    let spawned = turn
        .tool_requests()
        .filter(|request| {
            matches!(
                request.tool_type,
                protocol::ToolRequestType::AgentSpawn { .. }
            )
        })
        .map(|request| request.tool_call_id.clone())
        .collect::<Vec<_>>();
    if spawned.is_empty() || native_subagent_lifecycle_complete(&turn, &spawned, final_markers) {
        return turn;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while let Some(event) = host.next_chat(deadline).await {
        let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
        turn.events.push(event);
        if idle && native_subagent_lifecycle_complete(&turn, &spawned, final_markers) {
            break;
        }
    }
    turn
}

fn native_subagent_lifecycle_complete(
    turn: &Turn,
    spawned: &[String],
    final_markers: &[&str],
) -> bool {
    spawned.iter().all(|tool_call_id| {
        turn.tool_completions()
            .any(|completion| &completion.tool_call_id == tool_call_id)
    }) && turn.assistant_messages().last().is_some_and(|message| {
        final_markers
            .iter()
            .all(|marker| message.content.contains(marker))
    })
}

pub async fn stored_session<B: Backend>(host: &mut Harness<B>) -> server::backend::BackendSession {
    let id = host
        .last_session_id
        .as_ref()
        .expect("a session was spawned");
    let sessions = B::list_sessions(&server::backend::BackendProbeContext {
        workspace_roots: host.workspace_roots(),
        ..Default::default()
    })
    .await
    .expect("list real backend sessions");
    let mut matches = sessions
        .into_iter()
        .filter(|session| &session.id == id)
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "provider session catalog must list {id:?} exactly once"
    );
    let session = matches.remove(0);
    assert_eq!(session.backend_kind, host.backend());
    session
}

pub async fn resume_agent<B: Backend>(host: &mut Harness<B>, id: &SessionId) -> Agent {
    assert!(
        host.backend.is_none(),
        "close the previous session before resuming"
    );
    let (backend, mut events) = B::resume(host.workspace_roots(), host.config.clone(), id.clone())
        .await
        .expect("resume through Backend trait");
    assert_eq!(
        &backend.session_id(),
        id,
        "resume silently changed the provider session identity"
    );
    if let Some(ready) = events.take_resume_replay_complete() {
        tokio::time::timeout(Duration::from_secs(300), ready)
            .await
            .expect("resume replay timed out")
            .expect("resume replay failed");
    }
    let mut replayed_history = Vec::new();
    loop {
        match events.try_recv_backend() {
            Ok(BackendEvent::Chat(event)) => replayed_history.push(event),
            Ok(BackendEvent::ModelRequestTokenUsage(_) | BackendEvent::Compaction(_)) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                panic!("backend disconnected during resume")
            }
        }
    }
    host.backend = Some(backend);
    host.events = Some(events);
    host.last_session_id = Some(id.clone());
    Agent {
        session_id: id.clone(),
        replayed_history,
    }
}

pub async fn run_workflow<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
) -> Workflow {
    let turn = ask(host, agent, prompt).await;
    let mut events = turn.events().to_vec();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while let Some(event) = host.next_chat(deadline).await {
        let terminal = matches!(&event, ChatEvent::ToolProgress(progress)
            if matches!(&progress.update, protocol::ToolProgressUpdate::Workflow(state)
                if state.status != protocol::WorkflowRunStatus::Running));
        events.push(event);
        if terminal {
            break;
        }
    }
    Workflow {
        backend: host.backend(),
        prompt: prompt.to_owned(),
        turn,
        events,
    }
}

pub struct Workflow {
    backend: BackendKind,
    prompt: String,
    turn: Turn,
    /// The launching turn's events followed by everything drained after it, in
    /// arrival order. Ordering across that boundary is the point: the tool call
    /// completes in the turn and the run reports for as long as it takes.
    events: Vec<ChatEvent>,
}

impl Workflow {
    pub fn label(&self) -> String {
        let prompt: String = self.prompt.chars().take(48).collect();
        format!("{:?} workflow {prompt:?}", self.backend)
    }

    /// The turn that launched the run, for the universal contract.
    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }

    pub fn snapshots(&self) -> impl Iterator<Item = &protocol::WorkflowRunState> {
        self.events.iter().filter_map(|event| match event {
            ChatEvent::ToolProgress(progress) => match &progress.update {
                protocol::ToolProgressUpdate::Workflow(state) => Some(state),
                _ => None,
            },
            _ => None,
        })
    }

    /// Index into [`Workflow::events`] of the first terminal snapshot, and of the
    /// completion of the tool call that launched the run. Both are positions
    /// rather than values because the assertion that matters is their order.
    pub fn terminal_snapshot_position(&self) -> Option<usize> {
        self.events.iter().position(|event| {
            matches!(event, ChatEvent::ToolProgress(progress)
                if matches!(&progress.update, protocol::ToolProgressUpdate::Workflow(state)
                    if state.status != protocol::WorkflowRunStatus::Running))
        })
    }

    pub fn launching_completion_position(&self) -> Option<usize> {
        let tool_call_id = self.tool_call_id()?;
        self.events.iter().position(|event| {
            matches!(event, ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == tool_call_id)
        })
    }

    /// The id every workflow snapshot is addressed to, which is also the tool
    /// call that launched the run.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.events.iter().find_map(|event| match event {
            ChatEvent::ToolProgress(progress)
                if matches!(progress.update, protocol::ToolProgressUpdate::Workflow(_)) =>
            {
                Some(progress.tool_call_id.as_str())
            }
            _ => None,
        })
    }
}

pub async fn close_agent<B: Backend>(host: &mut Harness<B>, agent: &Agent) -> Vec<ChatEvent> {
    assert_eq!(
        &host.backend.as_ref().expect("running backend").session_id(),
        &agent.session_id
    );
    host.shutdown().await
}

pub async fn control_native_goal<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    control: protocol::GoalControl,
) {
    let backend = host.backend.as_ref().expect("running backend");
    assert_eq!(backend.session_id(), agent.session_id);
    assert!(
        matches!(
            backend
                .send_with_outcome(AgentInput::GoalControl(control))
                .await,
            SendOutcome::Accepted
        ),
        "backend refused goal control"
    );
}

pub async fn wait_native_goal<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    expected: Option<protocol::GoalStatus>,
) -> Vec<ChatEvent> {
    assert_eq!(
        host.backend.as_ref().expect("running backend").session_id(),
        agent.session_id
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut events = Vec::new();
    let mut reached = false;
    loop {
        let event = host
            .next_chat(deadline)
            .await
            .unwrap_or_else(|| panic!("native goal did not reach {expected:?}"));
        reached |= matches!(&event, ChatEvent::GoalChanged(goal) if goal.as_ref().map(|goal| goal.status) == expected);
        events.push(event);
        // Goal status can change inside a provider turn. The server queues
        // follow-up input there; a direct trait caller waits for actual idle.
        if reached
            && (expected != Some(protocol::GoalStatus::Complete)
                || matches!(events.last(), Some(ChatEvent::TypingStatusChanged(false))))
        {
            return events;
        }
    }
}

pub fn isolated_codex_settings_process() -> bool {
    if std::env::var_os("TYDE_CODEX_SETTINGS_ISOLATED").is_some() {
        return true;
    }
    let home = tempfile::tempdir().expect("isolated Codex home");
    std::fs::set_permissions(
        home.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("restrict isolated home");
    let source = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").expect("home")).join(".codex")
        });
    for name in ["config.toml", "auth.json", "models_cache.json"] {
        let file = source.join(name);
        if file.is_file() {
            std::fs::copy(file, home.path().join(name)).expect("copy isolated Codex state");
        }
    }
    let config = home.path().join("config.toml");
    let mut content = std::fs::read_to_string(&config).unwrap_or_default();
    content.push_str("\n# TYDE_CONFIG_PRESERVATION_SENTINEL\n");
    std::fs::write(&config, content).expect("seed isolated config");
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "real_codex_global_settings",
            "--ignored",
            "--nocapture",
        ])
        .env("CODEX_HOME", home.path())
        .env("TYDE_CODEX_SETTINGS_ISOLATED", "1")
        .status()
        .expect("run isolated settings scenario");
    assert!(status.success(), "isolated Codex settings scenario failed");
    false
}

pub async fn await_native_settings<B: Backend>(
    host: &mut Harness<B>,
) -> protocol::BackendNativeSettingsSnapshot {
    let snapshot = B::native_settings_snapshot(&server::backend::BackendProbeContext {
        workspace_roots: host.workspace_roots(),
        ..Default::default()
    })
    .await
    .expect("backend native settings snapshot");
    assert_eq!(snapshot.backend_kind, host.backend());
    snapshot
}

pub async fn save_native_settings<B: Backend>(
    host: &mut Harness<B>,
    document: serde_json::Value,
) -> (Result<(), String>, protocol::BackendNativeSettingsSnapshot) {
    let outcome = B::write_native_settings(
        document,
        &server::backend::BackendProbeContext {
            workspace_roots: host.workspace_roots(),
            ..Default::default()
        },
    )
    .await;
    let saved = await_native_settings(host).await;
    (outcome.result, saved)
}

pub fn model_setting_aliases(model: &str) -> Vec<String> {
    match model {
        "opus" => vec!["opus".to_owned(), "claude-opus-5".to_owned()],
        _ => vec![model.to_owned()],
    }
}

pub async fn await_session_schema<B: Backend>(
    host: &mut Harness<B>,
) -> protocol::SessionSettingsSchema {
    B::discover(&server::backend::BackendProbeContext {
        workspace_roots: host.workspace_roots(),
        ..Default::default()
    })
    .await
    .expect("discover real session settings schema")
    .schema
}

pub async fn spawn_agent_with_settings<B: Backend>(
    host: &mut Harness<B>,
    prompt: &str,
    settings: Option<SessionSettingsValues>,
) -> Agent {
    host.config.session_settings = settings;
    spawn_agent(host, prompt).await
}

pub async fn set_session_setting<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    key: &str,
    value: &str,
) -> SessionSettingsValues {
    set_session_setting_value(
        host,
        agent,
        key,
        protocol::SessionSettingValue::String(value.to_owned()),
    )
    .await
}

pub async fn set_session_setting_value<B: Backend>(
    host: &mut Harness<B>,
    _agent: &Agent,
    key: &str,
    value: protocol::SessionSettingValue,
) -> SessionSettingsValues {
    let backend = host.backend.as_mut().expect("live backend");
    let values = SessionSettingsValues([(key.to_owned(), value.clone())].into_iter().collect());
    backend
        .update_session_settings(protocol::SetSessionSettingsPayload { values })
        .await
        .expect("apply live backend session setting");
    let observed = backend
        .read_session_settings()
        .await
        .expect("read applied backend session settings");
    match &value {
        protocol::SessionSettingValue::Null => assert!(
            observed
                .0
                .get(key)
                .is_none_or(|value| *value == protocol::SessionSettingValue::Null),
            "reset setting {key}: {observed:?}"
        ),
        _ => assert_eq!(observed.0.get(key), Some(&value), "read back setting {key}"),
    }
    let persisted = host
        .config
        .session_settings
        .get_or_insert_with(SessionSettingsValues::default);
    if value == protocol::SessionSettingValue::Null {
        persisted.0.remove(key);
    } else {
        persisted.0.insert(key.to_owned(), value);
    }
    observed
}

pub fn add_worktree<B: Backend>(host: &Harness<B>, name: &str) -> std::path::PathBuf {
    let path = host.workspace().join(".claude/worktrees").join(name);
    let output = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            &path.to_string_lossy(),
            "-b",
            name,
            "HEAD",
        ])
        .current_dir(host.workspace())
        .output()
        .expect("run git worktree add");
    assert!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

pub fn claude_session_file(cwd: &Path, session_id: &SessionId) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let encoded: String = canonical
        .to_string_lossy()
        .trim()
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | ':' | '.' | '_') {
                '-'
            } else {
                ch
            }
        })
        .collect();
    std::path::PathBuf::from(
        std::env::var("HOME").expect("HOME must be set to locate Claude sessions"),
    )
    .join(".claude")
    .join("projects")
    .join(encoded)
    .join(format!("{}.jsonl", session_id.0))
}

pub async fn install_host_steering<B: Backend>(host: &mut Harness<B>, body: &str) {
    host.config.resolved_spawn_config.steering_body = body.to_owned();
}

pub struct Compaction {
    backend: BackendKind,
    events: Vec<ChatEvent>,
    pub observations: Vec<server::backend::compaction::BackendObservedCompaction>,
    pub terminal: server::backend::compaction::BackendCompactionResult,
}

impl Compaction {
    pub fn label(&self) -> String {
        format!("{:?} native compaction", self.backend)
    }
    pub fn events(&self) -> &[ChatEvent] {
        &self.events
    }
}

pub async fn compact<B: Backend>(host: &mut Harness<B>, agent: &Agent) -> Compaction {
    use server::backend::compaction::{
        BackendCompactionEvent, BackendCompactionRequest, BackendCompactionStart,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let operation_id = protocol::CompactionOperationId(uuid::Uuid::new_v4().to_string());
    let mut events = Vec::new();
    let mut observations = Vec::new();
    let mut accepted = loop {
        let request = BackendCompactionRequest {
            operation_id: operation_id.clone(),
            trigger: protocol::CompactionTrigger::UserRequested,
            focus: None,
            transcript_authoritative: true,
        };
        let start = tokio::time::timeout_at(
            deadline,
            host.backend
                .as_ref()
                .expect("live backend")
                .begin_compaction(request),
        )
        .await
        .expect("native compaction dispatch timed out");
        match start {
            BackendCompactionStart::Accepted(accepted) => break accepted,
            BackendCompactionStart::Deferred { reason } => {
                eprintln!("{}: compaction deferred: {reason:?}", host.test_name);
                loop {
                    let event = host
                        .next_chat(deadline)
                        .await
                        .expect("deferred compaction did not become idle");
                    let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
                    events.push(event);
                    if idle {
                        break;
                    }
                }
            }
            other => panic!(
                "{:?}: native compaction was not accepted: {other:?}",
                host.backend()
            ),
        }
    };
    assert_eq!(accepted.operation_id, operation_id);
    let terminal = loop {
        tokio::select! {
            terminal = &mut accepted.terminal => break terminal.expect("compaction terminal channel closed"),
            event = host.events.as_mut().expect("backend events").recv_backend() => {
                match event.expect("backend closed during compaction") {
                    BackendEvent::Chat(event) => events.push(event),
                    BackendEvent::Compaction(BackendCompactionEvent::Observed(observation)) => observations.push(*observation),
                    BackendEvent::Compaction(BackendCompactionEvent::Progress(progress)) => assert_eq!(progress.operation_id, operation_id),
                    BackendEvent::ModelRequestTokenUsage(_) => {},
                }
            }
            _ = tokio::time::sleep_until(deadline) => panic!("native compaction did not complete"),
        }
    };
    assert_eq!(terminal.operation_id, operation_id);
    assert_eq!(
        terminal.provider_session_id.as_ref(),
        Some(&agent.session_id)
    );
    let settle = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(event) = tokio::time::timeout_at(
        settle,
        host.events.as_mut().expect("backend events").recv_backend(),
    )
    .await
    {
        match event.expect("backend closed after compaction") {
            BackendEvent::Chat(event) => events.push(event),
            BackendEvent::Compaction(BackendCompactionEvent::Observed(observation)) => {
                observations.push(*observation)
            }
            BackendEvent::Compaction(BackendCompactionEvent::Progress(progress)) => {
                assert_eq!(progress.operation_id, operation_id)
            }
            BackendEvent::ModelRequestTokenUsage(_) => {}
        }
    }
    Compaction {
        backend: host.backend(),
        events,
        observations,
        terminal,
    }
}

pub struct Delegation {
    parent: Turn,
    child: Turn,
    child_agent: control::SpawnObservation,
}

impl Delegation {
    pub fn parent(&self) -> &Turn {
        &self.parent
    }
    pub fn child(&self) -> &Turn {
        &self.child
    }
    pub fn child_agent(&self) -> &control::SpawnObservation {
        &self.child_agent
    }
    pub fn child_inputs(&self) -> Vec<&str> {
        self.child
            .user_messages()
            .map(|message| message.content.as_str())
            .collect()
    }
    pub fn into_turns(self) -> [Turn; 2] {
        [self.parent, self.child]
    }
}

pub async fn delegate<B: Backend>(
    host: &mut Harness<B>,
    parent: &Agent,
    prompt: &str,
    child_prompt: &str,
) -> Delegation {
    let service = host
        .control
        .as_ref()
        .expect("agent-control MCP fixture")
        .0
        .clone();
    let before = service.children.lock().expect("children").len();
    let parent = ask(host, parent, prompt).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(360);
    loop {
        {
            let children = service.children.lock().expect("children");
            let spawned = children
                .get(before..)
                .expect("child registry cannot shrink");
            assert!(
                spawned.len() <= 1,
                "one spawn call created more than one child"
            );
            if let Some(child) = spawned.first() {
                assert!(
                    child.failure.is_none(),
                    "child backend failed: {:?}",
                    child.failure
                );
                if child.idle {
                    let mut turn = host.turn(child_prompt);
                    turn.events = child.events.clone();
                    turn.model_requests = child.requests.clone();
                    return Delegation {
                        parent,
                        child: turn,
                        child_agent: child.launch.clone(),
                    };
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "child did not complete its real backend turn"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn delegate_native<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
    child_prompt: &str,
    marker: &str,
) -> [Turn; 2] {
    send_prompt(host, agent, prompt).await;
    let parent = collect_native_subagent_turn(host, agent, prompt, &[marker]).await;
    let children = super::native_subagent_ids(&parent);
    let [child_id] = children.as_slice() else {
        panic!(
            "{}: expected one native child, got {children:?}",
            parent.label()
        );
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(360);
    loop {
        {
            let observations = host
                .observer
                .children
                .lock()
                .expect("native child observations");
            if let Some(child) = observations.iter().find(|child| &child.id == child_id) {
                let mut turn = host.turn(child_prompt);
                turn.events = child.events.clone();
                turn.model_requests = child.model_requests.clone();
                if turn
                    .assistant_messages()
                    .any(|message| message.content.contains(marker))
                    && turn
                        .events
                        .last()
                        .is_some_and(|event| matches!(event, ChatEvent::TypingStatusChanged(false)))
                {
                    return [parent, turn];
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "native child did not finish its delegated task"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

type CodexScenario = for<'a> fn(
    &'a mut Harness<server::backend::codex::CodexBackend>,
) -> futures_util::future::LocalBoxFuture<'a, ()>;

pub fn run_codex_scenario(name: &'static str, profile: Profile, scenario: CodexScenario) {
    authorize_paid_run();
    if !backend_selected("codex") {
        return;
    }
    std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build conformance runtime")
                .block_on(async {
                    use futures_util::FutureExt;
                    let mut host = Harness::new(profile, name);
                    let result = std::panic::AssertUnwindSafe(scenario(&mut host))
                        .catch_unwind()
                        .await;
                    host.finish().await;
                    if let Err(panic) = result {
                        std::panic::resume_unwind(panic);
                    }
                });
        })
        .expect("spawn conformance thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

pub struct FinalResponseSettlement {
    turn: Turn,
    settled_in: Option<Duration>,
}

impl FinalResponseSettlement {
    pub fn turn(&self) -> &Turn {
        &self.turn
    }
    pub fn settled_in(&self) -> Option<Duration> {
        self.settled_in
    }
}

pub async fn ask_through_final_response<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
    final_marker: &str,
    settle_window: Duration,
) -> FinalResponseSettlement {
    send_prompt(host, agent, prompt).await;
    let mut turn = host.turn(prompt);
    let mut final_at: Option<tokio::time::Instant> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let until = final_at.map(|at| at + settle_window).unwrap_or(deadline);
        let event = tokio::time::timeout_at(
            until,
            host.events.as_mut().expect("backend stream").recv_backend(),
        )
        .await;
        let event = match event {
            Ok(Some(event)) => event,
            Ok(None) => panic!("{}: backend closed awaiting final response", turn.label()),
            Err(_) if final_at.is_some() => {
                return FinalResponseSettlement {
                    turn,
                    settled_in: None,
                };
            }
            Err(_) => panic!("{}: final response did not arrive", turn.label()),
        };
        eprintln!("{} {event:?}", turn.label());
        match event {
            BackendEvent::Chat(event) => {
                if matches!(&event, ChatEvent::StreamEnd(end) if end.message.content.contains(final_marker))
                {
                    final_at = Some(tokio::time::Instant::now());
                }
                let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
                turn.events.push(event);
                if idle && final_at.is_some() {
                    return FinalResponseSettlement {
                        turn,
                        settled_in: final_at.map(|at| at.elapsed()),
                    };
                }
            }
            BackendEvent::ModelRequestTokenUsage(usage) => turn.model_requests.push(usage),
            BackendEvent::Compaction(_) => {}
        }
    }
}

pub async fn request_plan_approval<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    prompt: &str,
) -> (Turn, ToolRequest) {
    send_prompt(host, agent, prompt).await;
    let mut turn = host.turn(prompt);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let event = host
            .next_chat(deadline)
            .await
            .expect("backend did not request plan approval");
        let request = match &event {
            ChatEvent::ToolRequest(request)
                if matches!(request.tool_type, ToolRequestType::ExitPlanMode { .. }) =>
            {
                Some(request.clone())
            }
            _ => None,
        };
        turn.events.push(event);
        if let Some(request) = request {
            turn.events
                .extend(drain_events_for(host, Duration::from_secs(3)).await);
            return (turn, request);
        }
    }
}

pub async fn approve_plan<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    tool_call_id: &str,
) -> Turn {
    let mut payload = user_message("Approved. Implement the plan now.");
    payload.tool_response = Some(SendMessageToolResponse::ExitPlanMode {
        tool_call_id: tool_call_id.to_owned(),
        decision: protocol::ExitPlanModeDecision::Approve,
        feedback: None,
    });
    assert!(
        matches!(
            host.backend
                .as_ref()
                .expect("live backend")
                .send_with_outcome(AgentInput::SendMessage(payload))
                .await,
            SendOutcome::Accepted
        ),
        "backend refused plan approval"
    );
    collect_turn(host, agent, "Approved. Implement the plan now.").await
}
