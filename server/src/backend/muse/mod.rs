mod actor;
mod log;
mod tools;
mod transport;
mod wire;

use std::collections::BTreeMap;

use protocol::{
    AgentInput, BackendAccessMode, BackendKind, NativeToolCatalog, SelectOption,
    SendMessagePayload, SessionId, SessionSettingField, SessionSettingFieldType,
    SessionSettingValue, SessionSettingsSchema, SessionSettingsValues, ToolCategory, ToolPolicy,
};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tyde_agent_adapter::{BackendCapabilities, BackendCapability};

use super::{
    Backend, BackendDiscovery, BackendProbeContext, BackendSession, BackendSpawnConfig,
    BackendStartupError, CancelBackgroundTaskOutcome, EventStream, SendOutcome, SteerOutcome,
};
use actor::{Actor, Command};
use transport::Connection;
use wire::{GoalControlApplied, Models, SessionResult};

pub struct MuseBackend {
    session_id: SessionId,
    commands: mpsc::UnboundedSender<Command>,
    connection: Connection,
    actor: Option<JoinHandle<()>>,
}

impl Drop for MuseBackend {
    fn drop(&mut self) {
        self.connection.shutdown();
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

impl MuseBackend {
    async fn open(
        workspace_roots: Vec<String>,
        mut config: BackendSpawnConfig,
        source: Option<SessionId>,
        initial: Option<SendMessagePayload>,
        fork: bool,
    ) -> Result<(Self, EventStream), String> {
        if crate::remote::parse_remote_workspace_roots(&workspace_roots)?.is_some() {
            return Err(
                "Muse must run on the selected Tyde host, not through a direct SSH backend path"
                    .to_owned(),
            );
        }
        tracing::debug!(
            root_count = workspace_roots.len(),
            resuming = source.is_some(),
            fork,
            "Muse resolving startup workspace"
        );
        let roots = if workspace_roots.is_empty() {
            vec![super::tyde_owned_no_root_cwd("muse")?]
        } else {
            workspace_roots
        };
        let roots = super::validate_local_workspace_roots(roots)?;
        let cwd = roots
            .first()
            .ok_or("Muse requires a workspace root")?
            .clone();
        Self::validate_access_mode(config.resolved_spawn_config.access_mode)?;
        if config.execution_mode == super::BackendExecutionMode::InferenceOnly {
            validate_inference_only(&config, source.is_some())?;
        } else {
            Self::validate_tool_policy(&config.resolved_spawn_config.tool_policy)?;
            Self::validate_tool_categories(&config.resolved_spawn_config.excluded_tool_categories)?;
        }
        if !config.backend_config.0.is_empty() {
            return Err(
                "Muse does not expose mutable backend configuration through Tyde".to_owned(),
            );
        }
        let (mut connection, mut notifications, mut host) = launch(&config).await?;
        let models: Models = connection.typed("model/list", json!({})).await?;
        let mut settings = Self::resolve_session_settings(&config);
        let requested_model = matches!(
            settings.0.get("model"),
            Some(SessionSettingValue::String(_))
        );
        let mut model = match settings.0.get("model") {
            Some(SessionSettingValue::String(model)) => model.clone(),
            Some(SessionSettingValue::Null) | None => models
                .models
                .iter()
                .find(|model| model.is_default)
                .ok_or("Muse did not report a default model")?
                .model_id
                .clone(),
            _ => return Err("Muse model must be a native catalog identity".to_owned()),
        };
        if !models.models.iter().any(|entry| entry.model_id == model) {
            return Err("The requested Muse model is absent from the account catalog".to_owned());
        }
        settings.0.insert(
            "model".to_owned(),
            SessionSettingValue::String(model.clone()),
        );
        let profile = match config.resolved_spawn_config.access_mode {
            BackendAccessMode::EnforcedReadOnly => ":read-only",
            BackendAccessMode::Unrestricted | BackendAccessMode::ReadOnly => ":unrestricted",
        };
        let mcp = mcp_config(&config.startup_mcp_servers)?;
        let command_id = command_id();
        let active_goal = match &source {
            Some(source) if !fork => saved_active_goal(&connection, source).await?,
            Some(_) | None => false,
        };
        let mut opened: SessionResult = match &source {
            None => connection.typed("session/start", json!({"commandId":command_id,"workspaceRoot":cwd,"workspaceRoots":roots,"modelId":model,"permissionProfile":profile,"config":mcp})).await?,
            Some(source) if fork => connection.typed("session/fork", json!({"commandId":command_id,"sessionId":source.0,"excludeItems":true})).await?,
            Some(source) if active_goal => {
                // A resumed unfinished goal needs Muse's native idle wake.
                // Pipeline it so live starts may precede the resume reply.
                let resume = connection.typed::<SessionResult>("session/resume", json!({"commandId":command_id,"sessionId":source.0,"history":"snapshot","config":mcp}));
                let wake = async {
                    connection.typed::<Value>("session/setPermissionProfile", json!({"commandId":self::command_id(),"sessionId":source.0,"profileId":profile})).await?;
                    if requested_model {
                        connection.typed::<Value>("session/setModel", json!({"commandId":self::command_id(),"sessionId":source.0,"model":{"providerId":"meta","modelId":model}})).await?;
                    }
                    if let Some(SessionSettingValue::String(effort)) = settings.0.get("reasoning_effort") {
                        connection.typed::<Value>("session/setReasoningEffort", json!({"commandId":self::command_id(),"sessionId":source.0,"reasoningEffort":effort})).await?;
                    }
                    connection.typed::<Value>("goal/resume", json!({"commandId":self::command_id(),"sessionId":source.0})).await?;
                    Ok::<(), String>(())
                };
                let (opened, ()) = tokio::try_join!(resume, wake)?;
                opened
            }
            Some(source) => connection.typed("session/resume", json!({"commandId":command_id,"sessionId":source.0,"history":"snapshot","config":mcp})).await?,
        };
        if fork {
            let fork_id = opened.session.session_id.clone();
            connection.close().await?;
            (connection, notifications, host) = launch(&config).await?;
            opened = connection.typed("session/resume", json!({"commandId":self::command_id(),"sessionId":fork_id,"history":"snapshot","config":mcp})).await?;
        }
        if source.is_some() && !requested_model {
            model = opened
                .session
                .model_id
                .clone()
                .ok_or("Muse resumed session omitted its saved model identity")?;
            settings.0.insert(
                "model".to_owned(),
                SessionSettingValue::String(model.clone()),
            );
        }
        if opened.session.workspace_root.as_deref() != Some(cwd.as_str()) {
            return Err("Muse session belongs to a different workspace; native idle relocation is not supported".to_owned());
        }
        if opened
            .session
            .permission_state
            .as_ref()
            .and_then(|state| state.profile_id.as_deref())
            != Some(profile)
        {
            connection.request("session/setPermissionProfile", json!({"commandId":self::command_id(),"sessionId":opened.session.session_id,"profileId":profile})).await.map_err(|error| error.to_string())?;
            let read: SessionResult = connection
                .typed(
                    "session/read",
                    json!({"sessionId":opened.session.session_id,"excludeItems":true}),
                )
                .await?;
            if read
                .session
                .permission_state
                .as_ref()
                .and_then(|state| state.profile_id.as_deref())
                != Some(profile)
            {
                return Err(
                    "Muse did not commit the requested native permission profile".to_owned(),
                );
            }
        }
        if source.is_some() && requested_model {
            connection.request("session/setModel", json!({"sessionId":opened.session.session_id,"commandId":self::command_id(),"model":{"providerId":"meta","modelId":model}})).await.map_err(|error| error.to_string())?;
        }
        if let Some(SessionSettingValue::String(effort)) = settings.0.get("reasoning_effort") {
            connection.request("session/setReasoningEffort", json!({"sessionId":opened.session.session_id,"commandId":self::command_id(),"reasoningEffort":effort})).await.map_err(|error| error.to_string())?;
        }
        let session_id = SessionId(opened.session.session_id.clone());
        let (commands, receive) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        config.session_settings = Some(settings);
        let mut actor = Actor::new(
            connection.clone(),
            notifications,
            receive,
            events,
            opened,
            actor::Served {
                host,
                models: models.models,
            },
            config,
        )?;
        actor.initialize(source.is_some() && !fork).await?;
        let actor = tokio::spawn(actor.run(initial));
        Ok((
            Self {
                session_id,
                commands,
                connection,
                actor: Some(actor),
            },
            EventStream::new_backend(events_rx),
        ))
    }

    async fn ask<T>(&self, command: impl FnOnce(oneshot::Sender<T>) -> Command) -> Option<T> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(command(reply)).ok()?;
        receive.await.ok()
    }
}

impl Backend for MuseBackend {
    fn compaction_capability(&self) -> super::BackendCompactionCapability {
        super::BackendCompactionCapability::native(
            super::BackendCompactionMechanism::JsonRpcRequest,
            None,
            super::BackendCompactionCapabilityEvidence::AdapterContract,
        )
    }

