use std::path::{Path, PathBuf};

use protocol::{
    Project, ProjectSource, WorkbenchAvailability, WorkbenchEnvironment, WorkbenchSupport,
    WorkbenchUnsupportedReason,
};
use serde::Deserialize;
use tyde_process_env::launch::{AsyncCommandGroup, ProcessStdio};

use crate::error::{AppError, AppResult};

const RUNNER: &str = include_str!("workbench/runner.py");

#[derive(Debug, Deserialize)]
struct EngineFailure {
    reason: WorkbenchUnsupportedReason,
    message: String,
}

#[derive(Deserialize)]
struct EngineErrorOutput {
    error: EngineFailure,
}

fn unsupported(reason: WorkbenchUnsupportedReason, message: &str) -> WorkbenchAvailability {
    WorkbenchAvailability::Unsupported {
        reason,
        message: message.to_owned(),
    }
}

fn engine_command(
    action: &str,
    arguments: &[&Path],
) -> AppResult<(
    tempfile::NamedTempFile,
    tyde_process_env::launch::GroupCommand,
)> {
    let mut helper = tempfile::NamedTempFile::new()
        .map_err(|error| AppError::internal("workbench_environment", error))?;
    std::io::Write::write_all(&mut helper, RUNNER.as_bytes())
        .map_err(|error| AppError::internal("workbench_environment", error))?;
    let mut command = tyde_process_env::launch::host_command("python3")
        .map_err(|reason| AppError::invalid("workbench_environment", reason))?;
    command
        .arg(helper.path())
        .arg(action)
        .args(arguments)
        .stdin(ProcessStdio::Null)
        .stdout(ProcessStdio::Piped)
        .stderr(ProcessStdio::Piped);
    Ok((helper, command))
}

fn operation_started(action: &str) {
    tracing::info!(action, "Workbench engine operation started");
    #[cfg(feature = "test-support")]
    eprintln!("Workbench engine diagnostic: action={action} phase=started");
}

fn operation_launched(action: &str) {
    tracing::info!(action, "Workbench engine exec established");
    #[cfg(feature = "test-support")]
    eprintln!("Workbench engine diagnostic: action={action} phase=launched");
}

#[derive(Debug)]
enum EngineProcessPhase {
    Launch,
    Wait,
}

fn operation_io_error(action: &str, phase: EngineProcessPhase, error: std::io::Error) -> AppError {
    tracing::warn!(action, ?phase, error_kind = ?error.kind(), "Workbench engine process failed");
    #[cfg(feature = "test-support")]
    eprintln!(
        "Workbench engine diagnostic: action={action} phase=failed stage={phase:?} error_kind={:?}",
        error.kind()
    );
    AppError::internal("workbench_environment", error)
}

async fn invoke<T: serde::de::DeserializeOwned>(action: &str, arguments: &[&Path]) -> AppResult<T> {
    let (helper, command) = engine_command(action, arguments)?;
    operation_started(action);
    let child = command
        .group_spawn()
        .await
        .map_err(|error| operation_io_error(action, EngineProcessPhase::Launch, error))?;
    operation_launched(action);
    let result = child
        .wait_with_output()
        .await
        .map_err(|error| operation_io_error(action, EngineProcessPhase::Wait, error))?;
    drop(helper);
    operation_result(action, result)
}

fn probe_host() -> AppResult<WorkbenchSupport> {
    let (helper, mut command) = engine_command("probe", &[])?;
    operation_started("probe");
    let child = command
        .spawn_blocking()
        .map_err(|error| operation_io_error("probe", EngineProcessPhase::Launch, error))?;
    operation_launched("probe");
    let result = child
        .wait_with_output()
        .map_err(|error| operation_io_error("probe", EngineProcessPhase::Wait, error))?;
    drop(helper);
    operation_result("probe", result)
}

