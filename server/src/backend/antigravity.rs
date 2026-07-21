use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_group::AsyncCommandGroup;
use protocol::{
    AgentInput, BackendAccessMode, BackendKind, ChatEvent, ChatMessage, MessageSender,
    MessageTokenUsage, ModelInfo, OperationCancelledData, SelectOption, SessionId,
    SessionSettingField, SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema,
    SessionSettingsValues, SpawnCostHint, StreamEndData, StreamStartData, StreamTextDeltaData,
    TokenUsageUnavailableReason,
};
use serde_json::{Map, Value, json, to_value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::backend::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    StartupMcpServer, StartupMcpTransport, backend_fork_unsupported_message,
    protocol_images_to_attachments, render_combined_spawn_instructions,
    resolve_settings as resolve_backend_settings,
};
use crate::process_env;
use crate::subprocess::ImageAttachment;

const ANTIGRAVITY_AGENT_NAME: &str = "antigravity";
const ANTIGRAVITY_SPAWN_TIMEOUT: Duration = Duration::from_secs(120);
const ANTIGRAVITY_PRINT_TIMEOUT: &str = "5m";
const ANTIGRAVITY_LOG_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ANTIGRAVITY_DEFAULT_MODEL: &str = "Gemini 3.5 Flash (Medium)";
const ANTIGRAVITY_LOW_MODEL: &str = "Gemini 3.5 Flash (Low)";
const ANTIGRAVITY_HIGH_MODEL: &str = "Gemini 3.1 Pro (High)";
const ANTIGRAVITY_ERROR_PREFIXES: &[&str] = &["Authentication required", "Error:"];

static ANTIGRAVITY_TURN_COUNTER: AtomicU64 = AtomicU64::new(1);
static ANTIGRAVITY_MCP_CONFIG_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone)]
pub struct AntigravityBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    session_id: SessionId,
    inner: Arc<AntigravityInner>,
}

struct AntigravityInner {
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    state: Mutex<AntigravityState>,
}

struct AntigravityState {
    session_id: Option<SessionId>,
    conversations_dir: PathBuf,
    primary_root: String,
    extra_roots: Vec<String>,
    startup_mcp_servers: Vec<StartupMcpServer>,
    combined_instructions: Option<String>,
    session_settings: SessionSettingsValues,
    access_mode: BackendAccessMode,
    active_turn: Option<ActiveTurn>,
    /// Messages accepted while a turn was already active. The agent actor
    /// normally queues busy-time sends itself, so entries land here only in
    /// the narrow race where the actor observed idle before this backend
    /// cleared its turn. `run_prepared_turn` drains the front entry into a
    /// fresh turn once the active turn clears; entries that cannot run
    /// (shutdown, preparation failure) are surfaced as an error card, never
    /// silently dropped.
    queued_sends: VecDeque<QueuedSend>,
    /// Set by `shutdown`. Blocks new sends and queue drains so a queued
    /// message cannot start a turn (or spawn a fresh agy process) after the
    /// backend has been told to close.
    closing: bool,
}

struct ActiveTurn {
    id: u64,
    cancel_tx: Option<oneshot::Sender<()>>,
}

struct QueuedSend {
    message: String,
    session_capture: Option<SessionCapture>,
}

struct PreparedTurn {
    turn_id: u64,
    conversation_id: Option<SessionId>,
    mcp_namespace: String,
    primary_root: String,
    extra_roots: Vec<String>,
    startup_mcp_servers: Vec<StartupMcpServer>,
    log_file: PathBuf,
    prompt: String,
    model: String,
    access_mode: BackendAccessMode,
    message_id: String,
    cancel_rx: Option<oneshot::Receiver<()>>,
    session_capture: Option<SessionCapture>,
}

struct AntigravityStdoutSummary {
    stdout: String,
    streamed_text: String,
    stream_started: bool,
    blocked_error_prefix: bool,
}

enum WaitResult {
    Exited(Result<ExitStatus, String>),
    Cancelled,
}

enum TurnOutcome {
    Completed(AntigravityStdoutSummary),
    Cancelled(AntigravityStdoutSummary),
    Failed {
        summary: AntigravityStdoutSummary,
        error: String,
    },
}

#[derive(Clone)]
struct SessionCapture {
    tx: mpsc::UnboundedSender<Result<SessionId, String>>,
}

impl SessionCapture {
    fn new(ready_tx: oneshot::Sender<Result<SessionId, String>>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            if let Some(result) = rx.recv().await {
                let _ = ready_tx.send(result);
            }
        });
        Self { tx }
    }

    fn succeed(&self, session_id: SessionId) {
        let _ = self.tx.send(Ok(session_id));
    }

    fn fail(&self, error: impl Into<String>) {
        let _ = self.tx.send(Err(error.into()));
    }
}

impl Backend for AntigravityBackend {
    fn session_settings_schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Antigravity,
            fields: vec![SessionSettingField {
                key: "model".to_string(),
                label: "Model".to_string(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: antigravity_known_models(),
                    default: Some(ANTIGRAVITY_DEFAULT_MODEL.to_string()),
                    nullable: false,
                },
            }],
        }
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir = resolve_antigravity_conversations_dir(None)?;
        Self::spawn_with_conversations_dir(
            workspace_roots,
            config,
            initial_input,
            conversations_dir,
        )
        .await
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir = resolve_antigravity_conversations_dir(None)?;
        Self::resume_with_conversations_dir(workspace_roots, config, session_id, conversations_dir)
            .await
    }

    async fn fork(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Antigravity),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        Ok(Vec::new())
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).is_ok()
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).is_ok()
    }

    async fn shutdown(self) {
        let dropped_queued = {
            let mut state = self.inner.state.lock().await;
            state.closing = true;
            std::mem::take(&mut state.queued_sends)
        };
        if !dropped_queued.is_empty() {
            self.inner.emit_error(&format!(
                "Antigravity backend is shutting down; {} queued message(s) were not sent.",
                dropped_queued.len()
            ));
        }
        for mut queued in dropped_queued {
            fail_session_capture(
                &mut queued.session_capture,
                "Antigravity backend is shutting down; the queued message was not sent.",
            );
        }
        let _ = self.interrupt_tx.send(());
    }
}

impl AntigravityBackend {
    pub(crate) async fn spawn_with_conversations_dir(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        conversations_dir: PathBuf,
    ) -> Result<(Self, EventStream), String> {
        let (primary_root, extra_roots) = resolve_workspace_roots(&workspace_roots)?;
        let resolved_settings = resolve_session_settings(&config);
        let _ = selected_model(&resolved_settings)?;
        let combined_instructions =
            render_combined_spawn_instructions(&config.resolved_spawn_config);
        let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let inner = Arc::new(AntigravityInner {
            events_tx,
            state: Mutex::new(AntigravityState {
                session_id: None,
                conversations_dir,
                primary_root,
                extra_roots,
                startup_mcp_servers: config.startup_mcp_servers,
                combined_instructions,
                session_settings: resolved_settings,
                access_mode: config.resolved_spawn_config.access_mode,
                active_turn: None,
                queued_sends: VecDeque::new(),
                closing: false,
            }),
        });

        let inner_task = Arc::clone(&inner);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, String>>();
        tokio::spawn(async move {
            run_antigravity_actor(
                inner_task,
                input_rx,
                interrupt_rx,
                Some(initial_input),
                Some(ready_tx),
            )
            .await;
        });

        let session_id = match tokio::time::timeout(ANTIGRAVITY_SPAWN_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(session_id))) => session_id,
            Ok(Ok(Err(err))) => {
                let _ = interrupt_tx.send(());
                return Err(err);
            }
            Ok(Err(_)) => {
                return Err(
                    "Antigravity spawn initialization task ended before reporting a native conversation ID"
                        .to_string(),
                );
            }
            Err(_) => {
                let _ = interrupt_tx.send(());
                return Err(
                    "Timed out waiting for Antigravity to report a native conversation ID"
                        .to_string(),
                );
            }
        };

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                session_id,
                inner,
            },
            EventStream::new(events_rx),
        ))
    }

    pub(crate) async fn resume_with_conversations_dir(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
        conversations_dir: PathBuf,
    ) -> Result<(Self, EventStream), String> {
        if !is_antigravity_native_session_id(&session_id) {
            return Err(format!(
                "Antigravity resume requires a native agy conversation UUID, got {session_id}"
            ));
        }
        ensure_antigravity_conversation_exists(&session_id, &conversations_dir)?;

        let (primary_root, extra_roots) = resolve_workspace_roots(&workspace_roots)?;
        let resolved_settings = resolve_session_settings(&config);
        let _ = selected_model(&resolved_settings)?;
        let combined_instructions =
            render_combined_spawn_instructions(&config.resolved_spawn_config);
        let (input_tx, input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let inner = Arc::new(AntigravityInner {
            events_tx,
            state: Mutex::new(AntigravityState {
                session_id: Some(session_id.clone()),
                conversations_dir,
                primary_root,
                extra_roots,
                startup_mcp_servers: config.startup_mcp_servers,
                combined_instructions,
                session_settings: resolved_settings,
                access_mode: config.resolved_spawn_config.access_mode,
                active_turn: None,
                queued_sends: VecDeque::new(),
                closing: false,
            }),
        });

        let inner_task = Arc::clone(&inner);
        tokio::spawn(async move {
            run_antigravity_actor(inner_task, input_rx, interrupt_rx, None, None).await;
        });

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                session_id,
                inner,
            },
            EventStream::new(events_rx),
        ))
    }
}

