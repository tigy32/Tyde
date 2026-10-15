use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use base64::Engine as _;
use notify::Watcher;
use protocol::{
    AgentInput, AskUserQuestion, AskUserQuestionOption, BackendKind, ChatEvent,
    CurrentContextUsage, GoalCapabilities, GoalControl, GoalStatus, ImageData, MessageTokenUsage,
    ModelInfo, ModelRequestId, ModelRequestTokenUsage, ModelTurnId, NativeGoal, NativeToolCatalog,
    ReasoningData, SendMessagePayload, SendMessageToolResponse, SessionId, SessionSettingValue,
    SessionSettingsValues, SubAgentProgress, SubAgentProgressStatus, Task, TaskList, TaskStatus,
    TokenUsage, TokenUsageScope, ToolExecutionMode, ToolProgressData, ToolProgressUpdate,
    ToolRequestType, ToolUseData, UserQuestionMode,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{
    command_id,
    log::{Entry, NativeLog},
    tools,
    transport::{Connection, Incoming},
    wire::{self, *},
};
use crate::backend::{
    Backend, BackendEvent, CancelBackgroundTaskOutcome, SendOutcome, SteerOutcome, SubAgentEmitter,
    SubAgentHandle, compaction,
    turn_emitter::{AssistantMessagePayload, ResponseHandle, StreamEndPayload, TurnEmitter},
};

pub(super) enum Command {
    Send(AgentInput, oneshot::Sender<SendOutcome>),
    Steer(SendMessagePayload, oneshot::Sender<SteerOutcome>),
    Interrupt(oneshot::Sender<bool>),
    Settings(oneshot::Sender<Result<SessionSettingsValues, String>>),
    SetSettings(SessionSettingsValues, oneshot::Sender<Result<(), String>>),
    Catalog(oneshot::Sender<Option<NativeToolCatalog>>),
    CancelTask(String, oneshot::Sender<CancelBackgroundTaskOutcome>),
    Compact(
        compaction::BackendCompactionRequest,
        oneshot::Sender<compaction::BackendCompactionStart>,
    ),
}

struct CompactionInstall {
    install_id: String,
    metrics: protocol::CompactionMetrics,
}

struct PendingCompaction {
    request: compaction::BackendCompactionRequest,
    command_id: String,
    run_stream: Option<StreamRef>,
    installs: HashMap<String, CompactionInstall>,
    dispatch: compaction::BackendCompactionDispatchState,
    terminal: oneshot::Sender<compaction::BackendCompactionResult>,
}

#[derive(Clone)]
struct NativeChildSource {
    parent_run_id: String,
    objective: String,
    role: String,
    child_session_id: Option<String>,
    parent_call_id: Option<String>,
    live: bool,
    registered: bool,
}

enum NativeSourceUpdate {
    Changed(String),
    Failed,
}

struct NativeReplaySnapshot {
    cursor: String,
    last_turn: Option<LastTurn>,
}

struct NativeChild {
    actor: Box<Actor>,
    handle: SubAgentHandle,
    events: mpsc::UnboundedReceiver<BackendEvent>,
    parent_call_id: String,
    name: String,
    tool_calls: u64,
    last_tool_name: Option<String>,
    watcher: notify::RecommendedWatcher,
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        if self
            .watcher
            .unwatch(std::path::Path::new(&self.actor.session.path))
            .is_err()
        {
            tracing::debug!("Muse native child source watch was already closed");
        }
    }
}

struct Response {
    run_id: String,
    provider_id: String,
    handle: Option<ResponseHandle>,
    content: String,
    reasoning: String,
    declarations: Vec<ToolUseData>,
    usage: Option<MessageTokenUsage>,
    model: Option<ModelInfo>,
}

struct OpenTool {
    name: String,
    args: Value,
    request: Option<ToolRequestType>,
    task_id: Option<String>,
    background: bool,
    mcp_text: Option<String>,
    mcp_images: Vec<ModelVisibleContent>,
    mcp_completion: Option<Item>,
    mcp_sealed: bool,
}

#[derive(Default)]
struct Surface {
    text: String,
    committed: bool,
}

pub(super) struct Actor {
    connection: Connection,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<BackendEvent>,
    emitter: TurnEmitter,
    emitted: mpsc::UnboundedReceiver<Value>,
    session: wire::Session,
    history: Option<History>,
    resume_head: String,
    cwd: String,
    instructions: Option<String>,
    settings: SessionSettingsValues,
    models: Vec<Model>,
    subagents: Option<Arc<dyn SubAgentEmitter>>,
    active_turn: Option<String>,
    current_goal: Option<NativeGoal>,
    pending_compaction: Option<PendingCompaction>,
    command_results: HashMap<String, CommandResult>,
    command_tasks: HashSet<String>,
    background_tasks: HashSet<String>,
    response: Option<Response>,
    tools: HashMap<String, OpenTool>,
    undeclared_items: HashMap<String, Vec<Item>>,
    surfaces: HashMap<String, Surface>,
    revisions: HashMap<String, u64>,
    echoed_commands: HashSet<String>,
    user_images: HashMap<String, Vec<ImageData>>,
    pending_questions: HashMap<String, NativeQuestion>,
    question_outcomes: HashMap<String, QuestionSettled>,
    question_completions: HashMap<String, Item>,
    catalog: Option<NativeToolCatalog>,
    last_sequence: u64,
    fork_context_through: u64,
    last_position: Option<Position>,
    raw_stream: Option<StreamRef>,
    log: Option<NativeLog>,
    request_sequence: u32,
    usage_turn: Option<String>,
    turn_usage: TokenUsage,
    cumulative_usage: TokenUsage,
    replaying: bool,
    outer_session_id: String,
    native_runs: HashSet<String>,
    child_sources: HashMap<String, NativeChildSource>,
    children: HashMap<String, NativeChild>,
    source_updates: mpsc::UnboundedReceiver<NativeSourceUpdate>,
    source_update_tx: mpsc::UnboundedSender<NativeSourceUpdate>,
    view_cursor: Option<String>,
    last_terminal: Option<Terminal>,
    last_completed_turn: Option<LastTurn>,
    native_replay_snapshot: Option<NativeReplaySnapshot>,
    relay: Option<SubAgentHandle>,
}

impl Actor {
    pub fn new(
        connection: Connection,
        incoming: mpsc::UnboundedReceiver<Incoming>,
        commands: mpsc::UnboundedReceiver<Command>,
        events: mpsc::UnboundedSender<BackendEvent>,
        opened: SessionResult,
        models: Vec<Model>,
        config: crate::backend::BackendSpawnConfig,
    ) -> Result<Self, String> {
        let mut settings = super::MuseBackend::resolve_session_settings(&config);
        let cwd = opened
            .session
            .workspace_root
            .clone()
            .ok_or("Muse omitted its session working directory")?;
        let instructions =
            super::super::render_combined_spawn_instructions(&config.resolved_spawn_config);
        let subagents = config.subagent_emitter;
        let (tx, emitted) = mpsc::unbounded_channel();
        let (source_update_tx, source_updates) = mpsc::unbounded_channel();
        let outer_session_id = opened.session.session_id.clone();
        let selected = opened
            .session
            .model_id
            .as_ref()
            .and_then(|id| models.iter().find(|model| &model.model_id == id));
        if !settings.0.contains_key("reasoning_effort")
            && let Some(effort) = selected.and_then(|model| model.default_reasoning_effort.clone())
        {
            settings.0.insert(
                "reasoning_effort".to_owned(),
                SessionSettingValue::String(effort),
            );
        }
        Ok(Self {
            connection,
            incoming,
            commands,
            events,
            emitter: TurnEmitter::new(tx),
            emitted,
            session: opened.session,
            history: opened.history,
            resume_head: opened.view_cursor,
            cwd,
            instructions,
            settings,
            models,
            subagents,
            active_turn: None,
            current_goal: None,
            pending_compaction: None,
            command_results: HashMap::new(),
            command_tasks: HashSet::new(),
            background_tasks: HashSet::new(),
            response: None,
            tools: HashMap::new(),
            undeclared_items: HashMap::new(),
            surfaces: HashMap::new(),
            revisions: HashMap::new(),
            echoed_commands: HashSet::new(),
            user_images: HashMap::new(),
            pending_questions: HashMap::new(),
            question_outcomes: HashMap::new(),
            question_completions: HashMap::new(),
            catalog: None,
            last_sequence: 0,
            fork_context_through: 0,
            last_position: None,
            raw_stream: None,
            log: None,
            request_sequence: 0,
            usage_turn: None,
            turn_usage: TokenUsage::default(),
            cumulative_usage: TokenUsage::default(),
            replaying: false,
            outer_session_id,
            native_runs: HashSet::new(),
            child_sources: HashMap::new(),
            children: HashMap::new(),
            source_updates,
            source_update_tx,
            view_cursor: None,
            last_terminal: None,
            last_completed_turn: None,
            native_replay_snapshot: None,
            relay: None,
        })
    }

    pub async fn initialize(&mut self, resume: bool) -> Result<(), String> {
        let (log, stream, fork_context_through) = NativeLog::open(&self.session.path).await?;
        self.fork_context_through = fork_context_through;
        self.raw_stream = Some(stream.clone());
        self.log = Some(log);
        self.read_native_settings().await?;
        if resume {
            let mut history = BTreeMap::<u64, Vec<ViewEvent>>::new();
            let mut cursor = None::<String>;
            let mut replay_through = None::<Position>;
            let mut reached_head = false;
            loop {
                let mut params =
                    json!({"sessionId":self.session.session_id,"direction":"forward","limit":1000});
                if let Some(cursor) = &cursor {
                    params["cursor"] = json!(cursor);
                }
                let page: ViewPage = self.connection.typed("view/page", params).await?;
                for event in page.events {
                    if let Some(source) = event.params.get("sourceRange") {
                        let position: Position = parse(
                            source
                                .get("last")
                                .cloned()
                                .ok_or("Muse historical view omitted its source position")?,
                        )?;
                        if source.pointer("/stream/id").and_then(Value::as_str)
                            != Some(stream.id.as_str())
                        {
                            return Err("Muse history referenced another native stream".to_owned());
                        }
                        if replay_through
                            .as_ref()
                            .is_none_or(|previous| previous.sequence < position.sequence)
                        {
                            replay_through = Some(position);
                        }
                    }
                    let at_head = event.params.get("viewCursor").and_then(Value::as_str)
                        == Some(self.resume_head.as_str());
                    if matches!(
                        event.method.as_str(),
                        "item/completed" | "userInput/requested" | "userInput/settled"
                    ) {
                        let source = event
                            .params
                            .get("sourceRange")
                            .ok_or("Muse historical item omitted its native source range")?;
                        if source.pointer("/stream/id").and_then(Value::as_str)
                            != Some(stream.id.as_str())
                        {
                            return Err("Muse history referenced another native stream".to_owned());
                        }
                        let sequence = source
                            .pointer("/last/sequence")
                            .and_then(Value::as_u64)
                            .ok_or("Muse historical item omitted its source sequence")?;
                        if sequence > self.fork_context_through {
                            history.entry(sequence).or_default().push(event);
                        }
                    }
                    if at_head {
                        reached_head = true;
                        break;
                    }
                }
                if reached_head {
                    break;
                }
                match page.next_cursor {
                    Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
                    Some(_) => return Err("Muse view cursor did not advance".to_owned()),
                    None => break,
                }
            }
            if !reached_head {
                return Err(
                    "Muse paged history did not contain its acknowledged resume head".to_owned(),
                );
            }
            let replay_through =
                replay_through.ok_or("Muse resume head had no authoritative source position")?;
            self.replaying = true;
            while self.last_sequence < replay_through.sequence {
                let entry = self
                    .next_record()
                    .await?
                    .ok_or("Muse native log ended before its acknowledged resume head")?;
                self.consume_record(entry).await?;
                let ready = history
                    .range(..=self.last_sequence)
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>();
                for key in ready {
                    if let Some(events) = history.remove(&key) {
                        for event in events {
                            self.notification(&event.method, event.params).await?;
                        }
                    }
                }
                self.flush()?;
            }
            if !history.is_empty() {
                return Err(
                    "Muse view history extended beyond the authoritative raw-log replay".to_owned(),
                );
            }
            if self.response.is_some() {
                self.finish_response()?;
            }
            self.settle_mcp_results()?;
            self.replaying = false;
            if let Some(snapshot) = self
                .history
                .as_ref()
                .and_then(|history| history.snapshot.as_ref())
            {
                let goal = snapshot.state.goal.clone();
                let effort = snapshot
                    .state
                    .reasoning_effort
                    .as_ref()
                    .map(|effort| effort.reasoning_effort.clone());
                if let Some(effort) = effort
                    && !self.settings.0.contains_key("reasoning_effort")
                {
                    self.settings.0.insert(
                        "reasoning_effort".to_owned(),
                        SessionSettingValue::String(effort),
                    );
                }
                self.goal(goal)?;
            }
            self.flush()?;
            self.events
                .send(BackendEvent::ResumeReplayComplete(Ok(())))
                .map_err(|_| "Muse replay consumer closed".to_owned())?;
        }
        self.emitter.goal_capabilities(GoalCapabilities {
            set: true,
            pause: true,
            resume: true,
            clear: true,
        });
        if resume && self.session.status == SessionStatus::Running {
            self.active_turn = self.session.active_turn_id.clone();
            if self.active_turn.is_none() {
                return Err("Muse reported a running session without its active turn".to_owned());
            }
            self.emitter
                .typing_status_changed(self.pending_questions.is_empty());
        }
        self.flush()?;
        tracing::info!(
            resume,
            history_records = self.last_sequence,
            "Muse native session binding prepared"
        );
        Ok(())
    }