fn operation_result<T: serde::de::DeserializeOwned>(
    action: &str,
    result: std::process::Output,
) -> AppResult<T> {
    tracing::info!(
        action,
        success = result.status.success(),
        "Workbench engine operation completed"
    );
    #[cfg(feature = "test-support")]
    eprintln!(
        "Workbench engine diagnostic: action={action} phase=completed success={}",
        result.status.success()
    );
    if !result.status.success() {
        let failure = result
            .stderr
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .ok_or_else(|| {
                AppError::invalid(
                    "workbench_environment",
                    "Workbench engine omitted its terminal failure record",
                )
            })?;
        let error: EngineErrorOutput = serde_json::from_slice(failure).map_err(|error| {
            AppError::internal_message(
                "workbench_environment",
                "Workbench engine failed without a valid typed failure",
                error,
            )
        })?;
        tracing::warn!(reason = ?error.error.reason, "Workbench environment operation failed");
        #[cfg(feature = "test-support")]
        eprintln!(
            "Workbench engine diagnostic: action={action} phase=rejected reason={:?} message={}",
            error.error.reason, error.error.message
        );
        return Err(
            AppError::invalid("workbench_environment", error.error.message)
                .with_workbench_reason(error.error.reason),
        );
    }
    serde_json::from_slice(&result.stdout).map_err(|error| {
        AppError::internal_message(
            "workbench_environment",
            "Workbench engine returned an invalid result",
            error,
        )
    })
}

pub(crate) fn support() -> WorkbenchSupport {
    if !cfg!(target_os = "linux") {
        let reason = unsupported(
            WorkbenchUnsupportedReason::NotLinux,
            "Bubblewrap workbenches require Linux; Native remains available",
        );
        return WorkbenchSupport {
            bubblewrap: reason.clone(),
            nested_namespaces: reason,
        };
    }
    if tyde_process_env::find_executable_in_path("python3").is_none() {
        let reason = unsupported(
            WorkbenchUnsupportedReason::RuntimeToolMissing,
            "Bubblewrap requires host Python 3",
        );
        return WorkbenchSupport {
            bubblewrap: reason.clone(),
            nested_namespaces: reason,
        };
    }
    let support = match probe_host() {
        Ok(support) => support,
        Err(error) => {
            let reason = unsupported(
                WorkbenchUnsupportedReason::IncompatibleEnvironment,
                &error.message,
            );
            WorkbenchSupport {
                bubblewrap: reason.clone(),
                nested_namespaces: reason,
            }
        }
    };
    #[cfg(feature = "test-support")]
    {
        let unavailable_reason = |availability: &WorkbenchAvailability| match availability {
            WorkbenchAvailability::Available => None,
            WorkbenchAvailability::Unsupported { reason, .. } => Some(*reason),
        };
        eprintln!(
            "Workbench capability diagnostic: phase=reported bubblewrap_reason={:?} nested_reason={:?}",
            unavailable_reason(&support.bubblewrap),
            unavailable_reason(&support.nested_namespaces),
        );
    }
    support
}

pub(crate) async fn prepare(
    roots: &[protocol::WorkbenchRoot],
    environment_root: &Path,
) -> AppResult<WorkbenchEnvironment> {
    if !cfg!(target_os = "linux") {
        return Err(AppError::invalid(
            "workbench_create",
            "Bubblewrap workbenches require Linux; Native remains available",
        )
        .with_workbench_reason(WorkbenchUnsupportedReason::NotLinux));
    }
    let mut configurations = Vec::new();
    let mut automatic = Vec::new();
    for root in roots {
        let path = Path::new(&root.worktree_root.0).join(".tyde/workbench.json");
        match tokio::fs::read(path).await {
            Ok(contents) => {
                configurations.push(
                    serde_json::from_slice::<protocol::WorkbenchConfiguration>(&contents).map_err(
                        |_| {
                            AppError::invalid(
                                "workbench_create",
                                "Invalid .tyde/workbench.json configuration",
                            )
                            .with_workbench_reason(
                                WorkbenchUnsupportedReason::RuntimeConfigurationInvalid,
                            )
                        },
                    )?,
                );
                automatic.push(false);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                configurations.push(protocol::WorkbenchConfiguration::default());
                automatic.push(true);
            }
            Err(error) => return Err(AppError::internal("workbench_create", error)),
        }
    }
    let mut request = tempfile::NamedTempFile::new()
        .map_err(|error| AppError::internal("workbench_create", error))?;
    serde_json::to_writer(&mut request, &serde_json::json!({"roots": roots, "configurations": configurations, "automatic": automatic})).map_err(|error| AppError::internal("workbench_create", error))?;
    let mut setup = tempfile::NamedTempFile::new()
        .map_err(|error| AppError::internal("workbench_create", error))?;
    std::io::Write::write_all(&mut setup, include_str!("workbench/setup.py").as_bytes())
        .map_err(|error| AppError::internal("workbench_create", error))?;
    invoke("prepare", &[request.path(), environment_root, setup.path()]).await
}