async fn run_antigravity_actor(
    inner: Arc<AntigravityInner>,
    mut input_rx: mpsc::UnboundedReceiver<AgentInput>,
    mut interrupt_rx: mpsc::UnboundedReceiver<()>,
    initial_input: Option<protocol::SendMessagePayload>,
    initial_session_capture_tx: Option<oneshot::Sender<Result<SessionId, String>>>,
) {
    if let Some(initial_input) = initial_input {
        let initial_session_capture = initial_session_capture_tx.map(SessionCapture::new);
        inner
            .handle_send_message(initial_input, initial_session_capture)
            .await;
    }

    loop {
        tokio::select! {
            incoming = input_rx.recv() => {
                let Some(input) = incoming else {
                    break;
                };
                match input {
                    AgentInput::SendMessage(payload) => inner.handle_send_message(payload, None).await,
                    AgentInput::UpdateSessionSettings(payload) => inner.handle_update_settings(payload.values).await,
                    AgentInput::EditQueuedMessage(_)
                    | AgentInput::CancelQueuedMessage(_)
                    | AgentInput::SendQueuedMessageNow(_) => {
                        panic!("queued-message inputs must be handled by the agent actor before reaching the backend");
                    }
                }
            }
            interrupt = interrupt_rx.recv() => {
                let Some(()) = interrupt else {
                    break;
                };
                inner.cancel_active_turn().await;
            }
        }
    }

    inner.cancel_active_turn().await;
}

impl AntigravityInner {
    async fn handle_send_message(
        self: &Arc<Self>,
        payload: protocol::SendMessagePayload,
        session_capture: Option<SessionCapture>,
    ) {
        let images = protocol_images_to_attachments(payload.images);
        self.emit_user_message(&payload.message, images.as_deref());
        if images.as_ref().is_some_and(|images| !images.is_empty()) {
            let error = "Antigravity CLI does not support image input in headless print mode.";
            if let Some(capture) = session_capture {
                capture.fail(error);
            }
            self.emit_error(error);
            return;
        }

        let prepared = match self.prepare_turn(payload.message, session_capture).await {
            Ok(Some(turn)) => turn,
            Ok(None) => return,
            Err(err) => {
                self.emit_error(&err);
                return;
            }
        };

        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_prepared_turn(prepared).await;
        });
    }

    async fn handle_update_settings(&self, values: SessionSettingsValues) {
        let update_result = {
            let mut state = self.state.lock().await;
            crate::backend::apply_session_settings_update(&mut state.session_settings, &values);
            selected_model(&state.session_settings)
        };
        if let Err(err) = update_result {
            self.emit_error(&format!("Invalid Antigravity session settings: {err}"));
        }
    }

    /// Returns `Ok(None)` when the message was queued behind an active turn;
    /// `run_prepared_turn` drains the queue once that turn clears.
    async fn prepare_turn(
        &self,
        message: String,
        mut session_capture: Option<SessionCapture>,
    ) -> Result<Option<PreparedTurn>, String> {
        let mut state = self.state.lock().await;
        if state.closing {
            let err = "Antigravity backend is shutting down; the message was not sent.".to_string();
            fail_session_capture(&mut session_capture, &err);
            return Err(err);
        }
        if state.active_turn.is_some() {
            state.queued_sends.push_back(QueuedSend {
                message,
                session_capture,
            });
            tracing::info!(
                queued = state.queued_sends.len(),
                "queued message behind active Antigravity turn"
            );
            return Ok(None);
        }

        Self::prepare_turn_locked(&mut state, message, session_capture).map(Some)
    }

    /// Build a turn while holding the state lock, reserving `active_turn`
    /// before the lock is released so no other send can double-start.
    fn prepare_turn_locked(
        state: &mut AntigravityState,
        message: String,
        mut session_capture: Option<SessionCapture>,
    ) -> Result<PreparedTurn, String> {
        let model = match selected_model(&state.session_settings) {
            Ok(model) => model,
            Err(err) => {
                fail_session_capture(&mut session_capture, &err);
                return Err(err);
            }
        };
        let turn_id = ANTIGRAVITY_TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let log_file = match new_antigravity_log_file_path(turn_id) {
            Ok(path) => path,
            Err(err) => {
                fail_session_capture(&mut session_capture, &err);
                return Err(err);
            }
        };
        let message_id = format!("antigravity-msg-{turn_id}");
        let conversation_id = state.session_id.clone();
        if let Some(session_id) = conversation_id.as_ref()
            && let Err(err) =
                ensure_antigravity_conversation_exists(session_id, &state.conversations_dir)
        {
            fail_session_capture(&mut session_capture, &err);
            return Err(err);
        }
        let prompt = build_prompt(
            conversation_id
                .is_none()
                .then_some(state.combined_instructions.as_deref())
                .flatten(),
            &message,
        );
        let mcp_namespace = conversation_id
            .as_ref()
            .map(|session_id| session_id.0.clone())
            .unwrap_or_else(|| format!("pending-{turn_id}-{}", Uuid::new_v4()));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        state.active_turn = Some(ActiveTurn {
            id: turn_id,
            cancel_tx: Some(cancel_tx),
        });

        Ok(PreparedTurn {
            turn_id,
            conversation_id,
            mcp_namespace,
            primary_root: state.primary_root.clone(),
            extra_roots: state.extra_roots.clone(),
            startup_mcp_servers: state.startup_mcp_servers.clone(),
            log_file,
            prompt,
            model,
            access_mode: state.access_mode,
            message_id,
            cancel_rx: Some(cancel_rx),
            session_capture,
        })
    }

    async fn run_prepared_turn(self: Arc<Self>, mut prepared: PreparedTurn) {
        self.emit_typing_status(true);
        let outcome = self.run_turn(&mut prepared).await;

        match outcome {
            TurnOutcome::Completed(summary) => {
                let text = summary.streamed_text;
                if summary.stream_started {
                    self.emit_stream_end(&prepared.message_id, text.clone(), Some(&prepared.model));
                    self.clear_active_turn(prepared.turn_id).await;
                } else {
                    self.emit_error("Antigravity returned no assistant output.");
                    self.clear_active_turn(prepared.turn_id).await;
                }
            }
            TurnOutcome::Cancelled(summary) => {
                if summary.stream_started {
                    self.emit_stream_end(
                        &prepared.message_id,
                        summary.streamed_text,
                        Some(&prepared.model),
                    );
                }
                self.emit_operation_cancelled("Antigravity turn cancelled.");
                self.clear_active_turn(prepared.turn_id).await;
            }
            TurnOutcome::Failed { summary, error } => {
                if summary.stream_started {
                    self.emit_stream_end(
                        &prepared.message_id,
                        summary.streamed_text,
                        Some(&prepared.model),
                    );
                }
                self.emit_error(&error);
                self.clear_active_turn(prepared.turn_id).await;
            }
        }

        self.emit_typing_status(false);
        self.start_next_queued_send().await;
    }

    /// Dispatch the oldest message that arrived while a turn was active. The
    /// pop, turn preparation, and `active_turn` reservation happen under one
    /// state lock, so a racing send can neither reorder the queue nor
    /// double-start; a queued entry whose preparation fails is surfaced as an
    /// error and the drain continues with the next entry instead of stranding
    /// it. The user bubble for a queued message was already emitted when it
    /// was accepted. Returns an erased boxed future: this sits on the
    /// run_prepared_turn → start_next_queued_send loop, so an `async fn` here
    /// would make run_prepared_turn's opaque future type reference itself.
    fn start_next_queued_send(
        self: &Arc<Self>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let this = Arc::clone(self);
        Box::pin(async move {
            // One lock over the whole drain: a failing entry's error is
            // emitted (sync send) without releasing the lock, so a fresh send
            // cannot slip in between a failed entry and the next queued one.
            let launch = {
                let mut state = this.state.lock().await;
                loop {
                    if state.closing || state.active_turn.is_some() {
                        break None;
                    }
                    let Some(queued) = state.queued_sends.pop_front() else {
                        break None;
                    };
                    match Self::prepare_turn_locked(
                        &mut state,
                        queued.message,
                        queued.session_capture,
                    ) {
                        Ok(prepared) => break Some(prepared),
                        Err(err) => this.emit_error(&err),
                    }
                }
            };
            if let Some(prepared) = launch {
                let turn = Arc::clone(&this);
                tokio::spawn(async move {
                    turn.run_prepared_turn(prepared).await;
                });
            }
        })
    }

    async fn run_turn(self: &Arc<Self>, prepared: &mut PreparedTurn) -> TurnOutcome {
        let args = antigravity_cli_args(
            prepared.access_mode,
            &prepared.model,
            &prepared.extra_roots,
            prepared.conversation_id.as_ref(),
            &prepared.log_file,
            &prepared.prompt,
        );

        let mcp_guard = match install_antigravity_mcp_config(
            &prepared.mcp_namespace,
            &prepared.startup_mcp_servers,
        )
        .await
        {
            Ok(guard) => guard,
            Err(err) => {
                prepared.fail_session_capture(&err);
                return TurnOutcome::Failed {
                    summary: AntigravityStdoutSummary::empty(),
                    error: err,
                };
            }
        };

        let outcome = self.run_turn_process(prepared, &args).await;
        restore_antigravity_mcp_config(mcp_guard, outcome)
    }

    async fn run_turn_process(
        self: &Arc<Self>,
        prepared: &mut PreparedTurn,
        args: &[String],
    ) -> TurnOutcome {
        let mut command = Command::new("agy");
        for arg in args {
            command.arg(arg);
        }
        if let Some(path) = process_env::resolved_child_process_path() {
            command.env("PATH", path);
        }
        command
            .current_dir(&prepared.primary_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match command.group_spawn() {
            Ok(child) => child,
            Err(err) => {
                prepared.fail_session_capture(&format!("Failed to start Antigravity CLI: {err:?}"));
                return TurnOutcome::Failed {
                    summary: AntigravityStdoutSummary::empty(),
                    error: format!("Failed to start Antigravity CLI: {err:?}"),
                };
            }
        };

        let session_capture = prepared.session_capture.take();
        let stdout_session_capture = prepared
            .conversation_id
            .is_none()
            .then(|| session_capture.clone())
            .flatten();
        let log_watcher = start_antigravity_log_watcher(
            Arc::clone(self),
            prepared.log_file.clone(),
            prepared.conversation_id.clone(),
            session_capture,
        );

        let stdout = match child.inner().stdout.take() {
            Some(stdout) => stdout,
            None => {
                fail_pending_session_capture(
                    stop_antigravity_log_watcher(log_watcher).await,
                    "Failed to capture Antigravity stdout",
                );
                return TurnOutcome::Failed {
                    summary: AntigravityStdoutSummary::empty(),
                    error: "Failed to capture Antigravity stdout".to_string(),
                };
            }
        };
        let stderr = match child.inner().stderr.take() {
            Some(stderr) => stderr,
            None => {
                fail_pending_session_capture(
                    stop_antigravity_log_watcher(log_watcher).await,
                    "Failed to capture Antigravity stderr",
                );
                return TurnOutcome::Failed {
                    summary: AntigravityStdoutSummary::empty(),
                    error: "Failed to capture Antigravity stderr".to_string(),
                };
            }
        };

        let stdout_task = tokio::spawn(read_antigravity_stdout(
            stdout,
            self.events_tx.clone(),
            prepared.message_id.clone(),
            prepared.model.clone(),
            stdout_session_capture,
        ));
        let stderr_task = tokio::spawn(read_antigravity_stderr(stderr));

        let Some(mut cancel_rx) = prepared.cancel_rx.take() else {
            fail_pending_session_capture(
                stop_antigravity_log_watcher(log_watcher).await,
                "Antigravity turn cancellation channel was already consumed",
            );
            return TurnOutcome::Failed {
                summary: AntigravityStdoutSummary::empty(),
                error: "Antigravity turn cancellation channel was already consumed".to_string(),
            };
        };
        let wait_result = tokio::select! {
            _ = &mut cancel_rx => WaitResult::Cancelled,
            status = child.wait() => {
                WaitResult::Exited(status.map_err(|err| format!("Failed to wait for Antigravity process: {err:?}")))
            }
        };

        if matches!(wait_result, WaitResult::Cancelled) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        let summary = match stdout_task.await {
            Ok(summary) => summary,
            Err(err) => {
                fail_pending_session_capture(
                    stop_antigravity_log_watcher(log_watcher).await,
                    &format!("Failed to collect Antigravity stdout: {err:?}"),
                );
                return TurnOutcome::Failed {
                    summary: AntigravityStdoutSummary::empty(),
                    error: format!("Failed to collect Antigravity stdout: {err:?}"),
                };
            }
        };
        let stderr_output = match stderr_task.await {
            Ok(stderr) => stderr,
            Err(err) => {
                fail_pending_session_capture(
                    stop_antigravity_log_watcher(log_watcher).await,
                    &format!("Failed to collect Antigravity stderr: {err:?}"),
                );
                return TurnOutcome::Failed {
                    summary,
                    error: format!("Failed to collect Antigravity stderr: {err:?}"),
                };
            }
        };

        let log_result = stop_antigravity_log_watcher(log_watcher).await;
        let outcome = match wait_result {
            WaitResult::Cancelled => TurnOutcome::Cancelled(summary),
            WaitResult::Exited(Err(error)) => TurnOutcome::Failed { summary, error },
            WaitResult::Exited(Ok(status)) => evaluate_exit_status(status, summary, &stderr_output),
        };
        self.finalize_turn_conversation(prepared, log_result, outcome, &stderr_output)
            .await
    }

    async fn finalize_turn_conversation(
        &self,
        prepared: &PreparedTurn,
        log_result: AntigravityLogWatchResult,
        outcome: TurnOutcome,
        stderr_output: &str,
    ) -> TurnOutcome {
        match prepared.conversation_id.as_ref() {
            Some(expected) => {
                finalize_expected_conversation(&prepared.log_file, expected, log_result, outcome)
            }
            None => {
                self.finalize_new_conversation(
                    &prepared.log_file,
                    log_result,
                    outcome,
                    stderr_output,
                )
                .await
            }
        }
    }

    async fn finalize_new_conversation(
        &self,
        log_file: &Path,
        mut log_result: AntigravityLogWatchResult,
        outcome: TurnOutcome,
        stderr_output: &str,
    ) -> TurnOutcome {
        if let Some(session_id) = log_result.ids.authoritative_for_new_session() {
            if let Err(err) = self.set_native_session_id(session_id.clone()).await {
                if let Some(capture) = log_result.session_capture.take() {
                    capture.fail(err.clone());
                }
                return fail_outcome_if_not_failed(outcome, err);
            }
            if let Some(capture) = log_result.session_capture.take() {
                capture.succeed(session_id);
            }
            return outcome;
        }

        let error = missing_conversation_error(log_file, &outcome, stderr_output);
        if let Some(capture) = log_result.session_capture.take() {
            capture.fail(error.clone());
        }
        fail_outcome_if_not_failed(outcome, error)
    }

    async fn cancel_active_turn(&self) {
        let cancel_tx = {
            let mut state = self.state.lock().await;
            state
                .active_turn
                .as_mut()
                .and_then(|turn| turn.cancel_tx.take())
        };
        if let Some(cancel_tx) = cancel_tx {
            let _ = cancel_tx.send(());
        }
    }

    async fn set_native_session_id(&self, session_id: SessionId) -> Result<(), String> {
        let mut state = self.state.lock().await;
        match state.session_id.as_ref() {
            Some(existing) if existing == &session_id => Ok(()),
            Some(existing) => Err(format!(
                "Antigravity reported conversation {session_id}, but session is already bound to {existing}"
            )),
            None => {
                state.session_id = Some(session_id);
                Ok(())
            }
        }
    }

    async fn clear_active_turn(&self, turn_id: u64) {
        let mut state = self.state.lock().await;
        if state
            .active_turn
            .as_ref()
            .is_some_and(|active| active.id == turn_id)
        {
            state.active_turn = None;
        }
    }

    fn emit_user_message(&self, content: &str, images: Option<&[ImageAttachment]>) {
        let images = images
            .unwrap_or(&[])
            .iter()
            .map(|image| protocol::ImageData {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            })
            .collect::<Vec<_>>();
        let images = (!images.is_empty()).then_some(images);
        let _ = self.events_tx.send(ChatEvent::MessageAdded(ChatMessage {
            message_id: None,
            timestamp: now_ms(),
            sender: MessageSender::User,
            content: content.to_string(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images,
        }));
    }

    fn emit_typing_status(&self, typing: bool) {
        let _ = self.events_tx.send(ChatEvent::TypingStatusChanged(typing));
    }

    fn emit_stream_end(&self, message_id: &str, content: String, model: Option<&str>) {
        let _ = self.events_tx.send(ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: Some(protocol::ChatMessageId(message_id.to_string())),
                timestamp: now_ms(),
                sender: MessageSender::Assistant {
                    agent: ANTIGRAVITY_AGENT_NAME.to_string(),
                },
                content,
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: model.map(|model| ModelInfo {
                    model: model.to_string(),
                }),
                token_usage: Some(MessageTokenUsage::unavailable(
                    TokenUsageUnavailableReason::BackendDidNotReport,
                )),
                context_breakdown: None,
                images: None,
            },
        }));
    }

    fn emit_operation_cancelled(&self, message: &str) {
        let _ = self
            .events_tx
            .send(ChatEvent::OperationCancelled(OperationCancelledData {
                message: message.to_string(),
            }));
    }

    fn emit_error(&self, content: &str) {
        let _ = self.events_tx.send(ChatEvent::MessageAdded(ChatMessage {
            message_id: None,
            timestamp: now_ms(),
            sender: MessageSender::Error,
            content: content.to_string(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }));
    }
}

