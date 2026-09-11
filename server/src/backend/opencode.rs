use protocol::{
    AcpAdapterId, AcpAgentSpec, BackendKind, SessionId, SessionSettingValue, SpawnCostHint,
};
use tyde_agent_adapter::{BackendCapabilities, BackendCapability};

use crate::backend::{BackendSpawnConfig, resolve_settings};

const DEFAULT_FREE_MODEL: &str = "opencode/mimo-v2.5-free";

pub(crate) fn model_context_window(model: &str) -> Option<u64> {
    match model {
        "opencode/mimo-v2.5-free" | "opencode/big-pickle" => Some(200_000),
        "opencode/muse-spark-1.3-contributor-free" => Some(1_048_576),
        "opencode/nemotron-3-ultra-free" => Some(1_000_000),
        _ => None,
    }
}

pub(crate) fn agent_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        command: "opencode".to_owned(),
        args: vec!["acp".to_owned()],
        cwd: None,
        env: Default::default(),
        adapter: AcpAdapterId::Opencode,
    }
}

pub(crate) fn configure(mut config: BackendSpawnConfig) -> BackendSpawnConfig {
    config.acp_agent = Some(agent_spec());
    config.cost_hint = None;
    config
}

pub(crate) fn capabilities() -> BackendCapabilities {
    [
        BackendCapability::ListSessions,
        BackendCapability::ResumeSession,
        BackendCapability::SetWorkspaceRoots,
        BackendCapability::ImageInput,
        BackendCapability::SessionSettings,
        BackendCapability::StartupMcpServers,
        BackendCapability::AgentControlTools,
        BackendCapability::Subagents,
        BackendCapability::TurnUsageReported,
        BackendCapability::CumulativeUsageReported,
        BackendCapability::ModelRequestUsageReported,
        BackendCapability::ContextUsageReported,
        BackendCapability::ReasoningDeltas,
        BackendCapability::WorkspaceInstructions,
        BackendCapability::Customization,
        BackendCapability::GenericModifyFile,
        BackendCapability::GenericReadFiles,
        BackendCapability::GenericWebSearch,
        BackendCapability::GenericOtherTool,
    ]
    .into()
}

