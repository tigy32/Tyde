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
    ]
    .into()
}

pub(crate) async fn list_sessions() -> Result<Vec<BackendSession>, String> {
    let output = tokio::process::Command::new("grok")
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
                workspace_roots: Vec::new(),
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