fn resolve_workspace_roots(workspace_roots: &[String]) -> Result<(String, Vec<String>), String> {
    if workspace_roots.iter().all(|root| root.trim().is_empty()) {
        let no_root_cwd = antigravity_no_root_cwd()?;
        resolve_workspace_roots_with_no_root_cwd(workspace_roots, &no_root_cwd)
    } else {
        resolve_workspace_roots_with_no_root_cwd(workspace_roots, Path::new(""))
    }
}

fn resolve_workspace_roots_with_no_root_cwd(
    workspace_roots: &[String],
    no_root_cwd: &Path,
) -> Result<(String, Vec<String>), String> {
    let mut roots = workspace_roots
        .iter()
        .map(|root| root.trim())
        .filter(|root| !root.is_empty())
        .collect::<Vec<_>>();
    if roots.iter().any(|root| root.starts_with("ssh://")) {
        return Err("Antigravity backend requires local workspace roots".to_string());
    }
    if roots.is_empty() {
        fs::create_dir_all(no_root_cwd).map_err(|err| {
            format!(
                "Failed to create Antigravity no-root working directory {}: {err}",
                no_root_cwd.display()
            )
        })?;
        return Ok((no_root_cwd.to_string_lossy().to_string(), Vec::new()));
    }
    let primary = roots
        .first()
        .expect("empty roots returned above")
        .to_string();
    if !Path::new(&primary).is_dir() {
        return Err(format!(
            "Antigravity primary workspace root is not a directory: {primary}"
        ));
    }
    let extra = roots.drain(1..).map(str::to_string).collect::<Vec<_>>();
    Ok((primary, extra))
}

fn antigravity_no_root_cwd() -> Result<PathBuf, String> {
    Ok(crate::paths::home_dir()?
        .join(".tyde")
        .join("antigravity")
        .join("no-root"))
}

fn new_antigravity_log_file_path(turn_id: u64) -> Result<PathBuf, String> {
    let dir = crate::paths::home_dir()?
        .join(".tyde")
        .join("antigravity")
        .join("logs");
    fs::create_dir_all(&dir).map_err(|err| {
        format!(
            "Failed to create Antigravity log directory {}: {err}",
            dir.display()
        )
    })?;
    Ok(dir.join(format!("turn-{turn_id}-{}.log", Uuid::new_v4())))
}

pub(crate) fn antigravity_cli_args(
    access_mode: BackendAccessMode,
    model: &str,
    extra_roots: &[String],
    conversation_id: Option<&SessionId>,
    log_file: &Path,
    prompt: &str,
) -> Vec<String> {
    let mut args = vec![
        "--print-timeout".to_string(),
        ANTIGRAVITY_PRINT_TIMEOUT.to_string(),
        "--log-file".to_string(),
        log_file.to_string_lossy().to_string(),
    ];
    match access_mode {
        // `agy` has no workspace-write middle mode. ReadOnly is advisory, so it
        // must use the non-sandbox path to let build/test commands write target/.
        BackendAccessMode::Unrestricted => args.push("--dangerously-skip-permissions".to_string()),
        BackendAccessMode::ReadOnly => args.push("--dangerously-skip-permissions".to_string()),
    }
    args.push("--model".to_string());
    args.push(model.to_string());
    if let Some(conversation_id) = conversation_id {
        args.push(format!("--conversation={conversation_id}"));
    }
    for root in extra_roots {
        args.push("--add-dir".to_string());
        args.push(root.clone());
    }
    args.push("-p".to_string());
    args.push(prompt.to_string());
    args
}