pub(crate) async fn relocate_workspace_session(
    program: &str,
    session_id: &str,
    previous: &str,
    destination: &str,
) -> Result<(), String> {
    async fn export(
        program: &str,
        session_id: &str,
        cwd: &str,
    ) -> Result<serde_json::Value, String> {
        let output = crate::process_env::command(program)?
            .current_dir(cwd)
            .args(["export", session_id])
            .output()
            .await
            .map_err(|error| format!("Cannot export OpenCode conversation: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "OpenCode export failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("Invalid OpenCode conversation export: {error}"))
    }
    let before = export(program, session_id, previous).await?;
    if before
        .pointer("/info/id")
        .and_then(serde_json::Value::as_str)
        != Some(session_id)
        || before
            .pointer("/info/directory")
            .and_then(serde_json::Value::as_str)
            != Some(previous)
    {
        return Err("OpenCode conversation identity or directory changed concurrently".to_owned());
    }
    let mut file = tempfile::NamedTempFile::new()
        .map_err(|error| format!("Cannot prepare private OpenCode export: {error}"))?;
    serde_json::to_writer(file.as_file_mut(), &before)
        .map_err(|error| format!("Cannot save private OpenCode export: {error}"))?;
    let output = crate::process_env::command(program)?
        .current_dir(destination)
        .arg("import")
        .arg(file.path())
        .output()
        .await
        .map_err(|error| format!("Cannot import OpenCode conversation in destination: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "OpenCode relocation import failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let after = export(program, session_id, destination).await?;
    if after
        .pointer("/info/id")
        .and_then(serde_json::Value::as_str)
        != Some(session_id)
        || after
            .pointer("/info/directory")
            .and_then(serde_json::Value::as_str)
            != Some(destination)
        || after.get("messages") != before.get("messages")
    {
        return Err("OpenCode import did not preserve conversation identity/history and acknowledge the destination".to_owned());
    }
    tracing::info!(%session_id, %destination, "Native OpenCode import relocated session metadata and preserved every message");
    Ok(())
}

pub(crate) async fn list_sessions(
    context: &crate::backend::BackendProbeContext,
) -> Result<Vec<crate::backend::BackendSession>, String> {
    let mut sessions = std::collections::BTreeMap::new();
    let roots = if context.workspace_roots.is_empty() {
        vec![None]
    } else {
        context.workspace_roots.iter().map(Some).collect()
    };
    for root in roots {
        for session in
            list_workspace_sessions(context.program.as_deref().unwrap_or("opencode"), root).await?
        {
            sessions.insert(session.id.0.clone(), session);
        }
    }
    Ok(sessions.into_values().collect())
}

async fn list_workspace_sessions(
    program: &str,
    root: Option<&String>,
) -> Result<Vec<crate::backend::BackendSession>, String> {
    let mut command = crate::process_env::command(program)?;
    if let Some(root) = root {
        command.current_dir(root);
    }
    let output = command
        .args(["session", "list", "--format", "json", "--max-count", "1000"])
        .output()
        .await
        .map_err(|error| format!("failed to run 'opencode session list': {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "'opencode session list' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid OpenCode session list: {error}"))?;
    Ok(sessions
        .into_iter()
        .filter_map(|session| {
            let id = session.get("id")?.as_str()?.to_owned();
            Some(crate::backend::BackendSession {
                id: SessionId(id),
                backend_kind: BackendKind::Opencode,
                workspace_roots: session
                    .get("directory")
                    .and_then(serde_json::Value::as_str)
                    .map(|root| vec![root.to_owned()])
                    .unwrap_or_default(),
                title: session
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                token_count: None,
                created_at_ms: session.get("created").and_then(serde_json::Value::as_u64),
                updated_at_ms: session.get("updated").and_then(serde_json::Value::as_u64),
                resumable: true,
            })
        })
        .collect())
}

pub(crate) fn session_settings_schema() -> protocol::SessionSettingsSchema {
    protocol::SessionSettingsSchema {
        backend_kind: BackendKind::Opencode,
        fields: [
            (
                "model",
                "Model",
                vec![
                    ("opencode/mimo-v2.5-free", "MiMo V2.5 (free, multimodal)"),
                    (
                        "opencode/muse-spark-1.3-contributor-free",
                        "Muse Spark 1.3 (free, multimodal)",
                    ),
                    ("opencode/nemotron-3-ultra-free", "Nemotron 3 Ultra (free)"),
                    ("opencode/big-pickle", "Big Pickle (free)"),
                ],
                Some(DEFAULT_FREE_MODEL),
            ),
            (
                "mode",
                "Mode",
                vec![("build", "Build"), ("plan", "Plan")],
                Some("build"),
            ),
        ]
        .into_iter()
        .map(
            |(key, label, options, default)| protocol::SessionSettingField {
                key: key.to_owned(),
                label: label.to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: protocol::SessionSettingFieldType::Select {
                    options: options
                        .into_iter()
                        .map(|(value, label)| protocol::SelectOption {
                            value: value.to_owned(),
                            label: label.to_owned(),
                        })
                        .collect(),
                    default: default.map(str::to_owned),
                    nullable: true,
                },
            },
        )
        .collect(),
    }
}

fn cost_hint_defaults(_cost_hint: SpawnCostHint) -> protocol::SessionSettingsValues {
    let mut values = protocol::SessionSettingsValues::default();
    values.0.insert(
        "model".to_owned(),
        SessionSettingValue::String(DEFAULT_FREE_MODEL.to_owned()),
    );
    values.0.insert(
        "mode".to_owned(),
        SessionSettingValue::String("build".to_owned()),
    );
    values
}

pub(crate) fn resolve_session_settings(
    config: &BackendSpawnConfig,
) -> protocol::SessionSettingsValues {
    let mut resolved = resolve_settings(config, &session_settings_schema(), cost_hint_defaults);
    resolved
        .0
        .entry("model".to_owned())
        .or_insert_with(|| SessionSettingValue::String(DEFAULT_FREE_MODEL.to_owned()));
    resolved
}

pub struct OpencodeBackend(crate::backend::acp::backend::KiroBackend);

impl crate::backend::Backend for OpencodeBackend {
    fn default_child_workspace_roots(parent_roots: &[String]) -> Vec<String> {
        // OpenCode's native task calls omit Tyde's explicit workspace arguments.
        parent_roots.to_vec()
    }

    async fn prepare_context_replacement(
        _config: BackendSpawnConfig,
        _seed: crate::backend::compaction::BackendContextSeed,
    ) -> Result<
        (
            Self,
            crate::backend::EventStream,
            SessionId,
            crate::backend::compaction::BackendBindingReadyEvidence,
        ),
        crate::backend::compaction::BackendBindingPrepareError,
    > {
        Err(
            crate::backend::compaction::BackendBindingPrepareError::SpawnFailed {
                backend_kind: BackendKind::Opencode,
                message: "OpenCode does not expose manual compaction through ACP".to_owned(),
            },
        )
    }

    fn resolve_session_settings(config: &BackendSpawnConfig) -> protocol::SessionSettingsValues {
        resolve_session_settings(config)
    }

    fn capabilities() -> BackendCapabilities {
        capabilities()
    }

    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        session_settings_schema()
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, crate::backend::EventStream), String> {
        let (backend, events) = crate::backend::acp::backend::KiroBackend::spawn(
            workspace_roots,
            configure(config),
            initial_input,
        )
        .await?;
        Ok((Self(backend), events))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, crate::backend::EventStream), String> {
        let (backend, events) = crate::backend::acp::backend::KiroBackend::resume(
            workspace_roots,
            configure(config),
            session_id,
        )
        .await?;
        Ok((Self(backend), events))
    }

    async fn fork(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, crate::backend::EventStream), crate::backend::BackendStartupError> {
        Err(crate::backend::BackendStartupError::unsupported(
            crate::backend::backend_fork_unsupported_message(BackendKind::Opencode),
        ))
    }

    async fn list_sessions(
        context: &crate::backend::BackendProbeContext,
    ) -> Result<Vec<crate::backend::BackendSession>, String> {
        list_sessions(context).await
    }

    fn session_id(&self) -> SessionId {
        self.0.session_id()
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        self.0.update_session_settings(payload).await
    }

    async fn set_workspace_roots(&mut self, workspace_roots: Vec<String>) -> Result<(), String> {
        self.0.set_workspace_roots(workspace_roots).await
    }

    async fn read_session_settings(&self) -> Result<protocol::SessionSettingsValues, String> {
        self.0.read_session_settings().await
    }

    async fn send(&self, input: protocol::AgentInput) -> bool {
        self.0.send(input).await
    }

    fn compaction_capability(&self) -> crate::backend::BackendCompactionCapability {
        self.0.compaction_capability()
    }

    async fn interrupt(&self) -> bool {
        self.0.interrupt().await
    }

    async fn shutdown(self) {
        self.0.shutdown().await;
    }
}
