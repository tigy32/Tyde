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

pub(crate) const TYCODE_VERSION: &str = "0.10.0";
// Keep the stable grouped-settings adoption floor synchronized in its invariant test.
const TYCODE_RELEASE_BASE_URL: &str = "https://github.com/tigy32/Tycode/releases/download";
const TYCODE_SUBPROCESS_SHA256_AARCH64_APPLE_DARWIN: &str =
    "3a3b4ea1bb74bcf7b9078ba21de954468c944613e0573b6ed03abb81670ca96e";
const TYCODE_SUBPROCESS_SHA256_X86_64_APPLE_DARWIN: &str =
    "c1bbfc5b2a64d309d3d1c13a7b9057a5946a8c6e2cb66cc15019b97053eb6c1e";
const TYCODE_SUBPROCESS_SHA256_AARCH64_UNKNOWN_LINUX_MUSL: &str =
    "1844c3d98d126dbdf49e661d94930de6feb7c53a3f0806b7b0b797e34ad3481d";
const TYCODE_SUBPROCESS_SHA256_X86_64_UNKNOWN_LINUX_MUSL: &str =
    "abfcd6865151ba48d33d582b1fa706460d41b5807d4c194778c757102ff1d6c7";
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
            BackendKind::Tycode,
            BackendKind::Acp,
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
        BackendKind::Tycode,
        BackendKind::Acp,
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

pub(crate) fn tycode_versioned_binary_path() -> Result<PathBuf, String> {
    Ok(tycode_versioned_binary_path_for_home(&home_dir()?))
}

fn tycode_versioned_binary_path_for_home(home: &Path) -> PathBuf {
    home.join(".tyde")
        .join("tycode")
        .join(TYCODE_VERSION)
        .join("tycode-subprocess")
}

pub(crate) fn resolve_tycode_binary_path() -> Option<String> {
    let home = home_dir().ok()?;
    resolve_tycode_binary_path_for_home(&home)
}

fn resolve_tycode_binary_path_for_home(home: &Path) -> Option<String> {
    let path = tycode_versioned_binary_path_for_home(home);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    metadata
        .file_type()
        .is_file()
        .then(|| path.to_string_lossy().to_string())
}