    pub async fn run(mut self, initial: Option<SendMessagePayload>) {
        if let Some(initial) = initial
            && !matches!(
                self.send(AgentInput::SendMessage(initial)).await,
                SendOutcome::Accepted
            )
        {
            self.fail("Muse rejected the initial message");
            return;
        }
        loop {
            let result = tokio::select! {
                update=self.source_updates.recv()=>{
                    match update {
                        Some(NativeSourceUpdate::Changed(session)) => {
                            match self.refresh_owned_child(&session).await {
                                Ok(()) => self.settle_native_children().await,
                                Err(error) => Err(error),
                            }
                        }
                        Some(NativeSourceUpdate::Failed) => Err("Muse native child source watch failed".to_owned()),
                        None => Err("Muse native child source update channel closed".to_owned()),
                    }
                }
                input=self.commands.recv()=>{
                    let Some(command)=input else {break;};
                    match command {
                        Command::Send(input,reply)=>{let outcome=self.send(input).await;let _=reply.send(outcome);}
                        Command::Steer(input,reply)=>{let outcome=self.steer(input).await;let _=reply.send(outcome);}
                        Command::Interrupt(reply)=>{let outcome=self.interrupt().await;let _=reply.send(outcome);}
                        Command::Settings(reply)=>{
                            let settings = self.read_native_settings().await;
                            if let Err(error) = &settings { self.emitter.error_message(error); }
                            let _=reply.send(settings);
                        }
                        Command::SetSettings(settings,reply)=>{let outcome=self.set_settings(settings).await;let _=reply.send(outcome);}
                        Command::Catalog(reply)=>{let _=reply.send(self.catalog.clone());}
                        Command::CancelTask(id,reply)=>{let outcome=self.cancel_task(&id).await;let _=reply.send(outcome);}
                        Command::Compact(request,reply)=>{let outcome=self.begin_compaction(request).await;let _=reply.send(outcome);}
                    }
                    Ok(())
                }
                event=self.incoming.recv()=>{
                    match event {
                        Some(Incoming::Notification{method,params})=>self.notification(&method,params).await,
                        Some(Incoming::Failed(message))=>Err(message),
                        None=>break,
                    }
                }
            };
            if let Err(error) = result.and_then(|()| self.flush()) {
                self.fail(&error);
                break;
            }
        }
        self.fail_compaction(
            compaction::BackendCompactionFailureKind::TransportClosed,
            "Muse event transport closed during compaction",
        );
        self.connection.shutdown();
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<(), String> {
        if params
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != self.session.session_id)
        {
            return Ok(());
        }
        if let Some(range) = params.get("sourceRange") {
            let stream: StreamRef = parse(
                range
                    .get("stream")
                    .cloned()
                    .ok_or("Muse source range omitted its stream")?,
            )?;
            let last: Position = parse(
                range
                    .get("last")
                    .cloned()
                    .ok_or("Muse source range omitted its final position")?,
            )?;
            if self
                .raw_stream
                .as_ref()
                .is_none_or(|expected| expected.id != stream.id)
            {
                return Err(
                    "Muse view event referenced a different authoritative stream".to_owned(),
                );
            }
            self.read_through(last).await?;
        }
        if method == "item/started" && params.get("sourceRange").is_none() {
            self.read_tail().await?;
        }
        match method {
            "log/gap" | "view/gap"=>return Err("Muse reported a protocol gap; this session cannot continue with incomplete native events".to_owned()),
            "turn/started"=>{
                let turn:TurnEvent=parse(params)?;
                if turn.session_id!=self.session.session_id{return Ok(());}
                tracing::debug!(already_active = self.active_turn.is_some(), same_native_turn = self.active_turn.as_ref() == Some(&turn.turn_id), "Muse observed native turn start");
                self.last_terminal=None;
                self.active_turn=Some(turn.turn_id);
                if !self.replaying {self.emitter.typing_status_changed(true);}
            }
            "turn/completed"=>{
                let turn:TurnEvent=parse(params)?;
                if turn.session_id!=self.session.session_id{return Ok(());}
                tracing::debug!(matches_active = self.active_turn.as_ref() == Some(&turn.turn_id), response_open = self.response.is_some(), "Muse observed native turn completion");
                if !self.replaying && self.active_turn.as_ref().is_some_and(|active| active != &turn.turn_id) {
                    tracing::debug!("Muse retained a prior turn terminal without settling the active turn");
                    return Ok(());
                }
                self.settle_mcp_results()?;
                if self.response.is_some(){self.finish_response()?;}
                let sessions=self.children.keys().cloned().collect::<Vec<_>>();
                for session in sessions {self.refresh_owned_child(&session).await?;}
                let terminal=turn.terminal.ok_or("Muse turn completion omitted its terminal")?;
                self.last_terminal=Some(terminal);
                self.last_completed_turn=Some(LastTurn{turn_id:turn.turn_id.clone(),terminal});
                match terminal {
                    Terminal::Cancelled=>{
                        self.emitter.cancel_pending_foreground_tools("Muse interrupted the native turn");
                        self.emitter.operation_cancelled("Muse interrupted the native turn");
                    }
                    Terminal::Failed=>{
                        let error=turn.error.ok_or("Muse failed turn omitted its typed failure")?;
                        self.emitter.error_message(&format!("Muse native turn failed: {} (retryable: {})",error.kind,error.retryable));
                        self.emitter.cancel_pending_foreground_tools("Muse native turn failed");
                    }
                    Terminal::Completed=>{}
                }
                if self.active_turn.as_ref()==Some(&turn.turn_id){self.active_turn=None;if !self.replaying {self.emitter.typing_status_changed(false);}}
            }
            "item/started" | "item/updated" | "item/completed"=>{
                let event:ItemEvent=parse(params)?;
                if event.session_id==self.session.session_id {self.item(event.item).await?;}
            }
            "item/delta"=>{
                let delta:Delta=parse(params)?;
                if delta.session_id==self.session.session_id {self.delta(delta)?;}
            }
            "session/modelChanged"=>{
                if params.get("sessionId").and_then(Value::as_str)==Some(self.session.session_id.as_str()) {
                    let model=params.get("modelId").and_then(Value::as_str).ok_or("Muse model change omitted its native identity")?;
                    let is_default = self.models.iter().any(|entry| entry.is_default && entry.model_id == model);
                    if !(is_default && self.settings.0.get("model") == Some(&SessionSettingValue::Null)) { self.settings.0.insert("model".to_owned(),SessionSettingValue::String(model.to_owned())); }
                }
            }
            "session/reasoningEffortChanged"=>{
                if params.get("sessionId").and_then(Value::as_str)==Some(self.session.session_id.as_str()) {
                    let effort=params.get("reasoningEffort").and_then(Value::as_str).ok_or("Muse reasoning change omitted its native tier")?;
                    let is_default = self.models.iter().any(|entry| match self.settings.0.get("model") { Some(SessionSettingValue::String(model)) => entry.model_id == *model, Some(SessionSettingValue::Null) => entry.is_default, _ => false } && entry.default_reasoning_effort.as_deref() == Some(effort));
                    if !(is_default && self.settings.0.get("reasoning_effort") == Some(&SessionSettingValue::Null)) { self.settings.0.insert("reasoning_effort".to_owned(),SessionSettingValue::String(effort.to_owned())); }
                }
            }
            "session/todoListChanged"=>{
                #[derive(serde::Deserialize)]struct Todo{items:Vec<TodoEntry>}
                #[derive(serde::Deserialize)]struct TodoEntry{text:String,status:String}
                let todo:Todo=parse(params)?;
                let tasks=todo.items.into_iter().enumerate().map(|(index,item)|Ok(Task{id:index as u64+1,description:item.text,status:match item.status.as_str(){"pending"=>TaskStatus::Pending,"inProgress"|"in_progress"=>TaskStatus::InProgress,"completed"=>TaskStatus::Completed,"cancelled"=>TaskStatus::Failed,_=>return Err("Muse todo carried an unsupported native status".to_owned())}})).collect::<Result<Vec<_>,String>>()?;
                self.emitter.task_update(&TaskList{title:"Tasks".to_owned(),tasks});
            }
            "session/goalChanged"=>{let goal:GoalEvent=parse(params)?;self.goal(goal.goal)?;}
            "userInput/requested"=>{
                let question:NativeQuestion=parse(params)?;
                let call=question.tool_call_id.clone();
                let questions=question.questions.iter().map(|question|AskUserQuestion{id:Some(protocol::UserQuestionId(question.id.clone())),question:question.question.clone(),header:Some(question.header.clone()),options:question.options.iter().map(|option|AskUserQuestionOption{label:option.label.clone(),description:option.description.clone()}).collect(),answer_schema:Some(protocol::UserQuestionAnswerSchema{min_selections:question.selection.min_selections,max_selections:question.selection.max_selections,max_free_text_chars:Some(500),max_note_chars:Some(500)}),multi_select:question.selection.mode==SelectionMode::Multiple}).collect();
                let request=ToolRequestType::AskUserQuestion{questions,mode:UserQuestionMode::Blocking};
                let open=self.tools.get_mut(&call).ok_or("Muse question was not declared by a native response")?;
                if open.request.is_some(){return Err("Muse presented one question twice".to_owned());}
                open.request=Some(request.clone());self.emitter.tool_request(&call,request);
                self.pending_questions.insert(call,question);
                self.emitter.typing_status_changed(false);
            }
            "userInput/settled"=>{
                let settled: QuestionSettled = parse(params)?;
                let call = self.pending_questions.iter().find(|(_, question)| question.user_input_id == settled.user_input_id).map(|(call, _)| call.clone()).ok_or("Muse settled an unknown user prompt")?;
                self.pending_questions.remove(&call);
                self.question_outcomes.insert(call.clone(), settled);
                if let Some(item) = self.question_completions.remove(&call) {
                    self.complete_question(&call, item).await?;
                }
            }
            "approval/requested"=>{
                let choices=params.get("availableChoices").and_then(Value::as_array).ok_or("Muse approval omitted its native choices")?;
                let deny=choices.iter().find(|choice|choice.get("decision").and_then(Value::as_str)==Some("denied")).and_then(|choice|choice.get("choiceId")).and_then(Value::as_str).ok_or("Muse approval offered no rejection choice")?;
                self.connection.request("approval/decide",json!({"commandId":command_id(),"sessionId":self.session.session_id,"approvalId":params.get("approvalId"),"currentRequirementId":params.get("currentRequirementId"),"choiceId":deny})).await.map_err(|error|error.to_string())?;
                self.emitter.warning_message("Muse requested an approval that this adapter cannot present; the native action was denied");
            }
            "turn/retryScheduled"=>{
                let attempt=params.get("attempt").and_then(Value::as_u64).ok_or("Muse retry omitted its attempt")?;
                let max=params.get("maxAttempts").and_then(Value::as_u64).ok_or("Muse retry omitted its attempt bound")?;
                let delay=params.get("retryDelayMs").and_then(Value::as_u64).ok_or("Muse retry omitted its delay")?;
                self.emitter.retry_attempt(crate::backend::turn_emitter::RetryAttemptPayload{attempt,max_retries:max,error:"Muse scheduled a native model retry",backoff_ms:delay});
            }
            "usage/changed"=>self.capacity(params)?,
            "session/contextUsage" | "session/tokenUsage" | "turn/foregroundCompleted" |
            "session/started" | "session/closed" | "session/statusChanged" | "session/branchChanged" |
            "approval/resolved" | "approval/updated" | "userInput/engaged" |
            "session/nameChanged" | "skill/changed" | "plugin/changed" | "session/listChanged"=>{}
            _=>tracing::debug!(method, "Muse notification has no Tyde projection"),
        }
        self.settle_native_children().await?;
        Ok(())
    }

    async fn next_record(&mut self) -> Result<Option<Entry>, String> {
        self.log
            .as_mut()
            .ok_or("Muse native log was not opened")?
            .next()
            .await
    }

