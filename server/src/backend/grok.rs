use protocol::{
    AcpAdapterId, AcpAgentSpec, BackendKind, SessionId, SessionSettingValue, SpawnCostHint,
};
use tyde_agent_adapter::{BackendCapabilities, BackendCapability};

use crate::backend::{BackendSession, BackendSpawnConfig, resolve_settings};

pub(crate) fn agent_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        command: "grok".to_owned(),
        args: vec!["agent".to_owned(), "stdio".to_owned()],
        cwd: None,
        env: Default::default(),
        adapter: AcpAdapterId::Grok,
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
        BackendCapability::ImageInput,
        BackendCapability::Interrupt,
        BackendCapability::SessionSettings,
        BackendCapability::StartupMcpServers,
        BackendCapability::AgentControlTools,
        BackendCapability::Subagents,
        BackendCapability::TurnUsageReported,
        BackendCapability::CumulativeUsageReported,
        BackendCapability::ModelRequestUsageReported,
        BackendCapability::ContextUsageReported,
        BackendCapability::ContextBreakdownReported,
        BackendCapability::ReasoningDeltas,
        BackendCapability::WorkspaceInstructions,
        BackendCapability::Customization,
        BackendCapability::GenericModifyFile,
        BackendCapability::GenericReadFiles,
        BackendCapability::GenericWebSearch,
        BackendCapability::GenericOtherTool,
        BackendCapability::TaskUpdates,
        BackendCapability::TaskListReplacement,
        BackendCapability::TaskListClear,
        BackendCapability::CapacityTelemetry,
        BackendCapability::OutOfBandCapacity,
    ]
    .into()
}

pub(crate) async fn list_sessions(
    context: &crate::backend::BackendProbeContext,
) -> Result<Vec<BackendSession>, String> {
    let root = context
        .workspace_roots
        .first()
        .ok_or_else(|| "Grok session discovery requires a workspace root".to_owned())?;
    let output = crate::process_env::command(context.program.as_deref().unwrap_or("grok"))?
        .current_dir(root)
        .args(["sessions", "list", "--limit", "1000"])
        .output()
        .await
        .map_err(|error| format!("failed to run 'grok sessions list': {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "'grok sessions list' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let id = columns.next()?;
            if uuid::Uuid::parse_str(id).is_err() {
                return None;
            }
            let _created = columns.next()?;
            let _updated = columns.next()?;
            let _status = columns.next()?;
            let title = columns.collect::<Vec<_>>().join(" ");
            Some(BackendSession {
                id: SessionId(id.to_owned()),
                backend_kind: BackendKind::Grok,
                workspace_roots: vec![root.clone()],
                title: (!title.is_empty() && title != "(no summary)").then_some(title),
                token_count: None,
                created_at_ms: None,
                updated_at_ms: None,
                resumable: true,
            })
        })
        .collect())
}

pub(crate) fn session_settings_schema() -> protocol::SessionSettingsSchema {
    protocol::SessionSettingsSchema {
        backend_kind: BackendKind::Grok,
        fields: [
            (
                "model",
                "Model",
                vec![
                    protocol::SelectOption {
                        value: "grok-4.6".to_owned(),
                        label: "Grok 4.6".to_owned(),
                    },
                    protocol::SelectOption {
                        value: "grok-4.5".to_owned(),
                        label: "Grok 4.5".to_owned(),
                    },
                ],
                Some("grok-4.6".to_owned()),
            ),
            (
                "mode",
                "Reasoning effort",
                ["low", "medium", "high", "xhigh"]
                    .into_iter()
                    .map(|value| protocol::SelectOption {
                        value: value.to_owned(),
                        label: value.to_owned(),
                    })
                    .collect(),
                Some("high".to_owned()),
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
                    options,
                    default,
                    nullable: true,
                },
            },
        )
        .collect(),
    }
}

fn cost_hint_defaults(cost_hint: SpawnCostHint) -> protocol::SessionSettingsValues {
    let mut values = protocol::SessionSettingsValues::default();
    let effort = match cost_hint {
        SpawnCostHint::Low => "low",
        SpawnCostHint::Medium => "medium",
        SpawnCostHint::High => "high",
    };
    values.0.insert(
        "mode".to_owned(),
        SessionSettingValue::String(effort.to_owned()),
    );
    values
}

pub(crate) fn resolve_session_settings(
    config: &BackendSpawnConfig,
) -> protocol::SessionSettingsValues {
    resolve_settings(config, &session_settings_schema(), cost_hint_defaults)
}

pub struct GrokBackend(crate::backend::acp::backend::KiroBackend);

impl crate::backend::Backend for GrokBackend {
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
                backend_kind: BackendKind::Grok,
                message: "Grok does not expose manual compaction through ACP".to_owned(),
            },
        )
    }

    fn resolve_session_settings(config: &BackendSpawnConfig) -> protocol::SessionSettingsValues {
        resolve_session_settings(config)
    }

    async fn read_capacity_out_of_band(
        context: &crate::backend::BackendProbeContext,
    ) -> protocol::BackendCapacityState {
        crate::backend::acp::backend::read_grok_capacity_out_of_band(
            &context.workspace_roots,
            context.launch.as_ref(),
        )
        .await
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
            crate::backend::backend_fork_unsupported_message(BackendKind::Grok),
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
