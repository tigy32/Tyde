use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use command_group::AsyncCommandGroup;
use protocol::{
    AcpAdapterId, BackendKind, BackendSetupAction, BackendSetupCommand, BackendSetupDiagnostic,
    BackendSetupDiagnosticCode, BackendSetupInfo, BackendSetupPayload, BackendSetupStatus,
    HostPlatform,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::browse_stream::host_platform;
use crate::process_env;

const CLAUDE_CLI_CANDIDATES: &[&str] = &["claude"];
const CODEX_CLI_CANDIDATES: &[&str] = &["codex"];
const ANTIGRAVITY_CLI_CANDIDATES: &[&str] = &["agy"];
const KIRO_CLI_CANDIDATES: &[&str] = &["kiro-cli", "kiro-cli-chat"];
const HERMES_PYTHON_MODULE: &str = "tui_gateway.entry";
const ACP_AGGREGATE_MAX_FAILURES: usize = 4;
const ACP_AGGREGATE_LABEL_MAX_BYTES: usize = 96;
const ACP_AGGREGATE_DETAIL_MAX_BYTES: usize = 256;
const ACP_AGGREGATE_MESSAGE_MAX_BYTES: usize = 1536;
const ACP_COMMAND_DIAGNOSTIC_MAX_BYTES: usize = 512;
const ACP_DIAGNOSTIC_TRUNCATION_MARKER: &str = "… [truncated]";

pub(crate) struct PreparedBackendSetupCommand {
    program: String,
    arguments: Vec<String>,
    display_command: String,
    staged_script: PathBuf,
}

impl PreparedBackendSetupCommand {
    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub(crate) fn display_command(&self) -> &str {
        &self.display_command
    }
}

impl Drop for PreparedBackendSetupCommand {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.staged_script)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.staged_script.display(),
                %error,
                "failed to remove staged backend setup script"
            );
        }
    }
}

/// Probes every backend.
///
/// `acp_agents` carries the user's configured ACP agents (label + command) so
/// the ACP row reflects the agents that are actually configured rather than
/// only the built-in Kiro CLI. Pass an empty slice when settings are not
/// available; the built-in agent is still probed.
pub(crate) async fn collect_backend_setup(
    acp_agents: &[ConfiguredAcpAgent],
) -> BackendSetupPayload {
    let platform = host_platform();
    // Probe every backend concurrently. Each probe spawns a real `<cli>
    // --version` subprocess capped at a 2s timeout, so running them
    // sequentially made host startup wait for the sum of all probes.
    let backends = futures_util::future::join_all(
        [
            BackendKind::Kiro,
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Antigravity,
            BackendKind::Hermes,
        ]
        .into_iter()
        .map(|kind| probe_backend(kind, platform, acp_agents)),
    )
    .await;
    BackendSetupPayload { backends }
}

/// One configured ACP agent, as the setup probe sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredAcpAgent {
    pub label: String,
    pub command: String,
    pub adapter: AcpAdapterId,
}

/// Backend setup with no CLI probing for hosts explicitly configured to skip
/// installed-provider detection.
pub(crate) fn stub_backend_setup() -> BackendSetupPayload {
    let platform = host_platform();
    let backends = [
        BackendKind::Kiro,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Hermes,
    ]
    .into_iter()
    .map(|kind| {
        let install_command = install_command(kind, platform);
        let status = if install_command.is_none() {
            BackendSetupStatus::Unsupported
        } else {
            BackendSetupStatus::NotInstalled
        };
        BackendSetupInfo {
            backend_kind: kind,
            status,
            installed_version: None,
            docs_url: docs_url(kind),
            install_command,
            diagnostic: None,
            sign_in_command: sign_in_command(kind, None),
        }
    })
    .collect();
    BackendSetupPayload { backends }
}

pub(crate) async fn prepare_runnable_command(
    backend_kind: BackendKind,
    action: BackendSetupAction,
) -> Result<Option<PreparedBackendSetupCommand>, String> {
    let platform = host_platform();
    let payload = collect_backend_setup(&[]).await;
    let info = payload
        .backends
        .into_iter()
        .find(|info| info.backend_kind == backend_kind);
    let Some(info) = info else {
        return Ok(None);
    };

    let command = match action {
        BackendSetupAction::Install => info.install_command,
        BackendSetupAction::SignIn => info.sign_in_command,
    };
    let Some(command) = command.filter(|command| command.runnable) else {
        return Ok(None);
    };
    stage_backend_setup_command(&command.command, platform).map(Some)
}