impl PreparedTurn {
    fn fail_session_capture(&mut self, error: &str) {
        if let Some(capture) = self.session_capture.take() {
            capture.fail(error);
        }
    }
}

fn fail_session_capture(capture: &mut Option<SessionCapture>, error: &str) {
    if let Some(capture) = capture.take() {
        capture.fail(error);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AntigravityConversationLogIds {
    created: Option<SessionId>,
    active: Option<SessionId>,
}

impl AntigravityConversationLogIds {
    fn authoritative_for_new_session(&self) -> Option<SessionId> {
        self.active.clone().or_else(|| self.created.clone())
    }
}

#[derive(Default)]
struct AntigravityLogWatchResult {
    ids: AntigravityConversationLogIds,
    session_capture: Option<SessionCapture>,
}

struct AntigravityLogWatcher {
    stop_tx: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<AntigravityLogWatchResult>,
}

fn start_antigravity_log_watcher(
    inner: Arc<AntigravityInner>,
    log_file: PathBuf,
    expected_conversation_id: Option<SessionId>,
    session_capture: Option<SessionCapture>,
) -> AntigravityLogWatcher {
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(watch_antigravity_log_for_conversation(
        inner,
        log_file,
        expected_conversation_id,
        session_capture,
        stop_rx,
    ));
    AntigravityLogWatcher { stop_tx, task }
}

async fn stop_antigravity_log_watcher(watcher: AntigravityLogWatcher) -> AntigravityLogWatchResult {
    let _ = watcher.stop_tx.send(());
    watcher.task.await.unwrap_or_default()
}

async fn watch_antigravity_log_for_conversation(
    inner: Arc<AntigravityInner>,
    log_file: PathBuf,
    expected_conversation_id: Option<SessionId>,
    mut session_capture: Option<SessionCapture>,
    mut stop_rx: oneshot::Receiver<()>,
) -> AntigravityLogWatchResult {
    loop {
        let ids = read_antigravity_conversation_ids_from_log(&log_file);
        if let Some(active) = ids.active.as_ref() {
            match expected_conversation_id.as_ref() {
                Some(expected) if active == expected => {
                    return AntigravityLogWatchResult {
                        ids,
                        session_capture,
                    };
                }
                Some(_) => {}
                None => {
                    match inner.set_native_session_id(active.clone()).await {
                        Ok(()) => {
                            if let Some(capture) = session_capture.take() {
                                capture.succeed(active.clone());
                            }
                        }
                        Err(err) => {
                            if let Some(capture) = session_capture.take() {
                                capture.fail(err);
                            }
                        }
                    }
                    return AntigravityLogWatchResult {
                        ids,
                        session_capture,
                    };
                }
            }
        }

        tokio::select! {
            _ = &mut stop_rx => {
                return AntigravityLogWatchResult {
                    ids: read_antigravity_conversation_ids_from_log(&log_file),
                    session_capture,
                };
            }
            _ = tokio::time::sleep(ANTIGRAVITY_LOG_POLL_INTERVAL) => {}
        }
    }
}

fn finalize_expected_conversation(
    log_file: &Path,
    expected: &SessionId,
    mut log_result: AntigravityLogWatchResult,
    outcome: TurnOutcome,
) -> TurnOutcome {
    if let Some(capture) = log_result.session_capture.take() {
        match log_result.ids.active.as_ref() {
            Some(active) if active == expected => {
                capture.succeed(expected.clone());
            }
            Some(active) => {
                capture.fail(format!(
                    "Antigravity resumed conversation {active}, expected exact conversation {expected}"
                ));
            }
            None => {
                capture.fail(expected_conversation_missing_error(
                    log_file,
                    expected,
                    &log_result.ids,
                ));
            }
        }
    }

    match outcome {
        TurnOutcome::Completed(summary) => match log_result.ids.active.as_ref() {
            Some(active) if active == expected => TurnOutcome::Completed(summary),
            Some(active) => TurnOutcome::Failed {
                summary,
                error: format!(
                    "Antigravity resumed conversation {active}, expected exact conversation {expected}"
                ),
            },
            None => TurnOutcome::Failed {
                summary,
                error: expected_conversation_missing_error(log_file, expected, &log_result.ids),
            },
        },
        TurnOutcome::Cancelled(summary) => TurnOutcome::Cancelled(summary),
        TurnOutcome::Failed { summary, error } => TurnOutcome::Failed { summary, error },
    }
}

fn expected_conversation_missing_error(
    log_file: &Path,
    expected: &SessionId,
    ids: &AntigravityConversationLogIds,
) -> String {
    match ids.created.as_ref() {
        Some(created) => format!(
            "Antigravity log created conversation {created} but did not confirm exact conversation {expected}: {}",
            log_file.display()
        ),
        None => format!(
            "Antigravity log did not confirm exact conversation {expected}: {}",
            log_file.display()
        ),
    }
}

fn missing_conversation_error(
    log_file: &Path,
    outcome: &TurnOutcome,
    stderr_output: &str,
) -> String {
    if let TurnOutcome::Failed { error, .. } = outcome {
        return error.clone();
    }
    let stderr = stderr_output.trim();
    if !stderr.is_empty() {
        return format!(
            "Antigravity log did not report a native conversation UUID: {}; stderr: {stderr}",
            log_file.display()
        );
    }
    format!(
        "Antigravity log did not report a native conversation UUID: {}",
        log_file.display()
    )
}

fn fail_outcome_if_not_failed(outcome: TurnOutcome, error: String) -> TurnOutcome {
    match outcome {
        TurnOutcome::Completed(summary) | TurnOutcome::Cancelled(summary) => {
            TurnOutcome::Failed { summary, error }
        }
        TurnOutcome::Failed {
            summary,
            error: existing,
        } => TurnOutcome::Failed {
            summary,
            error: existing,
        },
    }
}

fn fail_pending_session_capture(mut result: AntigravityLogWatchResult, error: &str) {
    if let Some(capture) = result.session_capture.take() {
        capture.fail(error);
    }
}

fn read_antigravity_conversation_ids_from_log(path: &Path) -> AntigravityConversationLogIds {
    let Ok(contents) = fs::read_to_string(path) else {
        return AntigravityConversationLogIds::default();
    };
    parse_antigravity_conversation_ids(&contents)
}

fn parse_antigravity_conversation_ids(log: &str) -> AntigravityConversationLogIds {
    let mut created = None;
    let mut active = None;
    for line in log.lines() {
        if let Some(uuid) = parse_uuid_after_marker(line, "Created conversation ") {
            created = Some(SessionId(uuid.to_string()));
        }
        if let Some(uuid) = parse_uuid_after_marker(line, "conversation=") {
            active = Some(SessionId(uuid.to_string()));
        }
    }
    AntigravityConversationLogIds { created, active }
}

fn parse_uuid_after_marker(line: &str, marker: &str) -> Option<Uuid> {
    let start = line.find(marker)? + marker.len();
    let candidate = line[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_hexdigit() || *ch == '-')
        .collect::<String>();
    if candidate.is_empty() {
        return None;
    }
    Uuid::parse_str(&candidate).ok()
}

pub(crate) fn is_antigravity_native_session_id(session_id: &SessionId) -> bool {
    session_id.0.len() == 36 && Uuid::parse_str(&session_id.0).is_ok()
}

pub(crate) fn is_antigravity_session_resumable(
    session_id: &SessionId,
    conversations_dir: &Path,
) -> bool {
    is_antigravity_native_session_id(session_id)
        && antigravity_conversation_db_path(session_id, conversations_dir).is_file()
}

fn ensure_antigravity_conversation_exists(
    session_id: &SessionId,
    conversations_dir: &Path,
) -> Result<(), String> {
    let path = antigravity_conversation_db_path(session_id, conversations_dir);
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "Antigravity conversation {session_id} does not exist at {}; refusing to resume without an exact native agy conversation",
            path.display()
        ))
    }
}

fn antigravity_conversation_db_path(session_id: &SessionId, conversations_dir: &Path) -> PathBuf {
    conversations_dir.join(format!("{}.db", session_id.0))
}

pub(crate) fn resolve_antigravity_conversations_dir(
    configured_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    match configured_dir {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(crate::paths::home_dir()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")),
    }
}

pub(crate) fn antigravity_known_models() -> Vec<SelectOption> {
    [
        ANTIGRAVITY_LOW_MODEL,
        ANTIGRAVITY_DEFAULT_MODEL,
        "Gemini 3.5 Flash (High)",
        "Gemini 3.1 Pro (Low)",
        ANTIGRAVITY_HIGH_MODEL,
        "Claude Sonnet 4.6 (Thinking)",
        "Claude Opus 4.6 (Thinking)",
        "GPT-OSS 120B (Medium)",
    ]
    .into_iter()
    .map(|label| SelectOption {
        value: label.to_string(),
        label: label.to_string(),
    })
    .collect()
}

pub(crate) fn antigravity_cost_hint_defaults(cost_hint: SpawnCostHint) -> SessionSettingsValues {
    let model = match cost_hint {
        SpawnCostHint::Low => ANTIGRAVITY_LOW_MODEL,
        SpawnCostHint::Medium => ANTIGRAVITY_DEFAULT_MODEL,
        SpawnCostHint::High => ANTIGRAVITY_HIGH_MODEL,
    };
    let mut values = SessionSettingsValues::default();
    values.0.insert(
        "model".to_string(),
        SessionSettingValue::String(model.to_string()),
    );
    values
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    resolve_backend_settings(
        config,
        &AntigravityBackend::session_settings_schema(),
        antigravity_cost_hint_defaults,
    )
}

fn selected_model(values: &SessionSettingsValues) -> Result<String, String> {
    match values.0.get("model") {
        Some(SessionSettingValue::String(value)) if is_known_model(value) => Ok(value.clone()),
        Some(SessionSettingValue::String(value)) => Err(format!(
            "unknown Antigravity model label {value:?}; expected one of the known agy model labels"
        )),
        Some(other) => Err(format!(
            "Antigravity model setting must be a string, got {other:?}"
        )),
        None => Ok(ANTIGRAVITY_DEFAULT_MODEL.to_string()),
    }
}

fn is_known_model(value: &str) -> bool {
    antigravity_known_models()
        .into_iter()
        .any(|option| option.value == value)
}

fn build_prompt(instructions: Option<&str>, message: &str) -> String {
    let instructions = instructions
        .map(str::trim)
        .filter(|instructions| !instructions.is_empty());
    match instructions {
        Some(instructions) => format!("{instructions}\n\n{message}"),
        None => message.to_string(),
    }
}

async fn read_antigravity_stdout(
    stdout: ChildStdout,
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    message_id: String,
    model: String,
    startup_capture: Option<SessionCapture>,
) -> AntigravityStdoutSummary {
    let mut state = AntigravityStdoutState::new(events_tx, message_id, model, startup_capture);
    let mut reader = BufReader::new(stdout);
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buffer[..n]).to_string();
                state.consume_chunk(&text);
            }
            Err(err) => {
                state.consume_chunk(&format!(
                    "\nError: failed to read Antigravity stdout: {err}"
                ));
                break;
            }
        }
    }
    state.finish()
}

