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

pub(crate) async fn list_sessions() -> Result<Vec<crate::backend::BackendSession>, String> {
    let output = tokio::process::Command::new("opencode")
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