    async fn begin_compaction(
        &self,
        request: super::BackendCompactionRequest,
    ) -> super::BackendCompactionStart {
        self.ask(|reply| Command::Compact(request, reply))
            .await
            .unwrap_or(super::BackendCompactionStart::NotDispatched {
                reason: super::BackendCompactionNotDispatchedReason::BackendClosed,
            })
    }

    fn capabilities() -> BackendCapabilities {
        [
            BackendCapability::ListSessions,
            BackendCapability::ResumeSession,
            BackendCapability::ForkSession,
            BackendCapability::ImageInput,
            BackendCapability::Interrupt,
            BackendCapability::BackgroundTasks,
            BackendCapability::InterruptCancelsOwnedBackgroundCommands,
            BackendCapability::CancelsBackgroundTasks,
            BackendCapability::SessionSettings,
            BackendCapability::StartupMcpServers,
            BackendCapability::McpImageResults,
            BackendCapability::McpErrorResults,
            BackendCapability::AgentControlTools,
            BackendCapability::Subagents,
            BackendCapability::NativeSubagentHierarchy,
            BackendCapability::TurnUsageReported,
            BackendCapability::CumulativeUsageReported,
            BackendCapability::ModelRequestUsageReported,
            BackendCapability::ContextUsageReported,
            BackendCapability::CompactionReported,
            BackendCapability::DurableCompactionLedger,
            BackendCapability::AgentInitiatedTurns,
            BackendCapability::WorkspaceInstructions,
            BackendCapability::Customization,
            BackendCapability::GenericModifyFile,
            BackendCapability::GenericReadFiles,
            BackendCapability::GenericWebSearch,
            BackendCapability::GenericOtherTool,
            BackendCapability::TaskUpdates,
            BackendCapability::TaskListReplacement,
            BackendCapability::TaskListClear,
            BackendCapability::UserQuestionRequests,
            BackendCapability::UserQuestionHistory,
            BackendCapability::StructuredUserQuestionAnswers,
            BackendCapability::NativeToolPolicies,
            BackendCapability::OpaqueToolProgress,
            BackendCapability::RetryTelemetry,
            BackendCapability::CapacityTelemetry,
            BackendCapability::NativeGoals,
        ]
        .into()
    }