    async fn consume_record(&mut self, entry: Entry) -> Result<(), String> {
        let (stream, position) = match &entry {
            Entry::Record(record) => (
                &record.stream,
                Position {
                    id: record.id.clone(),
                    sequence: record.sequence,
                },
            ),
            Entry::Omitted { stream, position } => (stream, position.clone()),
        };
        if self
            .raw_stream
            .as_ref()
            .is_none_or(|expected| expected.id != stream.id || expected.kind != stream.kind)
        {
            return Err("Muse storage record belonged to another authoritative stream".to_owned());
        }
        if position.sequence != self.last_sequence + 1 {
            return Err(format!(
                "Muse native record sequence gap: expected {}, observed {}",
                self.last_sequence + 1,
                position.sequence
            ));
        }
        self.last_sequence = position.sequence;
        self.last_position = Some(position);
        if self.last_sequence <= self.fork_context_through {
            return Ok(());
        }
        match entry {
            Entry::Record(record) => self.record(record).await,
            Entry::Omitted { .. } => Ok(()),
        }
    }

    async fn read_tail(&mut self) -> Result<(), String> {
        while let Some(entry) = self.next_record().await? {
            self.consume_record(entry).await?;
        }
        Ok(())
    }

    async fn read_through(&mut self, target: Position) -> Result<(), String> {
        while self.last_sequence < target.sequence {
            let entry = self.next_record().await?.ok_or(
                "Muse view referenced native records unavailable from its authoritative log",
            )?;
            self.consume_record(entry).await?;
        }
        if self.last_sequence == target.sequence
            && self
                .last_position
                .as_ref()
                .is_none_or(|position| position.id != target.id)
        {
            return Err(
                "Muse view source position disagreed with the authoritative record identity"
                    .to_owned(),
            );
        }
        Ok(())
    }