fn stage_backend_setup_command(
    command: &str,
    platform: HostPlatform,
) -> Result<PreparedBackendSetupCommand, String> {
    let suffix = if platform == HostPlatform::Windows {
        ".ps1"
    } else {
        ".sh"
    };
    let mut staged = tempfile::Builder::new()
        .prefix("tyde-backend-setup-")
        .suffix(suffix)
        .tempfile()
        .map_err(|error| format!("failed to create private backend setup script: {error}"))?;

    #[cfg(unix)]
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to secure backend setup script: {error}"))?;

    let path = staged.path().to_path_buf();
    let path_text = path.to_string_lossy();
    let (program, arguments, display_command, script) = if platform == HostPlatform::Windows {
        let display_path = format!("\"{}\"", path_text.replace('"', "`\""));
        let display_command =
            format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -File {display_path}");
        let display_literal = display_command.replace('\'', "''");
        (
            "powershell.exe".to_owned(),
            vec![
                "-NoProfile".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-File".to_owned(),
                path_text.into_owned(),
            ],
            display_command,
            format!("Write-Output '$ {display_literal}'\n{command}\n"),
        )
    } else {
        let display_command = format!("/bin/sh {}", shell_quote(&path_text));
        let display_literal = shell_quote(&format!("$ {display_command}"));
        (
            "/bin/sh".to_owned(),
            vec![path_text.into_owned()],
            display_command,
            format!("printf '%s\\n' {display_literal}\n{command}\n"),
        )
    };

    staged
        .write_all(script.as_bytes())
        .map_err(|error| format!("failed to write backend setup script: {error}"))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|error| format!("failed to sync backend setup script: {error}"))?;
    let (file, staged_script) = staged
        .keep()
        .map_err(|error| format!("failed to retain backend setup script: {error}"))?;
    drop(file);

    Ok(PreparedBackendSetupCommand {
        program,
        arguments,
        display_command,
        staged_script,
    })
}

async fn probe_backend(
    kind: BackendKind,
    platform: HostPlatform,
    acp_agents: &[ConfiguredAcpAgent],
) -> BackendSetupInfo {
    let probe = match kind {
        BackendKind::Tycode => ProbeResult::not_installed(),
        BackendKind::Kiro => probe_acp_agents(acp_agents).await,
        BackendKind::Claude => probe_candidates(&command_candidates(CLAUDE_CLI_CANDIDATES)).await,
        BackendKind::Codex => probe_candidates(&command_candidates(CODEX_CLI_CANDIDATES)).await,
        BackendKind::Antigravity => {
            probe_candidates(&command_candidates(ANTIGRAVITY_CLI_CANDIDATES)).await
        }
        BackendKind::Hermes => probe_hermes_gateway().await,
    };

    backend_setup_info_from_probe(kind, platform, probe)
}

fn backend_setup_info_from_probe(
    kind: BackendKind,
    platform: HostPlatform,
    probe: ProbeResult,
) -> BackendSetupInfo {
    let docs_url = docs_url(kind);
    let install_command = install_command(kind, platform);
    let status = backend_setup_status_for_probe(probe.status, install_command.is_some());
    let sign_in_command = sign_in_command(kind, probe.hermes_executable.as_deref());

    BackendSetupInfo {
        backend_kind: kind,
        status,
        installed_version: probe.version,
        docs_url,
        install_command,
        diagnostic: probe.diagnostic,
        sign_in_command,
    }
}

fn backend_setup_status_for_probe(
    probe_status: BackendSetupStatus,
    has_install_command: bool,
) -> BackendSetupStatus {
    match probe_status {
        BackendSetupStatus::Installed | BackendSetupStatus::Unavailable => probe_status,
        BackendSetupStatus::NotInstalled if !has_install_command => BackendSetupStatus::Unsupported,
        BackendSetupStatus::NotInstalled | BackendSetupStatus::Unsupported => probe_status,
    }
}