    fn session_settings_schema() -> SessionSettingsSchema {
        super::empty_session_settings_schema(BackendKind::Muse)
    }
    fn has_dynamic_session_schema() -> bool {
        true
    }
    fn token_usage_tracking_mode() -> super::TokenUsageTrackingMode {
        super::TokenUsageTrackingMode::ModelRequests
    }
    fn discovery_policy() -> super::DiscoveryPolicy {
        super::DiscoveryPolicy {
            refresh_on_connect: true,
            ..Default::default()
        }
    }
    fn native_session_id_is_valid(id: &SessionId) -> bool {
        uuid::Uuid::parse_str(&id.0).is_ok()
    }
    fn default_child_workspace_roots(parent: &[String]) -> Vec<String> {
        parent.to_vec()
    }
    fn validate_access_mode(mode: BackendAccessMode) -> Result<(), String> {
        match mode {
            BackendAccessMode::Unrestricted | BackendAccessMode::ReadOnly => Ok(()),
            BackendAccessMode::EnforcedReadOnly => Err(
                "Muse does not support enforced read-only access: native mutation-path protection is not verified".to_owned(),
            ),
        }
    }
    fn validate_tool_categories(categories: &[ToolCategory]) -> Result<(), String> {
        for category in categories {
            match category {
                ToolCategory::AskUser => {}
                ToolCategory::AgentDelegation => {
                    return Err("Muse delegation-category exclusion is not supported".to_owned());
                }
            }
        }
        Ok(())
    }
    fn validate_tool_policy(policy: &ToolPolicy) -> Result<(), String> {
        validate_policy(policy, self::tools::admission_selection)
    }