async fn read_antigravity_stderr(stderr: ChildStderr) -> String {
    let mut out = String::new();
    let mut reader = BufReader::new(stderr);
    let _ = reader.read_to_string(&mut out).await;
    out
}

struct AntigravityStdoutState {
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    message_id: String,
    model: String,
    stdout: String,
    streamed_text: String,
    stream_started: bool,
    blocked_error_prefix: bool,
    startup_capture: Option<SessionCapture>,
}

impl AntigravityStdoutState {
    fn new(
        events_tx: mpsc::UnboundedSender<ChatEvent>,
        message_id: String,
        model: String,
        startup_capture: Option<SessionCapture>,
    ) -> Self {
        Self {
            events_tx,
            message_id,
            model,
            stdout: String::new(),
            streamed_text: String::new(),
            stream_started: false,
            blocked_error_prefix: false,
            startup_capture,
        }
    }

    fn consume_chunk(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.stdout.push_str(text);
        if self.blocked_error_prefix {
            self.fail_startup_capture_if_ready();
            return;
        }
        if !self.stream_started {
            match classify_initial_stdout(self.stdout.trim_start()) {
                InitialStdoutClassification::Pending => return,
                InitialStdoutClassification::Error => {
                    self.blocked_error_prefix = true;
                    self.fail_startup_capture_if_ready();
                    return;
                }
                InitialStdoutClassification::Assistant => {
                    self.startup_capture = None;
                    self.start_stream_with_buffer();
                    return;
                }
            }
        }
        self.streamed_text.push_str(text);
        let _ = self
            .events_tx
            .send(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some(self.message_id.clone()),
                text: text.to_string(),
            }));
    }

    fn fail_startup_capture_if_ready(&mut self) {
        let Some(error) = startup_error_capture_message(&self.stdout) else {
            return;
        };
        if let Some(capture) = self.startup_capture.take() {
            capture.fail(error);
        }
    }

    fn start_stream_with_buffer(&mut self) {
        if self.stream_started || self.stdout.trim_start().is_empty() {
            return;
        }
        let _ = self.events_tx.send(ChatEvent::StreamStart(StreamStartData {
            message_id: Some(self.message_id.clone()),
            agent: ANTIGRAVITY_AGENT_NAME.to_string(),
            model: Some(self.model.clone()),
        }));
        self.stream_started = true;
        self.streamed_text.push_str(&self.stdout);
        let _ = self
            .events_tx
            .send(ChatEvent::StreamDelta(StreamTextDeltaData {
                message_id: Some(self.message_id.clone()),
                text: self.stdout.clone(),
            }));
    }

    fn finish(mut self) -> AntigravityStdoutSummary {
        if !self.stream_started
            && !self.blocked_error_prefix
            && !self.stdout.trim_start().is_empty()
        {
            self.start_stream_with_buffer();
        }
        AntigravityStdoutSummary {
            stdout: self.stdout,
            streamed_text: self.streamed_text,
            stream_started: self.stream_started,
            blocked_error_prefix: self.blocked_error_prefix,
        }
    }
}

enum InitialStdoutClassification {
    Pending,
    Error,
    Assistant,
}

fn startup_error_capture_message(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim_start();
    if let Some(rest) = trimmed.strip_prefix("Error:") {
        return startup_error_capture_message_after_prefix(trimmed, "Error:", rest, false);
    }
    if let Some(rest) = trimmed.strip_prefix("Authentication required") {
        return startup_error_capture_message_after_prefix(
            trimmed,
            "Authentication required",
            rest,
            true,
        );
    }
    None
}

fn startup_error_capture_message_after_prefix(
    trimmed: &str,
    prefix: &str,
    rest: &str,
    allow_bare_prefix: bool,
) -> Option<String> {
    let has_detail = rest.chars().any(|ch| !ch.is_whitespace());
    if !has_detail {
        return allow_bare_prefix.then(|| prefix.to_string());
    }

    let first_line = trimmed.lines().next().unwrap_or(trimmed).trim_end();
    let message = if first_line == prefix {
        trimmed.trim_end()
    } else {
        first_line
    };
    if message == prefix && !allow_bare_prefix {
        None
    } else {
        Some(message.to_string())
    }
}

fn classify_initial_stdout(trimmed_start: &str) -> InitialStdoutClassification {
    if trimmed_start.is_empty() {
        return InitialStdoutClassification::Pending;
    }
    for prefix in ANTIGRAVITY_ERROR_PREFIXES {
        if trimmed_start.starts_with(prefix) {
            return InitialStdoutClassification::Error;
        }
        if prefix.starts_with(trimmed_start) {
            return InitialStdoutClassification::Pending;
        }
    }
    InitialStdoutClassification::Assistant
}

impl AntigravityStdoutSummary {
    fn empty() -> Self {
        Self {
            stdout: String::new(),
            streamed_text: String::new(),
            stream_started: false,
            blocked_error_prefix: false,
        }
    }

    fn error_message(&self) -> Option<String> {
        let trimmed = self.stdout.trim();
        if trimmed.is_empty() {
            return None;
        }
        if self.blocked_error_prefix || trimmed.contains("Authentication required") {
            return Some(trimmed.to_string());
        }
        if trimmed
            .lines()
            .any(|line| line.trim_start().starts_with("Error:"))
        {
            return Some(trimmed.to_string());
        }
        None
    }
}

fn evaluate_exit_status(
    status: ExitStatus,
    summary: AntigravityStdoutSummary,
    stderr_output: &str,
) -> TurnOutcome {
    if status.code() == Some(130) {
        return TurnOutcome::Cancelled(summary);
    }
    if let Some(error) = summary.error_message() {
        return TurnOutcome::Failed { summary, error };
    }
    if status.success() {
        return TurnOutcome::Completed(summary);
    }
    let stderr = stderr_output.trim();
    let error = if stderr.is_empty() {
        format!("Antigravity exited with status {status}")
    } else {
        stderr.to_string()
    };
    TurnOutcome::Failed { summary, error }
}

fn restore_antigravity_mcp_config(
    guard: Option<AntigravityMcpConfigGuard>,
    outcome: TurnOutcome,
) -> TurnOutcome {
    let Some(guard) = guard else {
        return outcome;
    };
    match guard.restore() {
        Ok(()) => outcome,
        Err(restore_error) => match outcome {
            TurnOutcome::Completed(summary) | TurnOutcome::Cancelled(summary) => {
                TurnOutcome::Failed {
                    summary,
                    error: restore_error,
                }
            }
            TurnOutcome::Failed { summary, error } => TurnOutcome::Failed {
                summary,
                error: format!("{error}; additionally, {restore_error}"),
            },
        },
    }
}

async fn install_antigravity_mcp_config(
    namespace: &str,
    startup_mcp_servers: &[StartupMcpServer],
) -> Result<Option<AntigravityMcpConfigGuard>, String> {
    if startup_mcp_servers.is_empty() {
        return Ok(None);
    }
    let guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
    let path = antigravity_mcp_config_path()?;
    let restore = AntigravityMcpConfigGuard::install(path, namespace, startup_mcp_servers, guard)?;
    Ok(Some(restore))
}

fn antigravity_mcp_config_path() -> Result<PathBuf, String> {
    Ok(crate::paths::home_dir()?
        .join(".gemini")
        .join("config")
        .join("mcp_config.json"))
}

struct AntigravityMcpConfigGuard {
    path: PathBuf,
    original_bytes: Option<Vec<u8>>,
    restored: bool,
    _mutex_guard: tokio::sync::MutexGuard<'static, ()>,
}

impl AntigravityMcpConfigGuard {
    fn install(
        path: PathBuf,
        namespace: &str,
        startup_mcp_servers: &[StartupMcpServer],
        mutex_guard: tokio::sync::MutexGuard<'static, ()>,
    ) -> Result<Self, String> {
        let original_bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(format!(
                    "Failed to read Antigravity MCP config {}: {err}",
                    path.display()
                ));
            }
        };
        let merged =
            merge_antigravity_mcp_config(original_bytes.as_deref(), namespace, startup_mcp_servers)
                .map_err(|err| {
                    format!(
                        "Failed to prepare Antigravity MCP config {}: {err}",
                        path.display()
                    )
                })?;
        write_bytes_atomically(&path, &merged).map_err(|err| {
            format!(
                "Failed to write Antigravity MCP config {}: {err}",
                path.display()
            )
        })?;
        Ok(Self {
            path,
            original_bytes,
            restored: false,
            _mutex_guard: mutex_guard,
        })
    }

    fn restore(mut self) -> Result<(), String> {
        self.restore_inner().map_err(|err| {
            format!(
                "Failed to restore Antigravity MCP config {}: {err}",
                self.path.display()
            )
        })?;
        self.restored = true;
        Ok(())
    }

    fn restore_inner(&self) -> Result<(), String> {
        match &self.original_bytes {
            Some(bytes) => write_bytes_atomically(&self.path, bytes),
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err.to_string()),
            },
        }
    }
}