struct ProbeResult {
    status: BackendSetupStatus,
    version: Option<String>,
    diagnostic: Option<BackendSetupDiagnostic>,
    hermes_executable: Option<String>,
}

impl ProbeResult {
    fn installed(version: Option<String>) -> Self {
        Self {
            status: BackendSetupStatus::Installed,
            version,
            diagnostic: None,
            hermes_executable: None,
        }
    }

    fn not_installed() -> Self {
        Self {
            status: BackendSetupStatus::NotInstalled,
            version: None,
            diagnostic: None,
            hermes_executable: None,
        }
    }

    fn not_installed_with_diagnostic(diagnostic: BackendSetupDiagnostic) -> Self {
        Self {
            status: BackendSetupStatus::NotInstalled,
            version: None,
            diagnostic: Some(diagnostic),
            hermes_executable: None,
        }
    }

    fn unavailable(diagnostic: BackendSetupDiagnostic) -> Self {
        Self {
            status: BackendSetupStatus::Unavailable,
            version: None,
            diagnostic: Some(diagnostic),
            hermes_executable: None,
        }
    }

    fn with_hermes_executable(mut self, executable: String) -> Self {
        self.hermes_executable = Some(executable);
        self
    }
}

/// ACP is the one backend whose "is it installed?" answer depends on user
/// configuration: the built-in Kiro agent plus any agent the user pointed at
/// their own binary.
///
/// The row reports installed when *any* configured agent resolves, because ACP
/// is usable at that point. When some resolve and others do not, the working
/// status is kept but the diagnostic names the broken ones — a user who
/// mistyped one command should not have the whole backend read as fine with no
/// hint, nor as broken when their other agents work.
async fn probe_acp_agents(agents: &[ConfiguredAcpAgent]) -> ProbeResult {
    if agents.is_empty() {
        let fallback = ConfiguredAcpAgent {
            label: "Kiro (ACP)".to_owned(),
            command: String::new(),
            adapter: AcpAdapterId::Kiro,
        };
        let probe = probe_acp_agent(&fallback).await;
        return aggregate_acp_agent_probes(vec![AcpAgentProbe {
            label: fallback.label,
            probe,
        }]);
    }

    let mut probes = Vec::with_capacity(agents.len());
    for agent in agents {
        probes.push(AcpAgentProbe {
            label: agent.label.clone(),
            probe: probe_acp_agent(agent).await,
        });
    }
    aggregate_acp_agent_probes(probes)
}

struct AcpAgentProbe {
    label: String,
    probe: ProbeResult,
}

fn acp_agent_command_candidates(agent: &ConfiguredAcpAgent) -> Vec<String> {
    let command = agent.command.trim();
    if !command.is_empty() {
        return vec![command.to_owned()];
    }
    match agent.adapter {
        AcpAdapterId::Kiro => KIRO_CLI_CANDIDATES
            .iter()
            .map(|candidate| (*candidate).to_owned())
            .collect(),
        AcpAdapterId::Stock => Vec::new(),
    }
}

async fn probe_acp_agent(agent: &ConfiguredAcpAgent) -> ProbeResult {
    let candidates = acp_agent_command_candidates(agent);
    if candidates.is_empty() {
        return ProbeResult::not_installed_with_diagnostic(BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandNotFound,
            message: "no ACP command is configured".to_owned(),
        });
    }
    probe_acp_candidates(&candidates).await
}

async fn probe_acp_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        let command = match classify_acp_candidate(candidate) {
            AcpCandidateClassification::Missing => continue,
            AcpCandidateClassification::Runnable(command) => command,
            AcpCandidateClassification::Unavailable(diagnostic) => {
                return ProbeResult::unavailable(diagnostic);
            }
        };

        return match probe_command(&command).await {
            Ok(version) => ProbeResult::installed(version),
            Err(failure) => ProbeResult::unavailable(version_command_failure(&command, failure)),
        };
    }

    ProbeResult::not_installed_with_diagnostic(BackendSetupDiagnostic {
        code: BackendSetupDiagnosticCode::CommandNotFound,
        message: if candidates.len() == 1 {
            format!("command {} was not found", candidates[0])
        } else {
            format!("none of the commands {} were found", candidates.join(", "))
        },
    })
}