    async fn record(&mut self, record: RawRecord) -> Result<(), String> {
        if record.schema_version != 1 {
            return Err(
                "Muse changed its native record schema; transcript projection cannot continue"
                    .to_owned(),
            );
        }
        if matches!(
            record.payload_type.as_str(),
            "subagent.control.spawn_accepted" | "subagent.control.start_attested"
        ) {
            if record.payload_schema_version != 1 {
                return Err("Muse changed its consumed native-child ownership schema".to_owned());
            }
            let control: SubagentControl = parse(record.payload)?;
            match control.record {
                SubagentRecord::SpawnAccepted {
                    subagent_id,
                    parent_run_id,
                    objective,
                    role,
                } => {
                    if self
                        .child_sources
                        .insert(
                            subagent_id,
                            NativeChildSource {
                                parent_run_id,
                                objective,
                                role,
                                child_session_id: None,
                                parent_call_id: None,
                                live: !self.replaying,
                                registered: false,
                            },
                        )
                        .is_some()
                    {
                        return Err("Muse reused its native child ownership identity".to_owned());
                    }
                }
                SubagentRecord::StartAttested {
                    subagent_id,
                    subagent_session_id,
                    dispatched_via_tool_call_id,
                } => {
                    let source = self
                        .child_sources
                        .get_mut(&subagent_id)
                        .ok_or("Muse attested a child without its accepted owner")?;
                    match (&source.child_session_id, &source.parent_call_id) {
                        (None, None) => {
                            source.child_session_id = Some(subagent_session_id);
                            source.parent_call_id = Some(dispatched_via_tool_call_id);
                        }
                        (Some(session), Some(call))
                            if session == &subagent_session_id
                                && call == &dispatched_via_tool_call_id =>
                        {
                            // Native followups attest another run under the original spawn owner.
                            tracing::debug!(
                                registered = source.registered,
                                "Muse retained its native child owner across a followup attestation"
                            );
                        }
                        _ => {
                            return Err(
                                "Muse native child start attestation changed its established owner"
                                    .to_owned(),
                            );
                        }
                    }
                }
                SubagentRecord::Other => {
                    return Err("Muse native-child ownership record changed kind".to_owned());
                }
            }
            return Ok(());
        }
        match record.payload_type.as_str() {
            "context.compact.manual.target" | "context.compact.manual.outcome" => {
                if record.payload_schema_version != 1 {
                    return Err("Muse changed its consumed manual-compaction schema".to_owned());
                }
                if record.payload_type == "context.compact.manual.target" {
                    let target: ManualCompactionTarget = parse(record.payload)?;
                    if let Some(pending) = &mut self.pending_compaction
                        && pending.command_id == target.command_id
                    {
                        if target.run_stream.kind != "run" || pending.run_stream.is_some() {
                            return Err(
                                "Muse manual compaction did not retain one explicit run target"
                                    .to_owned(),
                            );
                        }
                        pending.run_stream = Some(target.run_stream);
                    }
                } else {
                    let outcome: ManualCompactionOutcome = parse(record.payload)?;
                    self.complete_compaction(outcome)?;
                }
                return Ok(());
            }
            _ => {}
        }
        if record.payload_type == "runtime.command_intake.received" {
            let intake: CommandIntake = parse(record.payload)?;
            let received = intake.record;
            if let IntakeCommand::TurnQueueSubmit { payload } = received.command {
                if record.payload_schema_version != 1 {
                    return Err("Muse changed its consumed input-intake schema".to_owned());
                }
                if self
                    .raw_stream
                    .as_ref()
                    .is_none_or(|stream| stream.id != received.session_stream.id)
                {
                    return Err(
                        "Muse image intake belongs to a different native session stream".to_owned(),
                    );
                }
                let mut images = Vec::new();
                for attachment in payload.attachments.into_iter().flatten() {
                    let InputAttachment::Image {
                        base64_data,
                        media_type,
                    } = attachment;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&base64_data)
                        .map_err(|_| "Muse retained user image used invalid base64".to_owned())?;
                    if bytes.is_empty() || !media_type.starts_with("image/") {
                        return Err(
                            "Muse retained user image omitted its pixels or image media type"
                                .to_owned(),
                        );
                    }
                    images.push(ImageData {
                        media_type,
                        data: base64_data,
                    });
                }
                if !images.is_empty() && !self.echoed_commands.contains(&received.command_id) {
                    tracing::debug!(
                        images = images.len(),
                        "Muse retained user image intake for its native command"
                    );
                    if let Some(previous) =
                        self.user_images.insert(received.command_id, images.clone())
                        && previous != images
                    {
                        return Err(
                            "Muse retained inconsistent image intake for the same command"
                                .to_owned(),
                        );
                    }
                }
            }
            return Ok(());
        }
        if record.payload_type != "runtime.session" {
            return Ok(());
        }
        let runtime: RuntimeEvent = parse(record.payload)?;
        // Native approvals use v3 and are projected through stable MSP events.
        // Only the run/task ledger shapes consumed below are pinned to v1.
        if !matches!(runtime, RuntimeEvent::Other) && record.payload_schema_version != 1 {
            return Err("Muse changed a consumed run/task payload schema; transcript projection cannot continue".to_owned());
        }
        let (run_id, event) = match runtime {
            RuntimeEvent::Run { run_id, event } => (run_id, event),
            RuntimeEvent::Task {
                task_id,
                event: TaskEvent::Proposed { task_kind },
            } => {
                if task_kind == "tool.bash" {
                    self.command_tasks.insert(task_id);
                }
                return Ok(());
            }
            RuntimeEvent::Task {
                task_id,
                event: TaskEvent::Output { chunk },
            } => {
                if !self.command_tasks.contains(&task_id) {
                    return Ok(());
                }
                if !chunk.starts_with('{') {
                    return Ok(());
                }
                let content: Value = serde_json::from_str(&chunk)
                    .map_err(|_| "Muse task output was not valid native JSON".to_owned())?;
                if content.get("exit_code").is_some()
                    && content.get("command").is_some()
                    && content.get("terminal_status").is_some()
                {
                    let result: CommandResult = parse(content)?;
                    self.command_results.insert(task_id, result);
                } else if let Some(state) = content.get("execution_state") {
                    let BashExecutionState::BackgroundRunning = parse(state.clone())?;
                    // Bash yields can remain background work without an MSP background flag.
                    if !self.replaying
                        && self.relay.is_none()
                        && !self.native_background_tasks().await?.contains(&task_id)
                    {
                        return Err("Muse yielded bash work that was absent from its running background inventory".to_owned());
                    }
                    tracing::debug!(
                        replaying = self.replaying,
                        "Muse observed a native running background bash task"
                    );
                    self.mark_background_task(&task_id)?;
                }
                return Ok(());
            }
            RuntimeEvent::Task {
                event: TaskEvent::Other,
                ..
            }
            | RuntimeEvent::Other => return Ok(()),
        };
        self.native_runs.insert(run_id.clone());
        if self.usage_turn.as_ref() != Some(&run_id) {
            self.usage_turn = Some(run_id.clone());
            self.turn_usage = TokenUsage::default();
            self.request_sequence = 0;
        }
        match event {
            RunEvent::Terminal { terminal } => {
                tracing::debug!(
                    replaying = self.replaying,
                    response_open = self.response.is_some(),
                    matches_response_run = self
                        .response
                        .as_ref()
                        .is_some_and(|response| response.run_id == run_id),
                    cancelled = terminal == Terminal::Cancelled,
                    failed = terminal == Terminal::Failed,
                    native_sequence = self.last_sequence,
                    "Muse observed an attested native run terminal"
                );
                if let Some(response) = &self.response {
                    if response.run_id != run_id {
                        return Err(
                            "Muse run terminal did not own the open provider response".to_owned()
                        );
                    }
                    self.finish_response()?;
                }
            }
            RunEvent::ContextCompactionInstalled {
                candidate_id,
                install_id,
                budget_before,
                budget_after,
            } => {
                if let Some(pending) = &mut self.pending_compaction {
                    let target = pending
                        .run_stream
                        .as_ref()
                        .ok_or("Muse installed compaction before retaining its run target")?;
                    if target.id == run_id {
                        if pending.installs.len() >= 64
                            || install_id.is_empty()
                            || candidate_id.is_empty()
                        {
                            return Err("Muse compaction installation exceeded its identity/admission bounds".to_owned());
                        }
                        let install = CompactionInstall {
                            install_id,
                            metrics: protocol::CompactionMetrics {
                                before_tokens: Some(budget_before.estimated_prompt_tokens),
                                after_tokens: Some(budget_after.estimated_prompt_tokens),
                                ..Default::default()
                            },
                        };
                        if pending.installs.insert(candidate_id, install).is_some() {
                            return Err(
                                "Muse retained multiple installs for one compaction candidate"
                                    .to_owned(),
                            );
                        }
                    }
                }
            }
            RunEvent::ModelRequestConfigured { toolset } => {
                if !self.replaying {
                    let named_selectors = match toolset.mode {
                        ToolsetMode::Named => toolset.active_tools.as_slice(),
                        ToolsetMode::All | ToolsetMode::Other => &[],
                    };
                    self.catalog = Some(NativeToolCatalog {
                        backend_kind: BackendKind::Muse,
                        session_id: SessionId(self.session.session_id.clone()),
                        tools: toolset
                            .active_tools
                            .iter()
                            .map(|name| tools::descriptor(name.clone(), named_selectors))
                            .collect(),
                    });
                    tracing::debug!(
                        tools = self.catalog.as_ref().map(|catalog| catalog.tools.len()),
                        "Muse reported its effective native tool catalog"
                    );
                }
            }
            RunEvent::ModelResponseCreated { response_id } => {
                self.settle_mcp_results()?;
                tracing::debug!(
                    replaying = self.replaying,
                    response_open = self.response.is_some(),
                    same_response = self
                        .response
                        .as_ref()
                        .is_some_and(|response| response.provider_id == response_id),
                    same_response_run = self
                        .response
                        .as_ref()
                        .is_some_and(|response| response.run_id == run_id),
                    native_sequence = self.last_sequence,
                    "Muse observed native provider response creation"
                );
                if self.response.is_some() {
                    return Err(
                        "Muse opened a second provider response before settling the first"
                            .to_owned(),
                    );
                }
                self.request_sequence += 1;
                self.response = Some(Response {
                    run_id,
                    provider_id: response_id,
                    handle: if self.replaying {
                        None
                    } else {
                        Some(self.emitter.stream_start(None))
                    },
                    content: String::new(),
                    reasoning: String::new(),
                    declarations: Vec::new(),
                    usage: None,
                    model: None,
                });
            }
            RunEvent::ToolResultBatchCommitted { results } => {
                for result in results {
                    let tool = self
                        .tools
                        .get_mut(&result.tool_call_id)
                        .ok_or("Muse committed a result without its native declaration")?;
                    if tool.name.starts_with("mcp__")
                        && tool.mcp_text.replace(result.text).is_some()
                    {
                        return Err("Muse committed an MCP result more than once".to_owned());
                    }
                }
            }
            RunEvent::ToolResultModelVisibleContent { call_id, content } => {
                let tool = self
                    .tools
                    .get_mut(&call_id)
                    .ok_or("Muse rich tool content omitted its native declaration")?;
                if tool.name.starts_with("mcp__") {
                    if tool.mcp_sealed {
                        return Err(
                            "Muse delivered MCP image content after its model-input boundary"
                                .to_owned(),
                        );
                    }
                    tracing::debug!(
                        images = content.len(),
                        "Muse retained model-visible MCP images"
                    );
                    tool.mcp_images.extend(content);
                }
            }
            RunEvent::AssistantMessageCommitted {
                message_id,
                response_id,
                text,
            } => {
                self.check_response(&response_id)?;
                let surface = self.surfaces.entry(message_id).or_default();
                if !text.starts_with(&surface.text) {
                    return Err(
                        "Muse committed assistant text that differed from its streamed surface"
                            .to_owned(),
                    );
                }
                let suffix = text[surface.text.len()..].to_owned();
                surface.text = text;
                surface.committed = true;
                let response = self
                    .response
                    .as_mut()
                    .ok_or("Muse message arrived without a provider response")?;
                response.content.push_str(&suffix);
                if let Some(handle) = &response.handle {
                    self.emitter.stream_delta(handle, &suffix);
                }
            }
            RunEvent::ReasoningSummaryCommitted {
                message_id,
                response_id,
                text,
            } => {
                self.check_response(&response_id)?;
                let surface = self.surfaces.entry(message_id).or_default();
                if !text.starts_with(&surface.text) {
                    return Err(
                        "Muse committed reasoning that differed from its streamed summary"
                            .to_owned(),
                    );
                }
                let suffix = text[surface.text.len()..].to_owned();
                surface.text = text;
                surface.committed = true;
                let response = self
                    .response
                    .as_mut()
                    .ok_or("Muse summary arrived without a provider response")?;
                response.reasoning.push_str(&suffix);
                if let Some(handle) = &response.handle {
                    self.emitter.stream_reasoning_delta(handle, &suffix);
                }
            }
            RunEvent::ReasoningCommitted { response_id, text } => {
                self.check_response(&response_id)?;
                if !text.is_empty() {
                    let response = self
                        .response
                        .as_mut()
                        .ok_or("Muse reasoning arrived without a provider response")?;
                    response.reasoning.push_str(&text);
                    if let Some(handle) = &response.handle {
                        self.emitter.stream_reasoning_delta(handle, &text);
                    }
                }
            }
            RunEvent::AssistantToolCallsCommitted {
                response_id,
                tool_calls,
            } => {
                self.check_response(&response_id)?;
                let mut declarations = Vec::new();
                let mut pending_items = Vec::new();
                for call in tool_calls {
                    if let Some(items) = self.undeclared_items.remove(&call.call_id) {
                        pending_items.extend(items);
                    }
                    if self.tools.contains_key(&call.call_id) {
                        return Err("Muse reused a provider tool-call identity".to_owned());
                    }
                    let args: Value = match serde_json::from_str(&call.args) {
                        Ok(args) => args,
                        Err(_) => {
                            tracing::debug!(
                                "Muse retained a malformed native argument declaration verbatim"
                            );
                            Value::String(call.args)
                        }
                    };
                    declarations.push(ToolUseData {
                        tool_call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: args.clone(),
                        content_offset: None,
                    });
                    self.tools.insert(
                        call.call_id,
                        OpenTool {
                            name: call.name,
                            args,
                            request: None,
                            task_id: None,
                            background: false,
                            mcp_text: None,
                            mcp_images: Vec::new(),
                            mcp_completion: None,
                            mcp_sealed: false,
                        },
                    );
                }
                self.response
                    .as_mut()
                    .ok_or("Muse declared tools without a provider response")?
                    .declarations = declarations;
                self.finish_response()?;
                for item in pending_items {
                    self.item(item).await?;
                }
            }
            RunEvent::ModelCompleted {
                model,
                finish_reason,
                usage,
            } => {
                let request = normalize_usage(&usage)?;
                add_usage(&mut self.turn_usage, &request);
                add_usage(&mut self.cumulative_usage, &request);
                let response = self
                    .response
                    .as_mut()
                    .ok_or("Muse completed a model request without its response identity")?;
                response.model = Some(ModelInfo {
                    model: model.clone(),
                });
                response.usage = Some(MessageTokenUsage {
                    request: TokenUsageScope::Known {
                        usage: Box::new(request.clone()),
                    },
                    turn: TokenUsageScope::Known {
                        usage: Box::new(self.turn_usage.clone()),
                    },
                    cumulative: TokenUsageScope::Known {
                        usage: Box::new(self.cumulative_usage.clone()),
                    },
                });
                if !self.replaying || self.relay.is_some() {
                    let window = self
                        .models
                        .iter()
                        .find(|entry| entry.model_id == model)
                        .and_then(|entry| entry.context_limit);
                    self.flush()?;
                    self.emit_backend_event(BackendEvent::ModelRequestTokenUsage(
                        ModelRequestTokenUsage {
                            request_id: ModelRequestId {
                                turn_id: ModelTurnId(run_id),
                                sequence: self.request_sequence,
                            },
                            request,
                            turn: self.turn_usage.clone(),
                            cumulative: self.cumulative_usage.clone(),
                            model_context_window: window,
                            current_context_usage: window.map(|context_window| {
                                CurrentContextUsage::Known {
                                    input_tokens: usage.input_tokens + usage.output_tokens,
                                    context_window,
                                }
                            }),
                            estimated_context_breakdown: None,
                        },
                    ))?;
                }
                if finish_reason.as_deref() != Some("tool_calls") {
                    self.finish_response()?;
                }
            }
            RunEvent::Started | RunEvent::Other => {}
        }
        Ok(())
    }

    fn native_owner_count(&self, source: &NativeChildSource) -> usize {
        let own = usize::from(
            self.native_runs.contains(&source.parent_run_id)
                && source
                    .parent_call_id
                    .as_ref()
                    .is_some_and(|call| self.tools.contains_key(call)),
        );
        own + self
            .children
            .values()
            .map(|child| child.actor.native_owner_count(source))
            .sum::<usize>()
    }

    async fn settle_native_children(&mut self) -> Result<(), String> {
        if self.replaying {
            return Ok(());
        }
        loop {
            let candidates = self
                .child_sources
                .iter()
                .filter(|(_, source)| {
                    source.live
                        && !source.registered
                        && source.child_session_id.is_some()
                        && source.parent_call_id.is_some()
                })
                .map(|(id, source)| (id.clone(), source.clone()))
                .collect::<Vec<_>>();
            let mut progress = false;
            for (id, source) in candidates {
                match self.native_owner_count(&source) {
                    0 => continue,
                    1 => {}
                    _ => {
                        return Err(
                            "Muse native child has multiple provider-call owners".to_owned()
                        );
                    }
                }
                Box::pin(self.register_native_child(&source)).await?;
                self.child_sources
                    .get_mut(&id)
                    .ok_or("Muse native child lost its accepted ownership record")?
                    .registered = true;
                progress = true;
            }
            if !progress {
                break;
            }
        }
        Ok(())
    }

    async fn register_native_child(&mut self, source: &NativeChildSource) -> Result<(), String> {
        let call = source
            .parent_call_id
            .as_ref()
            .ok_or("Muse native child omitted its attested provider call")?;
        if self.native_runs.contains(&source.parent_run_id) && self.tools.contains_key(call) {
            let session_id = source
                .child_session_id
                .as_ref()
                .ok_or("Muse native child omitted its bound session")?;
            if self.children.contains_key(session_id) {
                return Err("Muse native child session was bound more than once".to_owned());
            }
            let emitter = self
                .subagents
                .as_ref()
                .cloned()
                .ok_or("Muse native child has no Tyde relay emitter")?;
            let open = self
                .tools
                .get_mut(call)
                .ok_or("Muse native child lost its declared provider call")?;
            if open.name != "subagent_spawn" {
                return Err(
                    "Muse native child was dispatched by an unsupported native tool family"
                        .to_owned(),
                );
            }
            if open.request.is_none() {
                let request = tools::request(&open.name, &open.args, &self.cwd)?;
                open.request = Some(request.clone());
                self.emitter.tool_request(call, request);
            }
            self.flush()?;
            let mut opened: SessionResult = self
                .connection
                .typed(
                    "session/read",
                    json!({"sessionId":session_id,"excludeItems":true}),
                )
                .await?;
            if opened.session.session_id != *session_id
                || opened.session.kind != Some(SessionKind::Subagent)
                || opened.session.parent_session_id.as_deref()
                    != Some(self.outer_session_id.as_str())
                || opened.session.workspace_root.as_deref() != Some(self.cwd.as_str())
            {
                return Err(
                    "Muse native child metadata disagreed with its explicit owner attestation"
                        .to_owned(),
                );
            }
            let snapshot = NativeReplaySnapshot {
                cursor: opened.view_cursor.clone(),
                last_turn: opened.last_turn.take(),
            };
            let handle = emitter
                .on_subagent_spawned(
                    call.clone(),
                    source.role.clone(),
                    source.objective.clone(),
                    "muse_native".to_owned(),
                    Some(SessionId(session_id.clone())),
                )
                .await?;
            let (events_tx, events) = mpsc::unbounded_channel();
            let (_, incoming) = mpsc::unbounded_channel();
            let (_, commands) = mpsc::unbounded_channel();
            let config = crate::backend::BackendSpawnConfig {
                subagent_emitter: handle.subagent_emitter.clone(),
                ..Default::default()
            };
            let mut actor = Actor::new(
                self.connection.clone(),
                incoming,
                commands,
                events_tx,
                opened,
                self.models.clone(),
                config,
            )?;
            actor.relay = Some(handle.clone());
            actor.outer_session_id = self.outer_session_id.clone();
            actor.replaying = true;
            actor.native_replay_snapshot = Some(snapshot);
            actor.source_update_tx = self.source_update_tx.clone();
            let (log, stream, fork_context_through) = NativeLog::open(&actor.session.path).await?;
            actor.log = Some(log);
            actor.raw_stream = Some(stream);
            actor.fork_context_through = fork_context_through;
            let updates = self.source_update_tx.clone();
            let observed_session = session_id.clone();
            let observed_path = std::path::PathBuf::from(&actor.session.path);
            let exact_path = observed_path.clone();
            let mut watcher =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    match event {
                        Ok(event) if event.paths.iter().any(|path| path == &exact_path) => {
                            let _ =
                                updates.send(NativeSourceUpdate::Changed(observed_session.clone()));
                        }
                        Ok(_) => {}
                        Err(_) => {
                            let _ = updates.send(NativeSourceUpdate::Failed);
                        }
                    }
                })
                .map_err(|_| "Muse native child source watch could not be created".to_owned())?;
            watcher
                .watch(&observed_path, notify::RecursiveMode::NonRecursive)
                .map_err(|_| "Muse native child source watch could not be attached".to_owned())?;
            Box::pin(actor.refresh_native_view()).await?;
            self.children.insert(
                session_id.clone(),
                NativeChild {
                    actor: Box::new(actor),
                    handle,
                    events,
                    parent_call_id: call.clone(),
                    name: source.role.clone(),
                    tool_calls: 0,
                    last_tool_name: None,
                    watcher,
                },
            );
            self.forward_native_child(session_id)?;
            tracing::debug!(
                owned_children = self.children.len(),
                "Muse native child relay registered under its actual provider-call owner"
            );
            return Ok(());
        }
        for child in self.children.values_mut() {
            if child.actor.native_owner_count(source) == 1 {
                Box::pin(child.actor.register_native_child(source)).await?;
                child.actor.flush()?;
                return Ok(());
            }
        }
        Err("Muse native child provider-call owner disappeared during registration".to_owned())
    }

    async fn refresh_native_view(&mut self) -> Result<(), String> {
        let mut cursor = self.view_cursor.clone();
        loop {
            let mut params =
                json!({"sessionId":self.session.session_id,"direction":"forward","limit":1000});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page: ViewPage = self.connection.typed("view/page", params).await?;
            for event in page.events {
                let next = event
                    .params
                    .get("viewCursor")
                    .and_then(Value::as_str)
                    .ok_or("Muse child view event omitted its opaque cursor")?
                    .to_owned();
                let at_snapshot = self
                    .native_replay_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.cursor == next);
                Box::pin(self.notification(&event.method, event.params)).await?;
                if at_snapshot {
                    self.apply_native_child_snapshot()?;
                }
                self.view_cursor = Some(next);
            }
            match page.next_cursor {
                Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
                Some(_) => {
                    return Err("Muse native child view paging cursor did not advance".to_owned());
                }
                None => break,
            }
        }
        if self.native_replay_snapshot.is_some() {
            return Err(
                "Muse native child view did not reach its acknowledged snapshot head".to_owned(),
            );
        }
        self.flush()
    }

    fn apply_native_child_snapshot(&mut self) -> Result<(), String> {
        let snapshot = self
            .native_replay_snapshot
            .take()
            .ok_or("Muse native child lost its snapshot barrier")?;
        if let Some(last_turn) = &snapshot.last_turn
            && self.last_completed_turn.as_ref().is_none_or(|completed| {
                completed.terminal != last_turn.terminal || completed.turn_id != last_turn.turn_id
            })
        {
            return Err(
                "Muse native child terminal snapshot disagreed with its retained completed turn"
                    .to_owned(),
            );
        }
        if self.last_terminal.is_some() && (self.active_turn.is_some() || self.response.is_some()) {
            return Err(
                "Muse native child retained unfinished work after its current turn terminal"
                    .to_owned(),
            );
        }
        tracing::debug!(
            historical_terminal_present = snapshot.last_turn.is_some(),
            current_terminal_present = self.last_terminal.is_some(),
            active_turn_present = self.active_turn.is_some(),
            response_open = self.response.is_some(),
            "Muse native child reached its attested snapshot barrier"
        );
        self.replaying = false;
        self.flush()?;
        if self.last_terminal.is_some() {
            self.emit_backend_event(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)))?;
        } else {
            // Native starts, not historical completion metadata, own unfinished work.
            self.emitter.typing_status_changed(true);
            if let Some(response) = &mut self.response {
                let handle = self.emitter.stream_start(None);
                if !response.content.is_empty() {
                    self.emitter.stream_delta(&handle, &response.content);
                }
                if !response.reasoning.is_empty() {
                    self.emitter
                        .stream_reasoning_delta(&handle, &response.reasoning);
                }
                response.handle = Some(handle);
            }
        }
        Ok(())
    }

    async fn refresh_owned_child(&mut self, session: &str) -> Result<(), String> {
        if let Some(child) = self.children.get_mut(session) {
            Box::pin(child.actor.refresh_native_view()).await?;
            return self.forward_native_child(session);
        }
        for child in self.children.values_mut() {
            Box::pin(child.actor.refresh_owned_child(session)).await?;
            child.actor.flush()?;
        }
        let sessions = self.children.keys().cloned().collect::<Vec<_>>();
        for session in sessions {
            self.forward_native_child(&session)?;
        }
        Ok(())
    }

    fn forward_native_child(&mut self, session: &str) -> Result<(), String> {
        let child = self
            .children
            .get_mut(session)
            .ok_or("Muse child relay lost its explicitly bound session")?;
        let mut changed = false;
        while let Ok(event) = child.events.try_recv() {
            match event {
                BackendEvent::Chat(event) => {
                    if let ChatEvent::ToolRequest(request) = &event {
                        child.tool_calls = child
                            .tool_calls
                            .checked_add(1)
                            .ok_or("Muse native child tool count exceeded its domain")?;
                        child.last_tool_name = Some(
                            child
                                .actor
                                .tools
                                .get(&request.tool_call_id)
                                .ok_or(
                                    "Muse native child tool card omitted its provider declaration",
                                )?
                                .name
                                .clone(),
                        );
                    }
                    changed = true;
                }
                BackendEvent::ModelRequestTokenUsage(_) => {}
                BackendEvent::ResumeReplayComplete(_) => {}
                BackendEvent::Compaction(_) => {
                    return Err(
                        "Observed Muse child emitted an unrequested compaction operation"
                            .to_owned(),
                    );
                }
            }
        }
        if changed {
            let status = match child.actor.last_terminal {
                Some(Terminal::Completed) => SubAgentProgressStatus::Completed,
                Some(Terminal::Failed) => SubAgentProgressStatus::Failed,
                Some(Terminal::Cancelled) => SubAgentProgressStatus::Stopped,
                None => SubAgentProgressStatus::Running,
            };
            self.emitter.tool_progress(&ToolProgressData {
                tool_call_id: child.parent_call_id.clone(),
                execution_mode: ToolExecutionMode::Background,
                cancellable: false,
                update: ToolProgressUpdate::SubAgent(SubAgentProgress {
                    backend_kind: None,
                    agent_id: child.handle.agent_id.clone(),
                    agent_name: child.name.clone(),
                    last_tool_name: child.last_tool_name.clone(),
                    tool_calls: child.tool_calls,
                    completed: child.actor.last_terminal.is_some(),
                    status,
                }),
            });
        }
        Ok(())
    }

    fn check_response(&self, id: &str) -> Result<(), String> {
        match &self.response {
            Some(response) if response.provider_id == id => Ok(()),
            _ => Err("Muse content referenced a different provider response".to_owned()),
        }
    }

    fn finish_response(&mut self) -> Result<(), String> {
        let response = self
            .response
            .take()
            .ok_or("Muse completed a response that was not open")?;
        tracing::debug!(
            replaying = self.replaying,
            live_stream = response.handle.is_some(),
            content_chars = response.content.chars().count(),
            reasoning_chars = response.reasoning.chars().count(),
            tool_calls = response.declarations.len(),
            native_sequence = self.last_sequence,
            "Muse settled its native provider response"
        );
        let reasoning = (!response.reasoning.is_empty()).then_some(ReasoningData {
            text: response.reasoning,
            tokens: None,
            signature: None,
            blob: None,
        });
        if self.replaying {
            self.emitter
                .replay_assistant_message(AssistantMessagePayload {
                    message_id: None,
                    content: response.content,
                    reasoning,
                    tool_calls: response.declarations,
                    model_info: response.model,
                    token_usage: response.usage,
                    context_breakdown: None,
                    images: Vec::new(),
                });
        } else {
            let handle = response
                .handle
                .ok_or("Muse live response had no stream handle")?;
            self.emitter.stream_end(
                handle,
                StreamEndPayload {
                    content: response.content,
                    model_info: response.model,
                    token_usage: response.usage,
                    reasoning,
                    tool_calls: response.declarations,
                    ..Default::default()
                },
            );
        }
        Ok(())
    }

    fn delta(&mut self, delta: Delta) -> Result<(), String> {
        if delta.field.as_deref().is_none_or(|field| field == "text") {
            let surface = self.surfaces.entry(delta.item_id).or_default();
            if surface.committed {
                return Ok(());
            }
            let response = self
                .response
                .as_mut()
                .ok_or("Muse text delta arrived before its native model response was opened")?;
            surface.text.push_str(&delta.delta);
            response.content.push_str(&delta.delta);
            if let Some(handle) = &response.handle {
                self.emitter.stream_delta(handle, &delta.delta);
            }
        }
        Ok(())
    }

    async fn item(&mut self, item: Item) -> Result<(), String> {
        if item.kind == ItemKind::ToolCall {
            let call = item
                .call_id
                .as_ref()
                .ok_or("Muse tool item omitted its provider call identity")?;
            if !self.tools.contains_key(call) {
                // MSP view and raw-log subscriptions have independent delivery order.
                let pending = self.undeclared_items.entry(call.clone()).or_default();
                if pending.len() >= 1024
                    || pending
                        .last()
                        .is_some_and(|previous| previous.revision >= item.revision)
                {
                    return Err("Muse undeclared tool item revisions did not advance within the admission bound".to_owned());
                }
                tracing::debug!(
                    revision = item.revision,
                    pending_items = pending.len(),
                    "Muse view awaits its explicitly identified raw declaration"
                );
                pending.push(item);
                return Ok(());
            }
        }
        if self
            .revisions
            .get(&item.item_id)
            .is_some_and(|revision| *revision >= item.revision)
        {
            return Ok(());
        }
        self.revisions.insert(item.item_id.clone(), item.revision);
        match item.kind {
            ItemKind::UserMessage => {
                if let Some(command) = &item.command_id
                    && !self.echoed_commands.insert(command.clone())
                {
                    return Ok(());
                }
                let text = item
                    .display_text
                    .as_deref()
                    .or(item.text.as_deref())
                    .ok_or("Muse user-message item omitted its text")?;
                let images = item
                    .command_id
                    .as_ref()
                    .and_then(|command| self.user_images.remove(command));
                match &item.attachments {
                    Some(metadata) => {
                        let retained = images.as_deref().unwrap_or(&[]);
                        if metadata.len() != retained.len()
                            || metadata.iter().zip(retained).any(|(meta, image)| {
                                meta.kind != "image" || meta.media_type != image.media_type
                            })
                        {
                            return Err("Muse user-image metadata disagreed with its retained native intake".to_owned());
                        }
                    }
                    None if images.is_some() => {
                        return Err(
                            "Muse user-image item omitted its native attachment metadata"
                                .to_owned(),
                        );
                    }
                    None => {}
                }
                self.emitter.user_message(text, images, None);
            }
            ItemKind::ToolCall => {
                let call = item
                    .call_id
                    .as_ref()
                    .ok_or("Muse tool item omitted its provider call identity")?;
                let Some(open) = self.tools.get_mut(call) else {
                    return Err(
                        "Muse tool item was not declared by a native provider response".to_owned(),
                    );
                };
                if item.tool.as_deref() != Some(open.name.as_str()) {
                    return Err(
                        "Muse tool item disagreed with its native declaration name".to_owned()
                    );
                }
                open.task_id = Some(item.item_id.clone());
                let name = open.name.clone();
                let args = open.args.clone();
                let needs_request = open.request.is_none();
                if needs_request
                    && (name != "request_user_input"
                        || matches!(
                            item.status,
                            ItemStatus::Rejected
                                | ItemStatus::Failed
                                | ItemStatus::Cancelled
                                | ItemStatus::TimedOut
                        ))
                {
                    if matches!(name.as_str(), "write_file" | "edit_file")
                        && item.patch_ref.is_none()
                        && item.status == ItemStatus::InProgress
                    {
                        return Ok(());
                    }
                    let request = if !args.is_object() {
                        ToolRequestType::Other { args: args.clone() }
                    } else if matches!(name.as_str(), "write_file" | "edit_file") {
                        tools::edit_request(
                            &self.connection,
                            &self.session.session_id,
                            &item,
                            &args,
                            &self.cwd,
                        )
                        .await?
                    } else {
                        tools::request(&name, &args, &self.cwd)?
                    };
                    self.tools
                        .get_mut(call)
                        .ok_or("Muse tool disappeared during request normalization")?
                        .request = Some(request.clone());
                    self.emitter.tool_request(call, request);
                }
                let open = self
                    .tools
                    .get_mut(call)
                    .ok_or("Muse tool disappeared during item processing")?;
                if item.background == Some(true) {
                    open.background = true;
                }
                if self.background_tasks.contains(&item.item_id) {
                    open.background = true;
                }
                if open.request.is_some() && (open.background || item.status_line.is_some()) {
                    self.emitter.tool_progress(&ToolProgressData{tool_call_id:call.clone(),execution_mode:if open.background{ToolExecutionMode::Background}else{ToolExecutionMode::Foreground},cancellable:open.background && item.status == ItemStatus::InProgress && !self.replaying && self.relay.is_none(),update:ToolProgressUpdate::Other{payload:json!({"status":item.status_line,"output":item.visible_output})}});
                }
                if item.status != ItemStatus::InProgress {
                    let request = open
                        .request
                        .as_ref()
                        .ok_or("Muse completed a tool before exposing its executable request")?;
                    if matches!(request, ToolRequestType::AskUserQuestion { .. }) {
                        self.complete_question(call, item.clone()).await?;
                        return Ok(());
                    }
                    if name.starts_with("mcp__")
                        && matches!(item.status, ItemStatus::Completed | ItemStatus::Failed)
                    {
                        open.mcp_completion = Some(item.clone());
                        self.complete_mcp_result(call)?;
                        return Ok(());
                    }
                    let outcome = tools::outcome(
                        &self.connection,
                        &self.session.session_id,
                        &item,
                        request,
                        self.command_results.get(&item.item_id),
                        self.replaying,
                    )
                    .await?;
                    self.emitter.tool_completed(call, outcome);
                    open.task_id = None;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn settle_mcp_results(&mut self) -> Result<(), String> {
        let calls = self
            .tools
            .iter_mut()
            .filter_map(|(call, tool)| {
                if tool.mcp_text.is_some() {
                    tool.mcp_sealed = true;
                    Some(call.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for call in calls {
            self.complete_mcp_result(&call)?;
        }
        Ok(())
    }

    fn complete_mcp_result(&mut self, call: &str) -> Result<(), String> {
        let tool = self
            .tools
            .get_mut(call)
            .ok_or("Muse MCP result lost its native declaration")?;
        if !tool.mcp_sealed || tool.mcp_completion.is_none() {
            return Ok(());
        }
        let item = tool
            .mcp_completion
            .take()
            .ok_or("Muse MCP completion disappeared")?;
        let text = tool
            .mcp_text
            .take()
            .ok_or("Muse MCP completion omitted its committed result")?;
        let images = std::mem::take(&mut tool.mcp_images);
        let image_count = images.len();
        let outcome = tools::mcp_outcome(&item, text, images)?;
        tracing::debug!(
            images = image_count,
            "Muse settled its committed MCP result"
        );
        self.emitter.tool_completed(call, outcome);
        tool.task_id = None;
        Ok(())
    }

    async fn complete_question(&mut self, call: &str, item: Item) -> Result<(), String> {
        let Some(settlement) = self.question_outcomes.get(call) else {
            self.question_completions.insert(call.to_owned(), item);
            return Ok(());
        };
        let completion = match settlement.outcome {
            QuestionOutcome::Cancelled
            | QuestionOutcome::Interrupted
            | QuestionOutcome::Aborted => protocol::ToolExecutionOutcome::Cancelled {
                message: "Muse cancelled the native user prompt".to_owned(),
            },
            QuestionOutcome::TimedOut => protocol::ToolExecutionOutcome::Failed {
                message: "Muse user prompt timed out".to_owned(),
                details: None,
                normalization_failure: None,
                result: None,
            },
            QuestionOutcome::Answered => {
                let Some(ToolRequestType::AskUserQuestion { questions, .. }) =
                    self.tools.get(call).and_then(|tool| tool.request.as_ref())
                else {
                    return Err("Muse answered a question without its canonical request".to_owned());
                };
                let answers = settlement
                    .answers
                    .iter()
                    .map(NativeQuestionAnswer::to_protocol)
                    .collect::<Result<Vec<_>, _>>()?;
                if answers.len() != questions.len() {
                    return Err("Muse question settlement did not answer every question".to_owned());
                }
                for question in questions {
                    let matching = answers
                        .iter()
                        .filter(|answer| question.id.as_ref() == Some(&answer.question_id))
                        .collect::<Vec<_>>();
                    let [answer] = matching.as_slice() else {
                        return Err(
                            "Muse question settlement did not identify each question exactly once"
                                .to_owned(),
                        );
                    };
                    question.validate_structured_answer(answer)?;
                }
                protocol::ToolExecutionOutcome::Succeeded {
                    result: protocol::ToolExecutionResult::AskUserQuestion { answers },
                }
            }
            QuestionOutcome::Clarified => {
                let request = self
                    .tools
                    .get(call)
                    .and_then(|tool| tool.request.as_ref())
                    .ok_or("Muse question completion had no request")?;
                tools::outcome(
                    &self.connection,
                    &self.session.session_id,
                    &item,
                    request,
                    self.command_results.get(&item.item_id),
                    self.replaying,
                )
                .await?
            }
        };
        self.emitter.tool_completed(call, completion);
        self.tools
            .get_mut(call)
            .ok_or("Muse completed an unknown question tool")?
            .task_id = None;
        Ok(())
    }

    async fn send(&mut self, input: AgentInput) -> SendOutcome {
        match input {
            AgentInput::SendMessage(payload) => {
                if payload.tool_response.is_some() {
                    return match self.answer(payload).await {
                        Ok(()) => SendOutcome::Accepted,
                        Err(error) => {
                            self.emitter.error_message(&error);
                            SendOutcome::Closed
                        }
                    };
                }
                let goal_active = self
                    .current_goal
                    .as_ref()
                    .is_some_and(|goal| goal.status == GoalStatus::Active);
                tracing::debug!(
                    native_turn_active = self.active_turn.is_some(),
                    goal_active,
                    response_open = self.response.is_some(),
                    pending_questions = self.pending_questions.len(),
                    "Muse ordinary-input admission"
                );
                if self.active_turn.is_some() && !goal_active {
                    return SendOutcome::Busy(AgentInput::SendMessage(payload));
                }
                if self.pending_compaction.is_some() {
                    return SendOutcome::Busy(AgentInput::SendMessage(payload));
                }
                let id = command_id();
                let mut params = json!({"sessionId":self.session.session_id,"commandId":id,"input":input_parts(&payload, self.instructions.as_deref()),"displayText":payload.message});
                if goal_active {
                    params["ifBusy"] = json!("steer");
                }
                match self.connection.request("turn/start", params).await {
                    Ok(result) => {
                        let disposition = result.get("disposition").and_then(Value::as_str);
                        tracing::debug!(disposition, "Muse native input admission acknowledged");
                        if !matches!(disposition, Some("started" | "queued"))
                            && !(goal_active && disposition == Some("steered"))
                        {
                            self.emitter.error_message(
                                "Muse reported an unsupported input admission disposition",
                            );
                            return SendOutcome::Closed;
                        }
                        let Some(turn) = result.get("turnId").and_then(Value::as_str) else {
                            self.emitter
                                .error_message("Muse start omitted its active turn identity");
                            return SendOutcome::Closed;
                        };
                        if disposition != Some("queued") {
                            self.active_turn = Some(turn.to_owned());
                        }
                        self.echo(&id, &payload);
                        SendOutcome::Accepted
                    }
                    Err(error) => {
                        self.emitter
                            .error_message(&format!("Muse turn/start failed: {error}"));
                        SendOutcome::Closed
                    }
                }
            }
            AgentInput::SteerMessage(payload) => match self.steer(payload).await {
                SteerOutcome::Accepted => SendOutcome::Accepted,
                SteerOutcome::NoActiveTurn(payload) | SteerOutcome::Unsupported(payload) => {
                    SendOutcome::Busy(AgentInput::SteerMessage(payload))
                }
                SteerOutcome::Closed => SendOutcome::Closed,
            },
            AgentInput::GoalControl(control) => {
                let (method, objective) = match control {
                    GoalControl::Set { objective } => ("goal/set", Some(objective)),
                    GoalControl::Pause => ("goal/pause", None),
                    GoalControl::Resume => ("goal/resume", None),
                    GoalControl::Clear => ("goal/clear", None),
                };
                let mut params =
                    json!({"sessionId":self.session.session_id,"commandId":command_id()});
                if let Some(objective) = objective {
                    params["objective"] = json!(objective);
                }
                tracing::debug!(method, native_turn_active = self.active_turn.is_some(),
                    goal_status = ?self.current_goal.as_ref().map(|goal| goal.status),
                    "Muse native goal-control admission");
                if method == "goal/resume" {
                    let inspected = self
                        .connection
                        .request("session/read", json!({"sessionId":self.session.session_id}))
                        .await;
                    match inspected {
                        Ok(inspected) => tracing::debug!(
                            has_goal = inspected
                                .pointer("/history/snapshot/state/goal")
                                .is_some_and(|goal| !goal.is_null()),
                            native_goal_status = inspected
                                .pointer("/history/snapshot/state/goal/status")
                                .and_then(|value| value.as_str()),
                            history_mode = inspected
                                .pointer("/history/mode")
                                .and_then(|value| value.as_str()),
                            "Muse native goal-control source inspection"
                        ),
                        Err(error) => tracing::debug!(
                            code = error.code,
                            "Muse native goal-control source inspection failed"
                        ),
                    }
                }
                match self.connection.request(method, params).await {
                    Ok(_) => SendOutcome::Accepted,
                    Err(error) => {
                        tracing::debug!(
                            method,
                            code = error.code,
                            rejection_fields = ?error.data.as_object().map(|data| data.keys().collect::<Vec<_>>()),
                            "Muse native goal-control rejection"
                        );
                        self.emitter
                            .error_message(&format!("Muse native goal control failed: {error}"));
                        SendOutcome::Closed
                    }
                }
            }
            AgentInput::UpdateSessionSettings(update) => {
                match self.set_settings(update.values).await {
                    Ok(()) => SendOutcome::Accepted,
                    Err(error) => {
                        self.emitter.error_message(&error);
                        SendOutcome::Closed
                    }
                }
            }
            AgentInput::EditQueuedMessage(_)
            | AgentInput::CancelQueuedMessage(_)
            | AgentInput::SendQueuedMessageNow(_) => {
                self.emitter.error_message(
                    "Muse input queue is owned by the Tyde server, not the provider adapter",
                );
                SendOutcome::Closed
            }
        }
    }

    async fn steer(&mut self, payload: SendMessagePayload) -> SteerOutcome {
        let Some(turn) = &self.active_turn else {
            return SteerOutcome::NoActiveTurn(payload);
        };
        if payload.tool_response.is_some() {
            return SteerOutcome::Unsupported(payload);
        }
        let id = command_id();
        match self.connection.request("turn/steer",json!({"sessionId":self.session.session_id,"commandId":id,"expectedTurnId":turn,"input":input_parts(&payload, None)})).await {
            Ok(_)=>{self.echo(&id,&payload);SteerOutcome::Accepted}
            Err(error) if error.data.get("kind").and_then(Value::as_str)==Some("commandRejected")=>SteerOutcome::NoActiveTurn(payload),
            Err(error)=>{self.emitter.error_message(&format!("Muse steering failed: {error}"));SteerOutcome::Closed}
        }
    }

    fn echo(&mut self, id: &str, payload: &SendMessagePayload) {
        let first_echo = self.echoed_commands.insert(id.to_owned());
        let host_restart = payload.origin == Some(protocol::MessageOrigin::HostRestart);
        tracing::debug!(
            first_echo,
            host_restart,
            "Muse acknowledged input presentation"
        );
        if first_echo && !host_restart {
            self.emitter.user_message(
                &payload.message,
                payload.images.clone(),
                payload.origin.clone(),
            );
        }
    }

    async fn interrupt(&mut self) -> bool {
        let Some(turn) = &self.active_turn else {
            return true;
        };
        match self
            .connection
            .request(
                "turn/interrupt",
                json!({"sessionId":self.session.session_id,"commandId":command_id(),"turnId":turn}),
            )
            .await
        {
            Ok(_) => true,
            Err(error) => {
                self.emitter
                    .error_message(&format!("Muse interrupt failed: {error}"));
                false
            }
        }
    }

    async fn read_native_settings(&mut self) -> Result<SessionSettingsValues, String> {
        let current: SessionResult = self
            .connection
            .typed("session/read", json!({"sessionId":self.session.session_id}))
            .await?;
        let active: Models = self
            .connection
            .typed("model/list", json!({"sessionId":self.session.session_id}))
            .await?;
        let mut selected = active.models.into_iter().filter(|model| model.is_active);
        let model = selected
            .next()
            .ok_or("Muse did not report its active session model")?
            .model_id;
        if selected.next().is_some() {
            return Err("Muse reported multiple active session models".to_owned());
        }
        let definition = self
            .models
            .iter()
            .find(|entry| entry.model_id == model)
            .ok_or("Muse native settings selected a model absent from its catalog")?;
        // Read has no snapshot preference in MSP 1.4; use its acknowledged
        // view head and native reconfigure events, not the requested local value.
        let mut cursor = None::<String>;
        let mut effort = None::<String>;
        let mut reached_head = false;
        loop {
            let mut params =
                json!({"sessionId":self.session.session_id,"direction":"forward","limit":1000});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page: ViewPage = self.connection.typed("view/page", params).await?;
            let empty = page.events.is_empty();
            for event in page.events {
                if event.method == "session/reasoningEffortChanged" {
                    if event.params.get("sessionId").and_then(Value::as_str)
                        != Some(self.session.session_id.as_str())
                    {
                        return Err(
                            "Muse settings view contained another session's reconfigure event"
                                .to_owned(),
                        );
                    }
                    effort = Some(
                        event
                            .params
                            .get("reasoningEffort")
                            .and_then(Value::as_str)
                            .ok_or("Muse native effort change omitted its tier")?
                            .to_owned(),
                    );
                }
                if event.params.get("viewCursor").and_then(Value::as_str)
                    == Some(current.view_cursor.as_str())
                {
                    reached_head = true;
                    break;
                }
            }
            if reached_head {
                break;
            }
            match page.next_cursor {
                Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
                Some(_) => return Err("Muse settings view cursor did not advance".to_owned()),
                None if empty && cursor.is_none() => break,
                None => {
                    return Err(
                        "Muse settings view did not contain its acknowledged read head".to_owned(),
                    );
                }
            }
        }
        let effort = match effort {
            Some(effort) => effort,
            None => {
                let host: Value = self
                    .connection
                    .request("settings/read", json!({}))
                    .await
                    .map_err(|error| error.to_string())?;
                let defaults = host
                    .get("values")
                    .and_then(Value::as_object)
                    .ok_or("Muse host settings omitted their native defaults")?;
                match defaults.get("reasoning_effort") {
                    Some(Value::String(effort)) => effort.clone(),
                    None | Some(Value::Null) => definition
                        .default_reasoning_effort
                        .clone()
                        .ok_or("Muse native catalog omitted its model-default reasoning effort")?,
                    Some(_) => {
                        return Err(
                            "Muse native default reasoning effort had an unsupported shape"
                                .to_owned(),
                        );
                    }
                }
            }
        };
        let mut values = SessionSettingsValues::default();
        for (key, actual, default) in [
            (
                "model",
                model.clone(),
                self.models
                    .iter()
                    .find(|model| model.is_default)
                    .map(|model| model.model_id.clone()),
            ),
            (
                "reasoning_effort",
                effort,
                definition.default_reasoning_effort.clone(),
            ),
        ] {
            let value = if self.settings.0.get(key) == Some(&SessionSettingValue::Null) {
                if default.as_ref() != Some(&actual) {
                    return Err(format!(
                        "Muse did not apply the native catalog default for {key}"
                    ));
                }
                SessionSettingValue::Null
            } else {
                SessionSettingValue::String(actual)
            };
            values.0.insert(key.to_owned(), value);
        }
        self.settings = values.clone();
        Ok(values)
    }

    async fn set_settings(&mut self, update: SessionSettingsValues) -> Result<(), String> {
        if self.pending_compaction.is_some() {
            return Err("Muse session settings cannot change during native compaction".to_owned());
        }
        for (key, value) in update.0 {
            match key.as_str() {
                "model" => {
                    let reset = value == SessionSettingValue::Null;
                    let selected = match value {
                        SessionSettingValue::String(model) => model,
                        SessionSettingValue::Null => self
                            .models
                            .iter()
                            .find(|model| model.is_default)
                            .ok_or("Muse catalog has no default model")?
                            .model_id
                            .clone(),
                        _ => return Err("Muse model selection has the wrong type".to_owned()),
                    };
                    if !self.models.iter().any(|model| model.model_id == selected) {
                        return Err(
                            "Selected Muse model is not in the native account catalog".to_owned()
                        );
                    }
                    self.connection.request("session/setModel",json!({"sessionId":self.session.session_id,"commandId":command_id(),"model":{"providerId":"meta","modelId":selected}})).await.map_err(|error|error.to_string())?;
                    self.settings.0.insert(
                        key,
                        if reset {
                            SessionSettingValue::Null
                        } else {
                            SessionSettingValue::String(selected)
                        },
                    );
                }
                "reasoning_effort" => {
                    let reset = value == SessionSettingValue::Null;
                    let effort = match value {
                        SessionSettingValue::String(effort) => effort,
                        SessionSettingValue::Null => self
                            .models
                            .iter()
                            .find(|model| match self.settings.0.get("model") {
                                Some(SessionSettingValue::String(selected)) => {
                                    model.model_id == *selected
                                }
                                Some(SessionSettingValue::Null) => model.is_default,
                                _ => false,
                            })
                            .and_then(|model| model.default_reasoning_effort.clone())
                            .ok_or("Muse model omitted its default reasoning tier")?,
                        _ => return Err("Muse effort selection has the wrong type".to_owned()),
                    };
                    self.connection.request("session/setReasoningEffort",json!({"sessionId":self.session.session_id,"commandId":command_id(),"reasoningEffort":effort})).await.map_err(|error|error.to_string())?;
                    self.settings.0.insert(
                        key,
                        if reset {
                            SessionSettingValue::Null
                        } else {
                            SessionSettingValue::String(effort)
                        },
                    );
                }
                _ => return Err("Muse received an unsupported session setting".to_owned()),
            }
            self.read_native_settings().await?;
        }
        Ok(())
    }

    async fn answer(&mut self, payload: SendMessagePayload) -> Result<(), String> {
        let Some(SendMessageToolResponse::AskUserQuestion {
            tool_call_id,
            answer,
            structured_answers,
        }) = payload.tool_response
        else {
            return Err("Muse does not expose native plan-approval responses".to_owned());
        };
        let question = self
            .pending_questions
            .get(&tool_call_id)
            .ok_or("Muse question is no longer pending")?;
        let cancel = structured_answers.is_none() && answer.trim().is_empty();
        let params = if let Some(answers) = structured_answers {
            let Some(ToolRequestType::AskUserQuestion { questions, .. }) = self
                .tools
                .get(&tool_call_id)
                .and_then(|tool| tool.request.as_ref())
            else {
                return Err("Muse structured answer has no canonical question request".to_owned());
            };
            if answers.len() != questions.len() {
                return Err("Muse structured answer must answer every question".to_owned());
            }
            let mut native = Vec::new();
            for question in questions {
                let matching = answers
                    .iter()
                    .filter(|answer| question.id.as_ref() == Some(&answer.question_id))
                    .collect::<Vec<_>>();
                let [answer] = matching.as_slice() else {
                    return Err(
                        "Muse structured answer must identify each question exactly once"
                            .to_owned(),
                    );
                };
                question.validate_structured_answer(answer)?;
                let mut value = json!({"questionId":answer.question_id});
                let note = match &answer.answer {
                    protocol::UserQuestionAnswerValue::Selected { labels, note } => {
                        if question.multi_select {
                            value["selectedLabels"] = json!(labels);
                        } else {
                            value["selectedLabel"] = json!(&labels[0]);
                        }
                        note
                    }
                    protocol::UserQuestionAnswerValue::FreeText { text, note } => {
                        value["freeText"] = json!(text);
                        note
                    }
                };
                if let Some(note) = note {
                    value["note"] = json!(note);
                }
                native.push(value);
            }
            json!({"sessionId":self.session.session_id,"commandId":command_id(),"userInputId":question.user_input_id,"answers":native})
        } else if cancel {
            json!({"sessionId":self.session.session_id,"commandId":command_id(),"userInputId":question.user_input_id})
        } else {
            let lines = answer
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>();
            if lines.len() != question.questions.len() {
                return Err(
                    "Muse answer must include one line for every native question".to_owned(),
                );
            }
            let answers = question
                .questions
                .iter()
                .zip(lines)
                .map(|(question, text)| {
                    let text = text.trim();
                    let text =
                        if let Some(rest) = text.strip_prefix(&format!("{}:", question.header)) {
                            rest.trim()
                        } else {
                            text
                        };
                    if question.selection.mode == SelectionMode::Multiple {
                        if text.contains(", ") && question.options.iter().any(|option| option.label.contains(", ")) {
                            return Err("Muse multiple selections cannot use ambiguous comma-separated labels".to_owned());
                        }
                        let labels = text.split(", ").collect::<Vec<_>>();
                        if labels.iter().all(|label| question.options.iter().any(|option| option.label == *label)) {
                            return Ok(json!({"questionId":question.id,"selectedLabels":labels}));
                        }
                    }
                    if question.options.iter().any(|option| option.label == text) {
                        Ok(json!({"questionId":question.id,"selectedLabel":text}))
                    } else {
                        Ok(json!({"questionId":question.id,"freeText":text}))
                    }
                })
                .collect::<Result<Vec<_>, String>>()?;
            json!({"sessionId":self.session.session_id,"commandId":command_id(),"userInputId":question.user_input_id,"answers":answers})
        };
        self.connection
            .request(
                if cancel {
                    "userInput/cancel"
                } else {
                    "userInput/answer"
                },
                params,
            )
            .await
            .map_err(|error| error.to_string())?;
        if !cancel {
            self.emitter.typing_status_changed(true);
        }
        Ok(())
    }

    async fn native_background_tasks(&self) -> Result<HashSet<String>, String> {
        let inventory: TaskInventory = self
            .connection
            .typed("task/list", json!({"sessionId":self.session.session_id}))
            .await?;
        if inventory
            .tasks
            .iter()
            .any(|task| task.status != ItemStatus::InProgress)
        {
            return Err(
                "Muse included terminal work in its running background inventory".to_owned(),
            );
        }
        Ok(inventory
            .tasks
            .into_iter()
            .map(|task| task.task_id)
            .collect())
    }

    fn mark_background_task(&mut self, task_id: &str) -> Result<(), String> {
        self.background_tasks.insert(task_id.to_owned());
        let calls = self
            .tools
            .iter()
            .filter(|(_, tool)| tool.task_id.as_deref() == Some(task_id))
            .map(|(call, _)| call.clone())
            .collect::<Vec<_>>();
        let call = match calls.as_slice() {
            [] => return Ok(()),
            [call] => call,
            _ => return Err("Muse background work mapped to multiple provider requests".to_owned()),
        };
        let tool = self
            .tools
            .get_mut(call)
            .ok_or("Muse background request disappeared during projection")?;
        tool.background = true;
        if tool.request.is_some() {
            self.emitter.tool_progress(&ToolProgressData {
                tool_call_id: call.clone(),
                execution_mode: ToolExecutionMode::Background,
                cancellable: !self.replaying && self.relay.is_none(),
                update: ToolProgressUpdate::Other {
                    payload: json!({"execution_state":"background_running"}),
                },
            });
        }
        Ok(())
    }

    async fn cancel_task(&mut self, id: &str) -> CancelBackgroundTaskOutcome {
        let task = self
            .tools
            .get(id)
            .filter(|tool| tool.background)
            .and_then(|tool| tool.task_id.as_ref());
        let Some(task) = task else {
            return CancelBackgroundTaskOutcome::NotTracked;
        };
        match self.native_background_tasks().await {
            Ok(tasks) if tasks.contains(task) => {}
            Ok(_) => return CancelBackgroundTaskOutcome::NotTracked,
            Err(error) => return CancelBackgroundTaskOutcome::Failed(error),
        }
        let command = command_id();
        match self
            .connection
            .typed::<TaskCommandResult>(
                "task/stop",
                json!({"sessionId":self.session.session_id,"commandId":command,"taskId":task}),
            )
            .await
        {
            Ok(result) if result.command_id == command && result.task_id == *task => {
                let TaskCommandStatus::Accepted = result.status;
                // This acknowledges admission; the native item owns the terminal event.
                tracing::debug!("Muse admitted a named background-task stop");
                CancelBackgroundTaskOutcome::Cancelled
            }
            Ok(_) => CancelBackgroundTaskOutcome::Failed(
                "Muse stop acknowledgement targeted a different command or task".to_owned(),
            ),
            Err(error) => CancelBackgroundTaskOutcome::Failed(error.to_string()),
        }
    }

    fn goal(&mut self, goal: Option<Goal>) -> Result<(), String> {
        let goal = goal
            .map(|goal| {
                Ok(NativeGoal {
                    objective: goal.objective,
                    status: match goal.status.as_str() {
                        "active" | "in_progress" => GoalStatus::Active,
                        "paused" => GoalStatus::Paused,
                        "blocked" => GoalStatus::Blocked,
                        "usage_limited" => GoalStatus::UsageLimited,
                        "complete" | "completed" => GoalStatus::Complete,
                        _ => return Err("Muse goal carried an unsupported native state".to_owned()),
                    },
                    token_budget: None,
                    tokens_used: None,
                    time_used_seconds: None,
                })
            })
            .transpose()?;
        self.current_goal = goal.clone();
        self.emitter.goal_changed(goal);
        Ok(())
    }

    fn capacity(&self, params: Value) -> Result<(), String> {
        let Some(subagents) = &self.subagents else {
            return Ok(());
        };
        let observed_at = params
            .get("observedAtMs")
            .and_then(Value::as_u64)
            .ok_or("Muse quota report omitted its observation time")?;
        let tier = params
            .get("tier")
            .and_then(Value::as_str)
            .ok_or("Muse quota report omitted its plan tier")?;
        let mut buckets = Vec::new();
        for (name, label) in [("window", "Session limit"), ("weekly", "Weekly limit")] {
            let value = params
                .get(name)
                .ok_or("Muse quota report omitted a native window")?;
            let used = value
                .get("usedPercent")
                .and_then(Value::as_u64)
                .ok_or("Muse quota window omitted its magnitude")?;
            let reset = value
                .get("resetsAtMs")
                .and_then(Value::as_u64)
                .ok_or("Muse quota window omitted its reset")?;
            let normalized = used.min(100) as u8;
            buckets.push(protocol::CapacityBucket {
                id: protocol::CapacityBucketId::Muse {
                    bucket: name.to_owned(),
                },
                label: label.to_owned(),
                measure: protocol::CapacityMeasure::UsedPercent {
                    used_percent: normalized,
                    remaining_percent: 100 - normalized,
                    provenance: protocol::ValueProvenance {
                        vendor_reported: used <= 100,
                    },
                },
                scope: protocol::CapacityScope::Account,
                window: match value.get("windowDurationMins").and_then(Value::as_u64) {
                    Some(duration_minutes) => protocol::CapacityWindow::Rolling {
                        duration_minutes: u32::try_from(duration_minutes).map_err(|_| {
                            "Muse quota window exceeded its duration domain".to_owned()
                        })?,
                    },
                    None => protocol::CapacityWindow::NotReported,
                },
                reset: protocol::CapacityReset::At { at_ms: reset },
                status: None,
            });
        }
        subagents.on_backend_capacity(
            BackendKind::Muse,
            protocol::BackendCapacityState::Known {
                report: protocol::CapacityReport {
                    source: protocol::CapacitySource::MuseUsage,
                    observed_at_ms: Some(observed_at),
                    plan: Some(protocol::CapacityPlanLabel {
                        label: tier.to_owned(),
                    }),
                    buckets,
                    coverage: protocol::CapacityCoverage::AllVendorBuckets,
                },
            },
        );
        Ok(())
    }

    fn emit_backend_event(&self, event: BackendEvent) -> Result<(), String> {
        if let Some(relay) = &self.relay {
            match &event {
                BackendEvent::Chat(event) => relay
                    .event_tx
                    .send(event.clone())
                    .map_err(|_| "Muse native child conversation relay closed".to_owned())?,
                BackendEvent::ModelRequestTokenUsage(usage) => relay
                    .model_usage_tx
                    .send(usage.clone())
                    .map_err(|_| "Muse native child accounting relay closed".to_owned())?,
                BackendEvent::ResumeReplayComplete(_) => {}
                BackendEvent::Compaction(_) => {
                    return Err(
                        "Observed Muse child emitted an unrequested compaction operation"
                            .to_owned(),
                    );
                }
            }
        }
        self.events
            .send(event)
            .map_err(|_| "Muse event consumer closed".to_owned())
    }

    fn flush(&mut self) -> Result<(), String> {
        while let Ok(value) = self.emitted.try_recv() {
            let event = if value.get("kind").and_then(Value::as_str) == Some("BackendCompaction") {
                let data = value
                    .get("data")
                    .ok_or("Muse internal compaction event omitted its typed payload")?;
                BackendEvent::Compaction(
                    serde_json::from_value(data.clone())
                        .map_err(|_| "Muse internal compaction projection failed".to_owned())?,
                )
            } else {
                let event: ChatEvent = serde_json::from_value(value).map_err(|error| {
                    format!("Muse chat projection failed: {:?}", error.classify())
                })?;
                BackendEvent::Chat(event)
            };
            self.emit_backend_event(event)?;
        }
        Ok(())
    }

    async fn begin_compaction(
        &mut self,
        request: compaction::BackendCompactionRequest,
    ) -> compaction::BackendCompactionStart {
        use compaction::{
            BackendCompactionDeferredReason as Deferred, BackendCompactionStart as Start,
        };
        if request.focus.is_some() {
            return Start::NotDispatched {
                reason: compaction::BackendCompactionNotDispatchedReason::InvalidFocus,
            };
        }
        if self.pending_compaction.is_some() {
            return Start::Deferred {
                reason: Deferred::AnotherCompactionActive,
            };
        }
        if self.active_turn.is_some()
            || self.response.is_some()
            || self
                .current_goal
                .as_ref()
                .is_some_and(|goal| goal.status == GoalStatus::Active)
        {
            return Start::Deferred {
                reason: Deferred::ActiveTurn,
            };
        }
        if !self.pending_questions.is_empty() {
            return Start::Deferred {
                reason: Deferred::ApprovalPending,
            };
        }
        match self.native_background_tasks().await {
            Ok(tasks) if tasks.is_empty() => {}
            Ok(_) => {
                return Start::Deferred {
                    reason: Deferred::BackgroundMutationActive,
                };
            }
            Err(_) => {
                return Start::NotDispatched {
                    reason: compaction::BackendCompactionNotDispatchedReason::CapabilityUnknown(
                        compaction::BackendCompactionUnknownReason::CapabilityProbeFailed(
                            "Muse task inventory could not establish idle compaction admission"
                                .to_owned(),
                        ),
                    ),
                };
            }
        }
        let (reply, terminal) = oneshot::channel();
        let operation_id = request.operation_id.clone();
        let command_id = command_id();
        self.pending_compaction = Some(PendingCompaction {
            request,
            command_id: command_id.clone(),
            run_stream: None,
            installs: HashMap::new(),
            dispatch: compaction::BackendCompactionDispatchState::MayHaveReachedProvider,
            terminal: reply,
        });
        self.emitter
            .compaction_event(&compaction::BackendCompactionEvent::Progress(
                compaction::BackendCompactionProgress {
                    operation_id: operation_id.clone(),
                    stage: protocol::CompactionStage::Dispatching,
                    elapsed_ms: None,
                },
            ));
        match self
            .connection
            .request(
                "session/compact",
                json!({"sessionId":self.session.session_id,"commandId":command_id}),
            )
            .await
        {
            Ok(value) => match serde_json::from_value::<CompactAccepted>(value) {
                Ok(ack) if ack.command_id == command_id => {
                    let pending = self
                        .pending_compaction
                        .as_mut()
                        .expect("compaction admission is serialized by the actor");
                    pending.dispatch = compaction::BackendCompactionDispatchState::Accepted;
                    tracing::debug!(status = ?ack.status, "Muse compaction admitted; awaiting durable outcome");
                    self.emitter
                        .compaction_event(&compaction::BackendCompactionEvent::Progress(
                            compaction::BackendCompactionProgress {
                                operation_id: operation_id.clone(),
                                stage: protocol::CompactionStage::Compacting,
                                elapsed_ms: None,
                            },
                        ));
                }
                _ => self.fail_compaction(
                    compaction::BackendCompactionFailureKind::ProtocolViolation,
                    "Muse compaction acknowledgement omitted its matching command identity/status",
                ),
            },
            Err(error) => {
                let rejected = error.data.get("kind").and_then(Value::as_str)
                    == Some("commandRejected")
                    || matches!(error.code, -32601 | -32602);
                if rejected && let Some(pending) = &mut self.pending_compaction {
                    pending.dispatch = compaction::BackendCompactionDispatchState::Rejected;
                }
                tracing::debug!(
                    rejected,
                    code = error.code,
                    "Muse compaction request failed at admission"
                );
                self.fail_compaction(
                    if rejected {
                        compaction::BackendCompactionFailureKind::ProviderRejected
                    } else {
                        compaction::BackendCompactionFailureKind::TransportClosed
                    },
                    "Muse compaction request did not yield a valid admission",
                );
            }
        }
        Start::Accepted(compaction::BackendAcceptedCompaction {
            operation_id,
            terminal,
        })
    }

    fn complete_compaction(&mut self, outcome: ManualCompactionOutcome) -> Result<(), String> {
        let Some(pending) = &mut self.pending_compaction else {
            return Ok(());
        };
        if pending.command_id != outcome.command_id {
            return Ok(());
        }
        if pending.run_stream.as_ref() != Some(&outcome.run_stream) {
            return Err(
                "Muse compaction terminal belongs to a different retained run target".to_owned(),
            );
        }
        let install = outcome
            .candidate_id
            .as_ref()
            .and_then(|candidate| pending.installs.remove(candidate));
        let completed = matches!(outcome.terminal_outcome, CompactionOutcome::Compacted);
        if completed != install.is_some() {
            return Err("Muse compaction outcome disagreed with its explicitly identified native installation".to_owned());
        }
        let metrics = install
            .as_ref()
            .map(|install| install.metrics.clone())
            .unwrap_or_default();
        let session_id = SessionId(self.session.session_id.clone());
        let evidence = match install {
            Some(install) => {
                self.emitter
                    .compaction_event(&compaction::BackendCompactionEvent::Observed(Box::new(
                        compaction::BackendObservedCompaction {
                            observation_id: compaction::stable_observation_id(
                                "muse",
                                &session_id.0,
                                &install.install_id,
                            ),
                            trigger: protocol::CompactionTrigger::BackendObservedManual,
                            method: protocol::CompactionMethod::NativeRpc,
                            provider_session_id: Some(session_id.clone()),
                            metrics: metrics.clone(),
                            source: compaction::BackendCompactionObservationSource::MuseInstall {
                                install_id: install.install_id.clone(),
                            },
                            user_focus: None,
                        },
                    )));
                compaction::BackendCompactionTerminalEvidence::Muse {
                    session_id: session_id.0.clone(),
                    install_id: install.install_id,
                }
            }
            None => compaction::BackendCompactionTerminalEvidence::None,
        };
        let (mutation, result) = match outcome.terminal_outcome {
            CompactionOutcome::Compacted => (
                compaction::BackendCompactionMutationState::Completed,
                Ok(compaction::BackendCompactionSuccess {
                    mechanism: protocol::CompactionMethod::NativeRpc,
                }),
            ),
            CompactionOutcome::Noop => (
                compaction::BackendCompactionMutationState::NotObserved,
                Ok(compaction::BackendCompactionSuccess {
                    mechanism: protocol::CompactionMethod::NativeRpc,
                }),
            ),
            CompactionOutcome::Failed | CompactionOutcome::Cancelled => (
                compaction::BackendCompactionMutationState::MayHaveMutated,
                Err(compaction::BackendCompactionFailure {
                    kind: if matches!(outcome.terminal_outcome, CompactionOutcome::Cancelled) {
                        compaction::BackendCompactionFailureKind::Interrupted
                    } else {
                        compaction::BackendCompactionFailureKind::ProviderFailed
                    },
                    message: "Muse reported an unsuccessful durable compaction outcome".to_owned(),
                }),
            ),
        };
        tracing::debug!(completed, "Muse durable manual compaction settled");
        let pending = self
            .pending_compaction
            .take()
            .ok_or("Muse compaction lost its admitted operation")?;
        let _ = pending.terminal.send(compaction::BackendCompactionResult {
            operation_id: pending.request.operation_id,
            dispatch: pending.dispatch,
            mutation,
            outcome: result,
            provider_session_id: Some(session_id),
            metrics,
            // Native budget snapshots are estimates, not a provider-reported occupancy reset.
            post_context_tokens: compaction::PostCompactionTokenCount::Unknown,
            evidence,
        });
        Ok(())
    }

    fn fail_compaction(&mut self, kind: compaction::BackendCompactionFailureKind, message: &str) {
        if let Some(pending) = self.pending_compaction.take() {
            let _ = pending.terminal.send(compaction::BackendCompactionResult {
                operation_id: pending.request.operation_id,
                mutation: if pending.dispatch
                    == compaction::BackendCompactionDispatchState::Rejected
                {
                    compaction::BackendCompactionMutationState::NotObserved
                } else {
                    compaction::BackendCompactionMutationState::MayHaveMutated
                },
                dispatch: pending.dispatch,
                outcome: Err(compaction::BackendCompactionFailure {
                    kind,
                    message: message.to_owned(),
                }),
                provider_session_id: Some(SessionId(self.session.session_id.clone())),
                metrics: Default::default(),
                post_context_tokens: compaction::PostCompactionTokenCount::Unknown,
                evidence: compaction::BackendCompactionTerminalEvidence::None,
            });
        }
    }

    fn fail(&mut self, error: &str) {
        self.fail_compaction(
            compaction::BackendCompactionFailureKind::TransportClosed,
            "Muse native event contract failed during compaction",
        );
        tracing::error!(message = error, "Muse native event contract failed");
        if self.response.is_some()
            && let Err(error) = self.finish_response()
        {
            tracing::error!(
                message = error,
                "Muse failed to settle its partial response"
            );
        }
        self.emitter
            .cancel_pending_foreground_tools("Muse native event stream failed");
        self.emitter.error_message(error);
        self.emitter.typing_status_changed(false);
        if let Err(error) = self.flush() {
            tracing::error!(
                message = error,
                "Muse failure could not reach its event consumer"
            );
        }
        self.connection.shutdown();
    }
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, String> {
    serde_json::from_value(params)
        .map_err(|error| format!("Malformed Muse native payload: {error}"))
}

fn input_parts(payload: &SendMessagePayload, instructions: Option<&str>) -> Vec<Value> {
    let mut input = Vec::new();
    if let Some(instructions) = instructions {
        input.push(json!({"type":"text","text":instructions}));
    }
    input.extend(vec![json!({"type":"text","text":payload.message})]);
    if let Some(images) = &payload.images {
        for image in images {
            input
                .push(json!({"type":"image","base64Data":image.data,"mediaType":image.media_type}));
        }
    }
    input
}

fn normalize_usage(usage: &Usage) -> Result<TokenUsage, String> {
    let cached = usage.cache_read_tokens.unwrap_or(usage.cached_tokens);
    let created = usage.cache_write_tokens.unwrap_or(0);
    let input = usage
        .input_tokens
        .checked_sub(cached + created)
        .ok_or("Muse reported cache counters above its inclusive prompt count")?;
    Ok(TokenUsage {
        input_tokens: input,
        output_tokens: usage.output_tokens,
        total_tokens: usage.input_tokens + usage.output_tokens,
        cached_prompt_tokens: Some(cached),
        cache_creation_input_tokens: Some(created),
        reasoning_tokens: Some(usage.reasoning_tokens),
    })
}

fn add_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.total_tokens += usage.total_tokens;
    total.cached_prompt_tokens =
        Some(total.cached_prompt_tokens.unwrap_or(0) + usage.cached_prompt_tokens.unwrap_or(0));
    total.cache_creation_input_tokens = Some(
        total.cache_creation_input_tokens.unwrap_or(0)
            + usage.cache_creation_input_tokens.unwrap_or(0),
    );
    total.reasoning_tokens =
        Some(total.reasoning_tokens.unwrap_or(0) + usage.reasoning_tokens.unwrap_or(0));
}