impl Drop for AntigravityMcpConfigGuard {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if let Err(err) = self.restore_inner() {
            tracing::error!(
                path = %self.path.display(),
                error = %err,
                "failed to restore Antigravity MCP config"
            );
        }
    }
}

fn merge_antigravity_mcp_config(
    original_bytes: Option<&[u8]>,
    namespace: &str,
    startup_mcp_servers: &[StartupMcpServer],
) -> Result<Vec<u8>, String> {
    let mut value = match original_bytes {
        Some(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => json!({}),
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .map_err(|err| format!("existing mcp_config.json is malformed: {err}"))?,
        None => json!({}),
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| "existing mcp_config.json must be a JSON object".to_string())?;
    if !object.contains_key("mcpServers") {
        object.insert("mcpServers".to_string(), Value::Object(Map::new()));
    }
    let servers = object
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "existing mcp_config.json mcpServers must be a JSON object".to_string())?;

    for server in startup_mcp_servers {
        let Some(config) = antigravity_mcp_server_config(server) else {
            continue;
        };
        let key = antigravity_mcp_server_key(namespace, &server.name);
        if servers.contains_key(&key) {
            return Err(format!(
                "Tyde MCP server key {key:?} already exists in Antigravity MCP config"
            ));
        }
        servers.insert(key, config);
    }

    serde_json::to_vec_pretty(&value)
        .map_err(|err| format!("failed to serialize merged mcp_config.json: {err}"))
}

fn antigravity_mcp_server_key(namespace: &str, server_name: &str) -> String {
    format!(
        "tyde_{}_{}",
        sanitize_mcp_key_component(namespace),
        sanitize_mcp_key_component(server_name)
    )
}

fn sanitize_mcp_key_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn antigravity_mcp_server_config(server: &StartupMcpServer) -> Option<Value> {
    let name = server.name.trim();
    if name.is_empty() {
        return None;
    }
    match &server.transport {
        StartupMcpTransport::Stdio { command, args, env } => {
            build_stdio_mcp_config(command, args, env)
        }
        StartupMcpTransport::Http { url, headers, .. } => build_http_mcp_config(url, headers),
    }
}

fn build_stdio_mcp_config(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Option<Value> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let mut cfg = Map::new();
    cfg.insert("command".to_string(), Value::String(command.to_string()));
    cfg.insert(
        "args".to_string(),
        to_value(args).expect("Vec<String> is always serializable"),
    );
    if !env.is_empty() {
        cfg.insert(
            "env".to_string(),
            to_value(env).expect("HashMap<String, String> is always serializable"),
        );
    }
    Some(Value::Object(cfg))
}