enum AcpCandidateClassification {
    Missing,
    Runnable(String),
    Unavailable(BackendSetupDiagnostic),
}

fn classify_acp_candidate(candidate: &str) -> AcpCandidateClassification {
    let path = Path::new(candidate);
    if path.components().count() == 1 {
        return process_env::find_executable_in_path(candidate)
            .map_or(AcpCandidateClassification::Missing, |path| {
                AcpCandidateClassification::Runnable(path.to_string_lossy().into_owned())
            });
    }
    classify_explicit_acp_candidate_with(candidate, |path| std::fs::metadata(path))
}

fn classify_explicit_acp_candidate_with(
    candidate: &str,
    metadata: impl FnOnce(&Path) -> std::io::Result<std::fs::Metadata>,
) -> AcpCandidateClassification {
    match metadata(Path::new(candidate)) {
        Ok(_) => AcpCandidateClassification::Runnable(candidate.to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            AcpCandidateClassification::Missing
        }
        Err(error) => {
            let candidate = bounded_acp_text(candidate, ACP_AGGREGATE_DETAIL_MAX_BYTES);
            let message = bounded_acp_text(
                &format!("could not inspect ACP command {candidate}: {error}"),
                ACP_COMMAND_DIAGNOSTIC_MAX_BYTES,
            );
            AcpCandidateClassification::Unavailable(BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message,
            })
        }
    }
}

fn aggregate_acp_agent_probes(probes: Vec<AcpAgentProbe>) -> ProbeResult {
    let installed_version = probes
        .iter()
        .find(|result| result.probe.status == BackendSetupStatus::Installed)
        .and_then(|result| result.probe.version.clone());
    let has_installed = probes
        .iter()
        .any(|result| result.probe.status == BackendSetupStatus::Installed);
    let failures = probes
        .iter()
        .filter(|result| result.probe.status != BackendSetupStatus::Installed)
        .collect::<Vec<_>>();

    let mut result = if has_installed {
        ProbeResult::installed(installed_version)
    } else if failures
        .iter()
        .any(|result| result.probe.status == BackendSetupStatus::Unavailable)
    {
        ProbeResult::unavailable(aggregate_acp_diagnostic(&failures))
    } else if failures.is_empty() {
        ProbeResult::not_installed()
    } else {
        ProbeResult::not_installed_with_diagnostic(aggregate_acp_diagnostic(&failures))
    };

    if has_installed && !failures.is_empty() {
        result.diagnostic = Some(aggregate_acp_diagnostic(&failures));
    }
    result
}

fn aggregate_acp_diagnostic(failures: &[&AcpAgentProbe]) -> BackendSetupDiagnostic {
    let code = failures
        .iter()
        .find(|result| result.probe.status == BackendSetupStatus::Unavailable)
        .and_then(|result| result.probe.diagnostic.as_ref())
        .or_else(|| {
            failures
                .iter()
                .find_map(|result| result.probe.diagnostic.as_ref())
        })
        .map(|diagnostic| diagnostic.code)
        .unwrap_or(BackendSetupDiagnosticCode::CommandNotFound);
    let omitted = failures.len().saturating_sub(ACP_AGGREGATE_MAX_FAILURES);
    let mut clauses = failures
        .iter()
        .take(ACP_AGGREGATE_MAX_FAILURES)
        .map(|result| {
            let detail = result
                .probe
                .diagnostic
                .as_ref()
                .map(|diagnostic| diagnostic.message.as_str())
                .unwrap_or("ACP command is unavailable");
            let label = bounded_acp_text(&result.label, ACP_AGGREGATE_LABEL_MAX_BYTES);
            let detail = bounded_acp_text(detail, ACP_AGGREGATE_DETAIL_MAX_BYTES);
            format!("{label}: {detail}")
        })
        .collect::<Vec<_>>();
    if omitted > 0 {
        clauses.push(format!("[truncated] {omitted} additional ACP profile(s)"));
    }
    let message = bounded_acp_text(&clauses.join("; "), ACP_AGGREGATE_MESSAGE_MAX_BYTES);
    BackendSetupDiagnostic { code, message }
}