    async fn discover(context: &BackendProbeContext) -> Result<BackendDiscovery, String> {
        let program = context.program.as_deref().unwrap_or("muse");
        let (connection, _notifications, _host) = connect(
            program,
            &[
                "serve".to_owned(),
                "--contributor-mode".to_owned(),
                "off".to_owned(),
            ],
            None,
            true,
        )
        .await?;
        let models: Models = connection.typed("model/list", json!({})).await?;
        connection.shutdown();
        if models.models.is_empty() {
            return Err(
                "Muse returned no available models; run muse login or configure META_API_KEY"
                    .to_owned(),
            );
        }
        let default = models
            .models
            .iter()
            .find(|model| model.is_default)
            .map(|model| model.model_id.clone());
        let efforts = models
            .models
            .iter()
            .filter_map(|model| model.variants.as_ref())
            .flatten()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let ordered = [
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ];
        let options = ordered
            .into_iter()
            .filter(|effort| efforts.contains(*effort))
            .map(|effort| SelectOption {
                value: effort.to_owned(),
                label: effort.to_owned(),
            })
            .collect::<Vec<_>>();
        let by_model = protocol::SelectOptionsBySetting {
            setting_key: "model".to_owned(),
            values: models
                .models
                .iter()
                .filter_map(|model| {
                    model
                        .variants
                        .as_ref()
                        .map(|efforts| protocol::SelectOptionsForValue {
                            setting_value: model.model_id.clone(),
                            options: efforts
                                .iter()
                                .map(|effort| SelectOption {
                                    value: effort.clone(),
                                    label: effort.clone(),
                                })
                                .collect(),
                        })
                })
                .collect(),
        };
        let schema = SessionSettingsSchema { backend_kind: BackendKind::Muse, model_resolutions: BTreeMap::new(), fields: vec![
            SessionSettingField { key:"model".to_owned(),label:"Model".to_owned(),description:Some("Models available to this Muse account; Wonder Full appears only when the native catalog grants it".to_owned()),field_type:SessionSettingFieldType::Select{options:models.models.iter().map(|model|SelectOption{value:model.model_id.clone(),label:model.display_label.clone()}).collect(),default,nullable:true},use_slider:false,select_options_by_setting:None },
            SessionSettingField { key:"reasoning_effort".to_owned(),label:"Reasoning".to_owned(),description:None,field_type:SessionSettingFieldType::Select{options,default:None,nullable:true},use_slider:true,select_options_by_setting:Some(by_model) },
        ]};
        Ok(BackendDiscovery {
            schema,
            launch_profiles: Vec::new(),
        })
    }