fn build_http_mcp_config(url: &str, headers: &HashMap<String, String>) -> Option<Value> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let mut cfg = Map::new();
    cfg.insert("serverUrl".to_string(), Value::String(url.to_string()));
    if !headers.is_empty() {
        cfg.insert(
            "headers".to_string(),
            to_value(headers).expect("HashMap<String, String> is always serializable"),
        );
    }
    Some(Value::Object(cfg))
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "Antigravity MCP config path has no parent: {}",
            path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create MCP config directory: {err}"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "Antigravity MCP config path has no file name: {}",
                path.display()
            )
        })?;
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", now_ms()));
    let mut file = fs::File::create(&tmp_path)
        .map_err(|err| format!("failed to create temp MCP config file: {err}"))?;
    file.write_all(bytes)
        .map_err(|err| format!("failed to write temp MCP config file: {err}"))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync temp MCP config file: {err}"))?;
    fs::rename(&tmp_path, path)
        .map_err(|err| format!("failed to atomically replace MCP config file: {err}"))?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_args_include_timeout_model_roots_and_unrestricted_permission() {
        let log_file = Path::new("/tmp/tyde-agy.log");
        let args = antigravity_cli_args(
            BackendAccessMode::Unrestricted,
            ANTIGRAVITY_DEFAULT_MODEL,
            &["/extra/one".to_string(), "/extra/two".to_string()],
            None,
            log_file,
            "hello",
        );

        assert_eq!(
            args,
            vec![
                "--print-timeout",
                "5m",
                "--log-file",
                "/tmp/tyde-agy.log",
                "--dangerously-skip-permissions",
                "--model",
                "Gemini 3.5 Flash (Medium)",
                "--add-dir",
                "/extra/one",
                "--add-dir",
                "/extra/two",
                "-p",
                "hello",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--conversation"));
        assert!(!args.iter().any(|arg| arg == "--continue"));
    }

    #[test]
    fn cli_args_resume_uses_exact_conversation_id() {
        let session_id = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string());
        let args = antigravity_cli_args(
            BackendAccessMode::Unrestricted,
            ANTIGRAVITY_DEFAULT_MODEL,
            &[],
            Some(&session_id),
            Path::new("/tmp/tyde-agy-resume.log"),
            "follow up",
        );

        assert!(args.iter().any(|arg| arg == "--log-file"));
        assert!(
            args.iter()
                .any(|arg| arg == "--conversation=55a3c5e1-a2e1-44c1-9246-6e3de751803d"),
            "resume args must use exact --conversation=<UUID>: {args:?}"
        );
        assert!(!args.iter().any(|arg| arg == "--continue" || arg == "-c"));
    }

    #[test]
    fn known_model_schema_defaults_to_medium_non_nullable_select() {
        let schema = AntigravityBackend::session_settings_schema();
        assert_eq!(schema.backend_kind, BackendKind::Antigravity);
        let field = schema.fields.first().expect("model field");
        let SessionSettingFieldType::Select {
            options,
            default,
            nullable,
        } = &field.field_type
        else {
            panic!("expected select field");
        };
        assert_eq!(default.as_deref(), Some(ANTIGRAVITY_DEFAULT_MODEL));
        assert!(!nullable);
        assert!(
            options
                .iter()
                .any(|option| option.value == ANTIGRAVITY_LOW_MODEL)
        );
        assert!(
            options
                .iter()
                .any(|option| option.value == ANTIGRAVITY_HIGH_MODEL)
        );
    }

    #[test]
    fn cost_hint_defaults_use_exact_agy_labels() {
        assert_eq!(
            antigravity_cost_hint_defaults(SpawnCostHint::Low)
                .0
                .get("model"),
            Some(&SessionSettingValue::String(
                ANTIGRAVITY_LOW_MODEL.to_string()
            ))
        );
        assert_eq!(
            antigravity_cost_hint_defaults(SpawnCostHint::Medium)
                .0
                .get("model"),
            Some(&SessionSettingValue::String(
                ANTIGRAVITY_DEFAULT_MODEL.to_string()
            ))
        );
        assert_eq!(
            antigravity_cost_hint_defaults(SpawnCostHint::High)
                .0
                .get("model"),
            Some(&SessionSettingValue::String(
                ANTIGRAVITY_HIGH_MODEL.to_string()
            ))
        );
    }

    #[test]
    fn native_session_ids_are_exact_agy_conversation_uuids() {
        assert!(is_antigravity_native_session_id(&SessionId(
            "55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string()
        )));
        assert!(!is_antigravity_native_session_id(&SessionId(
            "antigravity-55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string()
        )));
    }

    #[test]
    fn no_root_workspace_uses_tyde_owned_cwd_without_synthetic_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let no_root_cwd = dir.path().join("antigravity").join("no-root");
        let (primary, extra) =
            resolve_workspace_roots_with_no_root_cwd(&[], &no_root_cwd).expect("resolve no roots");

        assert_eq!(primary, no_root_cwd.to_string_lossy().to_string());
        assert!(extra.is_empty());
        assert!(no_root_cwd.is_dir());
    }

    #[test]
    fn workspace_resolver_rejects_ssh_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve_workspace_roots_with_no_root_cwd(
            &["ssh://example.com/repo".to_string()],
            dir.path(),
        )
        .expect_err("ssh roots must be rejected");

        assert!(err.contains("local workspace roots"));
    }

    #[test]
    fn log_parser_reads_created_and_active_conversation_ids() {
        let created = parse_antigravity_conversation_ids(
            "I0609 server.go:753] Created conversation 55a3c5e1-a2e1-44c1-9246-6e3de751803d\n",
        );
        assert_eq!(
            created.created,
            Some(SessionId(
                "55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string()
            ))
        );
        assert_eq!(created.active, None);
        assert_eq!(created.authoritative_for_new_session(), created.created);

        let active = parse_antigravity_conversation_ids(
            "I0609 server.go:753] Created conversation 11111111-1111-4111-8111-111111111111\n\
             I0609 printmode.go:147] Print mode: conversation=55a3c5e1-a2e1-44c1-9246-6e3de751803d, sending message\n",
        );
        assert_eq!(
            active.created,
            Some(SessionId(
                "11111111-1111-4111-8111-111111111111".to_string()
            ))
        );
        assert_eq!(
            active.active,
            Some(SessionId(
                "55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string()
            ))
        );
        assert_eq!(active.authoritative_for_new_session(), active.active);
    }

    #[tokio::test]
    async fn log_watcher_waits_for_active_conversation_before_new_session_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_file = dir.path().join("agy.log");
        let created = SessionId("11111111-1111-4111-8111-111111111111".to_string());
        let active = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string());
        let inner = test_antigravity_inner(dir.path().to_string_lossy().to_string());
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let session_capture = SessionCapture::new(ready_tx);
        let watcher = start_antigravity_log_watcher(
            Arc::clone(&inner),
            log_file.clone(),
            None,
            Some(session_capture),
        );

        std::fs::write(
            &log_file,
            format!("I0609 server.go:753] Created conversation {created}\n"),
        )
        .expect("write created log");
        tokio::time::sleep(ANTIGRAVITY_LOG_POLL_INTERVAL * 3).await;

        assert!(
            !watcher.task.is_finished(),
            "Created conversation alone must not complete the watcher before log finalization"
        );
        assert!(matches!(
            ready_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_file)
            .expect("open log append");
        writeln!(
            file,
            "I0609 printmode.go:147] Print mode: conversation={active}, sending message"
        )
        .expect("append active log");

        let captured = tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("capture should complete")
            .expect("capture sender")
            .expect("capture should succeed");
        assert_eq!(captured, active);

        let result = stop_antigravity_log_watcher(watcher).await;
        assert_eq!(result.ids.created, Some(created));
        assert_eq!(result.ids.active, Some(active.clone()));
        assert!(result.session_capture.is_none());
        let state = inner.state.lock().await;
        assert_eq!(state.session_id, Some(active));
    }

    #[test]
    fn expected_conversation_finalization_uses_active_not_created_id() {
        let expected = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string());
        let created = SessionId("11111111-1111-4111-8111-111111111111".to_string());
        let outcome = finalize_expected_conversation(
            Path::new("/tmp/agy.log"),
            &expected,
            AntigravityLogWatchResult {
                ids: AntigravityConversationLogIds {
                    created: Some(created.clone()),
                    active: Some(expected.clone()),
                },
                session_capture: None,
            },
            TurnOutcome::Completed(AntigravityStdoutSummary::empty()),
        );
        assert!(matches!(outcome, TurnOutcome::Completed(_)));

        let failed = finalize_expected_conversation(
            Path::new("/tmp/agy.log"),
            &expected,
            AntigravityLogWatchResult {
                ids: AntigravityConversationLogIds {
                    created: Some(created),
                    active: None,
                },
                session_capture: None,
            },
            TurnOutcome::Completed(AntigravityStdoutSummary::empty()),
        );
        match failed {
            TurnOutcome::Failed { error, .. } => {
                assert!(error.contains("did not confirm exact conversation"));
            }
            _ => panic!("missing active conversation must fail completed resume turns"),
        }
    }

    #[tokio::test]
    async fn finalizing_new_session_without_uuid_preserves_cli_startup_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = test_antigravity_inner(dir.path().to_string_lossy().to_string());
        let (ready_tx, ready_rx) = oneshot::channel();
        let session_capture = SessionCapture::new(ready_tx);
        let error = "Authentication required. Please visit the URL to log in:".to_string();
        let outcome = TurnOutcome::Failed {
            summary: AntigravityStdoutSummary {
                stdout: error.clone(),
                streamed_text: String::new(),
                stream_started: false,
                blocked_error_prefix: true,
            },
            error: error.clone(),
        };
        let finalized = inner
            .finalize_new_conversation(
                Path::new("/tmp/agy-auth.log"),
                AntigravityLogWatchResult {
                    ids: AntigravityConversationLogIds::default(),
                    session_capture: Some(session_capture),
                },
                outcome,
                "",
            )
            .await;

        let ready_error = ready_rx
            .await
            .expect("ready sender")
            .expect_err("startup should fail");
        assert!(ready_error.contains("Authentication required"));
        match finalized {
            TurnOutcome::Failed { error, .. } => {
                assert!(error.contains("Authentication required"));
            }
            _ => panic!("startup auth failure must remain failed"),
        }
    }

    fn test_antigravity_inner(primary_root: String) -> Arc<AntigravityInner> {
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        Arc::new(AntigravityInner {
            events_tx,
            state: Mutex::new(AntigravityState {
                session_id: None,
                conversations_dir: std::env::temp_dir(),
                primary_root,
                extra_roots: Vec::new(),
                startup_mcp_servers: Vec::new(),
                combined_instructions: None,
                session_settings: SessionSettingsValues::default(),
                access_mode: BackendAccessMode::Unrestricted,
                active_turn: None,
                queued_sends: VecDeque::new(),
                closing: false,
            }),
        })
    }

    #[tokio::test]
    async fn busy_send_queues_instead_of_dropping() {
        let inner = test_antigravity_inner("/tmp".to_string());
        {
            let mut state = inner.state.lock().await;
            state.active_turn = Some(ActiveTurn {
                id: 3,
                cancel_tx: None,
            });
        }

        let prepared = inner
            .prepare_turn("queued while busy".to_string(), None)
            .await
            .expect("busy-time send must not error");
        assert!(
            prepared.is_none(),
            "busy-time send must be queued, not prepared into a turn"
        );

        let state = inner.state.lock().await;
        assert_eq!(state.queued_sends.len(), 1);
        assert_eq!(state.queued_sends[0].message, "queued while busy");
    }

    #[tokio::test]
    async fn queue_drain_continues_past_preparation_failures() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let mut bad_settings = SessionSettingsValues::default();
        bad_settings.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("not-a-real-agy-model".to_owned()),
        );
        let inner = Arc::new(AntigravityInner {
            events_tx,
            state: Mutex::new(AntigravityState {
                session_id: None,
                conversations_dir: std::env::temp_dir(),
                primary_root: "/tmp".to_string(),
                extra_roots: Vec::new(),
                startup_mcp_servers: Vec::new(),
                combined_instructions: None,
                session_settings: bad_settings,
                access_mode: BackendAccessMode::Unrestricted,
                active_turn: None,
                queued_sends: VecDeque::new(),
                closing: false,
            }),
        });
        {
            let mut state = inner.state.lock().await;
            state.queued_sends.push_back(QueuedSend {
                message: "first queued".to_owned(),
                session_capture: None,
            });
            state.queued_sends.push_back(QueuedSend {
                message: "second queued".to_owned(),
                session_capture: None,
            });
        }

        inner.start_next_queued_send().await;

        {
            let state = inner.state.lock().await;
            assert!(
                state.queued_sends.is_empty(),
                "a failing entry must not strand the rest of the queue"
            );
        }
        let mut errors = 0;
        while let Ok(event) = events_rx.try_recv() {
            if let ChatEvent::MessageAdded(message) = &event
                && matches!(message.sender, MessageSender::Error)
            {
                errors += 1;
            }
        }
        assert_eq!(errors, 2, "each failed queued entry must surface an error");
    }

    #[test]
    fn stream_end_without_upstream_usage_is_explicitly_unavailable() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let inner = AntigravityInner {
            events_tx,
            state: Mutex::new(AntigravityState {
                session_id: None,
                conversations_dir: std::env::temp_dir(),
                primary_root: "/tmp".to_string(),
                extra_roots: Vec::new(),
                startup_mcp_servers: Vec::new(),
                combined_instructions: None,
                session_settings: SessionSettingsValues::default(),
                access_mode: BackendAccessMode::Unrestricted,
                active_turn: None,
                queued_sends: VecDeque::new(),
                closing: false,
            }),
        };

        inner.emit_stream_end("message-1", "done".to_string(), Some("agy-model"));

        let event = events_rx.try_recv().expect("StreamEnd event");
        let ChatEvent::StreamEnd(data) = event else {
            panic!("expected StreamEnd, got {event:?}");
        };
        let usage = data.message.token_usage.expect("explicit token usage");
        assert!(matches!(
            usage.turn,
            protocol::TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            }
        ));
        assert!(matches!(
            usage.cumulative,
            protocol::TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            }
        ));
    }

    #[test]
    fn read_only_no_longer_produces_read_only_refusal() {
        // ReadOnly is advisory for `agy`; keep asserting the arg builder no
        // longer surfaces the old "no enforceable read-only mode" error.
        let log_file = Path::new("/tmp/tyde-agy-readonly.log");
        let args = antigravity_cli_args(
            BackendAccessMode::ReadOnly,
            ANTIGRAVITY_DEFAULT_MODEL,
            &[],
            None,
            log_file,
            "hello",
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("no enforceable read-only mode")),
            "read-only args must not carry the old refusal error: {args:?}"
        );
    }

    #[tokio::test]
    async fn resume_rejects_legacy_synthetic_ids_and_fork_remains_unsupported() {
        assert!(
            AntigravityBackend::list_sessions()
                .await
                .expect("list sessions")
                .is_empty()
        );
        let resume = match AntigravityBackend::resume(
            Vec::new(),
            BackendSpawnConfig::default(),
            SessionId("antigravity-old".to_string()),
        )
        .await
        {
            Ok(_) => panic!("legacy synthetic resume must fail"),
            Err(err) => err,
        };
        assert!(resume.contains("native agy conversation UUID"));

        let fork = match AntigravityBackend::fork(
            Vec::new(),
            BackendSpawnConfig::default(),
            SessionId("antigravity-old".to_string()),
            protocol::SendMessagePayload {
                message: "hello".to_string(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .await
        {
            Ok(_) => panic!("fork unsupported"),
            Err(err) => err,
        };
        assert_eq!(fork.code, protocol::AgentErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn stdout_auth_error_fails_startup_capture_before_process_completion() {
        for error_prefix in [
            "Authentication required. Please visit the URL to log in:\n",
            "Error: authentication timed out.\n",
        ] {
            let (events_tx, mut events_rx) = mpsc::unbounded_channel();
            let (ready_tx, ready_rx) = oneshot::channel();
            let mut state = AntigravityStdoutState::new(
                events_tx,
                "msg-startup-error".to_string(),
                ANTIGRAVITY_DEFAULT_MODEL.to_string(),
                Some(SessionCapture::new(ready_tx)),
            );

            state.consume_chunk(error_prefix);

            let ready_error = tokio::time::timeout(Duration::from_secs(1), ready_rx)
                .await
                .expect("startup capture should resolve immediately")
                .expect("startup capture sender")
                .expect_err("startup capture should fail");
            assert!(
                ready_error.contains(error_prefix.trim()),
                "startup capture must report the real Antigravity stdout error, got {ready_error:?}"
            );
            assert!(
                events_rx.try_recv().is_err(),
                "auth/error stdout must not start an assistant stream"
            );
        }
    }

    #[tokio::test]
    async fn stdout_startup_error_waits_for_detail_after_bare_error_prefix() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let mut state = AntigravityStdoutState::new(
            events_tx,
            "msg-startup-error-split".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            Some(SessionCapture::new(ready_tx)),
        );

        state.consume_chunk("Error:");

        assert!(matches!(
            ready_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            events_rx.try_recv().is_err(),
            "bare error prefix must not start an assistant stream"
        );

        state.consume_chunk(" authentication timed out...\n");

        let ready_error = tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup capture should resolve after error detail")
            .expect("startup capture sender")
            .expect_err("startup capture should fail");
        assert!(
            ready_error.contains("Error: authentication timed out..."),
            "startup capture must wait for and report the error detail, got {ready_error:?}"
        );
        assert!(
            events_rx.try_recv().is_err(),
            "split error stdout must not start an assistant stream"
        );
    }

    #[test]
    fn stdout_state_streams_plain_text_and_blocks_auth_errors() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AntigravityStdoutState::new(
            tx,
            "msg-1".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            None,
        );
        state.consume_chunk("hello");
        state.consume_chunk(" world");
        let summary = state.finish();
        assert!(summary.stream_started);
        assert_eq!(summary.streamed_text, "hello world");
        assert!(matches!(
            rx.try_recv().expect("start"),
            ChatEvent::StreamStart(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("delta"),
            ChatEvent::StreamDelta(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("delta"),
            ChatEvent::StreamDelta(_)
        ));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AntigravityStdoutState::new(
            tx,
            "msg-2".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            None,
        );
        state.consume_chunk("Authentication required. Please visit the URL to log in:\n");
        let summary = state.finish();
        assert!(!summary.stream_started);
        assert!(
            summary
                .error_message()
                .expect("auth error")
                .contains("Authentication required")
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stdout_state_waits_for_partial_error_prefixes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AntigravityStdoutState::new(
            tx,
            "msg-auth".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            None,
        );
        state.consume_chunk("Authenticat");
        assert!(rx.try_recv().is_err());
        state.consume_chunk("ion required. Please visit the URL to log in:\n");
        let summary = state.finish();
        assert!(!summary.stream_started);
        assert!(summary.blocked_error_prefix);
        assert!(
            summary
                .error_message()
                .expect("auth error")
                .contains("Authentication required")
        );
        assert!(rx.try_recv().is_err());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AntigravityStdoutState::new(
            tx,
            "msg-error".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            None,
        );
        state.consume_chunk("Err");
        assert!(rx.try_recv().is_err());
        state.consume_chunk("or: failed before model output\n");
        let summary = state.finish();
        assert!(!summary.stream_started);
        assert!(summary.blocked_error_prefix);
        assert!(
            summary
                .error_message()
                .expect("error")
                .contains("Error: failed")
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stdout_state_streams_when_partial_prefix_is_disambiguated_or_ends() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AntigravityStdoutState::new(
            tx,
            "msg-author".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            None,
        );
        state.consume_chunk("Auth");
        assert!(rx.try_recv().is_err());
        state.consume_chunk("or reply");
        let summary = state.finish();
        assert!(summary.stream_started);
        assert_eq!(summary.streamed_text, "Author reply");
        assert!(matches!(
            rx.try_recv().expect("start"),
            ChatEvent::StreamStart(_)
        ));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AntigravityStdoutState::new(
            tx,
            "msg-short".to_string(),
            ANTIGRAVITY_DEFAULT_MODEL.to_string(),
            None,
        );
        state.consume_chunk("Err");
        assert!(rx.try_recv().is_err());
        let summary = state.finish();
        assert!(summary.stream_started);
        assert_eq!(summary.streamed_text, "Err");
        assert!(matches!(
            rx.try_recv().expect("start"),
            ChatEvent::StreamStart(_)
        ));
    }

    #[test]
    fn mcp_config_merge_uses_namespaced_keys_and_antigravity_shapes() {
        let namespace = "antigravity-abc-123";
        let servers = vec![
            StartupMcpServer {
                name: "docs".to_string(),
                transport: StartupMcpTransport::Stdio {
                    command: "/bin/echo".to_string(),
                    args: vec!["hi".to_string()],
                    env: HashMap::from([("A".to_string(), "B".to_string())]),
                },
            },
            StartupMcpServer {
                name: "web".to_string(),
                transport: StartupMcpTransport::Http {
                    url: "http://127.0.0.1:1/mcp".to_string(),
                    headers: HashMap::from([("X".to_string(), "Y".to_string())]),
                    bearer_token_env_var: None,
                },
            },
        ];
        let merged = merge_antigravity_mcp_config(
            Some(br#"{"mcpServers":{"user_server":{"command":"user"}},"x":1}"#),
            namespace,
            &servers,
        )
        .expect("merge");
        let value: Value = serde_json::from_slice(&merged).expect("json");
        let mcp_servers = value
            .get("mcpServers")
            .and_then(Value::as_object)
            .expect("servers");
        assert!(mcp_servers.contains_key("user_server"));
        let docs = mcp_servers
            .get("tyde_antigravity_abc_123_docs")
            .expect("docs server");
        assert_eq!(
            docs.get("command"),
            Some(&Value::String("/bin/echo".to_string()))
        );
        assert_eq!(docs.get("args"), Some(&json!(["hi"])));
        let web = mcp_servers
            .get("tyde_antigravity_abc_123_web")
            .expect("web server");
        assert_eq!(
            web.get("serverUrl"),
            Some(&Value::String("http://127.0.0.1:1/mcp".to_string()))
        );
        assert!(web.get("url").is_none());
    }

    #[test]
    fn malformed_mcp_config_fails_before_overwrite() {
        let err = merge_antigravity_mcp_config(
            Some(b"not json"),
            "antigravity-1",
            &[StartupMcpServer {
                name: "docs".to_string(),
                transport: StartupMcpTransport::Stdio {
                    command: "/bin/echo".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
            }],
        )
        .expect_err("malformed config must fail");
        assert!(err.contains("malformed"));
    }

    #[test]
    fn empty_and_whitespace_mcp_config_merge_as_empty_object() {
        for original in [b"".as_slice(), b"   \n\t".as_slice()] {
            let merged = merge_antigravity_mcp_config(
                Some(original),
                "antigravity-empty",
                &[StartupMcpServer {
                    name: "docs".to_string(),
                    transport: StartupMcpTransport::Stdio {
                        command: "/bin/echo".to_string(),
                        args: Vec::new(),
                        env: HashMap::new(),
                    },
                }],
            )
            .expect("empty config should merge");
            let value: Value = serde_json::from_slice(&merged).expect("merged json");
            let mcp_servers = value
                .get("mcpServers")
                .and_then(Value::as_object)
                .expect("mcpServers object");
            assert!(mcp_servers.contains_key("tyde_antigravity_empty_docs"));
        }
    }

    #[tokio::test]
    async fn mcp_guard_restore_restores_exact_original_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp_config.json");
        let original = br#"{"mcpServers":{"user":{"command":"user"}},"format":[1,2,3]}"#;
        std::fs::write(&path, original).expect("write original config");
        let mutex_guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
        let guard = AntigravityMcpConfigGuard::install(
            path.clone(),
            "antigravity-restore",
            &[StartupMcpServer {
                name: "docs".to_string(),
                transport: StartupMcpTransport::Stdio {
                    command: "/bin/echo".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
            }],
            mutex_guard,
        )
        .expect("install MCP config");

        assert_ne!(
            std::fs::read(&path).expect("read merged config"),
            original,
            "install should write merged config before restore"
        );
        guard.restore().expect("restore MCP config");
        assert_eq!(
            std::fs::read(&path).expect("read restored config"),
            original,
            "restore must preserve exact original bytes"
        );
    }

    #[tokio::test]
    async fn mcp_guard_restores_empty_original_config_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp_config.json");
        let original = b" \n\t";
        std::fs::write(&path, original).expect("write empty config");
        let mutex_guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
        let guard = AntigravityMcpConfigGuard::install(
            path.clone(),
            "antigravity-empty-restore",
            &[StartupMcpServer {
                name: "docs".to_string(),
                transport: StartupMcpTransport::Stdio {
                    command: "/bin/echo".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
            }],
            mutex_guard,
        )
        .expect("install MCP config over empty original");

        let merged = std::fs::read_to_string(&path).expect("read merged config");
        assert!(
            merged.contains("tyde_antigravity_empty_restore_docs"),
            "merged config should contain temporary Tyde MCP server: {merged}"
        );
        guard.restore().expect("restore MCP config");
        assert_eq!(
            std::fs::read(&path).expect("read restored config"),
            original,
            "restore must preserve exact whitespace-only original bytes"
        );
    }

    #[tokio::test]
    async fn mcp_guard_restore_failure_is_returned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp_config.json");
        let mutex_guard = ANTIGRAVITY_MCP_CONFIG_MUTEX.lock().await;
        let guard = AntigravityMcpConfigGuard::install(
            path.clone(),
            "antigravity-restore-fail",
            &[StartupMcpServer {
                name: "docs".to_string(),
                transport: StartupMcpTransport::Stdio {
                    command: "/bin/echo".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
            }],
            mutex_guard,
        )
        .expect("install MCP config");

        std::fs::remove_file(&path).expect("remove merged config");
        std::fs::create_dir(&path).expect("replace config file with directory");
        let err = guard
            .restore()
            .expect_err("restore failure must be returned");
        assert!(
            err.contains("Failed to restore Antigravity MCP config"),
            "unexpected restore error: {err}"
        );
        std::fs::remove_dir_all(&path).expect("clean failed restore directory");
    }

    #[test]
    fn cli_args_read_only_uses_skip_permissions_without_sandbox() {
        let args = antigravity_cli_args(
            BackendAccessMode::ReadOnly,
            ANTIGRAVITY_DEFAULT_MODEL,
            &["/extra/one".to_string()],
            None,
            Path::new("/tmp/tyde-agy-ro.log"),
            "hello",
        );
        assert!(
            args.iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "read-only args must allow non-interactive build/test commands: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == "--sandbox"),
            "read-only args must not enable the hard sandbox: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--gemini_dir" || arg.starts_with("--gemini_dir")),
            "read-only args must not relocate the gemini dir: {args:?}"
        );
    }

    #[test]
    fn cli_args_unrestricted_skips_permissions_without_sandbox() {
        let args = antigravity_cli_args(
            BackendAccessMode::Unrestricted,
            ANTIGRAVITY_DEFAULT_MODEL,
            &["/extra/one".to_string()],
            None,
            Path::new("/tmp/tyde-agy-rw.log"),
            "hello",
        );
        assert!(
            args.iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "unrestricted args must skip permissions: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == "--sandbox"),
            "unrestricted args must not enable the sandbox (combining --sandbox \
             with --dangerously-skip-permissions bypasses the sandbox): {args:?}"
        );
    }
}