fn bounded_acp_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    if max_bytes <= ACP_DIAGNOSTIC_TRUNCATION_MARKER.len() {
        let mut end = max_bytes;
        while !ACP_DIAGNOSTIC_TRUNCATION_MARKER.is_char_boundary(end) {
            end -= 1;
        }
        return ACP_DIAGNOSTIC_TRUNCATION_MARKER[..end].to_owned();
    }
    let mut end = max_bytes - ACP_DIAGNOSTIC_TRUNCATION_MARKER.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], ACP_DIAGNOSTIC_TRUNCATION_MARKER)
}

async fn probe_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        if Path::new(candidate).components().count() == 1
            && process_env::find_executable_in_path(candidate).is_none()
        {
            continue;
        }
        match probe_command(candidate).await {
            Ok(version) => return ProbeResult::installed(version),
            Err(failure) => {
                return ProbeResult::unavailable(version_command_failure(candidate, failure));
            }
        }
    }
    ProbeResult::not_installed()
}

async fn probe_command(command: &str) -> Result<Option<String>, VersionCommandFailure> {
    let child_path = process_env::resolved_child_process_path().map(std::ffi::OsStr::to_os_string);
    let output = run_version_command_with_child_path(command, child_path).await?;
    let version = output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string());
    Ok(version)
}

struct VersionCommandOutput {
    stdout: String,
    stderr: String,
}

enum VersionCommandFailure {
    Start(String),
    TimedOut,
    NonZero {
        status: String,
        stdout: String,
        stderr: String,
    },
}

fn version_command_failure(
    command: &str,
    failure: VersionCommandFailure,
) -> BackendSetupDiagnostic {
    match failure {
        VersionCommandFailure::Start(error) => BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandFailed,
            message: format!(
                "Tyde found {command}, but could not run its --version check: {error}"
            ),
        },
        VersionCommandFailure::TimedOut => BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandTimedOut,
            message: format!(
                "Tyde found {command}, but its --version check did not finish within 60 seconds"
            ),
        },
        VersionCommandFailure::NonZero {
            status,
            stdout,
            stderr,
        } => {
            let detail = stderr
                .lines()
                .chain(stdout.lines())
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| format!(": {line}"))
                .unwrap_or_default();
            BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message: format!(
                    "Tyde found {command}, but its --version check exited with {status}{detail}"
                ),
            }
        }
    }
}