#[derive(Deserialize)]
struct SnapshotResult {
    published: bool,
    #[serde(default)]
    cohort: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

pub(crate) async fn seal(project: &Project, discard_artifacts: bool) -> AppResult<()> {
    let ProjectSource::GitWorkbench {
        environment: WorkbenchEnvironment::Bubblewrap { manifest_path, .. },
        roots,
        ..
    } = &project.source
    else {
        return Ok(());
    };
    let Some(root) = roots.first() else {
        return Err(
            AppError::invalid("workbench_remove", "Bubblewrap requires a source root")
                .with_workbench_reason(WorkbenchUnsupportedReason::UnsupportedRoots),
        );
    };
    if !discard_artifacts {
        let result: SnapshotResult = invoke(
            "snapshot",
            &[
                Path::new(&manifest_path.0),
                Path::new(&root.worktree_root.0),
            ],
        )
        .await?;
        tracing::info!(published = result.published, cohort = ?result.cohort, reason = ?result.reason, "Workbench artifact snapshot completed");
    }
    Ok(())
}

#[derive(Deserialize)]
struct RemovedResource {
    removed: bool,
    #[serde(default)]
    already_absent: bool,
}

pub(crate) async fn finish(environment: &WorkbenchEnvironment) -> AppResult<()> {
    let WorkbenchEnvironment::Bubblewrap { manifest_path, .. } = environment else {
        return Ok(());
    };
    let result: RemovedResource = invoke("remove", &[Path::new(&manifest_path.0)]).await?;
    if result.removed == result.already_absent {
        return Err(AppError::invalid(
            "workbench_remove",
            "Workbench engine did not establish resource removal",
        ));
    }
    Ok(())
}

pub(crate) fn validate_execution(project: &Project) -> AppResult<()> {
    project
        .execution_root_mappings()
        .map(|_| ())
        .map_err(|message| {
            AppError::invalid("spawn_agent", message)
                .with_workbench_reason(WorkbenchUnsupportedReason::UnsupportedRoots)
        })?;
    if project.is_removing_workbench() {
        return Err(AppError::conflict(
            "spawn_agent",
            "Workbench removal is in progress",
        ));
    }
    Ok(())
}

pub(crate) async fn prepare_backend_execution(
    launch: Option<&protocol::AcpAgentSpec>,
) -> Result<(), String> {
    use tyde_process_env::launch::{current_project, set_execution_environment};
    let Some(project) = current_project() else {
        return Ok(());
    };
    project.execution_root_mappings().map(|_| ())?;
    let ProjectSource::GitWorkbench {
        environment:
            WorkbenchEnvironment::Bubblewrap {
                manifest_path,
                logical_root,
                ..
            },
        roots,
        ..
    } = &project.source
    else {
        return Ok(());
    };
    let Some(root) = roots.first() else {
        return Err("Execution workspace requires a source root".to_owned());
    };
    let mut environment = invoke::<tyde_process_env::launch::ExecutionEnvironment>(
        "environment",
        &[
            Path::new(&manifest_path.0),
            Path::new(&root.worktree_root.0),
        ],
    )
    .await
    .map_err(|error| error.to_string())?;
    if environment.values.get("TYDE_WORKBENCH_ROOT") != Some(&logical_root.0) {
        return Err("Execution workspace root differs from its canonical runner".to_owned());
    }
    if let Some(launch) = launch {
        if launch
            .env
            .get("TYDE_WORKBENCH_ROOT")
            .is_some_and(|root| root != &logical_root.0)
        {
            return Err(
                "Launch environment cannot replace the execution workspace root".to_owned(),
            );
        }
        environment.values.extend(launch.env.clone());
    }
    set_execution_environment(environment)
}

pub(crate) fn bubblewrap_environment(project: &Project) -> Option<&WorkbenchEnvironment> {
    match &project.source {
        ProjectSource::GitWorkbench {
            environment: environment @ WorkbenchEnvironment::Bubblewrap { .. },
            ..
        } => Some(environment),
        ProjectSource::GitWorkbench {
            environment: WorkbenchEnvironment::Native,
            ..
        }
        | ProjectSource::Standalone { .. } => None,
    }
}

pub(crate) fn environment_directory(store_path: &Path) -> AppResult<PathBuf> {
    let parent = store_path
        .parent()
        .ok_or_else(|| AppError::invalid("workbench_create", "Project store path has no parent"))?;
    Ok(parent
        .join("workbench-environments")
        .join(uuid::Uuid::new_v4().to_string()))
}
