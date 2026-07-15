use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use command_group::AsyncCommandGroup;
use protocol::{
    BackendKind, BackendSetupAction, BackendSetupCommand, BackendSetupDiagnostic,
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

pub(crate) async fn collect_backend_setup() -> BackendSetupPayload {
    let platform = host_platform();
    // Probe every backend concurrently. Each probe spawns a real `<cli>
    // --version` subprocess capped at a 2s timeout, so running them
    // sequentially made host startup wait for the sum of all probes.
    let backends = futures_util::future::join_all(
        [
            BackendKind::Tycode,
            BackendKind::Kiro,
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Antigravity,
            BackendKind::Hermes,
        ]
        .into_iter()
        .map(|kind| probe_backend(kind, platform)),
    )
    .await;
    BackendSetupPayload { backends }
}

/// Backend setup with no real CLI probing — used by test fixtures (and any
/// host configured with `skip_real_backend_probe`) so spawning a host does
/// not pay several seconds of subprocess + network cost per fixture. Reports
/// every backend as not installed; tests drive backends through the mock
/// backend rather than the host's installed-CLI detection.
pub(crate) fn stub_backend_setup() -> BackendSetupPayload {
    let platform = host_platform();
    let backends = [
        BackendKind::Tycode,
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
    let payload = collect_backend_setup().await;
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

async fn probe_backend(kind: BackendKind, platform: HostPlatform) -> BackendSetupInfo {
    let probe = match kind {
        BackendKind::Tycode => probe_installed_tycode().await,
        BackendKind::Kiro => probe_candidates(&command_candidates(KIRO_CLI_CANDIDATES)).await,
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

async fn probe_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        let Some(version) = probe_command(candidate).await else {
            continue;
        };
        return ProbeResult::installed(version);
    }
    ProbeResult::not_installed()
}

async fn probe_command(command: &str) -> Option<Option<String>> {
    let child_path = process_env::resolved_child_process_path().map(std::ffi::OsStr::to_os_string);
    let output = run_version_command_with_child_path(command, child_path)
        .await
        .ok()?;
    let version = output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string());
    Some(version)
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
    #[cfg(test)]
    eprintln!("version_probe command={command:?} stage={stage} elapsed_ms={elapsed_ms}");
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
        BackendKind::Tycode => tycode_install_command(platform),
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

#[cfg(test)]
mod tests {
    use super::*;

    const TYCODE_GROUPED_SETTINGS_ADOPTION_FLOOR: &str = "0.10.0";

    fn parse_semver(value: &str) -> Result<([u64; 3], Option<Vec<&str>>), String> {
        let (version, build) = value
            .split_once('+')
            .map_or((value, None), |(version, build)| (version, Some(build)));
        if let Some(build) = build {
            validate_semver_identifiers(build, false)?;
        }
        let (core, prerelease) = version
            .split_once('-')
            .map_or((version, None), |(core, prerelease)| {
                (core, Some(prerelease))
            });
        let mut parts = core.split('.');
        let core = [
            parse_semver_core_part(parts.next(), "major")?,
            parse_semver_core_part(parts.next(), "minor")?,
            parse_semver_core_part(parts.next(), "patch")?,
        ];
        if parts.next().is_some() {
            return Err(format!("semantic version has too many core parts: {value}"));
        }
        let prerelease = match prerelease {
            Some(prerelease) => {
                validate_semver_identifiers(prerelease, true)?;
                Some(prerelease.split('.').collect::<Vec<_>>())
            }
            None => None,
        };
        Ok((core, prerelease))
    }

    fn parse_semver_core_part(part: Option<&str>, name: &str) -> Result<u64, String> {
        let part = part.ok_or_else(|| format!("semantic version is missing {name}"))?;
        if part.len() > 1 && part.starts_with('0') {
            return Err(format!("semantic version {name} has a leading zero"));
        }
        part.parse::<u64>()
            .map_err(|err| format!("invalid semantic version {name} {part:?}: {err}"))
    }

    fn validate_semver_identifiers(
        value: &str,
        reject_numeric_leading_zero: bool,
    ) -> Result<(), String> {
        if value.is_empty() {
            return Err("semantic version identifier list is empty".to_string());
        }
        for identifier in value.split('.') {
            if identifier.is_empty()
                || !identifier
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            {
                return Err(format!(
                    "invalid semantic version identifier {identifier:?}"
                ));
            }
            if reject_numeric_leading_zero
                && identifier.len() > 1
                && identifier.starts_with('0')
                && identifier.chars().all(|ch| ch.is_ascii_digit())
            {
                return Err(format!(
                    "numeric semantic version identifier has a leading zero: {identifier}"
                ));
            }
        }
        Ok(())
    }

    fn compare_semver(left: &str, right: &str) -> Result<std::cmp::Ordering, String> {
        let (left_core, left_prerelease) = parse_semver(left)?;
        let (right_core, right_prerelease) = parse_semver(right)?;
        let core_order = left_core.cmp(&right_core);
        if core_order != std::cmp::Ordering::Equal {
            return Ok(core_order);
        }
        match (left_prerelease, right_prerelease) {
            (None, None) => Ok(std::cmp::Ordering::Equal),
            (None, Some(_)) => Ok(std::cmp::Ordering::Greater),
            (Some(_), None) => Ok(std::cmp::Ordering::Less),
            (Some(left), Some(right)) => Ok(compare_semver_prerelease(&left, &right)),
        }
    }

    fn compare_semver_prerelease(left: &[&str], right: &[&str]) -> std::cmp::Ordering {
        for (left, right) in left.iter().zip(right) {
            let left_numeric = left.chars().all(|ch| ch.is_ascii_digit());
            let right_numeric = right.chars().all(|ch| ch.is_ascii_digit());
            let order = match (left_numeric, right_numeric) {
                (true, true) => left.len().cmp(&right.len()).then_with(|| left.cmp(right)),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => left.cmp(right),
            };
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        left.len().cmp(&right.len())
    }

    #[test]
    fn pinned_tycode_meets_grouped_settings_adoption_floor() {
        let pinned_order = compare_semver(TYCODE_VERSION, TYCODE_GROUPED_SETTINGS_ADOPTION_FLOOR)
            .expect("Tycode adoption versions must be valid semantic versions");
        assert!(matches!(
            pinned_order,
            std::cmp::Ordering::Equal | std::cmp::Ordering::Greater
        ));
        assert_eq!(
            compare_semver("0.10.0-pre.1", TYCODE_GROUPED_SETTINGS_ADOPTION_FLOOR)
                .expect("0.10.0-pre.1 is a valid semantic version"),
            std::cmp::Ordering::Less
        );
    }

    struct EnvGuard {
        key: &'static str,
        old_value: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old_value = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old_value }
        }

        fn unset(key: &'static str) -> Self {
            let old_value = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old_value }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old_value.take() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(path)
                .expect("stat executable")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod executable");
        }
    }

    fn write_installed_tycode(home: &Path, body: &str) -> PathBuf {
        let path = tycode_versioned_binary_path_for_home(home);
        std::fs::create_dir_all(path.parent().expect("Tycode install directory"))
            .expect("create Tycode install directory");
        write_executable(&path, body);
        path
    }

    fn write_fake_hermes_cli_install(dir: &Path) -> String {
        let project = dir.join("hermes-agent");
        std::fs::create_dir_all(&project).expect("create fake Hermes project");
        let python = dir.join("fake_python");
        let console = dir.join("hermes_console");
        write_executable(
            &python,
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\nexit 1\n",
        );
        write_executable(
            &console,
            &format!("#!{}\nimport sys\nsys.exit(1)\n", python.to_string_lossy()),
        );
        let hermes = dir.join("hermes");
        write_executable(
            &hermes,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'Hermes Agent v9.9.9\\nProject: {}\\n'\n  exit 0\nfi\nexec {} \"$@\"\n",
                project.to_string_lossy(),
                shell_quote(&console.to_string_lossy())
            ),
        );
        hermes.to_string_lossy().to_string()
    }

    #[test]
    fn tycode_version_parser_ignores_task_update_frames() {
        let parsed =
            parse_tycode_version_output(r#"{"kind":"TaskUpdate","data":{"tasks":[]}}"#, "");
        assert_eq!(parsed, None);
    }

    #[test]
    fn tycode_version_parser_reads_plain_text_version_after_json_frame() {
        let parsed = parse_tycode_version_output(
            "{\"kind\":\"TaskUpdate\",\"data\":{\"tasks\":[]}}\ntycode-subprocess 0.7.3",
            "",
        );
        assert_eq!(parsed.as_deref(), Some("tycode-subprocess 0.7.3"));
    }

    #[test]
    fn tycode_version_parser_accepts_pinned_version() {
        let line = format!("tycode-subprocess {TYCODE_VERSION}");
        let parsed = parse_tycode_version_output(&line, "");
        assert_eq!(parsed.as_deref(), Some(line.as_str()));
        assert_eq!(
            parsed.as_deref().and_then(parse_tycode_reported_version),
            Some(TYCODE_VERSION)
        );
        assert_eq!(
            parse_tycode_version_output("tycode-subprocess 0.9.2-pre.1", "")
                .as_deref()
                .and_then(parse_tycode_reported_version),
            Some("0.9.2-pre.1")
        );
    }

    #[test]
    fn tycode_version_parser_accepts_v0_10_prerelease_format() {
        let line = "tycode-subprocess 0.10.0-pre.1";
        let parsed = parse_tycode_version_output(line, "");

        assert_eq!(parsed.as_deref(), Some(line));
        assert_eq!(
            parsed.as_deref().and_then(parse_tycode_reported_version),
            Some("0.10.0-pre.1")
        );
    }

    #[test]
    fn tycode_never_exposes_a_sign_in_command() {
        assert!(sign_in_command(BackendKind::Tycode, None).is_none());
    }

    #[test]
    fn hermes_sign_in_uses_resolved_executable_path() {
        let command = sign_in_command(BackendKind::Hermes, Some("/tmp/hermes path/hermes"))
            .expect("Hermes sign-in command");
        assert_eq!(command.command, "'/tmp/hermes path/hermes' setup");
        assert_eq!(
            command.display_command.as_deref(),
            Some("/tmp/hermes path/hermes setup")
        );
    }

    #[tokio::test]
    async fn missing_installed_tycode_artifact_preserves_install_flow() {
        let home = tempfile::tempdir().expect("create tempdir");

        let resolved = resolve_tycode_binary_path_for_home(home.path());
        assert_eq!(resolved, None);
        let probe = probe_resolved_tycode(resolved).await;
        let info = backend_setup_info_from_probe(BackendKind::Tycode, HostPlatform::Linux, probe);

        assert_eq!(info.status, BackendSetupStatus::NotInstalled);
        assert!(info.install_command.is_some());
        assert!(info.diagnostic.is_none());
    }

    #[test]
    fn path_tycode_imposter_is_ignored() {
        let home = tempfile::tempdir().expect("create tempdir");
        let path_dir = tempfile::tempdir().expect("create PATH tempdir");
        let imposter = path_dir.path().join("tycode-subprocess");
        write_executable(
            &imposter,
            &format!("#!/bin/sh\nprintf 'tycode-subprocess {TYCODE_VERSION}\\n'\n"),
        );

        assert!(imposter.is_file());
        assert_eq!(resolve_tycode_binary_path_for_home(home.path()), None);
    }

    #[test]
    fn installed_tycode_probe_on_unsupported_install_platform_stays_installed() {
        let info = backend_setup_info_from_probe(
            BackendKind::Tycode,
            HostPlatform::Windows,
            ProbeResult::installed(Some(format!("tycode-subprocess {TYCODE_VERSION}"))),
        );

        assert_eq!(info.status, BackendSetupStatus::Installed);
        let expected_version = format!("tycode-subprocess {TYCODE_VERSION}");
        assert_eq!(
            info.installed_version.as_deref(),
            Some(expected_version.as_str())
        );
        assert!(info.install_command.is_none());
        assert!(info.diagnostic.is_none());
    }

    #[tokio::test]
    async fn exact_installed_tycode_artifact_is_accepted() {
        let home = tempfile::tempdir().expect("create tempdir");
        let command = write_installed_tycode(
            home.path(),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'tycode-subprocess {TYCODE_VERSION}\\n'; exit 0; fi\nexit 1\n"
            ),
        );
        let resolved = resolve_tycode_binary_path_for_home(home.path());
        let command = command.to_string_lossy().into_owned();

        assert_eq!(resolved.as_deref(), Some(command.as_str()));
        let result = probe_resolved_tycode(resolved).await;

        assert_eq!(result.status, BackendSetupStatus::Installed);
        let expected_version = format!("tycode-subprocess {TYCODE_VERSION}");
        assert_eq!(result.version.as_deref(), Some(expected_version.as_str()));
        assert!(result.diagnostic.is_none());
    }

    #[tokio::test]
    async fn tycode_probe_rejects_mismatched_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("tycode-subprocess");
        write_executable(
            &command,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'tycode-subprocess 0.7.7\\n'; exit 0; fi\nexit 1\n",
        );

        let result = probe_resolved_tycode(Some(command.to_string_lossy().to_string())).await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        assert_eq!(result.version, None);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(diagnostic.code, BackendSetupDiagnosticCode::CommandFailed);
        assert!(
            diagnostic.message.contains("version 0.7.7")
                && diagnostic.message.contains(TYCODE_VERSION),
            "diagnostic should name reported and required versions: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn nonzero_exact_tycode_version_output_is_rejected() {
        let home = tempfile::tempdir().expect("create tempdir");
        let command = write_installed_tycode(
            home.path(),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'tycode-subprocess {TYCODE_VERSION}\\n'; exit 9; fi\nexit 1\n"
            ),
        );
        let resolved = resolve_tycode_binary_path_for_home(home.path());
        let command = command.to_string_lossy().into_owned();

        assert_eq!(resolved.as_deref(), Some(command.as_str()));
        let result = probe_resolved_tycode(resolved).await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        assert_eq!(result.version, None);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(diagnostic.code, BackendSetupDiagnosticCode::CommandFailed);
        assert!(
            diagnostic
                .message
                .contains("reported exact expected --version output")
                && diagnostic.message.contains("exited unsuccessfully")
                && diagnostic.message.contains("9"),
            "diagnostic should preserve exact output and failed status: {}",
            diagnostic.message
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn version_probe_bounds_descendant_pipe_drain() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("version-probe");
        write_executable(
            &command,
            "#!/bin/sh\n(sleep 30) &\nprintf 'version 1.0\\n'\nexit 0\n",
        );

        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run_version_command(&command.to_string_lossy()),
        )
        .await
        .expect("version probe must bound pipe draining held open by descendants");

        assert!(matches!(result, Err(VersionCommandFailure::TimedOut)));
    }

    #[test]
    fn unavailable_tycode_probe_on_unsupported_install_platform_stays_unavailable() {
        let diagnostic = BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandFailed,
            message: "Tycode probe failed".to_string(),
        };
        let info = backend_setup_info_from_probe(
            BackendKind::Tycode,
            HostPlatform::Windows,
            ProbeResult::unavailable(diagnostic.clone()),
        );

        assert_eq!(info.status, BackendSetupStatus::Unavailable);
        assert!(info.install_command.is_none());
        assert_eq!(info.diagnostic, Some(diagnostic));
    }

    #[test]
    fn not_installed_tycode_without_install_support_is_unsupported() {
        let info = backend_setup_info_from_probe(
            BackendKind::Tycode,
            HostPlatform::Windows,
            ProbeResult::not_installed(),
        );

        assert_eq!(info.status, BackendSetupStatus::Unsupported);
        assert!(info.install_command.is_none());
        assert!(info.diagnostic.is_none());
    }

    #[tokio::test]
    async fn hermes_probe_does_not_mark_failed_import_installed() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("python");
        std::fs::write(&command, "#!/bin/sh\nexit 1\n").expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&command)
                .expect("stat fake python")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&command, permissions).expect("chmod fake python");
        }

        assert!(
            probe_hermes_python_command(&command.to_string_lossy())
                .await
                .is_err(),
            "failed Hermes import probes must not be treated as installed"
        );
    }

    #[tokio::test]
    async fn hermes_explicit_executable_failure_is_unavailable_without_fallback() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", "/definitely/not/hermes");

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(diagnostic.code, BackendSetupDiagnosticCode::CommandNotFound);
        assert!(
            diagnostic.message.contains("HERMES_EXECUTABLE"),
            "diagnostic should name explicit override: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_auto_cli_failure_ignores_ambient_python() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\n'; exit 0; fi\nexit 1\n",
        );
        let python = dir.path().join("python");
        write_executable(
            &python,
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\nexit 1\n",
        );
        let _python_path = EnvGuard::set("PYTHON", &python.to_string_lossy());
        let _hermes_python = EnvGuard::unset("HERMES_PYTHON");
        let _hermes_executable = EnvGuard::unset("HERMES_EXECUTABLE");

        let result = probe_hermes_gateway_with_sources(
            None,
            None,
            vec![hermes.to_string_lossy().to_string()],
        )
        .await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        assert_eq!(result.version, None);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingProjectRoot
        );
        assert!(
            diagnostic.message.contains("Found Hermes CLI"),
            "diagnostic should preserve CLI failure instead of using ambient Python: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_cli_wrapper_without_project_venv_is_installed() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let hermes = write_fake_hermes_cli_install(dir.path());
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::unset("HERMES_EXECUTABLE");

        let result = probe_hermes_gateway_with_sources(None, None, vec![hermes.clone()]).await;

        assert_eq!(result.status, BackendSetupStatus::Installed);
        assert_eq!(result.version.as_deref(), Some("Hermes Agent v9.9.9"));
        assert!(result.diagnostic.is_none());
        assert_eq!(result.hermes_executable.as_deref(), Some(hermes.as_str()));
    }

    #[tokio::test]
    async fn hermes_found_unusable_cli_is_unavailable_not_not_installed() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let project = dir.path().join("hermes-agent");
        std::fs::create_dir_all(&project).expect("create project without venv");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\nProject: {}\\n'; exit 0; fi\nexit 1\n",
                project.to_string_lossy()
            ),
        );
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::unset("HERMES_EXECUTABLE");

        let result = probe_hermes_gateway_with_sources(
            None,
            None,
            vec![hermes.to_string_lossy().to_string()],
        )
        .await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingGatewayPython
        );
        assert!(
            diagnostic.message.contains("Hermes Agent v9.9.9")
                && diagnostic
                    .message
                    .contains(&project.to_string_lossy().to_string()),
            "diagnostic should name version and project: {}",
            diagnostic.message
        );
        assert!(
            !diagnostic.message.contains("so `hermes` is on PATH")
                && !diagnostic.message.contains("set HERMES_EXECUTABLE"),
            "found-unusable diagnostic should not recommend PATH/HERMES_EXECUTABLE remedies: {}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("Re-run the Hermes installer")
                && diagnostic.message.contains("HERMES_PYTHON"),
            "found-unusable diagnostic should include an actionable gateway-Python remedy: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_missing_project_root_is_typed_diagnostic() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\n'; exit 0; fi\nexit 1\n",
        );
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", &hermes.to_string_lossy());

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingProjectRoot
        );
        assert!(
            diagnostic.message.contains("Project:"),
            "diagnostic should describe missing Project line: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_missing_gateway_python_is_typed_diagnostic() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let project = dir.path().join("hermes-agent");
        std::fs::create_dir_all(&project).expect("create project without venv");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\nProject: {}\\n'; exit 0; fi\nexit 1\n",
                project.to_string_lossy()
            ),
        );
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", &hermes.to_string_lossy());

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingGatewayPython
        );
        assert!(
            diagnostic
                .message
                .contains("could not resolve a Python interpreter"),
            "diagnostic should mention the unresolved gateway Python: {}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("Re-run the Hermes installer")
                && diagnostic.message.contains("HERMES_PYTHON"),
            "diagnostic should include an actionable gateway-Python remedy: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_python_override_failure_is_authoritative() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let fake_python = dir.path().join("python");
        write_executable(&fake_python, "#!/bin/sh\nexit 1\n");
        let valid_hermes = write_fake_hermes_cli_install(dir.path());
        let _python = EnvGuard::set("HERMES_PYTHON", &fake_python.to_string_lossy());
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", &valid_hermes);

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::GatewayImportFailed
        );
        assert!(
            diagnostic.message.contains("HERMES_PYTHON"),
            "diagnostic should name explicit Python override: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_probe_timeout_is_not_installed_without_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("python");
        std::fs::write(&command, "#!/bin/sh\nsleep 5\n").expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&command)
                .expect("stat fake python")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&command, permissions).expect("chmod fake python");
        }

        assert!(
            probe_hermes_python_command(&command.to_string_lossy())
                .await
                .is_err(),
            "timed-out Hermes import probes must not be treated as installed"
        );
    }

    #[test]
    fn tycode_install_script_uses_portable_private_shell_state() {
        let command = tycode_install_command(HostPlatform::Linux).expect("Tycode install command");

        assert!(command.command.starts_with("set -eu\n"));
        assert!(!command.command.contains("pipefail"));
        assert_eq!(
            command.display_command.as_deref(),
            Some("/bin/sh <private Tyde v0.10.0 setup script>")
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_setup_command_is_private_truthful_and_bounded() {
        let prepared = stage_backend_setup_command("printf 'ready\\n'", HostPlatform::Linux)
            .expect("stage backend setup command");
        let path = prepared.staged_script.clone();
        let metadata = std::fs::metadata(&path).expect("stat staged setup script");
        let script = std::fs::read_to_string(&path).expect("read staged setup script");

        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(prepared.program(), "/bin/sh");
        assert_eq!(prepared.arguments(), &[path.to_string_lossy().into_owned()]);
        assert_eq!(
            prepared.display_command(),
            format!("/bin/sh {}", shell_quote(&path.to_string_lossy()))
        );
        assert!(
            script.starts_with(&format!(
                "printf '%s\\n' {}\n",
                shell_quote(&format!("$ {}", prepared.display_command()))
            )),
            "staged script must visibly report the exact command that launched it"
        );
        assert!(script.ends_with("printf 'ready\\n'\n"));

        drop(prepared);
        assert!(
            !path.exists(),
            "dropping the prepared command must clean up"
        );
    }
}