async fn wait_for_version_command_group(
    child: &mut command_group::AsyncGroupChild,
    started: Instant,
    command: &str,
) -> std::io::Result<std::process::ExitStatus> {
    trace_version_probe_stage(started, command, "try_wait_poll_started");
    // command-group's Unix group wait is not cancellation-safe, so keep the
    // whole process group authoritative through cancellable polling.
    loop {
        if let Some(status) = child.try_wait()? {
            trace_version_probe_stage(started, command, "try_wait_poll_completed");
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn trace_version_probe_stage(started: Instant, command: &str, stage: &str) {
    let elapsed_ms = started.elapsed().as_millis();
    tracing::debug!(command, stage, elapsed_ms = %elapsed_ms, "version probe stage");
}

async fn run_version_command_with_child_path(
    command: &str,
    child_path: Option<std::ffi::OsString>,
) -> Result<VersionCommandOutput, VersionCommandFailure> {
    let started = Instant::now();
    trace_version_probe_stage(started, command, "function_started");
    let mut command = Command::new(command);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = child_path {
        command.env("PATH", path);
    }
    let command_name = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    trace_version_probe_stage(started, &command_name, "group_spawn_started");
    let mut child = command
        .group_spawn()
        .map_err(|error| VersionCommandFailure::Start(format!("failed to spawn: {error}")))?;
    trace_version_probe_stage(started, &command_name, "group_spawn_completed");
    let mut stdout_pipe = child.inner().stdout.take().ok_or_else(|| {
        VersionCommandFailure::Start("failed to capture standard output".to_owned())
    })?;
    let mut stderr_pipe = child.inner().stderr.take().ok_or_else(|| {
        VersionCommandFailure::Start("failed to capture standard error".to_owned())
    })?;

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let probe = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            wait_for_version_command_group(&mut child, started, &command_name),
            async {
                trace_version_probe_stage(started, &command_name, "stdout_read_started");
                let result = stdout_pipe.read_to_end(&mut stdout_bytes).await;
                trace_version_probe_stage(started, &command_name, "stdout_read_completed");
                result
            },
            async {
                trace_version_probe_stage(started, &command_name, "stderr_read_started");
                let result = stderr_pipe.read_to_end(&mut stderr_bytes).await;
                trace_version_probe_stage(started, &command_name, "stderr_read_completed");
                result
            },
        )
    })
    .await;
    trace_version_probe_stage(started, &command_name, "probe_join_completed");
    let status = match probe {
        Ok((Ok(status), Ok(_), Ok(_))) => status,
        Ok((status, stdout, stderr)) => {
            trace_version_probe_stage(started, &command_name, "function_returning_read_error");
            return Err(VersionCommandFailure::Start(format!(
                "failed while waiting or reading output: status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
            )));
        }
        Err(_) => {
            trace_version_probe_stage(started, &command_name, "probe_timeout_fired");
            drop(stdout_pipe);
            drop(stderr_pipe);
            trace_version_probe_stage(started, &command_name, "pipe_readers_dropped");
            let kill_result = child.start_kill();
            trace_version_probe_stage(started, &command_name, "start_kill_returned");
            if let Err(error) = kill_result {
                tracing::warn!(%error, "failed to kill timed-out version command group");
            }
            trace_version_probe_stage(started, &command_name, "background_reap_spawning");
            let reap_command = command_name.clone();
            tokio::spawn(async move {
                trace_version_probe_stage(started, &reap_command, "background_reap_started");
                if let Err(error) = child.wait().await {
                    tracing::warn!(%error, "failed to reap timed-out version command group");
                }
                trace_version_probe_stage(started, &reap_command, "background_reap_completed");
            });
            trace_version_probe_stage(started, &command_name, "background_reap_spawned");
            trace_version_probe_stage(started, &command_name, "function_returning_timeout");
            return Err(VersionCommandFailure::TimedOut);
        }
    };

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    if !status.success() {
        trace_version_probe_stage(started, &command_name, "function_returning_nonzero");
        return Err(VersionCommandFailure::NonZero {
            status: status.to_string(),
            stdout,
            stderr,
        });
    }
    trace_version_probe_stage(started, &command_name, "function_returning_success");
    Ok(VersionCommandOutput { stdout, stderr })
}

async fn probe_explicit_hermes_python(candidate: &str) -> ProbeResult {
    match probe_hermes_python_command(candidate).await {
        Ok(version) => ProbeResult::installed(version),
        Err(err) => ProbeResult::unavailable(hermes_failure_diagnostic(
            err.explicit_override("HERMES_PYTHON"),
        )),
    }
}

async fn probe_hermes_gateway() -> ProbeResult {
    probe_hermes_gateway_with_sources(
        crate::backend::hermes::explicit_hermes_python(),
        crate::backend::hermes::explicit_hermes_executable(),
        crate::backend::hermes::hermes_executable_candidates(),
    )
    .await
}

async fn probe_hermes_gateway_with_sources(
    explicit_python: Option<String>,
    explicit_executable: Option<String>,
    executable_candidates: Vec<String>,
) -> ProbeResult {
    if let Some(candidate) = explicit_python {
        return probe_explicit_hermes_python(&candidate).await;
    }

    if let Some(candidate) = explicit_executable {
        return match crate::backend::hermes::probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                ProbeResult::installed(probe.version).with_hermes_executable(probe.executable)
            }
            Err(err) => ProbeResult::unavailable(hermes_failure_diagnostic(
                err.explicit_override("HERMES_EXECUTABLE"),
            )),
        };
    }

    let mut first_failure = None;
    for candidate in executable_candidates {
        match crate::backend::hermes::probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                return ProbeResult::installed(probe.version)
                    .with_hermes_executable(probe.executable);
            }
            Err(err) => {
                tracing::debug!("Hermes executable candidate {candidate} probe failed: {err}");
                if err.code != BackendSetupDiagnosticCode::CommandNotFound || candidate != "hermes"
                {
                    first_failure.get_or_insert(err);
                }
            }
        }
    }

    let failure = crate::backend::hermes::hermes_cli_required_failure(first_failure);
    let diagnostic = hermes_failure_diagnostic(failure.clone());
    if failure.code == BackendSetupDiagnosticCode::CommandNotFound {
        ProbeResult::not_installed_with_diagnostic(diagnostic)
    } else {
        ProbeResult::unavailable(diagnostic)
    }
}