    async fn spawn(
        roots: Vec<String>,
        config: BackendSpawnConfig,
        input: SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        Self::open(roots, config, None, Some(input), false).await
    }
    async fn resume(
        roots: Vec<String>,
        config: BackendSpawnConfig,
        id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        Self::open(roots, config, Some(id), None, false).await
    }
    async fn fork(
        roots: Vec<String>,
        config: BackendSpawnConfig,
        id: SessionId,
        input: SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Self::open(roots, config, Some(id), Some(input), true)
            .await
            .map_err(BackendStartupError::from)
    }
    async fn list_sessions(context: &BackendProbeContext) -> Result<Vec<BackendSession>, String> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            sessions: Vec<wire::Session>,
            next_cursor: Option<String>,
        }
        let (connection, _notifications, _host) = connect(
            context.program.as_deref().unwrap_or("muse"),
            &[
                "serve".to_owned(),
                "--contributor-mode".to_owned(),
                "off".to_owned(),
            ],
            None,
            true,
        )
        .await?;
        let mut cursor = None::<String>;
        let mut sessions = Vec::new();
        loop {
            let mut params = json!({"limit":100});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page: Page = connection.typed("session/list", params).await?;
            for session in page.sessions {
                let roots = session.workspace_root.into_iter().collect::<Vec<_>>();
                if !context.workspace_roots.is_empty()
                    && roots
                        .first()
                        .is_none_or(|root| !context.workspace_roots.contains(root))
                {
                    continue;
                }
                let timestamp = |text: &str| {
                    let time = chrono::DateTime::parse_from_rfc3339(text)
                        .map_err(|_| "Muse reported an invalid session timestamp".to_owned())?;
                    u64::try_from(time.timestamp_millis())
                        .map(Some)
                        .map_err(|_| "Muse session timestamp predates the epoch".to_owned())
                };
                sessions.push(BackendSession {
                    id: SessionId(session.session_id),
                    backend_kind: BackendKind::Muse,
                    workspace_roots: roots,
                    title: session.title,
                    token_count: None,
                    created_at_ms: timestamp(&session.created_at)?,
                    updated_at_ms: timestamp(&session.updated_at)?,
                    resumable: true,
                });
            }
            match page.next_cursor {
                Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
                Some(_) => return Err("Muse session-list cursor did not advance".to_owned()),
                None => break,
            }
        }
        connection.shutdown();
        Ok(sessions)
    }
    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }
    async fn native_tool_catalog(&self) -> Option<NativeToolCatalog> {
        self.ask(Command::Catalog).await.flatten()
    }
    async fn send(&self, input: AgentInput) -> bool {
        matches!(self.send_with_outcome(input).await, SendOutcome::Accepted)
    }
    async fn send_with_outcome(&self, input: AgentInput) -> SendOutcome {
        self.ask(|reply| Command::Send(input, reply))
            .await
            .unwrap_or(SendOutcome::Closed)
    }
    async fn steer(&self, input: SendMessagePayload) -> SteerOutcome {
        self.ask(|reply| Command::Steer(input, reply))
            .await
            .unwrap_or(SteerOutcome::Closed)
    }
    async fn interrupt(&self) -> bool {
        self.ask(Command::Interrupt).await.unwrap_or(false)
    }
    async fn read_session_settings(&self) -> Result<SessionSettingsValues, String> {
        self.ask(Command::Settings)
            .await
            .ok_or("Muse session is closed".to_owned())?
    }
    async fn update_session_settings(
        &mut self,
        update: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        self.ask(|reply| Command::SetSettings(update.values, reply))
            .await
            .ok_or("Muse session is closed".to_owned())?
    }
    async fn cancel_background_task(&self, id: &str) -> CancelBackgroundTaskOutcome {
        self.ask(|reply| Command::CancelTask(id.to_owned(), reply))
            .await
            .unwrap_or(CancelBackgroundTaskOutcome::NotTracked)
    }
    async fn shutdown(mut self) {
        if let Err(error) = self.connection.close().await {
            tracing::error!(%error, "Muse local process teardown failed");
        }
        if let Some(actor) = self.actor.take()
            && let Err(error) = actor.await
        {
            tracing::error!(%error, "Muse event actor could not be joined");
        }
    }
}

