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
        #[cfg(unix)]
        BackendCapability::SetWorkspaceRoots,
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
        BackendCapability::PlanApprovalRequests,
        BackendCapability::GenericOtherTool,
        BackendCapability::TaskUpdates,
        BackendCapability::TaskListReplacement,
        BackendCapability::TaskListClear,
        BackendCapability::CapacityTelemetry,
        BackendCapability::OutOfBandCapacity,
        BackendCapability::CompactionReported,
    ]
    .into()
}

fn link_workspace_session(
    session_id: &str,
    previous: &str,
    destination: &str,
) -> Result<std::path::PathBuf, String> {
    uuid::Uuid::parse_str(session_id)
        .map_err(|error| format!("Invalid Grok session identity: {error}"))?;
    let sessions = crate::paths::home_dir()?.join(".grok").join("sessions");
    let encode = |root: &str| -> String {
        root.bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                    char::from(byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect()
    };
    let source = sessions
        .join(encode(previous))
        .join(session_id)
        .canonicalize()
        .map_err(|error| format!("Cannot locate Grok conversation for relocation: {error}"))?;
    if !source.join("summary.json").is_file() {
        return Err("Grok conversation is missing its native summary".to_owned());
    }
    let alias = sessions.join(encode(destination)).join(session_id);
    if let Ok(existing) = alias.canonicalize() {
        return if existing == source {
            Ok(source)
        } else {
            Err(
                "Destination already contains a different Grok conversation with the same identity"
                    .to_owned(),
            )
        };
    }
    std::fs::create_dir_all(alias.parent().ok_or("Invalid Grok session alias path")?)
        .map_err(|error| format!("Cannot prepare Grok destination scope: {error}"))?;
    // Keep one native transcript. Copying would let later resumes diverge.
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&source, &alias);
    #[cfg(not(unix))]
    let linked: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Grok workspace relocation requires directory symlink support",
    ));
    linked.map_err(|error| {
        format!("Cannot alias Grok conversation into destination scope: {error}")
    })?;
    tracing::info!(%session_id, source = %source.display(), alias = %alias.display(), "Linked Grok conversation into destination workspace scope");
    Ok(source)
}

pub(crate) async fn relocate_workspace_session(
    session_id: &str,
    previous: &str,
    destination: &str,
) -> Result<(), String> {
    fn write_json(path: &std::path::Path, value: &serde_json::Value) -> Result<(), String> {
        let mut temporary =
            tempfile::NamedTempFile::new_in(path.parent().ok_or("Invalid native metadata path")?)
                .map_err(|error| format!("Cannot prepare native metadata update: {error}"))?;
        serde_json::to_writer(temporary.as_file_mut(), value)
            .map_err(|error| format!("Cannot encode native metadata: {error}"))?;
        temporary
            .persist(path)
            .map_err(|error| format!("Cannot replace native metadata: {error}"))?;
        Ok(())
    }
    let source = link_workspace_session(session_id, previous, destination)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(source.join("summary.json.lock"))
        .map_err(|error| format!("Cannot open Grok summary lock: {error}"))?;
    lock.try_lock()
        .map_err(|error| format!("Grok conversation is still being written: {error}"))?;
    let mut updates = Vec::new();
    for (filename, field) in [
        ("summary.json", "/info/cwd"),
        ("prompt_context.json", "/working_directory"),
    ] {
        let path = source.join(filename);
        let before: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&path)
                .map_err(|error| format!("Cannot read Grok {filename}: {error}"))?,
        )
        .map_err(|error| format!("Invalid Grok {filename}: {error}"))?;
        if before.pointer(field).and_then(serde_json::Value::as_str) != Some(previous) {
            return Err(format!(
                "Grok {filename} has a different workspace; refusing stale relocation"
            ));
        }
        let mut after = before.clone();
        *after
            .pointer_mut(field)
            .ok_or("Missing Grok workspace field")? = serde_json::json!(destination);
        if filename == "summary.json" {
            for (key, argument) in [
                ("git_root_dir", "--show-toplevel"),
                ("head_commit", "HEAD"),
                ("head_branch", "--abbrev-ref"),
            ] {
                let mut command = crate::process_env::command("git")?;
                command
                    .current_dir(destination)
                    .args(["rev-parse", argument]);
                if argument == "--abbrev-ref" {
                    command.arg("HEAD");
                }
                let output = command
                    .output()
                    .await
                    .map_err(|error| format!("Cannot inspect destination git metadata: {error}"))?;
                after[key] = if output.status.success() {
                    serde_json::json!(String::from_utf8_lossy(&output.stdout).trim())
                } else {
                    serde_json::Value::Null
                };
            }
        }
        updates.push((path, before, after));
    }
    for (index, (path, _, after)) in updates.iter().enumerate() {
        if let Err(error) = write_json(path, after) {
            for (path, before, _) in &updates[..index] {
                write_json(path, before)?;
            }
            return Err(error);
        }
    }
    tracing::info!(%session_id, %destination, "Relocated Grok native summary and prompt workspace without rewriting conversation history");
    Ok(())
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

    async fn begin_compaction(
        &self,
        request: crate::backend::compaction::BackendCompactionRequest,
    ) -> crate::backend::compaction::BackendCompactionStart {
        self.0.begin_compaction(request).await
    }

    async fn interrupt(&self) -> bool {
        self.0.interrupt().await
    }

    async fn shutdown(self) {
        self.0.shutdown().await;
    }
}