async fn probe_backend(
    kind: BackendKind,
    platform: HostPlatform,
    acp_agents: &[ConfiguredAcpAgent],
) -> BackendSetupInfo {
    let probe = match kind {
        BackendKind::Tycode => probe_installed_tycode().await,
        BackendKind::Acp => probe_acp_agents(acp_agents).await,
        BackendKind::Claude => probe_candidates(&command_candidates(CLAUDE_CLI_CANDIDATES)).await,
        BackendKind::Codex => probe_candidates(&command_candidates(CODEX_CLI_CANDIDATES)).await,
        BackendKind::Antigravity => probe_candidates(&antigravity_command_candidates()).await,
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

async fn probe_installed_tycode() -> ProbeResult {
    probe_resolved_tycode(resolve_tycode_binary_path()).await
}

async fn probe_resolved_tycode(command: Option<String>) -> ProbeResult {
    let Some(command) = command else {
        return ProbeResult::not_installed();
    };
    match validate_tycode_command(&command).await {
        TycodeCommandValidation::Compatible { version } => ProbeResult::installed(Some(version)),
        TycodeCommandValidation::Incompatible { diagnostic } => {
            ProbeResult::unavailable(diagnostic)
        }
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
                "Tyde found {command}, but its --version check did not finish within 2 seconds"
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

async fn run_version_command(command: &str) -> Result<VersionCommandOutput, VersionCommandFailure> {
    // The pinned Tycode command is already an explicit path. Resolving a login
    // shell PATH here would synchronously run outside the probe timeout.
    run_version_command_with_child_path(command, None).await
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
    let probe = tokio::time::timeout(Duration::from_secs(2), async {
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

enum TycodeCommandValidation {
    Compatible { version: String },
    Incompatible { diagnostic: BackendSetupDiagnostic },
}

pub(crate) async fn ensure_tycode_command_compatible(command: &str) -> Result<String, String> {
    let expected_path = tycode_versioned_binary_path()?;
    if Path::new(command) != expected_path {
        return Err(format!(
            "Tyde only runs the installed checksum-pinned Tycode artifact at {}; refusing {command}",
            expected_path.display()
        ));
    }
    match validate_tycode_command(command).await {
        TycodeCommandValidation::Compatible { version: _ } => Ok(command.to_string()),
        TycodeCommandValidation::Incompatible { diagnostic } => Err(diagnostic.message),
    }
}

async fn validate_tycode_command(command: &str) -> TycodeCommandValidation {
    let output = match run_version_command(command).await {
        Ok(output) => output,
        Err(failure) => {
            return TycodeCommandValidation::Incompatible {
                diagnostic: tycode_version_command_failure(command, failure),
            };
        }
    };
    let expected = format!("tycode-subprocess {TYCODE_VERSION}");
    if exact_tycode_version_output(&output, &expected) {
        return TycodeCommandValidation::Compatible { version: expected };
    }
    let Some(version_line) = parse_tycode_version_output(&output.stdout, &output.stderr) else {
        return TycodeCommandValidation::Incompatible {
            diagnostic: BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message: format!(
                    "Tycode command {command} did not report the exact expected --version output {expected:?}"
                ),
            },
        };
    };
    let Some(version) = parse_tycode_reported_version(&version_line) else {
        return TycodeCommandValidation::Incompatible {
            diagnostic: BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message: format!(
                    "Tycode command {command} reported unparseable version line {version_line:?}; Tyde requires tycode-subprocess {TYCODE_VERSION}"
                ),
            },
        };
    };
    TycodeCommandValidation::Incompatible {
        diagnostic: BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandFailed,
            message: format!(
                "Tycode command {command} reported {version_line:?} (version {version}), but Tyde requires exact --version output {expected:?} from the pinned installed artifact"
            ),
        },
    }
}

fn exact_tycode_version_output(output: &VersionCommandOutput, expected: &str) -> bool {
    output.stderr.is_empty()
        && (output.stdout == expected
            || output.stdout == format!("{expected}\n")
            || output.stdout == format!("{expected}\r\n"))
}

fn tycode_version_command_failure(
    command: &str,
    failure: VersionCommandFailure,
) -> BackendSetupDiagnostic {
    let expected = format!("tycode-subprocess {TYCODE_VERSION}");
    let message = match failure {
        VersionCommandFailure::Start(error) => {
            format!("Tycode command {command} could not run its required --version probe: {error}")
        }
        VersionCommandFailure::TimedOut => {
            format!("Tycode command {command} timed out during its required --version probe")
        }
        VersionCommandFailure::NonZero {
            status,
            stdout,
            stderr,
        } => {
            let output = VersionCommandOutput { stdout, stderr };
            if exact_tycode_version_output(&output, &expected) {
                format!(
                    "Tycode command {command} reported exact expected --version output {expected:?} but exited unsuccessfully with {status}"
                )
            } else {
                format!(
                    "Tycode command {command} exited unsuccessfully with {status} during --version; Tyde requires exact output {expected:?}"
                )
            }
        }
    };
    BackendSetupDiagnostic {
        code: BackendSetupDiagnosticCode::CommandFailed,
        message,
    }
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

fn antigravity_command_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(home) = home_dir() {
        let local = home.join(".local").join("bin").join("agy");
        if local.is_file() {
            candidates.push(local.to_string_lossy().to_string());
        }
    }
    for candidate in command_candidates(ANTIGRAVITY_CLI_CANDIDATES) {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
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

fn parse_tycode_version_output(stdout: &str, stderr: &str) -> Option<String> {
    for line in stdout.lines().chain(stderr.lines()).map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(version) = parse_tycode_plain_text_version_line(line) {
            return Some(version);
        }
        if let Some(version) = parse_tycode_version_frame(line) {
            return Some(version);
        }
    }
    None
}

fn parse_tycode_plain_text_version_line(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    let binary = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if binary != "tycode-subprocess" && binary != "tycode" {
        return None;
    }
    if !looks_like_semver(version) {
        return None;
    }
    Some(line.to_string())
}

fn parse_tycode_reported_version(line: &str) -> Option<&str> {
    let mut parts = line.split_whitespace();
    let binary = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if binary != "tycode-subprocess" && binary != "tycode" {
        return None;
    }
    looks_like_semver(version).then_some(version)
}

fn parse_tycode_version_frame(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = value.get("kind").and_then(serde_json::Value::as_str)?;
    if !kind.eq_ignore_ascii_case("version") {
        return None;
    }
    let version = value
        .get("version")
        .or_else(|| value.get("data").and_then(|data| data.get("version")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())?;
    let binary = value
        .get("binary")
        .or_else(|| value.get("data").and_then(|data| data.get("binary")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .unwrap_or("tycode-subprocess");
    Some(format!("{binary} {version}"))
}

fn looks_like_semver(value: &str) -> bool {
    let mut saw_digit = false;
    let mut saw_dot = false;
    for byte in value.bytes() {
        match byte {
            b'0'..=b'9' => saw_digit = true,
            b'.' => saw_dot = true,
            b'-' | b'+' | b'a'..=b'z' | b'A'..=b'Z' => {}
            _ => return false,
        }
    }
    saw_digit && saw_dot
}

fn docs_url(kind: BackendKind) -> String {
    match kind {
        BackendKind::Tycode => {
            format!("https://github.com/tigy32/Tycode/releases/tag/v{TYCODE_VERSION}")
        }
        BackendKind::Acp => "https://kiro.dev/docs/cli/installation/".to_string(),
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
        BackendKind::Tycode => tycode_install_command(platform),
        BackendKind::Acp => Some(BackendSetupCommand {
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
        BackendKind::Acp => Some(BackendSetupCommand {
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

fn tycode_install_command(platform: HostPlatform) -> Option<BackendSetupCommand> {
    match platform {
        HostPlatform::Macos | HostPlatform::Linux => Some(BackendSetupCommand {
            title: "Install release artifact".to_string(),
            description: format!(
                "Download the Tycode v{TYCODE_VERSION} release artifact for this host, extract tycode-subprocess, and install it into ~/.tyde/tycode/{TYCODE_VERSION}."
            ),
            command: tycode_unix_install_command(),
            display_command: Some(format!(
                "/bin/sh <private Tyde v{TYCODE_VERSION} setup script>"
            )),
            runnable: true,
        }),
        HostPlatform::Windows | HostPlatform::Other => None,
    }
}

fn tycode_unix_install_command() -> String {
    format!(
        r#"set -eu

VERSION="{version}"
BASE_URL="{release_base}/v{version}"
HOME_DIR="${{HOME:-}}"
[ -n "$HOME_DIR" ] || {{ echo "HOME is empty" >&2; exit 1; }}
command -v python3 >/dev/null 2>&1 || {{ echo "python3 is required for Tycode install" >&2; exit 1; }}
OS="$(uname -s)"
ARCH="$(uname -m)"

sha256_file() {{
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{{print $1}}'
    return
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{{print $1}}'
    return
  fi
  echo "No SHA256 tool found" >&2
  exit 1
}}

fsync_path() {{
  python3 - "$1" <<'PY'
import os
import sys

path = sys.argv[1]
fd = os.open(path, os.O_RDONLY)
try:
    os.fsync(fd)
finally:
    os.close(fd)
PY
}}

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64)
        ASSET="tycode-subprocess-aarch64-apple-darwin.tar.xz"
        EXPECTED_SHA256="{sha_macos_arm64}"
        ;;
      x86_64|amd64)
        ASSET="tycode-subprocess-x86_64-apple-darwin.tar.xz"
        EXPECTED_SHA256="{sha_macos_x64}"
        ;;
      *) echo "Unsupported Tycode architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      arm64|aarch64)
        ASSET="tycode-subprocess-aarch64-unknown-linux-musl.tar.xz"
        EXPECTED_SHA256="{sha_linux_arm64}"
        ;;
      x86_64|amd64)
        ASSET="tycode-subprocess-x86_64-unknown-linux-musl.tar.xz"
        EXPECTED_SHA256="{sha_linux_x64}"
        ;;
      *) echo "Unsupported Tycode architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported Tycode OS: $OS" >&2
    exit 1
    ;;
esac

URL="${{BASE_URL}}/${{ASSET}}"
INSTALL_ROOT="${{HOME_DIR}}/.tyde/tycode"
DEST_DIR="${{INSTALL_ROOT}}/${{VERSION}}"
TMP_ROOT="$(mktemp -d)"
ARCHIVE="${{TMP_ROOT}}/${{ASSET}}"
STAGED_BINARY="${{DEST_DIR}}/tycode-subprocess.tmp.$$"
FINAL_BINARY="${{DEST_DIR}}/tycode-subprocess"
cleanup() {{
  rm -rf "$TMP_ROOT"
  rm -f "$STAGED_BINARY"
}}
trap cleanup EXIT

mkdir -p "$DEST_DIR"
curl -fL "$URL" -o "$ARCHIVE"
ACTUAL_SHA256="$(sha256_file "$ARCHIVE")"
[ "$ACTUAL_SHA256" = "$EXPECTED_SHA256" ] || {{
  echo "Tycode SHA256 mismatch for $ASSET: expected $EXPECTED_SHA256 got $ACTUAL_SHA256" >&2
  exit 1
}}
tar -xJf "$ARCHIVE" -C "$TMP_ROOT"
BINARY="$(find "$TMP_ROOT" -type f -name 'tycode-subprocess' | head -n 1)"
[ -n "$BINARY" ] || {{ echo "Downloaded Tycode asset did not contain tycode-subprocess" >&2; exit 1; }}
install -m 755 "$BINARY" "$STAGED_BINARY"
fsync_path "$STAGED_BINARY"
mv -f "$STAGED_BINARY" "$FINAL_BINARY"
fsync_path "$DEST_DIR"
"$FINAL_BINARY" --version
"#,
        version = TYCODE_VERSION,
        release_base = TYCODE_RELEASE_BASE_URL,
        sha_macos_arm64 = TYCODE_SUBPROCESS_SHA256_AARCH64_APPLE_DARWIN,
        sha_macos_x64 = TYCODE_SUBPROCESS_SHA256_X86_64_APPLE_DARWIN,
        sha_linux_arm64 = TYCODE_SUBPROCESS_SHA256_AARCH64_UNKNOWN_LINUX_MUSL,
        sha_linux_x64 = TYCODE_SUBPROCESS_SHA256_X86_64_UNKNOWN_LINUX_MUSL,
    )
}

fn home_dir() -> Result<PathBuf, String> {
    crate::paths::home_dir()
}

#[allow(dead_code)]
fn _tycode_release_asset_url(asset_name: &str) -> String {
    format!("{TYCODE_RELEASE_BASE_URL}/v{TYCODE_VERSION}/{asset_name}")
}