pub(super) fn command_id() -> String {
    let mut bytes = *uuid::Uuid::new_v4().as_bytes();
    let milliseconds = crate::agent::now_ms().to_be_bytes();
    bytes[..6].copy_from_slice(&milliseconds[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// Hidden helpers get only Muse's verified empty named toolset with every
/// autonomous observer and implicit capability disabled; anything that
/// could reintroduce native activity is refused rather than approximated.
fn validate_inference_only(config: &BackendSpawnConfig, resuming: bool) -> Result<(), String> {
    if resuming {
        return Err("Muse inference-only execution requires a fresh session".to_owned());
    }
    if !matches!(&config.resolved_spawn_config.tool_policy, ToolPolicy::AllowList { tools } if tools.is_empty())
    {
        return Err("Muse inference-only execution requires an empty tool allow-list".to_owned());
    }
    if !config.startup_mcp_servers.is_empty() {
        return Err("Muse inference-only execution cannot attach MCP servers".to_owned());
    }
    Ok(())
}

/// Workspace stays untrusted so no project-local configuration, hooks, or
/// skills load; the reminder roster is Muse's set of autonomous observers.
/// Muse's provider-default system prompt documents its native `muse.*` tools
/// even when the toolset is empty, and Meta only sends `tool_choice: auto`, so
/// models call those absent tools. The system prompt override replaces it.
const INFERENCE_ONLY_ARGS: [&str; 17] = [
    "serve",
    "--contributor-mode",
    "off",
    "-c",
    r#"run.system_prompt="You are a text-only assistant. No tools are available, so never call a tool. Answer the request directly in plain text.""#,
    "-c",
    "run.toolset=[]",
    "-c",
    r#"run.reminder_roster={"agents":[]}"#,
    "-c",
    r#"run.workflow_trigger_mode="off""#,
    "-c",
    r#"tools.web_search.mode="off""#,
    "-c",
    "tools.web_fetch.enabled=false",
    "-c",
    "tools.artifact.enabled=false",
];

async fn launch(config: &BackendSpawnConfig) -> Result<Launched, String> {
    if config.execution_mode == super::BackendExecutionMode::InferenceOnly {
        let args = INFERENCE_ONLY_ARGS.map(str::to_owned);
        tracing::debug!(
            overrides = args.len(),
            "Muse launching tool-free, observer-free inference host"
        );
        return connect("muse", &args, None, false).await;
    }
    let mut args = vec![
        "serve".to_owned(),
        "--trust-workspace".to_owned(),
        "--contributor-mode".to_owned(),
        "off".to_owned(),
    ];
    if !named_toolset_required(config) {
        return connect("muse", &args, None, user_input_dialogs(config)).await;
    }
    // Hosts differ in which selectors run.toolset accepts, so the named
    // toolset is derived for the host identified by its served schema.
    let (identified, _notifications, host) = connect(
        "muse",
        &[
            "serve".to_owned(),
            "--contributor-mode".to_owned(),
            "off".to_owned(),
        ],
        None,
        false,
    )
    .await?;
    identified.close().await?;
    validate_policy(&config.resolved_spawn_config.tool_policy, |name| {
        self::tools::selection(name, &[], host)
    })?;
    let toolset = toolset(config, host).await?;
    args.extend([
        "-c".to_owned(),
        format!(
            "run.toolset={}",
            serde_json::to_string(&toolset).map_err(|error| error.to_string())?
        ),
    ]);
    let launched = connect("muse", &args, None, user_input_dialogs(config)).await?;
    if launched.2 != host {
        launched.0.shutdown();
        return Err("The Muse host changed between named toolset derivation and launch".to_owned());
    }
    Ok(launched)
}

type Launched = (
    Connection,
    mpsc::UnboundedReceiver<transport::Incoming>,
    self::tools::Host,
);

async fn connect(
    program: &str,
    args: &[String],
    customization: Option<tempfile::TempDir>,
    user_input_dialogs: bool,
) -> Result<Launched, String> {
    let (connection, notifications) = Connection::spawn(program, args, customization).await?;
    let hello=connection.request("initialize",json!({"clientInfo":{"name":"tyde","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true,"requestedCapabilities":["rawLog","sessionMcp"],"userInputDialogs":user_input_dialogs}})).await.map_err(|error|error.to_string())?;
    let host = hello
        .pointer("/schema/fingerprint")
        .and_then(Value::as_str)
        .and_then(self::tools::host)
        .filter(|_| hello.pointer("/schema/version").and_then(Value::as_u64) == Some(1))
        .ok_or("Muse's served MSP schema is not supported; this adapter requires a validated 1.4.6/1.4.7/1.4.10 schema")?;
    let grants = hello
        .get("grantedCapabilities")
        .and_then(Value::as_array)
        .ok_or("Muse omitted its capability grants")?;
    if !["rawLog", "sessionMcp"]
        .iter()
        .all(|required| grants.iter().any(|grant| grant.as_str() == Some(required)))
    {
        return Err(
            "Muse did not grant the required native raw-log and session-MCP surfaces".to_owned(),
        );
    }
    connection
        .notify("initialized", json!({}))
        .await
        .map_err(|error| error.to_string())?;
    tracing::info!(
        schema_version = 1,
        ?host,
        "Muse native protocol initialized with authoritative transcript access"
    );
    Ok((connection, notifications, host))
}

fn mcp_config(servers: &[super::StartupMcpServer]) -> Result<Value, String> {
    let servers=servers.iter().map(|server| {
        let value=match &server.transport {
            super::StartupMcpTransport::Http{url,headers,bearer_token_env_var} => {
                let mut headers = headers.clone();
                if let Some(variable) = bearer_token_env_var {
                    let token = std::env::var(variable)
                        .map_err(|_| "Muse MCP bearer-token environment variable is unavailable".to_owned())?;
                    headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
                    headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
                }
                json!({"transport":"streamableHttp","url":url,"headers":headers,"mode":"required"})
            },
            super::StartupMcpTransport::Stdio{command,args,env} => json!({"transport":"stdio","command":command,"args":args,"env":env,"mode":"required"}),
        };
        Ok((server.name.clone(),value))
    }).collect::<Result<serde_json::Map<_,_>,String>>()?;
    Ok(json!({"mcpServers":servers}))
}

async fn saved_active_goal(connection: &Connection, source: &SessionId) -> Result<bool, String> {
    let session: SessionResult = connection
        .typed(
            "session/read",
            json!({"sessionId":source.0,"excludeItems":true}),
        )
        .await?;
    if session.session.session_id != source.0 {
        return Err("Muse goal preflight returned another session".to_owned());
    }
    let (mut log, stream, inherited_through) = log::NativeLog::open(&session.session.path).await?;
    let mut goal = None;
    let mut sequence = 0;
    while let Some(entry) = log.next().await? {
        let (owner, position) = match &entry {
            log::Entry::Record(record) => (&record.stream, record.sequence),
            log::Entry::Omitted { stream, position } => (stream, position.sequence),
        };
        if owner.id != stream.id || owner.kind != stream.kind || position != sequence + 1 {
            return Err(
                "Muse goal preflight crossed its contiguous native owner stream".to_owned(),
            );
        }
        sequence = position;
        if let log::Entry::Record(record) = entry
            && record.sequence > inherited_through
            && record.payload_type == "session.goal_control.applied"
        {
            if record.schema_version != 1 || record.payload_schema_version != 1 {
                return Err("Muse changed the durable goal-control schema".to_owned());
            }
            let applied: GoalControlApplied = serde_json::from_value(record.payload)
                .map_err(|_| "Muse durable goal control had an invalid native shape".to_owned())?;
            goal = applied.record.goal;
        }
    }
    tracing::debug!(
        records = sequence,
        goal_status = goal.as_ref().map(|goal| goal.status.as_str()),
        "Muse durable goal preflight"
    );
    match goal.as_ref().map(|goal| goal.status.as_str()) {
        Some("active" | "in_progress") => Ok(true),
        Some("paused" | "blocked" | "usage_limited" | "complete" | "completed") | None => Ok(false),
        Some(_) => Err("Muse durable goal control reported an unsupported status".to_owned()),
    }
}

fn user_input_dialogs(config: &BackendSpawnConfig) -> bool {
    let resolved = &config.resolved_spawn_config;
    !resolved
        .excluded_tool_categories
        .contains(&ToolCategory::AskUser)
        && match &resolved.tool_policy {
            ToolPolicy::DenyList { tools } => !tools
                .iter()
                .flat_map(|name| self::tools::policy_names(name))
                .any(|name| name == "request_user_input"),
            ToolPolicy::Unrestricted | ToolPolicy::AllowList { .. } => true,
        }
}

fn named_toolset_required(config: &BackendSpawnConfig) -> bool {
    let resolved = &config.resolved_spawn_config;
    !(self::tools::question_only_policy(&resolved.tool_policy)
        && resolved
            .excluded_tool_categories
            .iter()
            .all(|category| *category == ToolCategory::AskUser))
}

fn validate_policy(
    policy: &ToolPolicy,
    selection: impl Fn(&str) -> protocol::NativeToolSelection,
) -> Result<(), String> {
    match policy {
        ToolPolicy::Unrestricted => Ok(()),
        ToolPolicy::AllowList { tools } | ToolPolicy::DenyList { tools } => {
            let allow_list = matches!(policy, ToolPolicy::AllowList { .. });
            if allow_list && tools.is_empty() {
                return Err("Muse has no verified empty named toolset".to_owned());
            }
            let mut normalized = Vec::new();
            for name in tools {
                if name.contains(['*', '?', '[', ']']) {
                    return Err("Muse tool policies require exact native tool names".to_owned());
                }
                for native in self::tools::policy_names(name) {
                    match selection(&native) {
                        protocol::NativeToolSelection::NamedSelectable
                        | protocol::NativeToolSelection::NamedWithoutMcp => {}
                        protocol::NativeToolSelection::DefaultOnly if !allow_list => {}
                        protocol::NativeToolSelection::DefaultOnly => {
                            return Err(
                                "Muse cannot select default-only controls in an exact allow list"
                                    .to_owned(),
                            );
                        }
                        protocol::NativeToolSelection::Unverified => {
                            // Per-session MCP identities require the configured inventory,
                            // not just this context-free policy definition.
                            if !self::tools::mcp_selector_reference(&native) {
                                return Err("Muse policy references an unverified native selector"
                                    .to_owned());
                            }
                        }
                    }
                    normalized.push(native);
                }
            }
            if !allow_list
                && !self::tools::question_only_policy(policy)
                && self::tools::NATIVE_TOOLS.iter().any(|name| {
                    selection(name) == protocol::NativeToolSelection::DefaultOnly
                        && !normalized.iter().any(|denied| denied.as_str() == *name)
                })
            {
                return Err(
                    "Muse named deny lists must explicitly deny every default-only control"
                        .to_owned(),
                );
            }
            Ok(())
        }
    }
}

async fn toolset(
    config: &BackendSpawnConfig,
    host: self::tools::Host,
) -> Result<Vec<String>, String> {
    let resolved = &config.resolved_spawn_config;
    let references = match &resolved.tool_policy {
        ToolPolicy::Unrestricted => Vec::new(),
        ToolPolicy::AllowList { tools } | ToolPolicy::DenyList { tools } => tools
            .iter()
            .flat_map(|tool| self::tools::policy_names(tool))
            .collect::<Vec<_>>(),
    };
    let needs_mcp = !matches!(resolved.tool_policy, ToolPolicy::AllowList { .. })
        || references
            .iter()
            .any(|name| self::tools::mcp_selector_reference(name));
    let mcp_names = if needs_mcp {
        configured_mcp_selectors(config).await?
    } else {
        Vec::new()
    };
    if references
        .iter()
        .any(|name| self::tools::mcp_selector_reference(name) && !mcp_names.contains(name))
    {
        return Err(
            "Muse policy references a tool absent from its configured MCP inventory".to_owned(),
        );
    }
    let mut names = match &resolved.tool_policy {
        ToolPolicy::AllowList { tools } => tools
            .iter()
            .flat_map(|tool| self::tools::policy_names(tool))
            .collect::<Vec<_>>(),
        ToolPolicy::Unrestricted | ToolPolicy::DenyList { .. } => {
            let mut names = self::tools::NATIVE_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>();
            names.extend(mcp_names.iter().cloned());
            names
        }
    };
    if let ToolPolicy::DenyList { tools } = &resolved.tool_policy {
        let denied = tools
            .iter()
            .flat_map(|tool| self::tools::policy_names(tool))
            .collect::<Vec<_>>();
        names.retain(|name| !denied.contains(name));
    }
    names.retain(|name| {
        let kind = self::tools::kind(name);
        if let protocol::NativeToolKind::Controlled { category } = kind
            && resolved.excluded_tool_categories.contains(&category)
        {
            return false;
        }
        true
    });
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err("Muse has no verified empty named toolset".to_owned());
    }
    let selects_mcp = names.iter().any(|name| mcp_names.contains(name));
    if selects_mcp
        && names.iter().any(|name| {
            self::tools::selection(name, &mcp_names, host)
                == protocol::NativeToolSelection::NamedWithoutMcp
        })
    {
        return Err(
            "Muse cannot combine MCP and native subagent tools in an exact named selection"
                .to_owned(),
        );
    }
    if names.iter().any(
        |name| match self::tools::selection(name, &mcp_names, host) {
            protocol::NativeToolSelection::NamedSelectable => false,
            protocol::NativeToolSelection::NamedWithoutMcp => selects_mcp,
            protocol::NativeToolSelection::DefaultOnly
            | protocol::NativeToolSelection::Unverified => true,
        },
    ) {
        return Err(format!(
            "Muse {host:?} named toolset contains a default-only or unverified selector"
        ));
    }
    Ok(names)
}

async fn configured_mcp_selectors(config: &BackendSpawnConfig) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for server in &config.startup_mcp_servers {
        let transport = match &server.transport {
            super::StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let mut headers = headers.clone();
                if let Some(variable) = bearer_token_env_var {
                    let token = std::env::var(variable).map_err(|_| {
                        "Muse MCP bearer-token environment variable is unavailable".to_owned()
                    })?;
                    headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
                    headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
                }
                crate::mcp_bridge::BridgeTransport::Http {
                    url: url.clone(),
                    headers,
                }
            }
            super::StartupMcpTransport::Stdio { command, args, env } => {
                crate::mcp_bridge::BridgeTransport::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.clone(),
                }
            }
        };
        let descriptor = crate::mcp_bridge::BridgeServerConfig {
            name: server.name.clone(),
            transport,
        };
        for tool in crate::mcp_bridge::discover_tool_names(&descriptor).await? {
            names.push(format!("mcp__{}__{tool}", server.name));
        }
    }
    Ok(names)
}