async fn probe_hermes_python_command(
    command: &str,
) -> Result<Option<String>, crate::backend::hermes::HermesProbeFailure> {
    crate::backend::hermes::probe_hermes_python_gateway_import(command)
        .await
        .map(|()| Some(format!("{command} -m {HERMES_PYTHON_MODULE}")))
}

fn hermes_failure_diagnostic(
    failure: crate::backend::hermes::HermesProbeFailure,
) -> BackendSetupDiagnostic {
    BackendSetupDiagnostic {
        code: failure.code,
        message: failure.message,
    }
}

fn command_candidates(defaults: &[&str]) -> Vec<String> {
    let mut candidates = Vec::<String>::new();
    for default in defaults {
        if let Some(path) = process_env::find_executable_in_path(default) {
            let path = path.to_string_lossy().to_string();
            if !candidates.contains(&path) {
                candidates.push(path);
            }
        }

        let candidate = default.to_string();
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn docs_url(kind: BackendKind) -> String {
    match kind {
        BackendKind::Tycode => "https://github.com/tigy32/Tycode".to_owned(),
        BackendKind::Kiro => "https://kiro.dev/docs/cli/installation/".to_string(),
        BackendKind::Claude => {
            "https://docs.anthropic.com/en/docs/claude-code/getting-started".to_string()
        }
        BackendKind::Codex => "https://help.openai.com/en/articles/11096431".to_string(),
        BackendKind::Antigravity => "https://antigravity.google/cli".to_string(),
        BackendKind::Hermes => {
            "https://github.com/NousResearch/hermes-agent/tree/main/ui-tui".to_string()
        }
    }
}

fn install_command(kind: BackendKind, platform: HostPlatform) -> Option<BackendSetupCommand> {
    match kind {
        BackendKind::Tycode => None,
        BackendKind::Kiro => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description: "Install Kiro CLI on this host. Kiro opens a browser for authentication after install.".to_string(),
            command: match platform {
                HostPlatform::Windows => {
                    "powershell -ExecutionPolicy Bypass -Command \"irm 'https://cli.kiro.dev/install.ps1' | iex\"".to_string()
                }
                _ => "curl -fsSL https://cli.kiro.dev/install | bash".to_string(),
            },
            display_command: None,
            runnable: true,
        }),
        BackendKind::Claude => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description:
                "Install Claude Code with npm. Anthropic documents Node.js 18+ as a prerequisite."
                    .to_string(),
            command: "npm install -g @anthropic-ai/claude-code".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Codex => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description: "Install Codex CLI with npm.".to_string(),
            command: "npm install -g @openai/codex".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Antigravity => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description: "Install Antigravity CLI on this host.".to_string(),
            command: "curl -fsSL https://antigravity.google/cli/install.sh | bash".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Hermes => Some(BackendSetupCommand {
            title: "Install Hermes".to_string(),
            description: "Install Hermes Agent so the hermes executable is on PATH. Set HERMES_EXECUTABLE only if Tyde cannot resolve it.".to_string(),
            command: "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash".to_string(),
            display_command: None,
            runnable: true,
        }),
    }
}

fn sign_in_command(
    kind: BackendKind,
    hermes_executable: Option<&str>,
) -> Option<BackendSetupCommand> {
    match kind {
        BackendKind::Tycode => None,
        BackendKind::Kiro => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start the Kiro login flow for this host.".to_string(),
            command: "kiro-cli login".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Claude => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start Claude Code so it can prompt for login on this host.".to_string(),
            command: "claude".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Codex => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start the Codex login flow for this host.".to_string(),
            command: "codex --login".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Antigravity => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start Antigravity CLI so it can prompt for login on this host."
                .to_string(),
            command: "agy".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Hermes => {
            let executable = hermes_executable?;
            Some(BackendSetupCommand {
                title: "Sign In".to_string(),
                description: "Run the Hermes setup wizard for provider authentication.".to_string(),
                command: format!("{} setup", shell_quote(executable)),
                display_command: Some(format!("{executable} setup")),
                runnable: true,
            })
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
